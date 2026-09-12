//! Separate the quote display cursor from authoritative chart closes.

use super::{split_initial_snapshot, Bar, BarEvent, MultiSeriesState, Timeframe};
use futures_util::{Stream, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;

/// `timeout(0, ready_stream.next())` may return the ready frame forever.
/// Expired rotation/completion deadlines must win over a busy quote stream.
pub(super) async fn next_union_frame<S: Stream + Unpin>(
    stream: &mut S,
    wait: Duration,
) -> Result<Option<S::Item>, ()> {
    if wait.is_zero() {
        return Err(());
    }
    tokio::select! {
        biased;
        _ = tokio::time::sleep(wait) => Err(()),
        frame = stream.next() => Ok(frame),
    }
}

pub(super) async fn publish_union_preview(
    state: &mut MultiSeriesState,
    bar: Bar,
    tx: &mpsc::Sender<BarEvent>,
) {
    if bar.ts < state.last_seen_ts || bar.ts <= state.last_closed_ts {
        return;
    }
    state.last_seen_ts = bar.ts;
    state.current_open = Some(bar.clone());
    let _ = tx.send(BarEvent::BarUpdate(bar)).await;
}

pub(super) async fn apply_authoritative_bars(
    state: &mut MultiSeriesState,
    tf: Timeframe,
    mut bars: Vec<Bar>,
    now: i64,
    tx: &mpsc::Sender<BarEvent>,
) {
    bars.sort_by_key(|bar| bar.ts);
    bars.dedup_by_key(|bar| bar.ts);
    let (closed, open) = split_initial_snapshot(bars, tf, now);
    // Defensive filtering also excludes unexpected future bars earlier than
    // the last row. Provider timestamps are candle opens, never close times.
    let closed: Vec<Bar> = closed
        .into_iter()
        .filter(|bar| bar.ts.saturating_add(tf.duration_ms()) <= now)
        .collect();
    if !state.historical_seeded {
        state.historical_seeded = true;
        if let Some(last) = closed.last() {
            state.last_closed_ts = last.ts;
        }
        if !closed.is_empty() {
            tracing::info!(symbol = %state.symbol, n = closed.len(), "union authoritative historical backfill");
            let _ = tx.send(BarEvent::Historical(closed)).await;
        }
    } else {
        for bar in closed {
            if bar.ts <= state.last_closed_ts {
                continue;
            }
            tracing::info!(
                symbol = %state.symbol, ts = bar.ts,
                delay_ms = now.saturating_sub(bar.ts.saturating_add(tf.duration_ms())),
                "union authoritative bar closed"
            );
            state.last_closed_ts = bar.ts;
            let _ = tx.send(BarEvent::BarClosed(bar)).await;
        }
    }
    if state
        .current_open
        .as_ref()
        .is_some_and(|bar| bar.ts <= state.last_closed_ts)
    {
        state.current_open = None;
    }
    if let Some(bar) = open.filter(|bar| bar.ts <= now) {
        publish_union_preview(state, bar, tx).await;
    }
}
