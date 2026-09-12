//! Candidate engine types (M6a).
//!
//! Mirrors `docs/M6a_KICKOFF.md` §3. All structures Serialize/Deserialize;
//! TS mirror in `ui/src/types/candidate.ts`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::detector::types::{
    CandleRef, Direction, IctStructure, LiquidityRef, PdaRef, ReversalConfirmKind, SmtDivergence,
    StrengthLabel,
};
use crate::types::{Bar, Timeframe};

/// v1 only SMT candidates; enum reserved for future setup types.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SetupType {
    Smt,
}

/// Structural lifecycle of a candidate (KB §11). Overlays SMT
/// `detection_state` without modifying the SMT state machine.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SetupStatus {
    /// Spawned: SMT reached C2Confirmed with valid PDA context.
    C2Confirmed,
    /// LTF 5m same-direction CISD/MSS confirmed on a trade_symbol.
    Validated,
    /// TTL expired without validation (no LTF reversal found in C2+C3).
    /// Candidate is kept for manual review, not marked as dead.
    ReCheck,
    /// TTL expired after validation (validated candidate reached TTL).
    Expired,
    /// Reverse LTF / C2 break / source SMT invalidated.
    Invalidated,
}

/// Decision lifecycle (LLM_DECISION_ENGINE §3). M6a always `New`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    New,
    LlmPending,
    Approved,
    Rejected,
    Expired,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExpiryReason {
    Ttl,
    Reverse,
    C2Break,
    SmtInvalidated,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionMode {
    Deterministic,
    LlmSingleCall,
    MultiAgent,
}

/// A trade candidate spawned from a confirmed SMT divergence.
///
/// §13 compliance: contains only facts / evidence / status / rule_version.
/// No entry/sl/tp/position/selected_symbol fields.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandidateSetup {
    // -- Identity & version --
    pub id: String,
    pub rule_version: String,
    pub watchlist_id: String,
    pub smt_id: String,
    pub setup_type: SetupType,
    // -- Symbols & SMT direction (snapshot from the sweeper) --
    pub symbol_set: Vec<String>,
    pub sweeper_symbol: String,
    pub trade_symbols: Vec<String>,
    pub candidate_direction: Direction,
    // -- Timeframes --
    pub context_timeframe: Timeframe,
    pub comparison_timeframe: Timeframe,
    pub validation_timeframe: Timeframe,
    // -- PDA context (in_pda gate evidence) --
    pub context_pda_id: Option<String>,
    pub observation_window: (i64, i64),
    // -- Chain candles (snapshot from sweeper chain) --
    pub c1_candle: CandleRef,
    pub smt_k_candle: CandleRef,
    pub c2_candle: CandleRef,
    pub c2_case: u8,
    pub c3_candle: Option<CandleRef>,
    // -- LTF validation event references --
    pub c2_cisd_event_ids: Vec<String>,
    pub c3_cisd_event_ids: Vec<String>,
    pub validation_kind: Option<ReversalConfirmKind>,
    pub validation_symbol: Option<String>,
    pub validation_ts: Option<i64>,
    /// Direction of the actual LTF CISD/MSS on `validation_symbol`.
    /// This can be the inverse of `candidate_direction` when the trade
    /// symbol is negatively correlated with the sweeper.
    #[serde(default)]
    pub validation_direction: Option<Direction>,
    /// One first accepted reversal per executable symbol (EURUSD/GBPUSD).
    /// Legacy single-value validation fields above are retained as the
    /// earliest validation summary for storage/UI compatibility.
    #[serde(default)]
    pub validations: Vec<SymbolValidation>,
    /// Symbols whose setup was terminally broken before validation. This is
    /// per-symbol so one FX leg cannot suppress the other leg's opportunity.
    #[serde(default)]
    pub invalidated_symbols: Vec<String>,
    /// Terminal reason for each invalidated execution leg. The aggregate
    /// `expiry_reason` remains the shared candidate lifecycle reason, while
    /// this map preserves why an individual Alert Inbox row became invalid.
    #[serde(default)]
    pub symbol_invalidation_reasons: BTreeMap<String, ExpiryReason>,
    // -- Dual status --
    pub setup_status: SetupStatus,
    pub decision_status: DecisionStatus,
    pub deterministic_score: f32,
    // -- Timestamps --
    pub created_at: i64,
    pub validated_at: Option<i64>,
    pub expired_at: Option<i64>,
    pub invalidated_at: Option<i64>,
    pub expiry_reason: Option<ExpiryReason>,
    // -- Descriptive evidence (§13) --
    pub strength: Vec<StrengthLabel>,
    /// Computed TTL deadline (c3_candle.ts + N * mtf_bar_duration).
    /// None while C3 has not closed (no TTL running).
    pub expiry_at: Option<i64>,
    /// Source SMT rule_version for traceability.
    pub smt_rule_version: String,
}

impl CandidateSetup {
    /// Mark one executable leg terminal without suppressing sibling legs.
    /// The first terminal market fact wins and remains available for audit.
    pub fn invalidate_symbol(&mut self, symbol: &str, reason: ExpiryReason) {
        if !self.invalidated_symbols.iter().any(|item| item == symbol) {
            self.invalidated_symbols.push(symbol.to_owned());
        }
        self.symbol_invalidation_reasons
            .entry(symbol.to_owned())
            .or_insert(reason);
    }
}

/// One row in the decision log per candidate state change.
///
/// M6a rows: provider='deterministic', decision_mode='deterministic',
/// parent_id=None, parse_ok=true. No `alert` field (that's M7 LlmDecision).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionLogEntry {
    pub id: String,
    pub candidate_id: String,
    #[serde(default)]
    pub watchlist_id: String,
    #[serde(default)]
    pub alert_id: Option<String>,
    #[serde(default)]
    pub trade_symbol: Option<String>,
    pub parent_id: Option<String>,
    pub provider: String,
    pub model: Option<String>,
    pub decision_mode: DecisionMode,
    pub prompt_version: Option<String>,
    pub strategy_version: String,
    pub context_version: String,
    pub request_json: String,
    pub raw_response: Option<String>,
    pub parsed_decision_json: String,
    pub parse_ok: bool,
    pub error: Option<String>,
    pub created_at: i64,
}

/// Packed evidence for a candidate at a point in time (Context Packer v1).
///
/// Pure function of (candidate_id, as_of_ts); no wall-clock, no future
/// structures (PO3 at_ts paradigm).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackedEvidence {
    pub candidate_id: String,
    pub candidate: CandidateSetup,
    pub watchlist_id: String,
    pub alert_id: Option<String>,
    pub trade_symbol: Option<String>,
    pub as_of_ts: i64,
    pub smt: SmtDivergence,
    pub ltf_cisd_mss: Vec<LtfEventRef>,
    pub pda_context: Option<PdaRef>,
    pub liquidity_refs: Vec<LiquidityRef>,
    pub strength: Vec<StrengthLabel>,
    pub context_version: String,
    pub strategy_version: String,
    /// Raw market snapshot collected by the caller. The M7b Context Packer
    /// is the only component allowed to turn these facts into provider JSON;
    /// it applies the as_of/closed-bar filters before serialization.
    #[serde(default)]
    pub market_bars: Vec<Bar>,
    #[serde(default)]
    pub market_structures: Vec<IctStructure>,
}

/// Lightweight reference to a 5m CISD/MSS event for packing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LtfEventRef {
    pub id: String,
    pub symbol: String,
    pub kind: ReversalConfirmKind,
    pub direction: Direction,
    pub ts: i64,
    pub price: f64,
}

/// First accepted 5m reversal for one executable FX symbol inside a
/// Candidate's shared C2/C3 window. A Candidate stays one UI row while
/// EURUSD and GBPUSD can each contribute one independent validation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SymbolValidation {
    pub event_id: String,
    pub symbol: String,
    pub kind: ReversalConfirmKind,
    pub direction: Direction,
    pub ts: i64,
    pub price: f64,
}

/// Result of a DecisionStrategy::evaluate call.
#[derive(Clone, Debug)]
pub struct DecisionRecord {
    pub provider: String,
    pub model: Option<String>,
    pub decision_mode: DecisionMode,
    pub prompt_version: Option<String>,
    pub strategy_version: String,
    pub context_version: String,
    pub request_json: String,
    pub raw_response: Option<String>,
    pub parsed_decision_json: String,
    pub parse_ok: bool,
    pub error: Option<String>,
}
