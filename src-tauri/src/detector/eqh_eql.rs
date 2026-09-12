use std::collections::{HashMap, HashSet};

use super::atr::{atr14, pip_size};
use super::swing::SwingKind;
use super::types::{
    structure_id, EqualHighsLows, IctStructure, LiquidityPoolKind, LiquiditySide, LiquiditySweep,
    StructureEvent,
};
use super::{Detector, DetectorCtx};
use crate::types::{Bar, Timeframe};

#[derive(Clone, Debug)]
pub struct EqhEqlConfig {
    pub tolerance_atr_mult: f64,
    pub tolerance_max_pips: f64,
}

impl Default for EqhEqlConfig {
    fn default() -> Self {
        Self {
            tolerance_atr_mult: 0.1,
            tolerance_max_pips: 3.0,
        }
    }
}

pub struct EqhEqlDetector {
    symbol: String,
    tf: Timeframe,
    cfg: EqhEqlConfig,
    pools: HashMap<String, EqualHighsLows>,
    emitted_pairs: HashSet<String>,
}

impl EqhEqlDetector {
    pub fn new(symbol: impl Into<String>, tf: Timeframe, cfg: EqhEqlConfig) -> Self {
        Self {
            symbol: symbol.into(),
            tf,
            cfg,
            pools: HashMap::new(),
            emitted_pairs: HashSet::new(),
        }
    }

    fn tolerance(&self, bars: &[Bar]) -> f64 {
        let pip_tol = self.cfg.tolerance_max_pips * pip_size(&self.symbol);
        atr14(bars)
            .map(|v| (self.cfg.tolerance_atr_mult * v).min(pip_tol))
            .unwrap_or(pip_tol)
    }

    fn maybe_emit_pool(
        &mut self,
        out: &mut Vec<StructureEvent>,
        bars: &[Bar],
        ctx: &DetectorCtx<'_>,
        kind: SwingKind,
    ) {
        let Some((older, newer)) = ctx.swings.last_two(kind) else {
            return;
        };
        let tolerance = self.tolerance(bars);
        if (older.price - newer.price).abs() > tolerance {
            return;
        }
        let side = match kind {
            SwingKind::High => LiquiditySide::BuySide,
            SwingKind::Low => LiquiditySide::SellSide,
        };
        let kind_tag = match kind {
            SwingKind::High => "eqh",
            SwingKind::Low => "eql",
        };
        let id = structure_id(&[
            &self.symbol,
            self.tf.tag(),
            kind_tag,
            &older.ts.to_string(),
            &newer.ts.to_string(),
        ]);
        if !self.emitted_pairs.insert(id.clone()) {
            return;
        }
        let pool = EqualHighsLows {
            id: id.clone(),
            symbol: self.symbol.clone(),
            tf: self.tf,
            side,
            ts_start: older.ts,
            ts_end: newer.ts,
            price: (older.price + newer.price) / 2.0,
            tolerance_price: tolerance,
            confirmed_at_ts: bars
                .last()
                .map(|bar| bar.ts.saturating_add(bar.tf.duration_ms())),
            swept: false,
        };
        self.pools.insert(id, pool.clone());
        out.push(StructureEvent::New(IctStructure::EqualHighsLows(pool)));
    }

    fn sweep_pool(&self, pool: &EqualHighsLows, last: &Bar) -> Option<LiquiditySweep> {
        let (pool_kind, sweep_price, swept) = match pool.side {
            LiquiditySide::BuySide => (
                LiquidityPoolKind::EqualHighs,
                last.high,
                last.high > pool.price && last.close < pool.price,
            ),
            LiquiditySide::SellSide => (
                LiquidityPoolKind::EqualLows,
                last.low,
                last.low < pool.price && last.close > pool.price,
            ),
        };
        if !swept {
            return None;
        }
        let id = structure_id(&[
            &self.symbol,
            self.tf.tag(),
            "liquidity_sweep",
            match pool_kind {
                LiquidityPoolKind::EqualHighs => "equal_highs",
                _ => "equal_lows",
            },
            &pool.id,
            &last.ts.to_string(),
        ]);
        Some(LiquiditySweep {
            id,
            symbol: self.symbol.clone(),
            tf: self.tf,
            side: pool.side,
            pool_kind,
            sweep_ts: last.ts,
            sweep_price,
            level_ts: pool.ts_end,
            level_price: pool.price,
            close_price: last.close,
        })
    }
}

impl Detector for EqhEqlDetector {
    fn name(&self) -> &'static str {
        "liquidity"
    }

    fn on_closed(&mut self, bars: &[Bar], ctx: &DetectorCtx<'_>) -> Vec<StructureEvent> {
        let mut out = Vec::new();
        let Some(last) = bars.last() else {
            return out;
        };
        self.maybe_emit_pool(&mut out, bars, ctx, SwingKind::High);
        self.maybe_emit_pool(&mut out, bars, ctx, SwingKind::Low);

        let ids: Vec<String> = self.pools.keys().cloned().collect();
        for id in ids {
            let Some(pool) = self.pools.get(&id).cloned() else {
                continue;
            };
            if pool.swept || last.ts <= pool.ts_end {
                continue;
            }
            if let Some(sweep) = self.sweep_pool(&pool, last) {
                let mut updated = pool.clone();
                updated.swept = true;
                self.pools.insert(id, updated.clone());
                out.push(StructureEvent::Update(IctStructure::EqualHighsLows(
                    updated,
                )));
                out.push(StructureEvent::New(IctStructure::LiquiditySweep(sweep)));
            }
        }
        out
    }

    fn apply_param(&mut self, key: &str, value: &serde_json::Value) -> bool {
        match key {
            "eq_tolerance_atr_mult" => value
                .as_f64()
                .map(|v| self.cfg.tolerance_atr_mult = v)
                .is_some(),
            "eq_tolerance_max_pips" => value
                .as_f64()
                .map(|v| self.cfg.tolerance_max_pips = v)
                .is_some(),
            _ => false,
        }
    }

    fn reset(&mut self) {
        self.pools.clear();
        self.emitted_pairs.clear();
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
    fn equal_highs_within_tolerance_emit_pool() {
        let mut swings = SwingSeries::new(2);
        swings.swings.push_back(Swing {
            ts: 1,
            price: 1.10000,
            kind: SwingKind::High,
        });
        swings.swings.push_back(Swing {
            ts: 2,
            price: 1.10020,
            kind: SwingKind::High,
        });
        let ctx = DetectorCtx::new(&swings);
        let mut det = EqhEqlDetector::new("EURUSD", Timeframe::M5, EqhEqlConfig::default());
        let evs = det.on_closed(&[b(3, 1.0, 1.0, 1.0, 1.0)], &ctx);
        assert!(
            matches!(evs.first(), Some(StructureEvent::New(IctStructure::EqualHighsLows(e))) if e.side == LiquiditySide::BuySide && e.confirmed_at_ts == Some(3 + Timeframe::M5.duration_ms()))
        );
    }

    #[test]
    fn equal_lows_outside_tolerance_do_not_emit() {
        let mut swings = SwingSeries::new(2);
        swings.swings.push_back(Swing {
            ts: 1,
            price: 1.10000,
            kind: SwingKind::Low,
        });
        swings.swings.push_back(Swing {
            ts: 2,
            price: 1.10050,
            kind: SwingKind::Low,
        });
        let ctx = DetectorCtx::new(&swings);
        let mut det = EqhEqlDetector::new("EURUSD", Timeframe::M5, EqhEqlConfig::default());
        let evs = det.on_closed(&[b(3, 1.0, 1.0, 1.0, 1.0)], &ctx);
        assert!(
            evs.is_empty(),
            "5 pips exceeds default 3-pip fallback tolerance"
        );
    }

    #[test]
    fn equal_highs_sweep_updates_pool_and_emits_sweep() {
        let mut swings = SwingSeries::new(2);
        swings.swings.push_back(Swing {
            ts: 1,
            price: 1.10000,
            kind: SwingKind::High,
        });
        swings.swings.push_back(Swing {
            ts: 2,
            price: 1.10010,
            kind: SwingKind::High,
        });
        let ctx = DetectorCtx::new(&swings);
        let mut det = EqhEqlDetector::new("EURUSD", Timeframe::M5, EqhEqlConfig::default());
        let _ = det.on_closed(&[b(3, 1.0990, 1.0995, 1.0980, 1.0990)], &ctx);
        let evs = det.on_closed(
            &[
                b(3, 1.0990, 1.0995, 1.0980, 1.0990),
                b(4, 1.0990, 1.1010, 1.0980, 1.0995),
            ],
            &ctx,
        );
        assert!(evs.iter().any(
            |e| matches!(e, StructureEvent::Update(IctStructure::EqualHighsLows(p)) if p.swept)
        ));
        assert!(evs.iter().any(|e| matches!(e, StructureEvent::New(IctStructure::LiquiditySweep(s)) if s.pool_kind == LiquidityPoolKind::EqualHighs)));
    }
}
