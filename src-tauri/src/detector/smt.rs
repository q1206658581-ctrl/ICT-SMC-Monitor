//! PDA-aware, DXY-owned SMT divergence engine.
//!
//! A formal SMT is emitted only after a closed DXY HTF candle takes confirmed
//! DXY liquidity inside the same active FVG PDA while at least one inverse
//! counter does not take its paired liquidity. C1/C2/C3 are then evaluated on
//! the mapped MTF from the canonical DXY interval.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::aggregator::expected_market_bar_opens;
use crate::detector::swing::{Swing, SwingKind};
use crate::detector::types::structure_id;
use crate::detector::types::{
    CandleRef, Correlation, Direction, Fvg, FvgState, IctStructure, LiquidityRef,
    LiquidityRefStatus, LiquiditySide, PdaRef, ReferenceScope, SmtDetectionState, SmtDivergence,
    SmtInvalidationReason, SmtReferenceEvidence, StrengthLabel, StructureEvent, SymbolChain,
};
use crate::types::{Bar, Timeframe};
use crate::watchlist::Watchlist;

// Replay emits from a common recent 1100-bar MTF event horizon. The UI keeps
// a wider M30/H1 audit window because a valid row's liquidity reference can
// predate its SMT K; this detector capacity also retains that validation
// context without widening the public event horizon.
const HISTORY_CAP: usize = 1_500;
/// Current public SMT contract. Read-side projections and the one-time
/// derived-data rebuild use the same value so older HTF rule rows cannot mix
/// with current chart/table evidence.
pub const CURRENT_RULE_VERSION: &str = "m6b.v15-pda90-smt-episodes";
/// Rebuild marker for implementation-level corrections within the public
/// v15 strategy contract.
/// Storage/replay marker only. M6c keeps the v15 strategy contract unchanged
/// but rebuilds derived rows once because SMT/Candidate identities are now
/// watchlist-scoped and two additional groups must be seeded.
pub const CURRENT_PIPELINE_VERSION: &str = "m6b.v15-pda90-smt-episodes.m6c1";
const RULE_VERSION: &str = CURRENT_RULE_VERSION;

/// An HTF FVG remains structurally Active/Mitigated50, but is no longer a
/// usable SMT PDA once price has traversed 90% of its original width.  This
/// is intentionally separate from FVG `Filled`: the chart and the SMT
/// eligibility contract must not rewrite the historical FVG fact.
pub const PDA_SMT_MAX_PENETRATION: f64 = 0.90;

/// Result of applying the canonical persistence boundary shared by live and
/// offline replay.  Keeping this normalization pure prevents replay from
/// silently drifting away from the production SMT lifecycle.
pub struct CanonicalSmtEvent {
    pub event: StructureEvent,
    pub invalidation_snapshot: Option<(SmtDivergence, i64)>,
}

pub fn canonicalize_smt_event(mut event: StructureEvent, fallback_ts: i64) -> CanonicalSmtEvent {
    let mut invalidation_reasons = Vec::new();
    if let StructureEvent::New(ref mut structure) | StructureEvent::Update(ref mut structure) =
        &mut event
    {
        if let IctStructure::SmtDivergence(divergence) = structure {
            invalidation_reasons.extend(divergence.invalidation_reasons.iter().copied());
            if divergence.rule_version != CURRENT_RULE_VERSION || divergence.htf_pda_ref.is_none() {
                invalidation_reasons.push(SmtInvalidationReason::CanonicalSweepInvalid);
            } else {
                let all_counters_swept = divergence
                    .liquidity_refs
                    .iter()
                    .filter(|liquidity| liquidity.symbol != divergence.sweeper_symbol)
                    .all(|liquidity| liquidity.status == LiquidityRefStatus::Swept);
                if all_counters_swept {
                    invalidation_reasons.push(SmtInvalidationReason::AllCountersSwept);
                }
            }

            divergence.trade_symbols = divergence
                .liquidity_refs
                .iter()
                .filter(|liquidity| liquidity.symbol != divergence.sweeper_symbol)
                .filter(|liquidity| liquidity.status != LiquidityRefStatus::Swept)
                .map(|liquidity| liquidity.symbol.clone())
                .collect();
            divergence.strength = divergence
                .liquidity_refs
                .iter()
                .filter(|liquidity| liquidity.symbol != divergence.sweeper_symbol)
                .map(|liquidity| StrengthLabel {
                    symbol: liquidity.symbol.clone(),
                    label: if liquidity.status == LiquidityRefStatus::Swept {
                        "weak".into()
                    } else {
                        "strong".into()
                    },
                })
                .collect();

            let sweeper_chain_invalidated = divergence
                .chains
                .iter()
                .find(|chain| chain.symbol == divergence.sweeper_symbol)
                .is_some_and(|chain| chain.detection_state == SmtDetectionState::Invalidated);
            if sweeper_chain_invalidated
                && !invalidation_reasons.contains(&SmtInvalidationReason::PdaConsumedByOtherSmt)
                && !invalidation_reasons.contains(&SmtInvalidationReason::AllCountersSwept)
                && !invalidation_reasons.contains(&SmtInvalidationReason::HtfFormationCancelled)
            {
                let reason = divergence
                    .chains
                    .iter()
                    .find(|chain| chain.symbol == divergence.sweeper_symbol)
                    .map(|chain| {
                        if chain.c2_candle.is_some() && chain.c3_candle.is_some() {
                            SmtInvalidationReason::SweeperC3Failed
                        } else {
                            SmtInvalidationReason::SweeperC2Failed
                        }
                    })
                    .unwrap_or(SmtInvalidationReason::SweeperC2Failed);
                invalidation_reasons.push(reason);
            }
        }
    }

    invalidation_reasons.sort_by_key(|reason| match reason {
        SmtInvalidationReason::SweeperC2Failed => 0,
        SmtInvalidationReason::SweeperC3Failed => 1,
        SmtInvalidationReason::AllCountersSwept => 2,
        SmtInvalidationReason::CanonicalReferenceChanged => 3,
        SmtInvalidationReason::CanonicalSweepInvalid => 4,
        SmtInvalidationReason::ReferenceAlreadyTaken => 5,
        SmtInvalidationReason::CanonicalChainChanged => 6,
        SmtInvalidationReason::PdaConsumedByOtherSmt => 7,
        SmtInvalidationReason::HtfFormationCancelled => 8,
    });
    invalidation_reasons.dedup();

    let invalidation_snapshot = if invalidation_reasons.is_empty() {
        None
    } else if let StructureEvent::New(IctStructure::SmtDivergence(divergence))
    | StructureEvent::Update(IctStructure::SmtDivergence(divergence)) = &event
    {
        let mut snapshot = divergence.clone();
        snapshot.invalidation_reasons = invalidation_reasons;
        let market_ts = smt_invalidation_market_ts(&snapshot, fallback_ts);
        snapshot.invalidation_ts = Some(market_ts);
        for chain in &mut snapshot.chains {
            if chain.symbol == snapshot.sweeper_symbol {
                chain.detection_state = SmtDetectionState::Invalidated;
            }
        }
        SmtEngine::stamp_pda_display_end(&mut snapshot);
        event = StructureEvent::Invalidated {
            id: snapshot.id.clone(),
            kind: "smt_divergence".into(),
        };
        Some((snapshot, market_ts))
    } else {
        None
    };

    CanonicalSmtEvent {
        event,
        invalidation_snapshot,
    }
}

pub fn smt_invalidation_market_ts(divergence: &SmtDivergence, fallback_ts: i64) -> i64 {
    divergence
        .invalidation_ts
        .or_else(|| {
            divergence
                .chains
                .iter()
                .find(|chain| chain.symbol == divergence.sweeper_symbol)
                .and_then(|chain| {
                    chain
                        .c3_candle
                        .as_ref()
                        .map(|candle| candle.ts)
                        .or_else(|| {
                            (chain.detection_state == SmtDetectionState::Invalidated).then_some(
                                chain
                                    .smt_k_candle
                                    .ts
                                    .saturating_add(divergence.comparison_timeframe.duration_ms()),
                            )
                        })
                })
        })
        .or_else(|| {
            divergence
                .liquidity_refs
                .iter()
                .filter(|liquidity| liquidity.status == LiquidityRefStatus::Swept)
                .filter_map(|liquidity| liquidity.mtf_sweep_candle.as_ref().map(|candle| candle.ts))
                .max()
        })
        .unwrap_or(fallback_ts)
}

/// SMT HTF TFs (§1.6): PDA observation / trigger lives here.
const SMT_HTF_TFS: &[Timeframe] = &[Timeframe::H1, Timeframe::H4];

/// All TFs the engine accepts (MTF + HTF).
const SMT_ALL_TFS: &[Timeframe] = &[Timeframe::M30, Timeframe::H1, Timeframe::H4];

/// Map SMT MTF to PDA HTF (one level up).
pub fn htf_tf(tf: Timeframe) -> Option<Timeframe> {
    match tf {
        Timeframe::M30 => Some(Timeframe::H1),
        Timeframe::H1 => Some(Timeframe::H4),
        _ => None,
    }
}

/// Map HTF back to MTF (inverse of `htf_tf`).
fn mtf_for_htf(htf: Timeframe) -> Option<Timeframe> {
    match htf {
        Timeframe::H4 => Some(Timeframe::H1),
        Timeframe::H1 => Some(Timeframe::M30),
        _ => None,
    }
}

struct SmtBucket {
    bars: VecDeque<Bar>,
}

impl SmtBucket {
    fn new() -> Self {
        Self {
            bars: VecDeque::with_capacity(HISTORY_CAP),
        }
    }
    fn bar_at(&self, ts: i64) -> Option<&Bar> {
        self.bars.iter().find(|b| b.ts == ts)
    }
}

pub struct SmtEngine {
    watchlist: Option<Watchlist>,
    buckets: HashMap<(String, Timeframe), SmtBucket>,
    divergences: HashMap<String, SmtDivergence>,
    /// PDA IDs whose SMT attempt completed a valid DXY C3. C2 only reserves
    /// the PDA; it does not consume it.
    consumed_pdas: HashSet<String>,
    /// SMTs detected inside a PDA and still waiting for a valid DXY C3.
    /// Several attempts may reserve the same PDA; the first valid C3 consumes
    /// it and invalidates competing reservations.
    reserved_pdas: HashMap<String, HashSet<String>>,
    pending_pda_htf_closes: Vec<Bar>,
    /// Current provisional SMT IDs keyed by their still-open HTF interval.
    /// They may create C2 Candidates early, but cannot consume the PDA until
    /// the parent HTF candle formally closes.
    provisional_windows: HashMap<(Timeframe, i64), HashSet<String>>,
    /// Prevent one partial child window from being evaluated once per symbol
    /// when network delivery order differs.
    evaluated_forming_windows: HashSet<(Timeframe, i64, i64)>,
}

/// Per-counter comparison result used when building a multi-symbol SMT
/// (§2.2: DXY + every counter each get their own chain + liquidity_ref).
struct CounterInfo {
    symbol: String,
    correlation: Correlation,
    expected_kind: SwingKind,
    status: LiquidityRefStatus,
    ref_price: f64,
    ref_ts: i64,
    mtf_ref_candle: CandleRef,
    mtf_sweep_candle: CandleRef,
}

#[derive(Clone, Debug)]
struct PdaLiquidityCandidate {
    kind: SwingKind,
    price: f64,
    ts: i64,
    tf: Timeframe,
    scope: ReferenceScope,
    rank: u8,
    /// A previous attempt already took this liquidity but failed DXY C2/C3.
    /// The same episode may continue only if the new HTF sweep makes a more
    /// adverse DXY extreme.
    previous_failed_extreme: Option<f64>,
}

fn sort_pda_liquidity_candidates(refs: &mut [PdaLiquidityCandidate], kind: SwingKind) {
    refs.sort_by(|a, b| {
        // A reference released by a failed C2/C3 may be retried only as a
        // last resort. Fresh liquidity formed since then owns the current
        // episode even when its structural rank is lower. Otherwise a
        // distant high taken hours earlier keeps stealing the main white line
        // from newly formed HTF liquidity such as 08/10 11:00.
        a.previous_failed_extreme
            .is_some()
            .cmp(&b.previous_failed_extreme.is_some())
            .then_with(|| a.rank.cmp(&b.rank))
            .then_with(|| match kind {
                SwingKind::High => b
                    .price
                    .partial_cmp(&a.price)
                    .unwrap_or(std::cmp::Ordering::Equal),
                SwingKind::Low => a
                    .price
                    .partial_cmp(&b.price)
                    .unwrap_or(std::cmp::Ordering::Equal),
            })
            .then_with(|| a.ts.cmp(&b.ts))
    });
}

impl Default for SmtEngine {
    fn default() -> Self {
        Self {
            watchlist: None,
            buckets: HashMap::new(),
            divergences: HashMap::new(),
            consumed_pdas: HashSet::new(),
            reserved_pdas: HashMap::new(),
            pending_pda_htf_closes: Vec::new(),
            provisional_windows: HashMap::new(),
            evaluated_forming_windows: HashSet::new(),
        }
    }
}

impl SmtEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check whether a PDA (by structure ID) has already been consumed by
    /// a prior SMT sweep. Used by `feed_smt` to enforce one-PDA-one-SMT.
    pub fn is_pda_consumed(&self, id: &str) -> bool {
        self.consumed_pdas.contains(id)
    }

    /// Mark a PDA as consumed so it cannot be matched with future SMTs.
    pub fn consume_pda(&mut self, id: &str) {
        self.consumed_pdas.insert(id.to_string());
        self.reserved_pdas.remove(id);
    }

    pub fn reserve_pda(&mut self, pda_id: &str, smt_id: &str) {
        self.reserved_pdas
            .entry(pda_id.to_string())
            .or_default()
            .insert(smt_id.to_string());
    }

    fn release_pda_reservation(&mut self, pda_id: &str, smt_id: &str) {
        let remove_bucket = self.reserved_pdas.get_mut(pda_id).is_some_and(|owners| {
            owners.remove(smt_id);
            owners.is_empty()
        });
        if remove_bucket {
            self.reserved_pdas.remove(pda_id);
        }
    }

    fn invalidate_pda_competitors(
        &mut self,
        pda_id: &str,
        winner_id: &str,
        invalidation_ts: i64,
    ) -> Vec<StructureEvent> {
        let loser_ids: Vec<String> = self
            .divergences
            .values()
            .filter(|other| other.id != winner_id)
            .filter(|other| {
                other
                    .htf_pda_ref
                    .as_ref()
                    .is_some_and(|pda| pda.id == pda_id)
            })
            .filter(|other| {
                other
                    .chains
                    .iter()
                    .find(|chain| chain.symbol == other.sweeper_symbol)
                    .is_some_and(|chain| {
                        matches!(
                            chain.detection_state,
                            SmtDetectionState::SmtKDetected | SmtDetectionState::C2Confirmed
                        )
                    })
            })
            .map(|other| other.id.clone())
            .collect();
        let mut events = Vec::new();
        for loser_id in loser_ids {
            if let Some(mut loser) = self.divergences.get(&loser_id).cloned() {
                for chain in &mut loser.chains {
                    chain.detection_state = SmtDetectionState::Invalidated;
                }
                loser
                    .invalidation_reasons
                    .push(SmtInvalidationReason::PdaConsumedByOtherSmt);
                loser.invalidation_ts = Some(invalidation_ts);
                events.extend(self.emit_smt(loser));
            }
        }
        events
    }

    pub fn set_watchlist(&mut self, wl: Watchlist) {
        tracing::info!(
            wl_id = wl.id.as_str(),
            buckets = self.buckets.len(),
            divergences = self.divergences.len(),
            "SmtEngine set_watchlist: clearing buckets + divergences"
        );
        self.watchlist = Some(wl);
        self.buckets.clear();
        self.divergences.clear();
        self.consumed_pdas.clear();
        self.reserved_pdas.clear();
        self.pending_pda_htf_closes.clear();
        self.provisional_windows.clear();
        self.evaluated_forming_windows.clear();
    }

    pub fn watchlist_id(&self) -> Option<&str> {
        self.watchlist.as_ref().map(|w| w.id.as_str())
    }

    pub fn list_active(&self) -> Vec<SmtDivergence> {
        self.divergences
            .values()
            .filter(|d| {
                d.chains
                    .iter()
                    .find(|chain| chain.symbol == d.sweeper_symbol)
                    .is_some_and(|chain| chain.detection_state != SmtDetectionState::Invalidated)
            })
            .cloned()
            .collect()
    }

    /// Return every divergence retained by the state machine, including
    /// terminal attempts. Offline replay needs the complete audit trail;
    /// the live inbox can continue to use `list_active` for its projection.
    pub fn list_all(&self) -> Vec<SmtDivergence> {
        self.divergences.values().cloned().collect()
    }

    pub fn invalidate_all(&mut self) {
        self.divergences.clear();
        self.consumed_pdas.clear();
        self.reserved_pdas.clear();
        self.pending_pda_htf_closes.clear();
    }

    /// Retain a terminal divergence as audit/continuation evidence while
    /// removing it from the active set through its Invalidated chain state.
    /// Keeping it in memory lets a later, more adverse HTF candle reuse the
    /// original liquidity after a failed C2/C3 attempt.
    pub fn invalidate_one(&mut self, id: &str) {
        if let Some(divergence) = self.divergences.get_mut(id) {
            for chain in &mut divergence.chains {
                chain.detection_state = SmtDetectionState::Invalidated;
            }
            if let Some(pda_id) = divergence.htf_pda_ref.as_ref().map(|pda| pda.id.clone()) {
                self.release_pda_reservation(&pda_id, id);
            }
        }
    }

    pub fn hydrate_divergences(
        &mut self,
        divergences: Vec<SmtDivergence>,
        fvg_ts_filled: &std::collections::HashMap<String, Option<i64>>,
    ) {
        let n = divergences.len();
        let mut cleared_ob = 0u32;
        let mut cleared_stale_fvg = 0u32;
        let mut divergences = divergences;
        // SQLite insertion time is not market time. Sorting is not required
        // for the historical snapshots themselves, but keeps hydration and
        // tracing deterministic across reconnects.
        divergences.sort_by(|a, b| {
            let a_ts = a
                .chains
                .iter()
                .find(|chain| chain.symbol == a.sweeper_symbol)
                .map(|chain| chain.smt_k_candle.ts)
                .unwrap_or(i64::MAX);
            let b_ts = b
                .chains
                .iter()
                .find(|chain| chain.symbol == b.sweeper_symbol)
                .map(|chain| chain.smt_k_candle.ts)
                .unwrap_or(i64::MAX);
            (a_ts, &a.id).cmp(&(b_ts, &b.id))
        });
        for mut d in divergences {
            if d.rule_version != RULE_VERSION {
                continue;
            }
            // Defensive compatibility guard. Current-rule rows can only own
            // FVG PDAs; an OB snapshot is never hydrated into the live engine.
            if d.htf_pda_ref.as_ref().is_some_and(|p| p.kind == "ob") {
                d.htf_pda_ref = None;
                cleared_ob += 1;
            }
            // Validate FVG PDA refs: a fill strictly before the SMT interval
            // makes the PDA stale. A fill on the SMT HTF bar itself is valid.
            // ts_filled comes from the PDA ref itself (new records) or
            // from fvg_ts_filled lookup (old records without the field).
            if let Some(ref pda) = d.htf_pda_ref {
                if pda.kind == "fvg" {
                    let ts_filled = pda
                        .ts_filled
                        .or_else(|| fvg_ts_filled.get(&pda.id).copied().flatten());
                    if let Some(ts_filled) = ts_filled {
                        let sweep_start = d
                            .observation_window
                            .1
                            .saturating_sub(d.context_timeframe.duration_ms());
                        if ts_filled < sweep_start {
                            d.htf_pda_ref = None;
                            cleared_stale_fvg += 1;
                        }
                    }
                }
            }
            // `htf_pda_ref` is an immutable historical fact: the PDA that was
            // valid when this SMT formed. Never clear that snapshot merely
            // because another persisted SMT references the same PDA. The
            // runtime `consumed_pdas` set has a different responsibility: it
            // blocks future detections from consuming any PDA represented in
            // history. Conflating the two previously made Candidate/Alert
            // rows point to an SMT whose PDA disappeared after restart.
            if d.rule_version == RULE_VERSION {
                if let Some(pda_id) = d.htf_pda_ref.as_ref().map(|pda| pda.id.clone()) {
                    match d
                        .chains
                        .iter()
                        .find(|chain| chain.symbol == d.sweeper_symbol)
                        .map(|chain| chain.detection_state)
                    {
                        Some(SmtDetectionState::C3Entry) if d.htf_confirmed => {
                            self.consume_pda(&pda_id);
                        }
                        Some(
                            SmtDetectionState::SmtKDetected
                            | SmtDetectionState::C2Confirmed
                            | SmtDetectionState::C3Entry,
                        ) => {
                            self.reserve_pda(&pda_id, &d.id);
                        }
                        _ => {}
                    }
                }
            }
            self.divergences.insert(d.id.clone(), d);
        }
        tracing::info!(
            hydrated = n,
            total = self.divergences.len(),
            cleared_ob,
            cleared_stale_fvg,
            "smt hydrated"
        );
    }

    /// Update a stored divergence (e.g. runtime fills `htf_pda_ref`).
    ///
    /// A replayed detection can be structurally newer while carrying no PDA
    /// because PDA selection is deliberately blocked by `consumed_pdas`.
    /// Do not let that empty runtime value erase the immutable formation-time
    /// snapshot already stored for the same SMT.
    pub fn update_divergence(&mut self, mut div: SmtDivergence) {
        if let Some(existing) = self.divergences.get(&div.id) {
            if div.htf_pda_ref.is_none() && existing.htf_pda_ref.is_some() {
                div.htf_pda_ref = existing.htf_pda_ref.clone();
            }
            if div.mtf_ref_candle.is_none() && existing.mtf_ref_candle.is_some() {
                div.mtf_ref_candle = existing.mtf_ref_candle.clone();
            }
        }
        if div.rule_version == RULE_VERSION {
            if let Some(pda_id) = div.htf_pda_ref.as_ref().map(|pda| pda.id.clone()) {
                match div
                    .chains
                    .iter()
                    .find(|chain| chain.symbol == div.sweeper_symbol)
                    .map(|chain| chain.detection_state)
                {
                    Some(SmtDetectionState::C3Entry) if div.htf_confirmed => {
                        self.consume_pda(&pda_id);
                    }
                    Some(
                        SmtDetectionState::SmtKDetected
                        | SmtDetectionState::C2Confirmed
                        | SmtDetectionState::C3Entry,
                    ) => {
                        self.reserve_pda(&pda_id, &div.id);
                    }
                    _ => self.release_pda_reservation(&pda_id, &div.id),
                }
            }
        }
        self.divergences.insert(div.id.clone(), div);
    }

    /// Backfill per-symbol C1/SMT-K/C2/C3 chains on historical rows that
    /// were persisted while one of the correlated MTF feeds was lagging.
    ///
    /// Every symbol in `symbol_set` must be chart-auditable from SMT Inbox.
    /// Return `None` when the row is already complete or when the canonical
    /// MTF bars are not available, so callers never persist a guessed chain.
    pub fn repair_missing_chains(&self, divergence: &SmtDivergence) -> Option<SmtDivergence> {
        let mut expected_symbols = divergence.symbol_set.clone();
        if expected_symbols.is_empty() {
            expected_symbols = divergence
                .liquidity_refs
                .iter()
                .map(|liquidity| liquidity.symbol.clone())
                .collect();
        }
        expected_symbols.sort();
        expected_symbols.dedup();
        if expected_symbols.is_empty()
            || expected_symbols.iter().all(|symbol| {
                divergence
                    .chains
                    .iter()
                    .any(|chain| chain.symbol == *symbol)
            })
        {
            return None;
        }

        let was_invalidated = divergence
            .chains
            .iter()
            .find(|chain| chain.symbol == divergence.sweeper_symbol)
            .is_some_and(|chain| chain.detection_state == SmtDetectionState::Invalidated);
        let mut repaired = divergence.clone();
        for symbol in &expected_symbols {
            if repaired.chains.iter().any(|chain| chain.symbol == *symbol) {
                continue;
            }
            let liquidity = divergence
                .liquidity_refs
                .iter()
                .find(|liquidity| liquidity.symbol == *symbol)?;
            let smt_k_ts = liquidity.mtf_sweep_candle.as_ref()?.ts;
            let direction = if *symbol == divergence.sweeper_symbol {
                divergence.candidate_direction
            } else if divergence.relationship == Correlation::Negative {
                divergence.candidate_direction.opposite()
            } else {
                divergence.candidate_direction
            };
            let mut chain =
                self.build_chain(symbol, divergence.comparison_timeframe, smt_k_ts, direction)?;
            if was_invalidated {
                chain.detection_state = SmtDetectionState::Invalidated;
            }
            repaired.chains.push(chain);
        }

        repaired.chains.sort_by_key(|chain| {
            expected_symbols
                .iter()
                .position(|symbol| symbol == &chain.symbol)
                .unwrap_or(usize::MAX)
        });
        Some(repaired)
    }

    /// Verify that every pane's persisted chain is still exactly resolvable
    /// from canonical MTF bars.  SMT Inbox promises that every visible row
    /// can draw all three panes; a missing C1 must therefore invalidate the
    /// row instead of letting the frontend silently shorten a C1-C3 box.
    pub fn canonical_chain_invalidation_reason(
        &self,
        divergence: &SmtDivergence,
    ) -> Option<SmtInvalidationReason> {
        let mut expected_symbols = divergence.symbol_set.clone();
        if expected_symbols.is_empty() {
            expected_symbols = divergence
                .liquidity_refs
                .iter()
                .map(|liquidity| liquidity.symbol.clone())
                .collect();
        }
        expected_symbols.sort();
        expected_symbols.dedup();
        for symbol in expected_symbols {
            let Some(stored) = divergence
                .chains
                .iter()
                .find(|chain| chain.symbol == symbol)
            else {
                return Some(SmtInvalidationReason::CanonicalChainChanged);
            };
            let direction = if symbol == divergence.sweeper_symbol {
                divergence.candidate_direction
            } else if divergence.relationship == Correlation::Negative {
                divergence.candidate_direction.opposite()
            } else {
                divergence.candidate_direction
            };
            let Some(rebuilt) = self.build_chain(
                &symbol,
                divergence.comparison_timeframe,
                stored.smt_k_candle.ts,
                direction,
            ) else {
                return Some(SmtInvalidationReason::CanonicalChainChanged);
            };
            if !candle_refs_match(&stored.c1_candle, &rebuilt.c1_candle)
                || !candle_refs_match(&stored.smt_k_candle, &rebuilt.smt_k_candle)
                || !optional_candle_refs_match(
                    stored.c2_candle.as_ref(),
                    rebuilt.c2_candle.as_ref(),
                )
                || !optional_candle_refs_match(
                    stored.c3_candle.as_ref(),
                    rebuilt.c3_candle.as_ref(),
                )
                || stored.c2_case != rebuilt.c2_case
            {
                return Some(SmtInvalidationReason::CanonicalChainChanged);
            }
        }
        None
    }

    /// Stamp a stable right edge for the PDA snapshot carried by one SMT.
    /// This is market-evidence time, never a future price-exit lookup.
    pub fn stamp_pda_display_end(divergence: &mut SmtDivergence) {
        let Some(chain) = divergence
            .chains
            .iter()
            .find(|chain| chain.symbol == divergence.sweeper_symbol)
        else {
            return;
        };
        let dur = divergence.comparison_timeframe.duration_ms();
        let end = match chain.detection_state {
            SmtDetectionState::C3Entry => chain.c3_candle.as_ref().map(|c| c.ts + dur),
            SmtDetectionState::Invalidated => chain
                .c3_candle
                .as_ref()
                .or(chain.c2_candle.as_ref())
                .map(|c| c.ts + dur)
                .or(Some(chain.smt_k_candle.ts + dur)),
            SmtDetectionState::SmtKDetected | SmtDetectionState::C2Confirmed => None,
        };
        if let Some(pda) = divergence.htf_pda_ref.as_mut() {
            pda.exit_ts = end;
        }
    }

    /// Audit a persisted PDA-SMT against the canonical bars currently held
    /// by the replay engine.
    ///
    /// Historical bars can be repaired/re-aggregated after an SMT was first
    /// stored.  The PDA snapshot itself is immutable, but its liquidity
    /// reference must still resolve to the same real HTF extreme and the
    /// current HTF bars must still form a sweep divergence.  `None` means the
    /// bounded local history cannot prove either result (for example a left
    /// edge bar is missing); callers must not invalidate on that basis alone.
    pub fn pda_history_invalidation_reasons(
        &self,
        divergence: &SmtDivergence,
    ) -> Option<Vec<SmtInvalidationReason>> {
        let pda = divergence.htf_pda_ref.as_ref()?;
        let htf = divergence.context_timeframe;
        let mtf = divergence.comparison_timeframe;
        let sweeper_ref = divergence
            .liquidity_refs
            .iter()
            .find(|liq| liq.symbol == divergence.sweeper_symbol)?;
        let kind = match sweeper_ref.side {
            LiquiditySide::BuySide => SwingKind::High,
            LiquiditySide::SellSide => SwingKind::Low,
        };
        let ref_start = divergence.observation_window.0;
        let ref_duration = if sweeper_ref.tf == mtf {
            mtf.duration_ms()
        } else {
            htf.duration_ms()
        };
        let ref_end = ref_start.saturating_add(ref_duration);
        let sweep_end = divergence.observation_window.1;
        let sweep_start = sweep_end.saturating_sub(htf.duration_ms());
        let reference_candle = self.interval_extreme_candle(
            &divergence.sweeper_symbol,
            mtf,
            ref_start,
            ref_end,
            kind,
        )?;
        let sweep_candle = self.interval_extreme_candle(
            &divergence.sweeper_symbol,
            mtf,
            sweep_start,
            sweep_end,
            kind,
        )?;
        let canonical_price = match kind {
            SwingKind::High => reference_candle.high,
            SwingKind::Low => reference_candle.low,
        };

        let mut reasons = Vec::new();
        let price_epsilon = canonical_price.abs().max(sweeper_ref.ref_price.abs()) * 1e-10 + 1e-12;
        if sweeper_ref.ref_ts != ref_start
            || (canonical_price - sweeper_ref.ref_price).abs() > price_epsilon
            || canonical_price < pda.price_low
            || canonical_price > pda.price_high
        {
            reasons.push(SmtInvalidationReason::CanonicalReferenceChanged);
        }
        if check_sweep(
            &Bar {
                symbol: divergence.sweeper_symbol.clone(),
                tf: htf,
                ts: sweep_start,
                open: sweep_candle.open,
                high: sweep_candle.high,
                low: sweep_candle.low,
                close: sweep_candle.close,
                volume: 0.0,
            },
            canonical_price,
            kind,
        ) != LiquidityRefStatus::Swept
            || match kind {
                SwingKind::High => sweep_candle.high,
                SwingKind::Low => sweep_candle.low,
            } < pda.price_low
            || match kind {
                SwingKind::High => sweep_candle.high,
                SwingKind::Low => sweep_candle.low,
            } > pda.price_high
        {
            reasons.push(SmtInvalidationReason::CanonicalSweepInvalid);
        }

        // A liquidity reference belongs to its earliest taking interval.
        // Once a failed C2/C3 releases it, a later interval may retry only
        // by making a strict new adverse extreme beyond every intervening
        // child candle.  This prevents a later, lower high/higher low from
        // being drawn as the sweep endpoint.
        if !self.reference_take_is_eligible(
            pda,
            &divergence.sweeper_symbol,
            mtf,
            ref_start,
            ref_end,
            sweeper_ref.tf,
            kind,
            canonical_price,
            sweep_start,
            &sweep_candle,
        ) {
            reasons.push(SmtInvalidationReason::ReferenceAlreadyTaken);
        }

        let mut missing_counter = false;
        let mut known_counter = false;
        let mut has_unswept_counter = false;
        for liq in divergence
            .liquidity_refs
            .iter()
            .filter(|liq| liq.symbol != divergence.sweeper_symbol)
        {
            let counter_kind = match liq.side {
                LiquiditySide::BuySide => SwingKind::High,
                LiquiditySide::SellSide => SwingKind::Low,
            };
            let Some(counter_ref) =
                self.interval_extreme_candle(&liq.symbol, mtf, ref_start, ref_end, counter_kind)
            else {
                missing_counter = true;
                continue;
            };
            let Some(counter_sweep) = self.interval_extreme_candle(
                &liq.symbol,
                mtf,
                sweep_start,
                sweep_end,
                counter_kind,
            ) else {
                missing_counter = true;
                continue;
            };
            let ref_price = match counter_kind {
                SwingKind::High => counter_ref.high,
                SwingKind::Low => counter_ref.low,
            };
            let status = check_sweep(
                &Bar {
                    symbol: liq.symbol.clone(),
                    tf: htf,
                    ts: sweep_start,
                    open: counter_sweep.open,
                    high: counter_sweep.high,
                    low: counter_sweep.low,
                    close: counter_sweep.close,
                    volume: 0.0,
                },
                ref_price,
                counter_kind,
            );
            known_counter = true;
            if status != LiquidityRefStatus::Swept {
                has_unswept_counter = true;
            }
        }
        if has_unswept_counter {
            Some(reasons)
        } else if missing_counter || !known_counter {
            (!reasons.is_empty()).then_some(reasons)
        } else {
            reasons.push(SmtInvalidationReason::AllCountersSwept);
            reasons.sort_by_key(|reason| match reason {
                SmtInvalidationReason::AllCountersSwept => 0,
                SmtInvalidationReason::CanonicalReferenceChanged => 1,
                SmtInvalidationReason::CanonicalSweepInvalid => 2,
                SmtInvalidationReason::ReferenceAlreadyTaken => 3,
                SmtInvalidationReason::CanonicalChainChanged => 4,
                SmtInvalidationReason::SweeperC2Failed => 5,
                SmtInvalidationReason::SweeperC3Failed => 6,
                SmtInvalidationReason::PdaConsumedByOtherSmt => 7,
                SmtInvalidationReason::HtfFormationCancelled => 8,
            });
            Some(reasons)
        }
    }

    /// Rebuild the market-evidence fields of a persisted PDA-SMT from the
    /// canonical replay bars. This is paired with historical invalidation so
    /// the Inbox never shows an old `EqualHighLow/strong` payload beside a
    /// newly proven `AllCountersSwept` reason.
    pub fn canonical_pda_snapshot(&self, divergence: &SmtDivergence) -> Option<SmtDivergence> {
        divergence.htf_pda_ref.as_ref()?;
        let htf = divergence.context_timeframe;
        let mtf = divergence.comparison_timeframe;
        let sweeper_index = divergence
            .liquidity_refs
            .iter()
            .position(|liquidity| liquidity.symbol == divergence.sweeper_symbol)?;
        let mut rebuilt = divergence.clone();
        rebuilt.rule_version = RULE_VERSION.into();
        let sweeper_symbol = rebuilt.sweeper_symbol.clone();

        let side = rebuilt.liquidity_refs[sweeper_index].side;
        let sweeper_kind = match side {
            LiquiditySide::BuySide => SwingKind::High,
            LiquiditySide::SellSide => SwingKind::Low,
        };
        let ref_start = rebuilt.observation_window.0;
        let ref_duration = if rebuilt.liquidity_refs[sweeper_index].tf == mtf {
            mtf.duration_ms()
        } else {
            htf.duration_ms()
        };
        let ref_end = ref_start.saturating_add(ref_duration);
        let sweep_end = rebuilt.observation_window.1;
        let sweep_start = sweep_end.saturating_sub(htf.duration_ms());
        let reference_candle =
            self.interval_extreme_candle(&sweeper_symbol, mtf, ref_start, ref_end, sweeper_kind)?;
        let sweep_candle = self.interval_extreme_candle(
            &sweeper_symbol,
            mtf,
            sweep_start,
            sweep_end,
            sweeper_kind,
        )?;
        let canonical_price = match sweeper_kind {
            SwingKind::High => reference_candle.high,
            SwingKind::Low => reference_candle.low,
        };
        {
            let sweeper = &mut rebuilt.liquidity_refs[sweeper_index];
            sweeper.ref_ts = ref_start;
            sweeper.ref_price = canonical_price;
            sweeper.mtf_ref_candle = Some(reference_candle.clone());
            sweeper.mtf_sweep_candle = Some(sweep_candle.clone());
            sweeper.status = check_sweep(
                &Bar {
                    symbol: sweeper_symbol.clone(),
                    tf: htf,
                    ts: sweep_start,
                    open: sweep_candle.open,
                    high: sweep_candle.high,
                    low: sweep_candle.low,
                    close: sweep_candle.close,
                    volume: 0.0,
                },
                canonical_price,
                sweeper_kind,
            );
        }

        for liquidity in rebuilt
            .liquidity_refs
            .iter_mut()
            .filter(|liquidity| liquidity.symbol != sweeper_symbol)
        {
            let kind = match liquidity.side {
                LiquiditySide::BuySide => SwingKind::High,
                LiquiditySide::SellSide => SwingKind::Low,
            };
            let reference =
                self.interval_extreme_candle(&liquidity.symbol, mtf, ref_start, ref_end, kind)?;
            let sweep =
                self.interval_extreme_candle(&liquidity.symbol, mtf, sweep_start, sweep_end, kind)?;
            let price = match kind {
                SwingKind::High => reference.high,
                SwingKind::Low => reference.low,
            };
            let status = check_sweep(
                &Bar {
                    symbol: liquidity.symbol.clone(),
                    tf: htf,
                    ts: sweep_start,
                    open: sweep.open,
                    high: sweep.high,
                    low: sweep.low,
                    close: sweep.close,
                    volume: 0.0,
                },
                price,
                kind,
            );
            liquidity.ref_ts = ref_start;
            liquidity.ref_price = price;
            liquidity.tf = htf;
            liquidity.status = status;
            liquidity.mtf_ref_candle = Some(reference);
            liquidity.mtf_sweep_candle = Some(sweep);
        }
        rebuilt.trade_symbols = rebuilt
            .liquidity_refs
            .iter()
            .filter(|liquidity| liquidity.symbol != sweeper_symbol)
            .filter(|liquidity| liquidity.status != LiquidityRefStatus::Swept)
            .map(|liquidity| liquidity.symbol.clone())
            .collect();
        rebuilt.strength = rebuilt
            .liquidity_refs
            .iter()
            .filter(|liquidity| liquidity.symbol != sweeper_symbol)
            .map(|liquidity| StrengthLabel {
                symbol: liquidity.symbol.clone(),
                label: if liquidity.status == LiquidityRefStatus::Swept {
                    "weak".into()
                } else {
                    "strong".into()
                },
            })
            .collect();
        rebuilt.mtf_ref_candle = Some(reference_candle);
        Some(rebuilt)
    }

    pub fn pda_snapshot_matches_history(&self, divergence: &SmtDivergence) -> Option<bool> {
        self.pda_history_invalidation_reasons(divergence)
            .map(|reasons| reasons.is_empty())
    }

    /// Rebuild the sweeper's MTF chain from canonical replay bars and report
    /// a C2 failure only when the required candles are present. Missing
    /// history stays inconclusive rather than being guessed.
    pub fn canonical_c2_invalidation_reason(
        &self,
        divergence: &SmtDivergence,
    ) -> Option<SmtInvalidationReason> {
        let smt_k_ts = divergence
            .chains
            .iter()
            .find(|chain| chain.symbol == divergence.sweeper_symbol)
            .map(|chain| chain.smt_k_candle.ts)?;
        let chain = self.build_chain(
            &divergence.sweeper_symbol,
            divergence.comparison_timeframe,
            smt_k_ts,
            divergence.candidate_direction,
        )?;
        (chain.detection_state == SmtDetectionState::Invalidated).then_some(
            if chain.c2_candle.is_some() && chain.c3_candle.is_some() {
                SmtInvalidationReason::SweeperC3Failed
            } else {
                SmtInvalidationReason::SweeperC2Failed
            },
        )
    }

    /// Check only the immutable reference endpoint against the canonical HTF
    /// candle. Unlike the full audit above, this does not require the counter
    /// symbols to have a complete covering HTF bar, so it can decisively
    /// reject a stale stored price even when a counterpart has a local data
    /// gap at the left edge.
    pub fn pda_reference_matches_history(&self, divergence: &SmtDivergence) -> Option<bool> {
        divergence.htf_pda_ref.as_ref()?;
        let htf = divergence.context_timeframe;
        let mtf = divergence.comparison_timeframe;
        let sweeper_ref = divergence
            .liquidity_refs
            .iter()
            .find(|liq| liq.symbol == divergence.sweeper_symbol)?;
        let kind = match sweeper_ref.side {
            LiquiditySide::BuySide => SwingKind::High,
            LiquiditySide::SellSide => SwingKind::Low,
        };
        let ref_start = divergence.observation_window.0;
        let duration = if sweeper_ref.tf == mtf {
            mtf.duration_ms()
        } else {
            htf.duration_ms()
        };
        let reference = self.interval_extreme_candle(
            &divergence.sweeper_symbol,
            mtf,
            ref_start,
            ref_start.saturating_add(duration),
            kind,
        )?;
        let canonical_price = match kind {
            SwingKind::High => reference.high,
            SwingKind::Low => reference.low,
        };
        let price_epsilon = canonical_price.abs().max(sweeper_ref.ref_price.abs()) * 1e-10 + 1e-12;
        Some(
            sweeper_ref.ref_ts == ref_start
                && (canonical_price - sweeper_ref.ref_price).abs() <= price_epsilon,
        )
    }

    fn complete_interval_bars(
        &self,
        symbol: &str,
        tf: Timeframe,
        start_ts: i64,
        end_ts: i64,
    ) -> Option<Vec<&Bar>> {
        let dur = tf.duration_ms();
        if end_ts <= start_ts || (end_ts - start_ts) % dur != 0 {
            return None;
        }
        let expected_ts = expected_market_bar_opens(symbol, tf, start_ts, end_ts);
        if expected_ts.is_empty() {
            return None;
        }
        let bucket = self.buckets.get(&(symbol.to_string(), tf))?;
        let bars: Vec<&Bar> = bucket
            .bars
            .iter()
            .filter(|bar| bar.ts >= start_ts && bar.ts < end_ts)
            .collect();
        if bars.len() != expected_ts.len()
            || bars
                .iter()
                .zip(&expected_ts)
                .any(|(bar, expected_ts)| bar.ts != *expected_ts)
        {
            return None;
        }
        Some(bars)
    }

    fn interval_extreme_candle(
        &self,
        symbol: &str,
        tf: Timeframe,
        start_ts: i64,
        end_ts: i64,
        kind: SwingKind,
    ) -> Option<CandleRef> {
        let bars = self.complete_interval_bars(symbol, tf, start_ts, end_ts)?;
        let mut best = *bars.first()?;
        for bar in bars.into_iter().skip(1) {
            let better = match kind {
                SwingKind::High => bar.high > best.high,
                SwingKind::Low => bar.low < best.low,
            };
            if better {
                best = bar;
            }
        }
        Some(candle_ref(best))
    }

    fn aggregate_interval(
        &self,
        symbol: &str,
        tf: Timeframe,
        start_ts: i64,
        end_ts: i64,
    ) -> Option<Bar> {
        let bars = self.complete_interval_bars(symbol, tf, start_ts, end_ts)?;
        let first = *bars.first()?;
        let last = *bars.last()?;
        Some(Bar {
            symbol: symbol.to_string(),
            tf,
            ts: start_ts,
            open: first.open,
            high: bars
                .iter()
                .map(|bar| bar.high)
                .fold(f64::NEG_INFINITY, f64::max),
            low: bars.iter().map(|bar| bar.low).fold(f64::INFINITY, f64::min),
            close: last.close,
            volume: bars.iter().map(|bar| bar.volume).sum(),
        })
    }

    fn first_pda_entry_ts(
        &self,
        symbol: &str,
        tf: Timeframe,
        pda: &PdaRef,
        before_ts: i64,
    ) -> Option<i64> {
        let bucket = self.buckets.get(&(symbol.to_string(), tf))?;
        // `ts_confirm` is the open timestamp of the FVG's third HTF candle.
        // The PDA is not known until that candle closes. A child MTF candle
        // inside the still-forming confirmation candle therefore cannot be a
        // post-PDA liquidity reference. Require price to leave the zone after
        // availability and then genuinely re-enter it.
        let phase_offset = self.pda_formation_phase(symbol, pda).1.unwrap_or(0);
        let available_at = pda
            .ts_confirm
            .saturating_add(phase_offset)
            .saturating_add(pda.tf.duration_ms());
        let mut departed = false;
        for bar in bucket
            .bars
            .iter()
            .filter(|bar| bar.ts >= available_at && bar.ts < before_ts)
        {
            let inside = overlaps(pda.price_low, pda.price_high, bar.low, bar.high);
            if !inside {
                departed = true;
            } else if departed {
                return Some(bar.ts);
            }
        }
        None
    }

    fn first_pda_depletion_ts(
        &self,
        symbol: &str,
        tf: Timeframe,
        pda: &PdaRef,
        before_ts: i64,
    ) -> Option<i64> {
        let bucket = self.buckets.get(&(symbol.to_string(), tf))?;
        let width = pda.price_high - pda.price_low;
        if !width.is_finite() || width <= 0.0 {
            return None;
        }
        let phase_offset = self.pda_formation_phase(symbol, pda).1.unwrap_or(0);
        let available_at = pda
            .ts_confirm
            .saturating_add(phase_offset)
            .saturating_add(pda.tf.duration_ms());
        bucket
            .bars
            .iter()
            .filter(|bar| bar.ts >= available_at && bar.ts < before_ts)
            .find_map(|bar| {
                let penetration = match pda.direction {
                    // A bullish FVG is revisited from above.
                    Direction::Bullish => (pda.price_high - bar.low) / width,
                    // A bearish FVG is revisited from below.
                    Direction::Bearish => (bar.high - pda.price_low) / width,
                };
                (penetration + 1e-12 >= PDA_SMT_MAX_PENETRATION).then_some(bar.ts)
            })
    }

    /// First bar that made this FVG ineligible as an SMT PDA.  Used by the
    /// chart projection to stop the PDA box at the same market fact used by
    /// detection.  It does not change the FVG's structural state.
    pub fn fvg_smt_depletion_ts(&self, fvg: &Fvg) -> Option<i64> {
        let comparison_tf = mtf_for_htf(fvg.tf)?;
        let pda = PdaRef {
            kind: "fvg".into(),
            id: fvg.id.clone(),
            tf: fvg.tf,
            direction: fvg.direction,
            price_low: fvg.price_low,
            price_high: fvg.price_high,
            ts_open: fvg.ts_open,
            ts_confirm: fvg.ts_confirm,
            exit_ts: fvg.consumed_exit_ts,
            ts_filled: fvg.ts_filled,
        };
        self.first_pda_depletion_ts(&fvg.symbol, comparison_tf, &pda, i64::MAX)
    }

    fn pda_market_age_bars(&self, symbol: &str, pda: &PdaRef, sweep_ts: i64) -> Option<usize> {
        let bucket = self.buckets.get(&(symbol.to_string(), pda.tf))?;
        Some(
            bucket
                .bars
                .iter()
                .filter(|bar| bar.ts > pda.ts_open && bar.ts <= sweep_ts)
                .count(),
        )
    }

    /// Reproduce the immutable three-candle FVG geometry on the exact native
    /// HTF grid carried by the structure timestamps.  A structure labelled
    /// H1 must use the same H1 opens shown by the app/TradingView; shifting
    /// the child window by M30 can manufacture a different, non-visible H1
    /// FVG (for example the rejected 08/05 17:30 phase).
    ///
    /// The return shape is retained because callers also need the formation
    /// offset when calculating PDA availability. A canonical match always
    /// has offset zero.
    fn pda_formation_phase(&self, symbol: &str, pda: &PdaRef) -> (bool, Option<i64>) {
        let Some(child_tf) = mtf_for_htf(pda.tf) else {
            return (false, None);
        };
        let parent_dur = pda.tf.duration_ms();
        let expected_confirm = pda.ts_open.saturating_add(2 * parent_dur);
        if pda.ts_confirm != expected_confirm {
            // The three FVG candles must be consecutive native HTF bars.
            // A shifted or discontinuous persisted PdaRef is definitively
            // non-canonical even when its first/third prices happen to match.
            return (true, None);
        }
        let approximately_equal = |actual: f64, stored: f64| {
            let epsilon = actual.abs().max(stored.abs()) * 1e-10 + 1e-12;
            (actual - stored).abs() <= epsilon
        };
        let Some(first) = self.aggregate_interval(
            symbol,
            child_tf,
            pda.ts_open,
            pda.ts_open.saturating_add(parent_dur),
        ) else {
            return (false, None);
        };
        if self
            .aggregate_interval(
                symbol,
                child_tf,
                pda.ts_open.saturating_add(parent_dur),
                pda.ts_open.saturating_add(2 * parent_dur),
            )
            .is_none()
        {
            return (false, None);
        }
        let Some(third) = self.aggregate_interval(
            symbol,
            child_tf,
            pda.ts_confirm,
            pda.ts_confirm.saturating_add(parent_dur),
        ) else {
            return (false, None);
        };
        let matches = match pda.direction {
            Direction::Bullish => {
                first.high < third.low
                    && approximately_equal(first.high, pda.price_low)
                    && approximately_equal(third.low, pda.price_high)
            }
            Direction::Bearish => {
                first.low > third.high
                    && approximately_equal(third.high, pda.price_low)
                    && approximately_equal(first.low, pda.price_high)
            }
        };
        (true, matches.then_some(0))
    }

    fn pda_formation_matches_history(&self, symbol: &str, pda: &PdaRef) -> Option<bool> {
        let (had_complete_phase, matching_phase) = self.pda_formation_phase(symbol, pda);
        matching_phase
            .map(|_| true)
            .or_else(|| had_complete_phase.then_some(false))
    }

    fn canonical_pda_candidates(
        &self,
        symbol: &str,
        htf: Timeframe,
        sweep: &CandleRef,
        structures: &[IctStructure],
    ) -> Vec<PdaRef> {
        let mut candidates: Vec<PdaRef> = compute_htf_pda_refs(structures, sweep)
            .into_iter()
            .filter(|pda| pda.tf == htf)
            .filter(|pda| pda.ts_confirm.saturating_add(pda.tf.duration_ms()) <= sweep.ts)
            .filter(|pda| !self.is_pda_consumed(&pda.id))
            .filter(|pda| {
                self.pda_formation_matches_history(symbol, pda)
                    .unwrap_or(false)
            })
            // Reaching 50% changes the FVG state to Mitigated50 but does not
            // consume it. Only a full fill, market-bar expiry, or a valid SMT
            // C3 consumes/expires this PDA.
            .filter(|pda| {
                let max_bars = match pda.tf {
                    Timeframe::H4 => 42,
                    Timeframe::H1 => 72,
                    _ => return false,
                };
                self.pda_market_age_bars(symbol, pda, sweep.ts)
                    .is_some_and(|age| age <= max_bars)
            })
            .collect();
        candidates.sort_by(|a, b| {
            b.ts_confirm
                .cmp(&a.ts_confirm)
                .then_with(|| {
                    let a_width = a.price_high - a.price_low;
                    let b_width = b.price_high - b.price_low;
                    a_width
                        .partial_cmp(&b_width)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.id.cmp(&b.id))
        });
        candidates
    }

    fn confirmed_swing_candidates(
        &self,
        symbol: &str,
        tf: Timeframe,
        kind: SwingKind,
        before_ts: i64,
        pda: &PdaRef,
        entry_ts: Option<i64>,
        scope_rank: impl Fn(i64) -> (ReferenceScope, u8),
    ) -> Vec<PdaLiquidityCandidate> {
        let Some(bucket) = self.buckets.get(&(symbol.to_string(), tf)) else {
            return Vec::new();
        };
        let dur = tf.duration_ms();
        let bars: Vec<&Bar> = bucket.bars.iter().collect();
        let mut out = Vec::new();
        for window in bars.windows(3) {
            let previous = window[0];
            let candidate = window[1];
            let confirming = window[2];
            if confirming.ts.saturating_add(dur) > before_ts {
                continue;
            }
            let is_swing = match kind {
                SwingKind::High => {
                    candidate.high >= previous.high && candidate.high >= confirming.high
                }
                SwingKind::Low => candidate.low <= previous.low && candidate.low <= confirming.low,
            };
            if !is_swing {
                continue;
            }
            let price = match kind {
                SwingKind::High => candidate.high,
                SwingKind::Low => candidate.low,
            };
            if price < pda.price_low || price > pda.price_high {
                continue;
            }
            if tf != pda.tf {
                let Some(entry) = entry_ts else {
                    continue;
                };
                if candidate.ts < entry {
                    continue;
                }
            }
            let first_check_ts = confirming.ts.saturating_add(dur);
            let already_taken = bucket.bars.iter().any(|bar| {
                bar.ts >= first_check_ts
                    && bar.ts < before_ts
                    && check_sweep(bar, price, kind) == LiquidityRefStatus::Swept
            });
            let previous_failed_extreme = already_taken
                .then(|| {
                    self.released_reference_watermark(
                        pda,
                        symbol,
                        candidate.ts,
                        tf,
                        kind,
                        before_ts,
                    )
                })
                .flatten();
            if already_taken && previous_failed_extreme.is_none() {
                continue;
            }
            let (scope, rank) = scope_rank(candidate.ts);
            out.push(PdaLiquidityCandidate {
                kind,
                price,
                ts: candidate.ts,
                tf,
                scope,
                rank,
                previous_failed_extreme,
            });
        }
        out
    }

    fn pda_liquidity_candidates(
        &self,
        symbol: &str,
        htf: Timeframe,
        mtf: Timeframe,
        sweep_start: i64,
        pda: &PdaRef,
        kind: SwingKind,
    ) -> Vec<PdaLiquidityCandidate> {
        let entry_ts = self.first_pda_entry_ts(symbol, mtf, pda, sweep_start);
        let mut refs =
            self.confirmed_swing_candidates(symbol, htf, kind, sweep_start, pda, entry_ts, |ts| {
                if entry_ts.is_some_and(|entry| ts >= entry) {
                    (ReferenceScope::LocalNearPda, 1)
                } else {
                    (ReferenceScope::DistantLeftSide, 0)
                }
            });
        // Priority 2: the immediately preceding completed HTF candle may be
        // new local liquidity formed after price entered this PDA.  It does
        // not yet have a right-side candle and therefore cannot satisfy the
        // three-candle swing detector, but the live 08/10 11:00 -> 12:00
        // case is still a legitimate adjacent HTF liquidity take.  Both the
        // nominated extreme and the sweep extreme are independently required
        // to stay inside the same DXY PDA by the caller.
        let previous_start = sweep_start.saturating_sub(htf.duration_ms());
        if let (Some(entry), Some(reference_candle)) = (
            entry_ts,
            self.interval_extreme_candle(symbol, mtf, previous_start, sweep_start, kind),
        ) {
            let price = match kind {
                SwingKind::High => reference_candle.high,
                SwingKind::Low => reference_candle.low,
            };
            if reference_candle.ts >= entry && price >= pda.price_low && price <= pda.price_high {
                refs.push(PdaLiquidityCandidate {
                    kind,
                    price,
                    ts: previous_start,
                    tf: htf,
                    scope: ReferenceScope::LocalNearPda,
                    rank: 1,
                    previous_failed_extreme: None,
                });
            }
        }
        refs.extend(self.confirmed_swing_candidates(
            symbol,
            mtf,
            kind,
            sweep_start,
            pda,
            entry_ts,
            |_| (ReferenceScope::LocalNearPda, 2),
        ));
        refs
    }

    fn containing_htf_interval(&self, symbol: &str, htf: Timeframe, ts: i64) -> Option<(i64, i64)> {
        let bucket = self.buckets.get(&(symbol.to_string(), htf))?;
        let parent = bucket
            .bars
            .iter()
            .rev()
            .find(|bar| bar.ts <= ts && ts < bar.ts.saturating_add(htf.duration_ms()))?;
        Some((parent.ts, parent.ts.saturating_add(htf.duration_ms())))
    }

    fn reference_interval(
        &self,
        symbol: &str,
        reference: &PdaLiquidityCandidate,
        htf: Timeframe,
    ) -> Option<(i64, i64)> {
        if reference.tf == htf {
            Some((reference.ts, reference.ts.saturating_add(htf.duration_ms())))
        } else {
            // A local MTF swing only nominates the liquidity. HTF SMT
            // evidence must use the complete native DXY parent interval.
            self.containing_htf_interval(symbol, htf, reference.ts)
        }
    }

    /// Return the adverse watermark a later retry must exceed after a prior
    /// C2/C3 failure released this reference.  The watermark covers every
    /// canonical child bar from the latest failed SMT K up to (but excluding)
    /// the proposed retry interval; comparing only against the failed SMT K
    /// allowed a lower later high/higher later low to masquerade as a new
    /// liquidity take.
    fn released_reference_watermark(
        &self,
        pda: &PdaRef,
        symbol: &str,
        reference_ts: i64,
        reference_tf: Timeframe,
        kind: SwingKind,
        before_ts: i64,
    ) -> Option<f64> {
        let ref_start = if reference_tf == pda.tf {
            reference_ts
        } else {
            self.containing_htf_interval(symbol, pda.tf, reference_ts)?
                .0
        };
        let latest_failed_ts = self
            .divergences
            .values()
            .filter(|divergence| {
                divergence
                    .htf_pda_ref
                    .as_ref()
                    .is_some_and(|item| item.id == pda.id)
            })
            .filter(|divergence| divergence.observation_window.0 == ref_start)
            .filter(|divergence| {
                divergence.invalidation_reasons.iter().any(|reason| {
                    matches!(
                        reason,
                        SmtInvalidationReason::SweeperC2Failed
                            | SmtInvalidationReason::SweeperC3Failed
                    )
                })
            })
            .filter_map(|divergence| {
                let chain = divergence
                    .chains
                    .iter()
                    .find(|chain| chain.symbol == divergence.sweeper_symbol)?;
                (chain.smt_k_candle.ts < before_ts).then_some(chain.smt_k_candle.ts)
            })
            .max()?;
        let child_tf = mtf_for_htf(pda.tf)?;
        let bucket = self.buckets.get(&(symbol.to_string(), child_tf))?;
        bucket
            .bars
            .iter()
            .filter(|bar| bar.ts >= latest_failed_ts && bar.ts < before_ts)
            .map(|bar| match kind {
                SwingKind::High => bar.high,
                SwingKind::Low => bar.low,
            })
            .reduce(|a, b| match kind {
                SwingKind::High => a.max(b),
                SwingKind::Low => a.min(b),
            })
    }

    #[allow(clippy::too_many_arguments)]
    fn reference_take_is_eligible(
        &self,
        pda: &PdaRef,
        symbol: &str,
        mtf: Timeframe,
        ref_start: i64,
        ref_end: i64,
        reference_tf: Timeframe,
        kind: SwingKind,
        reference_price: f64,
        sweep_start: i64,
        sweep_candle: &CandleRef,
    ) -> bool {
        let was_taken_earlier =
            self.buckets
                .get(&(symbol.to_string(), mtf))
                .is_some_and(|bucket| {
                    bucket.bars.iter().any(|bar| {
                        bar.ts >= ref_end
                            && bar.ts < sweep_start
                            && check_sweep(bar, reference_price, kind) == LiquidityRefStatus::Swept
                    })
                });
        if !was_taken_earlier {
            return true;
        }
        self.released_reference_watermark(pda, symbol, ref_start, reference_tf, kind, sweep_start)
            .is_some_and(|watermark| match kind {
                SwingKind::High => sweep_candle.high > watermark,
                SwingKind::Low => sweep_candle.low < watermark,
            })
    }

    pub fn queue_pda_htf_close(&mut self, bar: &Bar) {
        if !is_index_symbol(&bar.symbol) || !SMT_HTF_TFS.contains(&bar.tf) {
            return;
        }
        if !self.pending_pda_htf_closes.iter().any(|pending| {
            pending.symbol == bar.symbol && pending.tf == bar.tf && pending.ts == bar.ts
        }) {
            self.pending_pda_htf_closes.push(bar.clone());
            if self.pending_pda_htf_closes.len() > HISTORY_CAP {
                self.pending_pda_htf_closes.remove(0);
            }
        }
    }

    /// Return each newly-complete partial HTF window exactly once, after the
    /// same canonical MTF children are present for every watchlist symbol.
    /// Full parent windows continue through `queue_pda_htf_close`; this path
    /// exists only to make live C2/C3 useful before the HTF close.
    pub fn take_ready_forming_htf_windows(&mut self, bar: &Bar) -> Vec<(Bar, i64)> {
        let Some(watchlist) = self.watchlist.clone() else {
            return Vec::new();
        };
        let Some(index_symbol) = watchlist
            .symbols
            .iter()
            .find(|symbol| is_index_symbol(symbol))
            .cloned()
        else {
            return Vec::new();
        };
        let mut ready = Vec::new();
        for htf in SMT_HTF_TFS {
            let Some(mtf) = mtf_for_htf(*htf) else {
                continue;
            };
            if bar.tf != mtf {
                continue;
            }
            let start = htf.boundary_align(bar.ts);
            let available_end = bar.ts.saturating_add(mtf.duration_ms());
            let parent_end = start.saturating_add(htf.duration_ms());
            if available_end <= start || available_end >= parent_end {
                continue;
            }
            let key = (*htf, start, available_end);
            if self.evaluated_forming_windows.contains(&key)
                || !watchlist.symbols.iter().all(|symbol| {
                    self.complete_interval_bars(symbol, mtf, start, available_end)
                        .is_some()
                })
            {
                continue;
            }
            let Some(mut partial) =
                self.aggregate_interval(&index_symbol, mtf, start, available_end)
            else {
                continue;
            };
            partial.tf = *htf;
            self.evaluated_forming_windows.insert(key);
            ready.push((partial, available_end));
        }
        ready
    }

    fn pda_htf_close_inputs_ready(&self, bar: &Bar) -> bool {
        let Some(watchlist) = &self.watchlist else {
            return false;
        };
        let Some(mtf) = mtf_for_htf(bar.tf) else {
            return false;
        };
        let start = bar.ts;
        let end = start.saturating_add(bar.tf.duration_ms());
        watchlist.symbols.iter().all(|symbol| {
            self.complete_interval_bars(symbol, mtf, start, end)
                .is_some()
        })
    }

    pub fn take_ready_pda_htf_closes(&mut self) -> Vec<Bar> {
        let mut ready = Vec::new();
        let pending = std::mem::take(&mut self.pending_pda_htf_closes);
        for bar in pending {
            if self.pda_htf_close_inputs_ready(&bar) {
                ready.push(bar);
            } else {
                self.pending_pda_htf_closes.push(bar);
            }
        }
        ready.sort_by_key(|bar| {
            (
                bar.ts.saturating_add(bar.tf.duration_ms()),
                -bar.tf.duration_ms(),
            )
        });
        ready
    }

    /// Formally confirm a PDA-SMT on the DXY HTF close. Any matching live
    /// provisional row is promoted in place; a provisional row which no
    /// longer survives the completed parent interval is cancelled.
    pub fn detect_pda_htf_close(
        &mut self,
        bar: &Bar,
        structures: &[IctStructure],
    ) -> Vec<StructureEvent> {
        let end = bar.ts.saturating_add(bar.tf.duration_ms());
        let events = self.detect_pda_htf_window(bar, structures, end, true);
        self.reconcile_provisional_window(bar.tf, bar.ts, events, true, end)
    }

    /// Evaluate a synchronized, still-forming HTF interval. C2 may create a
    /// Candidate immediately, but a valid C3 only reserves the PDA here; the
    /// formal HTF close is the sole point that can consume it.
    pub fn detect_pda_htf_forming(
        &mut self,
        bar: &Bar,
        available_end: i64,
        structures: &[IctStructure],
    ) -> Vec<StructureEvent> {
        let parent_end = bar.ts.saturating_add(bar.tf.duration_ms());
        if available_end <= bar.ts || available_end >= parent_end {
            return Vec::new();
        }
        let events = self.detect_pda_htf_window(bar, structures, available_end, false);
        self.reconcile_provisional_window(bar.tf, bar.ts, events, false, available_end)
    }

    fn detect_pda_htf_window(
        &mut self,
        bar: &Bar,
        structures: &[IctStructure],
        available_end: i64,
        finalized: bool,
    ) -> Vec<StructureEvent> {
        if !is_index_symbol(&bar.symbol) || !SMT_HTF_TFS.contains(&bar.tf) {
            return Vec::new();
        }
        let Some(wl) = self.watchlist.clone() else {
            return Vec::new();
        };
        let htf = bar.tf;
        let Some(mtf) = mtf_for_htf(htf) else {
            return Vec::new();
        };
        let sweep_start = bar.ts;
        let sweep_end = available_end;
        let Some(sweeper_htf) = self.aggregate_interval(&bar.symbol, mtf, sweep_start, sweep_end)
        else {
            return Vec::new();
        };
        let sweep = candle_ref(&sweeper_htf);

        for pda in self.canonical_pda_candidates(&bar.symbol, htf, &sweep, structures) {
            let kind = match pda.direction {
                Direction::Bearish => SwingKind::High,
                Direction::Bullish => SwingKind::Low,
            };
            let sweep_extreme = match kind {
                SwingKind::High => sweep.high,
                SwingKind::Low => sweep.low,
            };
            if sweep_extreme < pda.price_low || sweep_extreme > pda.price_high {
                continue;
            }
            let mut refs =
                self.pda_liquidity_candidates(&bar.symbol, htf, mtf, sweep_start, &pda, kind);
            refs.retain(|reference| {
                check_sweep(&sweeper_htf, reference.price, reference.kind)
                    == LiquidityRefStatus::Swept
            });
            sort_pda_liquidity_candidates(&mut refs, kind);
            let Some(dxy_sweep_mtf) =
                self.interval_extreme_candle(&bar.symbol, mtf, sweep_start, sweep_end, kind)
            else {
                continue;
            };
            // SMT K itself is excluded.  A PDA already traversed by 90% on
            // any earlier DXY MTF bar is stale for SMT purposes even though
            // the FVG engine may correctly still call it Mitigated50.
            if self
                .first_pda_depletion_ts(&bar.symbol, mtf, &pda, dxy_sweep_mtf.ts)
                .is_some()
            {
                continue;
            }
            refs.retain(|reference| {
                reference
                    .previous_failed_extreme
                    .is_none_or(|failed| match kind {
                        SwingKind::High => dxy_sweep_mtf.high > failed,
                        SwingKind::Low => dxy_sweep_mtf.low < failed,
                    })
            });
            let mut selected = None;
            let mut terminal_all_swept = None;
            let mut divergence_refs = Vec::new();
            for reference in &refs {
                let Some((ref_start, ref_end)) =
                    self.reference_interval(&bar.symbol, reference, htf)
                else {
                    continue;
                };
                let Some(dxy_ref_mtf) =
                    self.interval_extreme_candle(&bar.symbol, mtf, ref_start, ref_end, kind)
                else {
                    continue;
                };
                let canonical_ref_price = match kind {
                    SwingKind::High => dxy_ref_mtf.high,
                    SwingKind::Low => dxy_ref_mtf.low,
                };
                if canonical_ref_price < pda.price_low
                    || canonical_ref_price > pda.price_high
                    || check_sweep(&sweeper_htf, canonical_ref_price, kind)
                        != LiquidityRefStatus::Swept
                {
                    continue;
                }
                if !self.reference_take_is_eligible(
                    &pda,
                    &bar.symbol,
                    mtf,
                    ref_start,
                    ref_end,
                    reference.tf,
                    kind,
                    canonical_ref_price,
                    sweep_start,
                    &dxy_sweep_mtf,
                ) {
                    continue;
                }
                let mut counters = Vec::new();
                for other in &wl.symbols {
                    if other == &bar.symbol {
                        continue;
                    }
                    let Some(pair) = wl.correlations.iter().find(|pair| {
                        (pair.a == bar.symbol && pair.b == *other)
                            || (pair.b == bar.symbol && pair.a == *other)
                    }) else {
                        continue;
                    };
                    let expected_kind = expected_counter_swing(kind, pair.direction);
                    let Some(ref_candle) =
                        self.interval_extreme_candle(other, mtf, ref_start, ref_end, expected_kind)
                    else {
                        return Vec::new();
                    };
                    let Some(sweep_candle) = self.interval_extreme_candle(
                        other,
                        mtf,
                        sweep_start,
                        sweep_end,
                        expected_kind,
                    ) else {
                        return Vec::new();
                    };
                    let ref_price = match expected_kind {
                        SwingKind::High => ref_candle.high,
                        SwingKind::Low => ref_candle.low,
                    };
                    let status = check_sweep(
                        &Bar {
                            symbol: other.clone(),
                            tf: htf,
                            ts: sweep_start,
                            open: sweep_candle.open,
                            high: sweep_candle.high,
                            low: sweep_candle.low,
                            close: sweep_candle.close,
                            volume: 0.0,
                        },
                        ref_price,
                        expected_kind,
                    );
                    counters.push(CounterInfo {
                        symbol: other.clone(),
                        correlation: pair.direction,
                        expected_kind,
                        status,
                        ref_price,
                        ref_ts: ref_start,
                        mtf_ref_candle: ref_candle,
                        mtf_sweep_candle: sweep_candle,
                    });
                }
                if counters.is_empty() {
                    return Vec::new();
                }
                let all_swept = counters
                    .iter()
                    .all(|counter| counter.status == LiquidityRefStatus::Swept);
                let evaluation = (
                    reference.clone(),
                    ref_start,
                    canonical_ref_price,
                    dxy_ref_mtf,
                    counters,
                );
                if all_swept {
                    if terminal_all_swept.is_none() {
                        terminal_all_swept = Some(evaluation);
                    }
                } else {
                    divergence_refs.push(reference.clone());
                    if selected.is_none() {
                        selected = Some(evaluation);
                    }
                }
            }
            // A higher-priority DXY reference that every counter also swept
            // is not an SMT. Continue down the approved reference hierarchy.
            // If none creates divergence, retain the highest-priority
            // all-swept evaluation only as terminal audit evidence.
            let selected_has_divergence = selected.is_some();
            let Some((primary, ref_start, canonical_ref_price, dxy_ref_mtf, counters)) =
                selected.or(terminal_all_swept)
            else {
                continue;
            };

            let direction = match kind {
                SwingKind::High => Direction::Bearish,
                SwingKind::Low => Direction::Bullish,
            };
            let swing = Swing {
                ts: sweep_start,
                price: canonical_ref_price,
                kind,
            };
            let Some(mut divergence) = self.build_multi_smt(
                &wl,
                &bar.symbol,
                &swing,
                direction,
                htf,
                mtf,
                htf,
                dxy_sweep_mtf.ts,
                canonical_ref_price,
                ref_start,
                counters,
            ) else {
                return Vec::new();
            };
            divergence.reference_scope = primary.scope;
            divergence.htf_confirmed = finalized;
            // Navigation and the HTF white line always use the complete
            // parent interval. `available_end` only limits the provisional
            // comparison evidence used during this evaluation.
            divergence.observation_window =
                (ref_start, sweep_start.saturating_add(htf.duration_ms()));
            let primary_interval = self.reference_interval(&bar.symbol, &primary, htf);
            divergence.confluence_refs = divergence_refs
                .iter()
                .filter(|reference| {
                    selected_has_divergence
                        && self.reference_interval(&bar.symbol, reference, htf) != primary_interval
                })
                .map(|reference| SmtReferenceEvidence {
                    ref_price: reference.price,
                    ref_ts: reference.ts,
                    side: swing_side(reference.kind),
                    tf: reference.tf,
                    scope: reference.scope,
                })
                .collect();
            if let Some(liquidity) = divergence
                .liquidity_refs
                .iter_mut()
                .find(|liquidity| liquidity.symbol == bar.symbol)
            {
                liquidity.ref_price = canonical_ref_price;
                liquidity.ref_ts = ref_start;
                liquidity.tf = htf;
                liquidity.mtf_ref_candle = Some(dxy_ref_mtf.clone());
                liquidity.mtf_sweep_candle = Some(dxy_sweep_mtf.clone());
            }
            divergence.mtf_ref_candle = Some(dxy_ref_mtf);
            divergence.htf_pda_ref = Some(pda.clone());
            Self::stamp_pda_display_end(&mut divergence);
            let state = divergence
                .chains
                .iter()
                .find(|chain| chain.symbol == divergence.sweeper_symbol)
                .map(|chain| chain.detection_state);
            let all_counters_swept = divergence
                .liquidity_refs
                .iter()
                .filter(|liquidity| liquidity.symbol != divergence.sweeper_symbol)
                .all(|liquidity| liquidity.status == LiquidityRefStatus::Swept);
            let mut events = Vec::new();
            match state.filter(|_| !all_counters_swept) {
                Some(SmtDetectionState::C3Entry) if finalized => {
                    self.consume_pda(&pda.id);
                    let winner_ts = divergence
                        .chains
                        .iter()
                        .find(|chain| chain.symbol == divergence.sweeper_symbol)
                        .and_then(|chain| chain.c3_candle.as_ref())
                        .map(|candle| candle.ts)
                        .unwrap_or(available_end);
                    events.extend(self.invalidate_pda_competitors(
                        &pda.id,
                        &divergence.id,
                        winner_ts,
                    ));
                }
                Some(
                    SmtDetectionState::SmtKDetected
                    | SmtDetectionState::C2Confirmed
                    | SmtDetectionState::C3Entry,
                ) => {
                    self.reserve_pda(&pda.id, &divergence.id);
                }
                _ => {}
            }
            events.extend(self.emit_smt(divergence));
            return events;
        }
        Vec::new()
    }

    fn is_provisional_id(&self, id: &str) -> bool {
        self.provisional_windows
            .values()
            .any(|ids| ids.contains(id))
    }

    fn reconcile_provisional_window(
        &mut self,
        htf: Timeframe,
        start: i64,
        mut events: Vec<StructureEvent>,
        finalized: bool,
        invalidation_ts: i64,
    ) -> Vec<StructureEvent> {
        let key = (htf, start);
        let current_ids: HashSet<String> = events
            .iter()
            .filter_map(|event| match event {
                StructureEvent::New(IctStructure::SmtDivergence(divergence))
                | StructureEvent::Update(IctStructure::SmtDivergence(divergence))
                    if divergence.context_timeframe == htf
                        && divergence
                            .chains
                            .iter()
                            .find(|chain| chain.symbol == divergence.sweeper_symbol)
                            .is_some_and(|chain| {
                                chain.detection_state != SmtDetectionState::Invalidated
                            })
                        && divergence
                            .liquidity_refs
                            .iter()
                            .filter(|liquidity| liquidity.symbol != divergence.sweeper_symbol)
                            .any(|liquidity| liquidity.status != LiquidityRefStatus::Swept) =>
                {
                    Some(divergence.id.clone())
                }
                _ => None,
            })
            .collect();
        let previous_ids = self.provisional_windows.remove(&key).unwrap_or_default();
        for stale_id in previous_ids.difference(&current_ids) {
            let Some(mut stale) = self.divergences.get(stale_id).cloned() else {
                continue;
            };
            let already_invalid = stale
                .chains
                .iter()
                .find(|chain| chain.symbol == stale.sweeper_symbol)
                .is_some_and(|chain| chain.detection_state == SmtDetectionState::Invalidated);
            if already_invalid {
                continue;
            }
            for chain in &mut stale.chains {
                chain.detection_state = SmtDetectionState::Invalidated;
            }
            if !stale
                .invalidation_reasons
                .contains(&SmtInvalidationReason::HtfFormationCancelled)
            {
                stale
                    .invalidation_reasons
                    .push(SmtInvalidationReason::HtfFormationCancelled);
            }
            stale.invalidation_ts = Some(invalidation_ts);
            if let Some(pda_id) = stale.htf_pda_ref.as_ref().map(|pda| pda.id.clone()) {
                self.release_pda_reservation(&pda_id, &stale.id);
            }
            events.extend(self.emit_smt(stale));
        }
        if !finalized && !current_ids.is_empty() {
            self.provisional_windows.insert(key, current_ids);
        } else if finalized {
            self.evaluated_forming_windows
                .retain(|(window_htf, window_start, _)| {
                    *window_htf != htf || *window_start != start
                });
        }
        events
    }

    /// Rebuild the MTF white-line start from canonical bars. Historical rows
    /// persisted before `mtf_ref_candle` was introduced cannot safely fall
    /// back to the HTF bar-open timestamp: that can select the wrong MTF
    /// candle and even draw a price outside the stored PDA band.
    pub fn resolve_mtf_ref_candle(&self, divergence: &SmtDivergence) -> Option<CandleRef> {
        let pda = divergence.htf_pda_ref.as_ref()?;
        let liquidity = divergence
            .liquidity_refs
            .iter()
            .find(|liquidity| liquidity.symbol == divergence.sweeper_symbol)?;
        let kind = match liquidity.side {
            LiquiditySide::BuySide => SwingKind::High,
            LiquiditySide::SellSide => SwingKind::Low,
        };
        let duration = if liquidity.tf == divergence.comparison_timeframe {
            divergence.comparison_timeframe.duration_ms()
        } else {
            divergence.context_timeframe.duration_ms()
        };
        let candle = self.interval_extreme_candle(
            &divergence.sweeper_symbol,
            divergence.comparison_timeframe,
            divergence.observation_window.0,
            divergence.observation_window.0.saturating_add(duration),
            kind,
        )?;
        let extreme = match liquidity.side {
            LiquiditySide::BuySide => candle.high,
            LiquiditySide::SellSide => candle.low,
        };
        (extreme >= pda.price_low && extreme <= pda.price_high).then_some(candle)
    }

    /// Feed a closed bar; returns SMT events (New/Update/Invalidated).
    /// Accepts both MTF (30m/1h) and HTF (1h/4h) bars.
    pub fn on_closed_bar(&mut self, bar: &Bar) -> Vec<StructureEvent> {
        let wl = match &self.watchlist {
            Some(w) => w.clone(),
            None => return Vec::new(),
        };
        if !SMT_ALL_TFS.contains(&bar.tf) {
            return Vec::new();
        }
        if !wl.symbols.iter().any(|s| s == &bar.symbol) {
            return Vec::new();
        }
        let key = (bar.symbol.clone(), bar.tf);
        let bucket = self.buckets.entry(key).or_insert_with(SmtBucket::new);
        if bucket.bars.len() == HISTORY_CAP {
            bucket.bars.pop_front();
        }
        bucket.bars.push_back(bar.clone());
        let mut events = Vec::new();

        // Before DXY C2 is confirmed, a counter that later takes its paired
        // liquidity loses trade eligibility. If every counter catches up,
        // the divergence is gone and the PDA reservation is released.
        events.extend(self.review_pending_counter_sweeps(bar));

        // Deferred C2/C3 upgrade (Option B): when an MTF bar closes, fill
        // in C2/C3 for any pending SMT chain (SmtKDetected / C2Confirmed)
        // whose C2 (=SMT K+mtf) or C3 (=SMT K+2*mtf) bar just closed.
        events.extend(self.upgrade_pending_chains(bar));
        events
    }

    fn review_pending_counter_sweeps(&mut self, bar: &Bar) -> Vec<StructureEvent> {
        if !matches!(bar.tf, Timeframe::H1 | Timeframe::M30) || is_index_symbol(&bar.symbol) {
            return Vec::new();
        }
        let ids: Vec<String> = self
            .divergences
            .values()
            .filter(|divergence| divergence.rule_version == RULE_VERSION)
            .filter(|divergence| divergence.comparison_timeframe == bar.tf)
            .filter(|divergence| {
                self.is_provisional_id(&divergence.id)
                    || divergence
                        .chains
                        .iter()
                        .find(|chain| chain.symbol == divergence.sweeper_symbol)
                        .is_some_and(|chain| {
                            matches!(
                                chain.detection_state,
                                SmtDetectionState::SmtKDetected | SmtDetectionState::C2Confirmed
                            )
                        })
            })
            .filter(|divergence| {
                divergence.liquidity_refs.iter().any(|liquidity| {
                    liquidity.symbol == bar.symbol && liquidity.status != LiquidityRefStatus::Swept
                })
            })
            .map(|divergence| divergence.id.clone())
            .collect();
        let mut events = Vec::new();
        for id in ids {
            let Some(mut divergence) = self.divergences.get(&id).cloned() else {
                continue;
            };
            let Some(liquidity) = divergence
                .liquidity_refs
                .iter_mut()
                .find(|liquidity| liquidity.symbol == bar.symbol)
            else {
                continue;
            };
            let kind = match liquidity.side {
                LiquiditySide::BuySide => SwingKind::High,
                LiquiditySide::SellSide => SwingKind::Low,
            };
            if check_sweep(bar, liquidity.ref_price, kind) != LiquidityRefStatus::Swept {
                continue;
            }
            liquidity.status = LiquidityRefStatus::Swept;
            divergence
                .trade_symbols
                .retain(|symbol| symbol != &bar.symbol);
            if let Some(strength) = divergence
                .strength
                .iter_mut()
                .find(|strength| strength.symbol == bar.symbol)
            {
                strength.label = "weak".into();
            }
            let all_swept = divergence
                .liquidity_refs
                .iter()
                .filter(|liquidity| liquidity.symbol != divergence.sweeper_symbol)
                .all(|liquidity| liquidity.status == LiquidityRefStatus::Swept);
            if all_swept {
                for chain in &mut divergence.chains {
                    chain.detection_state = SmtDetectionState::Invalidated;
                }
                divergence
                    .invalidation_reasons
                    .push(SmtInvalidationReason::AllCountersSwept);
                divergence.invalidation_ts = Some(bar.ts);
                if let Some(pda_id) = divergence.htf_pda_ref.as_ref().map(|pda| pda.id.clone()) {
                    self.release_pda_reservation(&pda_id, &divergence.id);
                }
            }
            events.extend(self.emit_smt(divergence));
        }
        events
    }

    /// Deferred C2/C3 upgrade (Option B). Re-evaluates the chain for the
    /// symbol whose MTF bar just closed, if that bar is the C2 or C3 bar
    /// of a pending SMT. Emits Update events for upgraded divergences.
    fn upgrade_pending_chains(&mut self, bar: &Bar) -> Vec<StructureEvent> {
        // Only MTF bars (H1, M30) can be C2/C3 bars.
        if !matches!(bar.tf, Timeframe::H1 | Timeframe::M30) {
            return Vec::new();
        }
        let mtf = bar.tf;
        let dur = mtf.duration_ms();
        let bar_ts = bar.ts;
        let symbol = bar.symbol.clone();
        if !self
            .watchlist
            .as_ref()
            .is_some_and(|watchlist| watchlist.symbols.iter().any(|item| item == &symbol))
        {
            return Vec::new();
        }

        // Every symbol advances its own local chain. DXY alone controls the
        // shared Candidate/PDA lifecycle.
        let candidates: Vec<(String, i64, Direction, SymbolChain)> = self
            .divergences
            .values()
            .filter(|d| d.comparison_timeframe == mtf)
            .filter_map(|d| {
                let chain = d.chains.iter().find(|c| c.symbol == symbol)?;
                if !matches!(
                    chain.detection_state,
                    SmtDetectionState::SmtKDetected | SmtDetectionState::C2Confirmed
                ) {
                    return None;
                }
                let smt_k_ts = chain.smt_k_candle.ts;
                if bar_ts != smt_k_ts + dur && bar_ts != smt_k_ts + 2 * dur {
                    return None;
                }
                let direction = if symbol == d.sweeper_symbol {
                    d.candidate_direction
                } else if d.relationship == Correlation::Negative {
                    d.candidate_direction.opposite()
                } else {
                    d.candidate_direction
                };
                Some((d.id.clone(), smt_k_ts, direction, chain.clone()))
            })
            .collect();

        // Phase 2 (read): re-build each candidate chain (C2/C3 now in
        // bucket). Skip if nothing changed.
        let mut upgrades: Vec<(String, SymbolChain)> = candidates
            .into_iter()
            .filter_map(|(id, smt_k_ts, direction, old)| {
                let updated = self.build_chain(&symbol, mtf, smt_k_ts, direction)?;
                let changed = updated.detection_state != old.detection_state
                    || updated.c2_candle.as_ref().map(|c| c.ts)
                        != old.c2_candle.as_ref().map(|c| c.ts)
                    || updated.c3_candle.as_ref().map(|c| c.ts)
                        != old.c3_candle.as_ref().map(|c| c.ts);
                if !changed {
                    return None;
                }
                Some((id, updated))
            })
            .collect();
        // HashMap iteration order is unstable. Deterministic ordering matters
        // when several DXY attempts reach valid C3 on the same close.
        upgrades.sort_by(|(a_id, a), (b_id, b)| {
            a.c2_candle
                .as_ref()
                .map(|candle| candle.ts)
                .unwrap_or(i64::MAX)
                .cmp(
                    &b.c2_candle
                        .as_ref()
                        .map(|candle| candle.ts)
                        .unwrap_or(i64::MAX),
                )
                .then_with(|| a.smt_k_candle.ts.cmp(&b.smt_k_candle.ts))
                .then_with(|| a_id.cmp(b_id))
        });

        // Phase 3 (write): apply upgrades + emit Update events.
        let mut events = Vec::new();
        for (id, new_chain) in upgrades {
            if let Some(mut div) = self.divergences.get(&id).cloned() {
                let owner_is_invalidated = div
                    .chains
                    .iter()
                    .find(|chain| chain.symbol == div.sweeper_symbol)
                    .is_some_and(|chain| chain.detection_state == SmtDetectionState::Invalidated);
                if owner_is_invalidated {
                    continue;
                }
                let Some(chain) = div.chains.iter_mut().find(|chain| chain.symbol == symbol) else {
                    continue;
                };
                *chain = new_chain.clone();
                if symbol == div.sweeper_symbol
                    && new_chain.detection_state == SmtDetectionState::Invalidated
                {
                    div.invalidation_ts = Some(
                        new_chain
                            .c3_candle
                            .as_ref()
                            .map(|candle| candle.ts)
                            .unwrap_or(bar.ts),
                    );
                }
                if symbol == div.sweeper_symbol {
                    Self::stamp_pda_display_end(&mut div);
                }
                if symbol == div.sweeper_symbol {
                    if let Some(pda_id) = div.htf_pda_ref.as_ref().map(|pda| pda.id.clone()) {
                        match new_chain.detection_state {
                            SmtDetectionState::C3Entry if !self.is_provisional_id(&id) => {
                                self.consume_pda(&pda_id);
                                let winner_ts = new_chain
                                    .c3_candle
                                    .as_ref()
                                    .map(|candle| candle.ts)
                                    .unwrap_or(bar.ts);
                                events.extend(
                                    self.invalidate_pda_competitors(&pda_id, &id, winner_ts),
                                );
                            }
                            SmtDetectionState::C2Confirmed
                            | SmtDetectionState::SmtKDetected
                            | SmtDetectionState::C3Entry => {
                                self.reserve_pda(&pda_id, &id);
                            }
                            SmtDetectionState::Invalidated => {
                                self.release_pda_reservation(&pda_id, &id);
                            }
                        }
                    }
                }
                events.extend(self.emit_smt(div));
            }
        }
        events
    }

    /// Seed a bucket's history without emitting (warm-start).
    pub fn seed_bar(&mut self, bar: &Bar) {
        let Some(wl) = &self.watchlist else {
            return;
        };
        if !SMT_ALL_TFS.contains(&bar.tf) {
            return;
        }
        if !wl.symbols.iter().any(|s| s == &bar.symbol) {
            return;
        }
        let key = (bar.symbol.clone(), bar.tf);
        let bucket = self.buckets.entry(key).or_insert_with(SmtBucket::new);
        if bucket.bars.len() == HISTORY_CAP {
            bucket.bars.pop_front();
        }
        bucket.bars.push_back(bar.clone());
    }

    // ---- Multi-symbol SMT comparison ----
    //
    // When the sweeper (DXY) sweeps at a given TF, compare ALL watchlist
    // counters in a single divergence (§2.2: each involved symbol gets its
    // own chain + liquidity_ref). An SMT is confirmed when DXY swept AND at
    // least one counter did NOT sweep. Every involved symbol then gets a
    // white line on both HTF and MTF panes (§5.8).

    /// Build chains, liquidity_refs, strength and the SmtDivergence for a
    /// confirmed multi-symbol SMT. Shared by the HTF and MTF paths.
    #[allow(clippy::too_many_arguments)]
    fn build_multi_smt(
        &self,
        wl: &Watchlist,
        sweeper: &str,
        swing: &Swing,
        direction: Direction,
        sweep_tf: Timeframe,
        chain_tf: Timeframe,
        context_tf: Timeframe,
        smt_k_ts: i64,
        sweeper_ref_price: f64,
        sweeper_ref_ts: i64,
        counters: Vec<CounterInfo>,
    ) -> Option<SmtDivergence> {
        let owner_chain = self.build_chain(sweeper, chain_tf, smt_k_ts, direction)?;
        let mut chains = vec![owner_chain.clone()];
        for ci in &counters {
            let counter_direction = match ci.correlation {
                Correlation::Negative => direction.opposite(),
                Correlation::Positive => direction,
            };
            chains.push(self.build_chain(
                &ci.symbol,
                chain_tf,
                ci.mtf_sweep_candle.ts,
                counter_direction,
            )?);
        }

        // The same DXY structure can legitimately be evaluated by more than
        // one strategy group.  Include the group in the persisted identity so
        // SQLite rows and downstream candidate IDs cannot overwrite/collide.
        let id = smt_id(&wl.id, sweeper, sweep_tf, chain_tf, direction, smt_k_ts);
        let win_start = swing.ts;
        let win_end = swing.ts.saturating_add(sweep_tf.duration_ms());

        let mut liquidity_refs = vec![LiquidityRef {
            symbol: sweeper.to_string(),
            ref_price: sweeper_ref_price,
            ref_ts: sweeper_ref_ts,
            side: swing_side(swing.kind),
            status: LiquidityRefStatus::Swept,
            tf: sweep_tf,
            mtf_ref_candle: None,
            mtf_sweep_candle: self
                .buckets
                .get(&(sweeper.to_string(), chain_tf))
                .and_then(|bucket| bucket.bar_at(smt_k_ts))
                .map(candle_ref),
        }];
        for ci in &counters {
            liquidity_refs.push(LiquidityRef {
                symbol: ci.symbol.clone(),
                ref_price: ci.ref_price,
                ref_ts: ci.ref_ts,
                side: swing_side(ci.expected_kind),
                status: ci.status,
                tf: sweep_tf,
                mtf_ref_candle: Some(ci.mtf_ref_candle.clone()),
                mtf_sweep_candle: Some(ci.mtf_sweep_candle.clone()),
            });
        }

        let mut strength = Vec::new();
        for ci in &counters {
            let label = if ci.status == LiquidityRefStatus::Swept {
                "weak"
            } else {
                "strong"
            };
            strength.push(StrengthLabel {
                symbol: ci.symbol.clone(),
                label: label.into(),
            });
        }

        let trade_symbols: Vec<String> = counters
            .iter()
            .filter(|c| c.status != LiquidityRefStatus::Swept)
            .map(|c| c.symbol.clone())
            .collect();
        // M6c groups deliberately keep every DXY↔counter edge the same sign,
        // so one relationship describes the whole divergence. A future
        // mixed-correlation group must persist/evaluate this per counter.
        let relationship = counters
            .first()
            .map(|c| c.correlation)
            .unwrap_or(Correlation::Negative);
        let symbol_set: Vec<String> = std::iter::once(sweeper.to_string())
            .chain(counters.iter().map(|c| c.symbol.clone()))
            .collect();

        let invalidation_ts = chains
            .iter()
            .find(|chain| chain.symbol == sweeper)
            .filter(|chain| chain.detection_state == SmtDetectionState::Invalidated)
            .map(|chain| {
                chain
                    .c3_candle
                    .as_ref()
                    .map(|candle| candle.ts)
                    .unwrap_or_else(|| chain.smt_k_candle.ts.saturating_add(chain_tf.duration_ms()))
            });

        Some(SmtDivergence {
            id,
            watchlist_id: wl.id.clone(),
            rule_version: RULE_VERSION.into(),
            symbol_set,
            relationship,
            context_timeframe: context_tf,
            comparison_timeframe: chain_tf,
            observation_window: (win_start, win_end),
            htf_confirmed: true,
            reference_scope: ReferenceScope::DistantLeftSide,
            liquidity_refs,
            confluence_refs: Vec::new(),
            candidate_direction: direction,
            sweeper_symbol: sweeper.to_string(),
            trade_symbols,
            strength,
            chains,
            invalidation_reasons: Vec::new(),
            invalidation_ts,
            htf_pda_ref: None,
            mtf_ref_candle: None,
        })
    }

    /// Insert/replace a divergence and emit New or Update.
    fn emit_smt(&mut self, mut div: SmtDivergence) -> Vec<StructureEvent> {
        let is_update = self.divergences.contains_key(&div.id);
        if is_update {
            if let Some(existing) = self.divergences.get(&div.id) {
                if div.htf_pda_ref.is_none() && existing.htf_pda_ref.is_some() {
                    div.htf_pda_ref = existing.htf_pda_ref.clone();
                    // A generic swing pass can rediscover the same frozen SMT
                    // after the PDA pass. Preserve the canonical PDA evidence
                    // rather than replacing it with a different HTF swing.
                    div.liquidity_refs = existing.liquidity_refs.clone();
                    div.trade_symbols = existing.trade_symbols.clone();
                    div.strength = existing.strength.clone();
                }
                if div.mtf_ref_candle.is_none() && existing.mtf_ref_candle.is_some() {
                    div.mtf_ref_candle = existing.mtf_ref_candle.clone();
                }
            }
        }
        self.divergences.insert(div.id.clone(), div.clone());
        let ev = if is_update {
            StructureEvent::Update(IctStructure::SmtDivergence(div))
        } else {
            StructureEvent::New(IctStructure::SmtDivergence(div))
        };
        vec![ev]
    }

    /// Build one symbol's canonical local C1/SMT K/C2/C3 chain.
    fn build_chain(
        &self,
        symbol: &str,
        tf: Timeframe,
        smt_k_ts: i64,
        direction: Direction,
    ) -> Option<SymbolChain> {
        let dur = tf.duration_ms();
        let bucket = self.buckets.get(&(symbol.to_string(), tf))?;
        let c1_bar = bucket.bar_at(smt_k_ts - dur)?;
        let smt_k_bar = bucket.bar_at(smt_k_ts)?;
        let c1_candle = candle_ref(c1_bar);
        let smt_k_candle = candle_ref(smt_k_bar);
        let (c2_candle, c2_case, c3_candle, detection_state) =
            self.evaluate_c2_c3(symbol, tf, smt_k_ts, direction, &c1_candle, &smt_k_candle);
        Some(SymbolChain {
            symbol: symbol.to_string(),
            c1_candle,
            smt_k_candle,
            c2_candle,
            c2_case,
            c3_candle,
            detection_state,
        })
    }

    /// Evaluate C2 (three cases, window=1) and C3.
    /// Returns (c2_candle, c2_case, c3_candle, detection_state).
    #[allow(clippy::too_many_arguments)]
    fn evaluate_c2_c3(
        &self,
        symbol: &str,
        tf: Timeframe,
        smt_k_ts: i64,
        direction: Direction,
        c1: &CandleRef,
        smt_k: &CandleRef,
    ) -> (
        Option<CandleRef>,
        Option<u8>,
        Option<CandleRef>,
        SmtDetectionState,
    ) {
        let dur = tf.duration_ms();
        let bucket = self.buckets.get(&(symbol.to_string(), tf));
        let smt_k_next: Option<CandleRef> = bucket
            .and_then(|b| b.bar_at(smt_k_ts + dur))
            .map(candle_ref);

        // Case 1 / Case 3: only on SMT K (C2 = SMT K). C3 = SMT K+1.
        // If C3 not yet closed, defer to C2Confirmed (upgraded later).
        if let Some(case) = check_c2_bar(smt_k.close, c1, direction) {
            let c3 = smt_k_next.clone();
            let state = match c3.as_ref() {
                Some(candle) if c3_is_valid(candle, c1, smt_k, direction, case) => {
                    SmtDetectionState::C3Entry
                }
                Some(_) => SmtDetectionState::Invalidated,
                None => SmtDetectionState::C2Confirmed,
            };
            return (Some(smt_k.clone()), Some(case), c3, state);
        }

        // Case 2: SMT K closes beyond the adverse side of C1. K+1 becomes
        // C2 once it reclaims SMT K's close without creating a new adverse
        // extreme. C1 only classifies this as case 2; it is intentionally
        // not the reclaim threshold for the two-step reversal.
        if let Some(next) = smt_k_next {
            let reclaimed = match direction {
                Direction::Bullish => next.close >= smt_k.close,
                Direction::Bearish => next.close <= smt_k.close,
            };
            let made_new_adverse_extreme = match direction {
                Direction::Bullish => next.low < smt_k.low,
                Direction::Bearish => next.high > smt_k.high,
            };
            if !in_range(smt_k.close, c1.low, c1.high) && reclaimed && !made_new_adverse_extreme {
                let c3 = bucket
                    .and_then(|b| b.bar_at(smt_k_ts + 2 * dur))
                    .map(candle_ref);
                let state = match c3.as_ref() {
                    Some(candle) if c3_is_valid(candle, c1, smt_k, direction, 2) => {
                        SmtDetectionState::C3Entry
                    }
                    Some(_) => SmtDetectionState::Invalidated,
                    None => SmtDetectionState::C2Confirmed,
                };
                return (Some(next), Some(2), c3, state);
            }
            // SMT K+1 closed but case 2 didn't match -> Invalidated.
            return (None, None, None, SmtDetectionState::Invalidated);
        }

        // SMT K+1 not yet closed -> defer case 2 evaluation (SmtKDetected).
        (None, None, None, SmtDetectionState::SmtKDetected)
    }
}

// ---- Helper functions -------------------------------------------------------

/// Check if a symbol is the index (DXY) - the only allowed sweeper (§2.2).
fn is_index_symbol(symbol: &str) -> bool {
    let ticker = symbol.split_once(':').map(|(_, t)| t).unwrap_or(symbol);
    ticker == "DXY"
}

fn candle_ref(bar: &Bar) -> CandleRef {
    CandleRef {
        ts: bar.ts,
        open: bar.open,
        high: bar.high,
        low: bar.low,
        close: bar.close,
    }
}

fn candle_refs_match(a: &CandleRef, b: &CandleRef) -> bool {
    let same_price = |left: f64, right: f64| {
        let epsilon = left.abs().max(right.abs()) * 1e-10 + 1e-12;
        (left - right).abs() <= epsilon
    };
    a.ts == b.ts
        && same_price(a.open, b.open)
        && same_price(a.high, b.high)
        && same_price(a.low, b.low)
        && same_price(a.close, b.close)
}

fn optional_candle_refs_match(a: Option<&CandleRef>, b: Option<&CandleRef>) -> bool {
    match (a, b) {
        (Some(left), Some(right)) => candle_refs_match(left, right),
        (None, None) => true,
        _ => false,
    }
}

fn smt_id(
    watchlist_id: &str,
    sweeper: &str,
    sweep_tf: Timeframe,
    chain_tf: Timeframe,
    dir: Direction,
    smt_k_ts: i64,
) -> String {
    let dir_s = match dir {
        Direction::Bullish => "bullish",
        Direction::Bearish => "bearish",
    };
    structure_id(&[
        watchlist_id,
        sweeper,
        sweep_tf.tag(),
        chain_tf.tag(),
        dir_s,
        &smt_k_ts.to_string(),
    ])
}

fn swing_side(kind: SwingKind) -> LiquiditySide {
    match kind {
        SwingKind::High => LiquiditySide::BuySide,
        SwingKind::Low => LiquiditySide::SellSide,
    }
}

fn expected_counter_swing(trigger: SwingKind, corr: Correlation) -> SwingKind {
    match (trigger, corr) {
        (SwingKind::High, Correlation::Positive) | (SwingKind::Low, Correlation::Negative) => {
            SwingKind::High
        }
        _ => SwingKind::Low,
    }
}

fn check_sweep(bar: &Bar, ref_price: f64, kind: SwingKind) -> LiquidityRefStatus {
    match kind {
        SwingKind::High => {
            if bar.high >= ref_price {
                LiquidityRefStatus::Swept
            } else {
                LiquidityRefStatus::NotSwept
            }
        }
        SwingKind::Low => {
            if bar.low <= ref_price {
                LiquidityRefStatus::Swept
            } else {
                LiquidityRefStatus::NotSwept
            }
        }
    }
}

fn check_c2_bar(close: f64, c1: &CandleRef, direction: Direction) -> Option<u8> {
    if in_range(close, c1.low, c1.high) {
        return Some(1);
    }
    match direction {
        Direction::Bullish => {
            if close > c1.high {
                Some(3)
            } else {
                None
            }
        }
        Direction::Bearish => {
            if close < c1.low {
                Some(3)
            } else {
                None
            }
        }
    }
}

fn c3_is_valid(
    c3: &CandleRef,
    c1: &CandleRef,
    smt_k: &CandleRef,
    direction: Direction,
    c2_case: u8,
) -> bool {
    // Case 2 is a two-step reclaim: C2 first recovers SMT K's close and C3
    // must hold that recovered boundary. Cases 1/3 already reclaimed C1 on
    // SMT K itself, so their C3 continues to hold the original C1 boundary.
    let reclaim_boundary = if c2_case == 2 {
        smt_k.close
    } else {
        match direction {
            Direction::Bullish => c1.low,
            Direction::Bearish => c1.high,
        }
    };
    match direction {
        // Equality is a liquidity touch and remains valid. Only a strict new
        // adverse extreme or a close that loses the applicable reclaim
        // boundary invalidates C3.
        Direction::Bullish => c3.low >= smt_k.low && c3.close >= reclaim_boundary,
        Direction::Bearish => c3.high <= smt_k.high && c3.close <= reclaim_boundary,
    }
}

fn in_range(val: f64, lo: f64, hi: f64) -> bool {
    val >= lo && val <= hi
}

/// Compute HTF PDA context reference by checking overlap between SMT K
/// candle and OB/FVG zones (§4.1, §5.4). Used by runtime post-processing.
///
/// Filters:
/// - `ts_confirm < sweep.ts` (sweep must be strictly after PDA
///   confirmation - the confirm bar itself is part of the formation,
///   not a return to the zone)
/// - OB state: Active | Tested
/// - FVG state: Active | Mitigated50 (only before Filled)
/// - Zone `[price_low, price_high]` overlaps SMT K `[low, high]`
pub fn compute_htf_pda_ref(structures: &[IctStructure], smt_k: &CandleRef) -> Option<PdaRef> {
    compute_htf_pda_refs(structures, smt_k).into_iter().next()
}

/// Compute ALL HTF PDA context references (OB/FVG) that overlap with the
/// sweep bar's price range (§4.1, §5.4). The runtime iterates these in
/// order and picks the first PDA where a valid swept swing reference is
/// found inside the zone (§1.4, §5.8).
pub fn compute_htf_pda_refs(structures: &[IctStructure], sweep: &CandleRef) -> Vec<PdaRef> {
    let mut out = Vec::new();
    for s in structures {
        match s {
            // OB removed from PDA candidates per user request ("OB区先不作为pda").
            // Only FVGs serve as PDA zones for SMT sweep matching.
            IctStructure::Fvg(fvg) => {
                if fvg.ts_confirm >= sweep.ts {
                    continue;
                }
                // Filled/IFVG snapshots are only valid historically when the
                // original FVG fill is timestamped on or after this SMT bar.
                // This prevents an IFVG from being treated as a fresh PDA.
                if matches!(
                    fvg.state,
                    FvgState::Filled | FvgState::InvertedActive | FvgState::InvertedMitigated
                ) && fvg.ts_filled.is_none()
                {
                    continue;
                }
                // Only consider FVGs that were still pre-Filled (Active or
                // Mitigated50) at the time of the sweep. We check ts_filled
                // (the bar that transitioned the FVG to Filled) rather than
                // the current state, because the FVG may have been filled
                // or inverted *after* the sweep occurred. If ts_filled is
                // None (never filled) or > sweep.ts (filled after the
                // sweep), the FVG was valid at sweep time.
                if let Some(ts_filled) = fvg.ts_filled {
                    if ts_filled < sweep.ts {
                        continue;
                    }
                }
                // Market-bar expiry is evaluated by SmtEngine because this
                // standalone mapper has no access to canonical DXY bars.
                if overlaps(fvg.price_low, fvg.price_high, sweep.low, sweep.high) {
                    out.push(PdaRef {
                        kind: "fvg".into(),
                        id: fvg.id.clone(),
                        tf: fvg.tf,
                        direction: fvg.direction,
                        price_low: fvg.price_low,
                        price_high: fvg.price_high,
                        ts_open: fvg.ts_open,
                        ts_confirm: fvg.ts_confirm,
                        exit_ts: None,
                        ts_filled: fvg.ts_filled,
                    });
                }
            }
            _ => {}
        }
    }
    // IctEngine stores structures in hash maps, so incoming order is not
    // stable. Prefer the most recently confirmed valid FVG and use ID as a
    // deterministic tie-breaker.
    out.sort_by(|a, b| {
        b.ts_confirm
            .cmp(&a.ts_confirm)
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

fn overlaps(a_lo: f64, a_hi: f64, b_lo: f64, b_hi: f64) -> bool {
    a_lo <= b_hi && b_lo <= a_hi
}

#[cfg(test)]
mod tests;
