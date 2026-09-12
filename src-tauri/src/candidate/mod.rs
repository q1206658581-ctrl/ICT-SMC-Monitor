//! Candidate Engine (M6a).
//!
//! Converts M5 SMT detection results into trade candidates with lifecycle
//! management. Mirrors the SmtEngine pattern: in-memory HashMap + SQLite
//! persistence via the caller.
//!
//! Lifecycle: C2Confirmed -> {Validated | ReCheck | Invalidated};
//! Validated -> {Expired | Invalidated}.
//! See `docs/M6a_KICKOFF.md` §2/§4.

pub mod types;

use std::collections::HashMap;

use crate::detector::types::{
    structure_id, Correlation, Direction, IctStructure, LiquidityRefStatus, ReversalConfirmKind,
    SmtDetectionState, SmtDivergence, StructureEvent, SymbolChain,
};
use crate::types::{Bar, Timeframe};
use async_trait::async_trait;
pub use types::*;

/// Compute the effective LTF direction for a symbol based on its role
/// (sweeper vs counter) and the SMT correlation. Counter symbols with
/// negative correlation (e.g. EU/GU vs DXY) have their direction flipped
/// so that a bullish CISD on EURUSD correctly validates a bearish-DXY
/// (bullish-trade) candidate, instead of invalidating it.
pub fn effective_ltf_dir(
    candidate_dir: Direction,
    symbol: &str,
    sweeper_symbol: &str,
    relationship: Correlation,
) -> Direction {
    if symbol == sweeper_symbol {
        candidate_dir
    } else {
        match (candidate_dir, relationship) {
            (Direction::Bullish, Correlation::Negative) => Direction::Bearish,
            (Direction::Bearish, Correlation::Negative) => Direction::Bullish,
            _ => candidate_dir,
        }
    }
}

fn executable_symbols(smt: &SmtDivergence) -> Vec<String> {
    // Every non-sweeper member of the strategy group is executable. This
    // preserves the established rule that both correlated trade symbols may
    // validate a setup even when only one was the original unswept SMT leg,
    // while removing the old EU/GU hard-code for M6c groups.
    smt.symbol_set
        .iter()
        .filter(|symbol| *symbol != &smt.sweeper_symbol)
        .cloned()
        .collect()
}

/// Default TTL in MTF bars after C3 closes.
const DEFAULT_EXPIRY_MTF_BARS: usize = 5;
/// Candidate rule version.
pub const CANDIDATE_RULE_VERSION: &str = "m6a.v1";

/// Internal entry pairing a candidate with its source SMT snapshot.
struct CandidateEntry {
    candidate: CandidateSetup,
    smt_snapshot: SmtDivergence,
    ltf_events: Vec<LtfEventRef>,
}

/// A state change result: the updated candidate + a decision log entry.
#[derive(Clone, Debug)]
pub struct CandidateChange {
    pub candidate: CandidateSetup,
    pub decision: DecisionLogEntry,
    /// Present only when this change accepted a symbol's first reversal.
    /// AlertEngine uses this instead of the aggregate status transition so
    /// the same Candidate can fire once for EURUSD and once for GBPUSD.
    pub validation_event: Option<SymbolValidation>,
}

#[derive(Clone)]
enum ReplayInput {
    Bar(Bar),
    Reversal(LtfEventRef),
}

/// Extract 5m CISD/MSS events from engine structures as LtfEventRef.
pub fn extract_ltf_events(structures: &[IctStructure]) -> Vec<LtfEventRef> {
    let mut events: Vec<LtfEventRef> = structures
        .iter()
        .filter_map(|s| match s {
            IctStructure::Cisd(c) => Some(LtfEventRef {
                id: c.id.clone(),
                symbol: c.symbol.clone(),
                kind: ReversalConfirmKind::Cisd,
                direction: c.direction,
                ts: c.break_ts,
                price: c.break_price,
            }),
            IctStructure::Mss(m) => Some(LtfEventRef {
                id: m.id.clone(),
                symbol: m.symbol.clone(),
                kind: ReversalConfirmKind::Mss,
                direction: m.direction,
                ts: m.break_ts,
                price: m.break_price,
            }),
            _ => None,
        })
        .collect();
    // IctEngine is backed by hash maps and SQLite orders structures by
    // persistence time, neither of which is the market-event order. The
    // first LTF event decides validate-vs-invalidate, so always process a
    // stable chronological timeline in both live and replay paths.
    events.sort_by(|a, b| (a.ts, &a.symbol, &a.id).cmp(&(b.ts, &b.symbol, &b.id)));
    events.dedup_by(|a, b| a.id == b.id);
    events
}

/// Get the sweeper chain's detection_state from an SMT divergence.
fn sweeper_detection_state(smt: &SmtDivergence) -> Option<SmtDetectionState> {
    smt.chains
        .iter()
        .find(|c| c.symbol == smt.sweeper_symbol)
        .map(|c| c.detection_state)
}

/// Get the sweeper chain from an SMT divergence.
fn sweeper_chain<'a>(smt: &'a SmtDivergence) -> Option<&'a SymbolChain> {
    smt.chains.iter().find(|c| c.symbol == smt.sweeper_symbol)
}

fn symbol_validation_window(smt: &SmtDivergence, symbol: &str) -> Option<(i64, Option<i64>, i64)> {
    let chain = smt.chains.iter().find(|chain| chain.symbol == symbol)?;
    if chain.detection_state == SmtDetectionState::Invalidated {
        return None;
    }
    let c2_ts = chain.c2_candle.as_ref()?.ts;
    let c3_ts = chain.c3_candle.as_ref().map(|candle| candle.ts);
    let dur = smt.comparison_timeframe.duration_ms();
    Some((c2_ts, c3_ts, c3_ts.unwrap_or(c2_ts).saturating_add(dur)))
}

/// Deterministic rule-based scoring strategy (M6a).
///
/// Strategy trait for evaluating a candidate decision (§5.1/§5.2).
///
/// M6a ships `DeterministicRuleStrategy` (no network). M7a makes the seam
/// asynchronous and adds `LlmSingleCallStrategy` without changing the
/// deterministic decision semantics.
#[async_trait]
pub trait DecisionStrategy: Send + Sync {
    /// Evaluate a packed evidence and return a decision record.
    async fn evaluate(&self, evidence: &PackedEvidence) -> DecisionRecord;
}

/// Build packed evidence for a candidate at a point in time (§5.2).
///
/// Pure function: no wall-clock, no side effects. `as_of_ts` filters
/// future structures (PO3 `at_ts` paradigm): only LTF events with
/// `ts <= as_of_ts` are included.
fn pack(entry: &CandidateEntry, as_of_ts: i64) -> PackedEvidence {
    PackedEvidence {
        candidate_id: entry.candidate.id.clone(),
        candidate: entry.candidate.clone(),
        watchlist_id: entry.candidate.watchlist_id.clone(),
        alert_id: None,
        trade_symbol: None,
        as_of_ts,
        smt: entry.smt_snapshot.clone(),
        ltf_cisd_mss: entry
            .ltf_events
            .iter()
            .filter(|e| e.ts <= as_of_ts)
            .cloned()
            .collect(),
        pda_context: entry.smt_snapshot.htf_pda_ref.clone(),
        liquidity_refs: entry.smt_snapshot.liquidity_refs.clone(),
        strength: entry.smt_snapshot.strength.clone(),
        context_version: CANDIDATE_RULE_VERSION.into(),
        strategy_version: CANDIDATE_RULE_VERSION.into(),
        market_bars: Vec::new(),
        market_structures: Vec::new(),
    }
}

/// Computes a score in [0, 1] from candidate facts. No network, no LLM.
/// Weights are v1 heuristics; recorded in strategy_version.
pub struct DeterministicRuleStrategy;

#[async_trait]
impl DecisionStrategy for DeterministicRuleStrategy {
    async fn evaluate(&self, evidence: &PackedEvidence) -> DecisionRecord {
        DecisionRecord {
            provider: "deterministic".into(),
            model: None,
            decision_mode: DecisionMode::Deterministic,
            prompt_version: None,
            strategy_version: evidence.strategy_version.clone(),
            context_version: evidence.context_version.clone(),
            request_json: serde_json::to_string(evidence).unwrap_or_else(|_| "{}".into()),
            raw_response: None,
            parsed_decision_json: Self::parsed_decision(&evidence.candidate),
            parse_ok: true,
            error: None,
        }
    }
}

impl DeterministicRuleStrategy {
    pub fn score(candidate: &CandidateSetup, smt: &SmtDivergence) -> f32 {
        let mut score: f32 = 0.0;
        if candidate.context_pda_id.is_some() {
            score += 0.2;
        }
        score += match candidate.c2_case {
            1 => 0.15,
            2 => 0.10,
            3 => 0.05,
            _ => 0.0,
        };
        if candidate.setup_status == SetupStatus::Validated {
            score += 0.3;
        }
        for sl in &candidate.strength {
            if sl.label == "strong" {
                score += 0.1;
            }
        }
        let has_divergence = smt
            .liquidity_refs
            .iter()
            .any(|r| r.status != LiquidityRefStatus::Swept);
        if has_divergence {
            score += 0.05;
        }
        score.min(1.0)
    }

    /// Build the parsed_decision_json for a candidate state.
    pub fn parsed_decision(candidate: &CandidateSetup) -> String {
        serde_json::json!({
            "validated": candidate.setup_status == SetupStatus::Validated,
            "deterministic_score": candidate.deterministic_score,
            "setup_status": format!("{:?}", candidate.setup_status).to_lowercase(),
            "validation_kind": candidate.validation_kind.map(|k| format!("{:?}", k).to_lowercase()),
            "validations": candidate.validations,
            "invalidated_symbols": candidate.invalidated_symbols,
            "symbol_invalidation_reasons": candidate.symbol_invalidation_reasons,
            "c2_cisd_event_ids": candidate.c2_cisd_event_ids,
            "c3_cisd_event_ids": candidate.c3_cisd_event_ids,
        })
        .to_string()
    }
}

/// Build a decision log entry for a candidate state change (free function
/// to avoid borrow conflicts with `self.entries`).
///
/// Uses `pack` -> `DeterministicRuleStrategy::evaluate` so `request_json`
/// contains the full `PackedEvidence` (not a stub). `now_ms` is the only
/// time source (no wall-clock).
fn make_decision(entry: &CandidateEntry, now_ms: i64) -> DecisionLogEntry {
    let strategy = DeterministicRuleStrategy;
    let evidence = pack(entry, now_ms);
    // CandidateEngine remains synchronous, while every strategy now exposes
    // the same async seam. The deterministic implementation is immediately
    // ready and has no I/O, so blocking here preserves the existing engine
    // contract without allowing callers to rebuild strategy output.
    let record = futures::executor::block_on(strategy.evaluate(&evidence));

    DecisionLogEntry {
        id: structure_id(&[
            "decision",
            &entry.candidate.id,
            &now_ms.to_string(),
            "deterministic",
        ]),
        candidate_id: entry.candidate.id.clone(),
        watchlist_id: entry.candidate.watchlist_id.clone(),
        alert_id: None,
        trade_symbol: None,
        parent_id: None,
        provider: record.provider,
        model: record.model,
        decision_mode: record.decision_mode,
        prompt_version: record.prompt_version,
        strategy_version: record.strategy_version,
        context_version: record.context_version,
        request_json: record.request_json,
        raw_response: record.raw_response,
        parsed_decision_json: record.parsed_decision_json,
        parse_ok: record.parse_ok,
        error: record.error,
        created_at: now_ms,
    }
}

/// Stable candidate ID: blake3(smt_id|trade_symbols|c2_ts).
fn candidate_id(smt_id: &str, trade_symbols: &[String], c2_ts: i64) -> String {
    let symbols = trade_symbols.join(",");
    structure_id(&[smt_id, &symbols, &c2_ts.to_string()])
}

/// Candidate engine: manages in-memory candidates, processes SMT + LTF events.
pub struct CandidateEngine {
    entries: HashMap<String, CandidateEntry>,
    watchlist_id: Option<String>,
    expiry_mtf_bars: usize,
}

impl Default for CandidateEngine {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            watchlist_id: None,
            expiry_mtf_bars: DEFAULT_EXPIRY_MTF_BARS,
        }
    }
}

impl CandidateEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_watchlist(&mut self, wl_id: String) {
        tracing::info!(
            wl_id = wl_id.as_str(),
            candidates = self.entries.len(),
            "CandidateEngine set_watchlist: clearing"
        );
        self.watchlist_id = Some(wl_id);
        self.entries.clear();
    }

    pub fn set_expiry_mtf_bars(&mut self, n: usize) {
        self.expiry_mtf_bars = n.max(1);
    }

    pub fn list_active(&self) -> Vec<CandidateSetup> {
        self.entries
            .values()
            .filter(|e| {
                e.candidate.setup_status == SetupStatus::C2Confirmed
                    || e.candidate.setup_status == SetupStatus::Validated
            })
            .map(|e| e.candidate.clone())
            .collect()
    }

    pub fn list_all(&self) -> Vec<CandidateSetup> {
        self.entries.values().map(|e| e.candidate.clone()).collect()
    }

    /// Return the current candidate for a source SMT attempt. This is used by
    /// the C2 alert reconciler because a later per-symbol C2 can arrive on an
    /// SMT update without causing a second aggregate Candidate transition.
    pub fn get_by_smt_id(&self, smt_id: &str) -> Option<CandidateSetup> {
        self.entries
            .get(smt_id)
            .map(|entry| entry.candidate.clone())
    }

    /// Process an SMT structure event, including a terminal invalidation.
    pub fn on_smt_event(&mut self, event: &StructureEvent, now_ms: i64) -> Vec<CandidateChange> {
        let smt = match event {
            StructureEvent::New(s) | StructureEvent::Update(s) => match s {
                IctStructure::SmtDivergence(d) => d,
                _ => return vec![],
            },
            StructureEvent::Invalidated { id, kind } => {
                if kind != "smt_divergence" {
                    return vec![];
                }
                return self.handle_smt_invalidated_by_id(id, None, now_ms);
            }
        };

        let det_state = sweeper_detection_state(smt).unwrap_or(SmtDetectionState::Invalidated);
        match det_state {
            SmtDetectionState::C2Confirmed => self.handle_c2_confirmed(smt, now_ms),
            SmtDetectionState::C3Entry => {
                let mut changes = self.handle_c2_confirmed(smt, now_ms);
                changes.extend(self.handle_c3_entry(smt, now_ms));
                changes
            }
            SmtDetectionState::Invalidated => self.handle_smt_invalidated(smt, now_ms),
            SmtDetectionState::SmtKDetected => vec![],
        }
    }

    /// Apply a terminal SMT snapshot at its deterministic market timestamp.
    /// This keeps Candidate audit IDs stable across live delivery and replay.
    pub fn on_smt_invalidated_at(
        &mut self,
        smt: &SmtDivergence,
        market_ts: i64,
    ) -> Vec<CandidateChange> {
        self.handle_smt_invalidated_by_id(&smt.id, Some(smt), market_ts)
    }

    fn handle_c2_confirmed(&mut self, smt: &SmtDivergence, _now_ms: i64) -> Vec<CandidateChange> {
        if smt.htf_pda_ref.is_none() {
            return vec![];
        }
        if let Some(entry) = self.entries.get_mut(&smt.id) {
            entry.smt_snapshot = smt.clone();
            return vec![];
        }
        let Some(chain) = sweeper_chain(smt) else {
            return vec![];
        };
        let Some(c2_ref) = &chain.c2_candle else {
            return vec![];
        };

        let id = candidate_id(&smt.id, &smt.trade_symbols, c2_ref.ts);
        let execution_symbols = executable_symbols(smt);
        if execution_symbols.is_empty() {
            return vec![];
        }
        let wl_id = self
            .watchlist_id
            .clone()
            .unwrap_or_else(|| smt.watchlist_id.clone());

        let mut candidate = CandidateSetup {
            id: id.clone(),
            rule_version: CANDIDATE_RULE_VERSION.into(),
            watchlist_id: wl_id,
            smt_id: smt.id.clone(),
            setup_type: SetupType::Smt,
            symbol_set: smt.symbol_set.clone(),
            sweeper_symbol: smt.sweeper_symbol.clone(),
            // SMT's divergence legs are an analytical fact, but Candidate
            // execution watches both configured FX instruments. DXY remains
            // context-only and can never validate or fire an alert.
            trade_symbols: execution_symbols,
            candidate_direction: smt.candidate_direction,
            context_timeframe: smt.context_timeframe,
            comparison_timeframe: smt.comparison_timeframe,
            validation_timeframe: Timeframe::M5,
            context_pda_id: smt.htf_pda_ref.as_ref().map(|p| p.id.clone()),
            observation_window: smt.observation_window,
            c1_candle: chain.c1_candle.clone(),
            smt_k_candle: chain.smt_k_candle.clone(),
            c2_candle: c2_ref.clone(),
            c2_case: chain.c2_case.unwrap_or(0),
            c3_candle: None,
            c2_cisd_event_ids: vec![],
            c3_cisd_event_ids: vec![],
            validation_kind: None,
            validation_symbol: None,
            validation_ts: None,
            validation_direction: None,
            validations: vec![],
            invalidated_symbols: vec![],
            symbol_invalidation_reasons: Default::default(),
            setup_status: SetupStatus::C2Confirmed,
            decision_status: DecisionStatus::New,
            deterministic_score: 0.0,
            created_at: c2_ref.ts,
            validated_at: None,
            expired_at: None,
            invalidated_at: None,
            expiry_reason: None,
            strength: smt.strength.clone(),
            expiry_at: None,
            smt_rule_version: smt.rule_version.clone(),
        };
        candidate.deterministic_score = DeterministicRuleStrategy::score(&candidate, smt);
        let entry = CandidateEntry {
            candidate: candidate.clone(),
            smt_snapshot: smt.clone(),
            ltf_events: Vec::new(),
        };
        // Use market transition time so live detection and cold-start replay
        // generate the same decision ID.
        let decision = make_decision(&entry, c2_ref.ts);
        self.entries.insert(smt.id.clone(), entry);
        vec![CandidateChange {
            candidate,
            decision,
            validation_event: None,
        }]
    }

    fn handle_c3_entry(&mut self, smt: &SmtDivergence, _now_ms: i64) -> Vec<CandidateChange> {
        let Some(entry) = self.entries.get_mut(&smt.id) else {
            return vec![];
        };
        let Some(chain) = sweeper_chain(smt) else {
            return vec![];
        };
        let Some(c3_ref) = &chain.c3_candle else {
            return vec![];
        };

        // SmtEngine can emit repeated Update events while later bars refresh
        // the same divergence. Re-applying the identical C3 transition would
        // create duplicate decisions on every refresh/restart.
        if entry.candidate.c3_candle.as_ref().map(|c| c.ts) == Some(c3_ref.ts) {
            entry.smt_snapshot = smt.clone();
            return vec![];
        }

        entry.candidate.c3_candle = Some(c3_ref.clone());
        let mtf_dur = entry.candidate.comparison_timeframe.duration_ms();
        let expiry_at = c3_ref.ts + (self.expiry_mtf_bars as i64) * mtf_dur;
        entry.candidate.expiry_at = Some(expiry_at);
        entry.smt_snapshot = smt.clone();
        entry.candidate.deterministic_score =
            DeterministicRuleStrategy::score(&entry.candidate, &entry.smt_snapshot);

        let decision = make_decision(entry, c3_ref.ts);
        vec![CandidateChange {
            candidate: entry.candidate.clone(),
            decision,
            validation_event: None,
        }]
    }

    fn handle_smt_invalidated(&mut self, smt: &SmtDivergence, now_ms: i64) -> Vec<CandidateChange> {
        self.handle_smt_invalidated_by_id(&smt.id, Some(smt), now_ms)
    }

    fn handle_smt_invalidated_by_id(
        &mut self,
        smt_id: &str,
        latest_smt: Option<&SmtDivergence>,
        now_ms: i64,
    ) -> Vec<CandidateChange> {
        let Some(entry) = self.entries.get_mut(smt_id) else {
            return vec![];
        };
        if entry.candidate.setup_status == SetupStatus::Invalidated
            || entry.candidate.setup_status == SetupStatus::Expired
        {
            return vec![];
        }
        entry.candidate.setup_status = SetupStatus::Invalidated;
        entry.candidate.expiry_reason = Some(ExpiryReason::SmtInvalidated);
        entry.candidate.invalidated_at = Some(now_ms);
        for symbol in entry.candidate.trade_symbols.clone() {
            entry
                .candidate
                .invalidate_symbol(&symbol, ExpiryReason::SmtInvalidated);
        }
        if let Some(smt) = latest_smt {
            entry.smt_snapshot = smt.clone();
        }
        entry.candidate.deterministic_score =
            DeterministicRuleStrategy::score(&entry.candidate, &entry.smt_snapshot);

        let decision = make_decision(entry, now_ms);
        vec![CandidateChange {
            candidate: entry.candidate.clone(),
            decision,
            validation_event: None,
        }]
    }

    /// Process a 5m bar close for a trade_symbol.
    pub fn on_ltf_bar_close(
        &mut self,
        symbol: &str,
        bar: &Bar,
        ltf_events: &[LtfEventRef],
        now_ms: i64,
    ) -> Vec<CandidateChange> {
        let mut changes = vec![];

        // 1. Validation / reverse / C2-break for candidates involving this symbol.
        let candidate_ids: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| {
                e.candidate.trade_symbols.contains(&symbol.to_string())
                    && (e.candidate.setup_status == SetupStatus::C2Confirmed
                        || e.candidate.setup_status == SetupStatus::Validated)
            })
            .map(|(id, _)| id.clone())
            .collect();

        for cid in candidate_ids {
            let mut local_changes = vec![];
            {
                let entry = match self.entries.get_mut(&cid) {
                    Some(e) => e,
                    None => continue,
                };
                let Some((c2_ts, c3_ts, validation_end)) =
                    symbol_validation_window(&entry.smt_snapshot, symbol)
                else {
                    continue;
                };
                let dir = effective_ltf_dir(
                    entry.candidate.candidate_direction,
                    symbol,
                    &entry.candidate.sweeper_symbol,
                    entry.smt_snapshot.relationship,
                );

                // Validation window: only C2 and C3 MTF candle periods.
                // C2 window = [c2_ts, c3_ts), C3 window = [c3_ts, c3_ts + mtf_dur).
                // Events outside this range are ignored (neither validate
                // nor invalidate). TTL expiry still uses expiry_at.
                let symbol_already_resolved = entry
                    .candidate
                    .validations
                    .iter()
                    .any(|validation| validation.symbol == symbol)
                    || entry
                        .candidate
                        .invalidated_symbols
                        .iter()
                        .any(|invalidated| invalidated == symbol);

                // A close beyond this FX symbol's C2 extreme closes only
                // that execution leg. The shared Candidate stays available
                // for the other FX symbol.
                // Evaluate it before CISD/MSS events from the same 5m close,
                // otherwise this call can emit a transient Validated change
                // (and a user alert) immediately followed by Invalidated.
                // A candidate can be created after an earlier 5m reversal in
                // its C2 window has already closed. Honour that earlier market
                // fact before evaluating the current bar's C2 break. For an
                // event on this same bar, the C2 break still wins.
                let has_earlier_unprocessed_event = ltf_events.iter().any(|event| {
                    event.ts >= c2_ts
                        && event.ts < bar.ts
                        && event.ts < validation_end
                        && !entry.candidate.c2_cisd_event_ids.contains(&event.id)
                        && !entry.candidate.c3_cisd_event_ids.contains(&event.id)
                });
                let c2_break_applies = !symbol_already_resolved && !has_earlier_unprocessed_event;
                if c2_break_applies {
                    let trade_c2 = entry
                        .smt_snapshot
                        .chains
                        .iter()
                        .find(|c| c.symbol == symbol)
                        .and_then(|c| c.c2_candle.as_ref());
                    let c2_low = trade_c2
                        .map(|c| c.low)
                        .unwrap_or(entry.candidate.c2_candle.low);
                    let c2_high = trade_c2
                        .map(|c| c.high)
                        .unwrap_or(entry.candidate.c2_candle.high);
                    let broke = match dir {
                        Direction::Bullish => bar.close < c2_low,
                        Direction::Bearish => bar.close > c2_high,
                    };
                    if broke {
                        entry
                            .candidate
                            .invalidate_symbol(symbol, ExpiryReason::C2Break);
                        let all_resolved_without_validation =
                            entry.candidate.validations.is_empty()
                                && entry
                                    .candidate
                                    .trade_symbols
                                    .iter()
                                    .all(|candidate_symbol| {
                                        entry
                                            .candidate
                                            .invalidated_symbols
                                            .contains(candidate_symbol)
                                    });
                        if all_resolved_without_validation {
                            entry.candidate.setup_status = SetupStatus::Invalidated;
                            entry.candidate.expiry_reason = Some(ExpiryReason::C2Break);
                            entry.candidate.invalidated_at = Some(bar.ts);
                        }
                        entry.candidate.deterministic_score =
                            DeterministicRuleStrategy::score(&entry.candidate, &entry.smt_snapshot);
                        local_changes.push((
                            entry.candidate.clone(),
                            make_decision(entry, bar.ts),
                            None,
                        ));
                    }
                }

                // The interval is inclusive at C2 and bounded by the bar
                // currently being processed. The latter prevents a hydrated
                // future structure from being consumed before market time.
                for ev in ltf_events
                    .iter()
                    .filter(|e| e.ts >= c2_ts && e.ts <= bar.ts)
                {
                    if entry.candidate.setup_status == SetupStatus::Invalidated
                        || entry
                            .candidate
                            .validations
                            .iter()
                            .any(|v| v.symbol == symbol)
                        || entry
                            .candidate
                            .invalidated_symbols
                            .iter()
                            .any(|s| s == symbol)
                    {
                        break;
                    }
                    if ev.ts >= validation_end {
                        continue;
                    }
                    let same_dir = ev.direction == dir;

                    if same_dir {
                        // Dedup: on_ltf_bar_close receives the full
                        // list_active(symbol, M5) every bar. M5 structures
                        // persist until explicitly invalidated, so the
                        // same CISD/MSS event is re-passed every bar.
                        // Skip events already collected to prevent
                        // duplicate accumulation + decision_log spam.
                        let already = entry.candidate.c2_cisd_event_ids.contains(&ev.id)
                            || entry.candidate.c3_cisd_event_ids.contains(&ev.id);
                        if already {
                            continue;
                        }
                        entry.ltf_events.push(ev.clone());
                        let validation = SymbolValidation {
                            event_id: ev.id.clone(),
                            symbol: symbol.to_string(),
                            kind: ev.kind,
                            direction: ev.direction,
                            ts: ev.ts,
                            price: ev.price,
                        };
                        entry.candidate.validations.push(validation.clone());
                        entry.candidate.validations.sort_by(|a, b| {
                            (a.ts, &a.symbol, &a.event_id).cmp(&(b.ts, &b.symbol, &b.event_id))
                        });
                        entry.candidate.setup_status = SetupStatus::Validated;
                        if entry.candidate.validation_ts.is_none_or(|ts| ev.ts < ts) {
                            entry.candidate.validated_at = Some(ev.ts);
                            entry.candidate.validation_kind = Some(ev.kind);
                            entry.candidate.validation_symbol = Some(symbol.to_string());
                            entry.candidate.validation_ts = Some(ev.ts);
                            entry.candidate.validation_direction = Some(ev.direction);
                        }
                        if let Some(c3) = c3_ts {
                            if ev.ts < c3 {
                                entry.candidate.c2_cisd_event_ids.push(ev.id.clone());
                            } else {
                                entry.candidate.c3_cisd_event_ids.push(ev.id.clone());
                            }
                        } else {
                            entry.candidate.c2_cisd_event_ids.push(ev.id.clone());
                        }
                        entry.candidate.deterministic_score =
                            DeterministicRuleStrategy::score(&entry.candidate, &entry.smt_snapshot);
                        local_changes.push((
                            entry.candidate.clone(),
                            make_decision(entry, ev.ts),
                            Some(validation),
                        ));
                    } else if !same_dir {
                        // The first opposite reversal closes only this symbol's
                        // leg. It must not hide the other symbol's opportunity.
                        entry
                            .candidate
                            .invalidate_symbol(symbol, ExpiryReason::Reverse);
                        let all_resolved_without_validation =
                            entry.candidate.validations.is_empty()
                                && entry
                                    .candidate
                                    .trade_symbols
                                    .iter()
                                    .all(|candidate_symbol| {
                                        entry
                                            .candidate
                                            .invalidated_symbols
                                            .contains(candidate_symbol)
                                    });
                        if all_resolved_without_validation {
                            entry.candidate.setup_status = SetupStatus::Invalidated;
                            entry.candidate.expiry_reason = Some(ExpiryReason::Reverse);
                            entry.candidate.invalidated_at = Some(ev.ts);
                        }
                        entry.candidate.deterministic_score =
                            DeterministicRuleStrategy::score(&entry.candidate, &entry.smt_snapshot);
                        local_changes.push((
                            entry.candidate.clone(),
                            make_decision(entry, ev.ts),
                            None,
                        ));
                        break;
                    }
                }
            }
            for (cand, dec, validation_event) in local_changes {
                changes.push(CandidateChange {
                    candidate: cand,
                    decision: dec,
                    validation_event,
                });
            }
        }

        // 2. TTL expiry check for all active candidates.
        let ttl_ids: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| {
                (e.candidate.setup_status == SetupStatus::C2Confirmed
                    || e.candidate.setup_status == SetupStatus::Validated)
                    && e.candidate
                        .expiry_at
                        .map(|exp| now_ms >= exp)
                        .unwrap_or(false)
            })
            .map(|(id, _)| id.clone())
            .collect();

        for cid in ttl_ids {
            if let Some(entry) = self.entries.get_mut(&cid) {
                // If never validated (no LTF reversal found in C2+C3),
                // mark as ReCheck for manual review instead of Expired.
                // If validated but TTL reached, mark as Expired.
                let was_validated = entry.candidate.setup_status == SetupStatus::Validated;
                entry.candidate.setup_status = if was_validated {
                    SetupStatus::Expired
                } else {
                    SetupStatus::ReCheck
                };
                entry.candidate.expiry_reason = Some(ExpiryReason::Ttl);
                let expiry_ts = entry.candidate.expiry_at.unwrap_or(now_ms);
                entry.candidate.expired_at = Some(expiry_ts);
                entry.candidate.deterministic_score =
                    DeterministicRuleStrategy::score(&entry.candidate, &entry.smt_snapshot);
                let decision = make_decision(entry, expiry_ts);
                changes.push(CandidateChange {
                    candidate: entry.candidate.clone(),
                    decision,
                    validation_event: None,
                });
            }
        }

        changes
    }

    /// Cold-start replay: rebuild active candidates from stored SMT + LTF events.
    pub fn replay(
        &mut self,
        smt_list: Vec<SmtDivergence>,
        ltf_events_by_symbol: &HashMap<String, Vec<LtfEventRef>>,
        ltf_bars_by_symbol: &HashMap<String, Vec<Bar>>,
        now_ms: i64,
    ) -> Vec<CandidateChange> {
        let mut changes = Vec::new();
        for smt in smt_list {
            let det_state = sweeper_detection_state(&smt);
            if det_state == Some(SmtDetectionState::Invalidated) {
                continue;
            }
            if smt.htf_pda_ref.is_none() {
                continue;
            }
            // Spawn candidate. For C3Entry SMTs, on_smt_event dispatches
            // to handle_c3_entry (which only fills C3 for an existing
            // candidate, does not spawn), so we call handle_c2_confirmed
            // directly to create the candidate, then handle_c3_entry to
            // fill the C3 candle.
            match det_state {
                Some(SmtDetectionState::C2Confirmed) => {
                    let transition_ts = sweeper_chain(&smt)
                        .and_then(|chain| chain.c2_candle.as_ref())
                        .map(|candle| candle.ts)
                        .unwrap_or(now_ms);
                    changes.extend(self.on_smt_event(
                        &StructureEvent::New(IctStructure::SmtDivergence(smt.clone())),
                        transition_ts,
                    ));
                }
                Some(SmtDetectionState::C3Entry) => {
                    let c2_ts = sweeper_chain(&smt)
                        .and_then(|chain| chain.c2_candle.as_ref())
                        .map(|candle| candle.ts)
                        .unwrap_or(now_ms);
                    let c3_ts = sweeper_chain(&smt)
                        .and_then(|chain| chain.c3_candle.as_ref())
                        .map(|candle| candle.ts)
                        .unwrap_or(now_ms);
                    changes.extend(self.handle_c2_confirmed(&smt, c2_ts));
                    changes.extend(self.handle_c3_entry(&smt, c3_ts));
                }
                _ => {} // SmtKDetected: C2 not confirmed yet, no spawn
            }

            // Replay both executable FX symbols on one chronological timeline.
            // Iterating HashMap buckets symbol-by-symbol can make a later GU
            // reversal run before an earlier EU validation (or vice versa),
            // producing a state that live processing could never reach.
            let mut timeline: Vec<(i64, u8, String, ReplayInput)> = executable_symbols(&smt)
                .iter()
                .flat_map(|sym| {
                    let Some((c2_ts, _, validation_end_r)) = symbol_validation_window(&smt, sym)
                    else {
                        return Vec::new().into_iter();
                    };
                    let reversal_inputs = ltf_events_by_symbol
                        .get(sym)
                        .into_iter()
                        .flatten()
                        .filter(move |e| e.ts >= c2_ts && e.ts < validation_end_r)
                        .cloned()
                        .map(|e| (e.ts, 1, sym.clone(), ReplayInput::Reversal(e)));
                    let bar_inputs = ltf_bars_by_symbol
                        .get(sym)
                        .into_iter()
                        .flatten()
                        .filter(move |bar| bar.ts >= c2_ts && bar.ts < validation_end_r)
                        .cloned()
                        // Same-bar C2 close invalidation has precedence over a
                        // reversal confirmation, matching the live path.
                        .map(|bar| (bar.ts, 0, sym.clone(), ReplayInput::Bar(bar)));
                    bar_inputs
                        .chain(reversal_inputs)
                        .collect::<Vec<_>>()
                        .into_iter()
                })
                .collect();
            timeline.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));

            for (_, _, sym, input) in timeline {
                let dir = effective_ltf_dir(
                    smt.candidate_direction,
                    &sym,
                    &smt.sweeper_symbol,
                    smt.relationship,
                );
                let Some(entry) = self.entries.get_mut(&smt.id) else {
                    continue;
                };
                if matches!(
                    entry.candidate.setup_status,
                    SetupStatus::Invalidated | SetupStatus::Expired | SetupStatus::ReCheck
                ) {
                    break;
                }
                if entry.candidate.validations.iter().any(|v| v.symbol == sym)
                    || entry
                        .candidate
                        .invalidated_symbols
                        .iter()
                        .any(|s| s == &sym)
                {
                    continue;
                }
                if let ReplayInput::Bar(bar) = input {
                    let trade_c2 = entry
                        .smt_snapshot
                        .chains
                        .iter()
                        .find(|chain| chain.symbol == sym)
                        .and_then(|chain| chain.c2_candle.as_ref());
                    let c2_low = trade_c2
                        .map(|candle| candle.low)
                        .unwrap_or(entry.candidate.c2_candle.low);
                    let c2_high = trade_c2
                        .map(|candle| candle.high)
                        .unwrap_or(entry.candidate.c2_candle.high);
                    let broke = match dir {
                        Direction::Bullish => bar.close < c2_low,
                        Direction::Bearish => bar.close > c2_high,
                    };
                    if broke {
                        entry
                            .candidate
                            .invalidate_symbol(&sym, ExpiryReason::C2Break);
                        let all_resolved_without_validation =
                            entry.candidate.validations.is_empty()
                                && entry
                                    .candidate
                                    .trade_symbols
                                    .iter()
                                    .all(|candidate_symbol| {
                                        entry
                                            .candidate
                                            .invalidated_symbols
                                            .contains(candidate_symbol)
                                    });
                        if all_resolved_without_validation {
                            entry.candidate.setup_status = SetupStatus::Invalidated;
                            entry.candidate.expiry_reason = Some(ExpiryReason::C2Break);
                            entry.candidate.invalidated_at = Some(bar.ts);
                        }
                        entry.candidate.deterministic_score =
                            DeterministicRuleStrategy::score(&entry.candidate, &entry.smt_snapshot);
                        changes.push(CandidateChange {
                            candidate: entry.candidate.clone(),
                            decision: make_decision(entry, bar.ts),
                            validation_event: None,
                        });
                    }
                    continue;
                }
                let ReplayInput::Reversal(ev) = input else {
                    continue;
                };
                if ev.direction == dir {
                    let already = entry.candidate.c2_cisd_event_ids.contains(&ev.id)
                        || entry.candidate.c3_cisd_event_ids.contains(&ev.id);
                    if already {
                        continue;
                    }
                    entry.ltf_events.push(ev.clone());
                    let validation = SymbolValidation {
                        event_id: ev.id.clone(),
                        symbol: sym.clone(),
                        kind: ev.kind,
                        direction: ev.direction,
                        ts: ev.ts,
                        price: ev.price,
                    };
                    entry.candidate.validations.push(validation.clone());
                    entry.candidate.validations.sort_by(|a, b| {
                        (a.ts, &a.symbol, &a.event_id).cmp(&(b.ts, &b.symbol, &b.event_id))
                    });
                    entry.candidate.setup_status = SetupStatus::Validated;
                    if entry.candidate.validation_ts.is_none_or(|ts| ev.ts < ts) {
                        entry.candidate.validated_at = Some(ev.ts);
                        entry.candidate.validation_kind = Some(ev.kind);
                        entry.candidate.validation_symbol = Some(sym.clone());
                        entry.candidate.validation_ts = Some(ev.ts);
                        entry.candidate.validation_direction = Some(ev.direction);
                    }
                    let c3_ts = entry
                        .smt_snapshot
                        .chains
                        .iter()
                        .find(|chain| chain.symbol == sym)
                        .and_then(|chain| chain.c3_candle.as_ref())
                        .map(|candle| candle.ts);
                    if let Some(c3) = c3_ts {
                        if ev.ts < c3 {
                            entry.candidate.c2_cisd_event_ids.push(ev.id.clone());
                        } else {
                            entry.candidate.c3_cisd_event_ids.push(ev.id.clone());
                        }
                    } else {
                        entry.candidate.c2_cisd_event_ids.push(ev.id.clone());
                    }
                    entry.candidate.deterministic_score =
                        DeterministicRuleStrategy::score(&entry.candidate, &entry.smt_snapshot);
                    changes.push(CandidateChange {
                        candidate: entry.candidate.clone(),
                        decision: make_decision(entry, ev.ts),
                        validation_event: Some(validation),
                    });
                } else {
                    entry
                        .candidate
                        .invalidate_symbol(&sym, ExpiryReason::Reverse);
                    let all_resolved_without_validation = entry.candidate.validations.is_empty()
                        && entry
                            .candidate
                            .trade_symbols
                            .iter()
                            .all(|candidate_symbol| {
                                entry
                                    .candidate
                                    .invalidated_symbols
                                    .contains(candidate_symbol)
                            });
                    if all_resolved_without_validation {
                        entry.candidate.setup_status = SetupStatus::Invalidated;
                        entry.candidate.expiry_reason = Some(ExpiryReason::Reverse);
                        entry.candidate.invalidated_at = Some(ev.ts);
                    }
                    entry.candidate.deterministic_score =
                        DeterministicRuleStrategy::score(&entry.candidate, &entry.smt_snapshot);
                    changes.push(CandidateChange {
                        candidate: entry.candidate.clone(),
                        decision: make_decision(entry, ev.ts),
                        validation_event: None,
                    });
                }
            }
        }
        // TTL pass: replay rebuilds candidates as Validated/C2Confirmed,
        // but the TTL expiry check in on_ltf_bar_close only fires when a
        // new 5m bar closes. On weekends / market closed, no new bars
        // arrive so candidates whose expiry_at has passed would stay
        // Validated indefinitely. Run the same TTL logic here so
        // cold-start replay immediately expires stale candidates.
        let ttl_ids: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| {
                (e.candidate.setup_status == SetupStatus::C2Confirmed
                    || e.candidate.setup_status == SetupStatus::Validated)
                    && e.candidate
                        .expiry_at
                        .map(|exp| now_ms >= exp)
                        .unwrap_or(false)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for cid in ttl_ids {
            if let Some(entry) = self.entries.get_mut(&cid) {
                let was_validated = entry.candidate.setup_status == SetupStatus::Validated;
                entry.candidate.setup_status = if was_validated {
                    SetupStatus::Expired
                } else {
                    SetupStatus::ReCheck
                };
                entry.candidate.expiry_reason = Some(ExpiryReason::Ttl);
                let expiry_ts = entry.candidate.expiry_at.unwrap_or(now_ms);
                entry.candidate.expired_at = Some(expiry_ts);
                entry.candidate.deterministic_score =
                    DeterministicRuleStrategy::score(&entry.candidate, &entry.smt_snapshot);
                changes.push(CandidateChange {
                    candidate: entry.candidate.clone(),
                    decision: make_decision(entry, expiry_ts),
                    validation_event: None,
                });
            }
        }
        changes
    }
}

#[cfg(test)]
mod tests;
