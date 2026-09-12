use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::alert::{build_c2_alert, AlertRecord, ChannelKind};
use crate::candidate::{CandidateSetup, DecisionMode, DecisionStatus, SetupStatus, SetupType};
use crate::detector::types::{
    CandleRef, Correlation, Direction, Fvg, FvgState, IctStructure, LiquidityRef,
    LiquidityRefStatus, LiquiditySide, Mss, ReferenceScope, SmtDetectionState, SmtDivergence,
    SymbolChain,
};
use crate::storage::SqliteStore;
use crate::types::{Bar, Timeframe};

use super::*;

fn candle(ts: i64) -> CandleRef {
    CandleRef {
        ts,
        open: 100.0,
        high: 101.0,
        low: 99.0,
        close: 100.5,
    }
}

fn fixture(watchlist_id: &str, candidate_id: &str) -> (CandidateSetup, SmtDivergence, AlertRecord) {
    let chain = |symbol: &str, c2_ts: i64| SymbolChain {
        symbol: symbol.into(),
        c1_candle: candle(1_000),
        smt_k_candle: candle(2_000),
        c2_candle: Some(candle(c2_ts)),
        c2_case: Some(1),
        c3_candle: Some(candle(c2_ts + 1_800_000)),
        detection_state: SmtDetectionState::C3Entry,
    };
    let smt = SmtDivergence {
        id: format!("smt-{candidate_id}"),
        watchlist_id: watchlist_id.into(),
        rule_version: crate::detector::smt::CURRENT_RULE_VERSION.into(),
        symbol_set: vec!["TVC:DXY".into(), "OANDA:EURUSD".into()],
        relationship: Correlation::Negative,
        context_timeframe: Timeframe::H4,
        comparison_timeframe: Timeframe::M30,
        observation_window: (500, 9_000),
        htf_confirmed: true,
        reference_scope: ReferenceScope::DistantLeftSide,
        liquidity_refs: vec![LiquidityRef {
            symbol: "OANDA:EURUSD".into(),
            ref_price: 98.0,
            ref_ts: 1_500,
            side: LiquiditySide::SellSide,
            status: LiquidityRefStatus::NotSwept,
            tf: Timeframe::H4,
            mtf_ref_candle: None,
            mtf_sweep_candle: None,
        }],
        confluence_refs: vec![],
        candidate_direction: Direction::Bullish,
        sweeper_symbol: "TVC:DXY".into(),
        trade_symbols: vec!["OANDA:EURUSD".into()],
        strength: vec![],
        chains: vec![chain("TVC:DXY", 3_000), chain("OANDA:EURUSD", 4_000)],
        invalidation_reasons: vec![],
        invalidation_ts: None,
        htf_pda_ref: None,
        mtf_ref_candle: None,
    };
    let candidate = CandidateSetup {
        id: candidate_id.into(),
        rule_version: "m6a.v1".into(),
        watchlist_id: watchlist_id.into(),
        smt_id: smt.id.clone(),
        setup_type: SetupType::Smt,
        symbol_set: smt.symbol_set.clone(),
        sweeper_symbol: smt.sweeper_symbol.clone(),
        trade_symbols: smt.trade_symbols.clone(),
        candidate_direction: Direction::Bullish,
        context_timeframe: Timeframe::H4,
        comparison_timeframe: Timeframe::M30,
        validation_timeframe: Timeframe::M5,
        context_pda_id: Some("pda-test".into()),
        observation_window: smt.observation_window,
        c1_candle: candle(1_000),
        smt_k_candle: candle(2_000),
        c2_candle: candle(3_000),
        c2_case: 1,
        c3_candle: Some(candle(4_800_000)),
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
        deterministic_score: 0.6,
        created_at: 4_000,
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
        "OANDA:EURUSD",
        4_000 + Timeframe::M30.duration_ms(),
        vec![ChannelKind::Inbox],
    )
    .expect("C2 alert fixture");
    (candidate, smt, alert)
}

fn valid_json(candidate_id: &str) -> String {
    let smt_id = format!("smt-{candidate_id}");
    serde_json::json!({
        "candidate_id": candidate_id,
        "alert": true,
        "direction": "bullish",
        "confidence": 81,
        "quality": "A",
        "reasoning_summary": "结构与流动性条件一致",
        "evidence_structure_ids": [smt_id],
        "invalidation_price": 98.5,
        "entry_zone": {"low": 99.0, "high": 99.5, "source": "C2"},
        "targets": [{"price": 102.0, "reason": "上方流动性"}],
        "risk_reward": 2.0,
        "should_wait_for": [],
        "warnings": ["仅作策略建议"]
    })
    .to_string()
}

fn test_store(name: &str) -> (SqliteStore, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "ict-monitor-m7a-{name}-{}.db",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = SqliteStore::open(&path).expect("open store");
    store
        .ensure_detector_config_schema()
        .expect("detector config schema");
    store.ensure_ict_schema().expect("ict structure schema");
    store.ensure_smt_schema().expect("smt schema");
    store.ensure_candidate_schema().expect("candidate schema");
    store.ensure_alert_schema().expect("alert schema");
    (store, path)
}

#[test]
fn legacy_decision_log_migrates_before_new_indexes_are_created() {
    let path = std::env::temp_dir().join(format!(
        "ict-monitor-m7a-legacy-migration-{}.db",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let conn = rusqlite::Connection::open(&path).expect("open legacy db");
        conn.execute_batch(
            "CREATE TABLE decision_log (
                id TEXT PRIMARY KEY,
                candidate_id TEXT NOT NULL,
                parent_id TEXT,
                provider TEXT NOT NULL,
                model TEXT,
                decision_mode TEXT NOT NULL,
                prompt_version TEXT,
                strategy_version TEXT NOT NULL,
                context_version TEXT NOT NULL,
                request_json TEXT NOT NULL,
                raw_response TEXT,
                parsed_decision_json TEXT NOT NULL,
                parse_ok INTEGER NOT NULL,
                error TEXT,
                created_at INTEGER NOT NULL
            );",
        )
        .expect("legacy decision schema");
    }

    let store = SqliteStore::open(&path).expect("open migrated store");
    store
        .ensure_detector_config_schema()
        .expect("detector config schema");
    store
        .ensure_candidate_schema()
        .expect("legacy decision migration");

    let conn = rusqlite::Connection::open(&path).expect("inspect migrated db");
    for column in ["watchlist_id", "alert_id", "trade_symbol"] {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('decision_log') WHERE name = ?1",
                [column],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "missing migrated column {column}");
    }
    let indexes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index'
               AND name IN ('idx_decision_alert', 'idx_decision_watchlist_created')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(indexes, 2);
    drop(conn);
    drop(store);
    let _ = std::fs::remove_file(path);
}

fn pipeline(store: SqliteStore, provider: MockLlmProvider) -> Arc<LlmDecisionPipeline> {
    Arc::new(LlmDecisionPipeline::new(
        store,
        Arc::new(LlmSingleCallStrategy::new(
            Arc::new(provider),
            "mock",
            Some("mock-fixed".into()),
        )),
    ))
}

#[tokio::test]
async fn valid_mock_decision_is_complete_and_idempotent() {
    let (store, path) = test_store("idempotent");
    let (candidate, smt, alert) = fixture("eu-gu", "candidate-1");
    let provider = MockLlmProvider::new(MockProviderReply::Json(valid_json(&candidate.id)));
    let calls = provider.clone();
    let pipeline = pipeline(store.clone(), provider);

    let first = pipeline
        .dispatch(&candidate, &alert, &smt, false)
        .await
        .expect("first dispatch");
    assert!(matches!(first, DispatchOutcome::Stored(_)));
    assert_eq!(
        pipeline
            .dispatch(&candidate, &alert, &smt, false)
            .await
            .expect("duplicate dispatch"),
        DispatchOutcome::Duplicate
    );
    assert_eq!(calls.calls(), 1, "duplicate must not call provider");

    let rows = store.list_decisions(&candidate.id).expect("decision rows");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.watchlist_id, "eu-gu");
    assert_eq!(row.alert_id.as_deref(), Some(alert.id.as_str()));
    assert_eq!(row.trade_symbol.as_deref(), Some("OANDA:EURUSD"));
    assert_eq!(row.decision_mode, DecisionMode::LlmSingleCall);
    assert_eq!(row.prompt_version.as_deref(), Some(M7D_PROMPT_VERSION));
    assert!(row.parse_ok);
    assert!(row.error.is_none());
    assert!(row.raw_response.is_some());
    let request: serde_json::Value = serde_json::from_str(&row.request_json).unwrap();
    assert_eq!(request["identity"]["candidate_id"], candidate.id);
    assert!(request.get("evidence").is_none());
    assert!(request.get("facets").is_some());
    let parsed: LlmDecision = serde_json::from_str(&row.parsed_decision_json).unwrap();
    assert_eq!(parsed.candidate_id, candidate.id);
    assert_eq!(parsed.direction, LlmDecisionDirection::Bearish);
    assert_eq!(parsed.confidence, 60);
    assert_eq!(parsed.quality, LlmDecisionQuality::C);
    assert_eq!(parsed.entry_zone.as_ref().map(|zone| zone.low), Some(100.0));
    assert_eq!(
        parsed.entry_zone.as_ref().map(|zone| zone.high),
        Some(100.5)
    );
    assert_eq!(parsed.invalidation_price, Some(101.0));
    assert_eq!(
        parsed.targets.first().map(|target| target.price),
        Some(98.0)
    );
    assert_eq!(parsed.risk_reward, Some(3.0));
    assert!(parsed.alert);
    assert!(parsed
        .warnings
        .iter()
        .any(|warning| warning.contains("本地确定性决策规则归一化")));
    assert_eq!(
        request["deterministic_guardrails"]["rule_version"],
        M7D_STRATEGY_VERSION
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn m7d_guardrails_reject_unknown_evidence_and_model_promotion() {
    let (mut candidate, smt, alert) = fixture("eu-gu", "candidate-guardrails");
    candidate.deterministic_score = 0.49;
    let context = ContextPacker::default()
        .pack(&packed_fixture(&candidate, &smt, &alert))
        .expect("pack guarded context");
    assert_eq!(context.deterministic_guardrails.confidence, 49);
    assert_eq!(
        context.deterministic_guardrails.quality,
        LlmDecisionQuality::Skip
    );
    assert!(!context.deterministic_guardrails.alert_eligible);

    let mut response: LlmDecision =
        serde_json::from_str(&valid_json(&candidate.id)).expect("valid model JSON");
    response.evidence_structure_ids = vec!["invented-evidence".into()];
    assert!(response
        .apply_guardrails(&context)
        .unwrap_err()
        .contains("unknown evidence_structure_id"));

    let response: LlmDecision =
        serde_json::from_str(&valid_json(&candidate.id)).expect("valid model JSON");
    let normalized = response
        .apply_guardrails(&context)
        .expect("normalize response");
    assert!(
        !normalized.alert,
        "LLM must not promote an ineligible setup"
    );
    assert_eq!(normalized.confidence, 49);
    assert_eq!(normalized.quality, LlmDecisionQuality::Skip);
}

#[test]
fn m7d_guardrails_bound_model_text_before_persistence() {
    let (candidate, smt, alert) = fixture("eu-gu", "candidate-output-bounds");
    let mut context = ContextPacker::default()
        .pack(&packed_fixture(&candidate, &smt, &alert))
        .expect("pack guarded context");
    context.deterministic_guardrails.allowed_evidence_ids = (0..80)
        .map(|index| format!("evidence-{index:03}"))
        .collect();
    context.deterministic_guardrails.required_evidence_ids = vec!["evidence-079".into()];
    context.deterministic_guardrails.should_wait_for = vec!["确定性等待条件".into()];
    context.deterministic_guardrails.warnings = vec!["确定性风险提示".into()];

    let mut response: LlmDecision =
        serde_json::from_str(&valid_json(&candidate.id)).expect("valid model JSON");
    response.reasoning_summary = "推".repeat(MAX_REASONING_SUMMARY_CHARS + 200);
    response.evidence_structure_ids = (0..80)
        .map(|index| format!("evidence-{index:03}"))
        .collect();
    response.should_wait_for = (0..30)
        .map(|index| format!("模型等待-{index:02}-{}", "长".repeat(300)))
        .collect();
    response.warnings = (0..30)
        .map(|index| format!("模型风险-{index:02}-{}", "长".repeat(300)))
        .collect();

    let normalized = response
        .apply_guardrails(&context)
        .expect("normalize bounded response");
    assert!(normalized.reasoning_summary.chars().count() <= MAX_REASONING_SUMMARY_CHARS);
    assert!(normalized.evidence_structure_ids.len() <= MAX_EVIDENCE_STRUCTURE_IDS);
    assert!(normalized
        .evidence_structure_ids
        .iter()
        .any(|id| id == "evidence-079"));
    assert!(normalized.should_wait_for.len() <= MAX_FINAL_ADVISORY_ITEMS);
    assert!(normalized.warnings.len() <= MAX_FINAL_ADVISORY_ITEMS);
    assert_eq!(
        normalized.should_wait_for.first().map(String::as_str),
        Some("确定性等待条件")
    );
    assert_eq!(
        normalized.warnings.first().map(String::as_str),
        Some("确定性风险提示")
    );
    assert!(normalized
        .should_wait_for
        .iter()
        .all(|item| item.chars().count() <= MAX_ADVISORY_TEXT_CHARS));
    assert!(normalized
        .warnings
        .iter()
        .all(|item| item.chars().count() <= MAX_ADVISORY_TEXT_CHARS));
}

#[test]
fn m7d_guardrails_reject_unknown_evidence_beyond_storage_cap() {
    let (candidate, smt, alert) = fixture("eu-gu", "candidate-unknown-after-cap");
    let mut context = ContextPacker::default()
        .pack(&packed_fixture(&candidate, &smt, &alert))
        .expect("pack guarded context");
    context.deterministic_guardrails.allowed_evidence_ids = (0..80)
        .map(|index| format!("evidence-{index:03}"))
        .collect();
    context
        .deterministic_guardrails
        .required_evidence_ids
        .clear();

    let mut response: LlmDecision =
        serde_json::from_str(&valid_json(&candidate.id)).expect("valid model JSON");
    response.alert = false;
    response.evidence_structure_ids = context
        .deterministic_guardrails
        .allowed_evidence_ids
        .clone();
    response
        .evidence_structure_ids
        .push("invented-after-storage-cap".into());

    let error = response
        .apply_guardrails(&context)
        .expect_err("unknown evidence must be checked before truncation");
    assert!(error.contains("invented-after-storage-cap"));
}

#[tokio::test]
async fn pipeline_ltf_reversals_exclude_cross_tf_and_pre_c2_events() {
    let (store, path) = test_store("ltf-window");
    let (candidate, smt, alert) = fixture("eu-gu", "candidate-ltf-window");
    let structures = [
        IctStructure::Mss(Mss {
            id: "mss-ltf-in-c2".into(),
            symbol: "OANDA:EURUSD".into(),
            tf: Timeframe::M5,
            direction: Direction::Bullish,
            break_ts: 5_000,
            break_price: 1.12,
            swing_ts: 4_000,
            swing_price: 1.11,
        }),
        IctStructure::Mss(Mss {
            id: "mss-ltf-before-c2".into(),
            symbol: "OANDA:EURUSD".into(),
            tf: Timeframe::M5,
            direction: Direction::Bullish,
            break_ts: 3_000,
            break_price: 1.12,
            swing_ts: 2_000,
            swing_price: 1.11,
        }),
        IctStructure::Mss(Mss {
            id: "mss-mtf-in-c2".into(),
            symbol: "OANDA:EURUSD".into(),
            tf: Timeframe::M30,
            direction: Direction::Bullish,
            break_ts: 6_000,
            break_price: 1.12,
            swing_ts: 4_000,
            swing_price: 1.11,
        }),
        IctStructure::Mss(Mss {
            id: "mss-other-symbol".into(),
            symbol: "TVC:DXY".into(),
            tf: Timeframe::M5,
            direction: Direction::Bullish,
            break_ts: 5_000,
            break_price: 100.0,
            swing_ts: 4_000,
            swing_price: 99.0,
        }),
    ];
    for structure in &structures {
        store
            .upsert_structure(structure, 10_000)
            .expect("persist test structure");
    }

    let provider = MockLlmProvider::new(MockProviderReply::Json(valid_json(&candidate.id)));
    let pipeline = pipeline(store.clone(), provider);
    pipeline
        .dispatch(&candidate, &alert, &smt, false)
        .await
        .expect("dispatch");

    let row = store.list_decisions(&candidate.id).unwrap().remove(0);
    let request: serde_json::Value = serde_json::from_str(&row.request_json).unwrap();
    let reversals = request["l2_strategy_reading"]["ltf_reversals"]
        .as_array()
        .expect("ltf_reversals array");
    assert_eq!(reversals.len(), 1);
    assert_eq!(reversals[0]["id"], "mss-ltf-in-c2");
    assert!(
        row.request_json.contains("mss-mtf-in-c2"),
        "MTF MSS remains available as L1 evidence"
    );
    assert!(!reversals
        .iter()
        .any(|event| event["id"] == "mss-ltf-before-c2"));
    assert!(!reversals.iter().any(|event| event["id"] == "mss-mtf-in-c2"));
    assert!(!reversals
        .iter()
        .any(|event| event["id"] == "mss-other-symbol"));

    drop(pipeline);
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn seeding_never_calls_provider_or_writes_decision() {
    let (store, path) = test_store("seeding");
    let (candidate, smt, alert) = fixture("eu-gu", "candidate-seed");
    let provider = MockLlmProvider::new(MockProviderReply::Json(valid_json(&candidate.id)));
    let calls = provider.clone();
    let pipeline = pipeline(store.clone(), provider);
    assert_eq!(
        pipeline
            .dispatch(&candidate, &alert, &smt, true)
            .await
            .unwrap(),
        DispatchOutcome::SkippedSeeding
    );
    assert_eq!(calls.calls(), 0);
    assert!(store.list_decisions(&candidate.id).unwrap().is_empty());
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn parse_failure_is_audited_without_touching_alert_fact() {
    let (store, path) = test_store("parse-failure");
    let (candidate, smt, alert) = fixture("eu-gu", "candidate-bad");
    let alert_before = serde_json::to_string(&alert).unwrap();
    let provider = MockLlmProvider::new(MockProviderReply::Json("not-json".into()));
    let pipeline = pipeline(store.clone(), provider);
    pipeline
        .dispatch(&candidate, &alert, &smt, false)
        .await
        .expect("dispatch remains durable");
    let row = store.list_decisions(&candidate.id).unwrap().remove(0);
    assert!(!row.parse_ok);
    assert!(row.error.as_deref().unwrap().contains("parse/validate"));
    assert_eq!(alert_before, serde_json::to_string(&alert).unwrap());
    let alert_json = serde_json::to_string(&alert).unwrap();
    for llm_only in [
        "reasoning_summary",
        "entry_zone",
        "targets",
        "risk_reward",
        "should_wait_for",
        "warnings",
    ] {
        assert!(!alert_json.contains(llm_only));
    }
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn schema_range_failure_is_audited_without_provider_side_effects() {
    let (store, path) = test_store("range-failure");
    let (candidate, smt, alert) = fixture("eu-gu", "candidate-range");
    let response = valid_json(&candidate.id).replace("\"confidence\":81", "\"confidence\":101");
    let provider = MockLlmProvider::new(MockProviderReply::Json(response));
    let pipeline = pipeline(store.clone(), provider);

    pipeline
        .dispatch(&candidate, &alert, &smt, false)
        .await
        .expect("invalid schema remains auditable");

    let row = store.list_decisions(&candidate.id).unwrap().remove(0);
    assert!(!row.parse_ok);
    assert!(row
        .error
        .as_deref()
        .unwrap()
        .contains("confidence must be between 0 and 100"));
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn slow_provider_runs_as_sidecar_and_group_ids_are_isolated() {
    let (store, path) = test_store("slow-groups");
    let (candidate_a, smt_a, alert_a) = fixture("eu-gu", "candidate-a");
    let (candidate_b, smt_b, alert_b) = fixture("aud-nzd", "candidate-b");
    let provider = MockLlmProvider::new(MockProviderReply::Json(valid_json(&candidate_a.id)))
        .with_delay(Duration::from_millis(120));
    let calls = provider.clone();
    let pipeline_a = pipeline(store.clone(), provider);

    let started = Instant::now();
    let handle =
        pipeline_a
            .clone()
            .spawn(candidate_a.clone(), alert_a.clone(), smt_a.clone(), false);
    assert!(
        started.elapsed() < Duration::from_millis(40),
        "spawning the sidecar must not await provider latency"
    );
    assert_eq!(
        handle.await.expect("join").expect("dispatch"),
        DispatchOutcome::Stored(LlmDecisionPipeline::decision_id(
            &alert_a.watchlist_id,
            &alert_a.id,
            "OANDA:EURUSD"
        ))
    );

    // A second group with an otherwise equivalent alert has an independent
    // deterministic key. Use a matching response for its candidate.
    let provider_b = MockLlmProvider::new(MockProviderReply::Json(valid_json(&candidate_b.id)));
    let calls_b = provider_b.clone();
    let pipeline_b = pipeline(store.clone(), provider_b);
    assert!(matches!(
        pipeline_b
            .dispatch(&candidate_b, &alert_b, &smt_b, false)
            .await
            .unwrap(),
        DispatchOutcome::Stored(_)
    ));
    assert_eq!(calls.calls(), 1);
    assert_eq!(calls_b.calls(), 1);
    assert_eq!(store.list_decisions(&candidate_a.id).unwrap().len(), 1);
    assert_eq!(store.list_decisions(&candidate_b.id).unwrap().len(), 1);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn provider_error_is_persisted_as_parse_failure() {
    let (store, path) = test_store("provider-error");
    let (candidate, smt, alert) = fixture("chf-cad", "candidate-error");
    let provider = MockLlmProvider::new(MockProviderReply::Error("mock unavailable".into()));
    let calls = provider.clone();
    let pipeline = pipeline(store.clone(), provider);
    pipeline
        .dispatch(&candidate, &alert, &smt, false)
        .await
        .unwrap();
    let row = store.list_decisions(&candidate.id).unwrap().remove(0);
    assert!(!row.parse_ok);
    assert!(row.error.as_deref().unwrap().contains("mock unavailable"));
    assert_eq!(pipeline.recover_stale_pending().await.unwrap(), 0);
    assert_eq!(
        calls.calls(),
        1,
        "a completed provider error is terminal and must not be retried"
    );
    let _ = std::fs::remove_file(path);
}

fn packed_fixture(
    candidate: &CandidateSetup,
    smt: &SmtDivergence,
    alert: &AlertRecord,
) -> crate::candidate::PackedEvidence {
    crate::candidate::PackedEvidence {
        candidate_id: candidate.id.clone(),
        candidate: candidate.clone(),
        watchlist_id: alert.watchlist_id.clone(),
        alert_id: Some(alert.id.clone()),
        trade_symbol: alert.validation_symbol.clone(),
        as_of_ts: alert.created_at,
        smt: smt.clone(),
        ltf_cisd_mss: Vec::new(),
        pda_context: smt.htf_pda_ref.clone(),
        liquidity_refs: smt.liquidity_refs.clone(),
        strength: smt.strength.clone(),
        context_version: M7D_CONTEXT_VERSION.into(),
        strategy_version: M7D_STRATEGY_VERSION.into(),
        market_bars: Vec::new(),
        market_structures: Vec::new(),
    }
}

fn bar(symbol: &str, tf: Timeframe, ts: i64, price: f64) -> Bar {
    Bar {
        symbol: symbol.into(),
        tf,
        ts,
        open: price + 0.000_009,
        high: price + 0.000_019,
        low: price - 0.000_019,
        close: price + 0.000_001,
        volume: 123.456,
    }
}

#[test]
fn m7b_context_packer_is_pure_and_excludes_future_facts() {
    let (candidate, smt, alert) = fixture("eu-gu", "candidate-context");
    let mut evidence = packed_fixture(&candidate, &smt, &alert);
    let as_of = 4_000 + Timeframe::M30.duration_ms();
    evidence.market_bars = vec![
        bar(
            "OANDA:EURUSD",
            Timeframe::M5,
            as_of - Timeframe::M5.duration_ms(),
            1.123_456_789,
        ),
        // Starts at as_of, so it is still open and must never reach context.
        bar("OANDA:EURUSD", Timeframe::M5, as_of, 9.999_999),
    ];
    evidence.market_structures = vec![
        IctStructure::Mss(Mss {
            id: "mss-visible".into(),
            symbol: "OANDA:EURUSD".into(),
            tf: Timeframe::M5,
            direction: Direction::Bullish,
            break_ts: as_of,
            break_price: 1.12,
            swing_ts: as_of - 600_000,
            swing_price: 1.11,
        }),
        IctStructure::Mss(Mss {
            id: "mss-future".into(),
            symbol: "OANDA:EURUSD".into(),
            tf: Timeframe::M5,
            direction: Direction::Bullish,
            break_ts: as_of + 1,
            break_price: 1.13,
            swing_ts: as_of,
            swing_price: 1.12,
        }),
        IctStructure::Fvg(Fvg {
            id: "fvg-filled-later".into(),
            symbol: "OANDA:EURUSD".into(),
            tf: Timeframe::M5,
            direction: Direction::Bullish,
            ts_open: as_of - 2 * Timeframe::M5.duration_ms(),
            ts_confirm: as_of,
            price_low: 1.11,
            price_high: 1.12,
            state: FvgState::Filled,
            ts_filled: Some(as_of + Timeframe::M5.duration_ms()),
            consumed_exit_ts: None,
        }),
    ];
    evidence.smt.liquidity_refs = vec![LiquidityRef {
        symbol: "TVC:DXY".into(),
        ref_price: 100.0,
        ref_ts: as_of - Timeframe::M30.duration_ms(),
        side: LiquiditySide::SellSide,
        status: LiquidityRefStatus::Swept,
        tf: Timeframe::H4,
        mtf_ref_candle: Some(candle(as_of - Timeframe::M30.duration_ms())),
        mtf_sweep_candle: Some(candle(as_of + 1)),
    }];

    let packer = ContextPacker::default();
    let first = serde_json::to_string(&packer.pack(&evidence).unwrap()).unwrap();
    let second = serde_json::to_string(&packer.pack(&evidence).unwrap()).unwrap();
    assert_eq!(first, second, "same input must produce byte-identical JSON");
    assert!(first.contains("mss-visible"));
    assert!(!first.contains("mss-future"));
    assert!(!first.contains("9.999999"));
    assert!(!first.contains("generated_at"));
    assert!(!first.contains("wall_clock"));
    assert!(!first.contains(&(as_of + Timeframe::M5.duration_ms()).to_string()));

    let context = packer.pack(&evidence).unwrap();
    let trade_chain = context
        .l2_strategy_reading
        .chains
        .iter()
        .find(|chain| chain.symbol == "OANDA:EURUSD")
        .unwrap();
    assert!(trade_chain.c2.is_some());
    assert!(trade_chain.c3.is_none(), "future C3 must be stripped");
    assert!(context.l2_strategy_reading.liquidity_refs[0]
        .mtf_sweep_candle
        .is_none());
    let future_fill = context
        .facets
        .entry_zone
        .structures
        .iter()
        .find(|structure| structure.structure_id == "fvg-filled-later");
    assert_eq!(
        future_fill.map(|structure| structure.state_as_of.as_str()),
        Some("unknown_pre_fill")
    );
    let ltf = context
        .closed_bars
        .iter()
        .find(|series| series.symbol == "OANDA:EURUSD" && series.timeframe == Timeframe::M5)
        .unwrap();
    assert_eq!(ltf.bars.len(), 1);
    assert_eq!(
        ltf.bars[0].open, 1.12346,
        "FX price is truncated to 5 decimals"
    );
}

#[test]
fn m7b_context_packer_enforces_default_windows_and_six_facets() {
    let (candidate, smt, alert) = fixture("eu-gu", "candidate-windows");
    let mut evidence = packed_fixture(&candidate, &smt, &alert);
    let as_of = 4_000 + Timeframe::M30.duration_ms();
    for symbol in ["TVC:DXY", "OANDA:EURUSD"] {
        for (tf, count) in [
            (Timeframe::H4, 130usize),
            (Timeframe::M30, 130usize),
            (Timeframe::M5, 150usize),
        ] {
            for index in 0..count {
                let ts = as_of - ((count - index) as i64 * tf.duration_ms());
                evidence.market_bars.push(bar(symbol, tf, ts, 1.0));
            }
        }
    }
    evidence.market_structures.push(IctStructure::Fvg(Fvg {
        id: "fvg-entry".into(),
        symbol: "TVC:DXY".into(),
        tf: Timeframe::H4,
        direction: Direction::Bullish,
        ts_open: as_of - 3 * Timeframe::H4.duration_ms(),
        ts_confirm: as_of - Timeframe::H4.duration_ms(),
        price_low: 99.1,
        price_high: 99.2,
        state: FvgState::Active,
        ts_filled: None,
        consumed_exit_ts: None,
    }));

    let context = ContextPacker::default().pack(&evidence).unwrap();
    for series in &context.closed_bars {
        let expected = if series.timeframe == Timeframe::M5 {
            120
        } else {
            100
        };
        assert_eq!(series.bars.len(), expected);
        assert!(series
            .bars
            .windows(2)
            .all(|window| window[0].ts < window[1].ts));
    }
    let value = serde_json::to_value(&context).unwrap();
    for facet in [
        "htf_bias",
        "liquidity",
        "entry_zone",
        "smt",
        "premium_discount",
        "po3",
    ] {
        assert!(
            value["facets"].get(facet).is_some(),
            "missing facet {facet}"
        );
    }
    assert_eq!(
        context.facets.entry_zone.structure_ids,
        vec!["fvg-entry".to_string()]
    );
    assert_eq!(
        context.facets.smt.structure_ids,
        vec![smt.id.clone()],
        "the source SMT id must be explicitly citable"
    );
}

#[test]
fn m7b_decision_key_isolated_by_watchlist_even_with_same_alert_id() {
    let id_a = LlmDecisionPipeline::decision_id("eu-gu", "same-alert", "OANDA:EURUSD");
    let id_b = LlmDecisionPipeline::decision_id("aud-nzd", "same-alert", "OANDA:EURUSD");
    assert_ne!(id_a, id_b);
}

fn pipeline_with_limit(
    store: SqliteStore,
    provider: MockLlmProvider,
    max_calls_per_day: u32,
) -> Arc<LlmDecisionPipeline> {
    Arc::new(
        LlmDecisionPipeline::new(
            store,
            Arc::new(LlmSingleCallStrategy::new(
                Arc::new(provider),
                "mock",
                Some("mock-fixed".into()),
            )),
        )
        .with_max_calls_per_day(max_calls_per_day),
    )
}

async fn local_chat_server(
    statuses: Vec<u16>,
    content: String,
    response_delay: Duration,
) -> (
    String,
    Arc<parking_lot::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    local_chat_server_with_finish(statuses, content, response_delay, None).await
}

async fn local_chat_server_with_finish(
    statuses: Vec<u16>,
    content: String,
    response_delay: Duration,
    finish_reason: Option<String>,
) -> (
    String,
    Arc<parking_lot::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let captured = requests.clone();
    let handle = tokio::spawn(async move {
        for status in statuses {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let expected_len = loop {
                let mut chunk = [0_u8; 4096];
                let read = stream.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break bytes.len();
                }
                bytes.extend_from_slice(&chunk[..read]);
                let Some(header_end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                let total = header_end + 4 + content_length;
                if bytes.len() >= total {
                    break total;
                }
            };
            captured.lock().push(
                String::from_utf8_lossy(&bytes[..expected_len.min(bytes.len())]).into_owned(),
            );
            if !response_delay.is_zero() {
                tokio::time::sleep(response_delay).await;
            }
            let (reason, body) = if status == 200 {
                (
                    "OK",
                    serde_json::json!({
                        "choices": [{"message": {"content": content}, "finish_reason": finish_reason}]
                    })
                    .to_string(),
                )
            } else {
                ("Server Error", "{\"error\":\"temporary\"}".into())
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    (format!("http://{address}"), requests, handle)
}

fn provider_request(candidate_id: &str) -> LlmDecisionRequest {
    let (candidate, smt, alert) = fixture("eu-gu", candidate_id);
    ContextPacker::default()
        .request(&packed_fixture(&candidate, &smt, &alert))
        .unwrap()
}

#[tokio::test]
async fn m7c_openai_compatible_provider_sends_strict_schema_and_bearer_key() {
    let request = provider_request("candidate-http");
    let response_json = valid_json("candidate-http");
    let (base_url, requests, server) =
        local_chat_server(vec![200], response_json.clone(), Duration::ZERO).await;
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleOptions {
        base_url,
        api_key: "secret-test-key".into(),
        model: "gpt-test".into(),
        temperature: 0.0,
        timeout: Duration::from_secs(2),
        max_retries: 0,
        max_output_tokens: 4096,
        proxy_url: None,
        structured_output: StructuredOutputMode::JsonSchema,
        reasoning_effort: None,
    })
    .unwrap();

    let response = provider.decide(request).await.unwrap();
    assert_eq!(response.raw_response, response_json);
    server.await.unwrap();
    let wire = requests.lock().first().cloned().unwrap();
    assert!(wire.starts_with("POST /chat/completions HTTP/1.1"));
    assert!(wire
        .to_ascii_lowercase()
        .contains("authorization: bearer secret-test-key"));
    assert!(wire.contains("\"model\":\"gpt-test\""));
    assert!(wire.contains("\"max_tokens\":4096"));
    assert!(wire.contains("\"type\":\"json_schema\""));
    assert!(wire.contains("\"strict\":true"));
    assert!(!wire.contains("reasoning_effort"));
    assert!(wire.contains(M7D_PROMPT_VERSION));
    assert!(wire.contains(M7D_STRATEGY_VERSION));
}

#[tokio::test]
async fn m7c_json_object_and_prompt_only_modes_keep_local_schema_guard() {
    let response_json = valid_json("candidate-json-object");
    let (base_url, requests, server) =
        local_chat_server(vec![200], response_json.clone(), Duration::ZERO).await;
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleOptions {
        base_url,
        api_key: "deepseek-test-key".into(),
        model: "deepseek-chat".into(),
        temperature: 0.0,
        timeout: Duration::from_secs(2),
        max_retries: 0,
        max_output_tokens: 4096,
        proxy_url: None,
        structured_output: StructuredOutputMode::JsonObject,
        reasoning_effort: None,
    })
    .unwrap();

    let response = provider
        .decide(provider_request("candidate-json-object"))
        .await
        .unwrap();
    assert_eq!(response.raw_response, response_json);
    server.await.unwrap();
    let wire = requests.lock().first().cloned().unwrap();
    assert!(wire.contains("\"type\":\"json_object\""));
    assert!(!wire.contains("\"type\":\"json_schema\""));
    assert!(wire.contains("字段不得缺失"));

    let response_json = valid_json("candidate-prompt-only");
    let (base_url, requests, server) =
        local_chat_server(vec![200], response_json, Duration::ZERO).await;
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleOptions {
        base_url,
        api_key: "ark-plan-test-key".into(),
        model: "plan-model-id".into(),
        temperature: 0.0,
        timeout: Duration::from_secs(2),
        max_retries: 0,
        max_output_tokens: 4096,
        proxy_url: None,
        structured_output: StructuredOutputMode::PromptOnly,
        reasoning_effort: None,
    })
    .unwrap();
    provider
        .decide(provider_request("candidate-prompt-only"))
        .await
        .unwrap();
    server.await.unwrap();
    let wire = requests.lock().first().cloned().unwrap();
    assert!(!wire.contains("response_format"));
    assert!(wire.contains("字段不得缺失"));
}

#[tokio::test]
async fn m7c_provider_retries_schema_invalid_output_and_returns_last_raw_for_audit() {
    let (base_url, requests, server) =
        local_chat_server(vec![200, 200], "not-json".into(), Duration::ZERO).await;
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleOptions {
        base_url,
        api_key: "test".into(),
        model: "deepseek-chat".into(),
        temperature: 0.0,
        timeout: Duration::from_secs(2),
        max_retries: 1,
        max_output_tokens: 4096,
        proxy_url: None,
        structured_output: StructuredOutputMode::JsonObject,
        reasoning_effort: None,
    })
    .unwrap();

    let response = provider
        .decide(provider_request("candidate-invalid-json"))
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(requests.lock().len(), 2);
    assert_eq!(response.raw_response, "not-json");
}

#[tokio::test]
async fn m7c_provider_retries_transient_http_once_and_times_out() {
    let response_json = valid_json("candidate-retry");
    let (base_url, requests, server) =
        local_chat_server(vec![500, 200], response_json, Duration::ZERO).await;
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleOptions {
        base_url,
        api_key: "test".into(),
        model: "gpt-test".into(),
        temperature: 0.0,
        timeout: Duration::from_secs(2),
        max_retries: 1,
        max_output_tokens: 4096,
        proxy_url: None,
        structured_output: StructuredOutputMode::JsonSchema,
        reasoning_effort: None,
    })
    .unwrap();
    provider
        .decide(provider_request("candidate-retry"))
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(requests.lock().len(), 2);

    let (base_url, _requests, server) = local_chat_server(
        vec![200],
        valid_json("candidate-timeout"),
        Duration::from_millis(100),
    )
    .await;
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleOptions {
        base_url,
        api_key: "test".into(),
        model: "gpt-test".into(),
        temperature: 0.0,
        timeout: Duration::from_millis(20),
        max_retries: 0,
        max_output_tokens: 4096,
        proxy_url: None,
        structured_output: StructuredOutputMode::JsonSchema,
        reasoning_effort: None,
    })
    .unwrap();
    let error = provider
        .decide(provider_request("candidate-timeout"))
        .await
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_timeout),
        "expected a classified reqwest timeout, got: {error:#}"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn m7c_config_degrades_when_disabled_missing_key_or_invalid() {
    let (store, path) = test_store("config-degrade");
    let disabled = crate::config::LlmConfig::default();
    assert!(build_configured_pipeline(&disabled, store.clone())
        .unwrap()
        .is_none());

    let mut missing = disabled.clone();
    missing.enabled = true;
    missing.api_key_env = "ICT_M7C_KEY_THAT_MUST_NOT_EXIST_938475".into();
    assert!(build_configured_pipeline(&missing, store.clone())
        .unwrap()
        .is_none());

    let mut invalid = missing;
    invalid.provider = "unsupported".into();
    assert!(build_configured_pipeline(&invalid, store.clone()).is_err());

    let defaults = crate::config::LlmConfig::default();
    assert_eq!(defaults.max_calls_per_day, 500);
    assert_eq!(defaults.max_output_tokens, 4096);
    assert_eq!(defaults.resolved_response_format(), "json_schema");
    let mut deepseek = defaults.clone();
    deepseek.provider = "deepseek_official".into();
    assert!(deepseek.validate().is_ok());
    assert_eq!(deepseek.resolved_response_format(), "json_object");
    let mut ark = defaults.clone();
    ark.provider = "volcengine_ark_plan".into();
    assert!(ark.validate().is_ok());
    assert_eq!(ark.resolved_response_format(), "prompt_only");
    ark.response_format = "prompt_only".into();
    assert!(ark.validate().is_ok());
    assert_eq!(ark.resolved_response_format(), "prompt_only");
    ark.response_format = "xml".into();
    assert!(ark.validate().is_err());

    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn m7c_daily_cap_is_durable_and_never_touches_alert_facts() {
    let (store, path) = test_store("daily-cap");
    let (candidate_a, smt_a, alert_a) = fixture("eu-gu", "candidate-cap-a");
    let (candidate_b, smt_b, mut alert_b) = fixture("eu-gu", "candidate-cap-b");
    alert_b.id.push_str("-second-episode");
    let alert_a_before = serde_json::to_vec(&alert_a).unwrap();
    let alert_b_before = serde_json::to_vec(&alert_b).unwrap();
    let provider = MockLlmProvider::new(MockProviderReply::Json(valid_json(&candidate_a.id)));
    let calls = provider.clone();
    let pipeline = pipeline_with_limit(store.clone(), provider, 1);

    pipeline
        .dispatch(&candidate_a, &alert_a, &smt_a, false)
        .await
        .unwrap();
    pipeline
        .dispatch(&candidate_b, &alert_b, &smt_b, false)
        .await
        .unwrap();
    assert_eq!(calls.calls(), 1);
    let capped = store.list_decisions(&candidate_b.id).unwrap().remove(0);
    assert_eq!(capped.error.as_deref(), Some("daily call limit reached"));
    assert_eq!(serde_json::to_vec(&alert_a).unwrap(), alert_a_before);
    assert_eq!(serde_json::to_vec(&alert_b).unwrap(), alert_b_before);

    drop(pipeline);
    drop(store);

    // The UTC-day reservation is persisted independently of the pipeline,
    // so restarting the application cannot reset the billing safety valve.
    let reopened = SqliteStore::open(&path).unwrap();
    reopened.ensure_candidate_schema().unwrap();
    let (candidate_c, smt_c, mut alert_c) = fixture("eu-gu", "candidate-cap-c");
    alert_c.id.push_str("-after-restart");
    let provider = MockLlmProvider::new(MockProviderReply::Json(valid_json(&candidate_c.id)));
    let calls_after_restart = provider.clone();
    let restarted = pipeline_with_limit(reopened.clone(), provider, 1);
    restarted
        .dispatch(&candidate_c, &alert_c, &smt_c, false)
        .await
        .unwrap();
    assert_eq!(
        calls_after_restart.calls(),
        0,
        "restart must not reset the durable daily call cap"
    );
    assert_eq!(
        reopened
            .list_decisions(&candidate_c.id)
            .unwrap()
            .remove(0)
            .error
            .as_deref(),
        Some("daily call limit reached")
    );

    drop(restarted);
    drop(reopened);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn m7c_t1_all_sidecar_states_preserve_alert_event_and_notification_bytes() {
    #[derive(Clone, Copy)]
    enum SidecarState {
        Disabled,
        Success,
        ProviderError,
        Timeout,
        DailyCap,
    }

    for (index, state) in [
        SidecarState::Disabled,
        SidecarState::Success,
        SidecarState::ProviderError,
        SidecarState::Timeout,
        SidecarState::DailyCap,
    ]
    .into_iter()
    .enumerate()
    {
        let (store, path) = test_store(&format!("t1-sidecar-{index}"));
        let (candidate, smt, mut alert) =
            fixture("eu-gu-dxy", &format!("candidate-t1-sidecar-{index}"));
        alert.channels_fired = vec![ChannelKind::Inbox, ChannelKind::DesktopNotify];
        store
            .upsert_candidate(&candidate, alert.created_at)
            .unwrap();
        store.insert_alert(&alert).unwrap();

        // Inbox persistence and `ict:alert:fired` both serialize this exact
        // AlertRecord. Desktop delivery order is represented by the immutable
        // channels_fired sequence assembled before the sidecar is spawned.
        let event_payload_before = serde_json::to_vec(&alert).unwrap();
        let notification_sequence_before = alert.channels_fired.clone();
        let inbox_before = serde_json::to_vec(&store.list_alerts(None).unwrap()).unwrap();

        if !matches!(state, SidecarState::Disabled) {
            let reply = match state {
                SidecarState::Success | SidecarState::DailyCap => {
                    MockProviderReply::Json(valid_json(&candidate.id))
                }
                SidecarState::ProviderError => MockProviderReply::Error("provider down".into()),
                SidecarState::Timeout => MockProviderReply::Error("request timed out".into()),
                SidecarState::Disabled => unreachable!(),
            };
            let calls = MockLlmProvider::new(reply);
            let max_calls = if matches!(state, SidecarState::DailyCap) {
                0
            } else {
                500
            };
            let sidecar = pipeline_with_limit(store.clone(), calls, max_calls);
            sidecar
                .dispatch(&candidate, &alert, &smt, false)
                .await
                .unwrap();
        }

        assert_eq!(serde_json::to_vec(&alert).unwrap(), event_payload_before);
        assert_eq!(alert.channels_fired, notification_sequence_before);
        assert_eq!(
            serde_json::to_vec(&store.list_alerts(None).unwrap()).unwrap(),
            inbox_before,
            "LLM sidecar state {index} changed the authoritative Alert Inbox"
        );
        assert_eq!(
            store.list_decisions(&candidate.id).unwrap().len(),
            usize::from(!matches!(state, SidecarState::Disabled))
        );

        drop(store);
        let _ = std::fs::remove_file(path);
    }
}

#[tokio::test]
async fn m7c_concurrent_and_restart_dispatch_are_idempotent() {
    let (store, path) = test_store("concurrent-restart");
    let (candidate, smt, alert) = fixture("eu-gu", "candidate-concurrent");
    let provider = MockLlmProvider::new(MockProviderReply::Json(valid_json(&candidate.id)))
        .with_delay(Duration::from_millis(40));
    let calls = provider.clone();
    let first_pipeline = pipeline(store.clone(), provider);
    let (first, second) = tokio::join!(
        first_pipeline.dispatch(&candidate, &alert, &smt, false),
        first_pipeline.dispatch(&candidate, &alert, &smt, false)
    );
    let outcomes = [first.unwrap(), second.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, DispatchOutcome::Stored(_)))
            .count(),
        1
    );
    assert_eq!(calls.calls(), 1);
    drop(first_pipeline);
    drop(store);

    let reopened = SqliteStore::open(&path).unwrap();
    reopened.ensure_candidate_schema().unwrap();
    let provider = MockLlmProvider::new(MockProviderReply::Json(valid_json(&candidate.id)));
    let calls = provider.clone();
    let pipeline = pipeline(reopened.clone(), provider);
    assert_eq!(
        pipeline
            .dispatch(&candidate, &alert, &smt, false)
            .await
            .unwrap(),
        DispatchOutcome::Duplicate
    );
    assert_eq!(calls.calls(), 0, "restart must not repeat a completed call");
    drop(pipeline);
    drop(reopened);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn m7c_startup_recovers_only_pending_row_with_alert_as_of_bars() {
    let (store, path) = test_store("pending-recovery");
    let (candidate, smt, alert) = fixture("eu-gu", "candidate-recover");
    store.upsert_smt(&smt, alert.created_at).unwrap();
    store
        .upsert_candidate(&candidate, alert.created_at)
        .unwrap();
    store.insert_alert(&alert).unwrap();
    for symbol in ["TVC:DXY", "OANDA:EURUSD"] {
        for tf in [Timeframe::H4, Timeframe::M30, Timeframe::M5] {
            store
                .insert_bar(&bar(
                    symbol,
                    tf,
                    alert.created_at - tf.duration_ms(),
                    1.111_11,
                ))
                .unwrap();
            store
                .insert_bar(&bar(
                    symbol,
                    tf,
                    alert.created_at + tf.duration_ms(),
                    9.999_99,
                ))
                .unwrap();
        }
    }
    let id = LlmDecisionPipeline::decision_id(&alert.watchlist_id, &alert.id, "OANDA:EURUSD");
    store
        .insert_decision(&crate::candidate::DecisionLogEntry {
            id: id.clone(),
            candidate_id: candidate.id.clone(),
            watchlist_id: alert.watchlist_id.clone(),
            alert_id: Some(alert.id.clone()),
            trade_symbol: Some("OANDA:EURUSD".into()),
            parent_id: None,
            provider: "openai_compatible".into(),
            model: Some("gpt-test".into()),
            decision_mode: DecisionMode::LlmSingleCall,
            prompt_version: Some(M7D_PROMPT_VERSION.into()),
            strategy_version: M7D_STRATEGY_VERSION.into(),
            context_version: M7D_CONTEXT_VERSION.into(),
            request_json: "{}".into(),
            raw_response: None,
            parsed_decision_json: "{}".into(),
            parse_ok: false,
            error: None,
            created_at: alert.created_at,
        })
        .unwrap();
    drop(store);

    let reopened = SqliteStore::open(&path).unwrap();
    reopened.ensure_candidate_schema().unwrap();
    let provider = MockLlmProvider::new(MockProviderReply::Json(valid_json(&candidate.id)));
    let calls = provider.clone();
    let pipeline = pipeline(reopened.clone(), provider);
    assert_eq!(pipeline.recover_stale_pending().await.unwrap(), 1);
    assert_eq!(calls.calls(), 1);
    let recovered = reopened.get_decision(&id).unwrap().unwrap();
    assert!(recovered.parse_ok);
    assert!(recovered.request_json.contains("1.11111"));
    assert!(
        !recovered.request_json.contains("9.99999"),
        "bars newer than the original alert as_of must not leak into recovery"
    );
    assert_eq!(pipeline.recover_stale_pending().await.unwrap(), 0);
    assert_eq!(calls.calls(), 1);

    drop(pipeline);
    drop(reopened);
    let _ = std::fs::remove_file(path);
}

#[test]
fn m7d_strategy_seed_is_versioned_and_record_only() {
    let (store, path) = test_store("strategy-seed");
    let conn = rusqlite::Connection::open(&path).unwrap();
    let value: String = conn
        .query_row(
            "SELECT value FROM detector_config WHERE detector='llm_strategy' AND key='m7.v1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&value).unwrap();
    assert_eq!(parsed["strategy_version"], M7D_STRATEGY_VERSION);
    drop(conn);
    drop(store);
    let _ = std::fs::remove_file(path);
}

fn v2_l1_evidence() -> crate::candidate::PackedEvidence {
    let (mut candidate, mut smt, alert) = fixture("v2-group", "v2-candidate");
    candidate.deterministic_score = 0.85;
    smt.liquidity_refs.clear();
    packed_fixture(&candidate, &smt, &alert)
}
fn v2_level(id: &str, price: f64, available: i64, buy: bool) -> IctStructure {
    let level = crate::detector::types::LevelMarker {
        id: id.into(),
        symbol: "OANDA:EURUSD".into(),
        tf: Timeframe::M1,
        price,
        label: if buy { "PDH" } else { "PDL" }.into(),
        confirmed_at_ts: Some(available),
        valid_from_ts: available - 86_400_000,
        valid_until_ts: available,
        source_ts: Some(available - 60_000),
    };
    if buy {
        IctStructure::Pdh(level)
    } else {
        IctStructure::Pdl(level)
    }
}

#[test]
fn v2_l1_targets_side_symbol_outside_nearest_three_and_grade() {
    let mut e = v2_l1_evidence();
    e.market_structures = vec![
        v2_level("far", 94.0, e.as_of_ts, false),
        v2_level("near", 99.0, e.as_of_ts, false),
        v2_level("middle", 97.0, e.as_of_ts, false),
        v2_level("fourth", 96.0, e.as_of_ts, false),
        v2_level("duplicate", 99.0, e.as_of_ts, false),
        v2_level("inside", 100.25, e.as_of_ts, false),
        v2_level("wrong-side", 98.0, e.as_of_ts, true),
        v2_level("future", 99.5, e.as_of_ts + 1, false),
    ];
    let mut other = v2_level("other-symbol", 99.8, e.as_of_ts, false);
    if let IctStructure::Pdl(s) = &mut other {
        s.symbol = "TVC:DXY".into();
    }
    e.market_structures.push(other);
    let context = ContextPacker::default().pack(&e).unwrap();
    let g = &context.deterministic_guardrails;
    assert_eq!(
        g.targets.iter().map(|t| t.price).collect::<Vec<_>>(),
        vec![99.0, 97.0, 96.0]
    );
    assert_eq!(g.rule_version, "m7d.decision_rules.v2");
    assert_eq!(g.confidence, 85);
    assert_eq!(g.quality, LlmDecisionQuality::A);
    assert_eq!(g.entry_zone.as_ref().unwrap().low, 100.0);
    assert_eq!(g.invalidation_price, Some(101.0));
    let response: LlmDecision = serde_json::from_str(&valid_json(&e.candidate_id)).unwrap();
    let normalized = response.apply_guardrails(&context).unwrap();
    assert_eq!(normalized.targets, g.targets);
    assert_eq!(normalized.direction, g.direction);
    assert!(normalized.confidence <= g.confidence);
    let mut decline: LlmDecision = serde_json::from_str(&valid_json(&e.candidate_id)).unwrap();
    decline.alert = false;
    assert!(!decline.apply_guardrails(&context).unwrap().alert);
}

#[test]
fn v2_pdh_waits_for_source_day_close_and_future_bars_do_not_change_targets() {
    let mut e = v2_l1_evidence();
    let as_of = e.as_of_ts;
    e.market_structures
        .push(v2_level("later-daily-low", 98.0, as_of + 1, false));
    assert!(ContextPacker::default()
        .pack_as_of(&e, as_of)
        .unwrap()
        .deterministic_guardrails
        .targets
        .is_empty());
    e.market_structures[0] = v2_level("known-daily-low", 98.0, as_of, false);
    let expected = ContextPacker::default()
        .pack_as_of(&e, as_of)
        .unwrap()
        .deterministic_guardrails;
    for offset in 0..100 {
        let mut future = bar("OANDA:EURUSD", Timeframe::M5, as_of + offset * 300_000, 1.0);
        future.low = -9999.0;
        e.market_bars.push(future);
    }
    assert_eq!(
        ContextPacker::default()
            .pack_as_of(&e, as_of)
            .unwrap()
            .deterministic_guardrails,
        expected
    );
}

#[test]
fn v2_l1_untouched_requires_contiguous_closed_coverage_and_rejects_sweep() {
    let mut e = v2_l1_evidence();
    let from = e.as_of_ts - 600_000;
    e.market_structures.push(v2_level("pdl", 98.0, from, false));
    let pack = |e: &crate::candidate::PackedEvidence| {
        ContextPacker::default()
            .pack_as_of(e, e.as_of_ts)
            .unwrap()
            .deterministic_guardrails
    };
    assert!(pack(&e).targets.is_empty());
    e.market_bars = vec![
        bar("OANDA:EURUSD", Timeframe::M5, from, 100.0),
        bar("OANDA:EURUSD", Timeframe::M5, from + 300_000, 100.0),
    ];
    assert_eq!(pack(&e).targets.len(), 1);
    e.market_bars[0].low = 97.0;
    assert!(pack(&e).targets.is_empty());
    e.market_bars[0].low = 99.0;
    e.market_structures.push(IctStructure::LiquiditySweep(
        crate::detector::types::LiquiditySweep {
            id: "dated-sweep".into(),
            symbol: "OANDA:EURUSD".into(),
            tf: Timeframe::M5,
            side: LiquiditySide::SellSide,
            pool_kind: crate::detector::types::LiquidityPoolKind::Pdl,
            sweep_ts: from,
            sweep_price: 97.0,
            level_ts: from - 60_000,
            level_price: 98.0,
            close_price: 99.0,
        },
    ));
    assert!(pack(&e).targets.is_empty());
}

#[test]
fn v2_equal_pool_confirmation_and_historical_state_not_current_boolean() {
    use crate::detector::types::EqualHighsLows;
    let mut e = v2_l1_evidence();
    let from = e.as_of_ts - 300_000;
    let pool = EqualHighsLows {
        id: "eql".into(),
        symbol: "OANDA:EURUSD".into(),
        tf: Timeframe::M5,
        side: LiquiditySide::SellSide,
        ts_start: from - 600_000,
        ts_end: from - 300_000,
        price: 98.0,
        tolerance_price: 0.001,
        confirmed_at_ts: Some(from),
        swept: true,
    };
    e.market_structures.push(IctStructure::EqualHighsLows(pool));
    e.market_bars
        .push(bar("OANDA:EURUSD", Timeframe::M5, from, 100.0));
    let pack = |e: &crate::candidate::PackedEvidence| {
        ContextPacker::default()
            .pack_as_of(e, e.as_of_ts)
            .unwrap()
            .deterministic_guardrails
    };
    assert_eq!(
        pack(&e).targets.len(),
        1,
        "today's swept flag cannot remove yesterday's confirmed untouched pool"
    );
    if let IctStructure::EqualHighsLows(p) = &mut e.market_structures[0] {
        p.confirmed_at_ts = Some(e.as_of_ts + 1);
    }
    assert!(pack(&e).targets.is_empty());
    if let IctStructure::EqualHighsLows(p) = &mut e.market_structures[0] {
        p.confirmed_at_ts = None;
        p.swept = false;
    }
    assert!(
        pack(&e).targets.is_empty(),
        "legacy pivot timestamp does not prove confirmation"
    );
}

#[test]
fn v2_sqlite_strategy_seed_upgrades_pointer_preserves_old_version() {
    let (store, path) = test_store("v2-seed");
    store.ensure_candidate_schema().unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("INSERT OR REPLACE INTO detector_config(detector,key,value) VALUES('llm_strategy','m7d.decision_rules.v1','legacy-policy')",[]).unwrap();
    conn.execute(
        "UPDATE detector_config SET value='{}' WHERE detector='llm_strategy' AND key='m7.v1'",
        [],
    )
    .unwrap();
    drop(conn);
    store.ensure_candidate_schema().unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    let pointer: String = conn
        .query_row(
            "SELECT value FROM detector_config WHERE detector='llm_strategy' AND key='m7.v1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&pointer).unwrap()["strategy_version"],
        M7D_STRATEGY_VERSION
    );
    let old:String=conn.query_row("SELECT value FROM detector_config WHERE detector='llm_strategy' AND key='m7d.decision_rules.v1'",[],|r|r.get(0)).unwrap();
    assert_eq!(old, "legacy-policy");
    assert_eq!(conn.query_row("SELECT count(*) FROM detector_config WHERE detector='llm_strategy' AND key='m7d.decision_rules.v2'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    drop(conn);
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn v2_legacy_pdh_display_and_source_windows_have_distinct_availability() {
    let mut e = v2_l1_evidence();
    let as_of = e.as_of_ts;
    let mut display = v2_level("display-window", 98.0, as_of, false);
    if let IctStructure::Pdl(level) = &mut display {
        level.confirmed_at_ts = None;
        level.valid_from_ts = as_of;
        level.valid_until_ts = as_of + 86_400_000;
        level.source_ts = Some(as_of - 60_000);
    }
    e.market_structures = vec![display];
    assert_eq!(
        ContextPacker::default()
            .pack_as_of(&e, as_of)
            .unwrap()
            .deterministic_guardrails
            .targets
            .len(),
        1
    );
    if let IctStructure::Pdl(level) = &mut e.market_structures[0] {
        level.source_ts = Some(as_of + 60_000);
    }
    assert!(
        ContextPacker::default()
            .pack_as_of(&e, as_of)
            .unwrap()
            .deterministic_guardrails
            .targets
            .is_empty(),
        "source-day extreme must wait for source day close"
    );
}

#[tokio::test]
async fn provider_does_not_retry_http_400_and_keeps_status() {
    let (base_url, requests, server) =
        local_chat_server(vec![400], String::new(), Duration::ZERO).await;
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleOptions {
        base_url,
        api_key: "test-secret".into(),
        model: "test".into(),
        temperature: 0.0,
        timeout: Duration::from_secs(2),
        max_retries: 1,
        max_output_tokens: 4096,
        proxy_url: None,
        structured_output: StructuredOutputMode::PromptOnly,
        reasoning_effort: None,
    })
    .unwrap();
    let error = provider
        .decide(provider_request("candidate-400"))
        .await
        .unwrap_err();
    server.await.unwrap();
    assert_eq!(requests.lock().len(), 1);
    assert!(error.to_string().contains("HTTP 400"));
    assert!(!error.to_string().contains("test-secret"));
    assert_eq!(
        error
            .downcast_ref::<reqwest::Error>()
            .unwrap()
            .status()
            .unwrap()
            .as_u16(),
        400
    );
}

#[test]
fn bounded_context_preserves_guardrail_prices_and_critical_evidence() {
    let mut request = provider_request("bounded-history");
    let c = &mut request.context;
    let original = c.deterministic_guardrails.clone();
    let pinned = original.required_evidence_ids[0].clone();
    for i in 0..30000 {
        let id = if i == 0 {
            pinned.clone()
        } else {
            format!("background-{i}")
        };
        c.facets.entry_zone.structure_ids.push(id.clone());
        c.deterministic_guardrails
            .allowed_evidence_ids
            .push(id.clone());
        c.facets.entry_zone.structures.push(StructureEvidence {
            structure_id: id,
            kind: "fvg".into(),
            symbol: "EURUSD".into(),
            timeframe: Timeframe::M5,
            available_at_ts: i,
            state_as_of: "active".into(),
            facts: serde_json::json!({"low": 1.1, "high": 1.2}),
        });
    }
    request.bound_provider_context();
    assert!(request.context.facets.entry_zone.structures.len() <= 16);
    assert!(request
        .context
        .facets
        .entry_zone
        .structure_ids
        .contains(&pinned));
    let now = &request.context.deterministic_guardrails;
    assert_eq!(now.entry_zone, original.entry_zone);
    assert_eq!(now.invalidation_price, original.invalidation_price);
    assert_eq!(now.targets, original.targets);
    assert_eq!(now.required_evidence_ids, original.required_evidence_ids);
    assert!(now.allowed_evidence_ids.len() < 100);
    assert!(serde_json::to_vec(&request).unwrap().len() < 196608);
    let once = serde_json::to_string(&request).unwrap();
    request.bound_provider_context();
    assert_eq!(serde_json::to_string(&request).unwrap(), once);
}

#[tokio::test]
async fn provider_sends_explicit_reasoning_effort_and_reports_empty_output() {
    let (base_url, requests, server) =
        local_chat_server(vec![200], String::new(), Duration::ZERO).await;
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleOptions {
        base_url,
        api_key: "test-secret".into(),
        model: "glm-5.3".into(),
        temperature: 0.0,
        timeout: Duration::from_secs(2),
        max_retries: 1,
        max_output_tokens: 16384,
        proxy_url: None,
        structured_output: StructuredOutputMode::PromptOnly,
        reasoning_effort: Some("low".into()),
    })
    .unwrap();
    let error = provider
        .decide(provider_request("candidate-reasoning"))
        .await
        .unwrap_err();
    server.await.unwrap();
    let captured = requests.lock();
    assert_eq!(captured.len(), 1);
    let wire = captured[0].split("\r\n\r\n").nth(1).unwrap();
    let body: serde_json::Value = serde_json::from_str(wire).unwrap();
    assert_eq!(body["reasoning_effort"], "low");
    assert_eq!(body["max_tokens"], 16384);
    assert!(error.to_string().contains("empty content"));
    assert!(error.to_string().contains("reasoning_effort=low"));
    assert!(!error.to_string().contains("test-secret"));
}

#[tokio::test]
async fn empty_response_redacts_finish_reason_before_truncating() {
    let secret = format!("ark-finish-{}", "private".repeat(20));
    let finish = format!("length\n{secret}{}", "界".repeat(1000));
    let (base_url, _, server) = local_chat_server_with_finish(
        vec![200], String::new(), Duration::ZERO, Some(finish),
    ).await;
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleOptions {
        base_url, api_key: secret, model: "test".into(), temperature: 0.0,
        timeout: Duration::from_secs(2), max_retries: 0, max_output_tokens: 4096,
        proxy_url: None, structured_output: StructuredOutputMode::PromptOnly,
        reasoning_effort: None,
    }).unwrap();
    let error = provider.decide(provider_request("empty-echo")).await.unwrap_err().to_string();
    server.await.unwrap();
    assert!(error.contains("finish_reason=length[REDACTED]"));
    assert!(!error.contains("ark-finish"));
    assert!(!error.contains("private"));
    assert!(!error.chars().any(char::is_control));
    assert!(error.chars().count() < 250);
}
