use serde_json::Value;

use crate::types::{Bar, Timeframe};

use super::swing::SwingKind;
use super::types::{
    structure_id, Direction, FvgState, GapState, IctStructure, ObState, OteZone, StructureEvent,
    ZoneState,
};
use super::{Detector, DetectorCtx};

#[derive(Clone, Debug)]
pub struct OteConfig {
    pub fib_low: f64,
    pub fib_high: f64,
    pub confluence_lookback_bars: usize,
}

impl Default for OteConfig {
    fn default() -> Self {
        Self {
            fib_low: 0.62,
            fib_high: 0.79,
            confluence_lookback_bars: 200,
        }
    }
}

pub struct OteDetector {
    symbol: String,
    tf: Timeframe,
    cfg: OteConfig,
    last_id: Option<String>,
}

impl OteDetector {
    pub fn new(symbol: impl Into<String>, tf: Timeframe, cfg: OteConfig) -> Self {
        Self {
            symbol: symbol.into(),
            tf,
            cfg,
            last_id: None,
        }
    }
}

impl Detector for OteDetector {
    fn name(&self) -> &'static str {
        "ote"
    }

    fn on_closed(&mut self, bars: &[Bar], ctx: &DetectorCtx<'_>) -> Vec<StructureEvent> {
        let Some(last) = bars.last() else {
            return Vec::new();
        };
        let Some(high) = ctx.swings.last_of(SwingKind::High) else {
            return Vec::new();
        };
        let Some(low) = ctx.swings.last_of(SwingKind::Low) else {
            return Vec::new();
        };
        let (direction, start_ts, end_ts, leg_low, leg_high) = if low.ts < high.ts {
            (Direction::Bullish, low.ts, high.ts, low.price, high.price)
        } else {
            (Direction::Bearish, high.ts, low.ts, low.price, high.price)
        };
        if leg_high <= leg_low {
            return Vec::new();
        }
        let range = leg_high - leg_low;
        let fib_low = self.cfg.fib_low.min(self.cfg.fib_high);
        let fib_high = self.cfg.fib_low.max(self.cfg.fib_high);
        let (price_low, price_high) = match direction {
            Direction::Bullish => (leg_high - fib_high * range, leg_high - fib_low * range),
            Direction::Bearish => (leg_low + fib_low * range, leg_low + fib_high * range),
        };
        let id = structure_id(&[
            &self.symbol,
            self.tf.tag(),
            "ote",
            &start_ts.to_string(),
            &end_ts.to_string(),
            &format!("{fib_low:.4}"),
            &format!("{fib_high:.4}"),
        ]);
        if self.last_id.as_deref() == Some(&id) {
            return Vec::new();
        }
        self.last_id = Some(id.clone());
        let confluent_structure_ids = confluence_ids(
            ctx.structures,
            &self.symbol,
            self.tf,
            last.ts,
            self.cfg.confluence_lookback_bars,
            price_low,
            price_high,
        );
        vec![StructureEvent::New(IctStructure::Ote(OteZone {
            id,
            symbol: self.symbol.clone(),
            tf: self.tf,
            direction,
            leg_start_ts: start_ts,
            leg_end_ts: end_ts,
            leg_low,
            leg_high,
            price_low,
            price_high,
            fib_low,
            fib_high,
            confluent_structure_ids,
        }))]
    }

    fn apply_param(&mut self, key: &str, value: &Value) -> bool {
        match key {
            "fib_low" => value.as_f64().map(|v| self.cfg.fib_low = v).is_some(),
            "fib_high" => value.as_f64().map(|v| self.cfg.fib_high = v).is_some(),
            "confluence_lookback_bars" => value
                .as_u64()
                .map(|v| self.cfg.confluence_lookback_bars = v as usize)
                .is_some(),
            _ => false,
        }
    }

    fn reset(&mut self) {
        self.last_id = None;
    }
}

fn confluence_ids(
    structures: &[IctStructure],
    symbol: &str,
    tf: Timeframe,
    last_ts: i64,
    lookback_bars: usize,
    price_low: f64,
    price_high: f64,
) -> Vec<String> {
    let min_ts = last_ts.saturating_sub(tf.duration_ms().saturating_mul(lookback_bars as i64));
    let mut ids = Vec::new();
    for s in structures {
        if s.symbol() != symbol || s.tf() != tf {
            continue;
        }
        let Some((anchor_ts, low, high, active)) = zone_bounds(s) else {
            continue;
        };
        if !active || anchor_ts < min_ts {
            continue;
        }
        if price_high >= low && high >= price_low {
            ids.push(s.id().to_string());
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

fn zone_bounds(s: &IctStructure) -> Option<(i64, f64, f64, bool)> {
    match s {
        IctStructure::Fvg(f) => Some((
            f.ts_confirm,
            f.price_low,
            f.price_high,
            matches!(
                f.state,
                FvgState::Active | FvgState::Mitigated50 | FvgState::InvertedActive
            ),
        )),
        IctStructure::OrderBlock(o) => Some((
            o.ts_confirm,
            o.price_low,
            o.price_high,
            matches!(o.state, ObState::Active | ObState::Tested),
        )),
        IctStructure::BreakerBlock(b) => Some((
            b.ts_confirm,
            b.price_low,
            b.price_high,
            matches!(b.state, ZoneState::Active | ZoneState::Tested),
        )),
        IctStructure::VolumeImbalance(v) => Some((
            v.ts_confirm,
            v.price_low,
            v.price_high,
            matches!(v.state, GapState::Active | GapState::Mitigated50),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::swing::SwingSeries;
    use crate::detector::types::{BreakerBlock, Fvg, OrderBlock, VolumeImbalance};

    fn b(ts: i64, h: f64, l: f64, c: f64) -> Bar {
        Bar {
            symbol: "EURUSD".into(),
            tf: Timeframe::M5,
            ts,
            open: c,
            high: h,
            low: l,
            close: c,
            volume: 0.0,
        }
    }

    fn swings_for(bars: &[Bar]) -> SwingSeries {
        let mut swings = SwingSeries::new(1);
        for i in 1..=bars.len() {
            swings.on_closed_bar(&bars[..i]);
        }
        swings
    }

    #[test]
    fn bullish_ote_zone_calculates() {
        let bars = vec![
            b(0, 1.0, 1.0, 1.0),
            b(1, 1.2, 1.1, 1.15),
            b(2, 1.1, 1.05, 1.08),
            b(3, 1.3, 1.2, 1.28),
            b(4, 1.2, 1.1, 1.15),
        ];
        let swings = swings_for(&bars);
        let mut d = OteDetector::new("EURUSD", Timeframe::M5, OteConfig::default());
        let evs = d.on_closed(&bars, &DetectorCtx::new(&swings));
        assert!(
            matches!(evs.first(), Some(StructureEvent::New(IctStructure::Ote(o))) if o.price_low < o.price_high)
        );
    }

    #[test]
    fn bearish_ote_zone_calculates_precisely() {
        let bars = vec![
            b(0, 1.30, 1.20, 1.25),
            b(1, 1.20, 1.00, 1.05),
            b(2, 1.24, 1.10, 1.20),
            b(3, 1.18, 0.90, 0.95),
            b(4, 1.10, 1.00, 1.05),
        ];
        let swings = swings_for(&bars);
        let mut d = OteDetector::new("EURUSD", Timeframe::M5, OteConfig::default());
        let evs = d.on_closed(&bars, &DetectorCtx::new(&swings));
        let Some(StructureEvent::New(IctStructure::Ote(o))) = evs.first() else {
            panic!("expected bearish OTE, got {evs:?}");
        };
        assert_eq!(o.direction, Direction::Bearish);
        assert!((o.leg_low - 0.90).abs() < 1e-9);
        assert!((o.leg_high - 1.24).abs() < 1e-9);
        assert!((o.price_low - (0.90 + 0.62 * 0.34)).abs() < 1e-9);
        assert!((o.price_high - (0.90 + 0.79 * 0.34)).abs() < 1e-9);
    }

    #[test]
    fn ote_records_price_overlap_confluence() {
        let bars = vec![
            b(0, 1.0, 1.0, 1.0),
            b(1, 1.2, 1.1, 1.15),
            b(2, 1.1, 1.05, 1.08),
            b(3, 1.3, 1.2, 1.28),
            b(4, 1.2, 1.1, 1.15),
        ];
        let swings = swings_for(&bars);
        let fvg = IctStructure::Fvg(Fvg {
            id: "fvg1".into(),
            symbol: "EURUSD".into(),
            tf: Timeframe::M5,
            direction: Direction::Bullish,
            ts_open: 2,
            ts_confirm: 3,
            price_low: 1.08,
            price_high: 1.12,
            state: FvgState::Active,
            ts_filled: None,
            consumed_exit_ts: None,
        });
        let ctx = DetectorCtx {
            swings: &swings,
            structures: &[fvg],
        };
        let mut d = OteDetector::new("EURUSD", Timeframe::M5, OteConfig::default());
        let evs = d.on_closed(&bars, &ctx);
        assert!(
            matches!(evs.first(), Some(StructureEvent::New(IctStructure::Ote(o))) if o.confluent_structure_ids == vec!["fvg1".to_string()])
        );
    }

    #[test]
    fn ote_records_ob_breaker_and_vi_confluence() {
        let bars = vec![
            b(0, 1.0, 1.0, 1.0),
            b(1, 1.2, 1.1, 1.15),
            b(2, 1.1, 1.05, 1.08),
            b(3, 1.3, 1.2, 1.28),
            b(4, 1.2, 1.1, 1.15),
        ];
        let swings = swings_for(&bars);
        let structures = vec![
            IctStructure::OrderBlock(OrderBlock {
                id: "ob1".into(),
                symbol: "EURUSD".into(),
                tf: Timeframe::M5,
                direction: Direction::Bullish,
                ts_open: 2,
                ts_confirm: 3,
                price_low: 1.09,
                price_high: 1.12,
                state: ObState::Active,
            }),
            IctStructure::BreakerBlock(BreakerBlock {
                id: "breaker1".into(),
                symbol: "EURUSD".into(),
                tf: Timeframe::M5,
                direction: Direction::Bullish,
                source_ob_id: "ob1".into(),
                ts_open: 2,
                ts_confirm: 3,
                price_low: 1.10,
                price_high: 1.13,
                state: ZoneState::Tested,
            }),
            IctStructure::VolumeImbalance(VolumeImbalance {
                id: "vi1".into(),
                symbol: "EURUSD".into(),
                tf: Timeframe::M5,
                direction: Direction::Bullish,
                ts_open: 2,
                ts_confirm: 3,
                price_low: 1.08,
                price_high: 1.11,
                state: GapState::Mitigated50,
            }),
        ];
        let ctx = DetectorCtx {
            swings: &swings,
            structures: &structures,
        };
        let mut d = OteDetector::new("EURUSD", Timeframe::M5, OteConfig::default());
        let evs = d.on_closed(&bars, &ctx);
        assert!(
            matches!(evs.first(), Some(StructureEvent::New(IctStructure::Ote(o))) if o.confluent_structure_ids == vec!["breaker1".to_string(), "ob1".to_string(), "vi1".to_string()])
        );
    }
}
