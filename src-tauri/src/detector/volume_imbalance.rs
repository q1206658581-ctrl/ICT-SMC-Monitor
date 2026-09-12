use serde_json::Value;

use crate::types::{Bar, Timeframe};

use super::gap_state::step_gap_state;
use super::types::{
    structure_id, Direction, GapState, IctStructure, StructureEvent, VolumeImbalance,
};
use super::{Detector, DetectorCtx};

#[derive(Clone, Debug)]
pub struct VolumeImbalanceConfig {
    pub min_size_pips: f64,
    pub pip_size: f64,
}

impl Default for VolumeImbalanceConfig {
    fn default() -> Self {
        Self {
            min_size_pips: 0.5,
            pip_size: 0.0001,
        }
    }
}

pub struct VolumeImbalanceDetector {
    symbol: String,
    tf: Timeframe,
    cfg: VolumeImbalanceConfig,
    tracked: Vec<VolumeImbalance>,
}

impl VolumeImbalanceDetector {
    pub fn new(symbol: impl Into<String>, tf: Timeframe, cfg: VolumeImbalanceConfig) -> Self {
        Self {
            symbol: symbol.into(),
            tf,
            cfg,
            tracked: Vec::new(),
        }
    }
}

impl Detector for VolumeImbalanceDetector {
    fn name(&self) -> &'static str {
        "volume_imbalance"
    }

    fn on_closed(&mut self, bars: &[Bar], _ctx: &DetectorCtx<'_>) -> Vec<StructureEvent> {
        let Some(last) = bars.last() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for vi in self.tracked.iter_mut() {
            let next = step_gap_state(
                vi.direction,
                last.low,
                last.high,
                vi.price_low,
                vi.price_high,
                vi.state,
            );
            if next != vi.state {
                vi.state = next;
                out.push(StructureEvent::Update(IctStructure::VolumeImbalance(
                    vi.clone(),
                )));
            }
        }
        self.tracked.retain(|vi| vi.state != GapState::Filled);
        if bars.len() < 2 {
            return out;
        }
        let prev = &bars[bars.len() - 2];
        let gap = last.open - prev.close;
        let min_gap = self.cfg.min_size_pips.max(0.0) * self.cfg.pip_size;
        if gap.abs() < min_gap {
            return out;
        }
        let (direction, price_low, price_high) = if gap > 0.0 {
            (Direction::Bullish, prev.close, last.open)
        } else {
            (Direction::Bearish, last.open, prev.close)
        };
        let id = structure_id(&[
            &self.symbol,
            self.tf.tag(),
            "volume_imbalance",
            &prev.ts.to_string(),
            &last.ts.to_string(),
            &format!("{price_low:.8}"),
            &format!("{price_high:.8}"),
        ]);
        let vi = VolumeImbalance {
            id,
            symbol: self.symbol.clone(),
            tf: self.tf,
            direction,
            ts_open: prev.ts,
            ts_confirm: last.ts,
            price_low,
            price_high,
            state: GapState::Active,
        };
        self.tracked.push(vi.clone());
        out.push(StructureEvent::New(IctStructure::VolumeImbalance(vi)));
        out
    }

    fn apply_param(&mut self, key: &str, value: &Value) -> bool {
        match key {
            "min_size_pips" => value
                .as_f64()
                .map(|v| self.cfg.min_size_pips = v.max(0.0))
                .is_some(),
            _ => false,
        }
    }

    fn reset(&mut self) {
        self.tracked.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::swing::SwingSeries;

    fn b(ts: i64, o: f64, h: f64, l: f64, c: f64) -> Bar {
        Bar {
            symbol: "EURUSD".into(),
            tf: Timeframe::M5,
            ts,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: 0.0,
        }
    }

    #[test]
    fn bullish_vi_formation_and_states() {
        let mut d = VolumeImbalanceDetector::new(
            "EURUSD",
            Timeframe::M5,
            VolumeImbalanceConfig {
                min_size_pips: 0.5,
                pip_size: 0.0001,
            },
        );
        let swings = SwingSeries::default();
        let evs = d.on_closed(
            &[
                b(0, 1.0, 1.0, 1.0, 1.1000),
                b(1, 1.1002, 1.1010, 1.1002, 1.1008),
            ],
            &DetectorCtx::new(&swings),
        );
        assert!(
            matches!(evs.first(), Some(StructureEvent::New(IctStructure::VolumeImbalance(v))) if v.direction == Direction::Bullish)
        );
        let evs = d.on_closed(
            &[
                b(0, 1.0, 1.0, 1.0, 1.1000),
                b(1, 1.1002, 1.1010, 1.1002, 1.1008),
                b(2, 1.1008, 1.1009, 1.1001, 1.1004),
            ],
            &DetectorCtx::new(&swings),
        );
        assert!(evs.iter().any(|e| matches!(e, StructureEvent::Update(IctStructure::VolumeImbalance(v)) if v.state == GapState::Mitigated50 || v.state == GapState::Filled)));
    }

    #[test]
    fn touch_midpoint_sets_mitigated50_without_fill() {
        let mut d = VolumeImbalanceDetector::new(
            "EURUSD",
            Timeframe::M5,
            VolumeImbalanceConfig {
                min_size_pips: 0.5,
                pip_size: 0.0001,
            },
        );
        let swings = SwingSeries::default();
        d.on_closed(
            &[
                b(0, 1.0, 1.0, 1.0, 1.1000),
                b(1, 1.1004, 1.1010, 1.1004, 1.1008),
            ],
            &DetectorCtx::new(&swings),
        );

        let evs = d.on_closed(
            &[
                b(0, 1.0, 1.0, 1.0, 1.1000),
                b(1, 1.1004, 1.1010, 1.1004, 1.1008),
                b(2, 1.1008, 1.1009, 1.1002, 1.1006),
            ],
            &DetectorCtx::new(&swings),
        );
        assert!(evs.iter().any(|e| matches!(
            e,
            StructureEvent::Update(IctStructure::VolumeImbalance(v)) if v.state == GapState::Mitigated50
        )));
        assert!(!evs.iter().any(|e| matches!(
            e,
            StructureEvent::Update(IctStructure::VolumeImbalance(v)) if v.state == GapState::Filled
        )));
    }

    #[test]
    fn min_size_filters_small_gap() {
        let mut d = VolumeImbalanceDetector::new(
            "EURUSD",
            Timeframe::M5,
            VolumeImbalanceConfig {
                min_size_pips: 1.0,
                pip_size: 0.0001,
            },
        );
        let swings = SwingSeries::default();
        let evs = d.on_closed(
            &[
                b(0, 1.0, 1.0, 1.0, 1.10000),
                b(1, 1.10005, 1.1010, 1.1000, 1.1008),
            ],
            &DetectorCtx::new(&swings),
        );
        assert!(evs.is_empty());
    }

    #[test]
    fn bearish_vi_formation_and_filled() {
        let mut d = VolumeImbalanceDetector::new(
            "EURUSD",
            Timeframe::M5,
            VolumeImbalanceConfig {
                min_size_pips: 0.5,
                pip_size: 0.0001,
            },
        );
        let swings = SwingSeries::default();
        let evs = d.on_closed(
            &[
                b(0, 1.0, 1.0, 1.0, 1.1008),
                b(1, 1.1000, 1.1000, 1.0990, 1.0995),
            ],
            &DetectorCtx::new(&swings),
        );
        assert!(
            matches!(evs.first(), Some(StructureEvent::New(IctStructure::VolumeImbalance(v))) if v.direction == Direction::Bearish)
        );

        let evs = d.on_closed(
            &[
                b(0, 1.0, 1.0, 1.0, 1.1008),
                b(1, 1.1000, 1.1000, 1.0990, 1.0995),
                b(2, 1.0995, 1.1009, 1.0994, 1.1007),
            ],
            &DetectorCtx::new(&swings),
        );
        assert!(evs.iter().any(|e| matches!(e, StructureEvent::Update(IctStructure::VolumeImbalance(v)) if v.state == GapState::Filled)));
    }
}
