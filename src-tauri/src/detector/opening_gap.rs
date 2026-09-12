use serde_json::Value;

use crate::types::{Bar, Timeframe};

use super::atr::pip_size;
use super::gap_state::step_gap_state;
use super::types::{
    structure_id, Direction, GapDirection, GapState, GapZone, IctStructure, OpeningGapKind,
    StructureEvent,
};
use super::{Detector, DetectorCtx};

#[derive(Clone, Debug)]
pub struct OpeningGapConfig {
    pub enabled: bool,
    pub min_nwog_size_pips: f64,
    pub min_ndog_size_pips: f64,
    pub pip_size: f64,
}

impl OpeningGapConfig {
    pub fn for_symbol(symbol: &str) -> Self {
        Self {
            pip_size: pip_size(symbol),
            ..Self::default()
        }
    }
}

impl Default for OpeningGapConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_nwog_size_pips: 2.0,
            min_ndog_size_pips: 0.5,
            pip_size: 0.0001,
        }
    }
}

pub struct OpeningGapDetector {
    symbol: String,
    cfg: OpeningGapConfig,
    prev_closed: Option<Bar>,
    tracked: Vec<GapZone>,
}

impl OpeningGapDetector {
    pub fn new(symbol: impl Into<String>, cfg: OpeningGapConfig) -> Self {
        Self {
            symbol: symbol.into(),
            cfg,
            prev_closed: None,
            tracked: Vec::new(),
        }
    }

    fn build_gap(&self, kind: OpeningGapKind, prev: &Bar, curr: &Bar) -> Option<GapZone> {
        let gap = curr.open - prev.close;
        if gap == 0.0 {
            return None;
        }
        let min_pips = match kind {
            OpeningGapKind::Nwog => self.cfg.min_nwog_size_pips,
            OpeningGapKind::Ndog => self.cfg.min_ndog_size_pips,
        };
        if gap.abs() < min_pips.max(0.0) * self.cfg.pip_size {
            return None;
        }
        let (direction, price_low, price_high) = if gap > 0.0 {
            (GapDirection::Up, prev.close, curr.open)
        } else {
            (GapDirection::Down, curr.open, prev.close)
        };
        let kind_tag = match kind {
            OpeningGapKind::Nwog => "nwog",
            OpeningGapKind::Ndog => "ndog",
        };
        let id = structure_id(&[&self.symbol, kind_tag, &curr.ts.to_string()]);
        Some(GapZone {
            id,
            symbol: self.symbol.clone(),
            tf: Timeframe::M1,
            kind,
            direction,
            ts_start: curr.ts,
            ts_end: curr.ts,
            prev_close_ts: prev.ts,
            new_open_ts: curr.ts,
            prev_close: prev.close,
            new_open: curr.open,
            price_low,
            price_high,
            state: GapState::Active,
        })
    }

    fn structure_for(gap: GapZone) -> IctStructure {
        match gap.kind {
            OpeningGapKind::Nwog => IctStructure::Nwog(gap),
            OpeningGapKind::Ndog => IctStructure::Ndog(gap),
        }
    }

    fn update_tracked(&mut self, bar: &Bar) -> Vec<StructureEvent> {
        let mut out = Vec::new();
        for gap in self.tracked.iter_mut() {
            let direction = match gap.direction {
                GapDirection::Up => Direction::Bullish,
                GapDirection::Down => Direction::Bearish,
            };
            let next = step_gap_state(
                direction,
                bar.low,
                bar.high,
                gap.price_low,
                gap.price_high,
                gap.state,
            );
            if next != gap.state {
                gap.state = next;
                out.push(StructureEvent::Update(Self::structure_for(gap.clone())));
            }
        }
        self.tracked.retain(|gap| gap.state != GapState::Filled);
        out
    }
}

impl Detector for OpeningGapDetector {
    fn name(&self) -> &'static str {
        "opening_gap"
    }

    fn on_closed(&mut self, bars: &[Bar], _ctx: &DetectorCtx<'_>) -> Vec<StructureEvent> {
        if !self.cfg.enabled {
            return Vec::new();
        }
        let Some(curr) = bars.last() else {
            return Vec::new();
        };
        if curr.tf != Timeframe::M1 {
            return Vec::new();
        }
        if self
            .prev_closed
            .as_ref()
            .is_some_and(|prev| prev.ts == curr.ts)
        {
            return Vec::new();
        }

        let mut out = self.update_tracked(curr);
        if let Some(prev) = self.prev_closed.clone() {
            let crossed_day =
                Timeframe::D1.boundary_align(prev.ts) != Timeframe::D1.boundary_align(curr.ts);
            let crossed_week =
                Timeframe::W1.boundary_align(prev.ts) != Timeframe::W1.boundary_align(curr.ts);
            if crossed_day {
                if let Some(gap) = self.build_gap(OpeningGapKind::Ndog, &prev, curr) {
                    self.tracked.push(gap.clone());
                    out.push(StructureEvent::New(Self::structure_for(gap)));
                }
            }
            if crossed_week {
                if let Some(gap) = self.build_gap(OpeningGapKind::Nwog, &prev, curr) {
                    self.tracked.push(gap.clone());
                    out.push(StructureEvent::New(Self::structure_for(gap)));
                }
            }
        }
        self.prev_closed = Some(curr.clone());
        out
    }

    fn apply_param(&mut self, key: &str, value: &Value) -> bool {
        match key {
            "min_nwog_size_pips" => value
                .as_f64()
                .map(|v| self.cfg.min_nwog_size_pips = v.max(0.0))
                .is_some(),
            "min_ndog_size_pips" => value
                .as_f64()
                .map(|v| self.cfg.min_ndog_size_pips = v.max(0.0))
                .is_some(),
            _ => false,
        }
    }

    fn reset(&mut self) {
        self.prev_closed = None;
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
            tf: Timeframe::M1,
            ts,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: 0.0,
        }
    }

    fn detector() -> OpeningGapDetector {
        OpeningGapDetector::new(
            "EURUSD",
            OpeningGapConfig {
                min_nwog_size_pips: 2.0,
                min_ndog_size_pips: 0.5,
                pip_size: 0.0001,
                enabled: true,
            },
        )
    }

    fn evs_for(prev: Bar, curr: Bar) -> Vec<StructureEvent> {
        let swings = SwingSeries::default();
        let mut det = detector();
        det.on_closed(&[prev.clone()], &DetectorCtx::new(&swings));
        det.on_closed(&[prev, curr], &DetectorCtx::new(&swings))
    }

    #[test]
    fn ndog_cross_day_emits_when_over_threshold() {
        let prev = b(
            Timeframe::D1.boundary_align(1_700_000_000_000) - 60_000,
            1.0,
            1.0,
            1.0,
            1.1000,
        );
        let curr = b(
            Timeframe::D1.boundary_align(1_700_000_000_000),
            1.1002,
            1.1004,
            1.1001,
            1.1003,
        );
        let evs = evs_for(prev, curr);
        assert!(
            matches!(evs.first(), Some(StructureEvent::New(IctStructure::Ndog(g))) if g.direction == GapDirection::Up)
        );
    }

    #[test]
    fn ndog_below_threshold_does_not_emit() {
        let boundary = Timeframe::D1.boundary_align(1_700_000_000_000);
        let evs = evs_for(
            b(boundary - 60_000, 1.0, 1.0, 1.0, 1.10000),
            b(boundary, 1.10002, 1.1001, 1.1000, 1.1000),
        );
        assert!(evs.is_empty());
    }

    #[test]
    fn nwog_cross_week_emits_when_over_threshold() {
        let boundary = Timeframe::W1.boundary_align(1_700_000_000_000);
        let evs = evs_for(
            b(boundary - 60_000, 1.0, 1.0, 1.0, 1.1000),
            b(boundary, 1.1004, 1.1005, 1.1002, 1.1003),
        );
        assert!(evs
            .iter()
            .any(|ev| matches!(ev, StructureEvent::New(IctStructure::Nwog(_)))));
    }

    #[test]
    fn same_bar_can_emit_ndog_and_nwog() {
        let boundary = Timeframe::W1.boundary_align(1_700_000_000_000);
        let evs = evs_for(
            b(boundary - 60_000, 1.0, 1.0, 1.0, 1.1000),
            b(boundary, 1.1004, 1.1005, 1.1002, 1.1003),
        );
        assert!(evs
            .iter()
            .any(|ev| matches!(ev, StructureEvent::New(IctStructure::Ndog(_)))));
        assert!(evs
            .iter()
            .any(|ev| matches!(ev, StructureEvent::New(IctStructure::Nwog(_)))));
    }

    #[test]
    fn gap_up_mid_then_filled() {
        let boundary = Timeframe::D1.boundary_align(1_700_000_000_000);
        let swings = SwingSeries::default();
        let mut det = detector();
        det.on_closed(
            &[b(boundary - 60_000, 1.0, 1.0, 1.0, 1.1000)],
            &DetectorCtx::new(&swings),
        );
        det.on_closed(
            &[
                b(boundary - 60_000, 1.0, 1.0, 1.0, 1.1000),
                b(boundary, 1.1004, 1.1005, 1.1004, 1.1005),
            ],
            &DetectorCtx::new(&swings),
        );
        let evs = det.on_closed(
            &[
                b(boundary - 60_000, 1.0, 1.0, 1.0, 1.1000),
                b(boundary, 1.1004, 1.1005, 1.1004, 1.1005),
                b(boundary + 60_000, 1.1005, 1.1005, 1.1002, 1.1003),
            ],
            &DetectorCtx::new(&swings),
        );
        assert!(evs.iter().any(|ev| matches!(ev, StructureEvent::Update(IctStructure::Ndog(g)) if g.state == GapState::Mitigated50)));
        let evs = det.on_closed(
            &[
                b(boundary - 60_000, 1.0, 1.0, 1.0, 1.1000),
                b(boundary, 1.1004, 1.1005, 1.1004, 1.1005),
                b(boundary + 60_000, 1.1005, 1.1005, 1.1002, 1.1003),
                b(boundary + 120_000, 1.1003, 1.1004, 1.0999, 1.1000),
            ],
            &DetectorCtx::new(&swings),
        );
        assert!(evs.iter().any(|ev| matches!(ev, StructureEvent::Update(IctStructure::Ndog(g)) if g.state == GapState::Filled)));
    }

    #[test]
    fn gap_down_mid_and_filled() {
        let boundary = Timeframe::D1.boundary_align(1_700_000_000_000);
        let swings = SwingSeries::default();
        let mut det = detector();
        det.on_closed(
            &[b(boundary - 60_000, 1.0, 1.0, 1.0, 1.1004)],
            &DetectorCtx::new(&swings),
        );
        det.on_closed(
            &[
                b(boundary - 60_000, 1.0, 1.0, 1.0, 1.1004),
                b(boundary, 1.1000, 1.1000, 1.0998, 1.0999),
            ],
            &DetectorCtx::new(&swings),
        );
        let evs = det.on_closed(
            &[
                b(boundary - 60_000, 1.0, 1.0, 1.0, 1.1004),
                b(boundary, 1.1000, 1.1000, 1.0998, 1.0999),
                b(boundary + 60_000, 1.0999, 1.1002, 1.0998, 1.1001),
            ],
            &DetectorCtx::new(&swings),
        );
        assert!(evs.iter().any(|ev| matches!(ev, StructureEvent::Update(IctStructure::Ndog(g)) if g.state == GapState::Mitigated50)));
        let evs = det.on_closed(
            &[
                b(boundary - 60_000, 1.0, 1.0, 1.0, 1.1004),
                b(boundary, 1.1000, 1.1000, 1.0998, 1.0999),
                b(boundary + 60_000, 1.0999, 1.1002, 1.0998, 1.1001),
                b(boundary + 120_000, 1.1001, 1.1005, 1.1000, 1.1004),
            ],
            &DetectorCtx::new(&swings),
        );
        assert!(evs.iter().any(|ev| matches!(ev, StructureEvent::Update(IctStructure::Ndog(g)) if g.state == GapState::Filled)));
    }
}
