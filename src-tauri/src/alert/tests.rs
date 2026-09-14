//! Alert engine tests (M6b, §12 T1-T12).
use std::sync::Arc;

use crate::alert::*;
use crate::candidate::*;
use crate::detector::types::*;
use crate::types::Timeframe;

fn make_candidate(id: &str, status: SetupStatus) -> CandidateSetup {
    CandidateSetup {
        id: id.into(),
        rule_version: "m6a.v1".into(),
        watchlist_id: "test".into(),
        smt_id: format!("smt-{}", id),
        setup_type: SetupType::Smt,
        symbol_set: vec!["TVC:DXY".into(), "OANDA:EURUSD".into()],
        sweeper_symbol: "TVC:DXY".into(),
        trade_symbols: vec!["OANDA:EURUSD".into()],
        candidate_direction: Direction::Bullish,
        context_timeframe: Timeframe::H4,
        comparison_timeframe: Timeframe::H1,
        validation_timeframe: Timeframe::M5,
        context_pda_id: Some("pda-1".into()),
        observation_window: (0, 0),
        c1_candle: CandleRef {
            ts: 1000,
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.5,
        },
        smt_k_candle: CandleRef {
            ts: 2000,
            open: 99.5,
            high: 100.5,
            low: 98.5,
            close: 99.0,
        },
        c2_candle: CandleRef {
            ts: 3000,
            open: 99.0,
            high: 100.0,
            low: 98.0,
            close: 99.5,
        },
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
        setup_status: status,
        decision_status: DecisionStatus::New,
        deterministic_score: 0.5,
        created_at: 3000,
        validated_at: None,
        expired_at: None,
        invalidated_at: None,
        expiry_reason: None,
        strength: vec![],
        expiry_at: None,
        smt_rule_version: "m5c2.v5".into(),
    }
}

fn make_change(id: &str, status: SetupStatus) -> CandidateChange {
    let cand = make_candidate(id, status);
    CandidateChange {
        candidate: cand,
        validation_event: None,
        decision: crate::candidate::DecisionLogEntry {
            id: "dec".into(),
            candidate_id: id.into(),
            watchlist_id: "test".into(),
            alert_id: None,
            trade_symbol: None,
            parent_id: None,
            provider: "deterministic".into(),
            model: None,
            decision_mode: DecisionMode::Deterministic,
            prompt_version: None,
            strategy_version: "m6a.v1".into(),
            context_version: "m6a.v1".into(),
            request_json: "{}".into(),
            raw_response: None,
            parsed_decision_json: "{}".into(),
            parse_ok: true,
            error: None,
            created_at: 5000,
        },
    }
}

#[test]
fn recovered_c2_stays_silent_and_the_next_live_c2_notifies_once_per_symbol() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct DesktopCounter(Arc<AtomicUsize>);
    impl AlertChannel for DesktopCounter {
        fn kind(&self) -> ChannelKind {
            ChannelKind::DesktopNotify
        }
        fn deliver(&self, _: &AlertRecord) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    let path = std::env::temp_dir().join(format!(
        "ict-c2-recovery-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = crate::storage::SqliteStore::open(&path).unwrap();
    store.ensure_alert_schema().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let mut engine = AlertEngine::with_store(store.clone());
    engine.add_channel(Box::new(DesktopCounter(count.clone())));
    let mut historical = make_candidate("recovered", SetupStatus::C2Confirmed);
    historical.trade_symbols = vec!["OANDA:EURUSD".into(), "OANDA:GBPUSD".into()];
    let smt = make_two_symbol_c2_smt(&historical);
    let rows = engine.on_c2_confirmed(&historical, &smt, true);
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|r| r.channels_fired.is_empty() && store.has_alert_id(&r.id).unwrap()));
    assert!(engine.on_c2_confirmed(&historical, &smt, false).is_empty());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let mut live = historical.clone();
    live.id = "live".into();
    live.smt_id = "smt-live".into();
    live.observation_window = (100_000, 200_000);
    let live_smt = make_two_symbol_c2_smt(&live);
    assert_eq!(engine.on_c2_confirmed(&live, &live_smt, false).len(), 2);
    assert!(engine.on_c2_confirmed(&live, &live_smt, false).is_empty());
    assert_eq!(count.load(Ordering::SeqCst), 2);
    drop(engine);
    drop(store);
    let _ = std::fs::remove_file(path);
}

fn make_two_symbol_c2_smt(candidate: &CandidateSetup) -> SmtDivergence {
    let chain = |symbol: &str, c2_ts: i64, c2_case: u8| SymbolChain {
        symbol: symbol.into(),
        c1_candle: CandleRef {
            ts: 1_000,
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.5,
        },
        smt_k_candle: CandleRef {
            ts: 2_000,
            open: 99.5,
            high: 100.5,
            low: 98.5,
            close: 99.0,
        },
        c2_candle: Some(CandleRef {
            ts: c2_ts,
            open: 99.0,
            high: 100.0,
            low: 98.0,
            close: 99.5,
        }),
        c2_case: Some(c2_case),
        c3_candle: None,
        detection_state: SmtDetectionState::C2Confirmed,
    };

    SmtDivergence {
        id: candidate.smt_id.clone(),
        watchlist_id: candidate.watchlist_id.clone(),
        rule_version: candidate.smt_rule_version.clone(),
        symbol_set: vec![
            "TVC:DXY".into(),
            "OANDA:EURUSD".into(),
            "OANDA:GBPUSD".into(),
        ],
        relationship: Correlation::Negative,
        context_timeframe: Timeframe::H4,
        comparison_timeframe: Timeframe::H1,
        observation_window: candidate.observation_window,
        htf_confirmed: true,
        reference_scope: ReferenceScope::DistantLeftSide,
        liquidity_refs: vec![],
        confluence_refs: vec![],
        candidate_direction: Direction::Bullish,
        sweeper_symbol: "TVC:DXY".into(),
        trade_symbols: vec!["OANDA:EURUSD".into(), "OANDA:GBPUSD".into()],
        strength: vec![],
        chains: vec![
            // The sweeper deliberately confirms after both trade symbols.
            // It must not delay their per-symbol Alert Inbox timestamps.
            chain("TVC:DXY", 6_000, 1),
            chain("OANDA:EURUSD", 4_000, 2),
            chain("OANDA:GBPUSD", 5_000, 3),
        ],
        invalidation_reasons: vec![],
        invalidation_ts: None,
        htf_pda_ref: None,
        mtf_ref_candle: None,
    }
}

#[test]
fn c2_confirmation_creates_one_alert_per_trade_symbol_without_cooldown_loss() {
    let path = std::env::temp_dir().join(format!(
        "ict-monitor-c2-symbol-alerts-{}.db",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = crate::storage::SqliteStore::open(&path).expect("open test db");
    store.ensure_alert_schema().expect("alert schema");
    let mut engine = AlertEngine::with_store(store);
    engine.set_cooldown_seconds(600);

    let mut candidate = make_candidate("two-symbol-c2", SetupStatus::C2Confirmed);
    candidate.trade_symbols = vec!["OANDA:EURUSD".into(), "OANDA:GBPUSD".into()];
    let smt = make_two_symbol_c2_smt(&candidate);

    let rows = engine.on_c2_confirmed(&candidate, &smt, false);
    assert_eq!(rows.len(), 2, "both trade symbols must alert");
    assert_ne!(rows[0].id, rows[1].id);
    assert!(rows
        .iter()
        .all(|row| row.trigger == AlertTrigger::C2Confirmed));
    assert_eq!(rows[0].trade_symbols.len(), 1);
    assert_eq!(rows[1].trade_symbols.len(), 1);
    assert_eq!(rows[0].candidate_direction, Direction::Bearish);
    assert_eq!(rows[1].candidate_direction, Direction::Bearish);
    assert_eq!(rows[0].c2_case, 2);
    assert_eq!(rows[1].c2_case, 3);
    assert_eq!(rows[0].created_at, 4_000 + Timeframe::H1.duration_ms());
    assert_eq!(rows[1].created_at, 5_000 + Timeframe::H1.duration_ms());

    assert!(
        engine.on_c2_confirmed(&candidate, &smt, false).is_empty(),
        "repeated SMT snapshots must not duplicate setup alerts"
    );

    let mut historical_invalidated = candidate.clone();
    historical_invalidated.id = "historical-invalidated".into();
    historical_invalidated.smt_id = "historical-invalidated-smt".into();
    historical_invalidated.observation_window = (10_000, 20_000);
    historical_invalidated.invalidated_symbols = vec!["OANDA:EURUSD".into()];
    let historical_smt = make_two_symbol_c2_smt(&historical_invalidated);
    assert_eq!(
        engine
            .on_c2_confirmed(&historical_invalidated, &historical_smt, true)
            .len(),
        2,
        "cold replay must reconstruct rows that entered C2 before later invalidation"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn alert_schema_repairs_legacy_c2_alert_time_to_trade_symbol_confirmation() {
    let path = std::env::temp_dir().join(format!(
        "ict-monitor-c2-alert-time-migration-{}.db",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = crate::storage::SqliteStore::open(&path).expect("open test db");
    store.ensure_alert_schema().expect("alert schema");

    let mut candidate = make_candidate("legacy-c2-time", SetupStatus::C2Confirmed);
    candidate.trade_symbols = vec!["OANDA:EURUSD".into()];
    let smt = make_two_symbol_c2_smt(&candidate);
    let mut alert = build_c2_alert(
        &candidate,
        &smt,
        "OANDA:EURUSD",
        9_000_000,
        vec![ChannelKind::Inbox],
    )
    .expect("build C2 alert");
    alert.created_at = 9_000_000; // Legacy max(trade C2, sweeper C2) result.
    store.insert_alert(&alert).expect("insert legacy alert");

    store
        .ensure_alert_schema()
        .expect("run idempotent alert migration");
    let repaired = store.list_alerts(None).expect("list repaired alert");
    assert_eq!(repaired.len(), 1);
    assert_eq!(
        repaired[0].created_at,
        4_000 + Timeframe::H1.duration_ms(),
        "historical alert time must identify the trade symbol C2 confirmation"
    );

    let _ = std::fs::remove_file(path);
}

// T1: C2Confirmed -> Validated triggers alert.
#[test]
fn t1_validated_triggers_alert() {
    let mut engine = AlertEngine::new();
    // Seed prev_status as C2Confirmed.
    let c2 = make_change("c1", SetupStatus::C2Confirmed);
    engine.on_candidate_change(&c2, 1000, true); // seeding=true, no alert
                                                 // Transition to Validated.
    let val = make_change("c1", SetupStatus::Validated);
    let alert = engine.on_candidate_change(&val, 2000, false);
    assert!(alert.is_some(), "Validated transition should trigger alert");
}

// T2: Non-Validated transitions don't trigger.
#[test]
fn t2_non_validated_no_alert() {
    let mut engine = AlertEngine::new();
    // spawn (C2Confirmed) - no alert
    let spawn = make_change("c2", SetupStatus::C2Confirmed);
    assert!(engine.on_candidate_change(&spawn, 1000, false).is_none());
    // expired - no alert
    let expired = make_change("c2", SetupStatus::Expired);
    assert!(engine.on_candidate_change(&expired, 2000, false).is_none());
    // invalidated - no alert
    let inv = make_change("c2", SetupStatus::Invalidated);
    assert!(engine.on_candidate_change(&inv, 3000, false).is_none());
}

// T5: Seeding suppresses alerts.
#[test]
fn t5_seeding_suppresses() {
    let mut engine = AlertEngine::new();
    let c2 = make_change("c3", SetupStatus::C2Confirmed);
    engine.on_candidate_change(&c2, 1000, true);
    let val = make_change("c3", SetupStatus::Validated);
    let alert = engine.on_candidate_change(&val, 2000, true);
    assert!(alert.is_none(), "seeding=true should suppress alert");
    // After seeding, prev_status should be Validated.
    let val2 = make_change("c3", SetupStatus::Validated);
    let alert2 = engine.on_candidate_change(&val2, 3000, false);
    assert!(
        alert2.is_none(),
        "same-state (Validated->Validated) should not trigger"
    );
}

// T6: Transition detection logic.
#[test]
fn t6_transition_detection() {
    let mut engine = AlertEngine::new();
    // prev=C2Confirmed -> new=Validated: triggers
    let c2 = make_change("c4", SetupStatus::C2Confirmed);
    engine.on_candidate_change(&c2, 1000, false);
    let val = make_change("c4", SetupStatus::Validated);
    assert!(engine.on_candidate_change(&val, 2000, false).is_some());

    // prev=Validated -> new=Validated: no trigger (same state)
    let val2 = make_change("c4", SetupStatus::Validated);
    assert!(engine.on_candidate_change(&val2, 3000, false).is_none());
}

// T7: AlertRecord has no forbidden fields.
#[test]
fn t7_no_forbidden_fields() {
    let c = make_candidate("c5", SetupStatus::Validated);
    let alert = crate::alert::types::build_alert(&c, 5000, vec![ChannelKind::Inbox]);
    let json = serde_json::to_string(&alert).unwrap();
    let forbidden = [
        "entry_price",
        "sl_price",
        "tp_price",
        "position_size",
        "selected_symbol",
        "targets",
        "risk_reward",
        "invalidation_price",
        "entry_zone",
        "advice",
    ];
    for f in &forbidden {
        assert!(
            !json.contains(f),
            "AlertRecord JSON must not contain '{}'",
            f
        );
    }
    assert_eq!(alert.watchlist_id, "test");

    let smt = make_two_symbol_c2_smt(&c);
    let c2_alert = crate::alert::types::build_c2_alert(
        &c,
        &smt,
        "OANDA:EURUSD",
        5_000,
        vec![ChannelKind::Inbox],
    )
    .expect("C2 alert");
    let c2_json = serde_json::to_string(&c2_alert).unwrap();
    for f in &forbidden {
        assert!(
            !c2_json.contains(f),
            "C2 AlertRecord JSON must not contain '{}'",
            f
        );
    }
}

#[test]
fn m6c_group_dedup_isolated_and_cooldown_is_shared() {
    let path = std::env::temp_dir().join(format!(
        "ict-monitor-alert-m6c-{}.db",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = crate::storage::SqliteStore::open(&path).expect("open test db");
    store.ensure_alert_schema().expect("alert schema");
    let shared = Arc::new(parking_lot::Mutex::new(0));
    let mut group_a = AlertEngine::with_store_and_global_cooldown(store.clone(), shared.clone());
    let mut group_b = AlertEngine::with_store_and_global_cooldown(store.clone(), shared);

    let validated = |watchlist_id: &str, candidate_id: &str| {
        let mut change = make_change(candidate_id, SetupStatus::Validated);
        change.candidate.watchlist_id = watchlist_id.into();
        change.validation_event = Some(SymbolValidation {
            event_id: format!("{watchlist_id}-validation"),
            symbol: "OANDA:USDCHF".into(),
            kind: ReversalConfirmKind::Mss,
            direction: Direction::Bullish,
            ts: 2_000,
            price: 0.82,
        });
        change
    };

    let first = group_a
        .on_candidate_change(&validated("group-a", "same-episode"), 2_000, false)
        .expect("group A fires");
    let second = group_b
        .on_candidate_change(&validated("group-b", "same-episode"), 2_001, false)
        .expect("group B is not deduplicated by group A");
    assert_eq!(first.watchlist_id, "group-a");
    assert_eq!(second.watchlist_id, "group-b");
    assert_ne!(first.id, second.id);

    group_a.set_cooldown_seconds(10);
    group_b.set_cooldown_seconds(10);
    let third = validated("group-a", "cooldown-source");
    assert!(group_a.on_candidate_change(&third, 20_000, false).is_some());
    let fourth = validated("group-b", "cooldown-target");
    assert!(
        group_b
            .on_candidate_change(&fourth, 20_001, false)
            .is_none(),
        "global cooldown must suppress another group's immediate alert",
    );

    let _ = std::fs::remove_file(path);
}

// T9: should_fire respects global cooldown.
#[test]
fn t9_global_cooldown() {
    let mut engine = AlertEngine::new();
    engine.set_cooldown_seconds(10);
    // First call: should fire.
    assert!(engine.should_fire("c6", 1000));
    // Simulate a fired alert.
    let c2 = make_change("c6", SetupStatus::C2Confirmed);
    engine.on_candidate_change(&c2, 1000, false);
    let val = make_change("c6", SetupStatus::Validated);
    engine.on_candidate_change(&val, 2000, false);
    // Within cooldown: should not fire for a different candidate.
    assert!(
        !engine.should_fire("c7", 5000),
        "within cooldown should suppress"
    );
    // After cooldown: should fire.
    assert!(
        engine.should_fire("c7", 15000),
        "after cooldown should allow"
    );
}

// T12: should_fire independent call (M7 seam).
#[test]
fn t12_should_fire_standalone() {
    let engine = AlertEngine::new();
    assert!(engine.should_fire("c8", 1000));
    assert!(engine.should_fire("c9", 1000));
}

// T3: per-candidate dedup (without store, should_fire always true).
#[test]
fn t3_per_candidate_dedup_no_store() {
    let mut engine = AlertEngine::new();
    let c2 = make_change("c10", SetupStatus::C2Confirmed);
    engine.on_candidate_change(&c2, 1000, false);
    let val = make_change("c10", SetupStatus::Validated);
    let alert1 = engine.on_candidate_change(&val, 2000, false);
    assert!(alert1.is_some(), "first Validated should trigger");
    // Second Validated (same state) -> no trigger.
    let val2 = make_change("c10", SetupStatus::Validated);
    let alert2 = engine.on_candidate_change(&val2, 3000, false);
    assert!(alert2.is_none(), "same-state should not re-trigger");
}

#[test]
fn t15_same_candidate_can_alert_once_for_each_fx_symbol() {
    let mut engine = AlertEngine::new();
    let mut eu = make_change("multi", SetupStatus::Validated);
    eu.validation_event = Some(SymbolValidation {
        event_id: "eu-event".into(),
        symbol: "OANDA:EURUSD".into(),
        kind: ReversalConfirmKind::Mss,
        direction: Direction::Bearish,
        ts: 2_000,
        price: 1.08,
    });
    let eu_alert = engine.on_candidate_change(&eu, 2_000, false).unwrap();
    assert_eq!(eu_alert.validation_symbol.as_deref(), Some("OANDA:EURUSD"));

    let mut gu = make_change("multi", SetupStatus::Validated);
    gu.validation_event = Some(SymbolValidation {
        event_id: "gu-event".into(),
        symbol: "OANDA:GBPUSD".into(),
        kind: ReversalConfirmKind::Cisd,
        direction: Direction::Bearish,
        ts: 3_000,
        price: 1.27,
    });
    let gu_alert = engine.on_candidate_change(&gu, 3_000, false).unwrap();
    assert_eq!(gu_alert.validation_symbol.as_deref(), Some("OANDA:GBPUSD"));
    assert_ne!(eu_alert.id, gu_alert.id);
}

#[test]
fn t16_store_dedup_is_per_candidate_and_validation_symbol() {
    let path = std::env::temp_dir().join(format!(
        "ict-monitor-alert-symbol-dedup-{}.db",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = crate::storage::SqliteStore::open(&path).expect("open test db");
    store.ensure_alert_schema().expect("alert schema");
    let mut engine = AlertEngine::with_store(store.clone());

    let validation_change = |symbol: &str, event_id: &str, ts: i64| {
        let mut change = make_change("multi-store", SetupStatus::Validated);
        change.validation_event = Some(SymbolValidation {
            event_id: event_id.into(),
            symbol: symbol.into(),
            kind: ReversalConfirmKind::Mss,
            direction: Direction::Bearish,
            ts,
            price: 1.08,
        });
        change
    };

    let eu = validation_change("OANDA:EURUSD", "eu-1", 2_000);
    assert!(engine.on_candidate_change(&eu, 2_000, false).is_some());
    assert!(
        engine.on_candidate_change(&eu, 2_001, false).is_none(),
        "same Candidate + symbol must be suppressed"
    );

    let gu = validation_change("OANDA:GBPUSD", "gu-1", 3_000);
    assert!(
        engine.on_candidate_change(&gu, 3_000, false).is_some(),
        "the other FX symbol must still be allowed"
    );
    assert_eq!(store.list_alerts(None).unwrap().len(), 2);

    let _ = std::fs::remove_file(path);
}

#[test]
fn t17_store_dedup_is_per_public_smt_episode_across_retry_candidates() {
    let path = std::env::temp_dir().join(format!(
        "ict-monitor-alert-episode-dedup-{}.db",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = crate::storage::SqliteStore::open(&path).expect("open test db");
    store
        .ensure_detector_config_schema()
        .expect("detector config schema");
    store.ensure_candidate_schema().expect("candidate schema");
    store.ensure_alert_schema().expect("alert schema");
    let mut engine = AlertEngine::with_store(store.clone());

    let retry = |id: &str, ts: i64| {
        let mut change = make_change(id, SetupStatus::Validated);
        change.candidate.smt_rule_version = crate::detector::smt::CURRENT_RULE_VERSION.into();
        change.validation_event = Some(SymbolValidation {
            event_id: format!("{id}-event"),
            symbol: "OANDA:EURUSD".into(),
            kind: ReversalConfirmKind::Mss,
            direction: Direction::Bearish,
            ts,
            price: 1.08,
        });
        change
    };
    let first = retry("episode-retry-1", 2_000);
    let second = retry("episode-retry-2", 2_500);
    store.upsert_candidate(&first.candidate, 1_900).unwrap();
    store.upsert_candidate(&second.candidate, 2_400).unwrap();

    assert!(engine.on_candidate_change(&first, 2_000, false).is_some());
    assert!(
        engine.on_candidate_change(&second, 2_500, false).is_none(),
        "a stricter internal retry of the same PDA/HTF episode must not alert twice"
    );

    let _ = std::fs::remove_file(path);
}

// T4: Global cooldown end-to-end (actual Validated transitions suppressed).
#[test]
fn t4_global_cooldown_e2e() {
    let mut engine = AlertEngine::new();
    engine.set_cooldown_seconds(10);
    // First candidate fires.
    let c2a = make_change("gc1", SetupStatus::C2Confirmed);
    engine.on_candidate_change(&c2a, 1000, false);
    let val_a = make_change("gc1", SetupStatus::Validated);
    assert!(
        engine.on_candidate_change(&val_a, 2000, false).is_some(),
        "first should fire"
    );
    // Second candidate within cooldown (2s + 10s*1000ms = 12s window) -> suppressed.
    let c2b = make_change("gc2", SetupStatus::C2Confirmed);
    engine.on_candidate_change(&c2b, 3000, false);
    let val_b = make_change("gc2", SetupStatus::Validated);
    assert!(
        engine.on_candidate_change(&val_b, 5000, false).is_none(),
        "within cooldown suppressed"
    );
    // After cooldown window -> fires.
    let c2c = make_change("gc3", SetupStatus::C2Confirmed);
    engine.on_candidate_change(&c2c, 10000, false);
    let val_c = make_change("gc3", SetupStatus::Validated);
    assert!(
        engine.on_candidate_change(&val_c, 20000, false).is_some(),
        "after cooldown fires"
    );
}

// T8: AlertChannel trait extensible - mock channel dispatched.
#[test]
fn t8_mock_channel_dispatch() {
    use std::sync::Mutex;
    struct MockChannel {
        ch_kind: ChannelKind,
        fired: Arc<Mutex<bool>>,
    }
    impl AlertChannel for MockChannel {
        fn kind(&self) -> ChannelKind {
            self.ch_kind
        }
        fn deliver(&self, _alert: &AlertRecord) -> Result<(), String> {
            *self.fired.lock().unwrap() = true;
            Ok(())
        }
    }
    let mut engine = AlertEngine::new();
    let fired = Arc::new(Mutex::new(false));
    engine.add_channel(Box::new(MockChannel {
        ch_kind: ChannelKind::Inbox,
        fired: fired.clone(),
    }));
    let c2 = make_change("mock1", SetupStatus::C2Confirmed);
    engine.on_candidate_change(&c2, 1000, false);
    let val = make_change("mock1", SetupStatus::Validated);
    let alert = engine.on_candidate_change(&val, 2000, false);
    assert!(alert.is_some());
    assert!(
        *fired.lock().unwrap(),
        "mock channel should have been called"
    );
    assert!(
        alert.unwrap().channels_fired.contains(&ChannelKind::Inbox),
        "channels_fired should record Inbox"
    );
}

// T9: desktop_notify_enabled=false -> DesktopNotify skipped, Inbox still fires.
#[test]
fn t9_desktop_notify_disabled() {
    use std::sync::Mutex;
    struct MockChannel {
        ch_kind: ChannelKind,
        fired: Arc<Mutex<bool>>,
    }
    impl AlertChannel for MockChannel {
        fn kind(&self) -> ChannelKind {
            self.ch_kind
        }
        fn deliver(&self, _alert: &AlertRecord) -> Result<(), String> {
            *self.fired.lock().unwrap() = true;
            Ok(())
        }
    }
    let mut engine = AlertEngine::new();
    engine.set_desktop_notify(false);
    let inbox_fired = Arc::new(Mutex::new(false));
    let desktop_fired = Arc::new(Mutex::new(false));
    engine.add_channel(Box::new(MockChannel {
        ch_kind: ChannelKind::Inbox,
        fired: inbox_fired.clone(),
    }));
    engine.add_channel(Box::new(MockChannel {
        ch_kind: ChannelKind::DesktopNotify,
        fired: desktop_fired.clone(),
    }));
    let c2 = make_change("dn1", SetupStatus::C2Confirmed);
    engine.on_candidate_change(&c2, 1000, false);
    let val = make_change("dn1", SetupStatus::Validated);
    let alert = engine.on_candidate_change(&val, 2000, false);
    assert!(alert.is_some());
    assert!(*inbox_fired.lock().unwrap(), "inbox channel should fire");
    assert!(
        !*desktop_fired.lock().unwrap(),
        "desktop channel should be skipped"
    );
    assert!(
        !alert
            .unwrap()
            .channels_fired
            .contains(&ChannelKind::DesktopNotify),
        "channels_fired should not contain DesktopNotify"
    );
}

#[test]
fn c2_feishu_notify_enabled_adds_feishu_channel() {
    use std::sync::Mutex;
    struct MockChannel {
        ch_kind: ChannelKind,
        fired: Arc<Mutex<bool>>,
    }
    impl AlertChannel for MockChannel {
        fn kind(&self) -> ChannelKind {
            self.ch_kind
        }
        fn deliver(&self, _alert: &AlertRecord) -> Result<(), String> {
            *self.fired.lock().unwrap() = true;
            Ok(())
        }
    }

    let mut candidate = make_candidate("c2-feishu-on", SetupStatus::C2Confirmed);
    candidate.trade_symbols = vec!["OANDA:EURUSD".into()];
    let smt = make_two_symbol_c2_smt(&candidate);
    let mut engine = AlertEngine::new();
    engine.set_feishu_notify(true);
    let inbox_fired = Arc::new(Mutex::new(false));
    let feishu_fired = Arc::new(Mutex::new(false));
    engine.add_channel(Box::new(MockChannel {
        ch_kind: ChannelKind::Inbox,
        fired: inbox_fired.clone(),
    }));
    engine.add_channel(Box::new(MockChannel {
        ch_kind: ChannelKind::FeishuNotify,
        fired: feishu_fired.clone(),
    }));

    let rows = engine.on_c2_confirmed(&candidate, &smt, false);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].channels_fired.contains(&ChannelKind::Inbox));
    assert!(rows[0].channels_fired.contains(&ChannelKind::FeishuNotify));
    assert!(*inbox_fired.lock().unwrap());
    assert!(*feishu_fired.lock().unwrap());
}

#[test]
fn c2_feishu_notify_disabled_skips_feishu_channel() {
    use std::sync::Mutex;
    struct MockChannel {
        ch_kind: ChannelKind,
        fired: Arc<Mutex<bool>>,
    }
    impl AlertChannel for MockChannel {
        fn kind(&self) -> ChannelKind {
            self.ch_kind
        }
        fn deliver(&self, _alert: &AlertRecord) -> Result<(), String> {
            *self.fired.lock().unwrap() = true;
            Ok(())
        }
    }

    let mut candidate = make_candidate("c2-feishu-off", SetupStatus::C2Confirmed);
    candidate.trade_symbols = vec!["OANDA:EURUSD".into()];
    let smt = make_two_symbol_c2_smt(&candidate);
    let mut engine = AlertEngine::new();
    engine.set_feishu_notify(false);
    let feishu_fired = Arc::new(Mutex::new(false));
    engine.add_channel(Box::new(MockChannel {
        ch_kind: ChannelKind::Inbox,
        fired: Arc::new(Mutex::new(false)),
    }));
    engine.add_channel(Box::new(MockChannel {
        ch_kind: ChannelKind::FeishuNotify,
        fired: feishu_fired.clone(),
    }));

    let rows = engine.on_c2_confirmed(&candidate, &smt, false);
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].channels_fired.contains(&ChannelKind::FeishuNotify));
    assert!(!*feishu_fired.lock().unwrap());
}

#[test]
fn validated_alert_never_routes_to_feishu() {
    use std::sync::Mutex;
    struct MockChannel {
        ch_kind: ChannelKind,
        fired: Arc<Mutex<bool>>,
    }
    impl AlertChannel for MockChannel {
        fn kind(&self) -> ChannelKind {
            self.ch_kind
        }
        fn deliver(&self, _alert: &AlertRecord) -> Result<(), String> {
            *self.fired.lock().unwrap() = true;
            Ok(())
        }
    }

    let mut engine = AlertEngine::new();
    engine.set_feishu_notify(true);
    let feishu_fired = Arc::new(Mutex::new(false));
    engine.add_channel(Box::new(MockChannel {
        ch_kind: ChannelKind::Inbox,
        fired: Arc::new(Mutex::new(false)),
    }));
    engine.add_channel(Box::new(MockChannel {
        ch_kind: ChannelKind::FeishuNotify,
        fired: feishu_fired.clone(),
    }));
    let c2 = make_change("validated-no-feishu", SetupStatus::C2Confirmed);
    engine.on_candidate_change(&c2, 1000, true);
    let val = make_change("validated-no-feishu", SetupStatus::Validated);
    let alert = engine.on_candidate_change(&val, 2000, false).unwrap();
    assert!(!alert.channels_fired.contains(&ChannelKind::FeishuNotify));
    assert!(!*feishu_fired.lock().unwrap());
}

// T10: list_alerts + has_fired_for_candidate with a real SQLite store.
#[test]
fn t10_list_alerts_with_store() {
    use crate::storage::SqliteStore;
    let path = std::env::temp_dir().join(format!(
        "ict-monitor-alert-test-{}.db",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = SqliteStore::open(&path).expect("open test db");
    store.ensure_alert_schema().expect("alert schema");

    let c1 = make_candidate("la1", SetupStatus::Validated);
    let alert1 = crate::alert::types::build_alert(&c1, 1000, vec![ChannelKind::Inbox]);
    store.insert_alert(&alert1).expect("insert alert1");

    let mut c2 = make_candidate("la2", SetupStatus::Validated);
    c2.observation_window = (1, 1); // a distinct HTF liquidity episode
    let alert2 = crate::alert::types::build_alert(&c2, 2000, vec![ChannelKind::Inbox]);
    store.insert_alert(&alert2).expect("insert alert2");

    // list all -> DESC by created_at
    let all = store.list_alerts(None).expect("list all");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].candidate_id, "la2");
    assert_eq!(all[1].candidate_id, "la1");
    assert_eq!(all[0].smt_k_candle_ts, Some(2000));
    assert_eq!(all[0].context_pda_id.as_deref(), Some("pda-1"));

    // list with limit
    let limited = store.list_alerts(Some(1)).expect("list limited");
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].candidate_id, "la2");

    // has_fired_for_candidate
    assert!(store.has_fired_for_candidate("la1").expect("has_fired la1"));
    assert!(!store
        .has_fired_for_candidate("unknown")
        .expect("has_fired unknown"));

    // clear_alerts
    store.clear_alerts().expect("clear");
    assert_eq!(store.list_alerts(None).expect("after clear").len(), 0);

    let _ = std::fs::remove_file(path);
}

#[test]
fn t10b_list_alerts_excludes_candidates_from_old_smt_rules() {
    use crate::detector::smt::CURRENT_RULE_VERSION;
    use crate::storage::SqliteStore;
    let path = std::env::temp_dir().join(format!(
        "ict-monitor-alert-rule-filter-{}.db",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = SqliteStore::open(&path).expect("open test db");
    store
        .ensure_detector_config_schema()
        .expect("detector config schema");
    store.ensure_candidate_schema().expect("candidate schema");
    store.ensure_alert_schema().expect("alert schema");

    let mut current = make_candidate("current-alert", SetupStatus::Validated);
    current.smt_rule_version = CURRENT_RULE_VERSION.into();
    store.upsert_candidate(&current, 1_000).unwrap();
    store
        .insert_alert(&crate::alert::types::build_alert(
            &current,
            1_000,
            vec![ChannelKind::Inbox],
        ))
        .unwrap();

    let mut legacy = make_candidate("legacy-alert", SetupStatus::Validated);
    legacy.smt_rule_version = "m6b.v13-legacy".into();
    store.upsert_candidate(&legacy, 2_000).unwrap();
    store
        .insert_alert(&crate::alert::types::build_alert(
            &legacy,
            2_000,
            vec![ChannelKind::Inbox],
        ))
        .unwrap();

    let rows = store.list_alerts(None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].candidate_id, current.id);

    let _ = std::fs::remove_file(path);
}

// T11: AlertEngine param hot-update (set_enabled / set_desktop_notify / set_cooldown).
#[test]
fn t11_alert_params_hot_update() {
    let mut engine = AlertEngine::new();

    // disabled -> no alert
    engine.set_enabled(false);
    let c2 = make_change("sp1", SetupStatus::C2Confirmed);
    engine.on_candidate_change(&c2, 1000, false);
    let val = make_change("sp1", SetupStatus::Validated);
    assert!(
        engine.on_candidate_change(&val, 2000, false).is_none(),
        "disabled should not fire"
    );

    // re-enable -> fires for a new candidate
    engine.set_enabled(true);
    let c2b = make_change("sp2", SetupStatus::C2Confirmed);
    engine.on_candidate_change(&c2b, 3000, false);
    let val_b = make_change("sp2", SetupStatus::Validated);
    assert!(
        engine.on_candidate_change(&val_b, 4000, false).is_some(),
        "re-enabled should fire"
    );

    // set cooldown high -> suppresses subsequent
    engine.set_cooldown_seconds(9999);
    let c2c = make_change("sp3", SetupStatus::C2Confirmed);
    engine.on_candidate_change(&c2c, 5000, false);
    let val_c = make_change("sp3", SetupStatus::Validated);
    assert!(
        engine.on_candidate_change(&val_c, 6000, false).is_none(),
        "high cooldown should suppress"
    );

    engine.set_feishu_notify(true);
    assert!(engine.feishu_notify_enabled());
    engine.set_feishu_notify(false);
    assert!(!engine.feishu_notify_enabled());
}

// T14: A2 regression - channels_fired is non-empty and correct in the
// alert that gets emitted. Previously the alert was built with empty
// channels_fired before delivering to channels.
#[test]
fn t14_channels_fired_non_empty() {
    use std::sync::Mutex;
    struct MockChannel {
        ch_kind: ChannelKind,
        received_channels_fired: Arc<Mutex<Vec<ChannelKind>>>,
    }
    impl AlertChannel for MockChannel {
        fn kind(&self) -> ChannelKind {
            self.ch_kind
        }
        fn deliver(&self, alert: &AlertRecord) -> Result<(), String> {
            *self.received_channels_fired.lock().unwrap() = alert.channels_fired.clone();
            Ok(())
        }
    }
    let mut engine = AlertEngine::new();
    let received = Arc::new(Mutex::new(vec![]));
    engine.add_channel(Box::new(MockChannel {
        ch_kind: ChannelKind::Inbox,
        received_channels_fired: received.clone(),
    }));
    let c2 = make_change("cf1", SetupStatus::C2Confirmed);
    engine.on_candidate_change(&c2, 1000, false);
    let val = make_change("cf1", SetupStatus::Validated);
    let alert = engine.on_candidate_change(&val, 2000, false);
    assert!(alert.is_some());
    let alert = alert.unwrap();
    assert!(
        !alert.channels_fired.is_empty(),
        "channels_fired should be non-empty in the returned alert"
    );
    assert!(
        alert.channels_fired.contains(&ChannelKind::Inbox),
        "channels_fired should contain Inbox"
    );
    // The channel should have received the alert with correct channels_fired
    // (not empty, which was the A2 bug).
    let received = received.lock().unwrap();
    assert!(
        !received.is_empty(),
        "channel should receive alert with non-empty channels_fired"
    );
    assert!(
        received.contains(&ChannelKind::Inbox),
        "channel should see Inbox in channels_fired"
    );
}
