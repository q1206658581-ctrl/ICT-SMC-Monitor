use std::collections::HashSet;

use super::swing::SwingKind;
use super::types::{
    structure_id, IctStructure, LiquidityPoolKind, LiquiditySide, LiquiditySweep, StructureEvent,
};
use super::{Detector, DetectorCtx};
use crate::types::{Bar, Timeframe};

pub struct LiquiditySweepDetector {
    symbol: String,
    tf: Timeframe,
    swept_levels: HashSet<String>,
}

impl LiquiditySweepDetector {
    pub fn new(symbol: impl Into<String>, tf: Timeframe) -> Self {
        Self {
            symbol: symbol.into(),
            tf,
            swept_levels: HashSet::new(),
        }
    }

    fn emit_sweep(
        &mut self,
        out: &mut Vec<StructureEvent>,
        last: &Bar,
        side: LiquiditySide,
        pool_kind: LiquidityPoolKind,
        level_ts: i64,
        level_price: f64,
        sweep_price: f64,
    ) {
        let key = format!("{:?}|{}|{}", pool_kind, level_ts, level_price);
        if !self.swept_levels.insert(key) {
            return;
        }
        let id = structure_id(&[
            &self.symbol,
            self.tf.tag(),
            "liquidity_sweep",
            match pool_kind {
                LiquidityPoolKind::SwingHigh => "swing_high",
                LiquidityPoolKind::SwingLow => "swing_low",
                LiquidityPoolKind::EqualHighs => "equal_highs",
                LiquidityPoolKind::EqualLows => "equal_lows",
                LiquidityPoolKind::Pdh => "pdh",
                LiquidityPoolKind::Pdl => "pdl",
            },
            &level_ts.to_string(),
            &last.ts.to_string(),
        ]);
        out.push(StructureEvent::New(IctStructure::LiquiditySweep(
            LiquiditySweep {
                id,
                symbol: self.symbol.clone(),
                tf: self.tf,
                side,
                pool_kind,
                sweep_ts: last.ts,
                sweep_price,
                level_ts,
                level_price,
                close_price: last.close,
            },
        )));
    }
}

impl Detector for LiquiditySweepDetector {
    fn name(&self) -> &'static str {
        "liquidity"
    }

    fn on_closed(&mut self, bars: &[Bar], ctx: &DetectorCtx<'_>) -> Vec<StructureEvent> {
        let mut out = Vec::new();
        let Some(last) = bars.last() else {
            return out;
        };
        if let Some(swing_high) = ctx.swings.last_of(SwingKind::High) {
            if last.high > swing_high.price && last.close < swing_high.price {
                self.emit_sweep(
                    &mut out,
                    last,
                    LiquiditySide::BuySide,
                    LiquidityPoolKind::SwingHigh,
                    swing_high.ts,
                    swing_high.price,
                    last.high,
                );
            }
        }
        if let Some(swing_low) = ctx.swings.last_of(SwingKind::Low) {
            if last.low < swing_low.price && last.close > swing_low.price {
                self.emit_sweep(
                    &mut out,
                    last,
                    LiquiditySide::SellSide,
                    LiquidityPoolKind::SwingLow,
                    swing_low.ts,
                    swing_low.price,
                    last.low,
                );
            }
        }
        out
    }

    fn reset(&mut self) {
        self.swept_levels.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::swing::{Swing, SwingSeries};

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
    fn buy_side_swing_sweep_emits_once() {
        let mut swings = SwingSeries::new(2);
        swings.swings.push_back(Swing {
            ts: 1,
            price: 1.1000,
            kind: SwingKind::High,
        });
        let ctx = DetectorCtx::new(&swings);
        let mut det = LiquiditySweepDetector::new("EURUSD", Timeframe::M5);
        let bars = [b(2, 1.0990, 1.1010, 1.0980, 1.0995)];
        let first = det.on_closed(&bars, &ctx);
        let second = det.on_closed(&[b(3, 1.0990, 1.1020, 1.0980, 1.0994)], &ctx);
        assert!(
            matches!(first.first(), Some(StructureEvent::New(IctStructure::LiquiditySweep(s))) if s.side == LiquiditySide::BuySide)
        );
        assert!(
            second.is_empty(),
            "same swing level should not emit twice: {second:?}"
        );
    }

    #[test]
    fn wick_without_close_back_inside_does_not_sweep() {
        let mut swings = SwingSeries::new(2);
        swings.swings.push_back(Swing {
            ts: 1,
            price: 1.1000,
            kind: SwingKind::High,
        });
        let ctx = DetectorCtx::new(&swings);
        let mut det = LiquiditySweepDetector::new("EURUSD", Timeframe::M5);
        let evs = det.on_closed(&[b(2, 1.0990, 1.1010, 1.0980, 1.1005)], &ctx);
        assert!(
            evs.is_empty(),
            "close above swept high is breakout, not sweep"
        );
    }

    #[test]
    fn sell_side_swing_sweep_emits() {
        let mut swings = SwingSeries::new(2);
        swings.swings.push_back(Swing {
            ts: 1,
            price: 1.1000,
            kind: SwingKind::Low,
        });
        let ctx = DetectorCtx::new(&swings);
        let mut det = LiquiditySweepDetector::new("EURUSD", Timeframe::M5);
        let evs = det.on_closed(&[b(2, 1.1010, 1.1020, 1.0990, 1.1005)], &ctx);
        assert!(
            matches!(evs.first(), Some(StructureEvent::New(IctStructure::LiquiditySweep(s))) if s.side == LiquiditySide::SellSide)
        );
    }
}
