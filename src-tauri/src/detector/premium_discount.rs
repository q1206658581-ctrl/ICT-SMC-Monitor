use serde_json::Value;

use crate::types::{Bar, Timeframe};

use super::atr::pip_size;
use super::swing::{Swing, SwingKind};
use super::types::{structure_id, IctStructure, PdSide, PremiumDiscount, StructureEvent};
use super::{Detector, DetectorCtx};

#[derive(Clone, Debug)]
pub struct PremiumDiscountConfig {
    pub enabled: bool,
    pub equilibrium_tolerance_pips: f64,
}

impl Default for PremiumDiscountConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            equilibrium_tolerance_pips: 1.0,
        }
    }
}

pub struct PremiumDiscountDetector {
    symbol: String,
    tf: Timeframe,
    cfg: PremiumDiscountConfig,
    active: Option<PremiumDiscount>,
}

impl PremiumDiscountDetector {
    pub fn new(symbol: impl Into<String>, tf: Timeframe, cfg: PremiumDiscountConfig) -> Self {
        Self {
            symbol: symbol.into(),
            tf,
            cfg,
            active: None,
        }
    }

    fn build(&self, last: &Bar, high: &Swing, low: &Swing) -> PremiumDiscount {
        let range_start_ts = high.ts.min(low.ts);
        let range_end_ts = high.ts.max(low.ts);
        let high_price = high.price.max(low.price);
        let low_price = high.price.min(low.price);
        let equilibrium = (high_price + low_price) / 2.0;
        let tolerance = self.cfg.equilibrium_tolerance_pips.max(0.0) * pip_size(&self.symbol);
        let current_side = side_for_price(last.close, equilibrium, tolerance);
        let id = structure_id(&[
            &self.symbol,
            self.tf.tag(),
            "premium_discount",
            &range_start_ts.to_string(),
            &range_end_ts.to_string(),
        ]);
        PremiumDiscount {
            id,
            symbol: self.symbol.clone(),
            tf: self.tf,
            range_start_ts,
            range_end_ts,
            high: high_price,
            low: low_price,
            equilibrium,
            current_side,
            current_price: last.close,
        }
    }
}

impl Detector for PremiumDiscountDetector {
    fn name(&self) -> &'static str {
        "premium_discount"
    }

    fn on_closed(&mut self, bars: &[Bar], ctx: &DetectorCtx<'_>) -> Vec<StructureEvent> {
        if !self.cfg.enabled {
            return Vec::new();
        }
        let Some(last) = bars.last() else {
            return Vec::new();
        };
        let Some((high, low)) = latest_swing_range(ctx) else {
            return Vec::new();
        };
        if (high.price - low.price).abs() <= f64::EPSILON {
            return Vec::new();
        }

        let next = self.build(last, high, low);
        let Some(prev) = self.active.as_ref() else {
            self.active = Some(next.clone());
            return vec![StructureEvent::New(IctStructure::PremiumDiscount(next))];
        };

        if prev.id != next.id {
            let old_id = prev.id.clone();
            self.active = Some(next.clone());
            return vec![
                StructureEvent::Invalidated {
                    id: old_id,
                    kind: "premium_discount".into(),
                },
                StructureEvent::New(IctStructure::PremiumDiscount(next)),
            ];
        }

        if prev.current_side != next.current_side
            || (prev.current_price - next.current_price).abs() > f64::EPSILON
        {
            self.active = Some(next.clone());
            return vec![StructureEvent::Update(IctStructure::PremiumDiscount(next))];
        }
        Vec::new()
    }

    fn apply_param(&mut self, key: &str, value: &Value) -> bool {
        match key {
            "enabled" => value.as_bool().map(|v| self.cfg.enabled = v).is_some(),
            "equilibrium_tolerance_pips" => value
                .as_f64()
                .map(|v| self.cfg.equilibrium_tolerance_pips = v.max(0.0))
                .is_some(),
            _ => false,
        }
    }

    fn reset(&mut self) {
        self.active = None;
    }
}

fn latest_swing_range<'a>(ctx: &'a DetectorCtx<'_>) -> Option<(&'a Swing, &'a Swing)> {
    let mut high: Option<&Swing> = None;
    let mut low: Option<&Swing> = None;
    for swing in ctx.swings.swings.iter().rev() {
        match swing.kind {
            SwingKind::High => {
                if high.map(|h| swing.price > h.price).unwrap_or(true) {
                    high = Some(swing);
                }
            }
            SwingKind::Low => {
                if low.map(|l| swing.price < l.price).unwrap_or(true) {
                    low = Some(swing);
                }
            }
        }
        if let (Some(h), Some(l)) = (high, low) {
            return Some((h, l));
        }
    }
    None
}

fn side_for_price(price: f64, equilibrium: f64, tolerance: f64) -> PdSide {
    if price > equilibrium + tolerance {
        PdSide::Premium
    } else if price < equilibrium - tolerance {
        PdSide::Discount
    } else {
        PdSide::Equilibrium
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::swing::SwingSeries;

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
        for end in 1..=bars.len() {
            swings.on_closed_bar(&bars[..end]);
        }
        swings
    }

    fn pd_from_event(evs: &[StructureEvent]) -> PremiumDiscount {
        evs.iter()
            .find_map(|ev| match ev {
                StructureEvent::New(IctStructure::PremiumDiscount(pd))
                | StructureEvent::Update(IctStructure::PremiumDiscount(pd)) => Some(pd.clone()),
                _ => None,
            })
            .expect("premium discount event")
    }

    #[test]
    fn swing_range_high_low_are_identified() {
        let bars = vec![
            b(0, 1.0, 1.0, 1.0),
            b(1, 1.2, 1.1, 1.15),
            b(2, 1.1, 1.05, 1.08),
            b(3, 1.3, 1.2, 1.28),
            b(4, 1.2, 1.1, 1.15),
        ];
        let swings = swings_for(&bars);
        let mut detector =
            PremiumDiscountDetector::new("EURUSD", Timeframe::M5, PremiumDiscountConfig::default());
        let evs = detector.on_closed(&bars, &DetectorCtx::new(&swings));
        let pd = pd_from_event(&evs);
        assert!((pd.high - 1.30).abs() < 1e-9);
        assert!((pd.low - 1.05).abs() < 1e-9);
        assert_eq!(pd.range_start_ts, 2);
        assert_eq!(pd.range_end_ts, 3);
        assert!((pd.equilibrium - 1.175).abs() < 1e-9);
    }

    #[test]
    fn close_above_eq_plus_tolerance_is_premium() {
        let bars = vec![
            b(0, 1.0, 1.0, 1.0),
            b(1, 1.2, 1.1, 1.15),
            b(2, 1.1, 1.05, 1.08),
            b(3, 1.3, 1.2, 1.28),
            b(4, 1.2, 1.1, 1.20),
        ];
        let swings = swings_for(&bars);
        let mut detector =
            PremiumDiscountDetector::new("EURUSD", Timeframe::M5, PremiumDiscountConfig::default());
        let pd = pd_from_event(&detector.on_closed(&bars, &DetectorCtx::new(&swings)));
        assert_eq!(pd.current_side, PdSide::Premium);
    }

    #[test]
    fn close_below_eq_minus_tolerance_is_discount() {
        let bars = vec![
            b(0, 1.0, 1.0, 1.0),
            b(1, 1.2, 1.1, 1.15),
            b(2, 1.1, 1.05, 1.08),
            b(3, 1.3, 1.2, 1.28),
            b(4, 1.2, 1.1, 1.16),
        ];
        let swings = swings_for(&bars);
        let mut detector =
            PremiumDiscountDetector::new("EURUSD", Timeframe::M5, PremiumDiscountConfig::default());
        let pd = pd_from_event(&detector.on_closed(&bars, &DetectorCtx::new(&swings)));
        assert_eq!(pd.current_side, PdSide::Discount);
    }

    #[test]
    fn close_within_tolerance_is_equilibrium() {
        let bars = vec![
            b(0, 1.0, 1.0, 1.0),
            b(1, 1.2, 1.1, 1.15),
            b(2, 1.1, 1.05, 1.08),
            b(3, 1.3, 1.2, 1.28),
            b(4, 1.2, 1.1, 1.17505),
        ];
        let swings = swings_for(&bars);
        let mut detector =
            PremiumDiscountDetector::new("EURUSD", Timeframe::M5, PremiumDiscountConfig::default());
        let pd = pd_from_event(&detector.on_closed(&bars, &DetectorCtx::new(&swings)));
        assert_eq!(pd.current_side, PdSide::Equilibrium);
    }

    #[test]
    fn same_range_side_change_emits_update_not_new_id() {
        let bars = vec![
            b(0, 1.0, 1.0, 1.0),
            b(1, 1.2, 1.1, 1.15),
            b(2, 1.1, 1.05, 1.08),
            b(3, 1.3, 1.2, 1.28),
            b(4, 1.2, 1.1, 1.20),
        ];
        let swings = swings_for(&bars);
        let mut detector =
            PremiumDiscountDetector::new("EURUSD", Timeframe::M5, PremiumDiscountConfig::default());
        let first = pd_from_event(&detector.on_closed(&bars, &DetectorCtx::new(&swings)));
        let mut next_bars = bars.clone();
        next_bars[4] = b(4, 1.2, 1.1, 1.16);
        let evs = detector.on_closed(&next_bars, &DetectorCtx::new(&swings));
        let second = pd_from_event(&evs);
        assert!(matches!(
            evs.first(),
            Some(StructureEvent::Update(IctStructure::PremiumDiscount(_)))
        ));
        assert_eq!(first.id, second.id);
        assert_eq!(second.current_side, PdSide::Discount);
    }

    #[test]
    fn new_swing_range_invalidates_old_pd() {
        let bars = vec![
            b(0, 1.0, 1.0, 1.0),
            b(1, 1.2, 1.1, 1.15),
            b(2, 1.1, 1.05, 1.08),
            b(3, 1.3, 1.2, 1.28),
            b(4, 1.2, 1.1, 1.18),
        ];
        let swings = swings_for(&bars);
        let mut detector =
            PremiumDiscountDetector::new("EURUSD", Timeframe::M5, PremiumDiscountConfig::default());
        let first = pd_from_event(&detector.on_closed(&bars, &DetectorCtx::new(&swings)));
        let next_bars = vec![
            b(0, 1.0, 1.0, 1.0),
            b(1, 1.2, 1.1, 1.15),
            b(2, 1.1, 1.05, 1.08),
            b(3, 1.3, 1.2, 1.28),
            b(4, 1.2, 1.1, 1.18),
            b(5, 1.35, 1.2, 1.32),
            b(6, 1.25, 1.0, 1.05),
            b(7, 1.15, 1.08, 1.10),
        ];
        let next_swings = swings_for(&next_bars);
        let evs = detector.on_closed(&next_bars, &DetectorCtx::new(&next_swings));
        assert!(evs.iter().any(|ev| matches!(            ev,            StructureEvent::Invalidated { id, kind } if id == &first.id && kind == "premium_discount"        )));
        let next = pd_from_event(&evs);
        assert_ne!(first.id, next.id);
    }
}
