//! Unit tests for the M5_ADDENDUM_C2 SMT engine.

use super::*;
use crate::detector::types::{Fvg, FvgState, ObState, OrderBlock};
use crate::watchlist::default_correlations_for;

const TF: Timeframe = Timeframe::M30;
const DUR: i64 = 30 * 60 * 1000; // 1_800_000

fn make_wl(symbols: Vec<String>) -> Watchlist {
    Watchlist {
        id: "test".into(),
        name: "test".into(),
        symbols: symbols.clone(),
        correlations: default_correlations_for(&symbols),
    }
}

/// Full OHLC bar at given index (ts = idx * DUR).
fn bar(sym: &str, idx: i64, open: f64, high: f64, low: f64, close: f64) -> Bar {
    Bar {
        symbol: sym.into(),
        tf: TF,
        ts: idx * DUR,
        open,
        high,
        low,
        close,
        volume: 1.0,
    }
}

fn feed_all(e: &mut SmtEngine, bars: &[Bar]) -> Vec<StructureEvent> {
    let mut events = Vec::new();
    for b in bars {
        events.extend(e.on_closed_bar(b));
    }
    events
}

#[test]
fn pda_depletion_is_90_percent_before_smt_k_and_keeps_fvg_state_separate() {
    let mut engine = SmtEngine::new();
    engine.set_watchlist(make_wl(vec!["DXY".into(), "EURUSD".into()]));
    let parent = Timeframe::H1;
    let pda = PdaRef {
        kind: "fvg".into(),
        id: "pda-90".into(),
        tf: parent,
        direction: Direction::Bullish,
        price_low: 99.0,
        price_high: 100.0,
        ts_open: 0,
        ts_confirm: 4 * DUR,
        exit_ts: None,
        ts_filled: None,
    };
    seed_pda_formation(
        &mut engine,
        "DXY",
        parent,
        Direction::Bullish,
        99.0,
        100.0,
        0,
        4 * DUR,
    );
    for source in [
        bar("DXY", 6, 100.1, 100.2, 99.50, 99.8),
        bar("DXY", 7, 99.8, 99.9, 99.11, 99.4),
        bar("DXY", 8, 99.4, 99.5, 99.10, 99.3),
    ] {
        engine.seed_bar(&source);
    }

    assert_eq!(
        engine.first_pda_depletion_ts("DXY", TF, &pda, 8 * DUR),
        None
    );
    assert_eq!(
        engine.first_pda_depletion_ts("DXY", TF, &pda, 9 * DUR),
        Some(8 * DUR)
    );

    let fvg = Fvg {
        id: pda.id.clone(),
        symbol: "DXY".into(),
        tf: parent,
        direction: Direction::Bullish,
        ts_open: pda.ts_open,
        ts_confirm: pda.ts_confirm,
        price_low: pda.price_low,
        price_high: pda.price_high,
        state: FvgState::Mitigated50,
        ts_filled: None,
        consumed_exit_ts: None,
    };
    assert_eq!(engine.fvg_smt_depletion_ts(&fvg), Some(8 * DUR));
    assert_eq!(fvg.state, FvgState::Mitigated50);
}

fn seed_pda_formation(
    engine: &mut SmtEngine,
    symbol: &str,
    tf: Timeframe,
    direction: Direction,
    price_low: f64,
    price_high: f64,
    ts_open: i64,
    ts_confirm: i64,
) {
    let child_tf = mtf_for_htf(tf).expect("test PDA must have a child timeframe");
    let child_dur = child_tf.duration_ms();
    let parent_dur = tf.duration_ms();
    let children_per_parent = parent_dur / child_dur;
    assert_eq!(ts_confirm, ts_open + 2 * parent_dur);
    let (first_high, first_low, third_high, third_low) = match direction {
        Direction::Bullish => (price_low, price_low - 0.1, price_high + 0.1, price_high),
        Direction::Bearish => (price_high + 0.1, price_high, price_low, price_low - 0.1),
    };
    let bucket = engine
        .buckets
        .entry((symbol.to_string(), child_tf))
        .or_insert_with(SmtBucket::new);
    for child_index in 0..(3 * children_per_parent) {
        let ts = ts_open + child_index * child_dur;
        if bucket.bars.iter().any(|bar| bar.ts == ts) {
            continue;
        }
        let (high, low) = match child_index / children_per_parent {
            0 => (first_high, first_low),
            1 => (first_high.max(third_high), first_low.min(third_low)),
            _ => (third_high, third_low),
        };
        bucket.bars.push_back(Bar {
            symbol: symbol.into(),
            tf: child_tf,
            ts,
            open: low,
            high,
            low,
            close: high,
            volume: 1.0,
        });
    }
    bucket.bars.make_contiguous().sort_by_key(|bar| bar.ts);
}

fn extract_smt(events: &[StructureEvent]) -> Vec<&SmtDivergence> {
    // Deduplicate by SMT ID, keeping the latest version (Update overwrites
    // New). Pattern B may fire before Pattern A for the same sweep; both
    // produce the same ID, and the later Update has the complete chains.
    let mut by_id: Vec<(&str, &SmtDivergence)> = Vec::new();
    for ev in events {
        if let Some(d) = match ev {
            StructureEvent::New(IctStructure::SmtDivergence(d))
            | StructureEvent::Update(IctStructure::SmtDivergence(d)) => Some(d),
            _ => None,
        } {
            if let Some(pos) = by_id.iter().position(|(id, _)| *id == d.id.as_str()) {
                by_id[pos] = (d.id.as_str(), d);
            } else {
                by_id.push((d.id.as_str(), d));
            }
        }
    }
    by_id.into_iter().map(|(_, d)| d).collect()
}

/// Canonical H1 -> M30 PDA-SMT fixture used by current-rule integration
/// tests. DXY owns the low-side sweep; EURUSD/GBPUSD are inverse counters.
/// The DXY HTF reference extreme is at M30 index 3 and its sweep extreme is
/// at index 7. Counter extrema deliberately occur on other sub-candles so
/// the per-symbol MTF endpoint contract is observable.
fn current_rule_h1_smt(eu_sweep_high: f64, gu_sweep_high: f64) -> (SmtEngine, Vec<StructureEvent>) {
    let mut engine = SmtEngine::new();
    engine.set_watchlist(make_wl(vec![
        "DXY".into(),
        "EURUSD".into(),
        "GBPUSD".into(),
    ]));

    let dxy = [
        (100.5, 101.0, 100.0, 100.6),
        (100.6, 101.1, 100.1, 100.7),
        (100.4, 100.8, 99.4, 100.0),
        (100.0, 100.5, 99.0, 99.8),
        (100.4, 100.9, 100.1, 100.6),
        (100.6, 101.0, 100.2, 100.7),
        (99.5, 99.7, 98.9, 99.4), // C1
        (99.4, 99.6, 98.5, 99.2), // SMT K + case-1 C2
    ];
    let eu = [
        (1.080, 1.085, 1.075, 1.081),
        (1.081, 1.086, 1.076, 1.082),
        (1.090, 1.100, 1.085, 1.091), // reference extreme
        (1.091, 1.098, 1.086, 1.092),
        (1.082, 1.087, 1.078, 1.083),
        (1.083, 1.088, 1.079, 1.084),
        (1.084, eu_sweep_high, 1.080, 1.085), // sweep-interval extreme
        (1.083, 1.087, 1.079, 1.084),
    ];
    let gu = [
        (1.280, 1.285, 1.275, 1.281),
        (1.281, 1.286, 1.276, 1.282),
        (1.290, 1.298, 1.285, 1.291),
        (1.291, 1.300, 1.286, 1.292), // reference extreme
        (1.282, 1.287, 1.278, 1.283),
        (1.283, 1.288, 1.279, 1.284),
        (1.284, 1.289, 1.280, 1.285),
        (1.285, gu_sweep_high, 1.281, 1.286), // sweep-interval extreme
    ];

    let mut mtf_bars = Vec::new();
    for (symbol, series) in [("DXY", &dxy[..]), ("EURUSD", &eu[..]), ("GBPUSD", &gu[..])] {
        for (index, &(open, high, low, close)) in series.iter().enumerate() {
            mtf_bars.push(bar(symbol, index as i64, open, high, low, close));
        }
    }
    mtf_bars.sort_by_key(|bar| (bar.ts, bar.symbol.clone()));
    feed_all(&mut engine, &mtf_bars);

    // Native HTF candles are stored for DXY liquidity discovery and PDA
    // market-bar ageing. Their values are derived from the exact child grid.
    let mut htf_bars = Vec::new();
    for symbol in ["DXY", "EURUSD", "GBPUSD"] {
        for htf_index in 0..4 {
            let start = htf_index * 2;
            let pair: Vec<&Bar> = mtf_bars
                .iter()
                .filter(|bar| {
                    bar.symbol == symbol && bar.ts >= start * DUR && bar.ts < (start + 2) * DUR
                })
                .collect();
            htf_bars.push(Bar {
                symbol: symbol.into(),
                tf: Timeframe::H1,
                ts: start * DUR,
                open: pair[0].open,
                high: pair
                    .iter()
                    .map(|bar| bar.high)
                    .fold(f64::NEG_INFINITY, f64::max),
                low: pair.iter().map(|bar| bar.low).fold(f64::INFINITY, f64::min),
                close: pair[1].close,
                volume: 2.0,
            });
        }
    }
    htf_bars.sort_by_key(|bar| (bar.ts, bar.symbol.clone()));
    feed_all(&mut engine, &htf_bars);

    let pda = IctStructure::Fvg(Fvg {
        id: "current-rule-pda".into(),
        symbol: "DXY".into(),
        tf: Timeframe::H1,
        direction: Direction::Bullish,
        ts_open: -8 * DUR,
        ts_confirm: -4 * DUR,
        price_low: 98.4,
        price_high: 99.2,
        state: FvgState::Active,
        ts_filled: None,
        consumed_exit_ts: None,
    });
    let closed_dxy_h1 = htf_bars
        .iter()
        .find(|bar| bar.symbol == "DXY" && bar.ts == 6 * DUR)
        .unwrap();
    seed_pda_formation(
        &mut engine,
        "DXY",
        Timeframe::H1,
        Direction::Bullish,
        98.4,
        99.2,
        -8 * DUR,
        -4 * DUR,
    );
    let events = engine.detect_pda_htf_close(closed_dxy_h1, &[pda]);
    (engine, events)
}

fn pending_case2_divergence(id: &str, pda_id: &str) -> SmtDivergence {
    let c1 = CandleRef {
        ts: 6 * DUR,
        open: 1.0825,
        high: 1.0830,
        low: 1.0820,
        close: 1.0825,
    };
    let smt_k = CandleRef {
        ts: 7 * DUR,
        open: 1.0805,
        high: 1.0810,
        low: 1.0790,
        close: 1.0805,
    };
    SmtDivergence {
        id: id.into(),
        watchlist_id: "test".into(),
        rule_version: CURRENT_RULE_VERSION.into(),
        symbol_set: vec!["DXY".into(), "EURUSD".into()],
        relationship: Correlation::Negative,
        context_timeframe: Timeframe::H1,
        comparison_timeframe: TF,
        observation_window: (2 * DUR, 8 * DUR),
        htf_confirmed: true,
        reference_scope: ReferenceScope::DistantLeftSide,
        liquidity_refs: vec![
            LiquidityRef {
                symbol: "DXY".into(),
                ref_price: 1.0800,
                ref_ts: 2 * DUR,
                side: LiquiditySide::SellSide,
                status: LiquidityRefStatus::Swept,
                tf: Timeframe::H1,
                mtf_ref_candle: None,
                mtf_sweep_candle: Some(smt_k.clone()),
            },
            LiquidityRef {
                symbol: "EURUSD".into(),
                ref_price: 1.0900,
                ref_ts: 2 * DUR,
                side: LiquiditySide::BuySide,
                status: LiquidityRefStatus::NotSwept,
                tf: Timeframe::H1,
                mtf_ref_candle: None,
                mtf_sweep_candle: None,
            },
        ],
        confluence_refs: Vec::new(),
        candidate_direction: Direction::Bullish,
        sweeper_symbol: "DXY".into(),
        trade_symbols: vec!["EURUSD".into()],
        strength: vec![StrengthLabel {
            symbol: "EURUSD".into(),
            label: "strong".into(),
        }],
        chains: vec![
            SymbolChain {
                symbol: "DXY".into(),
                c1_candle: c1.clone(),
                smt_k_candle: smt_k.clone(),
                c2_candle: None,
                c2_case: None,
                c3_candle: None,
                detection_state: SmtDetectionState::SmtKDetected,
            },
            SymbolChain {
                symbol: "EURUSD".into(),
                c1_candle: c1,
                smt_k_candle: smt_k,
                c2_candle: None,
                c2_case: None,
                c3_candle: None,
                detection_state: SmtDetectionState::SmtKDetected,
            },
        ],
        invalidation_reasons: Vec::new(),
        invalidation_ts: None,
        htf_pda_ref: Some(PdaRef {
            kind: "fvg".into(),
            id: pda_id.into(),
            tf: Timeframe::H1,
            direction: Direction::Bullish,
            price_low: 1.0790,
            price_high: 1.0830,
            ts_open: 0,
            ts_confirm: DUR,
            exit_ts: None,
            ts_filled: None,
        }),
        mtf_ref_candle: None,
    }
}

/// Get the sweeper (trigger) chain from a divergence (§2.2).
fn sweeper_chain(d: &SmtDivergence) -> &SymbolChain {
    d.chains
        .iter()
        .find(|c| c.symbol == d.sweeper_symbol)
        .expect("sweeper chain must exist")
}

/// Build bars forming a swing-low at `center_idx` with trough `price`.
/// 5 bars: center-2 .. center+2. All bars have low > trough except center.
fn swing_low_5(sym: &str, center_idx: i64, trough: f64) -> Vec<Bar> {
    let base = trough + 0.0020;
    vec![
        bar(sym, center_idx - 2, base, base + 0.001, base, base + 0.0005),
        bar(
            sym,
            center_idx - 1,
            base - 0.0005,
            base,
            base - 0.0005,
            base - 0.0008,
        ),
        bar(
            sym,
            center_idx,
            trough + 0.0005,
            trough + 0.001,
            trough,
            trough + 0.0003,
        ),
        bar(
            sym,
            center_idx + 1,
            base - 0.0005,
            base,
            base - 0.0005,
            base - 0.0008,
        ),
        bar(sym, center_idx + 2, base, base + 0.001, base, base + 0.0005),
    ]
}

/// Build bars forming a swing-high at `center_idx` with peak `price`.
fn swing_high_5(sym: &str, center_idx: i64, peak: f64) -> Vec<Bar> {
    let base = peak - 0.0020;
    vec![
        bar(
            sym,
            center_idx - 2,
            base,
            base + 0.0005,
            base - 0.0005,
            base,
        ),
        bar(
            sym,
            center_idx - 1,
            base + 0.0008,
            base + 0.001,
            base,
            base + 0.0005,
        ),
        bar(
            sym,
            center_idx,
            peak - 0.0003,
            peak,
            peak - 0.0005,
            peak - 0.0005,
        ),
        bar(
            sym,
            center_idx + 1,
            base + 0.0008,
            base + 0.001,
            base,
            base + 0.0005,
        ),
        bar(
            sym,
            center_idx + 2,
            base,
            base + 0.0005,
            base - 0.0005,
            base,
        ),
    ]
}

// ---- Test 1: correlation-aware comparison (positive vs inverse) ----

#[test]
fn positive_corr_same_direction() {
    // Only DXY can be the sweeper (§2.2). DXY↔EURUSD is always Inverse,
    // so positive correlation (EURUSD↔GBPUSD) is never the active pair in a
    // full SMT scenario. Test the correlation-aware helper directly instead.
    //
    // Positive: same-direction extremes (Low↔Low, High↔High).
    assert_eq!(
        expected_counter_swing(SwingKind::Low, Correlation::Positive),
        SwingKind::Low
    );
    assert_eq!(
        expected_counter_swing(SwingKind::High, Correlation::Positive),
        SwingKind::High
    );
    // Inverse: opposite extremes (DXY low ↔ EURUSD high, DXY high ↔ EURUSD low).
    assert_eq!(
        expected_counter_swing(SwingKind::Low, Correlation::Negative),
        SwingKind::High
    );
    assert_eq!(
        expected_counter_swing(SwingKind::High, Correlation::Negative),
        SwingKind::Low
    );
}

#[test]
fn positive_group_compares_same_side_and_all_swept_has_no_divergence() {
    let dxy_ref_high = 100.0;
    let chf_ref_high = 0.82;
    let cad_ref_high = 1.39;
    let dxy = bar("DXY", 1, 99.8, 100.1, 99.7, 100.0);
    let chf = bar("USDCHF", 1, 0.81, 0.82, 0.80, 0.815);
    let cad = bar("USDCAD", 1, 1.38, 1.391, 1.37, 1.385);

    let side = expected_counter_swing(SwingKind::High, Correlation::Positive);
    assert_eq!(side, SwingKind::High, "positive counters compare highs");
    let statuses = [
        check_sweep(&dxy, dxy_ref_high, SwingKind::High),
        check_sweep(&chf, chf_ref_high, side),
        check_sweep(&cad, cad_ref_high, side),
    ];
    assert!(statuses
        .iter()
        .all(|status| *status == LiquidityRefStatus::Swept));
    assert!(
        !statuses
            .iter()
            .any(|status| *status == LiquidityRefStatus::NotSwept),
        "when DXY, CHF and CAD all take their highs there is no SMT divergence",
    );
}

#[test]
fn current_rule_confirms_only_after_closed_htf_with_pda() {
    let (engine, events) = current_rule_h1_smt(1.095, 1.295);
    let smts = extract_smt(&events);
    assert_eq!(smts.len(), 1);
    let smt = smts[0];
    assert_eq!(smt.rule_version, CURRENT_RULE_VERSION);
    assert_eq!(smt.sweeper_symbol, "DXY");
    assert_eq!(smt.context_timeframe, Timeframe::H1);
    assert_eq!(smt.comparison_timeframe, Timeframe::M30);
    assert_eq!(smt.observation_window, (2 * DUR, 8 * DUR));
    assert_eq!(
        smt.htf_pda_ref.as_ref().map(|pda| pda.id.as_str()),
        Some("current-rule-pda")
    );
    assert_eq!(smt.trade_symbols, vec!["EURUSD", "GBPUSD"]);
    assert_eq!(
        sweeper_chain(smt).detection_state,
        SmtDetectionState::C2Confirmed
    );
    assert!(
        !engine.is_pda_consumed("current-rule-pda"),
        "C2 reserves the PDA; only a valid C3 consumes it"
    );
}

#[test]
fn current_rule_touch_or_equal_counts_as_swept_without_reclaim() {
    // Both counters exactly touch their paired highs. Close location is
    // irrelevant: all three symbols took liquidity, so no trade leg remains.
    let (engine, events) = current_rule_h1_smt(1.100, 1.300);
    let smts = extract_smt(&events);
    assert_eq!(smts.len(), 1, "terminal audit evidence is still emitted");
    let smt = smts[0];
    assert!(smt.trade_symbols.is_empty());
    assert!(smt
        .liquidity_refs
        .iter()
        .filter(|liquidity| liquidity.symbol != "DXY")
        .all(|liquidity| liquidity.status == LiquidityRefStatus::Swept));
    assert!(!engine.is_pda_consumed("current-rule-pda"));
}

#[test]
fn current_rule_preserves_per_symbol_mtf_extreme_timestamps() {
    let (_, events) = current_rule_h1_smt(1.095, 1.295);
    let smt = extract_smt(&events)[0];
    let dxy = smt
        .liquidity_refs
        .iter()
        .find(|item| item.symbol == "DXY")
        .unwrap();
    let eu = smt
        .liquidity_refs
        .iter()
        .find(|item| item.symbol == "EURUSD")
        .unwrap();
    let gu = smt
        .liquidity_refs
        .iter()
        .find(|item| item.symbol == "GBPUSD")
        .unwrap();
    assert_eq!(dxy.mtf_ref_candle.as_ref().unwrap().ts, 3 * DUR);
    assert_eq!(dxy.mtf_sweep_candle.as_ref().unwrap().ts, 7 * DUR);
    assert_eq!(eu.mtf_ref_candle.as_ref().unwrap().ts, 2 * DUR);
    assert_eq!(eu.mtf_sweep_candle.as_ref().unwrap().ts, 6 * DUR);
    assert_eq!(gu.mtf_ref_candle.as_ref().unwrap().ts, 3 * DUR);
    assert_eq!(gu.mtf_sweep_candle.as_ref().unwrap().ts, 7 * DUR);
    assert_eq!(gu.ref_ts, 2 * DUR, "HTF line starts at the parent bar open");
    assert_eq!(
        gu.ref_price, 1.300,
        "HTF line uses the parent interval extreme"
    );

    let dxy_chain = smt
        .chains
        .iter()
        .find(|chain| chain.symbol == "DXY")
        .unwrap();
    let eu_chain = smt
        .chains
        .iter()
        .find(|chain| chain.symbol == "EURUSD")
        .unwrap();
    let gu_chain = smt
        .chains
        .iter()
        .find(|chain| chain.symbol == "GBPUSD")
        .unwrap();
    assert_eq!(dxy_chain.smt_k_candle.ts, 7 * DUR);
    assert_eq!(eu_chain.smt_k_candle.ts, 6 * DUR);
    assert_eq!(gu_chain.smt_k_candle.ts, 7 * DUR);
}

#[test]
fn reference_priority_falls_through_when_higher_level_has_no_divergence() {
    let mut engine = SmtEngine::new();
    engine.set_watchlist(make_wl(vec![
        "DXY".into(),
        "EURUSD".into(),
        "GBPUSD".into(),
    ]));

    let dxy = [
        (100.5, 101.0, 100.0, 100.6),
        (100.6, 101.1, 100.2, 100.7),
        (99.6, 100.0, 99.3, 99.7),
        // Stays above the PDA midpoint (98.8): this is a valid unmitigated
        // distant reference under the current first-effective-return rule.
        (99.2, 99.7, 98.9, 99.3), // distant HTF reference
        (99.7, 100.0, 99.4, 99.8),
        (99.6, 99.9, 99.3, 99.7),
        (99.4, 99.7, 99.0, 99.5), // confirmed local MTF reference
        (99.5, 99.8, 99.2, 99.6),
        (99.1, 99.4, 98.9, 99.1), // C1; already touches local ref
        (99.0, 99.3, 98.5, 99.0), // canonical SMT K / C2 case 1
    ];
    let eu = [
        (1.05, 1.06, 1.04, 1.05),
        (1.05, 1.06, 1.04, 1.05),
        (1.07, 1.08, 1.06, 1.07),
        (1.08, 1.10, 1.07, 1.09), // distant paired high
        (1.08, 1.09, 1.07, 1.08),
        (1.08, 1.09, 1.07, 1.08),
        (1.10, 1.20, 1.09, 1.11), // local paired high
        (1.09, 1.10, 1.08, 1.09),
        (1.10, 1.15, 1.09, 1.11),
        (1.11, 1.14, 1.10, 1.12),
    ];
    let gu = [
        (1.25, 1.26, 1.24, 1.25),
        (1.25, 1.26, 1.24, 1.25),
        (1.27, 1.28, 1.26, 1.27),
        (1.28, 1.30, 1.27, 1.29), // distant paired high
        (1.28, 1.29, 1.27, 1.28),
        (1.28, 1.29, 1.27, 1.28),
        (1.30, 1.40, 1.29, 1.31), // local paired high
        (1.29, 1.30, 1.28, 1.29),
        (1.30, 1.35, 1.29, 1.31),
        (1.31, 1.34, 1.30, 1.32),
    ];
    let mut mtf_bars = Vec::new();
    for (symbol, series) in [("DXY", &dxy[..]), ("EURUSD", &eu[..]), ("GBPUSD", &gu[..])] {
        for (index, &(open, high, low, close)) in series.iter().enumerate() {
            mtf_bars.push(bar(symbol, index as i64, open, high, low, close));
        }
    }
    mtf_bars.sort_by_key(|bar| (bar.ts, bar.symbol.clone()));
    feed_all(&mut engine, &mtf_bars);

    let mut dxy_h1 = Vec::new();
    for start in (0..10).step_by(2) {
        let pair: Vec<&Bar> = mtf_bars
            .iter()
            .filter(|bar| {
                bar.symbol == "DXY"
                    && bar.ts >= start as i64 * DUR
                    && bar.ts < (start as i64 + 2) * DUR
            })
            .collect();
        dxy_h1.push(Bar {
            symbol: "DXY".into(),
            tf: Timeframe::H1,
            ts: start as i64 * DUR,
            open: pair[0].open,
            high: pair
                .iter()
                .map(|bar| bar.high)
                .fold(f64::NEG_INFINITY, f64::max),
            low: pair.iter().map(|bar| bar.low).fold(f64::INFINITY, f64::min),
            close: pair[1].close,
            volume: 2.0,
        });
    }
    feed_all(&mut engine, &dxy_h1);
    let pda = IctStructure::Fvg(Fvg {
        id: "priority-fallback-pda".into(),
        symbol: "DXY".into(),
        tf: Timeframe::H1,
        direction: Direction::Bullish,
        ts_open: -8 * DUR,
        ts_confirm: -4 * DUR,
        price_low: 98.4,
        price_high: 99.2,
        state: FvgState::Active,
        ts_filled: None,
        consumed_exit_ts: None,
    });

    seed_pda_formation(
        &mut engine,
        "DXY",
        Timeframe::H1,
        Direction::Bullish,
        98.4,
        99.2,
        -8 * DUR,
        -4 * DUR,
    );
    let events = engine.detect_pda_htf_close(dxy_h1.last().unwrap(), &[pda]);
    let smt = extract_smt(&events)[0];
    let dxy_liquidity = smt
        .liquidity_refs
        .iter()
        .find(|liquidity| liquidity.symbol == "DXY")
        .unwrap();
    assert_eq!(smt.reference_scope, ReferenceScope::LocalNearPda);
    assert_eq!(dxy_liquidity.tf, Timeframe::H1);
    assert_eq!(dxy_liquidity.ref_ts, 6 * DUR);
    assert_eq!(smt.trade_symbols, vec!["EURUSD", "GBPUSD"]);
    assert!(smt.confluence_refs.is_empty());
}

#[test]
fn adjacent_completed_htf_candle_can_supply_unconfirmed_local_liquidity() {
    let mut engine = SmtEngine::new();
    engine.set_watchlist(make_wl(vec![
        "DXY".into(),
        "EURUSD".into(),
        "GBPUSD".into(),
    ]));

    // H1 reference = M30 indices 2/3; H1 sweep = indices 4/5. The reference
    // H1 has no right-side H1 candle before the sweep and is intentionally
    // not a confirmed three-candle swing. EU takes its paired low while GU
    // does not, reproducing the 08/10 11:00 -> 12:00 DXY/GU divergence.
    let dxy = [
        (99.56, 99.59, 99.52, 99.57), // depart PDA
        (99.62, 99.68, 99.61, 99.66), // first return to PDA
        (99.68, 99.710, 99.65, 99.70),
        (99.70, 99.721, 99.698, 99.716), // adjacent HTF reference high / C1
        (99.717, 99.728, 99.710, 99.717), // SMT K + C2 case 1
        (99.718, 99.728, 99.693, 99.711), // valid C3
    ];
    let eu = [
        (1.1560, 1.1562, 1.1558, 1.1560),
        (1.1558, 1.1560, 1.1556, 1.1558),
        (1.1554, 1.1556, 1.1550, 1.1552),
        (1.1551, 1.15526, 1.15488, 1.15492), // paired reference low / C1
        (1.15492, 1.15502, 1.15480, 1.15498), // EU also sweeps
        (1.15497, 1.15526, 1.15484, 1.15515),
    ];
    let gu = [
        (1.3500, 1.3502, 1.3498, 1.3500),
        (1.3498, 1.3500, 1.3496, 1.3498),
        (1.3492, 1.3494, 1.3488, 1.3490),
        (1.3488, 1.34902, 1.34832, 1.34852), // paired reference low / C1
        (1.34852, 1.34868, 1.34838, 1.34860), // GU does not sweep
        (1.34859, 1.34902, 1.34848, 1.34884),
    ];
    let mut mtf_bars = Vec::new();
    for (symbol, series) in [("DXY", &dxy[..]), ("EURUSD", &eu[..]), ("GBPUSD", &gu[..])] {
        for (index, &(open, high, low, close)) in series.iter().enumerate() {
            mtf_bars.push(bar(symbol, index as i64, open, high, low, close));
        }
    }
    mtf_bars.sort_by_key(|item| (item.ts, item.symbol.clone()));
    feed_all(&mut engine, &mtf_bars);

    let mut dxy_h1 = Vec::new();
    for start in (0..6).step_by(2) {
        let children: Vec<&Bar> = mtf_bars
            .iter()
            .filter(|item| {
                item.symbol == "DXY"
                    && item.ts >= start as i64 * DUR
                    && item.ts < (start as i64 + 2) * DUR
            })
            .collect();
        dxy_h1.push(Bar {
            symbol: "DXY".into(),
            tf: Timeframe::H1,
            ts: start as i64 * DUR,
            open: children[0].open,
            high: children
                .iter()
                .map(|item| item.high)
                .fold(f64::NEG_INFINITY, f64::max),
            low: children
                .iter()
                .map(|item| item.low)
                .fold(f64::INFINITY, f64::min),
            close: children[1].close,
            volume: 2.0,
        });
    }
    feed_all(&mut engine, &dxy_h1);

    let pda = IctStructure::Fvg(Fvg {
        id: "adjacent-htf-pda".into(),
        symbol: "DXY".into(),
        tf: Timeframe::H1,
        direction: Direction::Bearish,
        ts_open: -8 * DUR,
        ts_confirm: -4 * DUR,
        price_low: 99.60,
        price_high: 99.80,
        state: FvgState::Mitigated50,
        ts_filled: None,
        consumed_exit_ts: None,
    });
    seed_pda_formation(
        &mut engine,
        "DXY",
        Timeframe::H1,
        Direction::Bearish,
        99.60,
        99.80,
        -8 * DUR,
        -4 * DUR,
    );
    let events = engine.detect_pda_htf_close(dxy_h1.last().unwrap(), &[pda]);
    let smts = extract_smt(&events);
    assert_eq!(smts.len(), 1);
    let smt = smts[0];
    assert_eq!(smt.reference_scope, ReferenceScope::LocalNearPda);
    assert_eq!(smt.observation_window.0, 2 * DUR);
    assert_eq!(smt.trade_symbols, vec!["GBPUSD"]);
    let dxy_ref = smt
        .liquidity_refs
        .iter()
        .find(|item| item.symbol == "DXY")
        .unwrap();
    assert_eq!(dxy_ref.ref_ts, 2 * DUR);
    assert_eq!(dxy_ref.mtf_ref_candle.as_ref().unwrap().ts, 3 * DUR);
    assert_eq!(dxy_ref.mtf_sweep_candle.as_ref().unwrap().ts, 4 * DUR);
    assert_eq!(
        sweeper_chain(smt).detection_state,
        SmtDetectionState::C3Entry
    );
    assert!(engine.is_pda_consumed("adjacent-htf-pda"));
}

#[test]
fn adjacent_htf_liquidity_replaces_an_older_reference_already_taken_in_same_pda() {
    let mut engine = SmtEngine::new();
    engine.set_watchlist(make_wl(vec!["DXY".into()]));

    // The confirmed H1 high at index 2 was already taken by the H1 candle at
    // index 6.  At index 10 it must therefore be ignored and the immediately
    // preceding completed H1 candle (index 8) must be offered as the current
    // local reference. This is the historical 08/10 12:00 fallback shape.
    let dxy = [
        (99.56, 99.59, 99.52, 99.57), // outside PDA
        (99.62, 99.68, 99.61, 99.66), // first return to PDA
        (99.67, 99.69, 99.64, 99.68),
        (99.68, 99.70, 99.66, 99.69), // confirmed older H1 high
        (99.68, 99.69, 99.65, 99.67),
        (99.67, 99.68, 99.64, 99.66),
        (99.67, 99.71, 99.66, 99.70), // already takes the older high
        (99.70, 99.705, 99.68, 99.69),
        (99.69, 99.710, 99.67, 99.70),
        (99.70, 99.721, 99.69, 99.716),   // adjacent reference
        (99.717, 99.728, 99.710, 99.717), // proposed sweep
        (99.718, 99.728, 99.693, 99.711),
    ];
    let mtf_bars: Vec<Bar> = dxy
        .iter()
        .enumerate()
        .map(|(index, &(open, high, low, close))| bar("DXY", index as i64, open, high, low, close))
        .collect();
    feed_all(&mut engine, &mtf_bars);

    let mut h1_bars = Vec::new();
    for start in (0..12).step_by(2) {
        let children = &mtf_bars[start..start + 2];
        h1_bars.push(Bar {
            symbol: "DXY".into(),
            tf: Timeframe::H1,
            ts: start as i64 * DUR,
            open: children[0].open,
            high: children
                .iter()
                .map(|item| item.high)
                .fold(f64::NEG_INFINITY, f64::max),
            low: children
                .iter()
                .map(|item| item.low)
                .fold(f64::INFINITY, f64::min),
            close: children[1].close,
            volume: 2.0,
        });
    }
    feed_all(&mut engine, &h1_bars);

    let pda = PdaRef {
        kind: "fvg".into(),
        id: "fallback-pda".into(),
        tf: Timeframe::H1,
        direction: Direction::Bearish,
        price_low: 99.60,
        price_high: 99.80,
        ts_open: -8 * DUR,
        ts_confirm: -4 * DUR,
        exit_ts: None,
        ts_filled: None,
    };
    let candidates = engine.pda_liquidity_candidates(
        "DXY",
        Timeframe::H1,
        Timeframe::M30,
        10 * DUR,
        &pda,
        SwingKind::High,
    );

    assert!(
        candidates.iter().all(|candidate| candidate.ts != 2 * DUR),
        "the old confirmed high was already taken and must not be reused"
    );
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.tf == Timeframe::H1 && candidate.ts == 8 * DUR),
        "the adjacent completed H1 candle must remain available as fallback"
    );
}

#[test]
fn fresh_adjacent_htf_reference_outranks_a_released_distant_retry() {
    let mut candidates = vec![
        PdaLiquidityCandidate {
            kind: SwingKind::High,
            price: 99.671,
            ts: 2 * DUR,
            tf: Timeframe::H1,
            scope: ReferenceScope::DistantLeftSide,
            rank: 0,
            previous_failed_extreme: Some(99.721),
        },
        PdaLiquidityCandidate {
            kind: SwingKind::High,
            price: 99.721,
            ts: 8 * DUR,
            tf: Timeframe::H1,
            scope: ReferenceScope::LocalNearPda,
            rank: 1,
            previous_failed_extreme: None,
        },
    ];

    sort_pda_liquidity_candidates(&mut candidates, SwingKind::High);

    assert_eq!(candidates[0].ts, 8 * DUR);
    assert_eq!(candidates[1].ts, 2 * DUR);
}

#[test]
fn inverse_corr_opposite_direction() {
    let (_, events) = current_rule_h1_smt(1.095, 1.295);
    let smts = extract_smt(&events);
    assert_eq!(smts.len(), 1, "should produce exactly 1 SMT");
    let d = smts[0];
    assert_eq!(d.relationship, Correlation::Negative);
    assert_eq!(d.candidate_direction, Direction::Bullish);
    assert_eq!(d.sweeper_symbol, "DXY");
}

// ---- Test 2: counter NotSwept (no penetration) ----

#[test]
fn counter_not_swept_no_penetration() {
    let (_, events) = current_rule_h1_smt(1.095, 1.295);
    let smts = extract_smt(&events);
    assert_eq!(smts.len(), 1);
    let counter_ref = smts[0]
        .liquidity_refs
        .iter()
        .find(|r| r.symbol == "EURUSD")
        .unwrap();
    assert_eq!(counter_ref.status, LiquidityRefStatus::NotSwept);
}

#[test]
fn live_counter_bar_arriving_after_dxy_still_resolves_smt() {
    // Current detection explicitly queues the DXY HTF close until every
    // symbol has the exact aligned child interval. The fixture exercises the
    // resolved result; queue readiness itself is deterministic and does not
    // depend on feed arrival order.
    let (mut engine, events) = current_rule_h1_smt(1.095, 1.295);
    let smt = extract_smt(&events)[0];
    assert_eq!(smt.sweeper_symbol, "DXY");
    let htf = Bar {
        symbol: "DXY".into(),
        tf: Timeframe::H1,
        ts: 6 * DUR,
        open: 99.5,
        high: 99.7,
        low: 98.5,
        close: 99.2,
        volume: 2.0,
    };
    engine.queue_pda_htf_close(&htf);
    assert_eq!(engine.take_ready_pda_htf_closes().len(), 1);
}

#[test]
fn mtf_touch_does_not_publish_before_htf_close() {
    let mut engine = SmtEngine::new();
    engine.set_watchlist(make_wl(vec![
        "DXY".into(),
        "EURUSD".into(),
        "GBPUSD".into(),
    ]));
    let mut events = Vec::new();
    for (symbol, high, low) in [
        ("DXY", 10.0, 8.9),
        ("EURUSD", 20.8, 20.1),
        ("GBPUSD", 30.8, 30.1),
    ] {
        events.extend(engine.on_closed_bar(&Bar {
            symbol: symbol.into(),
            tf: TF,
            ts: 4 * DUR,
            open: (high + low) / 2.0,
            high,
            low,
            close: high,
            volume: 1.0,
        }));
    }
    assert!(extract_smt(&events).is_empty());
    assert!(engine.divergences.is_empty());
}

#[test]
fn near_equal_below_reference_is_not_swept_without_tolerance() {
    let counter = bar("EURUSD", 7, 1.0820, 1.0899, 1.0810, 1.0898);
    assert_eq!(
        check_sweep(&counter, 1.0900, SwingKind::High),
        LiquidityRefStatus::NotSwept
    );
}

#[test]
fn exact_equal_high_or_low_is_swept_regardless_of_close() {
    let high = bar("EURUSD", 7, 1.0820, 1.0900, 1.0810, 1.0815);
    let low = bar("DXY", 7, 1.0812, 1.0825, 1.0800, 1.0810);

    assert_eq!(
        check_sweep(&high, 1.0900, SwingKind::High),
        LiquidityRefStatus::Swept
    );
    assert_eq!(
        check_sweep(&low, 1.0800, SwingKind::Low),
        LiquidityRefStatus::Swept
    );
}

// ---- Tests 4-7: C2 three cases + Invalidated ----

/// Helper: build a bullish SMT scenario where C1 and SMT K have specific
/// OHLC values, then feed bars and return the divergence.
///
/// DXY (sweeper, Inverse) sweeps a prior swing low -> Bullish.
/// EURUSD (counter, Inverse) does not sweep its prior swing high.
fn bullish_smt_scenario(
    c1_low: f64,
    c1_high: f64,
    c1_close: f64,
    smt_k_low: f64,
    smt_k_high: f64,
    smt_k_close: f64,
    k1_low: f64,
    k1_high: f64,
    k1_close: f64,
) -> Option<SmtDivergence> {
    let mut e = SmtEngine::new();
    let wl = make_wl(vec!["DXY".into(), "EURUSD".into()]);
    e.set_watchlist(wl.clone());
    let mut bars = Vec::new();
    // DXY (sweeper, Inverse): swing low at center=2, price 1.0800
    bars.extend(swing_low_5("DXY", 2, 1.0800));
    // EURUSD (counter, Inverse): swing high at center=2, price 1.0900
    bars.extend(swing_high_5("EURUSD", 2, 1.0900));
    // DXY bars: C1-1 (bar 5), C1 (bar 6), SMT K (bar 7), K+1 (bar 8), K+2 (bar 9)
    bars.push(bar("DXY", 5, 1.0830, 1.0840, 1.0825, 1.0835));
    bars.push(bar("DXY", 6, c1_close, c1_high, c1_low, c1_close));
    bars.push(bar(
        "DXY",
        7,
        smt_k_close,
        smt_k_high,
        smt_k_low,
        smt_k_close,
    ));
    bars.push(bar("DXY", 8, k1_close, k1_high, k1_low, k1_close));
    bars.push(bar("DXY", 9, 1.0820, 1.0830, 1.0815, 1.0825));
    // EURUSD counter: highs below 1.0900 -> NotSwept
    bars.push(bar("EURUSD", 5, 1.0830, 1.0840, 1.0825, 1.0835));
    bars.push(bar("EURUSD", 6, 1.0820, 1.0830, 1.0815, 1.0825));
    bars.push(bar("EURUSD", 7, 1.0820, 1.0835, 1.0810, 1.0815));
    bars.push(bar("EURUSD", 8, 1.0825, 1.0835, 1.0820, 1.0830));
    bars.push(bar("EURUSD", 9, 1.0830, 1.0840, 1.0825, 1.0835));
    bars.sort_by_key(|b| (b.ts, b.symbol.clone()));
    // These tests isolate the DXY-owned C1/C2/C3 state machine. HTF/PDA
    // discovery is covered separately through `detect_pda_htf_close`; build
    // the already-confirmed divergence from the canonical MTF evidence here
    // so this fixture does not depend on any retired HTF discovery path.
    feed_all(&mut e, &bars);
    let dxy_ref = candle_ref(e.buckets.get(&("DXY".into(), TF))?.bar_at(2 * DUR)?);
    let eu_ref = candle_ref(e.buckets.get(&("EURUSD".into(), TF))?.bar_at(2 * DUR)?);
    let eu_sweep = candle_ref(e.buckets.get(&("EURUSD".into(), TF))?.bar_at(7 * DUR)?);
    let counters = vec![CounterInfo {
        symbol: "EURUSD".into(),
        correlation: Correlation::Negative,
        expected_kind: SwingKind::High,
        status: LiquidityRefStatus::NotSwept,
        ref_price: eu_ref.high,
        ref_ts: 2 * DUR,
        mtf_ref_candle: eu_ref,
        mtf_sweep_candle: eu_sweep,
    }];
    let mut divergence = e.build_multi_smt(
        &wl,
        "DXY",
        &Swing {
            ts: 7 * DUR,
            price: 1.0800,
            kind: SwingKind::Low,
        },
        Direction::Bullish,
        Timeframe::H1,
        TF,
        Timeframe::H1,
        7 * DUR,
        1.0800,
        2 * DUR,
        counters,
    )?;
    divergence.mtf_ref_candle = Some(dxy_ref.clone());
    if let Some(liquidity) = divergence
        .liquidity_refs
        .iter_mut()
        .find(|liquidity| liquidity.symbol == "DXY")
    {
        liquidity.mtf_ref_candle = Some(dxy_ref);
    }
    Some(divergence)
}

#[test]
fn c2_case1_smt_k_close_in_c1_range() {
    // SMT K close in C1 [low, high] -> C2=SMT K, case=1
    let d = bullish_smt_scenario(
        1.0805, 1.0820, 1.0812, // C1
        1.0790, 1.0825, 1.0810, // SMT K (sweep: low<1.0800, close>1.0800)
        1.0810, 1.0825, 1.0818, // K+1 (not needed for case 1)
    )
    .expect("should produce SMT");
    assert_eq!(sweeper_chain(&d).c2_case, Some(1));
    assert_eq!(sweeper_chain(&d).c2_candle.as_ref().unwrap().ts, 7 * DUR); // C2 = SMT K
    assert_eq!(
        sweeper_chain(&d).detection_state,
        SmtDetectionState::C3Entry
    );
}

#[test]
fn c2_case2_k_plus_1_reclaims_smt_k_close_without_new_extreme() {
    // SMT K close outside C1 (reverse, < C1.low), K+1 reclaims SMT K's
    // close but deliberately remains below C1.low.
    // C1: [1.0820, 1.0830], SMT K: [1.0790, 1.0810] close=1.0805 (< C1.low)
    // K+1 close=1.0806 above SMT K close and low remains above SMT K low.
    let d = bullish_smt_scenario(
        1.0820, 1.0830, 1.0825, // C1
        1.0790, 1.0810, 1.0805, // SMT K: sweep (low<1.0800, close>1.0800)
        1.0795, 1.0810, 1.0806, // K+1: SMT K close reclaim only
    )
    .expect("should produce SMT");
    assert_eq!(sweeper_chain(&d).c2_case, Some(2));
    assert_eq!(sweeper_chain(&d).c2_candle.as_ref().unwrap().ts, 8 * DUR); // C2 = K+1
    assert_eq!(
        sweeper_chain(&d).detection_state,
        SmtDetectionState::C3Entry
    );
}

#[test]
fn case2_gu_0813_forms_c2_then_fails_c3_after_losing_smt_k_close() {
    let mut engine = SmtEngine::new();
    let symbol = "GBPUSD";
    let c1 = CandleRef {
        ts: 6 * DUR,
        open: 1.34958,
        high: 1.34978,
        low: 1.34918,
        close: 1.34943,
    };
    let smt_k = CandleRef {
        ts: 7 * DUR,
        open: 1.34944,
        high: 1.35008,
        low: 1.34743,
        close: 1.34826,
    };
    let mut bucket = SmtBucket::new();
    bucket
        .bars
        .push_back(bar(symbol, 8, 1.34826, 1.34883, 1.34755, 1.34828));
    bucket
        .bars
        .push_back(bar(symbol, 9, 1.34829, 1.34837, 1.34778, 1.34790));
    engine.buckets.insert((symbol.into(), TF), bucket);

    let (c2, case, c3, state) =
        engine.evaluate_c2_c3(symbol, TF, smt_k.ts, Direction::Bullish, &c1, &smt_k);
    assert_eq!(case, Some(2));
    assert_eq!(c2.as_ref().map(|candle| candle.ts), Some(8 * DUR));
    assert_eq!(c3.as_ref().map(|candle| candle.ts), Some(9 * DUR));
    assert_eq!(state, SmtDetectionState::Invalidated);
}

#[test]
fn case2_gu_0815_forms_c2_and_c3_while_remaining_outside_c1() {
    let mut engine = SmtEngine::new();
    let symbol = "GBPUSD";
    let c1 = CandleRef {
        ts: 6 * DUR,
        open: 1.35490,
        high: 1.35531,
        low: 1.35385,
        close: 1.35394,
    };
    let smt_k = CandleRef {
        ts: 7 * DUR,
        open: 1.35393,
        high: 1.35396,
        low: 1.35270,
        close: 1.35302,
    };
    let mut bucket = SmtBucket::new();
    bucket
        .bars
        .push_back(bar(symbol, 8, 1.35302, 1.35332, 1.35286, 1.35324));
    bucket
        .bars
        .push_back(bar(symbol, 9, 1.35325, 1.35350, 1.35298, 1.35348));
    engine.buckets.insert((symbol.into(), TF), bucket);

    let (c2, case, c3, state) =
        engine.evaluate_c2_c3(symbol, TF, smt_k.ts, Direction::Bullish, &c1, &smt_k);
    assert_eq!(case, Some(2));
    assert_eq!(c2.as_ref().map(|candle| candle.ts), Some(8 * DUR));
    assert_eq!(c3.as_ref().map(|candle| candle.ts), Some(9 * DUR));
    assert_eq!(state, SmtDetectionState::C3Entry);
}

#[test]
fn case2_reclaim_with_new_adverse_extreme_is_not_c2() {
    let mut engine = SmtEngine::new();
    let symbol = "DXY";
    let c1 = CandleRef {
        ts: 6 * DUR,
        open: 1.0825,
        high: 1.0830,
        low: 1.0820,
        close: 1.0825,
    };
    let smt_k = CandleRef {
        ts: 7 * DUR,
        open: 1.0805,
        high: 1.0810,
        low: 1.0790,
        close: 1.0805,
    };
    let mut bucket = SmtBucket::new();
    bucket
        .bars
        .push_back(bar(symbol, 8, 1.0800, 1.0810, 1.0789, 1.0806));
    engine.buckets.insert((symbol.into(), TF), bucket);

    let (c2, case, c3, state) =
        engine.evaluate_c2_c3(symbol, TF, smt_k.ts, Direction::Bullish, &c1, &smt_k);
    assert!(c2.is_none());
    assert!(c3.is_none());
    assert_eq!(case, None);
    assert_eq!(state, SmtDetectionState::Invalidated);
}

#[test]
fn bearish_case2_mirrors_smt_k_close_and_extreme_rules() {
    let mut engine = SmtEngine::new();
    let symbol = "DXY";
    let c1 = CandleRef {
        ts: 6 * DUR,
        open: 1.0805,
        high: 1.0810,
        low: 1.0800,
        close: 1.0805,
    };
    let smt_k = CandleRef {
        ts: 7 * DUR,
        open: 1.0815,
        high: 1.0830,
        low: 1.0810,
        close: 1.0820,
    };
    let mut bucket = SmtBucket::new();
    bucket
        .bars
        .push_back(bar(symbol, 8, 1.0820, 1.0829, 1.0815, 1.0819));
    bucket
        .bars
        .push_back(bar(symbol, 9, 1.0819, 1.0828, 1.0810, 1.0818));
    engine.buckets.insert((symbol.into(), TF), bucket);

    let (c2, case, c3, state) =
        engine.evaluate_c2_c3(symbol, TF, smt_k.ts, Direction::Bearish, &c1, &smt_k);
    assert_eq!(case, Some(2));
    assert_eq!(c2.as_ref().map(|candle| candle.ts), Some(8 * DUR));
    assert_eq!(c3.as_ref().map(|candle| candle.ts), Some(9 * DUR));
    assert_eq!(state, SmtDetectionState::C3Entry);
}

#[test]
fn c2_case3_close_outside_c1_direction_consistent() {
    // Bullish: SMT K close > C1.high -> case 3
    let d = bullish_smt_scenario(
        1.0801, 1.0810, 1.0805, // C1 (strictly above the 1.0800 ref)
        1.0790, 1.0825, 1.0815, // SMT K: sweep, close > C1.high
        1.0815, 1.0825, 1.0820, // K+1 (not needed)
    )
    .expect("should produce SMT");
    assert_eq!(sweeper_chain(&d).c2_case, Some(3));
    assert_eq!(sweeper_chain(&d).c2_candle.as_ref().unwrap().ts, 7 * DUR); // C2 = SMT K
    assert_eq!(
        sweeper_chain(&d).detection_state,
        SmtDetectionState::C3Entry
    );
}

#[test]
fn c2_case3_reverse_direction_not_confirmed() {
    // Bullish: SMT K close < C1.low (reverse), K+1 also fails all cases
    // C1: [1.0820, 1.0835]
    // SMT K: low=1.0790, high=1.0810, close=1.0805 (reverse, < C1.low)
    // K+1: close=1.0780, < SMT K low(1.0790) = continued reverse (down)
    //   check_c2_bar: not in C1 range, not > C1.high -> None
    //   case 2 (direction-aware): 1.0780 < SMT K low(1.0790) -> fails
    //   -> Invalidated
    // Exercise the C2 evaluator directly. In a full HTF scenario, a K+1
    // that makes a lower low becomes the actual SMT K because SMT K is the
    // extreme of the complete HTF interval.
    let mut engine = SmtEngine::new();
    let symbol = "DXY";
    let c1 = CandleRef {
        ts: 6 * DUR,
        open: 1.0825,
        high: 1.0835,
        low: 1.0820,
        close: 1.0825,
    };
    let smt_k = CandleRef {
        ts: 7 * DUR,
        open: 1.0805,
        high: 1.0810,
        low: 1.0790,
        close: 1.0805,
    };
    let mut bucket = SmtBucket::new();
    bucket.bars.push_back(Bar {
        symbol: symbol.into(),
        tf: TF,
        ts: 8 * DUR,
        open: 1.0800,
        high: 1.0810,
        low: 1.0770,
        close: 1.0780,
        volume: 1.0,
    });
    engine.buckets.insert((symbol.into(), TF), bucket);
    let (c2, case, _, state) =
        engine.evaluate_c2_c3(symbol, TF, smt_k.ts, Direction::Bullish, &c1, &smt_k);
    assert_eq!(state, SmtDetectionState::Invalidated);
    assert_eq!(case, None);
    assert!(c2.is_none());
}

#[test]
fn c2_case2_bullish_k1_breaks_above_smt_k() {
    // Direction-aware case 2 (new rule): Bullish SMT, SMT K close is
    // reverse (< C1.low), K+1 close breaks ABOVE SMT K high = bullish
    // reversal (price turned up strongly). Old rule rejected this
    // (close outside SMT K range); new rule accepts it as case 2.
    // C1: [1.0820, 1.0835]
    // SMT K: [1.0790, 1.0810] close=1.0805 (< C1.low, reverse)
    // K+1: close=1.0825 > SMT K high(1.0810) = broke above = reversal
    let d = bullish_smt_scenario(
        1.0820, 1.0835, 1.0825, // C1
        1.0790, 1.0810, 1.0805, // SMT K: sweep (low<1.0800, close>1.0800)
        1.0815, 1.0830, 1.0825, // K+1: broke above SMT K high
    )
    .expect("should produce SMT");
    let chain = sweeper_chain(&d);
    assert_eq!(chain.c2_case, Some(2));
    assert_eq!(chain.c2_candle.as_ref().unwrap().ts, 8 * DUR); // C2 = K+1
    assert_eq!(chain.detection_state, SmtDetectionState::C3Entry);
}

// ---- Test 8: C3 = C2+1 ----
#[test]
fn c2_tighten_k_plus_1_in_c1_range_is_not_case1() {
    // Tightening (m5c2.v2): case 1/3 are only matched on SMT K.
    // SMT K close is outside C1 (reverse), K+1 close lands in BOTH the
    // C1 range and the SMT K range. Before tightening this was case 1
    // (matched on K+1). Now it must be case 2 (C2 = SMT K+1), because
    // K+1 is never case 1/3 anymore.
    // C1: [1.0810, 1.0825], SMT K: [1.0790, 1.0820] close=1.0805 (< C1.low)
    // K+1 close=1.0815: in C1 [1.0810,1.0825] AND in SMT K [1.0790,1.0820]
    let d = bullish_smt_scenario(
        1.0815, 1.0825, 1.0810, // C1
        1.0790, 1.0820, 1.0805, // SMT K: sweep (low<1.0800, close>1.0800)
        1.0808, 1.0822, 1.0815, // K+1: close in both C1 and SMT K range
    )
    .expect("should produce SMT");
    let chain = sweeper_chain(&d);
    assert_eq!(chain.c2_case, Some(2));
    assert_eq!(chain.c2_candle.as_ref().unwrap().ts, 8 * DUR); // C2 = K+1
    assert_eq!(chain.detection_state, SmtDetectionState::C3Entry);
}

#[test]
fn c3_is_c2_plus_one() {
    // Case 1: C2 = SMT K (bar 7), C3 = bar 8
    let d = bullish_smt_scenario(
        1.0805, 1.0820, 1.0812, // C1
        1.0790, 1.0825, 1.0810, // SMT K
        1.0810, 1.0825, 1.0818, // K+1 (becomes C3)
    )
    .expect("should produce SMT");
    let c2_ts = sweeper_chain(&d).c2_candle.as_ref().unwrap().ts;
    let c3_ts = sweeper_chain(&d).c3_candle.as_ref().unwrap().ts;
    assert_eq!(c3_ts, c2_ts + DUR);
    assert_eq!(
        sweeper_chain(&d).detection_state,
        SmtDetectionState::C3Entry
    );
}

#[test]
fn c3_new_adverse_extreme_invalidates_after_c2() {
    let mut engine = SmtEngine::new();
    let symbol = "DXY";
    let c1 = CandleRef {
        ts: 6 * DUR,
        open: 1.0805,
        high: 1.0820,
        low: 1.0805,
        close: 1.0812,
    };
    let smt_k = CandleRef {
        ts: 7 * DUR,
        open: 1.0810,
        high: 1.0825,
        low: 1.0790,
        close: 1.0810,
    };
    let mut bucket = SmtBucket::new();
    bucket.bars.push_back(Bar {
        symbol: symbol.into(),
        tf: TF,
        ts: 8 * DUR,
        open: 1.0810,
        high: 1.0826,
        low: 1.0789,
        close: 1.0815,
        volume: 1.0,
    });
    engine.buckets.insert((symbol.into(), TF), bucket);
    let (c2, case, c3, state) =
        engine.evaluate_c2_c3(symbol, TF, smt_k.ts, Direction::Bullish, &c1, &smt_k);
    assert_eq!(case, Some(1));
    assert_eq!(c2.as_ref().map(|candle| candle.ts), Some(smt_k.ts));
    assert_eq!(c3.as_ref().map(|candle| candle.ts), Some(8 * DUR));
    assert_eq!(state, SmtDetectionState::Invalidated);
}

// ---- Test 12: deferred C2/C3 upgrade (Option B) ----

#[test]
fn c2_deferred_upgrade_smtk_to_c2_to_c3() {
    // Case 2 deferred: when the HTF bar that triggers SMT detection
    // closes before all counter MTF bars arrive, detection waits for a
    // complete cross-symbol snapshot, then starts at C2Confirmed and
    // upgrades to C3Entry as MTF bars close.
    //
    // Bar setup (strict case 2):
    //   C1 (bar 6): [1.0820, 1.0830] close=1.0825
    //   SMT K (bar 7): [1.0790, 1.0810] close=1.0805 (sweep + reverse)
    //   K+1 (bar 8): [1.0795, 1.0825] close=1.0822 (reclaims C1.low)
    //   K+2 (bar 9): filler
    //
    // Feeding order simulates real-time: H1 bar 8 is fed BEFORE M30 bar
    // 8. Detection must wait instead of choosing an incomplete HTF
    // interval or a stale counter bar.

    let mut e = SmtEngine::new();
    e.set_watchlist(make_wl(vec!["DXY".into(), "EURUSD".into()]));

    // DXY bars: swing low (0-4) + chain bars (5-9)
    let dxy_bars: Vec<Bar> = {
        let mut v = swing_low_5("DXY", 2, 1.0800);
        v.push(bar("DXY", 5, 1.0830, 1.0840, 1.0825, 1.0835));
        v.push(bar("DXY", 6, 1.0825, 1.0830, 1.0820, 1.0825)); // C1
        v.push(bar("DXY", 7, 1.0805, 1.0810, 1.0790, 1.0805)); // SMT K
        v.push(bar("DXY", 8, 1.0808, 1.0825, 1.0795, 1.0822)); // K+1
        v.push(bar("DXY", 9, 1.0820, 1.0830, 1.0815, 1.0825)); // K+2
        v
    };
    // EURUSD bars: swing high (0-4) + chain bars (5-9)
    let eur_bars: Vec<Bar> = {
        let mut v = swing_high_5("EURUSD", 2, 1.0900);
        v.push(bar("EURUSD", 5, 1.0830, 1.0840, 1.0825, 1.0835));
        v.push(bar("EURUSD", 6, 1.0820, 1.0830, 1.0815, 1.0825));
        v.push(bar("EURUSD", 7, 1.0820, 1.0835, 1.0810, 1.0815));
        v.push(bar("EURUSD", 8, 1.0825, 1.0835, 1.0820, 1.0830));
        v.push(bar("EURUSD", 9, 1.0830, 1.0840, 1.0825, 1.0835));
        v
    };

    let mut all_events = Vec::new();

    // Phase 1: seed closed M30 bars through SMT K. Detection has already
    // been confirmed by its closed HTF/PDA layer, but case 2 must wait for
    // SMT K+1.
    let mut phase1: Vec<Bar> = Vec::new();
    phase1.extend(dxy_bars.iter().take(8).cloned());
    phase1.extend(eur_bars.iter().take(8).cloned());
    phase1.sort_by_key(|b| (b.ts, b.symbol.clone()));
    feed_all(&mut e, &phase1);
    let wl = e.watchlist.clone().unwrap();
    let counter_ref = candle_ref(e.buckets[&("EURUSD".into(), TF)].bar_at(2 * DUR).unwrap());
    let counter_sweep = candle_ref(e.buckets[&("EURUSD".into(), TF)].bar_at(7 * DUR).unwrap());
    let mut pending = e
        .build_multi_smt(
            &wl,
            "DXY",
            &Swing {
                ts: 7 * DUR,
                price: 1.0800,
                kind: SwingKind::Low,
            },
            Direction::Bullish,
            Timeframe::H1,
            TF,
            Timeframe::H1,
            7 * DUR,
            1.0800,
            2 * DUR,
            vec![CounterInfo {
                symbol: "EURUSD".into(),
                correlation: Correlation::Negative,
                expected_kind: SwingKind::High,
                status: LiquidityRefStatus::NotSwept,
                ref_price: counter_ref.high,
                ref_ts: 2 * DUR,
                mtf_ref_candle: counter_ref,
                mtf_sweep_candle: counter_sweep,
            }],
        )
        .unwrap();
    assert_eq!(
        sweeper_chain(&pending).detection_state,
        SmtDetectionState::SmtKDetected
    );
    pending.htf_pda_ref = Some(PdaRef {
        kind: "fvg".into(),
        id: "deferred-pda".into(),
        tf: Timeframe::H1,
        direction: Direction::Bullish,
        price_low: 1.0790,
        price_high: 1.0830,
        ts_open: 0,
        ts_confirm: DUR,
        exit_ts: None,
        ts_filled: None,
    });
    e.reserve_pda("deferred-pda", &pending.id);
    all_events.extend(e.emit_smt(pending));

    // Phase 2: feed M30 bar 8. Once both symbols are present the pending
    // sweep resolves at C2Confirmed; C3 is not available yet.
    all_events.extend(e.on_closed_bar(&dxy_bars[8]));
    all_events.extend(e.on_closed_bar(&eur_bars[8]));

    // Phase 3: feed M30 bar 9 (upgrade_pending_chains fires).
    // DXY: C3 available -> C3Entry.
    all_events.extend(e.on_closed_bar(&dxy_bars[9]));
    all_events.extend(e.on_closed_bar(&eur_bars[9]));

    // Extract DXY sweeper chain detection_state from ALL events.
    let states: Vec<SmtDetectionState> = all_events
        .iter()
        .filter_map(|ev| match ev {
            StructureEvent::New(IctStructure::SmtDivergence(d))
            | StructureEvent::Update(IctStructure::SmtDivergence(d)) => Some(d),
            _ => None,
        })
        .map(|d| sweeper_chain(d).detection_state)
        .collect();

    assert!(!states.is_empty(), "should produce at least one SMT event");
    assert!(
        states.contains(&SmtDetectionState::C2Confirmed),
        "DXY chain should upgrade to C2Confirmed; states = {states:?}"
    );
    assert_eq!(
        *states.last().unwrap(),
        SmtDetectionState::C3Entry,
        "DXY chain should end as C3Entry; states = {states:?}"
    );
}

#[test]
fn first_valid_c3_deterministically_consumes_shared_pda_and_invalidates_other_reservation() {
    let mut engine = SmtEngine::new();
    engine.set_watchlist(make_wl(vec!["DXY".into(), "EURUSD".into()]));
    for source in [
        bar("DXY", 6, 1.0825, 1.0830, 1.0820, 1.0825),
        bar("EURUSD", 6, 1.0820, 1.0830, 1.0815, 1.0825),
        bar("DXY", 7, 1.0805, 1.0810, 1.0790, 1.0805),
        bar("EURUSD", 7, 1.0820, 1.0835, 1.0810, 1.0815),
    ] {
        engine.seed_bar(&source);
    }
    for id in ["a-earlier", "b-later"] {
        let divergence = pending_case2_divergence(id, "shared-pda");
        engine.reserve_pda("shared-pda", id);
        engine.emit_smt(divergence);
    }

    engine.on_closed_bar(&bar("DXY", 8, 1.0808, 1.0825, 1.0795, 1.0822));
    engine.on_closed_bar(&bar("EURUSD", 8, 1.0825, 1.0835, 1.0820, 1.0830));
    assert!(!engine.is_pda_consumed("shared-pda"));
    let events = engine.on_closed_bar(&bar("DXY", 9, 1.0820, 1.0830, 1.0815, 1.0825));

    assert!(engine.is_pda_consumed("shared-pda"));
    let winner = engine.divergences.get("a-earlier").unwrap();
    assert_eq!(
        sweeper_chain(winner).detection_state,
        SmtDetectionState::C3Entry
    );
    let loser = engine.divergences.get("b-later").unwrap();
    assert_eq!(
        sweeper_chain(loser).detection_state,
        SmtDetectionState::Invalidated
    );
    assert!(loser
        .invalidation_reasons
        .contains(&SmtInvalidationReason::PdaConsumedByOtherSmt));
    assert!(events.iter().any(|event| matches!(
        event,
        StructureEvent::Update(IctStructure::SmtDivergence(divergence))
            if divergence.id == "b-later"
                && sweeper_chain(divergence).detection_state == SmtDetectionState::Invalidated
    )));
}

#[test]
fn counter_catch_up_before_c2_invalidates_and_releases_pda() {
    let mut engine = SmtEngine::new();
    engine.set_watchlist(make_wl(vec!["DXY".into(), "EURUSD".into()]));
    for source in [
        bar("DXY", 6, 1.0825, 1.0830, 1.0820, 1.0825),
        bar("EURUSD", 6, 1.0820, 1.0830, 1.0815, 1.0825),
        bar("DXY", 7, 1.0805, 1.0810, 1.0790, 1.0805),
        bar("EURUSD", 7, 1.0820, 1.0835, 1.0810, 1.0815),
    ] {
        engine.seed_bar(&source);
    }
    let divergence = pending_case2_divergence("catch-up", "catch-up-pda");
    engine.reserve_pda("catch-up-pda", "catch-up");
    engine.emit_smt(divergence);

    engine.on_closed_bar(&bar("DXY", 8, 1.0808, 1.0825, 1.0795, 1.0822));
    engine.on_closed_bar(&bar("EURUSD", 8, 1.0825, 1.0900, 1.0820, 1.0830));

    let invalidated = engine.divergences.get("catch-up").unwrap();
    assert_eq!(
        sweeper_chain(invalidated).detection_state,
        SmtDetectionState::Invalidated
    );
    assert!(invalidated
        .invalidation_reasons
        .contains(&SmtInvalidationReason::AllCountersSwept));
    assert!(!engine.is_pda_consumed("catch-up-pda"));
}

#[test]
fn forming_window_waits_for_all_symbols_and_runs_once() {
    let mut engine = SmtEngine::new();
    engine.set_watchlist(make_wl(vec![
        "DXY".into(),
        "EURUSD".into(),
        "GBPUSD".into(),
    ]));
    for symbol in ["DXY", "EURUSD"] {
        let item = bar(symbol, 0, 1.0, 1.1, 0.9, 1.0);
        engine.on_closed_bar(&item);
        assert!(engine.take_ready_forming_htf_windows(&item).is_empty());
    }
    let gu = bar("GBPUSD", 0, 1.0, 1.1, 0.9, 1.0);
    engine.on_closed_bar(&gu);
    let ready = engine.take_ready_forming_htf_windows(&gu);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].0.tf, Timeframe::H1);
    assert_eq!(ready[0].0.ts, 0);
    assert_eq!(ready[0].1, DUR);
    assert!(engine.take_ready_forming_htf_windows(&gu).is_empty());
}

#[test]
fn counter_catch_up_cancels_provisional_even_after_early_c3() {
    let mut engine = SmtEngine::new();
    engine.set_watchlist(make_wl(vec!["DXY".into(), "EURUSD".into()]));
    let mut divergence = pending_case2_divergence("provisional-c3", "forming-pda");
    divergence.chains[0].detection_state = SmtDetectionState::C3Entry;
    divergence.htf_confirmed = false;
    engine.update_divergence(divergence);
    engine.reserve_pda("forming-pda", "provisional-c3");
    engine
        .provisional_windows
        .entry((Timeframe::H1, 0))
        .or_default()
        .insert("provisional-c3".into());

    let catch_up = bar("EURUSD", 9, 1.085, 1.095, 1.080, 1.090);
    let events = engine.on_closed_bar(&catch_up);
    let latest = extract_smt(&events)
        .into_iter()
        .find(|item| item.id == "provisional-c3")
        .expect("provisional C3 should be cancelled when the counter catches up");
    assert_eq!(
        sweeper_chain(latest).detection_state,
        SmtDetectionState::Invalidated
    );
    assert!(latest
        .invalidation_reasons
        .contains(&SmtInvalidationReason::AllCountersSwept));
    assert!(!engine.is_pda_consumed("forming-pda"));
    assert!(!engine
        .reserved_pdas
        .get("forming-pda")
        .is_some_and(|ids| ids.contains("provisional-c3")));
}

#[test]
fn provisional_reconciliation_emits_htf_formation_cancelled_at_market_time() {
    let mut engine = SmtEngine::new();
    let mut divergence = pending_case2_divergence("provisional-stale", "forming-pda");
    divergence.htf_confirmed = false;
    engine.update_divergence(divergence);
    engine.reserve_pda("forming-pda", "provisional-stale");
    engine
        .provisional_windows
        .entry((Timeframe::H1, 0))
        .or_default()
        .insert("provisional-stale".into());

    let market_ts = 10 * DUR;
    let events =
        engine.reconcile_provisional_window(Timeframe::H1, 0, Vec::new(), false, market_ts);
    let cancelled = extract_smt(&events)
        .into_iter()
        .find(|item| item.id == "provisional-stale")
        .expect("stale provisional SMT must emit a terminal update");
    assert!(cancelled
        .invalidation_reasons
        .contains(&SmtInvalidationReason::HtfFormationCancelled));
    assert_eq!(cancelled.invalidation_ts, Some(market_ts));
    assert!(!engine
        .reserved_pdas
        .get("forming-pda")
        .is_some_and(|ids| ids.contains("provisional-stale")));
}

// ---- Test 9: chain on sweeper ----

#[test]
fn chain_on_sweeper_symbol() {
    let d = bullish_smt_scenario(
        1.0805, 1.0820, 1.0812, // C1
        1.0790, 1.0825, 1.0810, // SMT K
        1.0810, 1.0825, 1.0818, // K+1
    )
    .expect("should produce SMT");
    assert_eq!(d.sweeper_symbol, "DXY");
    assert_eq!(d.trade_symbols, vec!["EURUSD"]);
    // Each involved symbol has its own chain (§2.2: DXY + EU/GU each)
    assert_eq!(
        d.chains.len(),
        2,
        "both sweeper and counter should have chains"
    );
    // Sweeper (DXY) chain
    assert!(sweeper_chain(&d).c1_candle.ts > 0);
    assert!(sweeper_chain(&d).smt_k_candle.ts > 0);
    assert!(sweeper_chain(&d).c2_candle.is_some());
    assert!(sweeper_chain(&d).c3_candle.is_some());
    // SMT K ts should be after C1 ts
    assert_eq!(
        sweeper_chain(&d).smt_k_candle.ts,
        sweeper_chain(&d).c1_candle.ts + DUR
    );
    // Counter (EURUSD) chain - independent C1/SMT K/C2/C3
    let counter = d
        .chains
        .iter()
        .find(|c| c.symbol == "EURUSD")
        .expect("counter chain");
    assert!(counter.c1_candle.ts > 0);
    assert!(counter.smt_k_candle.ts > 0);
    // Both chains share the same SMT K timestamp (divergence event)
    assert_eq!(counter.smt_k_candle.ts, sweeper_chain(&d).smt_k_candle.ts);
}

#[test]
fn historical_partial_chain_is_repaired_from_canonical_symbol_bars() {
    let complete = bullish_smt_scenario(
        1.0805, 1.0820, 1.0812, // C1
        1.0790, 1.0825, 1.0810, // SMT K
        1.0810, 1.0825, 1.0818, // K+1
    )
    .expect("should produce SMT");
    let mut engine = SmtEngine::new();
    engine.set_watchlist(make_wl(vec!["DXY".into(), "EURUSD".into()]));

    let mut bars = Vec::new();
    for chain in &complete.chains {
        let candles = [
            Some(&chain.c1_candle),
            Some(&chain.smt_k_candle),
            chain.c2_candle.as_ref(),
            chain.c3_candle.as_ref(),
        ];
        for candle in candles.into_iter().flatten() {
            bars.push(Bar {
                symbol: chain.symbol.clone(),
                tf: complete.comparison_timeframe,
                ts: candle.ts,
                open: candle.open,
                high: candle.high,
                low: candle.low,
                close: candle.close,
                volume: 1.0,
            });
        }
    }
    bars.sort_by_key(|bar| (bar.ts, bar.symbol.clone()));
    bars.dedup_by(|a, b| a.symbol == b.symbol && a.tf == b.tf && a.ts == b.ts);
    for bar in bars {
        let _ = engine.on_closed_bar(&bar);
    }

    let mut partial = complete.clone();
    partial
        .chains
        .retain(|chain| chain.symbol == partial.sweeper_symbol);
    assert_eq!(partial.chains.len(), 1);

    let repaired = engine
        .repair_missing_chains(&partial)
        .expect("counter chain should be reconstructed from real bars");
    assert_eq!(repaired.chains.len(), complete.symbol_set.len());
    assert!(repaired.chains.iter().any(|chain| chain.symbol == "EURUSD"));
    assert_eq!(
        repaired
            .chains
            .iter()
            .find(|chain| chain.symbol == "EURUSD")
            .unwrap()
            .smt_k_candle
            .ts,
        sweeper_chain(&complete).smt_k_candle.ts
    );
}

// ---- Test 10: strength labels ----

#[test]
fn strength_labels_bullish() {
    let d = bullish_smt_scenario(
        1.0805, 1.0820, 1.0812, // C1
        1.0790, 1.0825, 1.0810, // SMT K
        1.0810, 1.0825, 1.0818, // K+1
    )
    .expect("should produce SMT");
    // Sweeper (DXY) no longer carries a strength label; only the
    // counter does. Counter didn't sweep -> strong.
    assert!(d.strength.iter().all(|s| s.symbol != "DXY"));
    let counter_strength = d.strength.iter().find(|s| s.symbol == "EURUSD").unwrap();
    assert_eq!(counter_strength.label, "strong");
}

// ---- Test 7 (alt): TF filtering ----

#[test]
fn smt_only_on_30m_and_1h() {
    let mut e = SmtEngine::new();
    e.set_watchlist(make_wl(vec!["DXY".into(), "EURUSD".into()]));

    // Feed the same bullish SMT setup but on 5m -> should produce nothing
    let mut bars = Vec::new();
    let five = Timeframe::M5;
    let fdur = five.duration_ms();
    let mk = |sym: &str, idx: i64, o: f64, h: f64, l: f64, c: f64| Bar {
        symbol: sym.into(),
        tf: five,
        ts: idx * fdur,
        open: o,
        high: h,
        low: l,
        close: c,
        volume: 1.0,
    };
    bars.extend(swing_low_5("DXY", 2, 1.0800));
    // Override tf to M5
    for b in &mut bars {
        b.tf = five;
        b.ts = (b.ts / DUR) * fdur;
    }
    let mut eu_bars = swing_high_5("EURUSD", 2, 1.0900);
    for b in &mut eu_bars {
        b.tf = five;
        b.ts = (b.ts / DUR) * fdur;
    }
    bars.extend(eu_bars);
    let mut dxy2 = vec![
        mk("DXY", 5, 1.083, 1.084, 1.082, 1.083),
        mk("DXY", 6, 1.081, 1.082, 1.080, 1.081),
        mk("DXY", 7, 1.081, 1.082, 1.079, 1.081),
        mk("DXY", 8, 1.081, 1.082, 1.081, 1.081),
        mk("DXY", 9, 1.082, 1.083, 1.081, 1.082),
    ];
    let mut eu2 = vec![
        mk("EURUSD", 5, 1.083, 1.084, 1.082, 1.083),
        mk("EURUSD", 6, 1.082, 1.083, 1.081, 1.082),
        mk("EURUSD", 7, 1.082, 1.083, 1.081, 1.082),
        mk("EURUSD", 8, 1.083, 1.084, 1.082, 1.083),
        mk("EURUSD", 9, 1.083, 1.084, 1.082, 1.083),
    ];
    bars.append(&mut dxy2);
    bars.append(&mut eu2);
    bars.sort_by_key(|b| (b.ts, b.symbol.clone()));
    let events = feed_all(&mut e, &bars);
    let smts = extract_smt(&events);
    assert!(smts.is_empty(), "SMT should not be detected on 5m");
}

// ---- Test 11: HTF PDA ref computation ----

#[test]
fn htf_pda_ref_ob_excluded() {
    // OBs are no longer PDA candidates (user: "OB区先不作为pda").
    let smt_k = CandleRef {
        ts: 100,
        open: 1.0800,
        high: 1.0820,
        low: 1.0790,
        close: 1.0810,
    };
    let ob = IctStructure::OrderBlock(OrderBlock {
        id: "ob1".into(),
        symbol: "EURUSD".into(),
        tf: Timeframe::H1,
        direction: Direction::Bullish,
        ts_open: 50,
        ts_confirm: 90,
        price_low: 1.0785,
        price_high: 1.0815,
        state: ObState::Active,
    });
    assert!(compute_htf_pda_ref(&[ob], &smt_k).is_none());
}

#[test]
fn htf_pda_ref_no_overlap() {
    let smt_k = CandleRef {
        ts: 100,
        open: 1.0800,
        high: 1.0820,
        low: 1.0790,
        close: 1.0810,
    };
    let ob = IctStructure::OrderBlock(OrderBlock {
        id: "ob1".into(),
        symbol: "EURUSD".into(),
        tf: Timeframe::H1,
        direction: Direction::Bullish,
        ts_open: 50,
        ts_confirm: 90,
        price_low: 1.0900, // no overlap with [1.0790, 1.0820]
        price_high: 1.0950,
        state: ObState::Active,
    });
    assert!(compute_htf_pda_ref(&[ob], &smt_k).is_none());
}

#[test]
fn htf_pda_ref_at_ts_consistency() {
    // Zone confirmed AFTER smt_k.ts -> should NOT match (no look-ahead)
    let smt_k = CandleRef {
        ts: 100,
        open: 1.0800,
        high: 1.0820,
        low: 1.0790,
        close: 1.0810,
    };
    let ob = IctStructure::OrderBlock(OrderBlock {
        id: "ob1".into(),
        symbol: "EURUSD".into(),
        tf: Timeframe::H1,
        direction: Direction::Bullish,
        ts_open: 50,
        ts_confirm: 150, // > smt_k.ts (100) -> future, should be excluded
        price_low: 1.0785,
        price_high: 1.0815,
        state: ObState::Active,
    });
    assert!(compute_htf_pda_ref(&[ob], &smt_k).is_none());
}

#[test]
fn htf_pda_ref_fvg_mitigated50() {
    let smt_k = CandleRef {
        ts: 100,
        open: 1.0800,
        high: 1.0820,
        low: 1.0790,
        close: 1.0810,
    };
    let fvg = IctStructure::Fvg(Fvg {
        id: "fvg1".into(),
        symbol: "EURUSD".into(),
        tf: Timeframe::H1,
        direction: Direction::Bullish,
        ts_open: 50,
        ts_confirm: 80,
        price_low: 1.0795,
        price_high: 1.0815,
        state: FvgState::Mitigated50,
        ts_filled: None,
        consumed_exit_ts: None,
    });
    let r = compute_htf_pda_ref(&[fvg], &smt_k).unwrap();
    assert_eq!(r.kind, "fvg");
    assert_eq!(r.id, "fvg1");
    assert_eq!(r.price_low, 1.0795);
    assert_eq!(r.price_high, 1.0815);
    assert_eq!(r.ts_open, 50);
    assert_eq!(r.ts_confirm, 80);
    assert_eq!(r.direction, Direction::Bullish);
}

#[test]
fn pre_c2_invalidated_smt_releases_its_pda_reservation() {
    let mut engine = SmtEngine::new();
    let mut divergence = bullish_smt_scenario(
        1.0800, 1.0810, 1.0805, 1.0790, 1.0825, 1.0815, 1.0815, 1.0825, 1.0820,
    )
    .expect("should produce SMT");
    divergence.htf_pda_ref = Some(PdaRef {
        kind: "fvg".into(),
        id: "pda-release-test".into(),
        tf: Timeframe::H1,
        direction: Direction::Bullish,
        price_low: 1.0790,
        price_high: 1.0830,
        ts_open: 0,
        ts_confirm: DUR,
        exit_ts: None,
        ts_filled: None,
    });
    let divergence_id = divergence.id.clone();
    for chain in &mut divergence.chains {
        chain.detection_state = SmtDetectionState::SmtKDetected;
        chain.c2_candle = None;
        chain.c2_case = None;
        chain.c3_candle = None;
    }
    engine.reserve_pda("pda-release-test", &divergence_id);
    engine.divergences.insert(divergence_id.clone(), divergence);

    engine.invalidate_one(&divergence_id);

    assert!(!engine.is_pda_consumed("pda-release-test"));
    assert_eq!(
        sweeper_chain(engine.divergences.get(&divergence_id).unwrap()).detection_state,
        SmtDetectionState::Invalidated
    );
}

#[test]
fn pda_snapshot_audit_rejects_a_stale_aggregated_reference() {
    let mut engine = SmtEngine::new();
    let htf = Timeframe::H1;
    let htf_dur = htf.duration_ms();
    let htf_bar = |symbol: &str, ts: i64, high: f64, low: f64, close: f64| Bar {
        symbol: symbol.into(),
        tf: htf,
        ts,
        open: close,
        high,
        low,
        close,
        volume: 1.0,
    };
    for bar in [
        htf_bar("DXY", 0, 11.0, 10.0, 10.5),
        htf_bar("EURUSD", 0, 20.0, 19.0, 19.5),
        // DXY strictly sweeps/reclaims the prior low. EURUSD does not
        // sweep its inverse-correlated prior high.
        htf_bar("DXY", htf_dur, 11.0, 9.0, 10.5),
        htf_bar("EURUSD", htf_dur, 19.5, 18.5, 19.0),
    ] {
        let bucket = engine
            .buckets
            .entry((bar.symbol.clone(), htf))
            .or_insert_with(SmtBucket::new);
        bucket.bars.push_back(bar);
    }
    let mtf = Timeframe::M30;
    for bar in [
        Bar {
            symbol: "DXY".into(),
            tf: mtf,
            ts: 0,
            open: 10.5,
            high: 11.0,
            low: 10.0,
            close: 10.5,
            volume: 1.0,
        },
        Bar {
            symbol: "EURUSD".into(),
            tf: mtf,
            ts: 0,
            open: 19.5,
            high: 20.0,
            low: 19.0,
            close: 19.5,
            volume: 1.0,
        },
        Bar {
            symbol: "DXY".into(),
            tf: mtf,
            ts: DUR,
            open: 10.6,
            high: 10.9,
            low: 10.1,
            close: 10.5,
            volume: 1.0,
        },
        Bar {
            symbol: "EURUSD".into(),
            tf: mtf,
            ts: DUR,
            open: 19.4,
            high: 19.8,
            low: 19.1,
            close: 19.5,
            volume: 1.0,
        },
        Bar {
            symbol: "DXY".into(),
            tf: mtf,
            ts: htf_dur,
            open: 10.5,
            high: 11.0,
            low: 9.0,
            close: 10.5,
            volume: 1.0,
        },
        Bar {
            symbol: "EURUSD".into(),
            tf: mtf,
            ts: htf_dur,
            open: 19.0,
            high: 19.5,
            low: 18.5,
            close: 19.0,
            volume: 1.0,
        },
        Bar {
            symbol: "DXY".into(),
            tf: mtf,
            ts: htf_dur + DUR,
            open: 10.4,
            high: 10.8,
            low: 9.5,
            close: 10.5,
            volume: 1.0,
        },
        Bar {
            symbol: "EURUSD".into(),
            tf: mtf,
            ts: htf_dur + DUR,
            open: 18.9,
            high: 19.4,
            low: 18.6,
            close: 19.0,
            volume: 1.0,
        },
    ] {
        engine
            .buckets
            .entry((bar.symbol.clone(), mtf))
            .or_insert_with(SmtBucket::new)
            .bars
            .push_back(bar);
    }

    let candle = CandleRef {
        ts: htf_dur,
        open: 10.5,
        high: 11.0,
        low: 9.0,
        close: 10.5,
    };
    let mut divergence = SmtDivergence {
        id: "snapshot-audit".into(),
        watchlist_id: "test".into(),
        rule_version: RULE_VERSION.into(),
        symbol_set: vec!["DXY".into(), "EURUSD".into()],
        relationship: Correlation::Negative,
        context_timeframe: htf,
        comparison_timeframe: Timeframe::M30,
        observation_window: (0, 2 * htf_dur),
        htf_confirmed: true,
        reference_scope: ReferenceScope::DistantLeftSide,
        liquidity_refs: vec![
            LiquidityRef {
                symbol: "DXY".into(),
                ref_price: 10.0,
                ref_ts: 0,
                side: LiquiditySide::SellSide,
                status: LiquidityRefStatus::Swept,
                tf: htf,
                mtf_ref_candle: None,
                mtf_sweep_candle: None,
            },
            LiquidityRef {
                symbol: "EURUSD".into(),
                ref_price: 20.0,
                ref_ts: 0,
                side: LiquiditySide::BuySide,
                status: LiquidityRefStatus::NotSwept,
                tf: htf,
                mtf_ref_candle: None,
                mtf_sweep_candle: None,
            },
        ],
        confluence_refs: Vec::new(),
        candidate_direction: Direction::Bullish,
        sweeper_symbol: "DXY".into(),
        trade_symbols: vec!["EURUSD".into()],
        strength: Vec::new(),
        chains: vec![SymbolChain {
            symbol: "DXY".into(),
            c1_candle: CandleRef {
                ts: 0,
                ..candle.clone()
            },
            smt_k_candle: candle.clone(),
            c2_candle: Some(candle.clone()),
            c3_candle: None,
            c2_case: Some(1),
            detection_state: SmtDetectionState::C2Confirmed,
        }],
        invalidation_reasons: Vec::new(),
        invalidation_ts: None,
        htf_pda_ref: Some(PdaRef {
            kind: "fvg".into(),
            id: "audit-pda".into(),
            tf: htf,
            direction: Direction::Bullish,
            price_low: 8.0,
            price_high: 21.0,
            ts_open: -2 * htf_dur,
            ts_confirm: -htf_dur,
            exit_ts: None,
            ts_filled: None,
        }),
        mtf_ref_candle: None,
    };

    assert_eq!(engine.pda_snapshot_matches_history(&divergence), Some(true));
    assert_eq!(
        engine.pda_history_invalidation_reasons(&divergence),
        Some(Vec::new())
    );
    assert_eq!(
        engine.pda_reference_matches_history(&divergence),
        Some(true)
    );
    {
        let counter_sweep = engine
            .buckets
            .get_mut(&("EURUSD".into(), mtf))
            .unwrap()
            .bars
            .iter_mut()
            .find(|bar| bar.ts == htf_dur)
            .unwrap();
        counter_sweep.high = 20.0;
        counter_sweep.close = 19.0;
    }
    assert_eq!(
        engine.pda_history_invalidation_reasons(&divergence),
        Some(vec![SmtInvalidationReason::AllCountersSwept]),
        "an exact equal counter high removes the divergence regardless of close"
    );
    let rebuilt = engine
        .canonical_pda_snapshot(&divergence)
        .expect("complete canonical bars should rebuild the audit payload");
    let rebuilt_counter = rebuilt
        .liquidity_refs
        .iter()
        .find(|liquidity| liquidity.symbol == "EURUSD")
        .unwrap();
    assert_eq!(rebuilt.rule_version, RULE_VERSION);
    assert_eq!(rebuilt_counter.status, LiquidityRefStatus::Swept);
    assert!(rebuilt.trade_symbols.is_empty());
    assert_eq!(rebuilt.strength[0].label, "weak");
    {
        let counter_sweep = engine
            .buckets
            .get_mut(&("EURUSD".into(), mtf))
            .unwrap()
            .bars
            .iter_mut()
            .find(|bar| bar.ts == htf_dur)
            .unwrap();
        counter_sweep.high = 19.5;
        counter_sweep.close = 19.0;
    }
    divergence.liquidity_refs[0].ref_price = 10.25;
    assert_eq!(
        engine.pda_snapshot_matches_history(&divergence),
        Some(false)
    );
    assert_eq!(
        engine.pda_history_invalidation_reasons(&divergence),
        Some(vec![SmtInvalidationReason::CanonicalReferenceChanged])
    );
    assert_eq!(
        engine.pda_reference_matches_history(&divergence),
        Some(false)
    );
}

#[test]
fn canonical_mtf_reference_uses_the_real_subcandle_inside_pda() {
    let mut divergence = bullish_smt_scenario(
        1.0800, 1.0810, 1.0805, 1.0790, 1.0825, 1.0815, 1.0815, 1.0825, 1.0820,
    )
    .expect("should produce SMT");
    divergence.context_timeframe = Timeframe::H1;
    divergence.comparison_timeframe = Timeframe::M30;
    divergence.observation_window = (0, 2 * Timeframe::H1.duration_ms());
    divergence.sweeper_symbol = "DXY".into();
    divergence.liquidity_refs = vec![LiquidityRef {
        symbol: "DXY".into(),
        ref_price: 99.943,
        ref_ts: 0,
        side: LiquiditySide::SellSide,
        status: LiquidityRefStatus::Swept,
        tf: Timeframe::H1,
        mtf_ref_candle: None,
        mtf_sweep_candle: None,
    }];
    divergence.htf_pda_ref = Some(PdaRef {
        kind: "fvg".into(),
        id: "mtf-endpoint-pda".into(),
        tf: Timeframe::H1,
        direction: Direction::Bullish,
        price_low: 99.90,
        price_high: 99.96,
        ts_open: -Timeframe::H1.duration_ms(),
        ts_confirm: -1,
        exit_ts: None,
        ts_filled: None,
    });
    divergence.mtf_ref_candle = None;

    let mut engine = SmtEngine::new();
    let bucket = engine
        .buckets
        .entry(("DXY".into(), Timeframe::M30))
        .or_insert_with(SmtBucket::new);
    // The HTF bar opens at 00:00, but its sell-side extreme occurs on the
    // 00:30 MTF sub-candle. The old frontend fallback incorrectly used 00:00.
    bucket
        .bars
        .push_back(bar("DXY", 0, 100.00, 100.02, 99.973, 99.99));
    bucket
        .bars
        .push_back(bar("DXY", 1, 99.99, 100.00, 99.943, 99.95));

    let endpoint = engine
        .resolve_mtf_ref_candle(&divergence)
        .expect("canonical MTF endpoint should be reconstructable");
    assert_eq!(endpoint.ts, DUR);
    assert_eq!(endpoint.low, 99.943);

    divergence.htf_pda_ref.as_mut().unwrap().price_low = 99.95;
    assert!(engine.resolve_mtf_ref_candle(&divergence).is_none());
}

#[test]
fn hydration_keeps_duplicate_historical_pda_snapshots_but_reserves_future_use() {
    let mut engine = SmtEngine::new();
    let mut first = bullish_smt_scenario(
        1.0800, 1.0810, 1.0805, 1.0790, 1.0825, 1.0815, 1.0815, 1.0825, 1.0820,
    )
    .expect("should produce first SMT");
    let historical_pda = PdaRef {
        kind: "fvg".into(),
        id: "shared-historical-pda".into(),
        tf: Timeframe::H1,
        direction: Direction::Bullish,
        price_low: 1.0790,
        price_high: 1.0830,
        ts_open: 0,
        ts_confirm: DUR,
        exit_ts: None,
        // Filled after both historical SMTs formed: this must not rewrite
        // either SMT's formation-time PDA fact.
        ts_filled: Some(100 * DUR),
    };
    first.id = "historical-smt-1".into();
    first.htf_pda_ref = Some(historical_pda.clone());
    let mut second = first.clone();
    second.id = "historical-smt-2".into();
    second.htf_pda_ref = Some(historical_pda);

    engine.hydrate_divergences(vec![first, second], &HashMap::new());

    assert_eq!(
        engine
            .divergences
            .values()
            .filter(|d| d
                .htf_pda_ref
                .as_ref()
                .is_some_and(|p| p.id == "shared-historical-pda"))
            .count(),
        2,
        "hydration must not erase an already-recorded historical PDA snapshot"
    );
    assert!(
        engine.is_pda_consumed("shared-historical-pda"),
        "historical use must still reserve the PDA against future detections"
    );
}

#[test]
fn invalidated_hydrated_row_does_not_consume_pda() {
    let mut engine = SmtEngine::new();
    let mut historical = bullish_smt_scenario(
        1.0800, 1.0810, 1.0805, 1.0790, 1.0825, 1.0815, 1.0815, 1.0825, 1.0820,
    )
    .expect("should produce SMT");
    historical.htf_pda_ref = Some(PdaRef {
        kind: "fvg".into(),
        id: "migration-pda".into(),
        tf: Timeframe::H1,
        direction: Direction::Bullish,
        price_low: 1.0790,
        price_high: 1.0830,
        ts_open: 0,
        ts_confirm: DUR,
        exit_ts: None,
        ts_filled: None,
    });
    for chain in &mut historical.chains {
        chain.detection_state = SmtDetectionState::Invalidated;
    }
    historical.invalidation_reasons = vec![SmtInvalidationReason::CanonicalSweepInvalid];

    engine.hydrate_divergences(vec![historical], &HashMap::new());

    assert!(!engine.is_pda_consumed("migration-pda"));
    assert_eq!(engine.divergences.len(), 1, "audit snapshot is retained");
}

#[test]
fn replay_update_cannot_erase_historical_pda_snapshot() {
    let mut engine = SmtEngine::new();
    let mut historical = bullish_smt_scenario(
        1.0800, 1.0810, 1.0805, 1.0790, 1.0825, 1.0815, 1.0815, 1.0825, 1.0820,
    )
    .expect("should produce SMT");
    historical.htf_pda_ref = Some(PdaRef {
        kind: "fvg".into(),
        id: "immutable-pda".into(),
        tf: Timeframe::H1,
        direction: Direction::Bullish,
        price_low: 1.0790,
        price_high: 1.0830,
        ts_open: 0,
        ts_confirm: DUR,
        exit_ts: None,
        ts_filled: Some(100 * DUR),
    });
    let id = historical.id.clone();
    engine.hydrate_divergences(vec![historical.clone()], &HashMap::new());

    let mut replayed = historical;
    replayed.htf_pda_ref = None;
    replayed.mtf_ref_candle = None;
    engine.update_divergence(replayed);

    assert_eq!(
        engine.divergences[&id]
            .htf_pda_ref
            .as_ref()
            .map(|pda| pda.id.as_str()),
        Some("immutable-pda")
    );
    assert!(engine.is_pda_consumed("immutable-pda"));
}

#[test]
fn htf_pda_ref_fvg_expired_excluded() {
    // Expiry uses canonical DXY market bars, not wall-clock days. A 4H PDA
    // is valid through 42 closed H4 bars and excluded on the 43rd.
    let h4 = Timeframe::H4;
    let h4_dur = h4.duration_ms();
    let smt_k = CandleRef {
        ts: 43 * h4_dur,
        open: 1.0800,
        high: 1.0820,
        low: 1.0790,
        close: 1.0810,
    };
    let fvg = IctStructure::Fvg(Fvg {
        id: "fvg_old".into(),
        symbol: "DXY".into(),
        tf: h4,
        direction: Direction::Bullish,
        ts_open: 0,
        ts_confirm: 2 * h4_dur,
        price_low: 1.0795,
        price_high: 1.0815,
        state: FvgState::Mitigated50,
        ts_filled: None,
        consumed_exit_ts: None,
    });
    let mut engine = SmtEngine::new();
    for index in 0..=43 {
        let is_sweep = index == 43;
        engine
            .buckets
            .entry(("DXY".into(), h4))
            .or_insert_with(SmtBucket::new)
            .bars
            .push_back(Bar {
                symbol: "DXY".into(),
                tf: h4,
                ts: index * h4_dur,
                open: 1.0800,
                high: 1.0820,
                // Earlier bars stay above the PDA midpoint.  The sweep bar
                // itself may mitigate the zone and is deliberately excluded
                // from the prior-consumption check.
                low: if is_sweep { 1.0790 } else { 1.0810 },
                close: 1.0810,
                volume: 1.0,
            });
    }
    seed_pda_formation(
        &mut engine,
        "DXY",
        h4,
        Direction::Bullish,
        1.0795,
        1.0815,
        0,
        2 * h4_dur,
    );
    assert!(engine
        .canonical_pda_candidates("DXY", h4, &smt_k, &[fvg])
        .is_empty());

    // A genuinely later formation has its own market age and remains valid.
    let fvg_fresh = IctStructure::Fvg(Fvg {
        id: "fvg_fresh".into(),
        symbol: "DXY".into(),
        tf: h4,
        direction: Direction::Bullish,
        ts_open: 35 * h4_dur,
        ts_confirm: 37 * h4_dur,
        price_low: 1.0795,
        price_high: 1.0815,
        state: FvgState::Mitigated50,
        ts_filled: None,
        consumed_exit_ts: None,
    });
    seed_pda_formation(
        &mut engine,
        "DXY",
        h4,
        Direction::Bullish,
        1.0795,
        1.0815,
        35 * h4_dur,
        37 * h4_dur,
    );
    assert_eq!(
        engine
            .canonical_pda_candidates("DXY", h4, &smt_k, &[fvg_fresh])
            .len(),
        1
    );
}

#[test]
fn pda_previously_mitigated_to_midpoint_remains_eligible_until_filled_or_consumed() {
    let h1 = Timeframe::H1;
    let h1_dur = h1.duration_ms();
    let sweep = CandleRef {
        ts: 5 * h1_dur,
        open: 100.4,
        high: 100.8,
        low: 99.4,
        close: 100.1,
    };
    let fvg = IctStructure::Fvg(Fvg {
        id: "used-pda".into(),
        symbol: "DXY".into(),
        tf: h1,
        direction: Direction::Bullish,
        ts_open: 0,
        ts_confirm: 2 * h1_dur,
        price_low: 99.5,
        price_high: 100.5,
        state: FvgState::Mitigated50,
        ts_filled: None,
        consumed_exit_ts: None,
    });
    let mut engine = SmtEngine::new();
    let mut h1_bucket = SmtBucket::new();
    h1_bucket.bars.push_back(Bar {
        symbol: "DXY".into(),
        tf: h1,
        ts: 3 * h1_dur,
        open: 100.8,
        high: 101.0,
        low: 100.0,
        close: 100.7,
        volume: 1.0,
    });
    h1_bucket.bars.push_back(Bar {
        symbol: "DXY".into(),
        tf: h1,
        ts: 5 * h1_dur,
        open: sweep.open,
        high: sweep.high,
        low: sweep.low,
        close: sweep.close,
        volume: 1.0,
    });
    engine.buckets.insert(("DXY".into(), h1), h1_bucket);
    seed_pda_formation(
        &mut engine,
        "DXY",
        h1,
        Direction::Bullish,
        99.5,
        100.5,
        0,
        2 * h1_dur,
    );

    assert_eq!(
        engine
            .canonical_pda_candidates("DXY", h1, &sweep, &[fvg])
            .len(),
        1
    );
}

#[test]
fn pda_formation_rejects_shifted_child_phase_outside_native_htf_grid() {
    let h1 = Timeframe::H1;
    let m30 = Timeframe::M30;
    let h1_dur = h1.duration_ms();
    let m30_dur = m30.duration_ms();
    let pda = PdaRef {
        kind: "fvg".into(),
        id: "shifted-bearish-fvg".into(),
        tf: h1,
        direction: Direction::Bearish,
        price_low: 99.745,
        price_high: 99.802,
        ts_open: 0,
        ts_confirm: 2 * h1_dur,
        exit_ts: None,
        ts_filled: None,
    };
    let mut engine = SmtEngine::new();
    let mut bucket = SmtBucket::new();
    // The native :00 H1 candles do not reproduce this FVG. A shifted :30
    // aggregation would manufacture the stored 99.745-99.802 gap, but that
    // is not the H1 grid displayed by the app/TradingView and must be rejected.
    let values = [
        (0, 99.90, 99.70),
        (1, 99.90, 99.802),
        (2, 99.88, 99.82),
        (3, 99.745, 99.70),
        (4, 99.74, 99.69),
        (5, 99.745, 99.68),
        (6, 99.74, 99.67),
    ];
    for (slot, high, low) in values {
        bucket.bars.push_back(Bar {
            symbol: "DXY".into(),
            tf: m30,
            ts: slot * m30_dur,
            open: low,
            high,
            low,
            close: high,
            volume: 1.0,
        });
    }
    engine.buckets.insert(("DXY".into(), m30), bucket);
    assert_eq!(
        engine.pda_formation_matches_history("DXY", &pda),
        Some(false)
    );
}

#[test]
fn pda_formation_accepts_exact_native_htf_grid() {
    let h1 = Timeframe::H1;
    let m30 = Timeframe::M30;
    let h1_dur = h1.duration_ms();
    let m30_dur = m30.duration_ms();
    let pda = PdaRef {
        kind: "fvg".into(),
        id: "native-bearish-fvg".into(),
        tf: h1,
        direction: Direction::Bearish,
        price_low: 99.760,
        price_high: 99.807,
        ts_open: 0,
        ts_confirm: 2 * h1_dur,
        exit_ts: None,
        ts_filled: None,
    };
    let mut engine = SmtEngine::new();
    let mut bucket = SmtBucket::new();
    let values = [
        (0, 99.857, 99.810),
        (1, 99.848, 99.807),
        (2, 99.811, 99.711),
        (3, 99.745, 99.686),
        (4, 99.736, 99.650),
        (5, 99.760, 99.670),
    ];
    for (slot, high, low) in values {
        bucket.bars.push_back(Bar {
            symbol: "DXY".into(),
            tf: m30,
            ts: slot * m30_dur,
            open: low,
            high,
            low,
            close: high,
            volume: 1.0,
        });
    }
    engine.buckets.insert(("DXY".into(), m30), bucket);
    assert_eq!(
        engine.pda_formation_matches_history("DXY", &pda),
        Some(true)
    );
}

#[test]
fn pda_formation_accepts_market_complete_shortened_friday_h4() {
    let h4 = Timeframe::H4;
    let h1 = Timeframe::H1;
    // 2026-07-31 19:00 Beijing / 07:00 New York. The third H4 parent
    // starts at 15:00 New York and therefore legitimately contains only the
    // 15:00 and 16:00 H1 bars before the 17:00 FX close.
    let ts_open = 1_785_495_600_000;
    let pda = PdaRef {
        kind: "fvg".into(),
        id: "friday-shortened-bearish-fvg".into(),
        tf: h4,
        direction: Direction::Bearish,
        price_low: 99.965,
        price_high: 100.102,
        ts_open,
        ts_confirm: ts_open + 2 * h4.duration_ms(),
        exit_ts: None,
        ts_filled: None,
    };
    let values = [
        (100.345, 100.199),
        (100.392, 100.330),
        (100.463, 100.168),
        (100.437, 100.102),
        (100.160, 100.015),
        (100.066, 100.013),
        (100.057, 99.806),
        (99.977, 99.828),
        (99.962, 99.886),
        (99.965, 99.692),
    ];
    let mut engine = SmtEngine::new();
    for (index, (high, low)) in values.into_iter().enumerate() {
        engine
            .buckets
            .entry(("TVC:DXY".into(), h1))
            .or_insert_with(SmtBucket::new)
            .bars
            .push_back(Bar {
                symbol: "TVC:DXY".into(),
                tf: h1,
                ts: ts_open + index as i64 * h1.duration_ms(),
                open: low,
                high,
                low,
                close: high,
                volume: 1.0,
            });
    }

    assert_eq!(
        engine.pda_formation_matches_history("TVC:DXY", &pda),
        Some(true)
    );
}

#[test]
fn replay_context_keeps_pda_formation_before_detection_horizon() {
    use chrono::{TimeZone, Utc};

    let h4 = Timeframe::H4;
    let h1 = Timeframe::H1;
    let h1_dur = h1.duration_ms();
    // Keep the synthetic formation on an ordinary FX business day. Starting
    // from Unix epoch crosses New Year's Day in New York, where canonical FX
    // history intentionally has no bars.
    let base_ts = Utc
        .with_ymd_and_hms(2026, 2, 3, 0, 0, 0)
        .single()
        .unwrap()
        .timestamp_millis();
    let pda = PdaRef {
        kind: "fvg".into(),
        id: "preloaded-bullish-fvg".into(),
        tf: h4,
        direction: Direction::Bullish,
        price_low: 100.0,
        price_high: 101.0,
        ts_open: base_ts,
        ts_confirm: base_ts + 2 * h4.duration_ms(),
        exit_ts: None,
        ts_filled: None,
    };
    let mut engine = SmtEngine::new();
    engine.set_watchlist(make_wl(vec!["DXY".into()]));
    for index in 0..HISTORY_CAP {
        let parent = index / 4;
        let (high, low) = match parent {
            0 => (100.0, 99.5),
            1 => (101.0, 99.0),
            2 => (101.5, 101.0),
            _ => (102.0, 99.0),
        };
        engine.seed_bar(&Bar {
            symbol: "DXY".into(),
            tf: h1,
            ts: base_ts + index as i64 * h1_dur,
            open: low,
            high,
            low,
            close: high,
            volume: 1.0,
        });
    }

    assert_eq!(
        engine.pda_formation_matches_history("DXY", &pda),
        Some(true),
        "400 non-emitting context bars plus the 1100-bar detection horizon must fit"
    );
}

#[test]
fn pda_formation_rejects_nonconsecutive_confirm_timestamp() {
    let h1 = Timeframe::H1;
    let mut pda = PdaRef {
        kind: "fvg".into(),
        id: "nonconsecutive-fvg".into(),
        tf: h1,
        direction: Direction::Bullish,
        price_low: 100.0,
        price_high: 101.0,
        ts_open: 0,
        ts_confirm: 3 * h1.duration_ms(),
        exit_ts: None,
        ts_filled: None,
    };
    let engine = SmtEngine::new();
    assert_eq!(
        engine.pda_formation_matches_history("DXY", &pda),
        Some(false)
    );

    pda.ts_confirm = 2 * h1.duration_ms();
    assert_eq!(
        engine.pda_formation_matches_history("DXY", &pda),
        None,
        "missing child history is unknown, never a canonical match"
    );
}

#[test]
fn released_retry_uses_all_intervening_bars_as_its_watermark() {
    let mut engine = SmtEngine::new();
    let mut failed = pending_case2_divergence("failed-attempt", "retry-pda");
    failed
        .invalidation_reasons
        .push(SmtInvalidationReason::SweeperC3Failed);
    let pda = failed.htf_pda_ref.clone().unwrap();
    engine.divergences.insert(failed.id.clone(), failed);

    let mut bucket = SmtBucket::new();
    for (index, high) in [(7, 100.0), (8, 102.0), (9, 101.0)] {
        bucket
            .bars
            .push_back(bar("DXY", index, 99.0, high, 98.0, 99.5));
    }
    engine.buckets.insert(("DXY".into(), TF), bucket);

    assert_eq!(
        engine.released_reference_watermark(
            &pda,
            "DXY",
            2 * DUR,
            Timeframe::H1,
            SwingKind::High,
            10 * DUR,
        ),
        Some(102.0),
        "the later retry must beat the intervening 102 high, not only the failed SMT-K high"
    );
    let weaker_retry = CandleRef {
        ts: 10 * DUR,
        open: 100.0,
        high: 101.5,
        low: 99.0,
        close: 100.5,
    };
    assert!(!engine.reference_take_is_eligible(
        &pda,
        "DXY",
        TF,
        2 * DUR,
        3 * DUR,
        Timeframe::H1,
        SwingKind::High,
        99.5,
        10 * DUR,
        &weaker_retry,
    ));
    let stronger_retry = CandleRef {
        high: 102.1,
        ..weaker_retry
    };
    assert!(engine.reference_take_is_eligible(
        &pda,
        "DXY",
        TF,
        2 * DUR,
        3 * DUR,
        Timeframe::H1,
        SwingKind::High,
        99.5,
        10 * DUR,
        &stronger_retry,
    ));
}

#[test]
fn canonical_chain_audit_rejects_a_missing_or_changed_c1() {
    let (engine, events) = current_rule_h1_smt(1.095, 1.295);
    let divergence = extract_smt(&events)
        .into_iter()
        .next()
        .expect("fixture should create a current-rule SMT")
        .clone();
    assert_eq!(
        engine.canonical_chain_invalidation_reason(&divergence),
        None
    );

    let mut changed = divergence;
    changed.chains[0].c1_candle.ts -= DUR;
    assert_eq!(
        engine.canonical_chain_invalidation_reason(&changed),
        Some(SmtInvalidationReason::CanonicalChainChanged)
    );
}

#[test]
fn local_mtf_reference_requires_confirmed_departure_and_reentry() {
    let mut engine = SmtEngine::new();
    let pda = PdaRef {
        kind: "fvg".into(),
        id: "reentry-pda".into(),
        tf: Timeframe::H1,
        direction: Direction::Bullish,
        price_low: 99.0,
        price_high: 100.0,
        ts_open: -2 * DUR,
        ts_confirm: 0,
        exit_ts: None,
        ts_filled: None,
    };
    let mut bucket = SmtBucket::new();
    // H1 confirm is complete at 2*DUR. The first child is still touching;
    // it cannot count as a fresh return until price leaves and comes back.
    bucket
        .bars
        .push_back(bar("DXY", 2, 99.5, 100.2, 99.2, 99.8));
    bucket
        .bars
        .push_back(bar("DXY", 3, 100.5, 101.0, 100.3, 100.7));
    bucket
        .bars
        .push_back(bar("DXY", 4, 100.2, 100.4, 99.8, 100.0));
    engine.buckets.insert(("DXY".into(), TF), bucket);
    assert_eq!(
        engine.first_pda_entry_ts("DXY", TF, &pda, 6 * DUR),
        Some(4 * DUR)
    );
}

#[test]
fn htf_pda_ref_fvg_filled_excluded() {
    let smt_k = CandleRef {
        ts: 100,
        open: 1.0800,
        high: 1.0820,
        low: 1.0790,
        close: 1.0810,
    };
    let fvg = IctStructure::Fvg(Fvg {
        id: "fvg1".into(),
        symbol: "EURUSD".into(),
        tf: Timeframe::H1,
        direction: Direction::Bullish,
        ts_open: 50,
        ts_confirm: 80,
        price_low: 1.0795,
        price_high: 1.0815,
        state: FvgState::Filled,
        ts_filled: Some(90),
        consumed_exit_ts: None,
    });
    assert!(compute_htf_pda_ref(&[fvg], &smt_k).is_none());
}

#[test]
fn htf_pda_fill_on_the_smt_bar_remains_valid() {
    let smt_k = CandleRef {
        ts: 100,
        open: 1.0800,
        high: 1.0820,
        low: 1.0790,
        close: 1.0810,
    };
    let fvg = IctStructure::Fvg(Fvg {
        id: "same-bar-fill".into(),
        symbol: "DXY".into(),
        tf: Timeframe::H1,
        direction: Direction::Bullish,
        ts_open: 50,
        ts_confirm: 80,
        price_low: 1.0795,
        price_high: 1.0815,
        state: FvgState::Filled,
        ts_filled: Some(smt_k.ts),
        consumed_exit_ts: None,
    });

    assert_eq!(
        compute_htf_pda_ref(&[fvg], &smt_k)
            .map(|pda| pda.id)
            .as_deref(),
        Some("same-bar-fill")
    );
}

#[test]
fn ifvg_without_historical_fill_timestamp_is_not_an_original_pda() {
    let smt_k = CandleRef {
        ts: 100,
        open: 1.0800,
        high: 1.0820,
        low: 1.0790,
        close: 1.0810,
    };
    let ifvg = IctStructure::Fvg(Fvg {
        id: "ifvg-not-pda".into(),
        symbol: "DXY".into(),
        tf: Timeframe::H1,
        direction: Direction::Bullish,
        ts_open: 50,
        ts_confirm: 80,
        price_low: 1.0795,
        price_high: 1.0815,
        state: FvgState::InvertedActive,
        ts_filled: None,
        consumed_exit_ts: None,
    });

    assert!(compute_htf_pda_ref(&[ifvg], &smt_k).is_none());
}

#[test]
fn htf_pda_candidates_are_deterministically_newest_first() {
    let sweep = CandleRef {
        ts: 1_000,
        open: 1.0800,
        high: 1.0820,
        low: 1.0790,
        close: 1.0810,
    };
    let make_fvg = |id: &str, ts_confirm: i64| {
        IctStructure::Fvg(Fvg {
            id: id.into(),
            symbol: "EURUSD".into(),
            tf: Timeframe::H1,
            direction: Direction::Bullish,
            ts_open: ts_confirm - 10,
            ts_confirm,
            price_low: 1.0795,
            price_high: 1.0815,
            state: FvgState::Active,
            ts_filled: None,
            consumed_exit_ts: None,
        })
    };
    let refs = compute_htf_pda_refs(&[make_fvg("older", 100), make_fvg("newer", 200)], &sweep);
    assert_eq!(
        refs.iter().map(|pda| pda.id.as_str()).collect::<Vec<_>>(),
        vec!["newer", "older"]
    );
}

#[test]
fn h4_sweep_july15_replayed_from_db() {
    let home = std::env::var("HOME").unwrap();
    let db_path = format!("{home}/.ict-monitor/ict.db");
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skip: open db {db_path}: {e}");
            return;
        }
    };
    let syms = ["TVC:DXY", "OANDA:EURUSD", "OANDA:GBPUSD"];
    let load = |tf_tag: &str, tf: Timeframe| -> Vec<Bar> {
        let mut stmt = conn
            .prepare(
                "SELECT symbol,ts,open,high,low,close,volume FROM bars \
                 WHERE tf=?1 AND symbol IN (?2,?3,?4) ORDER BY ts",
            )
            .unwrap();
        stmt.query_map(rusqlite::params![tf_tag, syms[0], syms[1], syms[2]], |r| {
            Ok(Bar {
                symbol: r.get(0)?,
                tf,
                ts: r.get(1)?,
                open: r.get(2)?,
                high: r.get(3)?,
                low: r.get(4)?,
                close: r.get(5)?,
                volume: r.get(6)?,
            })
        })
        .unwrap()
        .filter_map(|x| x.ok())
        .collect::<Vec<_>>()
    };
    let h1 = load("1h", Timeframe::H1);
    let h4 = load("4h", Timeframe::H4);
    if h1.is_empty() || h4.is_empty() {
        eprintln!("skip: no bars in db");
        return;
    }
    // Skip if 4h bars have mixed-grid leftovers (consecutive bars < 3h
    // apart) from a failed grid realignment. The migration cleans this
    // on next app start, but the test may run before that.
    let mixed_grid = h4
        .windows(2)
        .any(|w| (w[1].ts - w[0].ts).abs() < 3 * 3_600_000);
    if mixed_grid {
        eprintln!("skip: 4h bars have mixed grid (pending migration)");
        return;
    }

    let wl = crate::watchlist::default_watchlists()
        .into_iter()
        .find(|w| w.id == "eu-gu-dxy")
        .unwrap();
    let mut e = SmtEngine::new();
    e.set_watchlist(wl);

    let mut events = Vec::new();
    for b in &h1 {
        events.extend(e.on_closed_bar(b));
    }
    for b in &h4 {
        events.extend(e.on_closed_bar(b));
    }
    let smts = extract_smt(&events);
    // The 7/15 DXY 4h sweep (UTC 09:00, high 101.029 piercing the 7/14 17:00
    // high 100.984) must now be detected since the reference is the bar
    // extreme, not a fractal swing.
    let has_h4_smt = smts
        .iter()
        .any(|d| d.context_timeframe == Timeframe::H4 && d.sweeper_symbol == "TVC:DXY");
    assert!(
        has_h4_smt,
        "expected at least one H4-context SMT from real DB data; got {} total",
        smts.len()
    );
}
