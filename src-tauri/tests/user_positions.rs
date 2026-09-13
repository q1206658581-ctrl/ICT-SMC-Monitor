use ict_monitor::{
    candidate::{DecisionLogEntry, DecisionMode, PackedEvidence},
    llm::{
        pack_stored_evidence, DeterministicDecisionGuardrails, LlmDecisionDirection, LlmEntryZone,
        LlmTarget, M7D_CONTEXT_VERSION, M7D_STRATEGY_VERSION,
    },
    positions::{prices_from_guardrails, PositionPrices, PositionSide},
    storage::SqliteStore,
};
use rusqlite::Connection;
#[path = "../src/backtest/fixture.rs"]
mod fixture;
fn store() -> SqliteStore {
    let p = std::env::temp_dir().join(format!("drawing-test-{:032x}.db", rand::random::<u128>()));
    let s = SqliteStore::open(p).unwrap();
    s.ensure_ict_schema().unwrap();
    s.ensure_smt_schema().unwrap();
    s.ensure_candidate_schema().unwrap();
    s.ensure_alert_schema().unwrap();
    s
}
fn prices() -> PositionPrices {
    PositionPrices {
        side: PositionSide::Long,
        entry_price: 100.0,
        stop_price: 99.0,
        target_price: Some(102.0),
    }
}
fn guardrails() -> DeterministicDecisionGuardrails {
    DeterministicDecisionGuardrails {
        direction: LlmDecisionDirection::Bullish,
        entry_zone: Some(LlmEntryZone {
            low: 100.0,
            high: 101.0,
            source: "program:C2_body".into(),
        }),
        invalidation_price: Some(99.0),
        targets: vec![LlmTarget {
            price: 104.0,
            reason: "program:liquidity".into(),
        }],
        ..Default::default()
    }
}
#[test]
fn d1_crud_isolated_symbols_nullable_target_and_idempotence() {
    let s = store();
    let a = s
        .create_user_position(
            "OANDA:EURUSD",
            prices(),
            Some("source-a".into()),
            Some("5m".into()),
        )
        .unwrap();
    let mut no_target = prices();
    no_target.target_price = None;
    let b = s
        .create_user_position("OANDA:EURUSD", no_target.clone(), None, Some("1h".into()))
        .unwrap();
    s.create_user_position("TVC:DXY", prices(), None, None)
        .unwrap();
    assert_eq!(s.list_user_positions("OANDA:EURUSD").unwrap().len(), 2);
    assert_eq!(s.list_user_positions("TVC:DXY").unwrap().len(), 1);
    let a2 = s.update_user_position(&a.id, no_target).unwrap();
    assert_eq!(a2.prices.target_price, None);
    assert_eq!(a2.source_alert_id, a.source_alert_id);
    assert_eq!(a2.drawn_tf, a.drawn_tf);
    assert!(a2.updated_at_ts > a.updated_at_ts);
    let again = s.update_user_position(&a.id, a2.prices.clone()).unwrap();
    assert_eq!(again, a2);
    s.delete_user_position(&a.id).unwrap();
    s.delete_user_position(&a.id).unwrap();
    assert_eq!(s.list_user_positions("OANDA:EURUSD").unwrap(), vec![b]);
    assert!(s.update_user_position(&a.id, prices()).is_err());
}
#[test]
fn d1_invalid_input_does_not_write() {
    let s = store();
    for (entry, stop, target) in [
        (100.0, 100.0, None),
        (100.0, 101.0, None),
        (100.0, 99.0, Some(98.0)),
        (f64::NAN, 99.0, None),
        (100.0, 99.0, Some(f64::INFINITY)),
    ] {
        assert!(s
            .create_user_position(
                "EUR",
                PositionPrices {
                    side: PositionSide::Long,
                    entry_price: entry,
                    stop_price: stop,
                    target_price: target
                },
                None,
                None
            )
            .is_err());
    }
    assert!(s.list_user_positions("EUR").unwrap().is_empty());
}
fn legacy_snapshot(conn: &Connection) -> Vec<(String, String, Vec<Vec<String>>)> {
    let tables: Vec<(String,String)> = conn.prepare("SELECT name,sql FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name!='user_positions' ORDER BY name").unwrap()
        .query_map([],|r| Ok((r.get(0)?,r.get(1)?))).unwrap().map(Result::unwrap).collect();
    tables
        .into_iter()
        .map(|(name, sql)| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT * FROM \"{}\" ORDER BY rowid",
                    name.replace('"', "\"\"")
                ))
                .unwrap();
            let columns = stmt.column_count();
            let rows = stmt
                .query_map([], |r| {
                    Ok((0..columns)
                        .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                        .collect::<Vec<_>>())
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            (name, sql, rows)
        })
        .collect()
}
#[test]
fn d2_old_database_migration_preserves_all_existing_tables_and_records() {
    let s = store();
    let path = s.path().to_path_buf();
    let (c, smt, a) = fixture::fixture("group", "migration");
    s.upsert_smt(&smt, a.created_at).unwrap();
    s.upsert_candidate(&c, a.created_at).unwrap();
    s.insert_alert(&a).unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute("DROP TABLE user_positions", []).unwrap();
    let before = legacy_snapshot(&conn);
    drop(conn);
    drop(s);
    let s = SqliteStore::open(&path).unwrap();
    s.ensure_user_positions_schema().unwrap();
    let conn = Connection::open(path).unwrap();
    assert_eq!(legacy_snapshot(&conn), before);
    assert!(s.list_user_positions("OANDA:EURUSD").unwrap().is_empty());
}
#[test]
fn d3_snapshot_mapping_ignores_model_opinions_and_preserves_missing_target() {
    let s = store();
    let (c, smt, a) = fixture::fixture("group", "prefill");
    s.insert_alert(&a).unwrap();
    let evidence = PackedEvidence {
        candidate_id: c.id.clone(),
        candidate: c.clone(),
        watchlist_id: a.watchlist_id.clone(),
        alert_id: Some(a.id.clone()),
        trade_symbol: a.validation_symbol.clone(),
        as_of_ts: a.created_at,
        pda_context: smt.htf_pda_ref.clone(),
        liquidity_refs: smt.liquidity_refs.clone(),
        strength: smt.strength.clone(),
        smt,
        ltf_cisd_mss: vec![],
        context_version: M7D_CONTEXT_VERSION.into(),
        strategy_version: M7D_STRATEGY_VERSION.into(),
        market_bars: vec![],
        market_structures: vec![],
    };
    let context = pack_stored_evidence(&Connection::open(s.path()).unwrap(), evidence).unwrap();
    for (absent, wrapped) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut g = guardrails();
        if absent {
            g.targets.clear();
        }
        let mut request = ict_monitor::llm::LlmDecisionRequest {
            context: context.clone(),
        };
        request.context.deterministic_guardrails = g.clone();
        // Use the actual production serializer: serde(transparent) puts the guardrails at root.
        let request_json = if wrapped {
            serde_json::json!({"context": request}).to_string()
        } else {
            serde_json::to_string(&request).unwrap()
        };
        let d = DecisionLogEntry {
            id: format!("decision-{absent}-{wrapped}"),
            candidate_id: c.id.clone(),
            watchlist_id: a.watchlist_id.clone(),
            alert_id: Some(a.id.clone()),
            trade_symbol: a.validation_symbol.clone(),
            parent_id: None,
            provider: "test".into(),
            model: None,
            decision_mode: DecisionMode::LlmSingleCall,
            prompt_version: None,
            strategy_version: M7D_STRATEGY_VERSION.into(),
            context_version: M7D_CONTEXT_VERSION.into(),
            request_json,
            raw_response: Some("provider failed".into()),
            parsed_decision_json: r#"{"entry_zone":{"low":999,"high":1000},"direction":"bearish"}"#
                .into(),
            parse_ok: false,
            error: Some("model unavailable".into()),
            created_at: a.created_at,
        };
        s.insert_decision(&d).unwrap();
        let draft = s.user_position_draft(&a.id, Some(&d.id)).unwrap();
        let expected = prices_from_guardrails(&g).unwrap();
        assert_eq!(
            serde_json::to_vec(&draft.prices).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        assert_eq!(draft.prices.entry_price, 100.5);
        assert_eq!(draft.source_alert_id, a.id);
        assert_eq!(
            draft.prices.target_price,
            if absent { None } else { Some(104.0) }
        );
        // Alert Inbox uses lookup by alert, rather than an explicit decision ID.
        assert!(s.user_position_draft(&a.id, None).is_ok());
        let missing = DecisionLogEntry {
            id: format!("missing-{absent}-{wrapped}"),
            request_json: "{}".into(),
            ..d
        };
        s.insert_decision(&missing).unwrap();
        assert!(s
            .user_position_draft(&a.id, Some(&missing.id))
            .unwrap_err()
            .to_string()
            .contains("缺少确定性护栏快照"));
        // Keep missing fixtures out of the automatic latest-decision lookup in later iterations.
        Connection::open(s.path())
            .unwrap()
            .execute("DELETE FROM decision_log WHERE id=?1", [&missing.id])
            .unwrap();
    }
    assert!(
        s.list_user_positions("OANDA:EURUSD").unwrap().is_empty(),
        "draft preparation must not write"
    );
}
#[test]
fn d3_no_decision_reuses_m8_shared_packer_at_alert_anchor() {
    let s = store();
    let (c, smt, a) = fixture::fixture("group", "recompute");
    s.upsert_smt(&smt, a.created_at).unwrap();
    s.upsert_candidate(&c, a.created_at).unwrap();
    s.insert_alert(&a).unwrap();
    let evidence = PackedEvidence {
        candidate_id: c.id.clone(),
        candidate: c,
        watchlist_id: a.watchlist_id.clone(),
        alert_id: Some(a.id.clone()),
        trade_symbol: a.validation_symbol.clone(),
        as_of_ts: a.created_at,
        pda_context: smt.htf_pda_ref.clone(),
        liquidity_refs: smt.liquidity_refs.clone(),
        strength: smt.strength.clone(),
        smt,
        ltf_cisd_mss: vec![],
        context_version: M7D_CONTEXT_VERSION.into(),
        strategy_version: M7D_STRATEGY_VERSION.into(),
        market_bars: vec![],
        market_structures: vec![],
    };
    let conn = Connection::open(s.path()).unwrap();
    let g = pack_stored_evidence(&conn, evidence)
        .unwrap()
        .deterministic_guardrails;
    let draft = s.user_position_draft(&a.id, None).unwrap();
    assert_eq!(
        serde_json::to_vec(&draft.prices).unwrap(),
        serde_json::to_vec(&prices_from_guardrails(&g).unwrap()).unwrap()
    );
    assert_eq!(draft.prices.side, PositionSide::Short);
    let source = include_str!("../src/backtest/source.rs");
    assert!(source.contains("ict_monitor::llm::pack_stored_evidence(conn, evidence)"));
}
#[test]
fn d5_manual_position_fields_never_enter_alert_event_or_notification_payloads() {
    for source in [
        include_str!("../src/alert/types.rs"),
        include_str!("../src/alert/channels.rs"),
        include_str!("../src/alert/mod.rs"),
    ] {
        for field in [
            "user_positions",
            "source_alert_id",
            "entry_price",
            "stop_price",
            "target_price",
        ] {
            assert!(!source.contains(field), "unexpected {field}");
        }
    }
    let (_, _, alert) = fixture::fixture("group", "boundary");
    let json = serde_json::to_string(&alert).unwrap();
    for field in [
        "source_alert_id",
        "entry_price",
        "stop_price",
        "target_price",
    ] {
        assert!(!json.contains(field));
    }
    let s = store();
    s.insert_alert(&alert).unwrap();
    let conn = Connection::open(s.path()).unwrap();
    let before = legacy_snapshot(&conn);
    let p = s
        .create_user_position("OANDA:EURUSD", prices(), Some(alert.id.clone()), None)
        .unwrap();
    let mut next = prices();
    next.entry_price = 100.5;
    s.update_user_position(&p.id, next).unwrap();
    s.delete_user_position(&p.id).unwrap();
    assert_eq!(legacy_snapshot(&conn), before);
}
#[test]
fn d7_reopen_restores_complete_positions_across_timeframes() {
    let s = store();
    let path = s.path().to_path_buf();
    let a = s
        .create_user_position(
            "OANDA:EURUSD",
            prices(),
            Some("alert-1".into()),
            Some("5m".into()),
        )
        .unwrap();
    let mut short = prices();
    short.side = PositionSide::Short;
    short.stop_price = 101.0;
    short.target_price = None;
    let b = s
        .create_user_position("OANDA:EURUSD", short, None, Some("1h".into()))
        .unwrap();
    drop(s);
    let reopened = SqliteStore::open(path).unwrap();
    let rows = reopened.list_user_positions("OANDA:EURUSD").unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.contains(&a));
    assert!(rows.contains(&b));
}

#[test]
fn historical_anchor_migrates_legacy_positions_and_survives_price_edits_and_reopen() {
    let s = store();
    let path = s.path().to_path_buf();
    let (_, _, mut alert) = fixture::fixture("group", "anchor");
    alert.created_at += 3_600_000; // insertion time must not become the C2 anchor
    s.insert_alert(&alert).unwrap();
    let expected = Some(alert.c2_candle_ts + alert.comparison_timeframe.duration_ms());
    let historical = s
        .create_user_position(
            "OANDA:EURUSD",
            prices(),
            Some(alert.id.clone()),
            Some("5m".into()),
        )
        .unwrap();
    assert_eq!(historical.anchor_ts, expected);
    let manual = s
        .create_user_position("OANDA:EURUSD", prices(), None, Some("5m".into()))
        .unwrap();
    assert_eq!(manual.anchor_ts, None);
    let conn = Connection::open(&path).unwrap();
    let before = legacy_snapshot(&conn);
    conn.execute("ALTER TABLE user_positions DROP COLUMN anchor_ts", [])
        .unwrap();
    drop(conn);
    drop(s);
    let reopened = SqliteStore::open(&path).unwrap();
    let rows = reopened.list_user_positions("OANDA:EURUSD").unwrap();
    assert!(rows.contains(&historical));
    assert!(rows.contains(&manual));
    assert_eq!(legacy_snapshot(&Connection::open(&path).unwrap()), before);
    let mut edited = historical.prices.clone();
    edited.entry_price += 0.1;
    assert_eq!(
        reopened
            .update_user_position(&historical.id, edited)
            .unwrap()
            .anchor_ts,
        expected
    );
    reopened.ensure_user_positions_schema().unwrap();
    assert_eq!(
        reopened
            .list_user_positions("OANDA:EURUSD")
            .unwrap()
            .iter()
            .find(|p| p.id == historical.id)
            .unwrap()
            .anchor_ts,
        expected
    );
}
