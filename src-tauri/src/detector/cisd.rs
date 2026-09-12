//! CISD (Change In State of Delivery) detector — §5.2.13.
//!
//! Reads `current_leg` from a shared `SwingSeries`. CISD line = leg-origin
//! candle's **open** (NOT close). When the current close-to-close delivery
//! leg closes through that line and has at least `min_leg_bars` bars, emit a
//! CISD event. Candle color is not part of the confirmation rule.

use super::swing::Leg;
use super::types::{structure_id, Cisd, Direction, IctStructure, StructureEvent};
use super::{Detector, DetectorCtx};
use crate::types::{Bar, Timeframe};

#[derive(Clone, Debug)]
pub struct CisdConfig {
    pub min_leg_bars: usize,
}
impl Default for CisdConfig {
    fn default() -> Self {
        Self { min_leg_bars: 2 }
    }
}

pub struct CisdDetector {
    pub symbol: String,
    pub tf: Timeframe,
    pub cfg: CisdConfig,
    /// Track which leg origins we've already emitted on, to dedupe.
    last_origin_emitted: Option<(i64, Direction)>,
    /// Most recent completed delivery leg that actually met min_leg_bars.
    /// A one-candle colour/close oscillation must not erase a valid CISD
    /// anchor before price closes through its origin open.
    qualified_completed_leg: Option<Leg>,
    last_seen_ts: i64,
}

impl CisdDetector {
    pub fn new(symbol: impl Into<String>, tf: Timeframe, cfg: CisdConfig) -> Self {
        Self {
            symbol: symbol.into(),
            tf,
            cfg,
            last_origin_emitted: None,
            qualified_completed_leg: None,
            last_seen_ts: i64::MIN,
        }
    }
}

impl Detector for CisdDetector {
    fn name(&self) -> &'static str {
        "cisd"
    }

    fn apply_param(&mut self, key: &str, value: &serde_json::Value) -> bool {
        match key {
            "min_leg_bars" => {
                if let Some(v) = value.as_u64() {
                    self.cfg.min_leg_bars = (v as usize).max(1);
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn reset(&mut self) {
        self.last_origin_emitted = None;
        self.qualified_completed_leg = None;
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

        // The engine advances the one shared SwingSeries before detectors.
        // Keep watching the most recently completed leg for the entire
        // opposing delivery run. The old implementation only checked the
        // very first opposite-colour candle, so a legitimate CISD close on
        // the second/later candle was silently missed.
        if let Some(completed) = ctx.swings.last_completed_leg.as_ref() {
            if completed.bars_in_leg >= self.cfg.min_leg_bars
                && self
                    .qualified_completed_leg
                    .as_ref()
                    .is_none_or(|qualified| qualified.origin_ts != completed.origin_ts)
            {
                self.qualified_completed_leg = Some(completed.clone());
            }
        }
        let Some(completed) = self.qualified_completed_leg.as_ref() else {
            return out;
        };
        let Some(current) = ctx.swings.current_leg.as_ref() else {
            return out;
        };
        if current.direction == completed.direction {
            return out;
        }

        // CISD line = leg_origin.open (ICT original).
        let cisd_line = completed.origin_open;
        let triggered = match completed.direction {
            Direction::Bullish => last.close < cisd_line,
            Direction::Bearish => last.close > cisd_line,
        };
        if !triggered {
            return out;
        }

        let dir = completed.direction.opposite();
        if self.last_origin_emitted == Some((completed.origin_ts, dir)) {
            return out;
        }

        let id = structure_id(&[
            &self.symbol,
            self.tf.tag(),
            "cisd",
            match dir {
                Direction::Bullish => "bull",
                Direction::Bearish => "bear",
            },
            &completed.origin_ts.to_string(),
            &last.ts.to_string(),
        ]);
        out.push(StructureEvent::New(IctStructure::Cisd(Cisd {
            id,
            symbol: self.symbol.clone(),
            tf: self.tf,
            direction: dir,
            leg_origin_ts: completed.origin_ts,
            leg_origin_price: completed.origin_open,
            break_ts: last.ts,
            break_price: last.close,
        })));
        self.last_origin_emitted = Some((completed.origin_ts, dir));
        self.qualified_completed_leg = None;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::swing::SwingSeries;

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

    fn run_with(min_leg: usize, bars: &[Bar]) -> Vec<StructureEvent> {
        let mut s = SwingSeries::new(2);
        let mut d = CisdDetector::new(
            "X",
            Timeframe::M5,
            CisdConfig {
                min_leg_bars: min_leg,
            },
        );
        let mut all = Vec::new();
        for end in 1..=bars.len() {
            let slice = &bars[..end];
            s.on_closed_bar(slice);
            let ctx = DetectorCtx::new(&s);
            all.extend(d.on_closed(slice, &ctx));
        }
        all
    }

    #[test]
    fn bearish_cisd_breaks_bullish_leg_origin_open() {
        // Bullish leg of 3 bars (origin open=1.00), then a bearish close < 1.00.
        let bars = vec![
            b(1, 1.00, 1.05, 0.99, 1.04), // origin (open 1.00)
            b(2, 1.04, 1.08, 1.03, 1.07),
            b(3, 1.07, 1.10, 1.06, 1.09),
            b(4, 1.09, 1.10, 0.95, 0.96), // bearish close < 1.00 → bearish CISD
        ];
        let evs = run_with(2, &bars);
        let cisd = evs.iter().find_map(|e| match e {
            StructureEvent::New(IctStructure::Cisd(c)) => Some(c.clone()),
            _ => None,
        });
        let cisd = cisd.expect("expected bearish CISD");
        assert_eq!(cisd.direction, Direction::Bearish);
        assert!((cisd.leg_origin_price - 1.00).abs() < 1e-9);
    }

    #[test]
    fn wick_only_does_not_trigger_cisd() {
        let bars = vec![
            b(1, 1.00, 1.05, 0.99, 1.04),
            b(2, 1.04, 1.08, 1.03, 1.07),
            b(3, 1.07, 1.10, 1.06, 1.09),
            b(4, 1.09, 1.10, 0.95, 1.05), // wick below 1.00 but green close
        ];
        let evs = run_with(2, &bars);
        let any = evs
            .iter()
            .any(|e| matches!(e, StructureEvent::New(IctStructure::Cisd(_))));
        assert!(!any);
    }

    #[test]
    fn short_leg_below_min_does_not_trigger() {
        // single-bar bullish leg (min_leg_bars=2) → no CISD even if next bar
        // closes below origin open.
        let bars = vec![b(1, 1.00, 1.05, 0.99, 1.04), b(2, 1.04, 1.06, 0.95, 0.96)];
        let evs = run_with(2, &bars);
        let any = evs
            .iter()
            .any(|e| matches!(e, StructureEvent::New(IctStructure::Cisd(_))));
        assert!(!any);
    }

    #[test]
    fn cisd_can_confirm_on_later_opposite_candle() {
        let bars = vec![
            b(1, 1.00, 1.05, 0.99, 1.04),
            b(2, 1.04, 1.08, 1.03, 1.07),
            b(3, 1.07, 1.10, 1.06, 1.09),
            b(4, 1.09, 1.10, 1.01, 1.03), // turns bearish, but no close through 1.00
            b(5, 1.03, 1.04, 0.97, 0.98), // later bearish close confirms CISD
        ];
        let evs = run_with(2, &bars);
        let cisd = evs
            .iter()
            .find_map(|event| match event {
                StructureEvent::New(IctStructure::Cisd(cisd)) => Some(cisd),
                _ => None,
            })
            .expect("later close should confirm CISD");
        assert_eq!(cisd.break_ts, 5);
        assert_eq!(cisd.direction, Direction::Bearish);
    }

    #[test]
    fn short_noise_leg_does_not_erase_last_qualified_delivery() {
        let bars = vec![
            b(1, 1.00, 1.05, 0.99, 1.04),
            b(2, 1.04, 1.08, 1.03, 1.07), // qualified bullish delivery
            b(3, 1.07, 1.08, 1.01, 1.03), // bearish, but no origin break
            b(4, 1.03, 1.06, 1.02, 1.05), // one-bar bullish noise
            b(5, 1.05, 1.06, 1.01, 1.02),
            b(6, 1.02, 1.03, 0.97, 0.98), // closes below original 1.00 line
        ];
        let evs = run_with(2, &bars);
        let cisd = evs
            .iter()
            .find_map(|event| match event {
                StructureEvent::New(IctStructure::Cisd(cisd)) => Some(cisd),
                _ => None,
            })
            .expect("qualified delivery must survive one-bar noise");
        assert_eq!(cisd.break_ts, 6);
        assert_eq!(cisd.leg_origin_ts, 1);
        assert_eq!(cisd.direction, Direction::Bearish);
    }

    #[test]
    fn delivery_direction_uses_previous_close_not_candle_colour() {
        let bars = vec![
            b(1, 1.00, 1.06, 0.99, 1.04),
            // Red body, but close is above the previous close: bullish delivery.
            b(2, 1.08, 1.09, 1.03, 1.05),
            b(3, 1.05, 1.06, 0.97, 0.98),
        ];
        let evs = run_with(2, &bars);
        let cisd = evs
            .iter()
            .find_map(|event| match event {
                StructureEvent::New(IctStructure::Cisd(cisd)) => Some(cisd),
                _ => None,
            })
            .expect("red candle with a higher close must extend bullish delivery");
        assert_eq!(cisd.leg_origin_ts, 1);
        assert_eq!(cisd.break_ts, 3);
    }
}
