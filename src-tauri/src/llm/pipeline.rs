use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use crate::alert::AlertRecord;
use crate::candidate::{
    CandidateSetup, DecisionLogEntry, DecisionMode, DecisionRecord, DecisionStrategy,
    PackedEvidence,
};
use crate::detector::types::{structure_id, SmtDivergence};
use crate::storage::SqliteStore;

use super::{
    ContextPacker, LlmDecision, LlmProvider, M7D_CONTEXT_VERSION, M7D_PROMPT_VERSION,
    M7D_STRATEGY_VERSION,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    SkippedSeeding,
    Duplicate,
    Stored(String),
}

pub struct LlmSingleCallStrategy {
    provider: Arc<dyn LlmProvider>,
    provider_name: String,
    model: Option<String>,
    packer: ContextPacker,
}

impl LlmSingleCallStrategy {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        provider_name: impl Into<String>,
        model: Option<String>,
    ) -> Self {
        Self {
            provider,
            provider_name: provider_name.into(),
            model,
            packer: ContextPacker::default(),
        }
    }

    pub fn with_packer(mut self, packer: ContextPacker) -> Self {
        self.packer = packer;
        self
    }

    fn request_json(&self, evidence: &PackedEvidence) -> Result<String, String> {
        self.packer
            .request(evidence)
            .and_then(|request| serde_json::to_string(&request).map_err(|error| error.to_string()))
    }

    fn pending_record(&self, request_json: String) -> DecisionRecord {
        DecisionRecord {
            provider: self.provider_name.clone(),
            model: self.model.clone(),
            decision_mode: DecisionMode::LlmSingleCall,
            prompt_version: Some(M7D_PROMPT_VERSION.into()),
            strategy_version: M7D_STRATEGY_VERSION.into(),
            context_version: format!("{M7D_CONTEXT_VERSION}.bounded-v1"),
            request_json,
            raw_response: None,
            parsed_decision_json: "{}".into(),
            parse_ok: false,
            error: None,
        }
    }
}

#[async_trait]
impl DecisionStrategy for LlmSingleCallStrategy {
    async fn evaluate(&self, evidence: &PackedEvidence) -> DecisionRecord {
        let request = match self.packer.request(evidence) {
            Ok(request) => request,
            Err(error) => {
                let mut record = self.pending_record("{}".into());
                record.error = Some(format!("context pack failed: {error}"));
                return record;
            }
        };
        let request_json = serde_json::to_string(&request).unwrap_or_else(|_| "{}".into());
        let mut record = self.pending_record(request_json);

        match self.provider.decide(request.clone()).await {
            Ok(response) => {
                record.raw_response = Some(response.raw_response.clone());
                match serde_json::from_str::<LlmDecision>(&response.raw_response)
                    .map_err(|error| error.to_string())
                    .and_then(|decision| decision.apply_guardrails(&request.context))
                {
                    Ok(decision) => {
                        record.parsed_decision_json =
                            serde_json::to_string(&decision).unwrap_or_else(|_| "{}".into());
                        record.parse_ok = true;
                    }
                    Err(error) => record.error = Some(format!("parse/validate failed: {error}")),
                }
            }
            Err(error) => record.error = Some(format!("provider failed: {error}")),
        }
        record
    }
}

/// Durable M7 sidecar. The alert row is created first by AlertEngine; this
/// pipeline only observes that fact and can never mutate or suppress it.
pub struct LlmDecisionPipeline {
    store: SqliteStore,
    strategy: Arc<LlmSingleCallStrategy>,
    max_calls_per_day: u32,
    in_flight: Arc<parking_lot::Mutex<HashSet<String>>>,
}

impl LlmDecisionPipeline {
    pub fn new(store: SqliteStore, strategy: Arc<LlmSingleCallStrategy>) -> Self {
        Self {
            store,
            strategy,
            max_calls_per_day: u32::MAX,
            in_flight: Arc::new(parking_lot::Mutex::new(HashSet::new())),
        }
    }

    pub fn with_max_calls_per_day(mut self, max_calls_per_day: u32) -> Self {
        self.max_calls_per_day = max_calls_per_day;
        self
    }

    pub fn decision_id(watchlist_id: &str, alert_id: &str, trade_symbol: &str) -> String {
        structure_id(&[watchlist_id, alert_id, trade_symbol])
    }

    fn evidence(
        &self,
        candidate: &CandidateSetup,
        alert: &AlertRecord,
        smt: &SmtDivergence,
    ) -> PackedEvidence {
        let trade_symbol = alert
            .validation_symbol
            .clone()
            .or_else(|| alert.trade_symbols.first().cloned());
        let mut evidence = PackedEvidence {
            candidate_id: candidate.id.clone(),
            candidate: candidate.clone(),
            watchlist_id: alert.watchlist_id.clone(),
            alert_id: Some(alert.id.clone()),
            trade_symbol: trade_symbol.clone(),
            as_of_ts: alert.created_at,
            smt: smt.clone(),
            ltf_cisd_mss: Vec::new(),
            pda_context: smt.htf_pda_ref.clone(),
            liquidity_refs: smt.liquidity_refs.clone(),
            strength: smt.strength.clone(),
            context_version: format!("{M7D_CONTEXT_VERSION}.bounded-v1"),
            strategy_version: M7D_STRATEGY_VERSION.into(),
            market_bars: Vec::new(),
            market_structures: Vec::new(),
        };
        if let Some(trade_symbol) = trade_symbol.as_deref() {
            let symbols = [smt.sweeper_symbol.as_str(), trade_symbol];
            let tfs = [
                smt.context_timeframe,
                smt.comparison_timeframe,
                candidate.validation_timeframe,
            ];
            let as_of_ts = super::context::canonical_as_of_ts(&evidence, trade_symbol);
            let bars_per_tf = self.strategy.packer.bars_per_tf();
            for symbol in symbols {
                for (index, tf) in tfs.into_iter().enumerate() {
                    let last_closed_start = as_of_ts.saturating_sub(tf.duration_ms());
                    if let Ok(mut bars) = self.store.recent_bars_at_or_before(
                        symbol,
                        tf,
                        last_closed_start,
                        bars_per_tf[index] as i64,
                    ) {
                        evidence.market_bars.append(&mut bars);
                    }
                    if let Ok(mut structures) =
                        self.store.list_structures_for_as_of(symbol, tf, as_of_ts)
                    {
                        evidence.market_structures.append(&mut structures);
                    }
                }
            }
            evidence
                .market_structures
                .sort_by(|a, b| a.id().cmp(b.id()));
            evidence.market_structures.dedup_by(|a, b| a.id() == b.id());
        }
        // L2 reversal facts are validation-timeframe facts only. The same
        // structure collection also carries HTF/MTF MSS/CISD for the L1
        // facets, so extracting from the complete list would mislabel those
        // higher-timeframe structures as LTF reversals.
        let validation_structures: Vec<_> = evidence
            .market_structures
            .iter()
            .filter(|structure| structure.tf() == candidate.validation_timeframe)
            .cloned()
            .collect();
        evidence.ltf_cisd_mss = crate::candidate::extract_ltf_events(&validation_structures);
        evidence
    }

    fn entry(
        id: String,
        candidate: &CandidateSetup,
        alert: &AlertRecord,
        trade_symbol: String,
        record: DecisionRecord,
    ) -> DecisionLogEntry {
        DecisionLogEntry {
            id,
            candidate_id: candidate.id.clone(),
            watchlist_id: alert.watchlist_id.clone(),
            alert_id: Some(alert.id.clone()),
            trade_symbol: Some(trade_symbol),
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
            created_at: alert.created_at,
        }
    }

    pub async fn dispatch(
        &self,
        candidate: &CandidateSetup,
        alert: &AlertRecord,
        smt: &SmtDivergence,
        seeding: bool,
    ) -> anyhow::Result<DispatchOutcome> {
        if seeding {
            return Ok(DispatchOutcome::SkippedSeeding);
        }
        let trade_symbol = alert
            .validation_symbol
            .clone()
            .or_else(|| alert.trade_symbols.first().cloned())
            .ok_or_else(|| anyhow::anyhow!("C2 alert missing trade symbol"))?;
        let id = Self::decision_id(&alert.watchlist_id, &alert.id, &trade_symbol);
        let Some(_guard) = InFlightGuard::try_acquire(self.in_flight.clone(), id.clone()) else {
            return Ok(DispatchOutcome::Duplicate);
        };
        let evidence = self.evidence(candidate, alert, smt);
        let request_json = self
            .strategy
            .request_json(&evidence)
            .map_err(anyhow::Error::msg)?;
        let pending = Self::entry(
            id.clone(),
            candidate,
            alert,
            trade_symbol.clone(),
            self.strategy.pending_record(request_json.clone()),
        );
        if !self.store.try_insert_decision(&pending)? {
            let recoverable = self.store.get_decision(&id)?.is_some_and(|existing| {
                !existing.parse_ok && existing.raw_response.is_none() && existing.error.is_none()
            });
            if !recoverable {
                return Ok(DispatchOutcome::Duplicate);
            }
            tracing::info!(decision_id = %id, "recovering stale pending LLM decision");
        }

        let record = if self
            .store
            .reserve_llm_daily_call(wall_clock_ms(), self.max_calls_per_day)?
        {
            self.strategy.evaluate(&evidence).await
        } else {
            tracing::warn!(
                decision_id = %id,
                max_calls_per_day = self.max_calls_per_day,
                "LLM daily call limit reached; provider call skipped"
            );
            let mut capped = self.strategy.pending_record(request_json);
            capped.error = Some("daily call limit reached".into());
            capped
        };
        let completed = Self::entry(id.clone(), candidate, alert, trade_symbol, record);
        self.store.update_decision(&completed)?;
        Ok(DispatchOutcome::Stored(id))
    }

    /// Resume only live calls that were durably reserved but interrupted
    /// before a provider result was stored. Evidence is rebuilt from the
    /// persisted alert's historical `created_at`, never from today's latest
    /// bars. This is deliberately separate from detector seeding/replay.
    pub async fn recover_stale_pending(&self) -> anyhow::Result<usize> {
        let pending = self.store.list_pending_llm_decisions()?;
        if pending.is_empty() {
            return Ok(0);
        }
        let candidates = self.store.list_all_candidates_for_inbox()?;
        let alerts = self.store.list_alerts(None)?;
        let mut recovered = 0usize;

        for row in pending {
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.id == row.candidate_id);
            let alert = row.alert_id.as_deref().and_then(|alert_id| {
                alerts
                    .iter()
                    .find(|alert| alert.id == alert_id && alert.watchlist_id == row.watchlist_id)
            });
            let smt = candidate.and_then(|candidate| {
                self.store
                    .list_all_smt(&row.watchlist_id)
                    .ok()?
                    .into_iter()
                    .find(|smt| smt.id == candidate.smt_id)
            });

            match (candidate, alert, smt) {
                (Some(candidate), Some(alert), Some(smt)) => {
                    match self.dispatch(candidate, alert, &smt, false).await {
                        Ok(DispatchOutcome::Stored(_)) => recovered += 1,
                        Ok(DispatchOutcome::Duplicate | DispatchOutcome::SkippedSeeding) => {}
                        Err(error) => tracing::warn!(
                            decision_id = %row.id,
                            ?error,
                            "stale LLM decision recovery failed; reservation remains pending"
                        ),
                    }
                }
                _ => {
                    let mut terminal = row;
                    terminal.error = Some(
                        "stale pending recovery failed: source alert/candidate/SMT unavailable"
                            .into(),
                    );
                    self.store.update_decision(&terminal)?;
                    tracing::warn!(
                        decision_id = %terminal.id,
                        "stale LLM decision closed because source facts are unavailable"
                    );
                }
            }
        }
        Ok(recovered)
    }

    /// Fire-and-forget entry used by the live alert path. Returning the join
    /// handle immediately is the explicit proof that slow providers cannot
    /// delay Alert Inbox persistence or desktop notification.
    pub fn spawn(
        self: Arc<Self>,
        candidate: CandidateSetup,
        alert: AlertRecord,
        smt: SmtDivergence,
        seeding: bool,
    ) -> tokio::task::JoinHandle<anyhow::Result<DispatchOutcome>> {
        tokio::spawn(async move { self.dispatch(&candidate, &alert, &smt, seeding).await })
    }
}

struct InFlightGuard {
    set: Arc<parking_lot::Mutex<HashSet<String>>>,
    id: String,
}

impl InFlightGuard {
    fn try_acquire(set: Arc<parking_lot::Mutex<HashSet<String>>>, id: String) -> Option<Self> {
        if !set.lock().insert(id.clone()) {
            return None;
        }
        Some(Self { set, id })
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.set.lock().remove(&self.id);
    }
}

fn wall_clock_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
