use std::collections::HashSet;

use serde_json::Value;

use crate::types::{Bar, Timeframe};

use super::swing::SwingKind;
use super::types::{structure_id, Bos, Direction, IctStructure, StructureEvent};
use super::{Detector, DetectorCtx};

#[derive(Clone, Debug)]
pub struct BosConfig {
    pub enabled: bool,
}

impl Default for BosConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

pub struct BosDetector {
    symbol: String,
    tf: Timeframe,
    cfg: BosConfig,
    triggered: HashSet<i64>,
}

impl BosDetector {
    pub fn new(symbol: impl Into<String>, tf: Timeframe, cfg: BosConfig) -> Self {
        Self {
            symbol: symbol.into(),
            tf,
            cfg,
            triggered: HashSet::new(),
        }
    }
}

impl Detector for BosDetector {
    fn name(&self) -> &'static str {
        "bos"
    }

    fn on_closed(&mut self, bars: &[Bar], ctx: &DetectorCtx<'_>) -> Vec<StructureEvent> {
        if !self.cfg.enabled {
            return Vec::new();
        }
        let Some(last) = bars.last() else {
            return Vec::new();
        };
        let highs = ctx.swings.last_two(SwingKind::High);
        let lows = ctx.swings.last_two(SwingKind::Low);
        let Some((prev_high, last_high)) = highs else {
            return Vec::new();
        };
        let Some((prev_low, last_low)) = lows else {
            return Vec::new();
        };
        let bullish_structure =
            last_high.price > prev_high.price && last_low.price > prev_low.price;
        let bearish_structure =
            last_high.price < prev_high.price && last_low.price < prev_low.price;
        let (direction, swing_ts, swing_price) =
            if bullish_structure && last.close > last_high.price {
                (Direction::Bullish, last_high.ts, last_high.price)
            } else if bearish_structure && last.close < last_low.price {
                (Direction::Bearish, last_low.ts, last_low.price)
            } else {
                return Vec::new();
            };
        if !self.triggered.insert(swing_ts) {
            return Vec::new();
        }
        let id = structure_id(&[
            &self.symbol,
            self.tf.tag(),
            "bos",
            direction_tag(direction),
            &swing_ts.to_string(),
            &last.ts.to_string(),
        ]);
        vec![StructureEvent::New(IctStructure::Bos(Bos {
            id,
            symbol: self.symbol.clone(),
            tf: self.tf,
            direction,
            break_ts: last.ts,
            break_price: last.close,
            swing_ts,
            swing_price,
        }))]
    }

    fn apply_param(&mut self, key: &str, value: &Value) -> bool {
        match key {
            "enabled" => value.as_bool().map(|v| self.cfg.enabled = v).is_some(),
            _ => false,
        }
    }

    fn reset(&mut self) {
        self.triggered.clear();
    }
}

fn direction_tag(direction: Direction) -> &'static str {
    match direction {
        Direction::Bullish => "bullish",
        Direction::Bearish => "bearish",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::mss::{MssConfig, MssDetector};
    use crate::detector::swing::{Swing, SwingSeries};
    use std::collections::VecDeque;

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

    fn ctx_with_swings(bars: &[Bar]) -> SwingSeries {
        let mut swings = SwingSeries::new(1);
        for i in 1..=bars.len() {
            swings.on_closed_bar(&bars[..i]);
        }
        swings
    }

    fn manual_swings(points: &[(i64, f64, SwingKind)]) -> SwingSeries {
        let mut swings = SwingSeries::new(1);
        swings.swings = points
            .iter()
            .map(|(ts, price, kind)| Swing {
                ts: *ts,
                price: *price,
                kind: *kind,
            })
            .collect::<VecDeque<_>>();
        swings
    }

    #[test]
    fn bullish_bos_breaks_higher_high() {
        let bars = vec![
            b(0, 1.10, 1.00, 1.05),
            b(1, 1.20, 1.05, 1.15),
            b(2, 1.12, 1.01, 1.08),
            b(3, 1.25, 1.10, 1.20),
            b(4, 1.18, 1.08, 1.12),
            b(5, 1.18, 1.09, 1.12),
            b(6, 1.30, 1.15, 1.28),
        ];
        let swings = ctx_with_swings(&bars[..6]);
        let mut d = BosDetector::new("EURUSD", Timeframe::M5, BosConfig::default());
        let evs = d.on_closed(&bars, &DetectorCtx::new(&swings));
        assert!(
            matches!(evs.first(), Some(StructureEvent::New(IctStructure::Bos(b))) if b.direction == Direction::Bullish)
        );
    }

    #[test]
    fn wick_only_does_not_trigger() {
        let bars = vec![
            b(0, 1.10, 1.00, 1.05),
            b(1, 1.20, 1.05, 1.15),
            b(2, 1.12, 1.01, 1.08),
            b(3, 1.25, 1.10, 1.20),
            b(4, 1.18, 1.08, 1.12),
            b(5, 1.18, 1.09, 1.12),
            b(6, 1.31, 1.15, 1.19),
        ];
        let swings = ctx_with_swings(&bars[..6]);
        let mut d = BosDetector::new("EURUSD", Timeframe::M5, BosConfig::default());
        assert!(d.on_closed(&bars, &DetectorCtx::new(&swings)).is_empty());
    }

    #[test]
    fn bearish_bos_breaks_lower_low() {
        let swings = manual_swings(&[
            (1, 1.30, SwingKind::High),
            (2, 1.10, SwingKind::Low),
            (3, 1.20, SwingKind::High),
            (4, 1.00, SwingKind::Low),
        ]);
        let mut d = BosDetector::new("EURUSD", Timeframe::M5, BosConfig::default());
        let evs = d.on_closed(&[b(5, 1.02, 0.98, 0.99)], &DetectorCtx::new(&swings));
        assert!(
            matches!(evs.first(), Some(StructureEvent::New(IctStructure::Bos(b))) if b.direction == Direction::Bearish && b.swing_ts == 4)
        );
    }

    #[test]
    fn same_swing_only_triggers_once() {
        let swings = manual_swings(&[
            (1, 1.20, SwingKind::High),
            (2, 1.00, SwingKind::Low),
            (3, 1.30, SwingKind::High),
            (4, 1.10, SwingKind::Low),
        ]);
        let mut d = BosDetector::new("EURUSD", Timeframe::M5, BosConfig::default());
        assert_eq!(
            d.on_closed(&[b(5, 1.35, 1.20, 1.31)], &DetectorCtx::new(&swings))
                .len(),
            1
        );
        assert!(
            d.on_closed(&[b(6, 1.36, 1.21, 1.32)], &DetectorCtx::new(&swings))
                .is_empty(),
            "same swing high must not emit duplicate BoS"
        );
    }

    #[test]
    fn same_break_is_not_both_bos_and_mss() {
        let swings = manual_swings(&[
            (1, 1.20, SwingKind::High),
            (2, 1.00, SwingKind::Low),
            (3, 1.30, SwingKind::High),
            (4, 1.10, SwingKind::Low),
        ]);
        let bars = [b(5, 1.35, 1.20, 1.31)];
        let ctx = DetectorCtx::new(&swings);
        let mut bos = BosDetector::new("EURUSD", Timeframe::M5, BosConfig::default());
        let mut mss = MssDetector::new("EURUSD", Timeframe::M5, MssConfig::default());

        assert!(matches!(
            bos.on_closed(&bars, &ctx).first(),
            Some(StructureEvent::New(IctStructure::Bos(_)))
        ));
        assert!(
            mss.on_closed(&bars, &ctx).is_empty(),
            "with-trend BoS break must not also emit MSS"
        );
    }
}
