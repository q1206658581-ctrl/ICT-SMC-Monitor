use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use chrono::{TimeZone, Utc};
use serde::Serialize;

use crate::alert::{build_c2_alert, ChannelKind};
use crate::candidate::{CandidateSetup, DecisionStatus, SetupStatus, SetupType};
use crate::detector::types::{
    CandleRef, Correlation, Direction, ReferenceScope, SmtDetectionState, SmtDivergence,
    SymbolChain,
};
use crate::storage::SqliteStore;
use crate::types::{Bar, Timeframe};

use super::{
    ContextPacker, DispatchOutcome, LlmDecision, LlmDecisionPipeline, LlmProvider,
    LlmSingleCallStrategy, OpenAiCompatibleOptions, OpenAiCompatibleProvider, StructuredOutputMode,
};

const WATCHLIST_ID: &str = "m7c-smoke-eu-gu";
const CANDIDATE_ID: &str = "m7c-smoke-candidate-v1";
const DXY: &str = "TVC:DXY";
const EURUSD: &str = "OANDA:EURUSD";

/// Secret-bearing provider settings for the isolated M7c smoke command.
/// Deliberately does not implement `Debug` or `Serialize`.
#[derive(Clone)]
pub struct ProviderSmokeConfig {
    pub provider_name: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub response_format: StructuredOutputMode,
    pub timeout: Duration,
    pub max_retries: usize,
    pub max_output_tokens: u32,
    pub proxy_url: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderSmokeReport {
    pub status: &'static str,
    pub provider: String,
    pub model: String,
    pub decision_id: String,
    pub parse_ok: bool,
    pub idempotent: bool,
    pub decision_rows: usize,
    pub closed_bar_series: usize,
    pub populated_bar_series: usize,
    pub three_table_rows_unchanged: bool,
    pub temporary_database_removed: bool,
    pub decision: LlmDecision,
}

/// Run one real provider call against a deterministic historical fixture.
///
/// The function creates a unique SQLite database under the OS temp directory,
/// calls the same M7c pipeline used by production, verifies the durable row and
/// duplicate dispatch behavior, then removes the database. It never opens the
/// configured production database and never invokes the alert engine or any
/// notification channel.
pub async fn run_provider_smoke(config: ProviderSmokeConfig) -> Result<ProviderSmokeReport> {
    if config.api_key.trim().is_empty() {
        return Err(anyhow!("smoke API key is empty"));
    }
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleOptions {
        base_url: config.base_url.clone(),
        api_key: config.api_key.clone(),
        model: config.model.clone(),
        temperature: 0.0,
        timeout: config.timeout,
        max_retries: config.max_retries,
        max_output_tokens: config.max_output_tokens,
        proxy_url: config.proxy_url.clone(),
        structured_output: config.response_format,
        reasoning_effort: None,
    })?;
    run_smoke_with_provider(Arc::new(provider), config.provider_name, config.model).await
}

async fn run_smoke_with_provider(
    provider: Arc<dyn LlmProvider>,
    provider_name: String,
    model: String,
) -> Result<ProviderSmokeReport> {
    let temp_db = TempDatabase::new()?;
    let store = SqliteStore::open(temp_db.path()).context("opening isolated smoke database")?;
    store.ensure_detector_config_schema()?;
    store.ensure_ict_schema()?;
    store.ensure_smt_schema()?;
    store.ensure_candidate_schema()?;
    store.ensure_alert_schema()?;

    let (candidate, smt, alert, bars) = fixture();
    store.insert_bars(&bars)?;
    let strategy = Arc::new(
        LlmSingleCallStrategy::new(provider, provider_name.clone(), Some(model.clone()))
            .with_packer(ContextPacker::new([8, 8, 12])),
    );
    let pipeline = LlmDecisionPipeline::new(store.clone(), strategy).with_max_calls_per_day(1);

    let decision_id = match pipeline.dispatch(&candidate, &alert, &smt, false).await? {
        DispatchOutcome::Stored(id) => id,
        other => return Err(anyhow!("first smoke dispatch was not stored: {other:?}")),
    };
    let idempotent = matches!(
        pipeline.dispatch(&candidate, &alert, &smt, false).await?,
        DispatchOutcome::Duplicate
    );
    if !idempotent {
        return Err(anyhow!("duplicate smoke dispatch was not suppressed"));
    }

    let rows = store.list_decisions(CANDIDATE_ID)?;
    if rows.len() != 1 {
        return Err(anyhow!(
            "expected exactly one decision row, found {}",
            rows.len()
        ));
    }
    let row = &rows[0];
    if !row.parse_ok {
        return Err(anyhow!(
            "provider response failed M7 schema validation: {}",
            row.error.as_deref().unwrap_or("unknown parse error")
        ));
    }
    let decision: LlmDecision =
        serde_json::from_str(&row.parsed_decision_json).context("reading parsed smoke decision")?;
    let request: serde_json::Value =
        serde_json::from_str(&row.request_json).context("reading persisted smoke request")?;
    let series = request["closed_bars"]
        .as_array()
        .ok_or_else(|| anyhow!("persisted smoke request has no closed_bars array"))?;
    let populated_bar_series = series
        .iter()
        .filter(|item| item["bars"].as_array().is_some_and(|bars| !bars.is_empty()))
        .count();
    if populated_bar_series != series.len() || series.len() != 6 {
        return Err(anyhow!(
            "historical context is incomplete: {populated_bar_series}/{} populated series",
            series.len()
        ));
    }

    let three_table_rows_unchanged = store.list_all_smt(WATCHLIST_ID)?.is_empty()
        && store.list_all_candidates_for_inbox()?.is_empty()
        && store.list_alerts(None)?.is_empty();
    if !three_table_rows_unchanged {
        return Err(anyhow!("smoke call unexpectedly wrote a three-table fact"));
    }

    let mut report = ProviderSmokeReport {
        status: "PASS",
        provider: provider_name,
        model,
        decision_id,
        parse_ok: true,
        idempotent,
        decision_rows: rows.len(),
        closed_bar_series: series.len(),
        populated_bar_series,
        three_table_rows_unchanged,
        temporary_database_removed: false,
        decision,
    };
    drop(pipeline);
    drop(store);
    report.temporary_database_removed = temp_db.cleanup();
    if !report.temporary_database_removed {
        return Err(anyhow!(
            "temporary smoke database could not be fully removed"
        ));
    }
    Ok(report)
}

fn fixture() -> (
    CandidateSetup,
    SmtDivergence,
    crate::alert::AlertRecord,
    Vec<Bar>,
) {
    let base = Utc
        .with_ymd_and_hms(2026, 8, 15, 1, 0, 0)
        .single()
        .expect("fixed smoke timestamp")
        .timestamp_millis();
    let mtf_ms = Timeframe::M30.duration_ms();
    let c1_ts = Timeframe::M30.boundary_align(base);
    let smt_k_ts = c1_ts + mtf_ms;
    let c2_ts = smt_k_ts + mtf_ms;
    let eligible_at = c2_ts + mtf_ms;

    let dxy_chain = chain(DXY, c1_ts, smt_k_ts, c2_ts, 99.60);
    let eur_chain = chain(EURUSD, c1_ts, smt_k_ts, c2_ts, 1.1000);
    let smt = SmtDivergence {
        id: "m7c-smoke-smt-v1".into(),
        watchlist_id: WATCHLIST_ID.into(),
        rule_version: crate::detector::smt::CURRENT_RULE_VERSION.into(),
        symbol_set: vec![DXY.into(), EURUSD.into()],
        relationship: Correlation::Negative,
        context_timeframe: Timeframe::H4,
        comparison_timeframe: Timeframe::M30,
        observation_window: (c1_ts, smt_k_ts + mtf_ms),
        htf_confirmed: true,
        reference_scope: ReferenceScope::DistantLeftSide,
        liquidity_refs: vec![],
        confluence_refs: vec![],
        candidate_direction: Direction::Bullish,
        sweeper_symbol: DXY.into(),
        trade_symbols: vec![EURUSD.into()],
        strength: vec![],
        chains: vec![dxy_chain.clone(), eur_chain],
        invalidation_reasons: vec![],
        invalidation_ts: None,
        htf_pda_ref: None,
        mtf_ref_candle: None,
    };
    let candidate = CandidateSetup {
        id: CANDIDATE_ID.into(),
        rule_version: "m7c.smoke.v1".into(),
        watchlist_id: WATCHLIST_ID.into(),
        smt_id: smt.id.clone(),
        setup_type: SetupType::Smt,
        symbol_set: smt.symbol_set.clone(),
        sweeper_symbol: DXY.into(),
        trade_symbols: vec![EURUSD.into()],
        candidate_direction: Direction::Bullish,
        context_timeframe: Timeframe::H4,
        comparison_timeframe: Timeframe::M30,
        validation_timeframe: Timeframe::M5,
        context_pda_id: None,
        observation_window: smt.observation_window,
        c1_candle: dxy_chain.c1_candle.clone(),
        smt_k_candle: dxy_chain.smt_k_candle.clone(),
        c2_candle: dxy_chain.c2_candle.clone().expect("smoke C2"),
        c2_case: 1,
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
        deterministic_score: 0.65,
        created_at: eligible_at,
        validated_at: None,
        expired_at: None,
        invalidated_at: None,
        expiry_reason: None,
        strength: vec![],
        expiry_at: None,
        smt_rule_version: crate::detector::smt::CURRENT_RULE_VERSION.into(),
    };
    let alert = build_c2_alert(
        &candidate,
        &smt,
        EURUSD,
        eligible_at,
        vec![ChannelKind::Inbox],
    )
    .expect("fixed smoke alert");
    let bars = historical_bars(eligible_at);
    (candidate, smt, alert, bars)
}

fn chain(symbol: &str, c1_ts: i64, smt_k_ts: i64, c2_ts: i64, base: f64) -> SymbolChain {
    SymbolChain {
        symbol: symbol.into(),
        c1_candle: candle(c1_ts, base, 0),
        smt_k_candle: candle(smt_k_ts, base, 1),
        c2_candle: Some(candle(c2_ts, base, 2)),
        c2_case: Some(1),
        c3_candle: None,
        detection_state: SmtDetectionState::C2Confirmed,
    }
}

fn candle(ts: i64, base: f64, step: i64) -> CandleRef {
    let unit = if base > 10.0 { 0.025 } else { 0.00025 };
    let open = base + step as f64 * unit;
    CandleRef {
        ts,
        open,
        high: open + unit * 1.5,
        low: open - unit,
        close: open + unit * 0.5,
    }
}

fn historical_bars(as_of_ts: i64) -> Vec<Bar> {
    let mut out = Vec::new();
    for (symbol, base) in [(DXY, 99.60), (EURUSD, 1.1000)] {
        for tf in [Timeframe::H4, Timeframe::M30, Timeframe::M5] {
            let duration = tf.duration_ms();
            let last = tf.boundary_align(as_of_ts.saturating_sub(duration));
            for index in 0..12_i64 {
                let ts = last.saturating_sub((11 - index) * duration);
                let unit = if symbol == DXY { 0.015 } else { 0.00015 };
                let wave = match index % 4 {
                    0 => -1.0,
                    1 => 0.5,
                    2 => 1.0,
                    _ => -0.25,
                };
                let open = base + index as f64 * unit * 0.2;
                let close = open + wave * unit;
                out.push(Bar {
                    symbol: symbol.into(),
                    tf,
                    ts,
                    open,
                    high: open.max(close) + unit,
                    low: open.min(close) - unit,
                    close,
                    volume: 100.0 + index as f64,
                });
            }
        }
    }
    out
}

struct TempDatabase {
    path: PathBuf,
    cleaned: bool,
}

impl TempDatabase {
    fn new() -> Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock before UNIX epoch")?
            .as_nanos();
        Ok(Self {
            path: std::env::temp_dir().join(format!(
                "ict-monitor-m7c-provider-smoke-{}-{nonce}.db",
                std::process::id()
            )),
            cleaned: false,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn cleanup(mut self) -> bool {
        let removed = remove_sqlite_files(&self.path);
        self.cleaned = true;
        removed
    }
}

impl Drop for TempDatabase {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = remove_sqlite_files(&self.path);
        }
    }
}

fn remove_sqlite_files(path: &Path) -> bool {
    let mut ok = true;
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        if candidate.exists() && std::fs::remove_file(&candidate).is_err() {
            ok = false;
        }
    }
    ok && !path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{MockLlmProvider, MockProviderReply};

    #[tokio::test]
    async fn isolated_smoke_verifies_parse_persistence_and_idempotency() {
        let response = serde_json::json!({
            "candidate_id": CANDIDATE_ID,
            "alert": false,
            "direction": "bearish",
            "confidence": 64,
            "quality": "B",
            "reasoning_summary": "固定历史上下文可解析",
            "evidence_structure_ids": ["m7c-smoke-smt-v1"],
            "invalidation_price": null,
            "entry_zone": null,
            "targets": [],
            "risk_reward": null,
            "should_wait_for": ["等待更多入场区证据"],
            "warnings": ["仅用于 M7c 冒烟测试"]
        })
        .to_string();
        let provider = MockLlmProvider::new(MockProviderReply::Json(response));
        let calls = provider.clone();
        let report =
            run_smoke_with_provider(Arc::new(provider), "mock-smoke".into(), "mock-fixed".into())
                .await
                .expect("isolated smoke");

        assert_eq!(report.status, "PASS");
        assert!(report.parse_ok);
        assert!(report.idempotent);
        assert_eq!(report.decision_rows, 1);
        assert_eq!(report.closed_bar_series, 6);
        assert_eq!(report.populated_bar_series, 6);
        assert!(report.three_table_rows_unchanged);
        assert!(report.temporary_database_removed);
        assert_eq!(calls.calls(), 1);
    }
}
