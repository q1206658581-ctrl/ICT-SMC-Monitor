//! ICT detector engine.

pub mod atr;
pub mod bootstrap;
pub mod bos;
pub mod breaker_block;
pub mod cisd;
pub mod engine;
pub mod eqh_eql;
pub mod fvg;
pub mod gap_state;
pub mod killzone;
pub mod liquidity_reversal;
pub mod liquidity_sweep;
pub mod mss;
pub mod opening_gap;
pub mod order_block;
pub mod ote;
pub mod pdh_pdl;
pub mod po3;
pub mod premium_discount;
pub mod session;
pub mod smt;
pub mod swing;
pub mod types;
pub mod volume_imbalance;

pub use engine::{EngineHandle, IctEngine};
pub use types::{
    Bos, BreakerBlock, CandleRef, Cisd, Correlation, Direction, EqualHighsLows, Fvg, FvgState,
    GapDirection, GapState, GapZone, IctStructure, KillZoneKind, KillZoneSpan, KillZoneWindow,
    LevelMarker, LiquidityPoolKind, LiquidityRef, LiquidityRefStatus, LiquidityReversal,
    LiquiditySide, LiquiditySweep, Mss, ObState, OpeningGapKind, OrderBlock, OteZone, PdSide,
    PdaRef, Po3ContextKind, Po3Stage, Po3StageBox, Po3State, PowerOf3, PremiumDiscount,
    ReferenceScope, ReversalConfirmKind, SessionKind, SessionRange, SmtDetectionState,
    SmtDivergence, StrengthLabel, StructureEvent, SymbolChain, VolumeImbalance, ZoneState,
};

use crate::types::Bar;
use swing::SwingSeries;

/// Context passed to detectors that depend on shared per-(symbol, tf) state.
pub struct DetectorCtx<'a> {
    pub swings: &'a SwingSeries,
    pub structures: &'a [types::IctStructure],
}

impl<'a> DetectorCtx<'a> {
    pub fn new(swings: &'a SwingSeries) -> Self {
        Self {
            swings,
            structures: &[],
        }
    }
}

/// Trait every detector implements. **No I/O, no clocks** — input is bars only.
pub trait Detector: Send {
    fn name(&self) -> &'static str;

    fn on_closed(&mut self, bars: &[Bar], ctx: &DetectorCtx<'_>) -> Vec<StructureEvent>;

    fn on_open(
        &mut self,
        _current: &Bar,
        _history: &[Bar],
        _ctx: &DetectorCtx<'_>,
    ) -> Vec<StructureEvent> {
        Vec::new()
    }

    /// Update one config parameter at runtime. Implementations parse `value`
    /// as the type they expect (number/bool) and return `true` when the
    /// param was applied. Default `false` means "unknown key".
    ///
    /// Side note: changing a param does NOT re-emit existing structures —
    /// the engine layer handles "drop active + replay history" so old
    /// structures aren't left orphaned.
    fn apply_param(&mut self, _key: &str, _value: &serde_json::Value) -> bool {
        false
    }

    /// Detector-driven post-step emit. Called by the engine after every
    /// `on_closed` (and after replay) so detectors that accumulate across
    /// many bars (e.g. PDH/PDL: tracks the running day's high/low and only
    /// fires on rollover) can also surface the **currently accumulated**
    /// state, which is what users expect to see on the chart between
    /// rollovers. Default = no-op.
    fn flush(&mut self, _history: &[Bar]) -> Vec<StructureEvent> {
        Vec::new()
    }

    /// Drop all internal state so the engine can replay the history slice
    /// from scratch (used after `apply_param`). Default = no-op (detector
    /// has no mutable state worth resetting).
    fn reset(&mut self) {}
}
