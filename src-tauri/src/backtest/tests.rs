use super::*;
use super::{model::*, simulator::simulate};
use ict_monitor::{
    candidate::PackedEvidence,
    llm::{
        ContextPacker, DeterministicDecisionGuardrails, LlmDecisionDirection, LlmEntryZone,
        LlmTarget,
    },
    storage::SqliteStore,
    types::{Bar, Timeframe},
};
#[path = "fixture.rs"]
mod fixture;

fn g() -> DeterministicDecisionGuardrails {
    DeterministicDecisionGuardrails {
        direction: LlmDecisionDirection::Bullish,
        entry_zone: Some(LlmEntryZone {
            low: 100.0,
            high: 101.0,
            source: "test".into(),
        }),
        invalidation_price: Some(99.0),
        targets: vec![LlmTarget {
            price: 105.0,
            reason: "test".into(),
        }],
        ..Default::default()
    }
}
fn bar(ts: i64, low: f64, high: f64, close: f64) -> Bar {
    Bar {
        symbol: "OANDA:EURUSD".into(),
        tf: Timeframe::M1,
        ts,
        open: close,
        high,
        low,
        close,
        volume: 1.0,
    }
}

#[test]
fn l2_immediate_entry_target_rr_and_time() {
    let s = simulate(&g(), 0, 100.0, &[bar(0, 100.0, 105.0, 104.0)], 2, 2);
    assert_eq!(s.outcome, Outcome::Win);
    assert_eq!(s.entry, Some(100.0));
    assert_eq!(s.realized_rr, Some(5.0));
    assert_eq!(s.time_to_entry_ms, Some(0));
    assert_eq!(s.time_to_target_ms, Some(MINUTE));
}
#[test]
fn l2_boundary_fill_then_target() {
    let s = simulate(
        &g(),
        0,
        103.0,
        &[
            bar(0, 100.5, 104.0, 102.0),
            bar(MINUTE, 102.0, 105.0, 104.0),
        ],
        2,
        2,
    );
    assert_eq!(s.entry, Some(101.0));
    assert_eq!(s.realized_rr, Some(2.0));
    assert_eq!(s.time_to_entry_ms, Some(MINUTE));
}
#[test]
fn l2_stop_before_entry_is_skip() {
    let s = simulate(&g(), 0, 103.0, &[bar(0, 98.0, 99.0, 98.5)], 2, 2);
    assert_eq!(s.outcome, Outcome::Skip);
    assert_eq!(s.entry, None);
}
#[test]
fn l2_same_bar_entry_stop_is_ambiguous_unfilled() {
    let s = simulate(&g(), 0, 103.0, &[bar(0, 98.0, 104.0, 102.0)], 2, 2);
    assert_eq!(s.outcome, Outcome::Ambiguous);
    assert_eq!(s.entry, None);
    assert_eq!(s.realized_rr, None);
}
#[test]
fn l2_window_exhausted_no_fill() {
    assert_eq!(
        simulate(
            &g(),
            0,
            103.0,
            &[
                bar(0, 102.0, 104.0, 103.0),
                bar(MINUTE, 102.0, 104.0, 103.0)
            ],
            2,
            2
        )
        .outcome,
        Outcome::NoFill
    );
}
#[test]
fn l2_stop_after_entry_is_minus_one() {
    let s = simulate(&g(), 0, 100.0, &[bar(0, 98.0, 102.0, 100.0)], 2, 2);
    assert_eq!(s.outcome, Outcome::Loss);
    assert_eq!(s.realized_rr, Some(-1.0));
    assert_eq!(s.time_to_stop_ms, Some(MINUTE));
}
#[test]
fn l2_same_bar_target_stop_conservative_loss() {
    let s = simulate(&g(), 0, 100.0, &[bar(0, 98.0, 106.0, 102.0)], 2, 2);
    assert_eq!(s.outcome, Outcome::Loss);
    assert_eq!(s.realized_rr, Some(-1.0));
    assert_eq!(s.mfe_rr, Some(0.0));
}
#[test]
fn l2_expired_marks_float_not_realized() {
    let s = simulate(
        &g(),
        0,
        100.0,
        &[bar(0, 99.5, 102.0, 101.0), bar(MINUTE, 99.5, 104.0, 103.0)],
        2,
        2,
    );
    assert_eq!(s.outcome, Outcome::Expired);
    assert_eq!(s.realized_rr, None);
    assert_eq!(s.floating_rr, Some(3.0));
    assert_eq!(s.mfe_rr, Some(4.0));
    assert_eq!(s.mae_rr, Some(0.5));
}
#[test]
fn l2_entry_bar_target_is_not_awarded_before_known_fill() {
    let s = simulate(
        &g(),
        0,
        103.0,
        &[
            bar(0, 100.0, 106.0, 102.0),
            bar(MINUTE, 102.0, 103.0, 102.0),
        ],
        2,
        1,
    );
    assert_eq!(s.outcome, Outcome::Expired);
    assert_eq!(s.floating_rr, Some(0.5));
}
#[test]
fn bearish_boundary_and_rr_are_symmetric() {
    let mut guard = g();
    guard.direction = LlmDecisionDirection::Bearish;
    guard.invalidation_price = Some(102.0);
    guard.targets[0].price = 96.0;
    let s = simulate(
        &guard,
        0,
        98.0,
        &[bar(0, 98.0, 100.5, 100.0), bar(MINUTE, 96.0, 100.0, 97.0)],
        2,
        2,
    );
    assert_eq!(s.entry, Some(100.0));
    assert_eq!(s.outcome, Outcome::Win);
    assert_eq!(s.realized_rr, Some(2.0));
}
#[test]
fn gaps_and_tail_are_not_fabricated_outcomes() {
    assert_eq!(
        simulate(&g(), 0, 100.0, &[], 2, 2).outcome,
        Outcome::InsufficientData
    );
    assert_eq!(
        simulate(&g(), 0, 100.0, &[bar(MINUTE, 99.5, 106.0, 100.0)], 2, 2).outcome,
        Outcome::DataGap
    );
    assert_eq!(
        simulate(&g(), 0, 103.0, &[bar(0, 102.0, 104.0, 103.0)], 2, 2).outcome,
        Outcome::InsufficientData
    );
}
#[test]
fn l1_simulator_ignores_all_pre_anchor_bars() {
    let future = bar(MINUTE, 100.0, 105.0, 102.0);
    let expected = simulate(&g(), MINUTE, 100.0, &[future.clone()], 2, 2);
    for i in 1..100 {
        let past = bar(MINUTE - i * MINUTE, -999.0, 999.0, 0.0);
        assert_eq!(
            simulate(&g(), MINUTE, 100.0, &[past, future.clone()], 2, 2),
            expected
        );
    }
}

fn evidence() -> PackedEvidence {
    let (c, s, a) = fixture::fixture("test-group", "test-candidate");
    PackedEvidence {
        candidate_id: c.id.clone(),
        candidate: c,
        watchlist_id: a.watchlist_id,
        alert_id: Some(a.id),
        trade_symbol: a.validation_symbol,
        as_of_ts: a.created_at,
        pda_context: s.htf_pda_ref.clone(),
        liquidity_refs: s.liquidity_refs.clone(),
        strength: s.strength.clone(),
        smt: s,
        ltf_cisd_mss: vec![],
        context_version: "test".into(),
        strategy_version: "test".into(),
        market_bars: vec![],
        market_structures: vec![],
    }
}
#[test]
fn l1_future_bars_and_c3_cannot_change_guardrails_property() {
    let base = evidence();
    let packer = ContextPacker::default();
    let expected = packer
        .pack_as_of(&base, base.as_of_ts)
        .unwrap()
        .deterministic_guardrails;
    for i in 0..100 {
        let mut mutated = base.clone();
        mutated
            .market_bars
            .push(bar(base.as_of_ts + i * MINUTE, -10000.0, 10000.0, 1000.0));
        for chain in &mut mutated.smt.chains {
            let c3 = chain.c3_candle.as_mut().unwrap();
            c3.low = -10000.0;
            c3.high = 10000.0;
        }
        assert_eq!(
            packer
                .pack_as_of(&mutated, base.as_of_ts)
                .unwrap()
                .deterministic_guardrails,
            expected
        );
        mutated.market_bars.clear();
        assert_eq!(
            packer
                .pack_as_of(&mutated, base.as_of_ts)
                .unwrap()
                .deterministic_guardrails,
            expected
        );
    }
}
#[test]
fn m7_p2_invalid_geometry_has_null_rr() {
    let mut e = evidence();
    let chain = e
        .smt
        .chains
        .iter_mut()
        .find(|c| c.symbol == "OANDA:EURUSD")
        .unwrap();
    // Bearish stop equals body high (zero wick); midpoint risk alone used to
    // misleadingly produce a positive RR despite an invalid boundary.
    chain.smt_k_candle.high = 100.5;
    chain.c2_candle.as_mut().unwrap().high = 100.5;
    let g = ContextPacker::default()
        .pack_as_of(&e, e.as_of_ts)
        .unwrap()
        .deterministic_guardrails;
    assert_eq!(g.risk_reward, None);
    assert!(!g.alert_eligible);
}

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ict-m8-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn seed(path: &Path) -> PackedEvidence {
    let e = evidence();
    let (c, s, a) = fixture::fixture("test-group", "test-candidate");
    let store = SqliteStore::open(path).unwrap();
    store.ensure_ict_schema().unwrap();
    store.ensure_smt_schema().unwrap();
    store.ensure_candidate_schema().unwrap();
    store.ensure_alert_schema().unwrap();
    store.upsert_smt(&s, 0).unwrap();
    store.upsert_candidate(&c, 0).unwrap();
    store.insert_alert(&a).unwrap();
    store
        .insert_bars(&[
            bar(e.as_of_ts - MINUTE, 100.0, 101.0, 100.25),
            bar(e.as_of_ts, 97.5, 100.5, 98.0),
            bar(e.as_of_ts + MINUTE, 97.5, 100.0, 99.0),
        ])
        .unwrap();
    e
}
fn parameters(path: &Path) -> Parameters {
    Parameters {
        version: VERSION.into(),
        source: path.to_string_lossy().into_owned(),
        cohort: "test".into(),
        entry_bars: 2,
        holding_bars: Some(2),
        ttl_mtf_bars: 5,
        mode: ExitMode::Targets,
        guardrail_version: ict_monitor::llm::M7D_STRATEGY_VERSION.into(),
    }
}
#[test]
fn l6_source_context_uses_same_packer_formula() {
    let temp = Temp::new();
    let path = temp.0.join("source.db");
    let e = seed(&path);
    let conn = source::open_source(&path).unwrap();
    let actual = source::pack_evidence(&conn, e.clone()).unwrap();
    let expected = ContextPacker::default().pack_as_of(&e, e.as_of_ts).unwrap();
    assert_eq!(
        actual.deterministic_guardrails,
        expected.deterministic_guardrails
    );
}
#[test]
fn explicit_alert_anchor_does_not_advance_to_future_sweeper_c2() {
    let mut e = evidence();
    e.smt.chains[0].c2_candle.as_mut().unwrap().ts = e.as_of_ts;
    let exact = ContextPacker::default().pack_as_of(&e, e.as_of_ts).unwrap();
    assert_eq!(exact.identity.as_of_ts, e.as_of_ts);
    assert!(exact.l2_strategy_reading.chains[0].c2.is_none());
    assert!(ContextPacker::default().pack(&e).unwrap().identity.as_of_ts > e.as_of_ts);
}
#[test]
fn l3_l4_l5_deterministic_cohort_production_readonly_and_schema_isolation() {
    let temp = Temp::new();
    let source_path = temp.0.join("source.db");
    seed(&source_path);
    let before = std::fs::read(&source_path).unwrap();
    let eval = temp.0.join("eval.db");
    let p = parameters(&source_path);
    let first = run(p.clone(), &eval).unwrap();
    assert_eq!(first.c2_count, 1);
    assert_eq!(first.trades[0].simulation.outcome, Outcome::Win);
    let second = run(p.clone(), &eval).unwrap();
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap()
    );
    assert_eq!(report::markdown(&first), report::markdown(&second));
    assert_eq!(report::csv(&first), report::csv(&second));
    assert_eq!(before, std::fs::read(&source_path).unwrap());
    let readonly = source::open_source(&source_path).unwrap();
    assert!(readonly.execute("DELETE FROM bars", []).is_err());
    assert_eq!(
        readonly
            .query_row("SELECT total_changes()", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    let schemas: String = readonly
        .query_row("SELECT group_concat(sql) FROM sqlite_master", [], |r| {
            r.get(0)
        })
        .unwrap();
    for field in [
        "eval_runs",
        "eval_trades",
        "user_v1",
        "target1_in_user_r",
        "buffer_points",
        "realized_rr",
        "floating_rr",
        "mfe_rr",
        "mae_rr",
    ] {
        assert!(!schemas.contains(field));
    }
    let e = evidence();
    let (_, _, a) = fixture::fixture("test-group", "test-candidate");
    for json in [
        serde_json::to_string(&e.candidate).unwrap(),
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&e.smt).unwrap(),
    ] {
        for field in [
            "realized_rr",
            "floating_rr",
            "mfe_rr",
            "mae_rr",
            "user_v1",
            "target1_in_user_r",
            "buffer_points",
        ] {
            assert!(!json.contains(field));
        }
    }
    let eval_db = Connection::open(&eval).unwrap();
    assert_eq!(
        eval_db
            .query_row("SELECT COUNT(*) FROM eval_trades", [], |r| r
                .get::<_, usize>(0))
            .unwrap(),
        1
    );
    drop(readonly);
    // Changes in the live source do not silently revise a completed cohort.
    let writable = Connection::open(&source_path).unwrap();
    writable.execute("DELETE FROM bars", []).unwrap();
    drop(writable);
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&run(p, &eval).unwrap()).unwrap()
    );
}
#[test]
fn output_aliases_cannot_write_production() {
    let temp = Temp::new();
    let path = temp.0.join("source.db");
    seed(&path);
    assert!(validate_paths(&path, &path, &temp.0.join("reports")).is_err());
    #[cfg(unix)]
    {
        let link = temp.0.join("alias.db");
        std::fs::hard_link(&path, &link).unwrap();
        assert!(validate_paths(&path, &link, &temp.0.join("reports")).is_err());
    }
}
#[test]
fn dst_aware_session_and_small_sample_reporting() {
    let jan = chrono::DateTime::parse_from_rfc3339("2026-01-05T12:00:00Z")
        .unwrap()
        .timestamp_millis();
    let jul = chrono::DateTime::parse_from_rfc3339("2026-07-06T11:00:00Z")
        .unwrap()
        .timestamp_millis();
    assert_eq!(source::time_buckets(jan).unwrap().0, "NY");
    assert_eq!(source::time_buckets(jul).unwrap().0, "NY");
    let temp = Temp::new();
    let path = temp.0.join("source.db");
    seed(&path);
    let result = run(parameters(&path), &temp.0.join("eval.db")).unwrap();
    assert!(report::markdown(&result).contains("小样本，不可下结论"));
    assert!(report::markdown(&result).contains("不能判断"));
}

#[test]
fn l1_deleting_future_source_m1_and_structures_preserves_context() {
    use ict_monitor::detector::types::{Direction, IctStructure, Mss};
    let temp = Temp::new();
    let path = temp.0.join("source.db");
    let e = seed(&path);
    let future = IctStructure::Mss(Mss {
        id: "future-mss".into(),
        symbol: "OANDA:EURUSD".into(),
        tf: Timeframe::M5,
        direction: Direction::Bearish,
        break_ts: e.as_of_ts + MINUTE,
        break_price: 1.0,
        swing_ts: e.as_of_ts,
        swing_price: 1.0,
    });
    let writable = Connection::open(&path).unwrap();
    writable.execute("INSERT INTO ict_structures(id,kind,symbol,tf,state,ts_created,ts_updated,payload_json) VALUES('future-mss','mss','OANDA:EURUSD','5m','active',0,0,?1)",[serde_json::to_string(&future).unwrap()]).unwrap();
    drop(writable);
    let before = {
        let conn = source::open_source(&path).unwrap();
        source::pack_evidence(&conn, e.clone()).unwrap()
    };
    let writable = Connection::open(&path).unwrap();
    writable
        .execute("DELETE FROM bars WHERE ts>=?1", [e.as_of_ts])
        .unwrap();
    writable
        .execute("DELETE FROM ict_structures WHERE id='future-mss'", [])
        .unwrap();
    drop(writable);
    let after = {
        let conn = source::open_source(&path).unwrap();
        source::pack_evidence(&conn, e.clone()).unwrap()
    };
    assert_eq!(
        serde_json::to_string(&before).unwrap(),
        serde_json::to_string(&after).unwrap()
    );
    assert_eq!(
        before.deterministic_guardrails,
        ContextPacker::default()
            .pack(&e)
            .unwrap()
            .deterministic_guardrails
    );
}

#[test]
fn fresh_evaluations_are_byte_deterministic_and_cohorts_refresh_explicitly() {
    let temp = Temp::new();
    let path = temp.0.join("source.db");
    seed(&path);
    let p = parameters(&path);
    let first = run(p.clone(), &temp.0.join("a.db")).unwrap();
    let second = run(p.clone(), &temp.0.join("b.db")).unwrap();
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap()
    );
    assert_eq!(report::markdown(&first), report::markdown(&second));
    assert_eq!(report::csv(&first), report::csv(&second));
    let writable = Connection::open(&path).unwrap();
    writable.execute("DELETE FROM bars", []).unwrap();
    drop(writable);
    let mut fresh = p;
    fresh.cohort = "updated".into();
    let third = run(fresh, &temp.0.join("a.db")).unwrap();
    assert_ne!(third.run_id, first.run_id);
    assert_eq!(
        third.trades[0].simulation.outcome,
        Outcome::InsufficientData
    );
}

#[test]
fn report_metrics_exclude_unfilled_and_expired_rr() {
    let temp = Temp::new();
    let path = temp.0.join("source.db");
    seed(&path);
    let base = run(parameters(&path), &temp.0.join("eval.db"))
        .unwrap()
        .trades
        .remove(0);
    let mut win = base.clone();
    win.simulation = simulate(&g(), 0, 100.0, &[bar(0, 100.0, 105.0, 104.0)], 2, 2);
    let mut loss = base.clone();
    loss.simulation = simulate(&g(), 0, 100.0, &[bar(0, 98.0, 100.0, 99.0)], 2, 2);
    let mut expired = base.clone();
    expired.simulation = simulate(&g(), 0, 100.0, &[bar(0, 100.0, 104.0, 103.0)], 2, 1);
    let mut skip = base;
    skip.simulation = Simulation::empty(Outcome::Skip, "test");
    let stats = report::stats(&[&win, &loss, &expired, &skip]);
    assert_eq!(stats.n, 4);
    assert_eq!(stats.resolved, 2);
    assert_eq!(stats.wins, 1);
    assert_eq!(stats.mean, Some(2.0));
    assert_eq!(stats.median, Some(2.0));
    assert_eq!(stats.profit_factor, Some(5.0));
    assert_eq!(stats.floating, Some(3.0));
    assert_eq!(stats.expired_n, 1);
    assert_eq!(stats.excursions_n, 3);
}

#[test]
fn malformed_geometry_and_ohlc_do_not_generate_returns() {
    let mut guard = g();
    guard.invalidation_price = Some(100.5);
    assert_eq!(
        simulate(&guard, 0, 100.0, &[], 2, 2).outcome,
        Outcome::Excluded
    );
    let mut invalid = bar(0, 100.0, 101.0, 100.5);
    invalid.open = 110.0;
    assert_eq!(
        simulate(&g(), 0, 100.0, &[invalid], 2, 2).outcome,
        Outcome::DataGap
    );
}

#[test]
fn fixed_r_actual_fill_and_independent_exits() {
    use super::simulator::simulate_exit;
    let mut guard = g();
    guard.targets.clear();
    let path = [bar(0, 100.0, 101.2, 100.5), bar(MINUTE, 98.0, 100.5, 99.0)];
    let results: Vec<_> = (1..=3)
        .map(|r| simulate_exit(&guard, 0, 100.0, &path, 2, 2, Some(r as f64)))
        .collect();
    assert_eq!(results[0].outcome, Outcome::Win);
    assert_eq!(results[0].realized_rr, Some(1.0));
    for s in &results[1..] {
        assert_eq!(s.outcome, Outcome::Loss);
        assert_eq!(s.realized_rr, Some(-1.0));
        assert_eq!(s.entry, results[0].entry);
        assert_eq!(s.entry_ts, results[0].entry_ts);
    }
    let boundary = [
        bar(0, 100.5, 104.0, 102.0),
        bar(MINUTE, 101.5, 102.7, 102.0),
    ];
    let s = simulate_exit(&guard, 0, 103.0, &boundary, 2, 1, Some(1.0));
    assert_eq!(s.entry, Some(101.0));
    assert_eq!(s.outcome, Outcome::Expired); // 1R target=103, not 102.
}

#[test]
fn fixed_r_ambiguous_double_touch_gap_and_bearish() {
    use super::simulator::simulate_exit;
    for r in 1..=3 {
        let ambiguous = simulate_exit(
            &g(),
            0,
            103.0,
            &[bar(0, 98.0, 110.0, 100.0)],
            2,
            2,
            Some(r as f64),
        );
        assert_eq!(ambiguous.outcome, Outcome::Ambiguous);
        assert!(ambiguous.entry.is_none());
        let loss = simulate_exit(
            &g(),
            0,
            100.0,
            &[bar(0, 98.0, 110.0, 100.0)],
            2,
            2,
            Some(r as f64),
        );
        assert_eq!(loss.outcome, Outcome::Loss);
    }
    let path = [
        bar(0, 100.0, 101.1, 100.5),
        bar(2 * MINUTE, 100.0, 105.0, 102.0),
    ];
    assert_eq!(
        simulate_exit(&g(), 0, 100.0, &path, 2, 3, Some(1.0)).outcome,
        Outcome::Win
    );
    assert_eq!(
        simulate_exit(&g(), 0, 100.0, &path, 2, 3, Some(2.0)).outcome,
        Outcome::DataGap
    );
    let mut guard = g();
    guard.direction = LlmDecisionDirection::Bearish;
    guard.invalidation_price = Some(102.0);
    guard.targets.clear();
    assert_eq!(
        simulate_exit(
            &guard,
            0,
            101.0,
            &[bar(0, 98.0, 101.0, 99.0)],
            2,
            2,
            Some(3.0)
        )
        .realized_rr,
        Some(3.0)
    );
}

#[test]
fn fixed_r_source_without_targets_and_deterministic_report() {
    let temp = Temp::new();
    let path = temp.0.join("source.db");
    seed(&path);
    let conn = Connection::open(&path).unwrap();
    conn.execute("UPDATE smt_divergences SET payload_json=json_set(payload_json,'$.liquidity_refs',json('[]'))",[]).unwrap();
    drop(conn);
    let mut p = parameters(&path);
    p.mode = ExitMode::FixedR;
    let first = run(p.clone(), &temp.0.join("eval.db")).unwrap();
    let second = run(p, &temp.0.join("eval.db")).unwrap();
    assert!(first.trades[0]
        .guardrails
        .as_ref()
        .unwrap()
        .targets
        .is_empty());
    assert_eq!(first.trades[0].fixed_r.len(), 3);
    for (i, r) in first.trades[0].fixed_r.iter().enumerate() {
        assert_eq!(r.simulation.outcome, Outcome::Win);
        assert_eq!(r.simulation.realized_rr, Some((i + 1) as f64));
    }
    assert_eq!(report::csv(&first), report::csv(&second));
    assert_eq!(report::markdown(&first), report::markdown(&second));
    assert_eq!(report::csv(&first).lines().count(), 4);
    assert!(
        report::markdown(&first).contains("非")
            || report::markdown(&first).contains("不代表生产策略")
    );
    assert!(report::markdown(&first).contains("含到期期望"));
}

#[test]
fn l6_v2_source_daily_liquidity_matches_context_packer() {
    use ict_monitor::detector::types::{IctStructure, LevelMarker};
    let temp = Temp::new();
    let path = temp.0.join("source.db");
    let mut e = seed(&path);
    e.smt.liquidity_refs.clear();
    let pdl = IctStructure::Pdl(LevelMarker {
        id: "known-pdl".into(),
        symbol: "OANDA:EURUSD".into(),
        tf: Timeframe::M1,
        price: 98.0,
        label: "PDL".into(),
        confirmed_at_ts: Some(e.as_of_ts),
        valid_from_ts: e.as_of_ts - 86_400_000,
        valid_until_ts: e.as_of_ts,
        source_ts: Some(e.as_of_ts - 60_000),
    });
    let conn = Connection::open(&path).unwrap();
    conn.execute("INSERT INTO ict_structures(id,kind,symbol,tf,state,ts_created,ts_updated,payload_json) VALUES('known-pdl','pdl','OANDA:EURUSD','1m','active',0,0,?1)",[serde_json::to_string(&pdl).unwrap()]).unwrap();
    drop(conn);
    let source_context =
        source::pack_evidence(&source::open_source(&path).unwrap(), e.clone()).unwrap();
    e.market_structures.push(pdl);
    let pure = ContextPacker::default().pack_as_of(&e, e.as_of_ts).unwrap();
    assert_eq!(
        source_context.deterministic_guardrails,
        pure.deterministic_guardrails
    );
    assert_eq!(
        source_context.deterministic_guardrails.targets[0].price,
        98.0
    );
}

#[test]
fn d1_symbol_tail_is_independent_of_other_feeds_and_internal_gaps_remain() {
    let temp = Temp::new();
    let path = temp.0.join("source.db");
    let e = seed(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE bars SET open=100.25, high=100.5, low=100.0, close=100.25 WHERE ts>=?1",
        [e.as_of_ts],
    )
    .unwrap();
    let before = source::evaluate(parameters(&path), "before".into()).unwrap();
    assert_eq!(
        before.trades[0].simulation.outcome,
        Outcome::InsufficientData
    );
    let store = SqliteStore::open(&path).unwrap();
    let mut unrelated = bar(e.as_of_ts + 100 * MINUTE, 100.0, 101.0, 100.25);
    unrelated.symbol = "TVC:DXY".into();
    store.insert_bars(&[unrelated]).unwrap();
    let after = source::evaluate(parameters(&path), "after".into()).unwrap();
    assert!(after.source_cutoff_ts > before.source_cutoff_ts);
    assert_eq!(before.trades[0].simulation, after.trades[0].simulation);
    assert_eq!(after.trades[0].available_future_m1, 1);
    // The newest EURUSD candle could hit target, but remains excluded even
    // when another symbol is much fresher.
    conn.execute(
        "UPDATE bars SET low=97.0 WHERE symbol='OANDA:EURUSD' AND ts=?1",
        [e.as_of_ts + MINUTE],
    )
    .unwrap();
    let forming = source::evaluate(parameters(&path), "forming".into()).unwrap();
    assert_eq!(before.trades[0].simulation, forming.trades[0].simulation);
    // A later candle in the SAME feed proves a hole is internal, not tail.
    conn.execute(
        "DELETE FROM bars WHERE symbol='OANDA:EURUSD' AND ts=?1",
        [e.as_of_ts + MINUTE],
    )
    .unwrap();
    store
        .insert_bars(&[
            bar(e.as_of_ts + 2 * MINUTE, 100.0, 100.5, 100.25),
            bar(e.as_of_ts + 3 * MINUTE, 100.0, 100.5, 100.25),
        ])
        .unwrap();
    let gap = source::evaluate(parameters(&path), "gap".into()).unwrap();
    assert_eq!(gap.trades[0].simulation.outcome, Outcome::DataGap);
}

fn user_c2() -> ict_monitor::detector::types::CandleRef {
    ict_monitor::detector::types::CandleRef {
        ts: 0,
        open: 100.0,
        close: 101.0,
        low: 99.0,
        high: 103.0,
    }
}
#[test]
fn user_immediate_entry_outside_body_and_no_anchor_holding() {
    let guard = g();
    let c2 = user_c2();
    let bars = [bar(0, 0.0, 999.0, 100.0), bar(MINUTE, 102.0, 105.0, 104.0)];
    let original = simulate(&guard, MINUTE, 102.0, &bars, 2, 1);
    let u = simulator::simulate_user(&guard, "OANDA:EURUSD", &c2, MINUTE, 102.0, &bars, 1, 0, 1.0);
    assert_eq!(u.simulation.outcome, Outcome::Win);
    assert_eq!(u.simulation.entry, Some(102.0));
    assert_eq!(u.simulation.entry_ts, Some(MINUTE));
    assert_eq!(u.simulation.time_to_entry_ms, Some(0));
    assert_eq!(u.simulation.exit_ts, Some(2 * MINUTE));
    assert_eq!(u.target, Some(105.0));
    assert_eq!(original.entry, None);
    assert_eq!(simulate(&guard, MINUTE, 102.0, &bars, 2, 1), original);
}
#[test]
fn user_buffers_share_production_precision_and_bearish_mirror() {
    for (symbol, point) in [
        ("OANDA:EURUSD", 0.00001),
        ("TVC:DXY", 0.001),
        ("OANDA:USDJPY", 0.001),
        ("COINBASE:BTCUSD", 0.01),
    ] {
        for buffer in USER_BUFFERS {
            for multiple in USER_MULTIPLES {
                for bull in [true, false] {
                    let mut guard = g();
                    if !bull {
                        guard.direction = LlmDecisionDirection::Bearish;
                    }
                    let u = simulator::simulate_user(
                        &guard,
                        symbol,
                        &user_c2(),
                        MINUTE,
                        101.0,
                        &[],
                        2,
                        buffer,
                        multiple,
                    );
                    let sign = if bull { 1.0 } else { -1.0 };
                    let stop = if bull {
                        99.0 - f64::from(buffer) * point
                    } else {
                        103.0 + f64::from(buffer) * point
                    };
                    assert!((u.stop.unwrap() - stop).abs() < 1e-12);
                    assert!(
                        (u.target.unwrap() - (101.0 + sign * multiple * (101.0 - stop).abs()))
                            .abs()
                            < 1e-12
                    );
                    assert_eq!(u.simulation.entry, Some(101.0));
                    assert_eq!(u.simulation.outcome, Outcome::InsufficientData);
                }
            }
        }
    }
}
#[test]
fn user_degenerate_stop_is_excluded_even_with_buffer() {
    for bull in [true, false] {
        let mut guard = g();
        if !bull {
            guard.direction = LlmDecisionDirection::Bearish;
        }
        for buffer in USER_BUFFERS {
            let price = if bull { 99.0 } else { 103.0 };
            let u = simulator::simulate_user(
                &guard,
                "EURUSD",
                &user_c2(),
                MINUTE,
                price,
                &[],
                2,
                buffer,
                1.0,
            );
            assert_eq!(u.simulation.reason, "degenerate_stop");
            assert_eq!(u.simulation.entry, None);
            assert_eq!(u.target1_in_user_r, None);
        }
    }
}
#[test]
fn user_double_touch_expiry_and_internal_gap() {
    for bull in [true, false] {
        let mut guard = g();
        if !bull {
            guard.direction = LlmDecisionDirection::Bearish;
        }
        let u = simulator::simulate_user(
            &guard,
            "EURUSD",
            &user_c2(),
            MINUTE,
            101.0,
            &[bar(MINUTE, 98.0, 104.0, 101.0)],
            2,
            0,
            1.0,
        );
        assert_eq!(u.simulation.outcome, Outcome::Loss);
        assert_eq!(u.simulation.realized_rr, Some(-1.0));
        let e = simulator::simulate_user(
            &guard,
            "EURUSD",
            &user_c2(),
            MINUTE,
            101.0,
            &[bar(MINUTE, 100.5, 101.5, 101.5)],
            1,
            0,
            1.5,
        );
        assert_eq!(e.simulation.outcome, Outcome::Expired);
        assert_eq!(
            e.simulation.floating_rr,
            Some(if bull { 0.25 } else { -0.25 })
        );
        let gap = simulator::simulate_user(
            &guard,
            "EURUSD",
            &user_c2(),
            MINUTE,
            101.0,
            &[bar(2 * MINUTE, 98.0, 104.0, 101.0)],
            2,
            0,
            1.0,
        );
        assert_eq!(gap.simulation.outcome, Outcome::DataGap);
    }
}
#[test]
fn user_target_description_is_signed_and_optional() {
    let mut guard = g();
    guard.targets[0].price = 100.0;
    let u = simulator::simulate_user(&guard, "EURUSD", &user_c2(), MINUTE, 101.0, &[], 1, 0, 1.5);
    assert_eq!(u.target1_in_user_r, Some(-0.5));
    guard.targets.clear();
    let u = simulator::simulate_user(&guard, "EURUSD", &user_c2(), MINUTE, 101.0, &[], 1, 0, 1.5);
    assert_eq!(u.target1_in_user_r, None);
    assert_eq!(u.simulation.entry, Some(101.0));
}
#[test]
fn user_grid_same_snapshot_control_readonly_and_cached_reports() {
    let temp = Temp::new();
    let path = temp.0.join("source.db");
    seed(&path);
    let before = std::fs::read(&path).unwrap();
    let mut p = parameters(&path);
    p.mode = ExitMode::UserV1;
    let result = run(p.clone(), &temp.0.join("eval.db")).unwrap();
    assert_eq!(result.trades[0].user_v1.len(), 12);
    let mut baseline = p.clone();
    baseline.mode = ExitMode::FixedR;
    let fixed = source::evaluate(baseline, "control".into()).unwrap();
    assert_eq!(
        serde_json::to_value(&result.trades[0].fixed_r).unwrap(),
        serde_json::to_value(&fixed.trades[0].fixed_r).unwrap()
    );
    for buffer in USER_BUFFERS {
        for r in USER_MULTIPLES {
            let t = &report::project_user(&result, buffer, r).trades[0];
            assert_eq!(t.simulation.entry_ts, Some(t.as_of_ts));
        }
    }
    let csv = report::csv(&result);
    assert_eq!(csv.lines().count(), 16);
    assert!(csv.contains("target1_in_user_R"));
    assert!(report::markdown(&result).contains("expired P25/P50/P75/P90"));
    let second = run(p, &temp.0.join("eval.db")).unwrap();
    assert_eq!(
        serde_json::to_vec(&result).unwrap(),
        serde_json::to_vec(&second).unwrap()
    );
    assert_eq!(csv, report::csv(&second));
    assert_eq!(report::markdown(&result), report::markdown(&second));
    assert_eq!(before, std::fs::read(&path).unwrap());
}
#[test]
fn user_missing_anchor_and_future_c2_never_create_fills() {
    let temp = Temp::new();
    let path = temp.0.join("source.db");
    let e = seed(&path);
    let c = Connection::open(&path).unwrap();
    c.execute("DELETE FROM bars WHERE ts=?1", [e.as_of_ts - MINUTE])
        .unwrap();
    let mut p = parameters(&path);
    p.mode = ExitMode::UserV1;
    let missing = source::evaluate(p.clone(), "missing".into()).unwrap();
    assert_eq!(missing.trades[0].user_v1.len(), 12);
    for u in &missing.trades[0].user_v1 {
        assert_eq!(u.simulation.outcome, Outcome::InsufficientData);
        assert_eq!(u.simulation.reason, "missing_anchor_m1_close");
    }
    SqliteStore::open(&path)
        .unwrap()
        .insert_bars(&[bar(e.as_of_ts - MINUTE, 100.0, 101.0, 100.25)])
        .unwrap();
    let mut smt = e.smt.clone();
    smt.chains
        .iter_mut()
        .find(|c| c.symbol == "OANDA:EURUSD")
        .unwrap()
        .c2_candle
        .as_mut()
        .unwrap()
        .ts = e.as_of_ts;
    SqliteStore::open(&path)
        .unwrap()
        .upsert_smt(&smt, 0)
        .unwrap();
    let future = source::evaluate(p, "future".into()).unwrap();
    for u in &future.trades[0].user_v1 {
        assert_eq!(u.simulation.outcome, Outcome::Excluded);
        assert!(u.simulation.reason.contains("missing_confirmed_trade_c2"));
        assert_eq!(u.simulation.entry, None);
    }
}

#[test]
fn shared_anchor_ignores_one_tick_truncated_body_without_changing_legacy() {
    let mut guard = g();
    guard.entry_zone = Some(LlmEntryZone {
        low: 1.15492,
        high: 1.15497,
        source: "truncated C2".into(),
    });
    guard.invalidation_price = Some(1.1548);
    let c2 = ict_monitor::detector::types::CandleRef {
        ts: 0,
        open: 1.15492,
        close: 1.15498,
        low: 1.1548,
        high: 1.15502,
    };
    let bars = [bar(MINUTE, 1.15494, 1.15520, 1.15510)];
    let legacy = simulator::simulate_exit(&guard, MINUTE, 1.15498, &bars, 2, 2, Some(1.0));
    assert_eq!(legacy.entry, Some(1.15497));
    assert_eq!(legacy.time_to_entry_ms, Some(MINUTE));
    for r in 1..=3 {
        let control = simulator::simulate_anchor(&guard, MINUTE, 1.15498, &bars, 2, r as f64);
        let user = simulator::simulate_user(
            &guard, "EURUSD", &c2, MINUTE, 1.15498, &bars, 2, 0, r as f64,
        );
        assert_eq!(control, user.simulation);
        assert_eq!(control.entry, Some(1.15498));
        assert_eq!(control.entry_ts, Some(MINUTE));
        assert_eq!(control.time_to_entry_ms, Some(0));
    }
    assert_eq!(
        simulator::simulate_exit(&guard, MINUTE, 1.15498, &bars, 2, 2, Some(1.0)),
        legacy
    );
}

#[test]
fn source_pair_uses_identical_entry_outside_body_and_exports_explicit_control_mode() {
    let temp = Temp::new();
    let path = temp.0.join("source.db");
    let e = seed(&path);
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE bars SET open=99.75,low=99.5,high=100.0,close=99.75 WHERE ts=?1",
        [e.as_of_ts - MINUTE],
    )
    .unwrap();
    let mut p = parameters(&path);
    p.mode = ExitMode::UserV1;
    let result = source::evaluate(p, "same-entry".into()).unwrap();
    let t = &result.trades[0];
    for sim in t
        .fixed_r
        .iter()
        .map(|r| &r.simulation)
        .chain(t.user_v1.iter().map(|r| &r.simulation))
    {
        assert_eq!(sim.entry, Some(99.75));
        assert_eq!(sim.entry_ts, Some(e.as_of_ts));
        assert_eq!(sim.time_to_entry_ms, Some(0));
    }
    assert!(report::csv(&result).contains("fixed_r_anchor"));
    let md = report::markdown(&result);
    assert!(md.contains("统一锚点收盘立即入场"));
    assert!(!md.contains("在区间内立即成交"));
}
