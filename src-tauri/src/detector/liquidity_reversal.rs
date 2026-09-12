use super::types::{
    structure_id, Direction, IctStructure, LiquidityPoolKind, LiquidityReversal, LiquiditySide,
    LiquiditySweep, ReversalConfirmKind, StructureEvent,
};
use crate::types::Timeframe;

#[derive(Clone, Debug)]
pub struct LiquidityReversalConfig {
    pub max_bars_after_sweep: usize,
    pub allow_cisd: bool,
    pub allow_mss: bool,
    pub min_score: u8,
}

impl Default for LiquidityReversalConfig {
    fn default() -> Self {
        Self {
            max_bars_after_sweep: 10,
            allow_cisd: true,
            allow_mss: true,
            min_score: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RecentSweep {
    pub sweep: LiquiditySweep,
}

pub fn maybe_reversal(
    symbol: &str,
    tf: Timeframe,
    sweep: &LiquiditySweep,
    confirm_id: &str,
    confirm_kind: ReversalConfirmKind,
    confirm_direction: Direction,
    confirm_ts: i64,
    confirm_price: f64,
    bar_ms: i64,
    cfg: &LiquidityReversalConfig,
) -> Option<StructureEvent> {
    if confirm_kind == ReversalConfirmKind::Cisd && !cfg.allow_cisd {
        return None;
    }
    if confirm_kind == ReversalConfirmKind::Mss && !cfg.allow_mss {
        return None;
    }
    let direction = match sweep.side {
        LiquiditySide::BuySide => Direction::Bearish,
        LiquiditySide::SellSide => Direction::Bullish,
    };
    if confirm_direction != direction {
        return None;
    }
    if confirm_ts < sweep.sweep_ts {
        return None;
    }
    let max_ms = bar_ms.saturating_mul(cfg.max_bars_after_sweep as i64);
    if confirm_ts.saturating_sub(sweep.sweep_ts) > max_ms {
        return None;
    }
    let score = sweep_score(sweep.pool_kind) + confirm_score(confirm_kind);
    if score < cfg.min_score {
        return None;
    }
    let id = structure_id(&[
        symbol,
        tf.tag(),
        "liquidity_reversal",
        &sweep.id,
        confirm_id,
    ]);
    Some(StructureEvent::New(IctStructure::LiquidityReversal(
        LiquidityReversal {
            id,
            symbol: symbol.to_string(),
            tf,
            direction,
            sweep_id: sweep.id.clone(),
            sweep_pool_kind: Some(sweep.pool_kind),
            sweep_side: Some(sweep.side),
            confirm_id: confirm_id.to_string(),
            confirm_kind,
            sweep_ts: sweep.sweep_ts,
            sweep_level_ts: Some(sweep.level_ts),
            confirm_ts,
            level_price: sweep.level_price,
            confirm_price: Some(confirm_price),
            score,
        },
    )))
}

fn sweep_score(kind: LiquidityPoolKind) -> u8 {
    match kind {
        LiquidityPoolKind::SwingHigh | LiquidityPoolKind::SwingLow => 1,
        LiquidityPoolKind::EqualHighs | LiquidityPoolKind::EqualLows => 2,
        LiquidityPoolKind::Pdh | LiquidityPoolKind::Pdl => 3,
    }
}

fn confirm_score(kind: ReversalConfirmKind) -> u8 {
    match kind {
        ReversalConfirmKind::Cisd => 1,
        ReversalConfirmKind::Mss => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{LiquidityPoolKind, LiquiditySide};
    use super::*;

    fn sweep(side: LiquiditySide, kind: LiquidityPoolKind) -> LiquiditySweep {
        LiquiditySweep {
            id: "sweep1".into(),
            symbol: "EURUSD".into(),
            tf: Timeframe::M5,
            side,
            pool_kind: kind,
            sweep_ts: 0,
            sweep_price: 1.2,
            level_ts: -1,
            level_price: 1.1,
            close_price: 1.09,
        }
    }

    #[test]
    fn buy_side_sweep_plus_bearish_cisd_reverses_bearish() {
        let cfg = LiquidityReversalConfig::default();
        let ev = maybe_reversal(
            "EURUSD",
            Timeframe::M5,
            &sweep(LiquiditySide::BuySide, LiquidityPoolKind::SwingHigh),
            "c1",
            ReversalConfirmKind::Cisd,
            Direction::Bearish,
            5 * 60_000,
            1.1,
            5 * 60_000,
            &cfg,
        );
        assert!(
            matches!(ev, Some(StructureEvent::New(IctStructure::LiquidityReversal(r))) if r.direction == Direction::Bearish)
        );
    }

    #[test]
    fn direction_mismatch_does_not_reverse() {
        let cfg = LiquidityReversalConfig::default();
        let ev = maybe_reversal(
            "EURUSD",
            Timeframe::M5,
            &sweep(LiquiditySide::BuySide, LiquidityPoolKind::SwingHigh),
            "c1",
            ReversalConfirmKind::Cisd,
            Direction::Bullish,
            5 * 60_000,
            1.1,
            5 * 60_000,
            &cfg,
        );
        assert!(ev.is_none());
    }

    #[test]
    fn too_late_confirm_does_not_reverse() {
        let cfg = LiquidityReversalConfig {
            max_bars_after_sweep: 2,
            ..Default::default()
        };
        let ev = maybe_reversal(
            "EURUSD",
            Timeframe::M5,
            &sweep(LiquiditySide::SellSide, LiquidityPoolKind::Pdl),
            "m1",
            ReversalConfirmKind::Mss,
            Direction::Bullish,
            15 * 60_000,
            1.1,
            5 * 60_000,
            &cfg,
        );
        assert!(ev.is_none());
    }
}
