use ict_monitor::alert::{build_c2_alert, AlertRecord, ChannelKind};
use ict_monitor::candidate::{CandidateSetup, DecisionStatus, SetupStatus, SetupType};
use ict_monitor::detector::types::{
    CandleRef, Correlation, Direction, LiquidityRef, LiquidityRefStatus, LiquiditySide,
    ReferenceScope, SmtDetectionState, SmtDivergence, SymbolChain,
};
use ict_monitor::types::Timeframe;
fn candle(ts: i64) -> CandleRef {
    CandleRef {
        ts,
        open: 100.0,
        high: 101.0,
        low: 99.0,
        close: 100.5,
    }
}

pub fn fixture(
    watchlist_id: &str,
    candidate_id: &str,
) -> (CandidateSetup, SmtDivergence, AlertRecord) {
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
        rule_version: ict_monitor::detector::smt::CURRENT_RULE_VERSION.into(),
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
        smt_rule_version: ict_monitor::detector::smt::CURRENT_RULE_VERSION.into(),
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
