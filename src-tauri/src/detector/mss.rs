//! MSS (Market Structure Shift) detector — §5.2.4.
//!
//! Reads swings from a shared SwingSeries and emits a directional MSS when
//! a closing bar takes out the most recent counter-trend swing in the
//! current structural trend. Pure BoS (with-trend) is *not* emitted in M3.
//!
//! Trend definition (HH/HL ↑, LH/LL ↓):
//!   - Up structure: last two swing highs higher AND last two swing lows higher.
//!   - Down structure: last two swing highs lower AND last two swing lows lower.

use super::swing::{Swing, SwingKind};
use super::types::{structure_id, Direction, IctStructure, Mss, StructureEvent};
use super::{Detector, DetectorCtx};
use crate::types::{Bar, Timeframe};

#[derive(Clone, Debug)]
pub struct MssConfig {
    pub fractal_n: usize,
}
impl Default for MssConfig {
    fn default() -> Self {
        Self { fractal_n: 2 }
    }
}

pub struct MssDetector {
    pub symbol: String,
    pub tf: Timeframe,
    pub cfg: MssConfig,
    /// Last (swing_ts, direction) we already emitted on, to dedupe.
    last_emit: Option<(i64, Direction)>,
    last_seen_ts: i64,
}

impl MssDetector {
    pub fn new(symbol: impl Into<String>, tf: Timeframe, cfg: MssConfig) -> Self {
        Self {
            symbol: symbol.into(),
            tf,
            cfg,
            last_emit: None,
            last_seen_ts: i64::MIN,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Trend {
    Up,
    Down,
    Range,
}

fn trend_from(highs: Option<(&Swing, &Swing)>, lows: Option<(&Swing, &Swing)>) -> Trend {
    match (highs, lows) {
        (Some((ho, hn)), Some((lo, ln))) => {
            let up_h = hn.price > ho.price;
            let up_l = ln.price > lo.price;
            let dn_h = hn.price < ho.price;
            let dn_l = ln.price < lo.price;
            if up_h && up_l {
                Trend::Up
            } else if dn_h && dn_l {
                Trend::Down
            } else {
                Trend::Range
            }
        }
        _ => Trend::Range,
    }
}

impl Detector for MssDetector {
    fn name(&self) -> &'static str {
        "mss"
    }

    fn apply_param(&mut self, key: &str, value: &serde_json::Value) -> bool {
        match key {
            "fractal_n" => {
                if let Some(v) = value.as_u64() {
                    self.cfg.fractal_n = (v as usize).max(1);
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn reset(&mut self) {
        self.last_emit = None;
        self.last_seen_ts = i64::MIN;
    }

    fn on_closed(&mut self, bars: &[Bar], ctx: &DetectorCtx<'_>) -> Vec<StructureEvent> {
        let mut out = Vec::new();
        let last = match bars.last() {
            Some(b) => b,
            None => return out,
        };
        if last.ts <= self.last_seen_ts {
            return out;
        }
        self.last_seen_ts = last.ts;

        let highs = ctx.swings.last_two(SwingKind::High);
        let lows = ctx.swings.last_two(SwingKind::Low);
        let trend = trend_from(highs, lows);

        match trend {
            Trend::Up => {
                if let Some(swing_low) = ctx.swings.last_of(SwingKind::Low) {
                    if last.close < swing_low.price {
                        if self.last_emit != Some((swing_low.ts, Direction::Bearish)) {
                            let id = structure_id(&[
                                &self.symbol,
                                self.tf.tag(),
                                "mss",
                                "bear",
                                &swing_low.ts.to_string(),
                                &last.ts.to_string(),
                            ]);
                            out.push(StructureEvent::New(IctStructure::Mss(Mss {
                                id,
                                symbol: self.symbol.clone(),
                                tf: self.tf,
                                direction: Direction::Bearish,
                                break_ts: last.ts,
                                break_price: last.close,
                                swing_ts: swing_low.ts,
                                swing_price: swing_low.price,
                            })));
                            self.last_emit = Some((swing_low.ts, Direction::Bearish));
                        }
                    }
                }
            }
            Trend::Down => {
                if let Some(swing_high) = ctx.swings.last_of(SwingKind::High) {
                    if last.close > swing_high.price {
                        if self.last_emit != Some((swing_high.ts, Direction::Bullish)) {
                            let id = structure_id(&[
                                &self.symbol,
                                self.tf.tag(),
                                "mss",
                                "bull",
                                &swing_high.ts.to_string(),
                                &last.ts.to_string(),
                            ]);
                            out.push(StructureEvent::New(IctStructure::Mss(Mss {
                                id,
                                symbol: self.symbol.clone(),
                                tf: self.tf,
                                direction: Direction::Bullish,
                                break_ts: last.ts,
                                break_price: last.close,
                                swing_ts: swing_high.ts,
                                swing_price: swing_high.price,
                            })));
                            self.last_emit = Some((swing_high.ts, Direction::Bullish));
                        }
                    }
                }
            }
            Trend::Range => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::swing::SwingSeries;
    use crate::types::{Bar, Timeframe};

    fn b(ts: i64, o: f64, h: f64, l: f64, c: f64) -> Bar {
        Bar {
            symbol: "X".into(),
            tf: Timeframe::M5,
            ts,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: 0.0,
        }
    }

    fn run(bars: &[Bar]) -> (SwingSeries, Vec<StructureEvent>) {
        let mut s = SwingSeries::new(2);
        let mut d = MssDetector::new("X", Timeframe::M5, MssConfig::default());
        let mut all = Vec::new();
        for end in 1..=bars.len() {
            let slice = &bars[..end];
            s.on_closed_bar(slice);
            let ctx = DetectorCtx::new(&s);
            all.extend(d.on_closed(slice, &ctx));
        }
        (s, all)
    }

    #[test]
    fn bearish_mss_in_uptrend_breaks_swing_low() {
        // Up-structure: HH at ts5 (1.20) → HH at ts12 (1.30); HL at ts8 (1.00) → HL at ts15 (1.08).
        // Then a candle closes below 1.08 → bearish MSS.
        let bars = vec![
            b(1, 1.00, 1.02, 0.99, 1.01),
            b(2, 1.01, 1.04, 1.00, 1.03),
            b(3, 1.03, 1.06, 1.02, 1.05),
            b(4, 1.05, 1.08, 1.04, 1.07),
            b(5, 1.07, 1.20, 1.06, 1.19),
            b(6, 1.19, 1.18, 1.10, 1.11),
            b(7, 1.11, 1.13, 1.05, 1.06),
            b(8, 1.06, 1.08, 1.00, 1.01),
            b(9, 1.01, 1.07, 1.01, 1.05),
            b(10, 1.05, 1.10, 1.04, 1.09),
            b(11, 1.09, 1.18, 1.08, 1.17),
            b(12, 1.17, 1.30, 1.15, 1.29),
            b(13, 1.29, 1.28, 1.20, 1.21),
            b(14, 1.21, 1.22, 1.10, 1.11),
            b(15, 1.11, 1.13, 1.08, 1.09),
            b(16, 1.09, 1.14, 1.09, 1.13),
            b(17, 1.13, 1.16, 1.11, 1.15),
            b(18, 1.15, 1.16, 0.95, 0.96),
        ];
        let (_, evs) = run(&bars);
        let bearish = evs.iter().find(|e| {
            matches!(e,
            StructureEvent::New(IctStructure::Mss(m)) if m.direction == Direction::Bearish)
        });
        assert!(bearish.is_some(), "expected bearish MSS, got {:?}", evs);
    }

    #[test]
    fn bullish_mss_in_downtrend_breaks_swing_high() {
        // Down-structure: LH ts5(1.31)→ts8(1.25)→ts13(1.20); LL ts5(1.10)→ts10(1.05).
        // Then candle closes above 1.20 → bullish MSS.
        let bars = vec![
            b(1, 1.30, 1.31, 1.28, 1.29),
            b(2, 1.29, 1.30, 1.27, 1.28),
            b(3, 1.28, 1.29, 1.25, 1.26),
            b(4, 1.26, 1.27, 1.23, 1.24),
            b(5, 1.24, 1.25, 1.10, 1.11),
            b(6, 1.11, 1.18, 1.11, 1.17),
            b(7, 1.17, 1.22, 1.16, 1.21),
            b(8, 1.21, 1.25, 1.20, 1.21),
            b(9, 1.21, 1.20, 1.15, 1.16),
            b(10, 1.16, 1.18, 1.05, 1.06),
            b(11, 1.06, 1.10, 1.06, 1.09),
            b(12, 1.09, 1.14, 1.08, 1.13),
            b(13, 1.13, 1.20, 1.12, 1.19),
            b(14, 1.19, 1.18, 1.15, 1.16),
            b(15, 1.16, 1.17, 1.10, 1.11),
            b(16, 1.11, 1.30, 1.10, 1.29),
        ];
        let (_, evs) = run(&bars);
        let bullish = evs.iter().find(|e| {
            matches!(e,
            StructureEvent::New(IctStructure::Mss(m)) if m.direction == Direction::Bullish)
        });
        assert!(bullish.is_some(), "expected bullish MSS, got {:?}", evs);
    }

    #[test]
    fn wick_only_does_not_trigger() {
        // Build an uptrend then a candle that wicks below swing low but closes back inside.
        let mut bars = vec![
            b(1, 1.10, 1.11, 1.09, 1.10),
            b(2, 1.10, 1.11, 1.08, 1.09),
            b(3, 1.09, 1.10, 1.05, 1.07),
            b(4, 1.07, 1.10, 1.06, 1.09),
            b(5, 1.09, 1.12, 1.08, 1.11),
            b(6, 1.11, 1.18, 1.10, 1.17),
            b(7, 1.17, 1.25, 1.15, 1.24),
            b(8, 1.24, 1.24, 1.18, 1.19),
            b(9, 1.19, 1.20, 1.13, 1.14),
            b(10, 1.14, 1.16, 1.10, 1.12),
            b(11, 1.12, 1.14, 1.11, 1.13),
            b(12, 1.13, 1.18, 1.12, 1.17),
            b(13, 1.17, 1.22, 1.16, 1.21),
            b(14, 1.21, 1.30, 1.20, 1.29),
            b(15, 1.29, 1.30, 1.25, 1.26),
            b(16, 1.26, 1.27, 1.22, 1.23),
        ];
        // Wick below 1.10 but close above it.
        bars.push(b(17, 1.23, 1.23, 1.05, 1.20));
        let (_, evs) = run(&bars);
        let any = evs
            .iter()
            .any(|e| matches!(e, StructureEvent::New(IctStructure::Mss(_))));
        assert!(!any, "wick-only should not trigger MSS, got {:?}", evs);
    }
}
