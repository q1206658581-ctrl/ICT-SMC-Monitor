use super::*;
use crate::aggregator::SymbolAggregator;

const BASE: i64 = 1_767_657_600_000; // Tuesday, 2026-01-06 00:00 UTC.

fn state() -> MultiSeriesState {
    MultiSeriesState {
        symbol: "OANDA:EURUSD".into(),
        current_open: None,
        last_seen_ts: i64::MIN,
        last_closed_ts: i64::MIN,
        last_quote_emit_ms: i64::MIN,
        historical_seeded: true,
    }
}

fn bar(minute: i64) -> Bar {
    Bar {
        symbol: "OANDA:EURUSD".into(),
        tf: Timeframe::M1,
        ts: BASE + minute * 60_000,
        open: 1.0,
        high: 1.2,
        low: 0.9,
        close: 1.1,
        volume: 10.0,
    }
}

async fn quote(state: &mut MultiSeriesState, minute: i64, tx: &mpsc::Sender<BarEvent>) {
    apply_quote_tick(
        state,
        Timeframe::M1,
        QuoteTick {
            symbol: state.symbol.clone(),
            price: 1.05,
            ts_ms: BASE + minute * 60_000,
        },
        BASE + minute * 60_000,
        tx,
    )
    .await;
}

#[tokio::test]
async fn sparse_quotes_do_not_drop_formal_minutes_or_higher_closes() {
    let mut state = state();
    let (tx, mut rx) = mpsc::channel(64);
    quote(&mut state, 0, &tx).await;
    quote(&mut state, 2, &tx).await; // no quote in minute 1
    apply_authoritative_bars(
        &mut state,
        Timeframe::M1,
        (0..=2).map(bar).collect(),
        BASE + 150_000,
        &tx,
    )
    .await;
    for minute in [3, 4, 5] {
        quote(&mut state, minute, &tx).await;
    }
    apply_authoritative_bars(
        &mut state,
        Timeframe::M1,
        (0..=5).map(bar).collect(),
        BASE + 330_000,
        &tx,
    )
    .await;
    let mut aggregator = SymbolAggregator::new(&state.symbol);
    let mut closed = Vec::new();
    let mut m5 = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let BarEvent::BarClosed(b) = &event {
            closed.push(b.ts);
        }
        for event in aggregator.on_m1_event(&event) {
            if let BarEvent::BarClosed(b) = event {
                if b.tf == Timeframe::M5 {
                    m5.push(b);
                }
            }
        }
    }
    assert_eq!(
        closed,
        (0..5).map(|m| BASE + m * 60_000).collect::<Vec<_>>()
    );
    assert_eq!(m5.len(), 1);
    assert_eq!(
        (m5[0].open, m5[0].high, m5[0].low, m5[0].close, m5[0].volume),
        (1.0, 1.2, 0.9, 1.1, 50.0)
    );
    assert_eq!(
        aggregator.current(Timeframe::M5).unwrap().ts,
        BASE + 300_000
    );
}

#[tokio::test]
async fn native_close_overrides_quote_geometry_once_without_rewinding_preview() {
    let mut state = state();
    let (tx, mut rx) = mpsc::channel(16);
    quote(&mut state, 4, &tx).await;
    quote(&mut state, 5, &tx).await;
    let mut preview = bar(5);
    preview.high = 999.0; // speculative extrema must not leak into finalized M5
    let mut aggregator = SymbolAggregator::new(&state.symbol);
    aggregator.on_m1_event(&BarEvent::BarUpdate(preview));
    for m in 0..5 {
        aggregator.on_m1_event(&BarEvent::BarClosed(bar(m)));
    }
    assert_eq!(aggregator.history(Timeframe::M5)[0].high, 1.2);
    for _ in 0..2 {
        apply_authoritative_bars(&mut state, Timeframe::M1, vec![bar(4)], BASE + 330_000, &tx)
            .await;
    }
    let mut closes = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let BarEvent::BarClosed(b) = event {
            closes.push(b);
        }
    }
    assert_eq!(closes.len(), 1);
    assert_eq!(closes[0].high, 1.2);
    assert_eq!(state.current_open.as_ref().unwrap().ts, BASE + 300_000);
}

#[tokio::test]
async fn initial_backfill_is_historical_even_if_quotes_arrive_first() {
    let mut state = state();
    state.historical_seeded = false;
    let (tx, mut rx) = mpsc::channel(16);
    quote(&mut state, 5, &tx).await;
    apply_authoritative_bars(
        &mut state,
        Timeframe::M1,
        (0..=4).map(bar).collect(),
        BASE + 330_000,
        &tx,
    )
    .await;
    assert!(matches!(rx.recv().await, Some(BarEvent::BarUpdate(_))));
    assert!(matches!(rx.recv().await, Some(BarEvent::Historical(bars)) if bars.len() == 5));
    assert!(rx.try_recv().is_err());
    assert_eq!(state.current_open.as_ref().unwrap().ts, BASE + 300_000);
}

#[tokio::test]
async fn boundary_close_does_not_require_a_quote_in_the_next_minute() {
    let mut state = state();
    let (tx, mut rx) = mpsc::channel(8);
    apply_authoritative_bars(&mut state, Timeframe::M1, vec![bar(4)], BASE + 299_999, &tx).await;
    assert!(matches!(rx.recv().await, Some(BarEvent::BarUpdate(_))));
    assert!(rx.try_recv().is_err());
    apply_authoritative_bars(&mut state, Timeframe::M1, vec![bar(4)], BASE + 300_000, &tx).await;
    assert!(matches!(rx.recv().await, Some(BarEvent::BarClosed(b)) if b.ts == BASE + 240_000));
    assert!(state.current_open.is_none());
    quote(&mut state, 4, &tx).await; // stale quote cannot resurrect a closed bar
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn overdue_rotation_wins_over_continuously_ready_quotes() {
    let mut ready = futures_util::stream::repeat(42);
    for _ in 0..100 {
        assert_eq!(next_union_frame(&mut ready, Duration::ZERO).await, Err(()));
    }
    assert_eq!(
        next_union_frame(&mut ready, Duration::from_secs(1)).await,
        Ok(Some(42))
    );
}

#[tokio::test]
async fn late_counter_snapshot_unblocks_smt_without_restart() {
    use crate::detector::smt::SmtEngine;
    let watchlist = crate::watchlist::default_watchlists().remove(0);
    let mut smt = SmtEngine::new();
    smt.set_watchlist(watchlist.clone());
    // DXY and one counter arrive first; the remaining counter is a full
    // rotation late. Sparse quote previews have already crossed the hour.
    let mut symbols = watchlist.symbols.clone();
    symbols.sort_by_key(|symbol| (!symbol.contains("DXY"), symbol.clone()));
    let mut ready = Vec::new();
    for (index, symbol) in symbols.iter().enumerate() {
        let mut state = state();
        state.symbol = symbol.clone();
        let mut aggregator = SymbolAggregator::new(symbol);
        let (tx, mut rx) = mpsc::channel(128);
        for minute in [0, 2, 30, 60] {
            quote(&mut state, minute, &tx).await;
        }
        let bars: Vec<Bar> = (0..60)
            .map(|minute| {
                let mut b = bar(minute);
                b.symbol = symbol.clone();
                b
            })
            .collect();
        apply_authoritative_bars(&mut state, Timeframe::M1, bars, BASE + 3_660_000, &tx).await;
        while let Ok(event) = rx.try_recv() {
            for higher in aggregator.on_m1_event(&event) {
                if let BarEvent::BarClosed(bar) = higher {
                    smt.on_closed_bar(&bar);
                    smt.queue_pda_htf_close(&bar);
                    ready.extend(smt.take_ready_pda_htf_closes());
                }
            }
        }
        if index < 2 {
            assert!(ready.is_empty(), "wait for all three canonical streams");
        }
    }
    assert_eq!(ready.len(), 1);
    assert!(ready[0].symbol.contains("DXY"));
    assert_eq!((ready[0].tf, ready[0].ts), (Timeframe::H1, BASE));
}
