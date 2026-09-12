//! Unit tests for the M6a Candidate Engine.

use super::*;
use crate::detector::types::IctStructure;
use crate::detector::types::{
    CandleRef, Correlation, Direction, LiquidityRef, LiquidityRefStatus, LiquiditySide, PdaRef,
    ReferenceScope, ReversalConfirmKind, SmtDetectionState, SmtDivergence, SmtInvalidationReason,
    StrengthLabel, StructureEvent, SymbolChain,
};
use crate::types::{Bar, Timeframe};

const MTF_DUR: i64 = 60 * 60 * 1000; // 1h
const TS_BASE: i64 = 1_700_000_000_000;

#[test]
fn positive_correlation_ltf_direction_is_not_flipped() {
    assert_eq!(
        effective_ltf_dir(
            Direction::Bullish,
            "OANDA:USDCHF",
            "TVC:DXY",
            Correlation::Positive,
        ),
        Direction::Bullish,
    );
    assert_eq!(
        effective_ltf_dir(
            Direction::Bearish,
            "OANDA:USDCAD",
            "TVC:DXY",
            Correlation::Positive,
        ),
        Direction::Bearish,
    );
}

#[test]
fn executable_symbols_are_group_driven_not_eu_gu_hard_coded() {
    let mut smt = make_smt(
        "positive-group",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        TS_BASE + 2 * MTF_DUR,
        None,
    );
    smt.sweeper_symbol = "TVC:DXY".into();
    smt.symbol_set = vec![
        "TVC:DXY".into(),
        "OANDA:USDCHF".into(),
        "OANDA:USDCAD".into(),
    ];
    smt.trade_symbols = vec!["OANDA:USDCHF".into(), "OANDA:USDCAD".into()];
    assert_eq!(executable_symbols(&smt), smt.trade_symbols);
}

fn candle(ts: i64, o: f64, h: f64, l: f64, c: f64) -> CandleRef {
    CandleRef {
        ts,
        open: o,
        high: h,
        low: l,
        close: c,
    }
}

fn make_smt(
    id: &str,
    direction: Direction,
    has_pda: bool,
    det_state: SmtDetectionState,
    c2_ts: i64,
    c3_ts: Option<i64>,
) -> SmtDivergence {
    let c1 = candle(TS_BASE, 100.0, 101.0, 99.0, 100.5);
    let smt_k = candle(TS_BASE + MTF_DUR, 99.5, 100.5, 98.5, 99.0);
    let c2 = candle(c2_ts, 99.0, 100.0, 98.0, 99.5);
    let c3 = c3_ts.map(|ts| candle(ts, 99.5, 100.5, 98.5, 100.0));

    let chain = SymbolChain {
        symbol: "TVC:DXY".into(),
        c1_candle: c1,
        smt_k_candle: smt_k,
        c2_candle: Some(c2),
        c2_case: Some(1),
        c3_candle: c3,
        detection_state: det_state,
    };
    // Counter (EURUSD) chain with compatible price scale (~1.08).
    let eu_c1 = candle(TS_BASE, 1.079, 1.082, 1.078, 1.080);
    let eu_smt_k = candle(TS_BASE + MTF_DUR, 1.081, 1.083, 1.080, 1.082);
    let eu_c2 = candle(c2_ts, 1.080, 1.084, 1.079, 1.082);
    let eu_c3 = c3_ts.map(|ts| candle(ts, 1.081, 1.085, 1.080, 1.083));
    let eu_chain = SymbolChain {
        symbol: "OANDA:EURUSD".into(),
        c1_candle: eu_c1,
        smt_k_candle: eu_smt_k,
        c2_candle: Some(eu_c2),
        c2_case: Some(1),
        c3_candle: eu_c3,
        detection_state: det_state,
    };

    let pda = if has_pda {
        Some(PdaRef {
            kind: "fvg".into(),
            id: "pda-test-1".into(),
            tf: Timeframe::H4,
            direction,
            price_low: 98.0,
            price_high: 101.0,
            ts_open: TS_BASE - MTF_DUR,
            ts_confirm: TS_BASE,
            exit_ts: None,
            ts_filled: None,
        })
    } else {
        None
    };

    SmtDivergence {
        id: id.into(),
        watchlist_id: "test-wl".into(),
        rule_version: "m5c2.v5".into(),
        symbol_set: vec!["TVC:DXY".into(), "OANDA:EURUSD".into()],
        relationship: Correlation::Negative,
        context_timeframe: Timeframe::H4,
        comparison_timeframe: Timeframe::H1,
        observation_window: (TS_BASE - MTF_DUR, TS_BASE + 5 * MTF_DUR),
        htf_confirmed: true,
        reference_scope: ReferenceScope::DistantLeftSide,
        liquidity_refs: vec![
            LiquidityRef {
                symbol: "TVC:DXY".into(),
                ref_price: 99.0,
                ref_ts: TS_BASE,
                side: LiquiditySide::SellSide,
                status: LiquidityRefStatus::Swept,
                tf: Timeframe::H4,
                mtf_ref_candle: None,
                mtf_sweep_candle: None,
            },
            LiquidityRef {
                symbol: "OANDA:EURUSD".into(),
                ref_price: 1.08,
                ref_ts: TS_BASE,
                side: LiquiditySide::BuySide,
                status: LiquidityRefStatus::NotSwept,
                tf: Timeframe::H4,
                mtf_ref_candle: None,
                mtf_sweep_candle: None,
            },
        ],
        confluence_refs: Vec::new(),
        candidate_direction: direction,
        sweeper_symbol: "TVC:DXY".into(),
        trade_symbols: vec!["OANDA:EURUSD".into()],
        strength: vec![StrengthLabel {
            symbol: "OANDA:EURUSD".into(),
            label: "strong".into(),
        }],
        chains: vec![chain, eu_chain],
        invalidation_reasons: Vec::new(),
        invalidation_ts: None,
        htf_pda_ref: pda,
        mtf_ref_candle: None,
    }
}

/// Direction convention for LTF events on counter symbols (EURUSD):
/// `candidate_direction` in `make_smt` is the DXY (sweeper) direction.
/// EURUSD has `Correlation::Negative`, so its effective trade direction
/// is the FLIP of DXY's. A Bullish-DXY candidate means a Bearish-EURUSD
/// trade. So EURUSD LTF events that VALIDATE must use the flipped
/// (Bearish) direction, and events that INVALIDATE use the original
/// (Bullish) direction.
fn ltf_event(
    symbol: &str,
    kind: ReversalConfirmKind,
    direction: Direction,
    ts: i64,
) -> LtfEventRef {
    LtfEventRef {
        id: format!("ltf-{}", ts),
        symbol: symbol.into(),
        kind,
        direction,
        ts,
        price: 1.08,
    }
}

fn make_bar(symbol: &str, ts: i64, close: f64) -> Bar {
    Bar {
        symbol: symbol.into(),
        tf: Timeframe::M5,
        ts,
        open: close,
        high: close + 0.001,
        low: close - 0.001,
        close,
        volume: 1.0,
    }
}

fn smt_event(smt: &SmtDivergence) -> StructureEvent {
    StructureEvent::New(IctStructure::SmtDivergence(smt.clone()))
}

fn smt_update(smt: &SmtDivergence) -> StructureEvent {
    StructureEvent::Update(IctStructure::SmtDivergence(smt.clone()))
}

// T1: SMT C2Confirmed + in_pda -> candidate generated.
#[test]
fn t01_spawn_on_c2_confirmed_with_pda() {
    let mut engine = CandidateEngine::new();
    let smt = make_smt(
        "smt-1",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        TS_BASE + 2 * MTF_DUR,
        None,
    );
    let changes = engine.on_smt_event(&smt_event(&smt), TS_BASE + 2 * MTF_DUR);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].candidate.setup_status, SetupStatus::C2Confirmed);
    assert_eq!(changes[0].candidate.smt_id, "smt-1");
    assert!(changes[0].candidate.context_pda_id.is_some());
    assert_eq!(changes[0].candidate.c2_case, 1);
    assert_eq!(changes[0].candidate.trade_symbols, vec!["OANDA:EURUSD"]);
}

// T2: SMT C2Confirmed + no PDA -> no candidate.
#[test]
fn t02_no_spawn_without_pda() {
    let mut engine = CandidateEngine::new();
    let smt = make_smt(
        "smt-2",
        Direction::Bullish,
        false,
        SmtDetectionState::C2Confirmed,
        TS_BASE + 2 * MTF_DUR,
        None,
    );
    let changes = engine.on_smt_event(&smt_event(&smt), TS_BASE + 2 * MTF_DUR);
    assert!(changes.is_empty());
}

// T3: trade_symbol 5m same-direction Cisd (C2 window) -> validated.
#[test]
fn t03_validate_cisd_in_c2_window() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt = make_smt(
        "smt-3",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt), c2_ts);

    // DXY Bullish -> EURUSD trade Bearish (flipped). LTF CISD must be
    // Bearish to validate (same effective direction).
    let ev = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Cisd,
        Direction::Bearish,
        c2_ts + 60_000,
    );
    let bar = make_bar("OANDA:EURUSD", c2_ts + 60_000, 1.082);
    let changes = engine.on_ltf_bar_close("OANDA:EURUSD", &bar, &[ev], c2_ts + 60_000);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].candidate.setup_status, SetupStatus::Validated);
    assert_eq!(
        changes[0].candidate.validation_kind,
        Some(ReversalConfirmKind::Cisd)
    );
    assert_eq!(
        changes[0].candidate.validation_direction,
        Some(Direction::Bearish),
        "Candidate must preserve the actual EURUSD validation direction, not the bullish DXY direction"
    );
    assert!(!changes[0].candidate.c2_cisd_event_ids.is_empty());
}

// T4: trade_symbol 5m same-direction Mss (C3 window) -> validated.
#[test]
fn t04_validate_mss_in_c3_window() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let c3_ts = c2_ts + MTF_DUR;
    // Spawn with C2Confirmed.
    let smt_c2 = make_smt(
        "smt-4",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt_c2), c2_ts);
    // Upgrade to C3Entry.
    let smt_c3 = make_smt(
        "smt-4",
        Direction::Bullish,
        true,
        SmtDetectionState::C3Entry,
        c2_ts,
        Some(c3_ts),
    );
    engine.on_smt_event(&smt_update(&smt_c3), c3_ts);

    // DXY Bullish -> EURUSD trade Bearish (flipped). LTF MSS must be
    // Bearish to validate.
    let ev = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Mss,
        Direction::Bearish,
        c3_ts + 60_000,
    );
    let bar = make_bar("OANDA:EURUSD", c3_ts + 60_000, 1.082);
    let changes = engine.on_ltf_bar_close("OANDA:EURUSD", &bar, &[ev], c3_ts + 60_000);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].candidate.setup_status, SetupStatus::Validated);
    assert_eq!(
        changes[0].candidate.validation_kind,
        Some(ReversalConfirmKind::Mss)
    );
    assert!(!changes[0].candidate.c3_cisd_event_ids.is_empty());
}

// T5: only DXY (sweeper) 5m CISD -> not validated.
#[test]
fn t05_sweeper_cisd_does_not_validate() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt = make_smt(
        "smt-5",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt), c2_ts);

    // DXY is the sweeper, not a trade_symbol.
    let ev = ltf_event(
        "TVC:DXY",
        ReversalConfirmKind::Cisd,
        Direction::Bullish,
        c2_ts + 60_000,
    );
    let bar = make_bar("TVC:DXY", c2_ts + 60_000, 100.5);
    let changes = engine.on_ltf_bar_close("TVC:DXY", &bar, &[ev], c2_ts + 60_000);
    // No changes because DXY is not in trade_symbols.
    assert!(changes
        .iter()
        .all(|c| c.candidate.setup_status != SetupStatus::Validated));
}

// T6: trade_symbol 5m reverse CISD -> invalidated.
#[test]
fn t06_reverse_cisd_invalidates() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt = make_smt(
        "smt-6",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt), c2_ts);

    // DXY Bullish -> EURUSD trade Bearish (flipped). Reverse direction
    // (Bullish on EURUSD) invalidates the candidate.
    let ev = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Cisd,
        Direction::Bullish,
        c2_ts + 60_000,
    );
    // Keep close below EURUSD C2 high so this isolates reverse-CISD logic.
    let bar = make_bar("OANDA:EURUSD", c2_ts + 60_000, 1.082);
    let changes = engine.on_ltf_bar_close("OANDA:EURUSD", &bar, &[ev], c2_ts + 60_000);
    assert!(changes.iter().any(|c| {
        c.candidate.setup_status == SetupStatus::Invalidated
            && c.candidate.expiry_reason == Some(ExpiryReason::Reverse)
            && c.candidate.symbol_invalidation_reasons.get("OANDA:EURUSD")
                == Some(&ExpiryReason::Reverse)
    }));
}

// T7: 5m close breaks C2 low (bullish) -> invalidated (c2_break); wick doesn't.
#[test]
fn t07_c2_break_on_close_not_wick() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt = make_smt(
        "smt-7",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt), c2_ts);

    // DXY Bullish -> EURUSD trade Bearish. C2 break = close > c2_high (1.084).
    // EURUSD C2 high = 1.084. Wick above but close below -> no break.
    let wick_bar = Bar {
        symbol: "OANDA:EURUSD".into(),
        tf: Timeframe::M5,
        ts: c2_ts + 60_000,
        open: 1.082,
        high: 1.087, // wick above C2 high
        low: 1.080,
        close: 1.082, // close below C2 high -> no break
        volume: 1.0,
    };
    let changes = engine.on_ltf_bar_close("OANDA:EURUSD", &wick_bar, &[], c2_ts + 60_000);
    assert!(changes
        .iter()
        .all(|c| c.candidate.setup_status != SetupStatus::Invalidated));

    // Close above C2 high -> break.
    let break_bar = Bar {
        symbol: "OANDA:EURUSD".into(),
        tf: Timeframe::M5,
        ts: c2_ts + 120_000,
        open: 1.083,
        high: 1.088,
        low: 1.082,
        close: 1.086, // close above C2 high (1.084)
        volume: 1.0,
    };
    let changes = engine.on_ltf_bar_close("OANDA:EURUSD", &break_bar, &[], c2_ts + 120_000);
    assert!(changes.iter().any(|c| {
        c.candidate.setup_status == SetupStatus::Invalidated
            && c.candidate.expiry_reason == Some(ExpiryReason::C2Break)
            && c.candidate.symbol_invalidation_reasons.get("OANDA:EURUSD")
                == Some(&ExpiryReason::C2Break)
    }));
}

// T8: C3Entry + N=5 MTF bar pass -> expired (ttl).
#[test]
fn t08_ttl_expiry_after_c3_plus_n_bars() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let c3_ts = c2_ts + MTF_DUR;
    let smt_c2 = make_smt(
        "smt-8",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt_c2), c2_ts);
    let smt_c3 = make_smt(
        "smt-8",
        Direction::Bullish,
        true,
        SmtDetectionState::C3Entry,
        c2_ts,
        Some(c3_ts),
    );
    engine.on_smt_event(&smt_update(&smt_c3), c3_ts);

    // Default expiry = 5 MTF bars. expiry_at = c3_ts + 5 * MTF_DUR.
    let expiry_at = c3_ts + 5 * MTF_DUR;
    // Bar close <= C2 high (1.084) to avoid C2 break; only TTL should fire.
    let bar = make_bar("OANDA:EURUSD", expiry_at, 1.082);
    let changes = engine.on_ltf_bar_close("OANDA:EURUSD", &bar, &[], expiry_at);
    // Candidate was never validated (no LTF reversal) -> TTL => ReCheck
    assert!(changes.iter().any(|c| {
        c.candidate.setup_status == SetupStatus::ReCheck
            && c.candidate.expiry_reason == Some(ExpiryReason::Ttl)
    }));
}

// T9: SMT Invalidated -> candidate invalidated (smt_invalidated).
#[test]
fn t09_smt_invalidation_propagates() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt_c2 = make_smt(
        "smt-9",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt_c2), c2_ts);

    let smt_inv = make_smt(
        "smt-9",
        Direction::Bullish,
        true,
        SmtDetectionState::Invalidated,
        c2_ts,
        None,
    );
    let changes = engine.on_smt_event(&smt_update(&smt_inv), c2_ts + 60_000);
    assert!(changes.iter().any(|c| {
        c.candidate.setup_status == SetupStatus::Invalidated
            && c.candidate.expiry_reason == Some(ExpiryReason::SmtInvalidated)
            && c.candidate.symbol_invalidation_reasons.get("OANDA:EURUSD")
                == Some(&ExpiryReason::SmtInvalidated)
    }));
}

#[test]
fn t09b_terminal_invalidation_event_propagates_by_id() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt = make_smt(
        "smt-9b",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt), c2_ts);

    let changes = engine.on_smt_event(
        &StructureEvent::Invalidated {
            id: "smt-9b".into(),
            kind: "smt_divergence".into(),
        },
        c2_ts + 60_000,
    );
    assert!(changes.iter().any(|c| {
        c.candidate.setup_status == SetupStatus::Invalidated
            && c.candidate.expiry_reason == Some(ExpiryReason::SmtInvalidated)
    }));
}

#[test]
fn t09c_snapshot_invalidation_uses_market_time_for_candidate_and_decision() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let mut smt = make_smt(
        "smt-9c",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt), c2_ts);

    let market_ts = c2_ts + MTF_DUR;
    smt.chains[0].detection_state = SmtDetectionState::Invalidated;
    smt.invalidation_reasons = vec![SmtInvalidationReason::SweeperC3Failed];
    smt.invalidation_ts = Some(market_ts);
    let changes = engine.on_smt_invalidated_at(&smt, market_ts);

    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].candidate.invalidated_at, Some(market_ts));
    assert_eq!(changes[0].decision.created_at, market_ts);
}

// T10: candidate without C3 stays c2_confirmed, expiry_at=None.
#[test]
fn t10_no_ttl_before_c3() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt = make_smt(
        "smt-10",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt), c2_ts);

    let active = engine.list_active();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].setup_status, SetupStatus::C2Confirmed);
    assert!(active[0].expiry_at.is_none());
    assert!(active[0].c3_candle.is_none());
}

// T11: every state change writes a decision log entry.
#[test]
fn t11_decision_log_on_every_change() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt = make_smt(
        "smt-11",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    let changes = engine.on_smt_event(&smt_event(&smt), c2_ts);
    assert_eq!(changes.len(), 1);
    let dec = &changes[0].decision;
    assert_eq!(dec.provider, "deterministic");
    assert!(dec.parse_ok);
    assert_eq!(dec.decision_mode, DecisionMode::Deterministic);
    assert!(dec.parent_id.is_none());
    // No "alert" field in parsed_decision_json.
    assert!(!dec.parsed_decision_json.contains("alert"));
}

// T14: §13 compliance - no forbidden fields in serialized JSON.
#[test]
fn t14_no_forbidden_fields_in_json() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt = make_smt(
        "smt-14",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    let changes = engine.on_smt_event(&smt_event(&smt), c2_ts);
    let cand_json = serde_json::to_string(&changes[0].candidate).unwrap();
    let dec_json = serde_json::to_string(&changes[0].decision).unwrap();
    for forbidden in &[
        "entry_price",
        "sl_price",
        "tp_price",
        "position_size",
        "selected_symbol",
        "risk_reward",
        "invalidation_price",
        "entry_zone",
    ] {
        assert!(
            !cand_json.contains(forbidden),
            "CandidateSetup JSON contains forbidden field: {forbidden}"
        );
        assert!(
            !dec_json.contains(forbidden),
            "DecisionLogEntry JSON contains forbidden field: {forbidden}"
        );
    }
}

// T16: TTL config - set_expiry_mtf_bars(3) takes effect.
#[test]
fn t16_ttl_config_n3() {
    let mut engine = CandidateEngine::new();
    engine.set_expiry_mtf_bars(3);
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let c3_ts = c2_ts + MTF_DUR;
    let smt_c2 = make_smt(
        "smt-16",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt_c2), c2_ts);
    let smt_c3 = make_smt(
        "smt-16",
        Direction::Bullish,
        true,
        SmtDetectionState::C3Entry,
        c2_ts,
        Some(c3_ts),
    );
    engine.on_smt_event(&smt_update(&smt_c3), c3_ts);

    // With N=3, expiry_at = c3_ts + 3 * MTF_DUR.
    let expiry_at = c3_ts + 3 * MTF_DUR;
    // Before expiry: no change. Close <= C2 high (1.084) to avoid C2 break.
    let bar_before = make_bar("OANDA:EURUSD", expiry_at - 1, 1.082);
    let changes_before = engine.on_ltf_bar_close("OANDA:EURUSD", &bar_before, &[], expiry_at - 1);
    assert!(changes_before
        .iter()
        .all(|c| c.candidate.setup_status != SetupStatus::Expired));

    // At expiry: re_check (never validated).
    let bar_at = make_bar("OANDA:EURUSD", expiry_at, 1.082);
    let changes_at = engine.on_ltf_bar_close("OANDA:EURUSD", &bar_at, &[], expiry_at);
    assert!(changes_at
        .iter()
        .any(|c| c.candidate.setup_status == SetupStatus::ReCheck));
}

// T15: cold-start replay rebuilds active candidates.
#[test]
fn t15_replay_rebuilds_active_candidates() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt = make_smt(
        "smt-15",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );

    // DXY Bullish -> EURUSD trade Bearish (flipped). LTF CISD must be
    // Bearish to validate during replay.
    let mut ltf_map = std::collections::HashMap::new();
    ltf_map.insert(
        "OANDA:EURUSD".to_string(),
        vec![ltf_event(
            "OANDA:EURUSD",
            ReversalConfirmKind::Cisd,
            Direction::Bearish,
            c2_ts + 60_000,
        )],
    );

    engine.replay(vec![smt], &ltf_map, &HashMap::new(), c2_ts + 120_000);
    let active = engine.list_active();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].setup_status, SetupStatus::Validated);
    assert_eq!(active[0].validation_kind, Some(ReversalConfirmKind::Cisd));
}

#[test]
fn t15b_replay_c3entry_spawns_candidate() {
    // Regression: C3Entry SMTs must spawn a candidate during replay.
    // Previously, on_smt_event dispatched to handle_c3_entry (which
    // doesn't spawn), so PDA-matched C3Entry SMTs were silently skipped.
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let c3_ts = TS_BASE + 3 * MTF_DUR;
    let smt = make_smt(
        "smt-15b",
        Direction::Bullish,
        true,
        SmtDetectionState::C3Entry,
        c2_ts,
        Some(c3_ts),
    );

    let ltf_map = std::collections::HashMap::new();
    let changes = engine.replay(vec![smt], &ltf_map, &HashMap::new(), c3_ts + 60_000);
    assert!(
        !changes.is_empty(),
        "replay should return changes for C3Entry SMT"
    );
    let active = engine.list_active();
    assert_eq!(active.len(), 1, "C3Entry SMT should spawn a candidate");
    assert!(active[0].c3_candle.is_some(), "C3 candle should be filled");
}

#[test]
fn t15c_replay_orders_cross_symbol_events_by_market_time() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let c3_ts = c2_ts + MTF_DUR;
    let mut smt = make_smt(
        "smt-15c",
        Direction::Bullish,
        true,
        SmtDetectionState::C3Entry,
        c2_ts,
        Some(c3_ts),
    );
    smt.trade_symbols.push("OANDA:GBPUSD".into());
    smt.symbol_set.push("OANDA:GBPUSD".into());
    smt.chains.push(SymbolChain {
        symbol: "OANDA:GBPUSD".into(),
        c1_candle: candle(TS_BASE, 1.279, 1.282, 1.278, 1.280),
        smt_k_candle: candle(TS_BASE + MTF_DUR, 1.281, 1.283, 1.280, 1.282),
        c2_candle: Some(candle(c2_ts, 1.280, 1.284, 1.279, 1.282)),
        c2_case: Some(1),
        c3_candle: Some(candle(c3_ts, 1.281, 1.285, 1.280, 1.283)),
        detection_state: SmtDetectionState::C3Entry,
    });

    let mut ltf_map = std::collections::HashMap::new();
    // EU validates first in market time. GU has a later reverse event.
    // A symbol-by-symbol replay can process GU first and wrongly terminate.
    ltf_map.insert(
        "OANDA:GBPUSD".to_string(),
        vec![ltf_event(
            "OANDA:GBPUSD",
            ReversalConfirmKind::Cisd,
            Direction::Bullish,
            c2_ts + 120_000,
        )],
    );
    ltf_map.insert(
        "OANDA:EURUSD".to_string(),
        vec![ltf_event(
            "OANDA:EURUSD",
            ReversalConfirmKind::Cisd,
            Direction::Bearish,
            c2_ts + 60_000,
        )],
    );

    engine.replay(vec![smt], &ltf_map, &HashMap::new(), c3_ts + 60_000);
    let active = engine.list_active();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].setup_status, SetupStatus::Validated);
    assert_eq!(active[0].validation_symbol.as_deref(), Some("OANDA:EURUSD"));
}

#[test]
fn t15d_extract_ltf_events_is_chronological() {
    let newer = IctStructure::Cisd(crate::detector::types::Cisd {
        id: "newer".into(),
        symbol: "OANDA:EURUSD".into(),
        tf: Timeframe::M5,
        direction: Direction::Bearish,
        leg_origin_ts: TS_BASE,
        leg_origin_price: 1.08,
        break_ts: TS_BASE + 120_000,
        break_price: 1.079,
    });
    let older = IctStructure::Cisd(crate::detector::types::Cisd {
        id: "older".into(),
        symbol: "OANDA:EURUSD".into(),
        tf: Timeframe::M5,
        direction: Direction::Bearish,
        leg_origin_ts: TS_BASE,
        leg_origin_price: 1.08,
        break_ts: TS_BASE + 60_000,
        break_price: 1.0795,
    });
    let events = extract_ltf_events(&[newer, older]);
    assert_eq!(
        events
            .iter()
            .map(|event| event.id.as_str())
            .collect::<Vec<_>>(),
        vec!["older", "newer"]
    );
}

#[test]
fn t18_first_same_dir_event_per_symbol_wins() {
    // One Candidate aggregates symbols, but each symbol contributes only its
    // first valid reversal so later noise cannot duplicate alerts.
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let c3_ts = c2_ts + MTF_DUR;
    let smt_c2 = make_smt(
        "smt-18",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt_c2), c2_ts);
    let smt_c3 = make_smt(
        "smt-18",
        Direction::Bullish,
        true,
        SmtDetectionState::C3Entry,
        c2_ts,
        Some(c3_ts),
    );
    engine.on_smt_event(&smt_update(&smt_c3), c3_ts);

    // DXY Bullish -> EURUSD trade Bearish (flipped). Both events must be
    // Bearish to validate + accumulate.
    let ev1 = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Cisd,
        Direction::Bearish,
        c2_ts + 60_000,
    );
    let bar1 = make_bar("OANDA:EURUSD", c2_ts + 60_000, 1.082);
    let changes1 = engine.on_ltf_bar_close("OANDA:EURUSD", &bar1, &[ev1.clone()], c2_ts + 60_000);
    assert!(changes1
        .iter()
        .any(|c| c.candidate.setup_status == SetupStatus::Validated));

    // Second same-direction MSS on the same symbol is ignored.
    let ev2 = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Mss,
        Direction::Bearish,
        c2_ts + 120_000,
    );
    let bar2 = make_bar("OANDA:EURUSD", c2_ts + 120_000, 1.082);
    let changes2 = engine.on_ltf_bar_close("OANDA:EURUSD", &bar2, &[ev2.clone()], c2_ts + 120_000);
    assert!(changes2.is_empty());
    let cand = engine.list_active()[0].clone();
    assert_eq!(cand.c2_cisd_event_ids.len(), 1);
    assert_eq!(cand.validations.len(), 1);
}

#[test]
fn t19_same_event_across_bars_no_duplicate() {
    // A6 dedup regression: on_ltf_bar_close receives the full
    // list_active(symbol, M5) every bar. The same CISD event is
    // re-passed on subsequent bars. It must NOT be re-added to
    // c2_cisd_event_ids or emit a new decision_log row.
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let c3_ts = c2_ts + MTF_DUR;
    let smt_c2 = make_smt(
        "smt-19",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt_c2), c2_ts);
    let smt_c3 = make_smt(
        "smt-19",
        Direction::Bullish,
        true,
        SmtDetectionState::C3Entry,
        c2_ts,
        Some(c3_ts),
    );
    engine.on_smt_event(&smt_update(&smt_c3), c3_ts);

    // DXY Bullish -> EURUSD trade Bearish (flipped). Events must be
    // Bearish to validate.
    let ev1 = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Cisd,
        Direction::Bearish,
        c2_ts + 60_000,
    );

    // bar1: first time ev1 is seen -> validates, c2_ids.len == 1.
    let bar1 = make_bar("OANDA:EURUSD", c2_ts + 60_000, 1.082);
    let changes1 = engine.on_ltf_bar_close("OANDA:EURUSD", &bar1, &[ev1.clone()], c2_ts + 60_000);
    assert!(changes1
        .iter()
        .any(|c| c.candidate.setup_status == SetupStatus::Validated));
    assert_eq!(
        changes1[0].candidate.c2_cisd_event_ids.len(),
        1,
        "first bar should add ev1"
    );

    // bar2: same ev1 re-passed -> should be skipped (no change, no dup).
    let bar2 = make_bar("OANDA:EURUSD", c2_ts + 120_000, 1.082);
    let changes2 = engine.on_ltf_bar_close("OANDA:EURUSD", &bar2, &[ev1.clone()], c2_ts + 120_000);
    assert!(
        changes2.is_empty(),
        "duplicate event should produce no changes"
    );

    // bar3: same ev1 again -> still skipped.
    let bar3 = make_bar("OANDA:EURUSD", c2_ts + 180_000, 1.082);
    let changes3 = engine.on_ltf_bar_close("OANDA:EURUSD", &bar3, &[ev1.clone()], c2_ts + 180_000);
    assert!(
        changes3.is_empty(),
        "duplicate event should still produce no changes"
    );

    // bar4: a new event on the already-resolved symbol is also ignored.
    let ev2 = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Mss,
        Direction::Bearish,
        c2_ts + 240_000,
    );
    let bar4 = make_bar("OANDA:EURUSD", c2_ts + 240_000, 1.082);
    let changes4 = engine.on_ltf_bar_close("OANDA:EURUSD", &bar4, &[ev2.clone()], c2_ts + 240_000);
    assert!(changes4.is_empty());
    assert_eq!(engine.list_active()[0].c2_cisd_event_ids.len(), 1);

    // bar5: ev1 + ev2 both re-passed -> both skipped, no change.
    let bar5 = make_bar("OANDA:EURUSD", c2_ts + 300_000, 1.082);
    let changes5 = engine.on_ltf_bar_close(
        "OANDA:EURUSD",
        &bar5,
        &[ev1.clone(), ev2.clone()],
        c2_ts + 300_000,
    );
    assert!(
        changes5.is_empty(),
        "both known events should produce no changes"
    );
    assert_eq!(changes5.len(), 0, "no decision_log rows for known events");
}

// T20: direction flip regression - Bearish DXY candidate.
// DXY Bearish (swept high, expecting down) -> EURUSD trade Bullish (flipped).
// A Bullish EURUSD CISD validates; a Bearish EURUSD CISD invalidates.
#[test]
fn t20_bearish_sweeper_direction_flip() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt = make_smt(
        "smt-20",
        Direction::Bearish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt), c2_ts);

    // EURUSD Bullish CISD -> validates (effective dir = Bullish for Bearish-DXY).
    let ev = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Cisd,
        Direction::Bullish,
        c2_ts + 60_000,
    );
    let bar = make_bar("OANDA:EURUSD", c2_ts + 60_000, 1.082);
    let changes = engine.on_ltf_bar_close("OANDA:EURUSD", &bar, &[ev], c2_ts + 60_000);
    assert_eq!(changes[0].candidate.setup_status, SetupStatus::Validated);
    assert_eq!(
        changes[0].candidate.validation_direction,
        Some(Direction::Bullish)
    );

    // New candidate (reset) to test invalidation.
    let mut engine2 = CandidateEngine::new();
    let smt2 = make_smt(
        "smt-20b",
        Direction::Bearish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine2.on_smt_event(&smt_event(&smt2), c2_ts);

    // EURUSD Bearish CISD -> invalidates (reverse of effective Bullish dir).
    let ev_rev = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Cisd,
        Direction::Bearish,
        c2_ts + 60_000,
    );
    let bar2 = make_bar("OANDA:EURUSD", c2_ts + 60_000, 1.082);
    let changes2 = engine2.on_ltf_bar_close("OANDA:EURUSD", &bar2, &[ev_rev], c2_ts + 60_000);
    assert!(changes2.iter().any(|c| {
        c.candidate.setup_status == SetupStatus::Invalidated
            && c.candidate.expiry_reason == Some(ExpiryReason::Reverse)
    }));
}

// T21: per-symbol resolution. EURUSD validation is retained while a reverse
// GBPUSD event closes only GBPUSD; later EURUSD noise is ignored.
#[test]
fn t21_option_b_non_validation_symbol_reverse() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;

    // Build SMT with EURUSD + GBPUSD as trade_symbols.
    let mut smt = make_smt(
        "smt-21",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    smt.trade_symbols.push("OANDA:GBPUSD".into());
    smt.symbol_set.push("OANDA:GBPUSD".into());
    smt.chains.push(SymbolChain {
        symbol: "OANDA:GBPUSD".into(),
        c1_candle: candle(TS_BASE, 1.279, 1.282, 1.278, 1.280),
        smt_k_candle: candle(TS_BASE + MTF_DUR, 1.281, 1.283, 1.280, 1.282),
        c2_candle: Some(candle(c2_ts, 1.280, 1.284, 1.279, 1.282)),
        c2_case: Some(1),
        c3_candle: None,
        detection_state: SmtDetectionState::C2Confirmed,
    });
    smt.liquidity_refs.push(LiquidityRef {
        symbol: "OANDA:GBPUSD".into(),
        ref_price: 1.28,
        ref_ts: TS_BASE,
        side: LiquiditySide::BuySide,
        status: LiquidityRefStatus::NotSwept,
        tf: Timeframe::H4,
        mtf_ref_candle: None,
        mtf_sweep_candle: None,
    });

    engine.on_smt_event(&smt_event(&smt), c2_ts);

    // Step 1: EURUSD Bearish CISD (effective dir for Bullish-DXY) -> validates.
    let ev_eu = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Cisd,
        Direction::Bearish,
        c2_ts + 60_000,
    );
    let bar_eu = make_bar("OANDA:EURUSD", c2_ts + 60_000, 1.082);
    let changes = engine.on_ltf_bar_close("OANDA:EURUSD", &bar_eu, &[ev_eu], c2_ts + 60_000);
    assert!(changes
        .iter()
        .any(|c| c.candidate.setup_status == SetupStatus::Validated));
    assert_eq!(
        changes[0].candidate.validation_symbol.as_deref(),
        Some("OANDA:EURUSD")
    );

    // Step 2: GBPUSD Bullish CISD (reverse of Bearish effective dir).
    // Should NOT invalidate - only EURUSD can invalidate after validation.
    let ev_gu = ltf_event(
        "OANDA:GBPUSD",
        ReversalConfirmKind::Cisd,
        Direction::Bullish,
        c2_ts + 120_000,
    );
    // close <= C2 high (1.284) to avoid C2 break on GBPUSD.
    let bar_gu = make_bar("OANDA:GBPUSD", c2_ts + 120_000, 1.282);
    let changes2 = engine.on_ltf_bar_close("OANDA:GBPUSD", &bar_gu, &[ev_gu], c2_ts + 120_000);
    assert!(
        changes2
            .iter()
            .all(|c| c.candidate.setup_status != SetupStatus::Invalidated),
        "GBPUSD reverse CISD must not invalidate EURUSD-validated candidate"
    );

    // Step 3: EURUSD is already confirmed; later reverse noise is ignored.
    let ev_eu_rev = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Cisd,
        Direction::Bullish,
        c2_ts + 180_000,
    );
    // close <= C2 high (1.084) so only the reverse CISD triggers, not C2 break.
    let bar_eu_rev = make_bar("OANDA:EURUSD", c2_ts + 180_000, 1.082);
    let changes3 =
        engine.on_ltf_bar_close("OANDA:EURUSD", &bar_eu_rev, &[ev_eu_rev], c2_ts + 180_000);
    assert!(changes3.is_empty());
    let candidate = engine.list_active()[0].clone();
    assert_eq!(candidate.setup_status, SetupStatus::Validated);
    assert_eq!(candidate.validations.len(), 1);
    assert!(candidate
        .invalidated_symbols
        .contains(&"OANDA:GBPUSD".to_string()));
    assert_eq!(
        candidate.symbol_invalidation_reasons.get("OANDA:GBPUSD"),
        Some(&ExpiryReason::Reverse)
    );
}

// T22: Integration - LTF bar close -> CandidateChange (Validated) ->
// AlertEngine -> AlertRecord. Regression for A1: ict_radar.rs LTF
// persist loop was missing the alerts.on_candidate_change() call.
#[test]
fn t22_ltf_path_to_alert_integration() {
    use crate::alert::{AlertEngine, AlertTrigger};
    use crate::types::{Bar, Timeframe};

    // 1. CandidateEngine: spawn candidate + validate via LTF bar close.
    let mut cand_engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt = make_smt(
        "smt-t22",
        Direction::Bullish,
        true, // has PDA
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    cand_engine.on_smt_event(&smt_event(&smt), c2_ts);

    // LTF CISD (Bearish = flipped for Bullish-DXY) -> Validated.
    let ev = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Cisd,
        Direction::Bearish,
        c2_ts + 60_000,
    );
    let bar = Bar {
        symbol: "OANDA:EURUSD".into(),
        tf: Timeframe::M5,
        ts: c2_ts + 60_000,
        open: 1.082,
        high: 1.083,
        low: 1.081,
        close: 1.082,
        volume: 1.0,
    };
    let changes = cand_engine.on_ltf_bar_close("OANDA:EURUSD", &bar, &[ev], c2_ts + 60_000);

    // 2. Find the Validated transition in the changes.
    let validated = changes
        .iter()
        .find(|c| c.candidate.setup_status == SetupStatus::Validated);
    assert!(
        validated.is_some(),
        "LTF bar close should produce Validated transition"
    );
    let change = validated.unwrap();

    // 3. Feed to AlertEngine (mirrors ict_radar.rs LTF loop wiring).
    let mut alert_engine = AlertEngine::new();
    // Pre-seed prev_status with C2Confirmed (prior on_candidate_change).
    let c2_change = cand_engine.list_active().into_iter().next().map(|c| {
        use crate::candidate::DecisionLogEntry;
        use crate::candidate::DecisionMode;
        CandidateChange {
            candidate: CandidateSetup {
                setup_status: SetupStatus::C2Confirmed,
                ..c
            },
            validation_event: None,
            decision: DecisionLogEntry {
                id: "seed".into(),
                candidate_id: "seed".into(),
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
                created_at: c2_ts,
            },
        }
    });
    if let Some(c2c) = c2_change {
        alert_engine.on_candidate_change(&c2c, c2_ts, false);
    }

    // 4. Alert should fire on the Validated transition.
    let alert = alert_engine.on_candidate_change(change, c2_ts + 60_000, false);
    assert!(
        alert.is_some(),
        "AlertEngine should fire on LTF Validated transition"
    );
    let alert = alert.unwrap();
    assert_eq!(alert.trigger, AlertTrigger::Validated);
    assert_eq!(alert.candidate_id, change.candidate.id);
    assert_eq!(
        alert.validation_direction,
        change.candidate.validation_direction
    );
}

#[test]
fn t23_c2_timestamp_is_inside_validation_window() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt = make_smt(
        "smt-23",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt), c2_ts);

    let event = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Cisd,
        Direction::Bearish,
        c2_ts,
    );
    let bar = make_bar("OANDA:EURUSD", c2_ts, 1.082);
    let changes = engine.on_ltf_bar_close("OANDA:EURUSD", &bar, &[event], c2_ts);

    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].candidate.setup_status, SetupStatus::Validated);
    assert_eq!(changes[0].candidate.validation_ts, Some(c2_ts));
}

#[test]
fn t24_future_ltf_event_is_not_consumed_early() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt = make_smt(
        "smt-24",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt), c2_ts);

    let future_event = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Cisd,
        Direction::Bearish,
        c2_ts + 10 * 60_000,
    );
    let current_bar = make_bar("OANDA:EURUSD", c2_ts + 5 * 60_000, 1.082);
    let changes = engine.on_ltf_bar_close(
        "OANDA:EURUSD",
        &current_bar,
        &[future_event],
        current_bar.ts,
    );

    assert!(changes.is_empty());
    assert_eq!(
        engine.list_active()[0].setup_status,
        SetupStatus::C2Confirmed
    );
}

#[test]
fn t25_c2_break_wins_over_same_bar_validation() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt = make_smt(
        "smt-25",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&smt), c2_ts);

    let event = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Cisd,
        Direction::Bearish,
        c2_ts + 60_000,
    );
    // Effective EURUSD direction is bearish, so a close above its C2 high
    // (1.084) is terminal even if a same-bar bearish CISD is present.
    let bar = make_bar("OANDA:EURUSD", c2_ts + 60_000, 1.090);
    let changes = engine.on_ltf_bar_close("OANDA:EURUSD", &bar, &[event], bar.ts);

    assert_eq!(changes.len(), 1, "must not emit a transient validation");
    assert_eq!(changes[0].candidate.setup_status, SetupStatus::Invalidated);
    assert_eq!(
        changes[0].candidate.expiry_reason,
        Some(ExpiryReason::C2Break)
    );
}

#[test]
fn t26_repeated_c3_update_is_idempotent() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let c3_ts = c2_ts + MTF_DUR;
    let c2 = make_smt(
        "smt-26",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    engine.on_smt_event(&smt_event(&c2), c2_ts);
    let c3 = make_smt(
        "smt-26",
        Direction::Bullish,
        true,
        SmtDetectionState::C3Entry,
        c2_ts,
        Some(c3_ts),
    );

    assert_eq!(engine.on_smt_event(&smt_update(&c3), c3_ts).len(), 1);
    assert!(engine
        .on_smt_event(&smt_update(&c3), c3_ts + 60_000)
        .is_empty());
}

#[test]
fn t27_aggregates_eu_and_gu_even_when_only_gu_is_smt_trade_leg() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let mut smt = make_smt(
        "smt-27",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    // Model the real 08/12 case: EURUSD swept together with DXY, so only
    // GBPUSD is the analytical SMT divergence leg. EURUSD is still an
    // executable instrument and must be respected for LTF validation.
    smt.trade_symbols = vec!["OANDA:GBPUSD".into()];
    smt.symbol_set.push("OANDA:GBPUSD".into());
    smt.chains.push(SymbolChain {
        symbol: "OANDA:GBPUSD".into(),
        c1_candle: candle(TS_BASE, 1.279, 1.282, 1.278, 1.280),
        smt_k_candle: candle(TS_BASE + MTF_DUR, 1.281, 1.283, 1.280, 1.282),
        c2_candle: Some(candle(c2_ts, 1.280, 1.284, 1.279, 1.282)),
        c2_case: Some(1),
        c3_candle: None,
        detection_state: SmtDetectionState::C2Confirmed,
    });

    let spawned = engine.on_smt_event(&smt_event(&smt), c2_ts);
    assert_eq!(
        spawned[0].candidate.trade_symbols,
        vec!["OANDA:EURUSD", "OANDA:GBPUSD"]
    );

    let eu_event = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Mss,
        Direction::Bearish,
        c2_ts + 5 * 60_000,
    );
    let eu_bar = make_bar("OANDA:EURUSD", eu_event.ts, 1.082);
    let eu_changes =
        engine.on_ltf_bar_close("OANDA:EURUSD", &eu_bar, &[eu_event.clone()], eu_event.ts);
    assert_eq!(eu_changes.len(), 1);
    assert_eq!(
        eu_changes[0]
            .validation_event
            .as_ref()
            .map(|validation| validation.symbol.as_str()),
        Some("OANDA:EURUSD")
    );

    let gu_event = ltf_event(
        "OANDA:GBPUSD",
        ReversalConfirmKind::Cisd,
        Direction::Bearish,
        c2_ts + 10 * 60_000,
    );
    let gu_bar = make_bar("OANDA:GBPUSD", gu_event.ts, 1.282);
    let gu_changes =
        engine.on_ltf_bar_close("OANDA:GBPUSD", &gu_bar, &[gu_event.clone()], gu_event.ts);
    assert_eq!(gu_changes.len(), 1);
    assert_eq!(gu_changes[0].candidate.validations.len(), 2);
    assert_eq!(gu_changes[0].candidate.validation_ts, Some(eu_event.ts));
    assert_eq!(
        gu_changes[0]
            .validation_event
            .as_ref()
            .map(|validation| validation.symbol.as_str()),
        Some("OANDA:GBPUSD")
    );
}

#[test]
fn t30_trade_symbol_uses_its_own_earlier_c2_c3_window() {
    let mut engine = CandidateEngine::new();
    let dxy_c2_ts = TS_BASE + 3 * MTF_DUR;
    let eu_c2_ts = TS_BASE + MTF_DUR;
    let eu_c3_ts = TS_BASE + 2 * MTF_DUR;
    let mut smt = make_smt(
        "smt-30",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        dxy_c2_ts,
        None,
    );
    let eu_chain = smt
        .chains
        .iter_mut()
        .find(|chain| chain.symbol == "OANDA:EURUSD")
        .unwrap();
    eu_chain.c2_candle = Some(candle(eu_c2_ts, 1.080, 1.084, 1.079, 1.082));
    eu_chain.c3_candle = Some(candle(eu_c3_ts, 1.081, 1.085, 1.080, 1.083));
    eu_chain.detection_state = SmtDetectionState::C3Entry;

    engine.on_smt_event(&smt_event(&smt), dxy_c2_ts);
    let reversal_ts = eu_c2_ts + 5 * 60_000;
    let reversal = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Mss,
        Direction::Bearish,
        reversal_ts,
    );
    let current_bar = make_bar("OANDA:EURUSD", dxy_c2_ts, 1.082);
    let changes = engine.on_ltf_bar_close("OANDA:EURUSD", &current_bar, &[reversal], dxy_c2_ts);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].candidate.validation_ts, Some(reversal_ts));
}

#[test]
fn t28_replay_uses_earlier_reversal_before_later_c2_break() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt = make_smt(
        "smt-28",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    let validation = ltf_event(
        "OANDA:EURUSD",
        ReversalConfirmKind::Mss,
        Direction::Bearish,
        c2_ts + 5 * 60_000,
    );
    let mut structures = HashMap::new();
    structures.insert("OANDA:EURUSD".to_string(), vec![validation.clone()]);
    let mut bars = HashMap::new();
    bars.insert(
        "OANDA:EURUSD".to_string(),
        vec![make_bar("OANDA:EURUSD", c2_ts + 10 * 60_000, 1.090)],
    );

    engine.replay(vec![smt], &structures, &bars, c2_ts + 15 * 60_000);
    let candidate = engine.list_active().into_iter().next().expect("candidate");
    assert_eq!(candidate.setup_status, SetupStatus::Validated);
    assert_eq!(candidate.validation_ts, Some(validation.ts));
}

#[test]
fn t29_replay_same_bar_c2_break_beats_reversal() {
    let mut engine = CandidateEngine::new();
    let c2_ts = TS_BASE + 2 * MTF_DUR;
    let smt = make_smt(
        "smt-29",
        Direction::Bullish,
        true,
        SmtDetectionState::C2Confirmed,
        c2_ts,
        None,
    );
    let ts = c2_ts + 5 * 60_000;
    let mut structures = HashMap::new();
    structures.insert(
        "OANDA:EURUSD".to_string(),
        vec![ltf_event(
            "OANDA:EURUSD",
            ReversalConfirmKind::Mss,
            Direction::Bearish,
            ts,
        )],
    );
    let mut bars = HashMap::new();
    bars.insert(
        "OANDA:EURUSD".to_string(),
        vec![make_bar("OANDA:EURUSD", ts, 1.090)],
    );

    let changes = engine.replay(vec![smt], &structures, &bars, ts);
    assert!(changes.iter().any(|change| {
        change.candidate.setup_status == SetupStatus::Invalidated
            && change.candidate.expiry_reason == Some(ExpiryReason::C2Break)
    }));
    assert!(engine.list_active().is_empty());
}
