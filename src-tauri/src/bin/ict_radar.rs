//! Tauri main binary for the ICT Radar desktop app.
//!
//! Responsibilities:
//! 1. Boot tracing + the Tauri runtime.
//! 2. Open the SQLite store.
//! 3. Spawn one subscription task that:
//!      - warm-starts the aggregator from SQLite history,
//!      - subscribes to `OANDA:EURUSD` 1m via TradingView,
//!      - feeds bars into the aggregator,
//!      - emits `bar:update` / `bar:closed` events to the frontend,
//!      - persists every closed bar (1m + higher TFs) to SQLite.
//!
//! The aggregator and emit path are deliberately decoupled — future detector
//! engines and the alert engine will subscribe to the same broadcast channel
//! that powers the Tauri emit task.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tauri::Emitter;
use tauri::Manager;
use tokio::sync::broadcast;
use tokio::sync::Mutex as AsyncMutex;
use tracing_subscriber::EnvFilter;

use ict_monitor::aggregator::{
    aggregate_h4_from_h1, aggregate_h4_with_native_fallback, expected_market_bar_opens,
    has_incomplete_h4_window, next_window_boundary, SymbolAggregator,
};
use ict_monitor::alert::{
    AlertEngine, AlertRecord, AlertTrigger, DesktopNotifyChannel, FeishuAlertSender,
    FeishuNotifyChannel, InboxChannel,
};
use ict_monitor::candidate::{
    extract_ltf_events, CandidateEngine, CandidateSetup, DecisionLogEntry,
};
use ict_monitor::config::AppConfig;
use ict_monitor::data_source::tradingview::{TvAuth, TvClient};
use ict_monitor::detector::smt::{smt_invalidation_market_ts, SmtEngine, CURRENT_RULE_VERSION};
use ict_monitor::detector::types::{
    IctStructure, PdaRef, SmtDetectionState, SmtDivergence, SmtInvalidationReason, StructureEvent,
};
use ict_monitor::detector::EngineHandle;
use ict_monitor::ipc::{
    AlertHandle, AppState, AppStatusPayload, BarPayload, CandidateHandle, EngineGroup,
    EngineGroups, LlmDecisionHandle, SmtHandle, SymbolMeta,
};
use ict_monitor::llm::{build_configured_pipeline, sort_llm_decision_items, LlmDecisionListItem};
use ict_monitor::storage::SqliteStore;
use ict_monitor::types::Timeframe as IctTimeframe;
use ict_monitor::types::{Bar, BarEvent, Timeframe};
use ict_monitor::watchlist::{
    default_correlation as wl_default_correlation, merge_defaults,
    DefaultCorrelation as WlDefaultCorrelation, Watchlist, WatchlistInput,
};

type WatchlistHandle = Arc<AsyncMutex<Option<Watchlist>>>;
/// Pre-computed HTF structures cache used during SMT replay to avoid
/// locking the engine (which competes with real-time 1m bar processing).
type StructuresCache = std::collections::HashMap<(String, Timeframe), Vec<IctStructure>>;

const WARM_START_M1_LIMIT: i64 = 5_000;
/// Only inspect the recent tail before deciding whether the bounded M1 repair
/// is needed. This runs for rotating TradingView history snapshots, so keep
/// the read small while still covering several days on M30/H1.
const AGGREGATE_GAP_INSPECTION_LIMIT: i64 = 256;
/// A reconnect gap is necessarily close to the live edge. Keep the additive
/// repair bounded so rotating TradingView calibration snapshots never repeat
/// the much heavier full-retention reconciliation used only at startup.
const AGGREGATE_GAP_FILL_M1_LIMIT: i64 = 5_000;
/// Aggregate repair is a data-integrity pass, not a detector warm start.  It
/// must cover every retained M1 candle; otherwise an older stale M30/H1 row
/// survives and the chart can draw an SMT endpoint at a price which is not an
/// extreme of the visible HTF candle.  Current retention is well below this
/// ceiling (~43k rows/symbol), while the detector still warm-starts from the
/// smaller window above.
const AGGREGATE_REPAIR_M1_LIMIT: i64 = 100_000;
/// Forex markets have normal weekend closures, but a gap longer than five
/// days is missing history rather than a market session boundary. Never let a
/// bounded aggregate repair delete authoritative higher-TF candles across
/// such a hole.
const MAX_HISTORY_GAP_MS: i64 = 5 * 24 * 60 * 60_000;
const MAX_HISTORY_STALENESS_MS: i64 = 7 * 24 * 60 * 60_000;
const SMT_SOURCE_HISTORY_LIMIT: i64 = 1_500;
const SMT_SOURCE_MIN_BARS: usize = 1_100;
/// Startup SMT uses H4 parents before any chart IPC can request an H4
/// backfill. Rebuild from the authoritative H1 history first so every group
/// sees the same NY-calendar H4 grid even when TV-native H4 rows are sparse.
const H4_PREFLIGHT_H1_LIMIT: i64 = 5_000;
const H4_NATIVE_FALLBACK_LIMIT: i64 = 1_500;
/// PDA expiry can span 42 closed H4 bars. Keep additional formation/context
/// bars and refuse to advertise the H4 branch when even this audit floor is
/// unavailable for one pane.
const H4_REPLAY_MIN_BARS: usize = 64;
const BROADCAST_CAPACITY: usize = 65536;
/// A successful reconnect audit covers the full monitored universe, so the
/// remaining per-symbol historical snapshots from the same TradingView
/// reconnect must not start duplicate passes.
const RECONNECT_PREFLIGHT_SUCCESS_COOLDOWN_MS: i64 = 60_000;
/// Provider failures are retried in the background, but not in a tight loop
/// that competes with the live union feed or interactive chart refreshes.
const RECONNECT_PREFLIGHT_RETRY_COOLDOWN_MS: i64 = 15_000;
const RECONNECT_PREFLIGHT_MAX_ATTEMPTS: usize = 3;
const RECONNECT_CHART_HISTORY_LIMIT: i64 = 1_500;
const RECONNECT_CALENDAR_HISTORY_LIMIT: i64 = 500;

fn db_path() -> PathBuf {
    if let Ok(p) = std::env::var("ICT_DB_PATH") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".ict-monitor").join("ict.db")
}

fn finalized_history_bars(bars: &[Bar], now_ms: i64) -> Vec<Bar> {
    let mut out: Vec<Bar> = bars
        .iter()
        .filter(|bar| {
            let window = bar.tf.boundary_align(bar.ts);
            next_window_boundary(bar.tf, window) <= now_ms
        })
        .cloned()
        .collect();
    out.sort_by_key(|b| b.ts);
    out
}

fn bars_have_large_gap(bars: &[Bar]) -> bool {
    bars.windows(2)
        .any(|pair| pair[1].ts.saturating_sub(pair[0].ts) > MAX_HISTORY_GAP_MS)
}

/// Detect a missing canonical candle inside an otherwise recent intraday
/// history slice. A provider outage can leave a one-bar hole which is far
/// smaller than `MAX_HISTORY_GAP_MS`; if that hole is not noticed, the strict
/// M1 aggregator correctly drops the incomplete higher-TF bucket but the
/// chart never asks TradingView to repair it.
///
/// `expected_market_bar_opens` applies the forex market calendar, so the
/// normal Friday-close to Sunday-open interval is not treated as missing
/// history.
fn bars_have_unexpected_intraday_gap(bars: &[Bar]) -> bool {
    bars.windows(2).any(|pair| {
        let previous = &pair[0];
        let next = &pair[1];
        if previous.symbol != next.symbol || previous.tf != next.tf {
            return true;
        }
        let duration = previous.tf.duration_ms();
        if next.ts.saturating_sub(previous.ts) <= duration {
            return false;
        }
        !expected_market_bar_opens(
            &previous.symbol,
            previous.tf,
            previous.ts.saturating_add(duration),
            next.ts,
        )
        .is_empty()
    })
}

fn is_fixed_intraday(tf: Timeframe) -> bool {
    matches!(
        tf,
        Timeframe::M1 | Timeframe::M5 | Timeframe::M15 | Timeframe::M30 | Timeframe::H1
    )
}

/// Whether a canonical calendar window contains any tradable FX time.
///
/// H4/D1/W1/MN1 cannot be checked by adding their nominal duration because
/// New-York DST makes some windows one hour shorter or longer. H1 is fixed,
/// so it is the common source calendar used to decide whether a missing
/// parent window is a real hole or merely a weekend closure.
fn calendar_window_expects_bar(symbol: &str, tf: Timeframe, window: i64) -> bool {
    let end = next_window_boundary(tf, window);
    !expected_market_bar_opens(symbol, Timeframe::H1, window, end).is_empty()
}

/// Detect missing candles for every chart timeframe.
///
/// Fixed intraday periods use their exact source cadence. Calendar periods
/// walk canonical NY-local boundaries so DST and normal FX weekends do not
/// create false positives. Off-grid or duplicate calendar rows are also
/// treated as damaged history and cause an authoritative provider refresh.
fn bars_have_unexpected_market_gap(bars: &[Bar]) -> bool {
    let Some(first) = bars.first() else {
        return false;
    };
    if is_fixed_intraday(first.tf) {
        return bars_have_unexpected_intraday_gap(bars);
    }

    bars.windows(2).any(|pair| {
        let previous = &pair[0];
        let next = &pair[1];
        if previous.symbol != next.symbol || previous.tf != next.tf {
            return true;
        }

        let previous_window = previous.tf.boundary_align(previous.ts);
        let next_window = next.tf.boundary_align(next.ts);
        if previous.ts != previous_window
            || next.ts != next_window
            || next_window <= previous_window
        {
            return true;
        }

        let mut expected = next_window_boundary(previous.tf, previous_window);
        while expected < next_window {
            if calendar_window_expects_bar(&previous.symbol, previous.tf, expected) {
                return true;
            }
            let following = next_window_boundary(previous.tf, expected);
            if following <= expected {
                return true;
            }
            expected = following;
        }
        false
    })
}

fn history_needs_backfill(bars: &[Bar], minimum: usize, market_now_ms: i64) -> bool {
    if bars.len() < minimum || bars_have_large_gap(bars) || bars_have_unexpected_market_gap(bars) {
        return true;
    }
    bars.last().map_or(true, |bar| {
        market_now_ms.saturating_sub(bar.ts) > MAX_HISTORY_STALENESS_MS
    })
}

/// Decide whether a chart refresh needs to contact the provider.
///
/// The UI renders its cached snapshot before calling this authoritative path,
/// so internal repair is allowed here without making a timeframe switch show
/// an empty chart. Every genuine market-open hole must request a refresh;
/// normal FX closures are excluded by the shared market calendar.
fn chart_history_needs_provider_refresh(bars: &[Bar], minimum: usize, market_now_ms: i64) -> bool {
    if bars.len() < minimum || bars_have_unexpected_market_gap(bars) {
        return true;
    }
    let Some(last) = bars.last() else {
        return true;
    };
    let last_window = last.tf.boundary_align(last.ts);
    if last.ts != last_window {
        return true;
    }
    let current_window = last.tf.boundary_align(market_now_ms);
    let mut missing = next_window_boundary(last.tf, last_window);
    while missing < current_window {
        let expects_bar = if is_fixed_intraday(last.tf) {
            !expected_market_bar_opens(
                &last.symbol,
                last.tf,
                missing,
                next_window_boundary(last.tf, missing),
            )
            .is_empty()
        } else {
            calendar_window_expects_bar(&last.symbol, last.tf, missing)
        };
        if expects_bar {
            return true;
        }
        let following = next_window_boundary(last.tf, missing);
        if following <= missing {
            return true;
        }
        missing = following;
    }
    false
}

fn fixed_aggregate_rows_need_fill(
    rows: &[Bar],
    symbol: &str,
    tf: Timeframe,
    market_now_ms: i64,
) -> bool {
    if bars_have_unexpected_intraday_gap(rows) {
        return true;
    }
    let current_window = tf.boundary_align(market_now_ms);
    let latest_closed_start = current_window.saturating_sub(tf.duration_ms());
    // During a normal FX closure there is no candle expected immediately
    // before the current wall-clock boundary, so an older tail is not a gap.
    if expected_market_bar_opens(symbol, tf, latest_closed_start, current_window).is_empty() {
        return false;
    }
    rows.last().map_or(true, |bar| bar.ts < latest_closed_start)
}

fn recent_fixed_aggregate_history_needs_fill(
    store: &SqliteStore,
    symbol: &str,
    market_now_ms: i64,
) -> Result<bool> {
    for tf in [Timeframe::M5, Timeframe::M15, Timeframe::M30, Timeframe::H1] {
        let rows = store.recent_bars(symbol, tf, AGGREGATE_GAP_INSPECTION_LIMIT)?;
        if fixed_aggregate_rows_need_fill(&rows, symbol, tf, market_now_ms) {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn ensure_smt_source_history(
    store: &SqliteStore,
    client: &TvClient,
    symbols: &[String],
    market_now_ms: i64,
) -> Result<usize> {
    let mut repaired_pairs = 0usize;
    for symbol in symbols {
        for tf in [Timeframe::M30, Timeframe::H1] {
            // Inspect the same horizon that we are prepared to fetch.  Looking at
            // only the minimum replay tail can hide an older hole inside the
            // retained 1,500-bar SMT window (notably the M30 July gap seen in
            // CHF/CAD/AUD/NZD after the destructive aggregate repair).
            let existing = store.recent_bars(symbol, tf, SMT_SOURCE_HISTORY_LIMIT)?;
            if !history_needs_backfill(&existing, SMT_SOURCE_MIN_BARS, market_now_ms) {
                continue;
            }
            tracing::warn!(
                %symbol,
                tf = %tf.tag(),
                rows = existing.len(),
                large_gap = bars_have_large_gap(&existing),
                "SMT source history incomplete; fetching authoritative TV history"
            );
            match client
                .fetch_history(symbol, tf, SMT_SOURCE_HISTORY_LIMIT as u32)
                .await
            {
                Ok(fetched) if !fetched.is_empty() => {
                    let finalized = finalized_history_bars(&fetched, market_now_ms);
                    store.insert_bars(&finalized)?;
                    let refreshed = store.recent_bars(symbol, tf, SMT_SOURCE_HISTORY_LIMIT)?;
                    if history_needs_backfill(&refreshed, SMT_SOURCE_MIN_BARS, market_now_ms) {
                        tracing::warn!(
                            %symbol,
                            tf = %tf.tag(),
                            fetched = fetched.len(),
                            retained = refreshed.len(),
                            "TV backfill completed but SMT source coverage is still incomplete"
                        );
                    } else {
                        repaired_pairs += 1;
                        tracing::info!(
                            %symbol,
                            tf = %tf.tag(),
                            fetched = fetched.len(),
                            "SMT source history backfilled"
                        );
                    }
                }
                Ok(_) => tracing::warn!(%symbol, tf = %tf.tag(), "TV returned no SMT history"),
                Err(error) => tracing::warn!(
                    ?error,
                    %symbol,
                    tf = %tf.tag(),
                    "SMT source history backfill failed"
                ),
            }
        }
    }
    Ok(repaired_pairs)
}

/// Fetch native H4 only when strict H1 aggregation exposes an internal closed
/// window with missing children. The native rows are temporary evidence: the
/// canonicalization pass below keeps only on-grid rows that agree with the
/// available H1 OHLC and removes the provider's off-grid rows again.
async fn backfill_native_h4_for_incomplete_windows(
    store: &SqliteStore,
    client: &TvClient,
    symbols: &[String],
    market_now_ms: i64,
) -> Result<usize> {
    let mut repaired_symbols = 0usize;
    for symbol in symbols {
        if !is_forex_symbol(symbol) {
            continue;
        }
        let h1 = store.recent_bars(symbol, Timeframe::H1, H4_PREFLIGHT_H1_LIMIT)?;
        if !has_incomplete_h4_window(&h1, symbol, market_now_ms) {
            continue;
        }
        tracing::warn!(
            %symbol,
            "canonical H4 window incomplete; fetching native H4 fallback"
        );
        match client
            .fetch_history(symbol, Timeframe::H4, H4_NATIVE_FALLBACK_LIMIT as u32)
            .await
        {
            Ok(fetched) if !fetched.is_empty() => {
                let finalized = finalized_history_bars(&fetched, market_now_ms);
                store.insert_bars(&finalized)?;
                repaired_symbols += 1;
                tracing::info!(
                    %symbol,
                    fetched = fetched.len(),
                    finalized = finalized.len(),
                    "native H4 fallback history staged"
                );
            }
            Ok(_) => tracing::warn!(%symbol, "TV returned no native H4 fallback history"),
            Err(error) => tracing::warn!(
                ?error,
                %symbol,
                "native H4 fallback fetch failed"
            ),
        }
    }
    Ok(repaired_symbols)
}

/// Rebuild the recent fixed intraday timeframes from the same M1 source used
/// by live detection. This removes stale/sparse rows left by older builds and
/// prevents the chart, SMT replay and detector engine from reading different
/// OHLC for the same timestamp. Deeper native history is kept outside the M1
/// coverage window.
fn repair_recent_aggregates(store: &SqliteStore, symbol: &str) -> Result<usize> {
    let m1 = store.recent_bars(symbol, Timeframe::M1, AGGREGATE_REPAIR_M1_LIMIT)?;
    if m1.is_empty() {
        return Ok(0);
    }

    // Split around genuinely missing history. The old implementation used
    // one [first M1, last M1] replacement range; when a newly-added symbol had
    // a multi-week M1 hole, that deleted otherwise-complete native M30/H1/H4
    // candles from the hole and caused its SMT replay to collapse.
    let mut spans: Vec<&[Bar]> = Vec::new();
    let mut span_start = 0usize;
    for index in 1..m1.len() {
        if m1[index].ts.saturating_sub(m1[index - 1].ts) > MAX_HISTORY_GAP_MS {
            spans.push(&m1[span_start..index]);
            span_start = index;
        }
    }
    spans.push(&m1[span_start..]);

    let mut repaired = 0usize;
    for span in spans {
        let (Some(_first), Some(_last)) = (span.first(), span.last()) else {
            continue;
        };
        let mut aggregator = SymbolAggregator::new(symbol);
        let mut rebuilt: std::collections::HashMap<Timeframe, Vec<Bar>> =
            std::collections::HashMap::new();
        for bar in span {
            for event in aggregator.warm_start_with_closed(bar) {
                let BarEvent::BarClosed(higher) = event else {
                    continue;
                };
                if matches!(
                    higher.tf,
                    Timeframe::M5 | Timeframe::M15 | Timeframe::M30 | Timeframe::H1 | Timeframe::H4
                ) {
                    rebuilt.entry(higher.tf).or_default().push(higher);
                }
            }
        }
        for tf in [
            Timeframe::M5,
            Timeframe::M15,
            Timeframe::M30,
            Timeframe::H1,
            Timeframe::H4,
        ] {
            let rows = rebuilt.remove(&tf).unwrap_or_default();
            // A fresh aggregator does not emit the partial bucket at either
            // edge. Replace only timestamps for which a complete rebuilt bar
            // exists; authoritative native bars at the gap edges survive.
            let (Some(first_row), Some(last_row)) = (rows.first(), rows.last()) else {
                continue;
            };
            store.replace_bars_in_range(symbol, tf, first_row.ts, last_row.ts, &rows)?;
            repaired += rows.len();
        }
    }
    Ok(repaired)
}

/// Fill closed intraday candles that are absent even though their complete M1
/// source window is already available locally.
///
/// This is deliberately additive. A TradingView reconnect can backfill older
/// M1 minutes after the live aggregator has already advanced beyond that
/// window. Replaying those minutes through the live detector would be
/// out-of-order, while replacing the whole higher-TF slice could overwrite a
/// native provider candle. Rebuilding in an isolated aggregator and using
/// `INSERT OR IGNORE` repairs only the missing keys and leaves every existing
/// row untouched.
fn fill_missing_recent_aggregates(store: &SqliteStore, symbol: &str) -> Result<usize> {
    let m1 = store.recent_bars(symbol, Timeframe::M1, AGGREGATE_GAP_FILL_M1_LIMIT)?;
    if m1.is_empty() {
        return Ok(0);
    }

    let mut aggregator = SymbolAggregator::new(symbol);
    let mut rebuilt = Vec::new();
    for bar in &m1 {
        for event in aggregator.warm_start_with_closed(bar) {
            let BarEvent::BarClosed(higher) = event else {
                continue;
            };
            // H4 has a separate canonical H1-based reconciliation path. Keep
            // this reconnect repair limited to fixed intraday chart TFs.
            if matches!(
                higher.tf,
                Timeframe::M5 | Timeframe::M15 | Timeframe::M30 | Timeframe::H1
            ) {
                rebuilt.push(higher);
            }
        }
    }
    store.insert_bars_if_missing(&rebuilt)
}

/// Canonicalize the H4 slice covered by retained H1 history.
///
/// This deliberately replaces only the H1-covered range. Deeper native H4
/// chart history is preserved, while sparse/off-grid rows inside the
/// auditable range are removed. Returning `None` means there is not enough H1
/// evidence to safely rewrite anything; callers can then let the coverage
/// guard disable only the H4 SMT branch instead of fabricating completeness.
fn canonicalize_forex_h4_from_h1(
    store: &SqliteStore,
    symbol: &str,
    market_now_ms: i64,
) -> Result<Option<usize>> {
    if !is_forex_symbol(symbol) {
        return Ok(None);
    }
    let h1 = store.recent_bars(symbol, Timeframe::H1, H4_PREFLIGHT_H1_LIMIT)?;
    if h1.len() < H4_REPLAY_MIN_BARS * 4 || bars_have_large_gap(&h1) {
        return Ok(None);
    }
    let Some(first) = h1.first() else {
        return Ok(None);
    };
    let Some(last) = h1.last() else {
        return Ok(None);
    };
    // Keep a snapshot before replacing the covered slice. It may contain a
    // TV-native on-grid candle staged specifically for an H1 provider hole.
    let native_h4 = store.recent_bars(symbol, Timeframe::H4, H4_NATIVE_FALLBACK_LIMIT)?;
    let rebuilt = aggregate_h4_with_native_fallback(&h1, &native_h4, symbol, market_now_ms);
    if rebuilt.len() < H4_REPLAY_MIN_BARS {
        return Ok(None);
    }
    let start = Timeframe::H4.boundary_align(first.ts);
    let end = Timeframe::H4.boundary_align(last.ts);
    store.replace_bars_in_range(symbol, Timeframe::H4, start, end, &rebuilt)?;
    Ok(Some(rebuilt.len()))
}

/// Return a human-readable reason when one pane cannot support a trustworthy
/// H4 -> H1 replay. Counts alone are insufficient: a short recent tail after
/// a multi-week hole used to pass through as if it were complete.
fn h4_replay_coverage_issue(
    store: &SqliteStore,
    symbols: &[String],
    market_now_ms: i64,
) -> Option<String> {
    const MAX_RECENT_GAP_MS: i64 = 5 * 24 * 60 * 60_000;
    const MAX_STALENESS_MS: i64 = 7 * 24 * 60 * 60_000;

    for symbol in symbols {
        let bars = match store.recent_bars(symbol, Timeframe::H4, H4_REPLAY_MIN_BARS as i64) {
            Ok(bars) => bars,
            Err(error) => return Some(format!("{symbol}: 4H 查询失败: {error}")),
        };
        if bars.len() < H4_REPLAY_MIN_BARS {
            return Some(format!(
                "{symbol}: 仅 {} 根 4H，至少需要 {H4_REPLAY_MIN_BARS} 根",
                bars.len()
            ));
        }
        if bars
            .windows(2)
            .any(|pair| pair[1].ts.saturating_sub(pair[0].ts) > MAX_RECENT_GAP_MS)
        {
            return Some(format!("{symbol}: 最近 4H 历史存在超过 5 天的断档"));
        }
        let latest = bars.last().map(|bar| bar.ts).unwrap_or_default();
        if market_now_ms.saturating_sub(latest) > MAX_STALENESS_MS {
            return Some(format!("{symbol}: 最近 4H 已超过 7 天未更新"));
        }
    }
    None
}

#[derive(Default)]
struct ReconnectHistoryPreflightState {
    running: bool,
    last_attempt_ms: Option<i64>,
    last_success_ms: Option<i64>,
}

/// Single-flight gate for the global history audit triggered by TradingView
/// historical snapshots. A union reconnect emits one snapshot per symbol;
/// those snapshots represent one reconnect, not seven independent audits.
#[derive(Default)]
struct ReconnectHistoryPreflightGate {
    state: parking_lot::Mutex<ReconnectHistoryPreflightState>,
}

impl ReconnectHistoryPreflightGate {
    fn try_begin(&self, market_now_ms: i64) -> bool {
        let mut state = self.state.lock();
        if state.running {
            return false;
        }
        if state.last_success_ms.is_some_and(|last| {
            market_now_ms.saturating_sub(last) < RECONNECT_PREFLIGHT_SUCCESS_COOLDOWN_MS
        }) {
            return false;
        }
        if state.last_attempt_ms.is_some_and(|last| {
            market_now_ms.saturating_sub(last) < RECONNECT_PREFLIGHT_RETRY_COOLDOWN_MS
        }) {
            return false;
        }
        state.running = true;
        state.last_attempt_ms = Some(market_now_ms);
        true
    }

    fn finish(&self, market_now_ms: i64, complete: bool) {
        let mut state = self.state.lock();
        state.running = false;
        if complete {
            state.last_success_ms = Some(market_now_ms);
        }
    }
}

#[derive(Default)]
struct ReconnectHistoryPreflightReport {
    repaired_pairs: usize,
    incomplete_pairs: usize,
}

async fn refresh_reconnect_history_pair(
    store: &SqliteStore,
    client: &TvClient,
    symbol: &str,
    tf: Timeframe,
    limit: i64,
    market_now_ms: i64,
) -> Result<bool> {
    let existing = store.recent_bars(symbol, tf, limit)?;
    if !chart_history_needs_provider_refresh(&existing, 2, market_now_ms) {
        return Ok(false);
    }

    tracing::warn!(
        %symbol,
        tf = %tf.tag(),
        rows = existing.len(),
        "reconnect audit found incomplete chart history; fetching provider snapshot"
    );
    match client.fetch_history(symbol, tf, limit as u32).await {
        Ok(fetched) if !fetched.is_empty() => {
            let persisted =
                persist_chart_provider_history(store, symbol, tf, &fetched, market_now_ms)?;
            tracing::info!(
                %symbol,
                tf = %tf.tag(),
                fetched = fetched.len(),
                persisted = persisted.len(),
                "reconnect chart history repaired"
            );
            Ok(true)
        }
        Ok(_) => {
            tracing::warn!(%symbol, tf = %tf.tag(), "provider returned no reconnect history");
            Ok(false)
        }
        Err(error) => {
            tracing::warn!(
                ?error,
                %symbol,
                tf = %tf.tag(),
                "reconnect chart history fetch failed"
            );
            Ok(false)
        }
    }
}

/// Audit and repair every monitored chart timeframe after connectivity is
/// restored. This runs behind the same provider lock used by chart refreshes,
/// so an automatic reconnect cannot open competing history sessions or
/// overwrite a newer interactive result.
async fn repair_reconnect_history_once(
    store: &SqliteStore,
    client: &TvClient,
    symbols: &[String],
    market_now_ms: i64,
) -> Result<ReconnectHistoryPreflightReport> {
    let mut report = ReconnectHistoryPreflightReport::default();

    // Repair the authoritative minute source first. Any recovered minute
    // holes can then rebuild closed fixed-period candles locally.
    for symbol in symbols {
        if refresh_reconnect_history_pair(
            store,
            client,
            symbol,
            Timeframe::M1,
            WARM_START_M1_LIMIT,
            market_now_ms,
        )
        .await?
        {
            report.repaired_pairs += 1;
        }
        report.repaired_pairs += fill_missing_recent_aggregates(store, symbol)?;
    }

    // M5/M15 are chart-native checks even though they can usually be rebuilt
    // from M1. Calendar periods always use provider candles normalized to the
    // same NY-local boundaries as the live aggregator.
    for symbol in symbols {
        for (tf, limit) in [
            (Timeframe::M5, RECONNECT_CHART_HISTORY_LIMIT),
            (Timeframe::M15, RECONNECT_CHART_HISTORY_LIMIT),
            (Timeframe::D1, RECONNECT_CALENDAR_HISTORY_LIMIT),
            (Timeframe::W1, RECONNECT_CALENDAR_HISTORY_LIMIT),
            (Timeframe::MN1, RECONNECT_CALENDAR_HISTORY_LIMIT),
        ] {
            if refresh_reconnect_history_pair(store, client, symbol, tf, limit, market_now_ms)
                .await?
            {
                report.repaired_pairs += 1;
            }
        }
    }

    report.repaired_pairs +=
        ensure_smt_source_history(store, client, symbols, market_now_ms).await?;
    report.repaired_pairs +=
        backfill_native_h4_for_incomplete_windows(store, client, symbols, market_now_ms).await?;
    for symbol in symbols {
        if canonicalize_forex_h4_from_h1(store, symbol, market_now_ms)?.is_some() {
            report.repaired_pairs += 1;
        }
    }

    // Re-read every pair after persistence. A provider outage or a bounded
    // response that still contains a hole keeps the audit incomplete and is
    // retried without requiring an app restart or manual timeframe switch.
    for symbol in symbols {
        for (tf, limit) in [
            (Timeframe::M1, WARM_START_M1_LIMIT),
            (Timeframe::M5, RECONNECT_CHART_HISTORY_LIMIT),
            (Timeframe::M15, RECONNECT_CHART_HISTORY_LIMIT),
            (Timeframe::D1, RECONNECT_CALENDAR_HISTORY_LIMIT),
            (Timeframe::W1, RECONNECT_CALENDAR_HISTORY_LIMIT),
            (Timeframe::MN1, RECONNECT_CALENDAR_HISTORY_LIMIT),
        ] {
            let bars = store.recent_bars(symbol, tf, limit)?;
            if chart_history_needs_provider_refresh(&bars, 2, market_now_ms) {
                report.incomplete_pairs += 1;
            }
        }
        for tf in [Timeframe::M30, Timeframe::H1] {
            let bars = store.recent_bars(symbol, tf, SMT_SOURCE_HISTORY_LIMIT)?;
            if history_needs_backfill(&bars, SMT_SOURCE_MIN_BARS, market_now_ms) {
                report.incomplete_pairs += 1;
            }
        }
        if h4_replay_coverage_issue(store, std::slice::from_ref(symbol), market_now_ms).is_some() {
            report.incomplete_pairs += 1;
        }
    }

    Ok(report)
}

async fn run_reconnect_history_preflight(
    app: tauri::AppHandle,
    store: SqliteStore,
    auth: TvAuth,
    symbols: Vec<String>,
    history_fetch_lock: Arc<AsyncMutex<()>>,
    gate: Arc<ReconnectHistoryPreflightGate>,
) {
    // Give the per-symbol workers a short head start to persist the reconnect
    // M1 snapshots before inspecting the shared store.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let client = TvClient::with_auth(auth);

    for attempt in 1..=RECONNECT_PREFLIGHT_MAX_ATTEMPTS {
        let result = {
            let _history_guard = history_fetch_lock.lock().await;
            repair_reconnect_history_once(&store, &client, &symbols, now_ms()).await
        };
        let finished_at = now_ms();
        let (complete, repaired_pairs, incomplete_pairs) = match result {
            Ok(report) => (
                report.incomplete_pairs == 0,
                report.repaired_pairs,
                report.incomplete_pairs,
            ),
            Err(error) => {
                tracing::warn!(?error, attempt, "reconnect history preflight failed");
                (false, 0, 1)
            }
        };
        gate.finish(finished_at, complete);

        if repaired_pairs > 0 {
            let _ = app.emit("seed_complete", ());
        }
        tracing::info!(
            attempt,
            repaired_pairs,
            incomplete_pairs,
            complete,
            "reconnect history preflight finished"
        );
        if complete || attempt == RECONNECT_PREFLIGHT_MAX_ATTEMPTS {
            break;
        }

        tokio::time::sleep(Duration::from_millis(
            RECONNECT_PREFLIGHT_RETRY_COOLDOWN_MS as u64,
        ))
        .await;
        if !gate.try_begin(now_ms()) {
            break;
        }
    }
}

#[path = "../runtime_log.rs"]
mod runtime_log;

fn main() {
    let db_path = db_path();
    let logs_dir = db_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("logs");
    let log = runtime_log::RuntimeLog::open(&logs_dir, 20 * 1024 * 1024).unwrap_or_else(|error| {
        eprintln!("Cannot open ICT runtime log: {error}");
        runtime_log::RuntimeLog::stderr()
    });
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,ict_monitor=info")),
        )
        .with_target(false)
        .with_ansi(false)
        .with_writer(move || log.clone())
        .init();

    tracing::info!(
        pid = std::process::id(),
        "ICT Radar started; authoritative closes enabled"
    );
    let store = SqliteStore::open(&db_path).expect("open sqlite");
    tracing::info!(path = %db_path.display(), "sqlite store opened");

    let cfg = AppConfig::load().unwrap_or_default();
    // Seed the watchlist the user last selected (persisted), not just the
    // first one in config - otherwise a restart seeds the wrong watchlist
    // and the real one gets re-seeded on the front-end's switch (double
    // work + a multi-minute freeze while the warm-start seed blocks the
    // live TV feed).
    let active_watchlist = cfg
        .active_watchlist_id
        .as_deref()
        .and_then(|id| cfg.watchlists.iter().find(|w| w.id == id).cloned())
        .or_else(|| cfg.watchlists.first().cloned())
        .unwrap_or_else(|| {
            ict_monitor::watchlist::default_watchlists()
                .into_iter()
                .next()
                .unwrap()
        });
    // Subscribe once to the exact union of strategy groups. Keep BTC in the
    // manual symbol picker, but do not spend a live subscription on it unless
    // a configured group explicitly contains it.
    let monitored_symbols = monitored_symbol_union(&cfg.watchlists);
    let mut all_symbols = monitored_symbols.clone();
    for symbol in &cfg.chart_symbols {
        if !all_symbols.contains(symbol) { all_symbols.push(symbol.clone()); }
    }
    if !all_symbols.contains(&"COINBASE:BTCUSD".to_string()) {
        all_symbols.push("COINBASE:BTCUSD".to_string());
    }
    let tv_auth = TvAuth {
        sessionid: cfg.tradingview.sessionid.clone(),
        sessionid_sign: cfg.tradingview.sessionid_sign.clone(),
        proxy_url: cfg.tradingview.proxy_url.clone(),
    };
    let tv_client = TvClient::with_auth(tv_auth.clone());
    let feishu_sender = FeishuAlertSender::new(cfg.alerts.feishu.clone());
    let feishu_notify_default = cfg.alerts.feishu.enabled;

    let aggregators: Arc<
        AsyncMutex<std::collections::HashMap<String, Arc<AsyncMutex<SymbolAggregator>>>>,
    > = Arc::new(AsyncMutex::new(std::collections::HashMap::new()));

    // Build engine BEFORE Tauri so commands can read it.
    if let Err(e) = store.ensure_ict_schema() {
        tracing::error!(error = ?e, "ensure_ict_schema failed");
    }
    if let Err(e) = store.ensure_smt_schema() {
        tracing::error!(error = ?e, "ensure_smt_schema failed");
    }
    if let Err(e) = store.ensure_detector_config_schema() {
        tracing::error!(error = ?e, "ensure_detector_config_schema failed");
    }
    if let Err(e) = store.ensure_candidate_schema() {
        tracing::error!(error = ?e, "ensure_candidate_schema failed");
    }
    if let Err(e) = store.ensure_alert_schema() {
        tracing::error!(error = ?e, "ensure_alert_schema failed");
    }
    let llm_decisions = match build_configured_pipeline(&cfg.llm, store.clone()) {
        Ok(pipeline) => pipeline,
        Err(error) => {
            tracing::warn!(
                ?error,
                "LLM sidecar failed to configure; continuing without it"
            );
            None
        }
    };
    match store.reset_derived_smt_pipeline_for_current_rule() {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            rows = n,
            rule_version = CURRENT_RULE_VERSION,
            "reset derived SMT/Candidate/Alert rows for current rule"
        ),
        Err(e) => tracing::error!(error = ?e, "current SMT rule rebuild reset failed"),
    }
    match store.repair_candidates_from_invalidated_smts(now_ms()) {
        Ok(0) => {}
        Ok(n) => tracing::info!(rows = n, "repaired candidates with invalidated source SMT"),
        Err(e) => tracing::error!(error = ?e, "candidate/SMT consistency repair failed"),
    }
    // cleanup_orphaned_structures REMOVED: its 6x tf-duration
    // threshold was too aggressive for 5m (30 min), marking
    // legitimate CISD structures as invalidated. The bar-
    // continuity guard already prevents phantom structures.
    // purge_invalidated_structures still runs to clean up rows
    // invalidated by the emit-task during live processing.
    {
        match store.purge_invalidated_structures() {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                rows = n,
                "purge_invalidated_structures: dropped historical invalidated rows"
            ),
            Err(e) => tracing::error!(error = ?e, "purge_invalidated_structures failed"),
        }
        // Remove seed/test data from candidates and alerts (leftover from
        // manual UI testing). The seed script has been deleted; this ensures
        // any previously-inserted rows are cleaned on next restart.
        match store.purge_seed_data() {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                rows = n,
                "purge_seed_data: removed seed/test entries from candidates and alerts"
            ),
            Err(e) => tracing::error!(error = ?e, "purge_seed_data failed"),
        }
        // PDH/PDL one-shot cleanup. Older builds emitted both the
        // rollover pair AND the running-day pair (current-day high/low),
        // leaving two PDH and two PDL rows per symbol in the table.
        // Active build (option A) emits only the rollover pair; wipe and
        // let cold-start seed re-emit the canonical single pair below.
        match store.purge_pdh_pdl() {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                rows = n,
                "purge_pdh_pdl: dropped pdh/pdl rows so cold-start seed can re-emit a single canonical pair"
            ),
            Err(e) => tracing::error!(error = ?e, "purge_pdh_pdl failed"),
        }
    }
    let engine = EngineHandle::new(BROADCAST_CAPACITY);

    let global_alert_cooldown = Arc::new(parking_lot::Mutex::new(0_i64));
    let mut group_map = std::collections::HashMap::new();
    for watchlist in &cfg.watchlists {
        let mut smt_engine = SmtEngine::new();
        smt_engine.set_watchlist(watchlist.clone());
        let smt = Arc::new(parking_lot::Mutex::new(smt_engine));
        let candidates = Arc::new(parking_lot::Mutex::new(CandidateEngine::new()));
        candidates.lock().set_watchlist(watchlist.id.clone());
        let alerts = Arc::new(parking_lot::Mutex::new(
            AlertEngine::with_store_and_global_cooldown(
                store.clone(),
                global_alert_cooldown.clone(),
            ),
        ));
        alerts.lock().set_feishu_notify(feishu_notify_default);
        group_map.insert(
            watchlist.id.clone(),
            Arc::new(EngineGroup {
                watchlist: watchlist.clone(),
                smt,
                candidates,
                alerts,
                llm_decisions: llm_decisions.clone(),
            }),
        );
    }
    let engine_groups: EngineGroups = Arc::new(parking_lot::RwLock::new(group_map));
    let active_watchlist_h: WatchlistHandle =
        Arc::new(AsyncMutex::new(Some(active_watchlist.clone())));
    let config: Arc<parking_lot::RwLock<AppConfig>> = Arc::new(parking_lot::RwLock::new(cfg));

    let engine_groups_for_setup = engine_groups.clone();
    let llm_decisions_for_setup = llm_decisions.clone();
    let seeding_guard = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let history_fetch_lock = Arc::new(AsyncMutex::new(()));
    let app_state = AppState {
        store: store.clone(),
        structures_cache: Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
        symbols: all_symbols.clone(),
        tv_client: tv_client.clone(),
        history_fetch_lock: history_fetch_lock.clone(),
        aggregators: aggregators.clone(),
        engine: engine.clone(),
        engine_groups: engine_groups.clone(),
        llm_decisions: llm_decisions.clone(),
        global_alert_cooldown: global_alert_cooldown.clone(),
        feishu_sender: feishu_sender.clone(),
        active_watchlist: active_watchlist_h.clone(),
        config: config.clone(),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            list_symbols,
            search_symbols,
            copy_screenshot,
            list_symbol_prices,
            get_history,
            get_cached_history,
            get_history_cutoff,
            add_symbol,
            remove_symbol,
            list_structures,
            set_detector_enabled,
            set_detector_param,
            set_detector_params,
            set_detector_params_config_only,
            ui_log,
            list_watchlists,
            get_active_watchlist,
            create_watchlist,
            update_watchlist,
            delete_watchlist,
            restore_default_watchlists,
            default_correlation_cmd,
            list_smt,
            get_smt_snapshot,
            list_runtime_logs,
            list_candidates,
            list_decisions,
            list_llm_decision_items,
            list_alerts,
            list_reversals,
            set_alert_param,
            clear_alerts,
            test_desktop_notification,
            test_feishu_notification,
            set_viewing_group,
            set_active_watchlist,
        ])
        .setup(move |app| {
            ict_monitor::alert::prepare_desktop_notifications();
            feishu_sender.start_worker();
            if let Some(pipeline) = llm_decisions_for_setup.clone() {
                tauri::async_runtime::spawn(async move {
                    match pipeline.recover_stale_pending().await {
                        Ok(0) => {}
                        Ok(recovered) => tracing::info!(
                            recovered,
                            "recovered stale live LLM decisions from durable reservations"
                        ),
                        Err(error) => tracing::warn!(
                            ?error,
                            "stale LLM decision recovery failed; app continues normally"
                        ),
                    }
                });
            }
            // M6b: register alert channels (need app handle).
            {
                for group in engine_groups_for_setup.read().values() {
                    let mut ae = group.alerts.lock();
                    ae.add_channel(Box::new(InboxChannel::new(
                        store.clone(),
                        app.handle().clone(),
                    )));
                    ae.add_channel(Box::new(DesktopNotifyChannel::new(app.handle().clone())));
                    ae.add_channel(Box::new(FeishuNotifyChannel::new(feishu_sender.clone())));
                }
            }
            let handle = app.handle().clone();
            let store_clone = store.clone();
            let monitored_symbols_clone = monitored_symbols.clone();
            let aggs_clone = aggregators.clone();
            let tv_auth_clone = tv_auth.clone();
            let engine_for_loop = engine.clone();
            let engine_groups_for_loop = engine_groups_for_setup.clone();

            // Spawn an emit task that pushes StructureEvent → Tauri AND
            // mirrors persistence: New/Update upsert into ict_structures,
            // Invalidated marks the row state='invalidated'. This is the
            // single source of truth for SQLite — the historical
            // `persist_engine_after_step` snapshots are kept as a safety
            // net but no longer required for correctness on Invalidated.
            {
                let handle = handle.clone();
                let mut rx = engine.event_tx.subscribe();
                let store_for_emit = store.clone();
                tauri::async_runtime::spawn(async move {
                    use ict_monitor::detector::types::StructureEvent;
                    let mut kz_received: u64 = 0;
                    loop {
                        match rx.recv().await {
                            Ok(ev) => {
                                let topic = ev.topic();
                                let now = now_ms();
                                match &ev {
                                    StructureEvent::New(s) | StructureEvent::Update(s) => {
                                        if s.kind_tag() == "kill_zone" {
                                            kz_received += 1;
                                            if kz_received <= 16 {
                                                tracing::info!(
                                                    id = %s.id(), n = kz_received,
                                                    "emit-task received kill_zone"
                                                );
                                            }
                                        }
                                        if matches!(s.kind_tag(), "pdh" | "pdl") {
                                            let op = match &ev {
                                                StructureEvent::New(_) => "new",
                                                StructureEvent::Update(_) => "update",
                                                _ => "?",
                                            };
                                            tracing::info!(
                                                target: "pdh_pdl_trace",
                                                op,
                                                id = %s.id(),
                                                kind = s.kind_tag(),
                                                state = s.state_tag(),
                                                topic,
                                                "emit-task pdh/pdl event"
                                            );
                                        }
                                        if let Err(e) = store_for_emit.upsert_structure(s, now) {
                                            tracing::warn!(error = ?e, id = %s.id(), "emit-task upsert failed");
                                        }
                                    }
                                    StructureEvent::Invalidated { id, .. } => {
                                        if let StructureEvent::Invalidated { kind, .. } = &ev {
                                            if matches!(kind.as_str(), "pdh" | "pdl") {
                                                tracing::info!(
                                                    target: "pdh_pdl_trace",
                                                    op = "invalidated",
                                                    %id,
                                                    %kind,
                                                    "emit-task pdh/pdl event"
                                                );
                                            }
                                        }
                                        if let Err(e) = store_for_emit.mark_structure_invalidated(id, now) {
                                            tracing::warn!(error = ?e, %id, "emit-task invalidate failed");
                                        }
                                    }
                                }
                                let _ = handle.emit(topic, ev);
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!(skipped = n, "emit-task lagged on broadcast — dropped events");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                tracing::warn!("emit-task: broadcast closed, exiting");
                                break;
                            }
                        }
                    }
                });
            }

            // M5: create bar_tx + SubManager here in setup (not inside the
            // async subscription loop) so watchlist commands can access
            // SubManager immediately, without racing the spawned task.
            let (bar_tx, _) = broadcast::channel::<BarPayload>(BROADCAST_CAPACITY);
            let sub_manager = Arc::new(AsyncMutex::new(SubManager {
                store: store_clone.clone(),
                engine: engine_for_loop.clone(),
                engine_groups: engine_groups_for_loop.clone(),
                aggregators: aggs_clone.clone(),
                auth: tv_auth_clone.clone(),
                bar_tx: bar_tx.clone(),
                app: handle.clone(),
                subs: std::collections::HashMap::new(),
                feed_handle: None,
                history_seed_lock: Arc::new(AsyncMutex::new(())),
                history_fetch_lock: history_fetch_lock.clone(),
                reconnect_preflight_gate: Arc::new(ReconnectHistoryPreflightGate::default()),
            }));
            handle.manage(sub_manager.clone());

            tauri::async_runtime::spawn(async move {
                if let Err(e) = run_subscription_loop(
                    handle.clone(),
                    store_clone,
                    monitored_symbols_clone,
                    engine_for_loop,
                    engine_groups_for_loop,
                    bar_tx,
                    sub_manager,
                    seeding_guard.clone(),
                )
                .await
                {
                    tracing::error!(error = ?e, "subscription loop exited with error");
                    let _ = handle.emit(
                        "app:status",
                        AppStatusPayload {
                            kind: "fatal".into(),
                            message: format!("{e}"),
                        },
                    );
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

async fn run_subscription_loop(
    app: tauri::AppHandle,
    store: SqliteStore,
    monitored_symbols: Vec<String>,
    engine: EngineHandle,
    engine_groups: EngineGroups,
    bar_tx: broadcast::Sender<BarPayload>,
    sub_manager: Arc<AsyncMutex<SubManager>>,
    seeding_guard: Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    // ---- Bootstrap ICT detector engine -----------------------------------
    bootstrap_detectors(&engine, &monitored_symbols);
    // Re-apply persisted detector config (PO3 thresholds etc.) so the
    // detectors use the user's saved settings instead of defaults.
    for group in engine_groups.read().values() {
        load_and_apply_detector_config(&engine, &store, &group.candidates, &group.alerts);
    }

    // Row counts alone cannot prove that history is usable: a native batch
    // from an older session plus a short live tail can satisfy `LIMIT 1100`
    // while hiding a multi-week hole in the middle. Repair M30/H1 before the
    // destructive aggregate reconciliation and before any SMT replay.
    let history_client = {
        let manager = sub_manager.lock().await;
        TvClient::with_auth(manager.auth.clone())
    };
    let history_now = now_ms();
    match ensure_smt_source_history(&store, &history_client, &monitored_symbols, history_now).await
    {
        Ok(0) => {}
        Ok(repaired_pairs) => tracing::info!(
            repaired_pairs,
            "SMT source history preflight repaired incomplete symbol/TF pairs"
        ),
        Err(error) => tracing::warn!(?error, "SMT source history preflight failed"),
    }

    // Older builds persisted incomplete/misaligned local aggregates beside
    // TV-native rows. Repair the recent M1-covered slice before any detector,
    // PDA or SMT replay reads it.
    for symbol in &monitored_symbols {
        match repair_recent_aggregates(&store, symbol) {
            Ok(0) => {}
            Ok(repaired) => {
                tracing::info!(%symbol, repaired, "recent aggregates reconciled from M1")
            }
            Err(error) => tracing::warn!(?error, %symbol, "recent aggregate repair failed"),
        }
    }

    // A short M1 outage can remove one or two strict H1 rows and therefore a
    // whole canonical H4 candle. Recover those windows from TV-native H4
    // before canonicalization; off-grid native rows are filtered immediately
    // by `canonicalize_forex_h4_from_h1`.
    match backfill_native_h4_for_incomplete_windows(
        &store,
        &history_client,
        &monitored_symbols,
        now_ms(),
    )
    .await
    {
        Ok(0) => {}
        Ok(repaired_symbols) => {
            tracing::info!(repaired_symbols, "native H4 fallback preflight completed")
        }
        Err(error) => tracing::warn!(?error, "native H4 fallback preflight failed"),
    }

    // M6c preflight must run before the detector seed below. Previously H4
    // was canonicalized only after a user opened an H4 chart, so the new
    // groups replayed against 22-36 sparse native candles and silently lost
    // their entire H4 -> H1 branch.
    let preflight_now = now_ms();
    for symbol in &monitored_symbols {
        match canonicalize_forex_h4_from_h1(&store, symbol, preflight_now) {
            Ok(Some(rebuilt)) => tracing::info!(
                %symbol,
                rebuilt,
                "startup H4 canonicalized from retained H1"
            ),
            Ok(None) => tracing::warn!(
                %symbol,
                "startup H4 preflight skipped: insufficient canonical H1 coverage"
            ),
            Err(error) => {
                tracing::warn!(?error, %symbol, "startup H4 canonicalization failed")
            }
        }
    }

    // One-time PO3 re-detection migration (PRAGMA user_version < 1): wipe
    // every power_of_3 row BEFORE hydration so the engine starts with no PO3,
    // then the normal cold-start seed (500 bars) re-detects them fresh with
    // the corrected distribution-box geometry. The rehydrate fix (range-lock
    // for DistributionConfirmed) ensures they survive future restarts. A deep
    // seed window was tried but is too slow - the PO3 cross-TF snapshot clones
    // all symbol structures per bar (quadratic). Guarded by user_version so it
    // runs exactly once; if it fails or crashes before marking done, the next
    // boot re-runs it idempotently.
    let deep_seed = match store.user_version() {
        Ok(v) if v < 1 => {
            match store.delete_all_po3() {
                Ok(n) => tracing::info!(
                    deleted = n,
                    "PO3 re-detection migration: wiped all power_of_3 rows; deep seed this boot"
                ),
                Err(e) => tracing::warn!(
                    error = ?e,
                    "PO3 migration: delete_all_po3 failed; deep-seeding anyway"
                ),
            }
            true
        }
        _ => false,
    };

    // Do not hydrate SQLite structures before the cold-start seed. Several
    // detectors take a snapshot of the current structure set on every bar;
    // preloading years of structures made the bounded history replay
    // effectively quadratic and blocked both the TV subscriptions and Tauri
    // commands for minutes. Seed the detectors from bars first, then merge the
    // SQLite source of truth below. PO3 also remains disabled while seeding so
    // its cross-TF snapshots do not reintroduce the same startup cost.

    // Cold-start seed: replay the most recent N bars per (symbol, tf) through
    // the engine so detectors have material to chew on even when the market
    // is closed (e.g. weekends — TV won't push new BarClosed). The bars are
    // already in SQLite from M2's history backfill. Keep the window small
    // (1500 / 500) to bound startup CPU; same as DETECTOR_HISTORY.
    //
    // Expose startup replay progress through the existing readiness guard.
    seeding_guard.store(true, std::sync::atomic::Ordering::SeqCst);
    {
        // Suppress event broadcast during cold-start seed to avoid
        // overflowing the broadcast channel (capacity 64) with tens of
        // thousands of historical events. The front-end re-fetches via
        // list_structures after the seed_complete event.
        engine
            .lock()
            .seeding
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Same seven TFs as `core_tfs` so the cold-start replay actually
        // exercises every registered detector — without W1 here, weekly
        // detector instances would receive zero history and the chart
        // would render empty until a fresh weekly close arrives (which on
        // forex pairs takes up to a week).
        let detector_tfs: &[Timeframe] = &[
            Timeframe::M1,
            Timeframe::M5,
            Timeframe::M15,
            Timeframe::M30,
            Timeframe::H1,
            Timeframe::H4,
            Timeframe::D1,
            Timeframe::W1,
        ];
        for sym in &monitored_symbols {
            for tf in detector_tfs.iter().copied() {
                // 1m needs 3000 (≈50h) so PdhPdlDetector::flush has enough
                // history to scan a full previous civil day window even on a
                // Monday morning cold start when the seed window's start would
                // otherwise land mid-prev-day. Other TFs stay at 500 — those
                // detectors only key off the recent tail.
                let limit: i64 = if matches!(tf, Timeframe::M1) {
                    3000
                } else {
                    500
                };
                let bars = match store.recent_bars(sym, tf, limit) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(error = ?e, %sym, tf = %tf.tag(), "seed: recent_bars failed");
                        continue;
                    }
                };
                if bars.is_empty() {
                    continue;
                }
                let first_ts = bars.first().map(|b| b.ts).unwrap_or(0);
                let last_ts = bars.last().map(|b| b.ts).unwrap_or(0);
                let is_asc = first_ts <= last_ts;
                tracing::info!(
                    %sym, tf = %tf.tag(), n = bars.len(),
                    first_ts, last_ts, is_asc,
                    "seed: bar order check"
                );
                let started = std::time::Instant::now();
                {
                    let mut eng = engine.lock();
                    // Safety: clear any stale bucket history so the
                    // continuity guard does not skip bars.  On a fresh
                    // engine this is a no-op; if something populated the
                    // bucket before us (e.g. a race with the TV
                    // subscription task) this prevents bar starvation.
                    eng.clear_bucket_for_seeding(sym, tf);
                    // Use seed_closed_bar so the historical replay does NOT
                    // trip the bar-gap continuity guard on weekends — that
                    // guard was clearing 1m history every time the SQLite
                    // backfill crossed a Sunday close, leaving only the last
                    // contiguous segment (~8h) in the bucket. PDH/PDL flush
                    // then could not cover a full prev-day window, so the
                    // user saw "黄线消失" after toggling daily_boundary mode.
                    // recent_bars already returns ASC (oldest-first); feed bars in order so
                    // the continuity guard (bar.ts <= prev.ts => skip) keeps them all.
                    for bar in bars.iter() {
                        eng.seed_closed_bar(bar);
                    }
                }
                tracing::info!(
                    %sym, tf = %tf.tag(), n = bars.len(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "engine seeded from sqlite history"
                );
            }
        }
        // Re-hydrate from SQLite after seed: the truncated seed window
        // (500 bars) may cause detectors to lose structures they can't
        // maintain with limited context. Re-hydrating restores these
        // from the SQLite source of truth so they survive the persist.
        // Only insert structures not already in the engine (don't
        // overwrite newer versions created/updated during seed).
        {
            let mut restored = 0usize;
            for sym in &monitored_symbols {
                for tf in [
                    Timeframe::M1,
                    Timeframe::M5,
                    Timeframe::M15,
                    Timeframe::M30,
                    Timeframe::H1,
                    Timeframe::H4,
                    Timeframe::D1,
                    Timeframe::W1,
                ] {
                    if let Ok(rows) = store.list_active_structures(sym, tf) {
                        let eng = engine.lock();
                        for r in rows {
                            let id = r.id().to_string();
                            if !eng.structures.contains_key(&id) {
                                eng.hydrate(r);
                                restored += 1;
                            }
                        }
                    }
                }
            }
            if restored > 0 {
                tracing::info!(restored, "re-hydrated structures lost during seed");
            }
        }
        // The one-time PO3 migration deliberately removed the persisted PO3
        // rows above, so an old database has nothing to restore. Rebuild PO3
        // once from the seeded history before committing the migration. Normal
        // boots never enter this path and retain the fast startup behaviour.
        if deep_seed {
            for sym in &monitored_symbols {
                let replayed = engine.lock().replay_po3_for_symbol(sym);
                tracing::info!(
                    symbol = %sym,
                    replayed,
                    "PO3 migration: replayed seeded history"
                );
            }
        }
        // Persist whatever the seed + re-hydrate produced (no prune).
        persist_engine_after_step(&store, &engine, false);
        // The hydrated PO3 set remains the source of truth after the regular
        // detector seed. Enable normal PO3 processing only for future live
        // bars; symbols without a hydrated PO3 set are handled after their TV
        // history backfill by run_symbol_subscription.
        {
            let mut eng = engine.lock();
            for sym in &monitored_symbols {
                if eng.po3_count_for_symbol(sym) > 0 {
                    eng.po3_replay_done.insert(sym.clone());
                }
            }
        }
        if deep_seed {
            match store.set_user_version(1) {
                Ok(()) => tracing::info!(
                    "PO3 re-detection migration complete; user_version=1 (will not repeat)"
                ),
                Err(e) => tracing::warn!(
                    error = ?e,
                    "PO3 migration: failed to set user_version=1; will re-run next boot"
                ),
            }
        }
        engine
            .lock()
            .seeding
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let snap = { engine.lock().structures.len() };
        tracing::info!(active_structures = snap, "engine cold-start seed done");

        // M6c: hydrate/replay groups sequentially in stable id order. This
        // avoids three heavy history scans contending for CPU while keeping
        // every group live before subscriptions begin.
        let mut groups: Vec<Arc<EngineGroup>> = engine_groups.read().values().cloned().collect();
        groups.sort_by(|a, b| a.watchlist.id.cmp(&b.watchlist.id));
        for group in groups {
            let started = Instant::now();
            hydrate_smt(
                &group.smt,
                &store,
                &group.watchlist.id,
                &group.watchlist.symbols,
            );
            replay_smt_history(
                &group.smt,
                &group.candidates,
                &group.alerts,
                group.llm_decisions.as_ref(),
                &engine,
                &store,
                &app,
                &group.watchlist.symbols,
            );
            replay_candidates(
                &group.smt,
                &group.candidates,
                &group.alerts,
                &store,
                &group.watchlist.symbols,
            );
            tracing::info!(
                watchlist_id = %group.watchlist.id,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "M6c group cold-start replay complete"
            );
        }

        // Tell the front-end the engine has finished hydrating + seeding
        // (including SMT replay) so it can re-fetch `list_structures` and
        // pick up the full hydrated set without needing a manual toggle.
        let _ = app.emit("seed_complete", ());

        // Clear the guard so future watchlist switches can seed.
        seeding_guard.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    // M3 scope rollback: KillZone visualization is temporarily disabled.
    // Keep the detector module/types in the codebase for a later milestone,
    // but do not spawn the scheduler or emit KZ structures in this build.
    // (cfg already loaded by `main`; auth + aggregators handed in)
    // bar_tx is created in setup and passed in. Spawn the emit task that
    // forwards every broadcast payload as a Tauri event.
    // Spawn the emit task.
    let emit_app = app.clone();
    let mut emit_rx = bar_tx.subscribe();
    tokio::spawn(async move {
        loop {
            let payload = match emit_rx.recv().await {
                Ok(payload) => payload,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "bar event forwarding lagged; resuming");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let topic = if payload.closed {
                "bar:closed"
            } else {
                "bar:update"
            };
            let _ = emit_app.emit(topic, payload);
        }
    });

    let _ = app.emit(
        "app:status",
        AppStatusPayload {
            kind: "starting".into(),
            message: "warm-starting from sqlite".into(),
        },
    );

    // Subscribe once to the union. DXY therefore has one WebSocket task but
    // its bars are routed into all three strategy groups below.
    let subscribed_symbols = {
        let state = app.state::<AppState>();
        let cfg = state.config.read();
        let mut symbols = monitored_symbol_union(&cfg.watchlists);
        for symbol in &cfg.chart_symbols {
            if !symbols.contains(symbol) { symbols.push(symbol.clone()); }
        }
        symbols
    };
    sub_manager.lock().await.set_symbols(&subscribed_symbols);

    std::future::pending::<()>().await;
    #[allow(unreachable_code)]
    Ok(())
}

async fn run_symbol_subscription(
    app: tauri::AppHandle,
    store: SqliteStore,
    symbol: String,
    aggregators: Arc<
        AsyncMutex<std::collections::HashMap<String, Arc<AsyncMutex<SymbolAggregator>>>>,
    >,
    engine: EngineHandle,
    bar_tx: broadcast::Sender<BarPayload>,
    engine_groups: EngineGroups,
    history_seed_lock: Arc<AsyncMutex<()>>,
    mut rx: tokio::sync::mpsc::Receiver<BarEvent>,
) -> Result<()> {
    // Warm-start aggregator from local 1m history. We store it behind an
    // async mutex inside `aggregators` so command handlers (e.g.
    // `get_history`) can peek at the rolling, not-yet-closed bars.
    let agg_handle: Arc<AsyncMutex<SymbolAggregator>> =
        Arc::new(AsyncMutex::new(SymbolAggregator::new(&symbol)));
    {
        let mut map = aggregators.lock().await;
        map.insert(symbol.clone(), agg_handle.clone());
    }

    let warm_bars = store.recent_bars(&symbol, Timeframe::M1, WARM_START_M1_LIMIT)?;
    tracing::info!(
        %symbol,
        n = warm_bars.len(),
        "warm-start aggregator with sqlite history"
    );
    // Persist any higher-TF closes the aggregator emits while replaying 1m
    // history. Without this, a fresh DB only ever has 1m bars, so switching
    // to 5m/15m/30m/1h on the UI shows almost nothing.
    let mut warm_higher: Vec<Bar> = Vec::new();
    {
        let mut agg = agg_handle.lock().await;
        for bar in warm_bars.iter() {
            for higher in agg.warm_start_with_closed(bar) {
                if let BarEvent::BarClosed(b) = higher {
                    warm_higher.push(b);
                }
            }
        }
    }
    if !warm_higher.is_empty() {
        if let Err(e) = store.insert_bars(&warm_higher) {
            tracing::error!(error = ?e, %symbol, "warm-start higher-tf insert failed");
        }
        tracing::info!(%symbol, n = warm_higher.len(), "persisted higher-tf warm-start bars");
    }

    // Seed the engine with this symbol's SQLite history (all TFs) BEFORE
    // subscribing to the live TV feed. Symbols added via add_symbol (single-
    // mode symbol switch) or watchlist switch were not cold-start seeded
    // (cold start only covers the active watchlist), so without this the
    // engine has no higher-TF history and PDH/PDL/PO3 lack sufficient
    // context - the chart shows only 1m structures (or nothing at all).
    //
    // Three things the original warm-start seed was missing, all required
    // for indicators to appear on a freshly-switched symbol:
    //   1. `engine.seeding = true` so apply_and_broadcast suppresses the
    //      broadcast (capacity 64) instead of flooding it with thousands
    //      of historical structure events.
    //   2. `persist_engine_after_step` so the SQLite fallback path in
    //      list_structures (used while the engine lock is held) returns
    //      real structures instead of an empty set.
    //   3. A `seed_complete` emit so the front-end re-fetches
    //      list_structures immediately after the seed finishes.
    let needs_seed = {
        let eng = engine.lock();
        !eng.has_bar_history(&symbol)
    };
    if needs_seed {
        // Safety net: ensure detectors are registered. Cold start
        // bootstraps all_symbols, but a dynamically added symbol that
        // isn't in any watchlist would have no detectors - feeding bars
        // would silently produce zero structures.
        let needs_bootstrap = !engine.lock().has_symbol(&symbol);
        if needs_bootstrap {
            bootstrap_detectors(&engine, &[symbol.clone()]);
            for group in engine_groups.read().values() {
                load_and_apply_detector_config(&engine, &store, &group.candidates, &group.alerts);
            }
            engine.lock().po3_replay_done.insert(symbol.clone());
        }

        // Publish seeding=true before scheduling the blocking replay. There
        // is no await/cancellation point between this store and
        // spawn_blocking, and a launched blocking task runs to completion
        // even if its JoinHandle is dropped. Setting the flag only inside the
        // closure left a scheduling race where list_structures could read and
        // cache a partial, session-less engine snapshot.
        engine
            .lock()
            .seeding
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let engine = engine.clone();
        let store = store.clone();
        let symbol_seed = symbol.clone();
        tokio::task::spawn_blocking(move || {
            seed_symbol_history(&engine, &store, &[symbol_seed.clone()]);
            // Re-hydrate this symbol's structures from SQLite that
            // were lost during the truncated seed replay.
            for tf in [
                Timeframe::M1,
                Timeframe::M5,
                Timeframe::M15,
                Timeframe::M30,
                Timeframe::H1,
                Timeframe::H4,
                Timeframe::D1,
                Timeframe::W1,
            ] {
                if let Ok(rows) = store.list_active_structures(&symbol_seed, tf) {
                    let eng = engine.lock();
                    for r in rows {
                        let id = r.id().to_string();
                        if !eng.structures.contains_key(&id) {
                            eng.hydrate(r);
                        }
                    }
                }
            }
            persist_engine_after_step(&store, &engine, false);
            engine
                .lock()
                .seeding
                .store(false, std::sync::atomic::Ordering::Relaxed);
        })
        .await
        .ok();
        let _ = app.emit("seed_complete", ());
    }

    // Subscribe to TV.
    let _ = app.emit(
        "app:status",
        AppStatusPayload {
            kind: "ws_connecting".into(),
            message: format!("connecting to TradingView for {symbol}"),
        },
    );
    // PO3 cold-start replay runs once per symbol after its first TV Historical
    // batch. TV disconnects/reconnects every ~30 min and re-sends the
    // Historical batch; without this guard the replay would fire on every
    // reconnection, locking the engine for ~15 s each time and freezing the
    // chart (list_structures blocks on the engine Mutex).
    let mut po3_replayed = false;

    while let Some(evt) = rx.recv().await {
        match &evt {
            BarEvent::Historical(bars) => {
                // The union WebSocket can deliver seven initial snapshots in
                // quick succession. Replay them one symbol at a time: the
                // detector's `seeding` flag is global and concurrent history
                // replays could otherwise clear it while another symbol is
                // still rebuilding higher timeframes.
                let _history_guard = history_seed_lock.lock().await;
                let _ = app.emit(
                    "app:status",
                    AppStatusPayload {
                        kind: "connected".into(),
                        message: format!("TradingView connected: {symbol}"),
                    },
                );
                let finalized = finalized_history_bars(bars, now_ms());
                tracing::info!(
                    %symbol,
                    n = bars.len(),
                    finalized = finalized.len(),
                    "TV historical batch"
                );
                if let Err(e) = store.insert_bars(&finalized) {
                    tracing::error!(error = ?e, %symbol, "historical insert failed");
                }
                // A reconnect snapshot can contain minutes older than the
                // engine/aggregator tail. Those rows are intentionally not
                // replayed through the live state machine below, but complete
                // elapsed M5/M15/M30/H1 windows still need to be restored for
                // charts and subsequent cold-start replay.
                let repaired_aggregates =
                    match recent_fixed_aggregate_history_needs_fill(&store, &symbol, now_ms()) {
                        Ok(true) => match fill_missing_recent_aggregates(&store, &symbol) {
                            Ok(repaired) => {
                                if repaired > 0 {
                                    tracing::info!(
                                        %symbol,
                                        repaired,
                                        "filled missing aggregates after TV reconnect history"
                                    );
                                }
                                repaired
                            }
                            Err(error) => {
                                tracing::warn!(
                                    ?error,
                                    %symbol,
                                    "reconnect aggregate gap repair failed"
                                );
                                0
                            }
                        },
                        Ok(false) => 0,
                        Err(error) => {
                            tracing::warn!(
                                ?error,
                                %symbol,
                                "reconnect aggregate gap inspection failed"
                            );
                            0
                        }
                    };
                // Suppress broadcast during historical batch replay.
                engine
                    .lock()
                    .seeding
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                // Track whether we seeded any bars so we can skip
                // expensive persist + aggregation on reconnections
                // where no new data arrived.
                // Decide which bars to seed. On the first batch for a fresh
                // symbol (not yet seeded by the warm-start), clear buckets and
                // seed the full TV history. Otherwise (reconnection, or
                // warm-start already seeded), seed only bars newer than what
                // the engine already has - re-feeding older bars through the
                // already-warm-started aggregator would spuriously flush its
                // rolling state and emit garbage higher-TF closes.
                let bars_to_seed: Vec<&Bar>;
                {
                    let mut eng = engine.lock();
                    let already_seeded = eng.has_bar_history(&symbol);
                    if !po3_replayed && !already_seeded {
                        for tf in [
                            Timeframe::M1,
                            Timeframe::M5,
                            Timeframe::M15,
                            Timeframe::M30,
                            Timeframe::H1,
                            Timeframe::H4,
                            Timeframe::D1,
                            Timeframe::W1,
                        ] {
                            eng.clear_bucket_for_seeding(&symbol, tf);
                        }
                        tracing::info!(%symbol, "cleared all buckets for full TV history replay");
                        bars_to_seed = finalized.iter().collect();
                    } else {
                        let last_ts = eng.last_bar_ts(&symbol, Timeframe::M1).unwrap_or(0);
                        bars_to_seed = finalized.iter().filter(|b| b.ts > last_ts).collect();
                        if !bars_to_seed.is_empty() {
                            tracing::info!(
                                %symbol,
                                new = bars_to_seed.len(),
                                last_ts,
                                "seeding only new bars (warm-start seeded or reconnect)"
                            );
                        }
                    }
                    for b in &bars_to_seed {
                        eng.seed_closed_bar(b);
                    }
                }
                let seeded_any = !bars_to_seed.is_empty();
                if seeded_any {
                    for b in &bars_to_seed {
                        feed_smt_to_groups(&engine_groups, &engine, &store, &app, b, true);
                    }
                    persist_engine_after_step(&store, &engine, false);
                    // Build higher-TF closes from ONLY the bars we just
                    // seeded, so the (already warm-started) aggregator is
                    // not fed stale bars that would spuriously flush its
                    // rolling state.
                    let mut agg = agg_handle.lock().await;
                    let mut historical_higher = Vec::new();
                    for b in &bars_to_seed {
                        historical_higher.extend(agg.warm_start_with_closed(b));
                    }
                    drop(agg);
                    for higher in historical_higher {
                        if let BarEvent::BarClosed(bar) = higher {
                            if let Err(e) = store.insert_bar(&bar) {
                                tracing::error!(error = ?e, %symbol, tf = %bar.tf.tag(), "insert historical higher-tf closed failed");
                            }
                            {
                                let mut eng = engine.lock();
                                eng.seed_closed_bar(&bar);
                            }
                            feed_smt_to_groups(&engine_groups, &engine, &store, &app, &bar, true);
                            if bar.tf == Timeframe::M5 {
                                feed_ltf_to_groups(
                                    &engine_groups,
                                    &engine,
                                    &store,
                                    &bar,
                                    bar.ts,
                                    true,
                                );
                            }
                        }
                    }
                    persist_engine_after_step(&store, &engine, false);
                } else {
                    tracing::info!(%symbol, "no new bars, skipping persist+aggregate");
                }

                // The cold-start seed only replayed ~500 bars and skipped po3
                // (a truncated window spuriously invalidates PO3 whose lifecycle
                // extends past the edge). Now that the TV Historical batch has
                // filled this symbol's buckets with the complete history, replay
                // po3 over them — same invalidate + replay path the manual toggle
                // uses, but scoped to this symbol so the engine lock is held for
                // a shorter time. Guarded by `po3_replayed` so TV reconnections
                // (which re-send the Historical batch) don't re-lock the engine.
                if !po3_replayed {
                    {
                        let mut eng = engine.lock();
                        let has_po3 = eng.po3_count_for_symbol(&symbol) > 0;
                        if has_po3 {
                            // PO3 was hydrated from SQLite at cold start - keep it
                            // and just enable live detection going forward. The
                            // hydrated set was computed with full history in the
                            // previous session, so it is at least as complete as a
                            // replay over the bucket would produce.
                            eng.po3_replay_done.insert(symbol.to_string());
                            tracing::info!(
                                %symbol,
                                "po3 hydrated from SQLite, skipping replay"
                            );
                        } else {
                            let replayed = eng.replay_po3_for_symbol(&symbol);
                            tracing::info!(
                                %symbol, replayed,
                                "po3 full-history replay after TV backfill"
                            );
                        }
                    }
                    persist_engine_after_step(&store, &engine, false);
                    let _ = app.emit("po3_replay_done", ());
                    po3_replayed = true;
                }
                engine
                    .lock()
                    .seeding
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                if seeded_any {
                    // A reconnect is a historical catch-up, not a live signal.
                    // Candidate creation can trail its M5 validation bars when
                    // symbol snapshots arrive separately; reconcile those facts
                    // now instead of waiting for a full application restart.
                    for group in groups_for_symbol(&engine_groups, &symbol) {
                        let symbols = &group.watchlist.symbols;
                        replay_candidates(
                            &group.smt,
                            &group.candidates,
                            &group.alerts,
                            &store,
                            symbols,
                        );
                    }
                }
                if seeded_any || repaired_aggregates > 0 {
                    // Reconnected historical bars are persisted without live
                    // bar broadcasts; force every pane to reload the repaired
                    // bars and authoritative structure snapshot.
                    let _ = app.emit("seed_complete", ());
                }
                continue;
            }
            BarEvent::BarUpdate(bar) => {
                emit_bar(&bar_tx, bar, false);
                {
                    let mut eng = engine.lock();
                    eng.on_open_bar(bar);
                }
            }
            BarEvent::BarClosed(bar) => {
                if let Err(e) = store.insert_bar(bar) {
                    tracing::error!(error = ?e, %symbol, "insert closed 1m failed");
                }
                emit_bar(&bar_tx, bar, true);
                {
                    let mut eng = engine.lock();
                    eng.on_closed_bar(bar);
                }
                feed_smt_to_groups(&engine_groups, &engine, &store, &app, bar, false);
                // NOTE: persist_engine_after_step removed from per-bar path.
                // The emit-task already upserts New/Update/Invalidated to
                // SQLite in real-time. Re-upserting all 30k+ structures
                // after every bar froze the DB and the frontend.
            }
        }

        let started = Instant::now();
        let agg_events = {
            let mut agg = agg_handle.lock().await;
            agg.on_m1_event(&evt)
        };
        let elapsed_us = started.elapsed().as_micros();
        if elapsed_us > 1_000 {
            tracing::warn!(%symbol, elapsed_us, "aggregator step over 1ms");
        }

        // Persist + emit higher-tf events.
        for higher in agg_events {
            match higher {
                BarEvent::BarClosed(bar) => {
                    if let Err(e) = store.insert_bar(&bar) {
                        tracing::error!(error = ?e, %symbol, tf = %bar.tf.tag(), "insert higher-tf closed failed");
                    }
                    emit_bar(&bar_tx, &bar, true);
                    {
                        let mut eng = engine.lock();
                        eng.on_closed_bar(&bar);
                    }
                    feed_smt_to_groups(&engine_groups, &engine, &store, &app, &bar, false);
                    // M6a: LTF validation on 5m bar close.
                    if bar.tf == Timeframe::M5 {
                        let now = now_ms();
                        feed_ltf_to_groups(&engine_groups, &engine, &store, &bar, now, false);
                    }
                    // NOTE: persist_engine_after_step removed (see above).
                }
                BarEvent::BarUpdate(bar) => {
                    emit_bar(&bar_tx, &bar, false);
                    {
                        let mut eng = engine.lock();
                        eng.on_open_bar(&bar);
                    }
                }
                BarEvent::Historical(_) => {}
            }
        }
    }

    Ok(())
}
fn emit_bar(tx: &broadcast::Sender<BarPayload>, bar: &Bar, closed: bool) {
    let payload = BarPayload::from_bar(bar, closed);
    let _ = tx.send(payload);
}

fn is_dxy(symbol: &str) -> bool {
    symbol.split_once(':').map(|(_, t)| t).unwrap_or(symbol) == "DXY"
}

fn groups_for_symbol(groups: &EngineGroups, symbol: &str) -> Vec<Arc<EngineGroup>> {
    let mut selected: Vec<_> = groups
        .read()
        .values()
        .filter(|group| group.watchlist.symbols.iter().any(|item| item == symbol))
        .cloned()
        .collect();
    // DXY may produce simultaneous transitions in all groups. A stable route
    // order makes the shared global cooldown deterministic across restarts.
    selected.sort_by(|a, b| a.watchlist.id.cmp(&b.watchlist.id));
    selected
}

fn monitored_symbol_union(watchlists: &[Watchlist]) -> Vec<String> {
    let mut union = Vec::new();
    for watchlist in watchlists {
        for symbol in &watchlist.symbols {
            if !union.contains(symbol) {
                union.push(symbol.clone());
            }
        }
    }
    union
}

#[cfg(test)]
mod m6c_routing_tests {
    use super::*;

    #[test]
    fn reconnect_preflight_gate_coalesces_snapshots_after_success() {
        let gate = ReconnectHistoryPreflightGate::default();
        let started_at = 100_000;
        assert!(gate.try_begin(started_at));
        assert!(!gate.try_begin(started_at + 1));

        gate.finish(started_at + 10, true);
        assert!(!gate.try_begin(started_at + 10 + RECONNECT_PREFLIGHT_SUCCESS_COOLDOWN_MS - 1));
        assert!(gate.try_begin(started_at + 10 + RECONNECT_PREFLIGHT_SUCCESS_COOLDOWN_MS));
    }

    #[test]
    fn reconnect_preflight_gate_allows_bounded_failure_retry() {
        let gate = ReconnectHistoryPreflightGate::default();
        let started_at = 200_000;
        assert!(gate.try_begin(started_at));
        gate.finish(started_at + 10, false);

        assert!(!gate.try_begin(started_at + RECONNECT_PREFLIGHT_RETRY_COOLDOWN_MS - 1));
        assert!(gate.try_begin(started_at + RECONNECT_PREFLIGHT_RETRY_COOLDOWN_MS));
    }

    fn temp_db_path(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("ict-monitor-{name}-{unique}.db"))
    }

    fn hourly_bar(symbol: &str, ts: i64, index: usize) -> Bar {
        let base = 1.0 + index as f64 * 0.0001;
        Bar {
            symbol: symbol.to_string(),
            tf: Timeframe::H1,
            ts,
            open: base,
            high: base + 0.0002,
            low: base - 0.0002,
            close: base + 0.0001,
            volume: 1.0,
        }
    }

    fn minute_bar(symbol: &str, ts: i64, index: usize) -> Bar {
        let base = 1.0 + index as f64 * 0.000001;
        Bar {
            symbol: symbol.to_string(),
            tf: Timeframe::M1,
            ts,
            open: base,
            high: base + 0.000002,
            low: base - 0.000002,
            close: base + 0.000001,
            volume: 1.0,
        }
    }

    fn bar_at(symbol: &str, tf: Timeframe, ts: i64, index: usize) -> Bar {
        let base = 1.0 + index as f64 * 0.0001;
        Bar {
            symbol: symbol.to_string(),
            tf,
            ts,
            open: base,
            high: base + 0.0002,
            low: base - 0.0002,
            close: base + 0.0001,
            volume: 1.0 + index as f64,
        }
    }

    #[test]
    fn subscription_union_contains_one_dxy_and_every_group_symbol() {
        let groups = ict_monitor::watchlist::default_watchlists();
        let union = monitored_symbol_union(&groups);
        assert_eq!(
            union.iter().filter(|symbol| *symbol == "TVC:DXY").count(),
            1
        );
        assert_eq!(union.len(), 7);
        for expected in [
            "OANDA:EURUSD",
            "OANDA:GBPUSD",
            "OANDA:AUDUSD",
            "OANDA:NZDUSD",
            "OANDA:USDCHF",
            "OANDA:USDCAD",
            "TVC:DXY",
        ] {
            assert!(union.iter().any(|symbol| symbol == expected));
        }
    }

    #[test]
    fn a_pair_specific_outage_does_not_change_other_groups_membership() {
        let groups = ict_monitor::watchlist::default_watchlists();
        let routed = |symbol: &str| {
            groups
                .iter()
                .filter(|group| group.symbols.iter().any(|item| item == symbol))
                .map(|group| group.id.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(routed("OANDA:EURUSD"), vec!["eu-gu-dxy"]);
        assert_eq!(routed("OANDA:USDCHF"), vec!["chf-cad-dxy"]);
        assert_eq!(routed("TVC:DXY").len(), 3);
    }

    #[test]
    fn source_history_with_enough_rows_but_a_multiweek_hole_still_needs_backfill() {
        let symbol = "OANDA:USDCHF";
        let start = 1_783_296_000_000_i64;
        let mut bars: Vec<Bar> = (0..750)
            .map(|index| hourly_bar(symbol, start + index as i64 * 3_600_000, index))
            .collect();
        let resumed = start + 60 * 24 * 3_600_000;
        bars.extend(
            (0..750)
                .map(|index| hourly_bar(symbol, resumed + index as i64 * 3_600_000, index + 750)),
        );

        assert_eq!(bars.len(), SMT_SOURCE_HISTORY_LIMIT as usize);
        assert!(history_needs_backfill(
            &bars,
            SMT_SOURCE_MIN_BARS,
            bars.last().expect("last bar").ts + 3_600_000,
        ));
    }

    #[test]
    fn source_history_with_one_missing_market_bar_needs_backfill() {
        let symbol = "OANDA:NZDUSD";
        // 2026-08-28 02:30 and 03:30 Asia/Shanghai. The missing 03:00
        // candle is inside the open forex session and must be repaired.
        let first_ts = 1_787_855_400_000_i64;
        let bars = vec![
            Bar {
                symbol: symbol.into(),
                tf: Timeframe::M30,
                ts: first_ts,
                open: 0.595,
                high: 0.596,
                low: 0.594,
                close: 0.595,
                volume: 1.0,
            },
            Bar {
                symbol: symbol.into(),
                tf: Timeframe::M30,
                ts: first_ts + 2 * Timeframe::M30.duration_ms(),
                open: 0.595,
                high: 0.596,
                low: 0.594,
                close: 0.595,
                volume: 1.0,
            },
        ];

        assert!(bars_have_unexpected_intraday_gap(&bars));
    }

    #[test]
    fn chart_refresh_repairs_an_internal_gap_when_the_tail_is_current() {
        let symbol = "TVC:DXY";
        let duration = Timeframe::H1.duration_ms();
        let bars = vec![
            hourly_bar(symbol, 0, 0),
            // The 01:00 candle is absent, but the most recent closed candle
            // is present. Cached history still paints first; the authoritative
            // background path must now repair this internal hole.
            hourly_bar(symbol, 2 * duration, 2),
        ];

        assert!(bars_have_unexpected_intraday_gap(&bars));
        assert!(chart_history_needs_provider_refresh(&bars, 2, 3 * duration,));
    }

    #[test]
    fn chart_refresh_detects_a_missing_calendar_window() {
        let tf = Timeframe::D1;
        let first = tf.boundary_align(1_780_000_000_000);
        let missing = next_window_boundary(tf, first);
        let third = next_window_boundary(tf, missing);
        let bars = vec![
            bar_at("COINBASE:BTCUSD", tf, first, 0),
            bar_at("COINBASE:BTCUSD", tf, third, 2),
        ];

        assert!(bars_have_unexpected_market_gap(&bars));
        assert!(chart_history_needs_provider_refresh(
            &bars,
            bars.len(),
            next_window_boundary(tf, third),
        ));
    }

    #[test]
    fn calendar_provider_rows_are_canonicalized_and_deduplicated() {
        let tf = Timeframe::D1;
        let window = tf.boundary_align(1_780_000_000_000);
        let first = bar_at("TVC:DXY", tf, window + 60 * 60_000, 0);
        let second = bar_at("TVC:DXY", tf, window + 2 * 60 * 60_000, 1);

        let normalized = canonicalize_provider_calendar_bars(vec![first, second.clone()]);

        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized[0].ts, window);
        assert_eq!(normalized[0].close, second.close);
        assert_eq!(normalized[0].high, second.high);
        assert_eq!(normalized[0].volume, second.volume);
    }

    #[test]
    fn chart_refresh_detects_a_missing_market_open_tail() {
        let symbol = "TVC:DXY";
        let duration = Timeframe::H1.duration_ms();
        let bars = vec![hourly_bar(symbol, 0, 0)];

        assert!(chart_history_needs_provider_refresh(&bars, 1, 2 * duration,));
    }

    #[test]
    fn chart_refresh_does_not_fetch_during_a_normal_weekend_close() {
        let friday_last_bar = Bar {
            symbol: "OANDA:EURUSD".into(),
            tf: Timeframe::M30,
            ts: 1_787_949_000_000,
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
        };

        let saturday = friday_last_bar.ts + 24 * 60 * 60 * 1_000;
        assert!(!chart_history_needs_provider_refresh(
            &[friday_last_bar],
            1,
            saturday,
        ));
    }

    #[test]
    fn normal_forex_weekend_does_not_need_gap_backfill() {
        let symbol = "OANDA:NZDUSD";
        // Last Friday candle before the 17:00 New York close, followed by
        // the first Sunday candle at the 17:00 New York reopen.
        let bars = vec![
            Bar {
                symbol: symbol.into(),
                tf: Timeframe::M30,
                ts: 1_787_949_000_000,
                open: 0.595,
                high: 0.596,
                low: 0.594,
                close: 0.595,
                volume: 1.0,
            },
            Bar {
                symbol: symbol.into(),
                tf: Timeframe::M30,
                ts: 1_788_123_600_000,
                open: 0.595,
                high: 0.596,
                low: 0.594,
                close: 0.595,
                volume: 1.0,
            },
        ];

        assert!(!bars_have_unexpected_intraday_gap(&bars));
    }

    #[test]
    fn aggregate_repair_never_deletes_native_higher_tf_inside_an_m1_gap() {
        let path = temp_db_path("aggregate-gap");
        let store = SqliteStore::open(&path).expect("open test db");
        let symbol = "OANDA:USDCHF";
        let start = 1_783_296_000_000_i64;
        let second_start = start + 10 * 24 * 60 * 60_000;
        let mut m1: Vec<Bar> = (0..180)
            .map(|index| minute_bar(symbol, start + index as i64 * 60_000, index))
            .collect();
        m1.extend(
            (0..180)
                .map(|index| minute_bar(symbol, second_start + index as i64 * 60_000, index + 180)),
        );
        store.insert_bars(&m1).expect("insert split M1 history");

        let native_gap_ts = start + 5 * 24 * 60 * 60_000;
        let stale_repairable_ts = Timeframe::H1.boundary_align(second_start);
        store
            .insert_bars(&[
                Bar {
                    symbol: symbol.into(),
                    tf: Timeframe::H1,
                    ts: native_gap_ts,
                    open: 7.0,
                    high: 7.1,
                    low: 6.9,
                    close: 7.0,
                    volume: 1.0,
                },
                Bar {
                    symbol: symbol.into(),
                    tf: Timeframe::H1,
                    ts: stale_repairable_ts,
                    open: 9.0,
                    high: 9.0,
                    low: 9.0,
                    close: 9.0,
                    volume: 1.0,
                },
            ])
            .expect("insert native/stale H1 rows");

        repair_recent_aggregates(&store, symbol).expect("repair aggregates");
        let h1 = store
            .recent_bars(symbol, Timeframe::H1, 100)
            .expect("query repaired H1");
        let native = h1
            .iter()
            .find(|bar| bar.ts == native_gap_ts)
            .expect("native H1 inside M1 hole must survive");
        assert_eq!(native.close, 7.0);
        let repaired = h1
            .iter()
            .find(|bar| bar.ts == stale_repairable_ts)
            .expect("covered H1 should be rebuilt");
        assert_ne!(repaired.close, 9.0);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reconnect_repair_fills_missing_0900_m30_without_overwriting_native_rows() {
        let path = temp_db_path("reconnect-m30-gap");
        let store = SqliteStore::open(&path).expect("open test db");
        let symbol = "OANDA:GBPUSD";
        // 2026-09-01 08:30 Asia/Shanghai. Sixty complete M1 bars cover the
        // closed 08:30 and 09:00 M30 windows seen in the reported outage.
        let start = 1_788_222_600_000_i64;
        let m1: Vec<Bar> = (0..60)
            .map(|index| minute_bar(symbol, start + index as i64 * 60_000, index))
            .collect();
        store.insert_bars(&m1).expect("insert complete M1 source");

        let native_0830 = Bar {
            symbol: symbol.into(),
            tf: Timeframe::M30,
            ts: start,
            open: 7.0,
            high: 7.1,
            low: 6.9,
            close: 7.0,
            volume: 1.0,
        };
        store
            .insert_bar(&native_0830)
            .expect("insert native 08:30 row");

        assert!(fixed_aggregate_rows_need_fill(
            std::slice::from_ref(&native_0830),
            symbol,
            Timeframe::M30,
            start + 64 * 60_000,
        ));

        let inserted =
            fill_missing_recent_aggregates(&store, symbol).expect("fill elapsed aggregate holes");
        assert!(inserted >= 1);

        let m30 = store
            .recent_bars(symbol, Timeframe::M30, 10)
            .expect("query repaired M30");
        let preserved = m30.iter().find(|bar| bar.ts == start).expect("08:30 row");
        assert_eq!(preserved.close, 7.0, "native 08:30 must not be overwritten");
        let repaired = m30
            .iter()
            .find(|bar| bar.ts == start + Timeframe::M30.duration_ms())
            .expect("missing 09:00 row must be restored");
        assert_ne!(repaired.close, 7.0);
        assert!(!fixed_aggregate_rows_need_fill(
            &m30,
            symbol,
            Timeframe::M30,
            start + 64 * 60_000,
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn partial_engine_snapshot_merges_persisted_sessions_without_overwriting_live_rows() {
        use ict_monitor::detector::types::{IctStructure, SessionKind, SessionRange};

        let path = temp_db_path("session-snapshot-merge");
        let store = SqliteStore::open(&path).expect("open test db");
        store.ensure_ict_schema().expect("ict schema");
        let persisted = IctStructure::SessionRange(SessionRange {
            id: "dxy-asia".into(),
            symbol: "TVC:DXY".into(),
            tf: Timeframe::M1,
            session: SessionKind::Asia,
            label: "Asia".into(),
            ts_start: 1_000,
            ts_end: 2_000,
            high: 100.0,
            low: 99.0,
            high_ts: 1_200,
            low_ts: 1_400,
            finalized: false,
        });
        store
            .upsert_structure(&persisted, 2_000)
            .expect("persist session");

        let mut empty_snapshot = Vec::new();
        merge_persisted_session_structures(&store, "TVC:DXY", &mut empty_snapshot);
        assert_eq!(empty_snapshot.len(), 1);

        let mut live = match persisted.clone() {
            IctStructure::SessionRange(range) => range,
            _ => unreachable!(),
        };
        live.high = 101.0;
        let mut live_snapshot = vec![IctStructure::SessionRange(live)];
        merge_persisted_session_structures(&store, "TVC:DXY", &mut live_snapshot);
        assert_eq!(live_snapshot.len(), 1, "same id must not be duplicated");
        assert!(matches!(
            &live_snapshot[0],
            IctStructure::SessionRange(range) if range.high == 101.0
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn startup_h4_preflight_replaces_sparse_covered_slice_and_preserves_deep_history() {
        let path = temp_db_path("h4-preflight");
        let store = SqliteStore::open(&path).expect("open test db");
        let symbol = "OANDA:USDCHF";
        // Monday 2026-07-06 00:00 UTC. Supplying all hours is intentional:
        // the canonical aggregator itself rejects non-market weekend groups,
        // leaving well over the 64-bar audit floor.
        let start = 1_783_296_000_000_i64;
        let h1: Vec<Bar> = (0..480)
            .map(|index| hourly_bar(symbol, start + index as i64 * 60 * 60_000, index))
            .collect();
        store.insert_bars(&h1).expect("insert H1 history");

        let deep_ts = Timeframe::H4.boundary_align(start) - 10 * 24 * 60 * 60_000;
        let sparse_ts = start + 1_234;
        store
            .insert_bars(&[
                Bar {
                    symbol: symbol.into(),
                    tf: Timeframe::H4,
                    ts: deep_ts,
                    open: 0.8,
                    high: 0.9,
                    low: 0.7,
                    close: 0.85,
                    volume: 1.0,
                },
                Bar {
                    symbol: symbol.into(),
                    tf: Timeframe::H4,
                    ts: sparse_ts,
                    open: 9.0,
                    high: 9.0,
                    low: 9.0,
                    close: 9.0,
                    volume: 1.0,
                },
            ])
            .expect("insert existing H4 rows");

        let rebuilt = canonicalize_forex_h4_from_h1(&store, symbol, start + 600 * 60 * 60_000)
            .expect("canonicalize H4")
            .expect("sufficient H1 coverage");
        assert!(rebuilt >= H4_REPLAY_MIN_BARS);

        let h4 = store
            .recent_bars(symbol, Timeframe::H4, 1_000)
            .expect("query canonical H4");
        assert!(h4.iter().any(|bar| bar.ts == deep_ts));
        assert!(!h4.iter().any(|bar| bar.ts == sparse_ts));
        assert!(
            h4_replay_coverage_issue(&store, &[symbol.to_string()], start + 600 * 60 * 60_000,)
                .is_none()
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn startup_h4_preflight_never_wipes_history_without_enough_h1_evidence() {
        let path = temp_db_path("h4-preflight-short");
        let store = SqliteStore::open(&path).expect("open test db");
        let symbol = "OANDA:USDCAD";
        let start = 1_783_296_000_000_i64;
        let h1: Vec<Bar> = (0..16)
            .map(|index| hourly_bar(symbol, start + index as i64 * 60 * 60_000, index))
            .collect();
        store.insert_bars(&h1).expect("insert short H1 history");
        store
            .insert_bar(&Bar {
                symbol: symbol.into(),
                tf: Timeframe::H4,
                ts: Timeframe::H4.boundary_align(start),
                open: 1.0,
                high: 1.1,
                low: 0.9,
                close: 1.05,
                volume: 1.0,
            })
            .expect("insert retained H4");

        assert_eq!(
            canonicalize_forex_h4_from_h1(&store, symbol, start + 24 * 60 * 60_000)
                .expect("preflight result"),
            None
        );
        assert_eq!(store.count(symbol, Timeframe::H4.tag()).unwrap(), 1);
        assert!(
            h4_replay_coverage_issue(&store, &[symbol.to_string()], start + 24 * 60 * 60_000,)
                .is_some()
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn startup_h4_preflight_preserves_a_valid_native_fallback_for_an_h1_hole() {
        let path = temp_db_path("h4-native-fallback");
        let store = SqliteStore::open(&path).expect("open test db");
        let symbol = "TVC:DXY";
        let start = 1_783_296_000_000_i64;
        let target_window = Timeframe::H4.boundary_align(start + 72 * 60 * 60_000);
        let mut h1: Vec<Bar> = (0..480)
            .map(|index| hourly_bar(symbol, start + index as i64 * 60 * 60_000, index))
            .collect();
        let target_children: Vec<Bar> = h1
            .iter()
            .filter(|bar| Timeframe::H4.boundary_align(bar.ts) == target_window)
            .cloned()
            .collect();
        assert_eq!(target_children.len(), 4);
        let missing_a = target_children[1].ts;
        let missing_b = target_children[2].ts;
        h1.retain(|bar| bar.ts != missing_a && bar.ts != missing_b);
        store.insert_bars(&h1).expect("insert H1 history with hole");

        let native = Bar {
            symbol: symbol.into(),
            tf: Timeframe::H4,
            ts: target_window,
            open: target_children[0].open,
            high: target_children
                .iter()
                .map(|bar| bar.high)
                .fold(f64::NEG_INFINITY, f64::max),
            low: target_children
                .iter()
                .map(|bar| bar.low)
                .fold(f64::INFINITY, f64::min),
            close: target_children[3].close,
            volume: 4.0,
        };
        store.insert_bar(&native).expect("stage native H4 fallback");

        let rebuilt = canonicalize_forex_h4_from_h1(&store, symbol, start + 600 * 60 * 60_000)
            .expect("canonicalize H4")
            .expect("sufficient H1 coverage");
        assert!(rebuilt >= H4_REPLAY_MIN_BARS);
        let h4 = store
            .recent_bars(symbol, Timeframe::H4, 1_000)
            .expect("query canonical H4");
        let recovered = h4
            .iter()
            .find(|bar| bar.ts == target_window)
            .expect("native fallback must survive canonical replacement");
        assert_eq!(recovered.open, native.open);
        assert_eq!(recovered.high, native.high);
        assert_eq!(recovered.low, native.low);
        assert_eq!(recovered.close, native.close);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn chart_h4_reaggregation_preserves_deeper_native_history() {
        let path = temp_db_path("h4-chart-bounded-replace");
        let store = SqliteStore::open(&path).expect("open test db");
        let symbol = "TVC:DXY";
        let start = Timeframe::H4.boundary_align(1_783_296_000_000_i64);
        let deeper_ts = start - 20 * 4 * 60 * 60_000;
        let dirty_ts = start + 60 * 60_000;

        store
            .insert_bars(&[
                Bar {
                    symbol: symbol.into(),
                    tf: Timeframe::H4,
                    ts: deeper_ts,
                    open: 98.0,
                    high: 98.5,
                    low: 97.5,
                    close: 98.2,
                    volume: 1.0,
                },
                Bar {
                    symbol: symbol.into(),
                    tf: Timeframe::H4,
                    ts: dirty_ts,
                    open: 99.0,
                    high: 99.5,
                    low: 98.5,
                    close: 99.2,
                    volume: 1.0,
                },
            ])
            .expect("insert native and dirty H4");
        assert_eq!(
            persist_canonical_h4_slice(&store, symbol, &[])
                .expect("empty canonical slice is a no-op"),
            0
        );
        assert_eq!(store.count(symbol, Timeframe::H4.tag()).unwrap(), 2);

        let canonical = vec![
            Bar {
                symbol: symbol.into(),
                tf: Timeframe::H4,
                ts: start,
                open: 100.0,
                high: 101.0,
                low: 99.0,
                close: 100.5,
                volume: 4.0,
            },
            Bar {
                symbol: symbol.into(),
                tf: Timeframe::H4,
                ts: start + 4 * 60 * 60_000,
                open: 100.5,
                high: 101.5,
                low: 100.0,
                close: 101.0,
                volume: 4.0,
            },
        ];

        assert_eq!(
            persist_canonical_h4_slice(&store, symbol, &canonical)
                .expect("persist canonical H4 slice"),
            canonical.len()
        );
        let stored = store
            .recent_bars(symbol, Timeframe::H4, 20)
            .expect("query H4");
        assert!(stored.iter().any(|bar| bar.ts == deeper_ts));
        assert!(!stored.iter().any(|bar| bar.ts == dirty_ts));
        assert!(canonical
            .iter()
            .all(|expected| stored.iter().any(|bar| bar.ts == expected.ts)));

        let _ = std::fs::remove_file(path);
    }
}

fn feed_smt_to_groups(
    groups: &EngineGroups,
    engine: &EngineHandle,
    store: &SqliteStore,
    app: &tauri::AppHandle,
    bar: &Bar,
    historical: bool,
) {
    for group in groups_for_symbol(groups, &bar.symbol) {
        feed_smt(
            &group.smt,
            &group.candidates,
            &group.alerts,
            group.llm_decisions.as_ref(),
            engine,
            store,
            app,
            bar,
            None,
            historical,
        );
    }
}

fn feed_ltf_to_groups(
    groups: &EngineGroups,
    engine: &EngineHandle,
    store: &SqliteStore,
    bar: &Bar,
    transition_ts: i64,
    seeding: bool,
) {
    let ltf_structures = engine.lock().list_active(&bar.symbol, Timeframe::M5);
    let ltf_events = extract_ltf_events(&ltf_structures);
    for group in groups_for_symbol(groups, &bar.symbol) {
        let changes =
            group
                .candidates
                .lock()
                .on_ltf_bar_close(&bar.symbol, bar, &ltf_events, transition_ts);
        for change in changes {
            if let Err(error) = store.upsert_candidate(&change.candidate, transition_ts) {
                tracing::warn!(?error, watchlist_id = %group.watchlist.id, "upsert candidate (ltf) failed");
            }
            if let Err(error) = store.insert_decision(&change.decision) {
                tracing::warn!(?error, watchlist_id = %group.watchlist.id, "insert decision (ltf) failed");
            }
            if seeding {
                if let Some(validation) = change.validation_event.as_ref() {
                    let already_fired = store
                        .has_fired_for_candidate_group_symbol(
                            &change.candidate.id,
                            &change.candidate.watchlist_id,
                            &validation.symbol,
                        )
                        .unwrap_or(false);
                    if !already_fired {
                        let alert = ict_monitor::alert::types::build_alert_for_validation(
                            &change.candidate,
                            Some(validation),
                            validation.ts,
                            vec![ict_monitor::alert::ChannelKind::Inbox],
                        );
                        if let Err(error) = store.insert_alert(&alert) {
                            tracing::warn!(?error, watchlist_id = %group.watchlist.id, "historical validation alert backfill failed");
                        }
                    }
                }
            }
            group
                .alerts
                .lock()
                .on_candidate_change(&change, transition_ts, seeding);
        }
    }
}

/// Feed a closed bar to the SmtEngine, persist any resulting divergences,
/// fill HTF PDA context, and broadcast SMT events.
fn feed_smt(
    smt: &SmtHandle,
    candidates: &CandidateHandle,
    alerts: &AlertHandle,
    llm_decisions: Option<&LlmDecisionHandle>,
    engine: &EngineHandle,
    store: &SqliteStore,
    app: &tauri::AppHandle,
    bar: &Bar,
    replay_cache: Option<&StructuresCache>,
    historical: bool,
) {
    let (mut events, ready_htf_closes, ready_forming_windows) = {
        let mut smt_guard = smt.lock();
        let events = smt_guard.on_closed_bar(bar);
        smt_guard.queue_pda_htf_close(bar);
        let ready = smt_guard.take_ready_pda_htf_closes();
        // Historical replay must only persist the final parent result. Live
        // delivery additionally evaluates synchronized partial HTF windows so
        // C2 can create a timely Candidate before the parent candle closes.
        let forming = if !historical {
            smt_guard.take_ready_forming_htf_windows(bar)
        } else {
            Vec::new()
        };
        (events, ready, forming)
    };
    for (forming_htf, available_end) in ready_forming_windows {
        let structures = engine
            .lock()
            .list_active(&forming_htf.symbol, forming_htf.tf);
        events.extend(
            smt.lock()
                .detect_pda_htf_forming(&forming_htf, available_end, &structures),
        );
    }
    for closed_htf in ready_htf_closes {
        let structures = match replay_cache {
            Some(cache) => cache
                .get(&(closed_htf.symbol.clone(), closed_htf.tf))
                .cloned()
                .unwrap_or_default(),
            None => engine.lock().list_active(&closed_htf.symbol, closed_htf.tf),
        };
        events.extend(smt.lock().detect_pda_htf_close(&closed_htf, &structures));
    }
    if bar.tf == Timeframe::H4 && is_dxy(&bar.symbol) {
        tracing::info!(
            ts = bar.ts,
            events = events.len(),
            "feed_smt H4 DXY bar processed"
        );
    }
    if events.is_empty() {
        return;
    }
    let now = now_ms();
    for raw_event in events {
        let canonical = ict_monitor::detector::smt::canonicalize_smt_event(raw_event, bar.ts);
        let ev = canonical.event;
        let invalidation_snapshot = canonical.invalidation_snapshot;
        if let Some((snapshot, market_ts)) = &invalidation_snapshot {
            tracing::info!(
                id = %snapshot.id,
                reasons = ?snapshot.invalidation_reasons,
                "invalidating SMT"
            );
            // Retain the failed attempt in memory as audit evidence; later
            // HTF bars may continue only with a stricter adverse extreme.
            smt.lock().update_divergence(snapshot.clone());
            let _ = store.mark_smt_invalidated_with_snapshot(
                snapshot,
                *market_ts,
                &snapshot.invalidation_reasons,
            );
            if let Err(e) = store.mark_candidates_smt_invalidated(&snapshot.id, *market_ts) {
                tracing::warn!(error = ?e, smt_id = %snapshot.id, "invalidate persisted SMT candidates failed");
            }
        }
        if let StructureEvent::New(s) | StructureEvent::Update(s) = &ev {
            if let IctStructure::SmtDivergence(d) = s {
                if d.context_timeframe == Timeframe::H4 {
                    tracing::info!(
                        id = %d.id,
                        smt_k_ts = d.chains.iter()
                            .find(|c| c.symbol == d.sweeper_symbol)
                            .map(|c| c.smt_k_candle.ts)
                            .unwrap_or(0),
                        has_pda = d.htf_pda_ref.is_some(),
                        pda_kind = d.htf_pda_ref.as_ref().map(|p| p.kind.as_str()).unwrap_or("none"),
                        "DEBUG upsert_smt H4"
                    );
                }
                if let Err(e) = store.upsert_smt(d, now) {
                    tracing::warn!(error = ?e, "upsert smt failed");
                }
            }
        }
        // Keep the canonical snapshot for the per-symbol C2 alert reconciler.
        // A later trade-leg C2 may update the SMT without producing a second
        // aggregate CandidateChange, so reconciliation cannot depend only on
        // `cand_changes`.
        let alert_source_smt = match &ev {
            StructureEvent::New(IctStructure::SmtDivergence(divergence))
            | StructureEvent::Update(IctStructure::SmtDivergence(divergence)) => {
                Some(divergence.clone())
            }
            _ => None,
        };

        // M6a: feed SMT event to candidate engine.
        let cand_changes = if let Some((snapshot, market_ts)) = &invalidation_snapshot {
            candidates
                .lock()
                .on_smt_invalidated_at(snapshot, *market_ts)
        } else {
            candidates.lock().on_smt_event(&ev, now)
        };
        for change in cand_changes {
            let transition_ts = invalidation_snapshot
                .as_ref()
                .map(|(_, market_ts)| *market_ts)
                .unwrap_or(now);
            if let Err(e) = store.upsert_candidate(&change.candidate, transition_ts) {
                tracing::warn!(error = ?e, "upsert candidate failed");
            }
            if let Err(e) = store.insert_decision(&change.decision) {
                tracing::warn!(error = ?e, "insert decision failed");
            }
            // M6b: alert hook.
            let _ = alerts.lock().on_candidate_change(&change, now, historical);
        }
        if let Some(source_smt) = alert_source_smt {
            let candidate = candidates.lock().get_by_smt_id(&source_smt.id);
            if let Some(candidate) = candidate {
                let seeding = historical;
                let created = alerts
                    .lock()
                    .on_c2_confirmed(&candidate, &source_smt, seeding);
                if !seeding {
                    if let Some(pipeline) = llm_decisions {
                        for alert in created {
                            let handle = pipeline.clone().spawn(
                                candidate.clone(),
                                alert,
                                source_smt.clone(),
                                false,
                            );
                            drop(handle);
                        }
                    }
                }
            }
        }
        let _ = app.emit(ev.topic(), ev);
    }
}

/// Hydrate SMT divergences from SQLite for the given watchlist. Called
/// after `set_watchlist` (which clears divergences) and before
/// `replay_smt_history` so the replay can update/add on top.
fn hydrate_smt(smt: &SmtHandle, store: &SqliteStore, watchlist_id: &str, symbols: &[String]) {
    // Failed C2/C3 attempts are hydrated too: they remain invisible to the
    // active chart set, but are required to continue a released liquidity
    // episode after restart.
    match store.list_all_smt(watchlist_id) {
        Ok(mut divs) => {
            // Build a map of FVG id -> ts_filled from all stored FVG
            // structures. This lets hydrate_divergences validate stale
            // PDA refs from old DB records that predate the ts_filled
            // field on PdaRef.
            let mut fvg_ts_filled: std::collections::HashMap<String, Option<i64>> =
                std::collections::HashMap::new();
            for sym in symbols {
                for tf in [IctTimeframe::H4, IctTimeframe::H1] {
                    if let Ok(structs) = store.list_active_structures(sym, tf) {
                        for s in &structs {
                            if let IctStructure::Fvg(f) = s {
                                fvg_ts_filled.insert(f.id.clone(), f.ts_filled);
                            }
                        }
                    }
                }
            }
            // Older cold-start code could clear a valid SMT PDA and then
            // persist the null back to smt_divergences. Candidate snapshots
            // are immutable audit evidence of the original spawn gate, so
            // use their (smt_id, context_pda_id) pair to repair only those
            // missing refs that still resolve to a real stored FVG. The SMT
            // engine's hydrate_divergences pass below still rejects a FVG
            // that was already stale when the SMT formed. Duplicate historical
            // refs are preserved as facts; the runtime consumed set alone
            // blocks future PDA reuse.
            let candidate_pdas: std::collections::HashMap<String, String> = store
                .list_all_candidates(watchlist_id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|candidate| {
                    candidate
                        .context_pda_id
                        .map(|pda_id| (candidate.smt_id, pda_id))
                })
                .collect();
            let fvg_refs: std::collections::HashMap<String, PdaRef> = symbols
                .iter()
                .flat_map(|sym| {
                    [IctTimeframe::H4, IctTimeframe::H1]
                        .into_iter()
                        .flat_map(move |tf| {
                            store.list_active_structures(sym, tf).unwrap_or_default()
                        })
                })
                .filter_map(|structure| match structure {
                    IctStructure::Fvg(fvg) => Some((
                        fvg.id.clone(),
                        PdaRef {
                            kind: "fvg".into(),
                            id: fvg.id,
                            tf: fvg.tf,
                            direction: fvg.direction,
                            price_low: fvg.price_low,
                            price_high: fvg.price_high,
                            ts_open: fvg.ts_open,
                            ts_confirm: fvg.ts_confirm,
                            exit_ts: None,
                            ts_filled: fvg.ts_filled,
                        },
                    )),
                    _ => None,
                })
                .collect();
            let mut repaired_ids = std::collections::HashSet::new();
            for divergence in &mut divs {
                if divergence.htf_pda_ref.is_some() {
                    continue;
                }
                let Some(pda_id) = candidate_pdas.get(&divergence.id) else {
                    continue;
                };
                let Some(pda) = fvg_refs.get(pda_id) else {
                    continue;
                };
                divergence.htf_pda_ref = Some(pda.clone());
                repaired_ids.insert(divergence.id.clone());
            }
            let repaired = repaired_ids.len();
            if repaired > 0 {
                tracing::info!(
                    repaired,
                    "restored SMT PDA refs from candidate audit evidence"
                );
            }
            smt.lock().hydrate_divergences(divs, &fvg_ts_filled);
            // Make the audit repair durable. Previously the restored ref only
            // lived in memory and the next restart had to infer it again from
            // Candidate; worse, a later replay could persist the null snapshot
            // back over it. Persist only the IDs repaired above, after hydrate
            // has rejected any PDA that was already filled at formation time.
            if repaired > 0 {
                let repaired_divergences: Vec<SmtDivergence> = smt
                    .lock()
                    .list_active()
                    .into_iter()
                    .filter(|d| repaired_ids.contains(&d.id) && d.htf_pda_ref.is_some())
                    .collect();
                let now = now_ms();
                for divergence in repaired_divergences {
                    if let Err(e) = store.upsert_smt(&divergence, now) {
                        tracing::warn!(
                            error = ?e,
                            smt_id = %divergence.id,
                            "persist restored historical SMT PDA failed"
                        );
                    }
                }
            }
        }
        Err(e) => tracing::warn!(error = ?e, %watchlist_id, "hydrate_smt failed"),
    }
}

/// Replay recent SQLite history through the SMT engine so divergences are
/// available on cold start and after a watchlist switch. Feeds each closed
/// bar via `feed_smt` (persist + emit) — SMT has no invalidation during
/// seeding, so this is safe to run over historical batches.
/// M6a: Cold-start candidate replay. After SMT replay, rebuild active
/// candidates from the in-memory SMT list + stored 5m CISD/MSS events.
/// Without this, candidates are only generated for SMTs that reach
/// C2Confirmed in real-time; existing PDA-matched SMTs (already in
/// C3Entry) would be missed.
fn replay_candidates(
    smt: &SmtHandle,
    candidates: &CandidateHandle,
    alerts: &AlertHandle,
    store: &SqliteStore,
    symbols: &[String],
) {
    let smt_list = smt.lock().list_active();
    let mut ltf_map: std::collections::HashMap<String, Vec<_>> = std::collections::HashMap::new();
    let mut ltf_bars: std::collections::HashMap<String, Vec<Bar>> =
        std::collections::HashMap::new();
    for sym in symbols {
        match store.list_active_structures(sym, Timeframe::M5) {
            Ok(structures) => {
                ltf_map.insert(sym.clone(), extract_ltf_events(&structures));
            }
            Err(e) => {
                tracing::warn!(error = ?e, %sym, "candidate replay: list_active_structures 5m failed");
            }
        }
        match store.recent_bars(sym, Timeframe::M5, 1000) {
            Ok(bars) => {
                ltf_bars.insert(sym.clone(), bars);
            }
            Err(e) => tracing::warn!(error = ?e, %sym, "candidate replay: recent 5m bars failed"),
        }
    }
    let now = now_ms();
    let cand_changes = candidates.lock().replay(smt_list, &ltf_map, &ltf_bars, now);
    tracing::info!(
        candidates_spawned = cand_changes.len(),
        "candidate cold-start replay complete"
    );
    for change in &cand_changes {
        if let Err(e) = store.upsert_candidate(&change.candidate, now) {
            tracing::warn!(error = ?e, "upsert candidate (replay) failed");
        }
        if let Err(e) = store.insert_decision(&change.decision) {
            tracing::warn!(error = ?e, "insert decision (replay) failed");
        }
        // A replay may discover historical per-symbol validations that an old
        // rule skipped. Reconcile those into Reversal Inbox, but
        // never dispatch historical desktop/banner notifications.
        if let Some(validation) = change.validation_event.as_ref() {
            let already_fired = store
                .has_fired_for_candidate_group_symbol(
                    &change.candidate.id,
                    &change.candidate.watchlist_id,
                    &validation.symbol,
                )
                .unwrap_or(false);
            if !already_fired {
                let alert = ict_monitor::alert::types::build_alert_for_validation(
                    &change.candidate,
                    Some(validation),
                    validation.ts,
                    vec![ict_monitor::alert::ChannelKind::Inbox],
                );
                if let Err(e) = store.insert_alert(&alert) {
                    tracing::warn!(error = ?e, "backfill historical validation alert failed");
                }
            }
        }
    }
    // M6b: replay reconstructs historical facts; it must never notify as
    // though they were live transitions. Feed changes with seeding=true so
    // prev_status is rebuilt while desktop/inbox delivery stays suppressed.
    for change in &cand_changes {
        let _ = alerts.lock().on_candidate_change(change, now, true);
    }
    // Backfill the new per-symbol C2 Alert Inbox independently of Candidate
    // transition output. Existing candidates and later-added trade-leg C2s
    // otherwise would only appear after another live SMT update.
    let all_candidates = candidates.lock().list_all();
    let watchlist_id = all_candidates
        .first()
        .map(|candidate| candidate.watchlist_id.as_str());
    let mut smts_by_id: std::collections::HashMap<String, SmtDivergence> = watchlist_id
        .and_then(|id| store.list_all_smt(id).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|divergence| (divergence.id.clone(), divergence))
        .collect();
    // The in-memory active snapshot is newer than its persisted counterpart.
    for divergence in smt.lock().list_active() {
        smts_by_id.insert(divergence.id.clone(), divergence);
    }
    for candidate in all_candidates {
        if let Some(source_smt) = smts_by_id.get(&candidate.smt_id) {
            let _ = alerts.lock().on_c2_confirmed(&candidate, source_smt, true);
        }
    }
    let active_cands = candidates.lock().list_active();
    alerts.lock().hydrate_missing(&active_cands);
}

static SMT_REPLAYING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn replay_detection_start_index(
    bars: &[Bar],
    event_start: Option<i64>,
    detection_limit: usize,
) -> usize {
    event_start
        .map(|start| bars.partition_point(|bar| bar.ts < start))
        .unwrap_or_else(|| bars.len().saturating_sub(detection_limit))
}

#[cfg(test)]
#[test]
fn replay_context_before_common_chart_start_is_non_emitting() {
    let bars: Vec<Bar> = (0..8)
        .map(|index| Bar {
            symbol: "TVC:DXY".into(),
            tf: Timeframe::H4,
            ts: index * Timeframe::H4.duration_ms(),
            open: 1.0,
            high: 2.0,
            low: 0.5,
            close: 1.5,
            volume: 1.0,
        })
        .collect();

    assert_eq!(
        replay_detection_start_index(&bars, Some(5 * Timeframe::H4.duration_ms()), 3),
        5,
        "bars before the common comparison-TF chart edge are context only"
    );
    assert_eq!(replay_detection_start_index(&bars, None, 3), 5);
}

fn replay_smt_history(
    smt: &SmtHandle,
    candidates: &CandidateHandle,
    alerts: &AlertHandle,
    llm_decisions: Option<&LlmDecisionHandle>,
    engine: &EngineHandle,
    store: &SqliteStore,
    app: &tauri::AppHandle,
    symbols: &[String],
) {
    use std::sync::atomic::Ordering;
    if SMT_REPLAYING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        tracing::info!("smt replay already in progress, skipping");
        return;
    }
    // SMT evaluates completed HTF sweeps and derives the MTF structure from
    // the completed HTF interval.
    let tfs = [Timeframe::M30, Timeframe::H1, Timeframe::H4];
    // Feed bars TF-by-TF, interleaving all symbols by timestamp within each
    // TF so counter buckets are populated when DXY is evaluated.
    // Pre-compute HTF structures once to avoid locking the engine during
    // replay (which competes with real-time 1m bar processing for the
    // engine lock). HTF structures are stable during replay: the IctEngine
    // was already seeded, and real-time 1m bars don't create HTF structures.
    let replay_cache: StructuresCache = {
        let eg = engine.lock();
        let mut m = StructuresCache::new();
        for sym in symbols {
            for htf in [Timeframe::H1, Timeframe::H4] {
                m.insert((sym.clone(), htf), eg.list_active(sym, htf));
            }
        }
        m
    };
    let common_detection_start = |comparison_tf: Timeframe| -> Option<i64> {
        let mut starts = Vec::with_capacity(symbols.len());
        for symbol in symbols {
            let Ok(bars) = store.recent_bars(symbol, comparison_tf, 1_100) else {
                return None;
            };
            let Some(first) = bars.first() else {
                return None;
            };
            starts.push(first.ts);
        }
        // A multi-pane SMT must be chart-auditable on every symbol. Use the
        // latest left edge so a longer history on one feed cannot publish an
        // SMT outside another pane's common comparison-TF window.
        starts.into_iter().max()
    };
    let m30_detection_start = common_detection_start(Timeframe::M30);
    let h1_detection_start = common_detection_start(Timeframe::H1);
    let h4_coverage_issue = h4_replay_coverage_issue(store, symbols, now_ms());
    if let Some(reason) = &h4_coverage_issue {
        tracing::warn!(
            reason = %reason,
            "H4 -> H1 SMT replay disabled for incomplete multi-pane history"
        );
    }
    for tf in tfs {
        if tf == Timeframe::H4 && h4_coverage_issue.is_some() {
            continue;
        }
        // Keep the public detection horizon at 1100 bars, but preload an
        // older, non-emitting context for child TFs. A sweep near the left
        // edge may legitimately use a still-active PDA formed before that
        // edge; rejecting it only because its formation children were not
        // loaded is a replay artifact, not a strategy decision.
        const DETECTION_LIMIT: usize = 1_100;
        const CONTEXT_LIMIT: i64 = 1_500;
        let load_limit = if matches!(tf, Timeframe::M30 | Timeframe::H1) {
            CONTEXT_LIMIT
        } else {
            DETECTION_LIMIT as i64
        };
        let event_start = match tf {
            // H1 parents produce M30 comparison evidence; H4 parents produce
            // H1 evidence. Context before these common chart boundaries may
            // validate a PDA, but must never emit an older inbox row.
            Timeframe::H1 => m30_detection_start,
            Timeframe::H4 => h1_detection_start,
            _ => None,
        };
        let mut context_bars: Vec<Bar> = Vec::new();
        let mut all_bars: Vec<Bar> = Vec::new();
        for sym in symbols {
            match store.recent_bars(sym, tf, load_limit) {
                Ok(bars) => {
                    let detection_start =
                        replay_detection_start_index(&bars, event_start, DETECTION_LIMIT);
                    context_bars.extend(bars[..detection_start].iter().cloned());
                    all_bars.extend(bars[detection_start..].iter().cloned());
                }
                Err(e) => {
                    tracing::warn!(error = ?e, %sym, tf = %tf.tag(), "smt seed: recent_bars failed");
                }
            }
        }
        context_bars.sort_by_key(|bar| (bar.ts, bar.symbol.clone()));
        let context_n = context_bars.len();
        if !context_bars.is_empty() {
            let mut detector = smt.lock();
            for bar in &context_bars {
                detector.seed_bar(bar);
            }
        }
        if all_bars.is_empty() {
            continue;
        }
        all_bars.sort_by_key(|bar| (bar.ts, bar.symbol.clone()));
        let n = all_bars.len();
        for bar in &all_bars {
            feed_smt(
                smt,
                candidates,
                alerts,
                llm_decisions,
                engine,
                store,
                app,
                bar,
                Some(&replay_cache),
                true,
            );
        }
        let smt_count = smt.lock().list_active().len();
        tracing::info!(
            tf = %tf.tag(), n, context_n, event_start = ?event_start, active_smt = smt_count,
            "smt seeded from sqlite history (interleaved)"
        );
    }

    // Repair any incomplete current-rule multi-pane snapshot from canonical
    // buckets before the UI is told that seeding finished. If a required bar
    // is genuinely absent, keep the original evidence unchanged.
    // Materialize the ID in its own scope. Keeping `smt.lock()` inside the
    // `if let` scrutinee extends the temporary guard across the whole body
    // and deadlocks when the repair loop locks SMT again.
    let replay_watchlist_id = { smt.lock().watchlist_id().map(str::to_string) };
    if let Some(watchlist_id) = replay_watchlist_id {
        if let Ok(rows) = store.list_all_smt(&watchlist_id) {
            let mut repaired_count = 0usize;
            let now = now_ms();
            for row in rows {
                let repaired = { smt.lock().repair_missing_chains(&row) };
                let Some(repaired) = repaired else { continue };
                let invalidated = repaired
                    .chains
                    .iter()
                    .find(|chain| chain.symbol == repaired.sweeper_symbol)
                    .is_some_and(|chain| chain.detection_state == SmtDetectionState::Invalidated);
                if !invalidated {
                    smt.lock().update_divergence(repaired.clone());
                }
                if let Err(error) = store.upsert_smt(&repaired, now) {
                    tracing::warn!(
                        error = ?error,
                        smt_id = %repaired.id,
                        "persist repaired multi-pane SMT chains failed"
                    );
                    continue;
                }
                repaired_count += 1;
            }
            if repaired_count > 0 {
                tracing::info!(
                    repaired = repaired_count,
                    "repaired historical SMT rows with missing symbol chains"
                );
            }
        }

        // Backfill a missing terminal reason only when canonical replay bars
        // can prove it; inconclusive evidence remains explicitly unknown.
        if let Ok(rows) = store.list_all_smt(&watchlist_id) {
            let backfills: Vec<(String, Vec<SmtInvalidationReason>)> = {
                let smt_guard = smt.lock();
                rows.into_iter()
                    .filter(|row| row.invalidation_reasons.is_empty())
                    .filter(|row| {
                        row.chains
                            .iter()
                            .find(|chain| chain.symbol == row.sweeper_symbol)
                            .is_some_and(|chain| {
                                chain.detection_state == SmtDetectionState::Invalidated
                            })
                    })
                    .filter_map(|row| {
                        let mut reasons = smt_guard
                            .pda_history_invalidation_reasons(&row)
                            .unwrap_or_default();
                        if let Some(reason) = smt_guard.canonical_c2_invalidation_reason(&row) {
                            reasons.push(reason);
                        }
                        if let Some(reason) = smt_guard.canonical_chain_invalidation_reason(&row) {
                            reasons.push(reason);
                        }
                        reasons.sort_by_key(|reason| match reason {
                            SmtInvalidationReason::SweeperC2Failed => 0,
                            SmtInvalidationReason::SweeperC3Failed => 1,
                            SmtInvalidationReason::AllCountersSwept => 2,
                            SmtInvalidationReason::CanonicalReferenceChanged => 3,
                            SmtInvalidationReason::CanonicalSweepInvalid => 4,
                            SmtInvalidationReason::ReferenceAlreadyTaken => 5,
                            SmtInvalidationReason::CanonicalChainChanged => 6,
                            SmtInvalidationReason::PdaConsumedByOtherSmt => 7,
                            SmtInvalidationReason::HtfFormationCancelled => 8,
                        });
                        reasons.dedup();
                        (!reasons.is_empty()).then_some((row.id, reasons))
                    })
                    .collect()
            };
            let now = now_ms();
            for (id, reasons) in &backfills {
                if let Err(error) = store.mark_smt_invalidated(id, now, reasons) {
                    tracing::warn!(
                        error = ?error,
                        smt_id = %id,
                        "backfill SMT invalidation reasons failed"
                    );
                }
            }
            if !backfills.is_empty() {
                tracing::info!(
                    rows = backfills.len(),
                    "backfilled SMT invalidation reasons from canonical history"
                );
            }
        }

        // Invalidated rows are still first-class audit evidence. Rebuild
        // their canonical liquidity/status/strength payload even when they
        // already have a reason; otherwise a rule migration can leave a row
        // saying `EU strong` beside `all counters swept`.
        if let Ok(rows) = store.list_all_smt(&watchlist_id) {
            let repairs: Vec<(SmtDivergence, Vec<SmtInvalidationReason>)> = {
                let smt_guard = smt.lock();
                rows.into_iter()
                    .filter(|row| {
                        row.chains
                            .iter()
                            .find(|chain| chain.symbol == row.sweeper_symbol)
                            .is_some_and(|chain| {
                                chain.detection_state == SmtDetectionState::Invalidated
                            })
                    })
                    .filter_map(|row| {
                        let mut reasons = row.invalidation_reasons.clone();
                        reasons.extend(
                            smt_guard
                                .pda_history_invalidation_reasons(&row)
                                .unwrap_or_default(),
                        );
                        if let Some(reason) = smt_guard.canonical_c2_invalidation_reason(&row) {
                            reasons.push(reason);
                        }
                        if let Some(reason) = smt_guard.canonical_chain_invalidation_reason(&row) {
                            reasons.push(reason);
                        }
                        reasons.sort_by_key(|reason| match reason {
                            SmtInvalidationReason::SweeperC2Failed => 0,
                            SmtInvalidationReason::SweeperC3Failed => 1,
                            SmtInvalidationReason::AllCountersSwept => 2,
                            SmtInvalidationReason::CanonicalReferenceChanged => 3,
                            SmtInvalidationReason::CanonicalSweepInvalid => 4,
                            SmtInvalidationReason::ReferenceAlreadyTaken => 5,
                            SmtInvalidationReason::CanonicalChainChanged => 6,
                            SmtInvalidationReason::PdaConsumedByOtherSmt => 7,
                            SmtInvalidationReason::HtfFormationCancelled => 8,
                        });
                        reasons.dedup();
                        let snapshot = smt_guard.canonical_pda_snapshot(&row)?;
                        (!reasons.is_empty()).then_some((snapshot, reasons))
                    })
                    .collect()
            };
            let now = now_ms();
            let mut repaired = 0usize;
            for (snapshot, reasons) in repairs {
                if let Err(error) =
                    store.mark_smt_invalidated_with_snapshot(&snapshot, now, &reasons)
                {
                    tracing::warn!(
                        error = ?error,
                        smt_id = %snapshot.id,
                        "repair invalidated canonical SMT evidence failed"
                    );
                } else {
                    repaired += 1;
                }
            }
            if repaired > 0 {
                tracing::info!(
                    rows = repaired,
                    "repaired invalidated SMT rows with canonical evidence"
                );
            }
        }
    }

    // A persisted SMT can outlive the aggregate bars from which its PDA
    // liquidity reference was derived.  Re-audit only when the replay has
    // enough canonical history to give a definitive answer; missing
    // left-edge history is inconclusive and must not invalidate a setup.
    let noncanonical_rows: Vec<(SmtDivergence, Vec<SmtInvalidationReason>)> = {
        let smt_guard = smt.lock();
        smt_guard
            .list_active()
            .into_iter()
            .filter_map(|divergence| {
                let mut reasons = smt_guard
                    .pda_history_invalidation_reasons(&divergence)
                    .unwrap_or_default();
                if let Some(reason) = smt_guard.canonical_chain_invalidation_reason(&divergence) {
                    reasons.push(reason);
                }
                reasons.sort_by_key(|reason| match reason {
                    SmtInvalidationReason::SweeperC2Failed => 0,
                    SmtInvalidationReason::SweeperC3Failed => 1,
                    SmtInvalidationReason::AllCountersSwept => 2,
                    SmtInvalidationReason::CanonicalReferenceChanged => 3,
                    SmtInvalidationReason::CanonicalSweepInvalid => 4,
                    SmtInvalidationReason::ReferenceAlreadyTaken => 5,
                    SmtInvalidationReason::CanonicalChainChanged => 6,
                    SmtInvalidationReason::PdaConsumedByOtherSmt => 7,
                    SmtInvalidationReason::HtfFormationCancelled => 8,
                });
                reasons.dedup();
                if reasons.is_empty() {
                    None
                } else {
                    let snapshot = smt_guard
                        .canonical_pda_snapshot(&divergence)
                        .unwrap_or(divergence);
                    Some((snapshot, reasons))
                }
            })
            .collect()
    };
    if !noncanonical_rows.is_empty() {
        for (snapshot, reasons) in &noncanonical_rows {
            let id = &snapshot.id;
            let market_ts = smt_invalidation_market_ts(snapshot, snapshot.observation_window.1);
            let mut terminal_snapshot = snapshot.clone();
            terminal_snapshot.invalidation_ts = Some(market_ts);
            terminal_snapshot.invalidation_reasons = reasons.clone();
            smt.lock().invalidate_one(id);
            if let Err(e) =
                store.mark_smt_invalidated_with_snapshot(&terminal_snapshot, market_ts, reasons)
            {
                tracing::warn!(error = ?e, smt_id = %id, "invalidate non-canonical replay SMT failed");
            }
            if let Err(e) = store.mark_candidates_smt_invalidated(id, market_ts) {
                tracing::warn!(error = ?e, smt_id = %id, "invalidate candidates for non-canonical replay SMT failed");
            }
            let event = StructureEvent::Invalidated {
                id: id.clone(),
                kind: "smt_divergence".into(),
            };
            let changes = candidates
                .lock()
                .on_smt_invalidated_at(&terminal_snapshot, market_ts);
            for change in changes {
                let _ = store.upsert_candidate(&change.candidate, market_ts);
                let _ = store.insert_decision(&change.decision);
                let _ = alerts.lock().on_candidate_change(&change, market_ts, true);
            }
            let _ = app.emit(event.topic(), event);
        }
        tracing::warn!(
            invalidated = noncanonical_rows.len(),
            "replay invalidated SMTs whose PDA liquidity references no longer match canonical bars"
        );
    }

    // Post-replay: re-compute current-rule chart geometry from full history.
    {
        let mut smt_guard = smt.lock();
        let mut divs: Vec<SmtDivergence> = smt_guard.list_active();
        let active_ids: std::collections::HashSet<String> = divs
            .iter()
            .map(|divergence| divergence.id.clone())
            .collect();
        // Invalidated rows remain clickable audit evidence in SMT Inbox, so
        // repair their MTF endpoints too. Do not hydrate them back into the
        // live engine (which would re-reserve a consumed PDA).
        if let Some(watchlist_id) = smt_guard.watchlist_id() {
            if let Ok(stored) = store.list_all_smt(watchlist_id) {
                let known_ids: std::collections::HashSet<String> = divs
                    .iter()
                    .map(|divergence| divergence.id.clone())
                    .collect();
                divs.extend(
                    stored
                        .into_iter()
                        .filter(|divergence| !known_ids.contains(&divergence.id)),
                );
            }
        }
        let now = now_ms();
        let mut updated = 0u32;
        for mut d in divs {
            if let Some(endpoint) = smt_guard.resolve_mtf_ref_candle(&d) {
                d.mtf_ref_candle = Some(endpoint);
            }
            let Some(ref pda) = d.htf_pda_ref else {
                // Persist SMTs whose PDA was cleared during hydration
                // (stale FVG filled before sweep, or OB ref removed) so
                // the DB stays in sync and the fix is permanent across
                // restarts.
                let _ = store.upsert_smt(&d, now);
                continue;
            };
            let _ = pda;
            SmtEngine::stamp_pda_display_end(&mut d);
            if active_ids.contains(&d.id) {
                smt_guard.update_divergence(d.clone());
            }
            let _ = store.upsert_smt(&d, now);
            updated += 1;
        }
        tracing::info!(updated, "post-replay SMT geometry re-computation done");
    }

    SMT_REPLAYING.store(false, Ordering::SeqCst);
}

// ---- M5/M6c subscription manager -------------------------------------------

struct SubEntry {
    handle: tokio::task::JoinHandle<()>,
    tx: tokio::sync::mpsc::Sender<BarEvent>,
}

/// Owns one independent aggregation/strategy worker per symbol, fed by one
/// multiplexed TradingView WebSocket for the complete symbol union. Keeping
/// workers separate preserves fault/strategy isolation while avoiding the TV
/// concurrent chart-session cap that starved M6c groups 2 and 3.
struct SubManager {
    store: SqliteStore,
    engine: EngineHandle,
    engine_groups: EngineGroups,
    aggregators:
        Arc<AsyncMutex<std::collections::HashMap<String, Arc<AsyncMutex<SymbolAggregator>>>>>,
    auth: TvAuth,
    bar_tx: broadcast::Sender<BarPayload>,
    app: tauri::AppHandle,
    subs: std::collections::HashMap<String, SubEntry>,
    feed_handle: Option<tokio::task::JoinHandle<()>>,
    history_seed_lock: Arc<AsyncMutex<()>>,
    history_fetch_lock: Arc<AsyncMutex<()>>,
    reconnect_preflight_gate: Arc<ReconnectHistoryPreflightGate>,
}

impl SubManager {
    fn acquire(&mut self, symbol: &str) {
        if self.subs.contains_key(symbol) {
            return;
        }
        let app = self.app.clone();
        let store = self.store.clone();
        let engine = self.engine.clone();
        let engine_groups = self.engine_groups.clone();
        let aggregators = self.aggregators.clone();
        let bar_tx = self.bar_tx.clone();
        let history_seed_lock = self.history_seed_lock.clone();
        let sym = symbol.to_string();
        let (tx, rx) = tokio::sync::mpsc::channel::<BarEvent>(2048);
        let handle = tokio::spawn(async move {
            if let Err(e) = run_symbol_subscription(
                app,
                store,
                sym.clone(),
                aggregators,
                engine,
                bar_tx,
                engine_groups,
                history_seed_lock,
                rx,
            )
            .await
            {
                tracing::error!(error = ?e, %sym, "symbol subscription exited");
            }
        });
        self.subs
            .insert(symbol.to_string(), SubEntry { handle, tx });
    }

    fn release(&mut self, symbol: &str) {
        if let Some(entry) = self.subs.remove(symbol) {
            entry.handle.abort();
            tracing::info!(%symbol, "subscription released");
        }
    }

    /// Reconcile running subscriptions against a desired symbol set:
    /// release symbols no longer wanted, acquire new ones.
    fn set_symbols(&mut self, desired: &[String]) {
        let before: std::collections::BTreeSet<String> = self.subs.keys().cloned().collect();
        let desired_set: std::collections::BTreeSet<String> = desired.iter().cloned().collect();
        if before == desired_set && self.feed_handle.is_some() {
            return;
        }
        let to_remove: Vec<String> = self
            .subs
            .keys()
            .filter(|s| !desired.iter().any(|d| d == *s))
            .cloned()
            .collect();
        for s in to_remove {
            self.release(&s);
        }
        for s in desired {
            self.acquire(s);
        }

        if let Some(handle) = self.feed_handle.take() {
            handle.abort();
        }
        let mut symbols: Vec<String> = self.subs.keys().cloned().collect();
        symbols.sort();
        let routes: std::collections::HashMap<_, _> = self
            .subs
            .iter()
            .map(|(symbol, entry)| (symbol.clone(), entry.tx.clone()))
            .collect();
        let client = TvClient::with_auth(self.auth.clone());
        let preflight_app = self.app.clone();
        let preflight_store = self.store.clone();
        let preflight_auth = self.auth.clone();
        let preflight_symbols = symbols.clone();
        let history_fetch_lock = self.history_fetch_lock.clone();
        let reconnect_preflight_gate = self.reconnect_preflight_gate.clone();
        self.feed_handle = Some(tokio::spawn(async move {
            let mut rx = match client.subscribe_many_bars(&symbols, Timeframe::M1).await {
                Ok(rx) => rx,
                Err(error) => {
                    tracing::error!(?error, "failed to start TradingView union subscription");
                    return;
                }
            };
            tracing::info!(symbols = symbols.len(), "TradingView union feed started");
            while let Some(event) = rx.recv().await {
                let is_non_empty_history =
                    matches!(&event, BarEvent::Historical(bars) if !bars.is_empty());
                let event_symbol = match &event {
                    BarEvent::Historical(bars) => bars.first().map(|bar| bar.symbol.clone()),
                    BarEvent::BarUpdate(bar) | BarEvent::BarClosed(bar) => Some(bar.symbol.clone()),
                };
                let Some(event_symbol) = event_symbol else {
                    continue;
                };
                if let Some(route) = routes.get(&event_symbol) {
                    if route.send(event).await.is_err() {
                        tracing::warn!(symbol = %event_symbol, "union route worker unavailable");
                    } else if is_non_empty_history && reconnect_preflight_gate.try_begin(now_ms()) {
                        tokio::spawn(run_reconnect_history_preflight(
                            preflight_app.clone(),
                            preflight_store.clone(),
                            preflight_auth.clone(),
                            preflight_symbols.clone(),
                            history_fetch_lock.clone(),
                            reconnect_preflight_gate.clone(),
                        ));
                    }
                }
            }
            tracing::warn!("TradingView union feed channel closed");
        }));
    }
}

// ---- IPC command wrappers (must live in this crate for `generate_handler!`) ----

/// Snapshot the visible webview directly so canvas layers, fonts and clipping
/// match the app exactly. No files or screen-recording permission are needed.
#[tauri::command]
async fn copy_screenshot(window: tauri::WebviewWindow) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        window.with_webview(move |view| {
            use objc2::{class, msg_send, runtime::AnyObject};
            use objc2_foundation::NSString;
            let sender = std::sync::Mutex::new(Some(tx));
            let completion = block2::RcBlock::new(move |image: *mut AnyObject, error: *mut AnyObject| {
                let result = if image.is_null() || !error.is_null() {
                    Err("无法生成页面快照，请重试".to_string())
                } else {
                    // SAFETY: WebKit invokes this block on the main thread and owns
                    // the NSImage for the callback duration. Pasteboard copies its data.
                    unsafe {
                        let data: *mut AnyObject = msg_send![image, TIFFRepresentation];
                        if data.is_null() { Err("无法编码截图".to_string()) } else {
                            let pasteboard: *mut AnyObject = msg_send![class!(NSPasteboard), generalPasteboard];
                            let kind = NSString::from_str("public.tiff");
                            // AppKit declares NSInteger clearContents (change count), not void.
                            let _: isize = msg_send![pasteboard, clearContents];
                            let ok: bool = msg_send![pasteboard, setData: data, forType: &*kind];
                            if ok { Ok(()) } else { Err("系统拒绝写入图片剪贴板".to_string()) }
                        }
                    }
                };
                if let Some(sender) = sender.lock().unwrap().take() { let _ = sender.send(result); }
            });
            // SAFETY: Tauri dispatches with_webview on the main thread. inner is the
            // live WKWebView; WebKit copies and retains the completion block.
            unsafe {
                let webview = view.inner().cast::<AnyObject>();
                let _: () = msg_send![webview, takeSnapshotWithConfiguration: std::ptr::null::<AnyObject>(), completionHandler: &*completion];
            }
        }).map_err(|_| "无法访问 APP 页面")?;
        tokio::time::timeout(std::time::Duration::from_secs(15), rx).await
            .map_err(|_| "截图超时，请重试")?.map_err(|_| "截图中断")?
    }
    #[cfg(not(target_os = "macos"))]
    { let _ = window; Err("当前图片剪贴板实现仅支持 macOS".into()) }
}

#[tauri::command]
async fn list_symbols(state: tauri::State<'_, AppState>) -> Result<Vec<SymbolMeta>, String> {
    let cfg = state.config.read();
    let mut symbols = state.symbols.clone();
    symbols.extend(cfg.chart_symbols.iter().cloned());
    symbols.extend(monitored_symbol_union(&cfg.watchlists));
    let mut seen = std::collections::HashSet::new();
    symbols.retain(|s| seen.insert(s.clone()));
    Ok(symbols.into_iter().map(|symbol| SymbolMeta { symbol, provider: "tradingview".into() }).collect())
}

#[tauri::command]
async fn search_symbols(state: tauri::State<'_, AppState>, query: String) -> Result<Vec<ict_monitor::data_source::symbol_search::SearchSymbol>, String> {
    let proxy = state.config.read().tradingview.proxy_url.clone();
    ict_monitor::data_source::symbol_search::search(&query, proxy.as_deref()).await
}

#[tauri::command]
async fn list_symbol_prices(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<(String, Option<f64>)>, String> {
    let mut out = Vec::new();
    let symbols = {
        let cfg = state.config.read();
        let mut symbols = state.symbols.clone();
        symbols.extend(cfg.chart_symbols.clone());
        symbols.extend(monitored_symbol_union(&cfg.watchlists));
        symbols.sort(); symbols.dedup(); symbols
    };
    for sym in &symbols {
        let price = match state.store.recent_bars(sym, Timeframe::M1, 1) {
            Ok(bars) => bars.first().map(|b| b.close),
            Err(_) => None,
        };
        out.push((sym.clone(), price));
    }
    Ok(out)
}

fn is_forex_symbol(symbol: &str) -> bool {
    let s = symbol.to_uppercase();
    if s.contains("DXY") {
        return true;
    }
    let core = s.strip_prefix("OANDA:").unwrap_or(&s);
    core.len() == 6 && core.chars().all(|c| c.is_ascii_uppercase())
}

/// Persist only the canonical H4 slice that can be audited from the retained
/// H1 history. Rows inside that slice are replaced so TV-raw/off-grid candles
/// cannot leak into the chart, while older native H4 history remains intact.
fn persist_canonical_h4_slice(
    store: &SqliteStore,
    symbol: &str,
    bars: &[Bar],
) -> anyhow::Result<usize> {
    let (Some(first), Some(last)) = (bars.first(), bars.last()) else {
        return Ok(0);
    };
    store.replace_bars_in_range(symbol, Timeframe::H4, first.ts, last.ts, bars)?;
    Ok(bars.len())
}

/// Normalize provider calendar candles onto the same NY-local boundaries used
/// by the live aggregator. TradingView can return daily/weekly rows with an
/// exchange timestamp inside the canonical window; keeping both timestamps in
/// SQLite makes one logical candle look duplicated and can hide a real gap.
fn canonicalize_provider_calendar_bars(bars: Vec<Bar>) -> Vec<Bar> {
    let Some(first) = bars.first() else {
        return bars;
    };
    if !matches!(first.tf, Timeframe::D1 | Timeframe::W1 | Timeframe::MN1) {
        return bars;
    }

    let mut canonical = BTreeMap::<i64, Bar>::new();
    for mut bar in bars {
        let window = bar.tf.boundary_align(bar.ts);
        bar.ts = window;
        match canonical.entry(window) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(bar);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let merged = entry.get_mut();
                merged.high = merged.high.max(bar.high);
                merged.low = merged.low.min(bar.low);
                merged.close = bar.close;
                // Provider snapshots can contain the same logical candle more
                // than once. `max` avoids double-counting its volume.
                merged.volume = merged.volume.max(bar.volume);
            }
        }
    }
    canonical.into_values().collect()
}

/// Persist an authoritative chart snapshot. Fixed intraday rows remain
/// additive, while calendar rows replace the fetched logical range so legacy
/// off-grid duplicates cannot survive beside their canonical replacements.
fn persist_chart_provider_history(
    store: &SqliteStore,
    symbol: &str,
    tf: Timeframe,
    fetched: &[Bar],
    history_now: i64,
) -> anyhow::Result<Vec<Bar>> {
    let finalized = finalized_history_bars(fetched, history_now);
    let canonical = canonicalize_provider_calendar_bars(finalized);

    if matches!(tf, Timeframe::D1 | Timeframe::W1 | Timeframe::MN1) {
        if let (Some(first), Some(last)) = (canonical.first(), canonical.last()) {
            let end = next_window_boundary(tf, last.ts).saturating_sub(1);
            store.replace_bars_in_range(symbol, tf, first.ts, end, &canonical)?;
        }
    } else {
        store.insert_bars(&canonical)?;
    }
    Ok(canonical)
}

/// Forex 4h re-aggregation path.
///
/// TVC:DXY (and occasionally OANDA forex) 4h bars arrive on an inconsistent
/// grid, which desyncs cross-symbol SMT white-line endpoints. 1h bars are
/// always clean, so we backfill 1h from TV (up to ~4x the requested 4h depth),
/// re-aggregate 4h on the canonical NY-local grid, replace only the auditable
/// H1-covered slice (preserving deeper native H4), and return the most recent
/// `want` closed bars plus the live rolling bar. 1h->4h aggregation is
/// OHLC-identical to the 1m->4h path the live aggregator uses, so there is no
/// discrepancy with live bars.
async fn forex_h4_history(
    state: &AppState,
    symbol: &str,
    want: usize,
    force_fresh: bool,
    allow_network: bool,
) -> Result<Vec<BarPayload>, String> {
    let need_h1 = ((want as i64) * 4 + 8).min(5000);
    let mut h1 = state
        .store
        .recent_bars(symbol, Timeframe::H1, need_h1)
        .map_err(|e| format!("query 1h failed: {e}"))?;

    // Backfill 1h from TV when forced fresh or when SQLite lacks enough depth.
    if allow_network
        && (force_fresh || chart_history_needs_provider_refresh(&h1, need_h1 as usize, now_ms()))
    {
        let n = need_h1.min(5000) as u32;
        let _history_guard = state.history_fetch_lock.lock().await;
        match state
            .tv_client
            .fetch_history(symbol, Timeframe::H1, n)
            .await
        {
            Ok(fetched) if !fetched.is_empty() => {
                let finalized = finalized_history_bars(&fetched, now_ms());
                tracing::info!(
                    tf = "1h",
                    n = fetched.len(),
                    finalized = finalized.len(),
                    "TV 1h backfill (forex-4h reaggregate)"
                );
                if let Err(e) = state.store.insert_bars(&finalized) {
                    tracing::error!(error = ?e, "forex-4h 1h insert failed");
                }
                h1 = state
                    .store
                    .recent_bars(symbol, Timeframe::H1, need_h1)
                    .map_err(|e| format!("query 1h failed: {e}"))?;
            }
            Ok(_) => tracing::warn!(tf = "1h", "TV returned no 1h bars (forex-4h)"),
            Err(e) => tracing::warn!(tf = "1h", error = ?e, "TV 1h fetch failed (forex-4h)"),
        }
    }

    let history_now = now_ms();
    let mut native_h4 = state
        .store
        .recent_bars(symbol, Timeframe::H4, H4_NATIVE_FALLBACK_LIMIT)
        .map_err(|e| format!("query existing 4h failed: {e}"))?;
    let strict_h4 = aggregate_h4_from_h1(&h1, symbol, history_now);
    let mut aggregated = aggregate_h4_with_native_fallback(&h1, &native_h4, symbol, history_now);

    // A new short provider outage may have happened after startup. Fetch a
    // native H4 snapshot on demand when strict H1 aggregation has an internal
    // hole and the stored canonical set contains no usable fallback yet.
    if allow_network
        && has_incomplete_h4_window(&h1, symbol, history_now)
        && aggregated.len() == strict_h4.len()
    {
        let _history_guard = state.history_fetch_lock.lock().await;
        match state
            .tv_client
            .fetch_history(symbol, Timeframe::H4, H4_NATIVE_FALLBACK_LIMIT as u32)
            .await
        {
            Ok(fetched) if !fetched.is_empty() => {
                native_h4.extend(finalized_history_bars(&fetched, history_now));
                aggregated =
                    aggregate_h4_with_native_fallback(&h1, &native_h4, symbol, history_now);
                tracing::info!(
                    %symbol,
                    fetched = fetched.len(),
                    recovered = aggregated.len().saturating_sub(strict_h4.len()),
                    "on-demand native H4 fallback merged"
                );
            }
            Ok(_) => tracing::warn!(%symbol, "TV returned no on-demand native H4 fallback"),
            Err(error) => tracing::warn!(
                ?error,
                %symbol,
                "on-demand native H4 fallback fetch failed"
            ),
        }
    }

    // Replace only the canonical slice reconstructed above. A full pair wipe
    // would discard deeper TV-native H4 every time the user opens an H4 pane.
    if let Err(e) = persist_canonical_h4_slice(&state.store, symbol, &aggregated) {
        tracing::error!(error = ?e, "forex-4h bounded replace failed");
    }

    // Return the `want` most recent closed 4h bars.
    let start = aggregated.len().saturating_sub(want);
    let mut out: Vec<BarPayload> = aggregated[start..]
        .iter()
        .map(|b| BarPayload::from_bar(b, true))
        .collect();

    // Append the still-rolling (open) 4h bar from the live aggregator.
    if let Some(handle) = {
        let map = state.aggregators.lock().await;
        map.get(symbol).cloned()
    } {
        let agg = handle.lock().await;
        if let Some(open_bar) = agg.current(Timeframe::H4) {
            if out.last().map(|b| b.ts) != Some(open_bar.ts) {
                out.push(BarPayload::from_bar(open_bar, false));
            }
        }
    }

    Ok(out)
}

#[tauri::command]
async fn get_history(
    state: tauri::State<'_, AppState>,
    symbol: String,
    tf: String,
    limit: i64,
    force_fresh: Option<bool>,
) -> Result<Vec<BarPayload>, String> {
    let tf_enum = IctTimeframe::from_tag(&tf).ok_or_else(|| format!("unknown tf: {tf}"))?;
    let want = limit.max(1) as usize;
    let force_fresh = force_fresh.unwrap_or(false);

    // Forex 4h: TV-raw 4h bars (WebSocket API) land on a 01/05/09/13/17/21
    // NY-local grid, but TV chart displays 4h on 23/03/07/11/15/19 NY-local.
    // Re-aggregate 4h from clean 1h bars on the chart grid so the app
    // matches what the user sees on TradingView. See forex_h4_history.
    if tf_enum == Timeframe::H4 && is_forex_symbol(&symbol) {
        return forex_h4_history(&*state, &symbol, want, force_fresh, true).await;
    }

    let mut provider_attempted = false;

    // If the caller explicitly asks for a fresh batch (typically the first
    // load right after the UI starts), pull straight from TradingView so we
    // pick up any 1m bars produced between the last shutdown and "now".
    if force_fresh {
        provider_attempted = true;
        let need = want.min(5000) as u32;
        let _history_guard = state.history_fetch_lock.lock().await;
        match state.tv_client.fetch_history(&symbol, tf_enum, need).await {
            Ok(fetched) if !fetched.is_empty() => {
                match persist_chart_provider_history(
                    &state.store,
                    &symbol,
                    tf_enum,
                    &fetched,
                    now_ms(),
                ) {
                    Ok(finalized) => tracing::info!(
                        tf = %tf_enum.tag(),
                        n = fetched.len(),
                        finalized = finalized.len(),
                        "TV history backfill (force_fresh)"
                    ),
                    Err(e) => tracing::error!(error = ?e, "force_fresh insert failed"),
                }
            }
            Ok(_) => tracing::warn!(tf = %tf_enum.tag(), "TV returned no bars (force_fresh)"),
            Err(e) => {
                tracing::warn!(tf = %tf_enum.tag(), error = ?e, "TV force_fresh fetch failed")
            }
        }
    }

    let mut rows = state
        .store
        .recent_bars(&symbol, tf_enum, limit)
        .map_err(|e| format!("query failed: {e}"))?;
    // Heal elapsed fixed-TF holes from complete local M1 before asking the
    // provider. The inspection is cheap and the rebuild runs only when the
    // requested history has an internal gap or is missing its latest fully
    // closed canonical window.
    if matches!(
        tf_enum,
        Timeframe::M5 | Timeframe::M15 | Timeframe::M30 | Timeframe::H1
    ) && fixed_aggregate_rows_need_fill(&rows, &symbol, tf_enum, now_ms())
    {
        match fill_missing_recent_aggregates(&state.store, &symbol) {
            Ok(repaired) if repaired > 0 => {
                tracing::info!(
                    %symbol,
                    tf = %tf_enum.tag(),
                    repaired,
                    "filled missing aggregates before chart history query"
                );
                rows = state
                    .store
                    .recent_bars(&symbol, tf_enum, limit)
                    .map_err(|e| format!("query repaired history failed: {e}"))?;
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(
                ?error,
                %symbol,
                tf = %tf_enum.tag(),
                "on-demand aggregate gap repair failed"
            ),
        }
    }
    // If SQLite doesn't have enough bars for this TF (typically true for the
    // first time the user opens 4h/1d/1w on a fresh install), fetch a fresh
    // batch from TradingView and persist it. This is a one-shot WS session
    // separate from the live 1m subscription.
    let needs_provider_refresh = chart_history_needs_provider_refresh(&rows, want, now_ms());
    if needs_provider_refresh && !provider_attempted {
        let need = want.min(5000) as u32;
        let _history_guard = state.history_fetch_lock.lock().await;
        match state.tv_client.fetch_history(&symbol, tf_enum, need).await {
            Ok(fetched) if !fetched.is_empty() => {
                match persist_chart_provider_history(
                    &state.store,
                    &symbol,
                    tf_enum,
                    &fetched,
                    now_ms(),
                ) {
                    Ok(finalized) => tracing::info!(
                        tf = %tf_enum.tag(),
                        n = fetched.len(),
                        finalized = finalized.len(),
                        "TV history backfill (on-demand)"
                    ),
                    Err(e) => tracing::error!(error = ?e, "on-demand insert failed"),
                }
                rows = state
                    .store
                    .recent_bars(&symbol, tf_enum, limit)
                    .map_err(|e| format!("query failed: {e}"))?;
            }
            Ok(_) => tracing::warn!(tf = %tf_enum.tag(), "TV returned no bars"),
            Err(e) => tracing::warn!(tf = %tf_enum.tag(), error = ?e, "TV history fetch failed"),
        }
    }

    let mut out: Vec<BarPayload> = rows.iter().map(|b| BarPayload::from_bar(b, true)).collect();

    // Append the still-rolling (not yet closed) bar if we have one — gives the
    // chart a continuous view across TF switches.
    if let Some(handle) = {
        let map = state.aggregators.lock().await;
        map.get(&symbol).cloned()
    } {
        let agg = handle.lock().await;
        if let Some(open_bar) = agg.current(tf_enum) {
            if out.last().map(|b| b.ts) != Some(open_bar.ts) {
                out.push(BarPayload::from_bar(open_bar, false));
            }
        }
    }

    Ok(out)
}

/// Return the best complete local chart snapshot without opening a
/// TradingView history connection or running an aggregate repair pass.
///
/// UI timeframe switches and Inbox navigation use this command first so
/// cached candles render immediately. `get_history` remains the authoritative
/// background refresh path and can persist a newer provider snapshot later.
#[tauri::command]
async fn get_cached_history(
    state: tauri::State<'_, AppState>,
    symbol: String,
    tf: String,
    limit: i64,
) -> Result<Vec<BarPayload>, String> {
    let tf_enum = IctTimeframe::from_tag(&tf).ok_or_else(|| format!("unknown tf: {tf}"))?;
    let want = limit.max(1) as usize;

    if tf_enum == Timeframe::H4 && is_forex_symbol(&symbol) {
        return forex_h4_history(&*state, &symbol, want, false, false).await;
    }

    let rows = state
        .store
        .recent_bars(&symbol, tf_enum, limit.max(1))
        .map_err(|e| format!("cached history query failed: {e}"))?;
    let mut out: Vec<BarPayload> = rows
        .iter()
        .map(|bar| BarPayload::from_bar(bar, true))
        .collect();

    if let Some(handle) = {
        let map = state.aggregators.lock().await;
        map.get(&symbol).cloned()
    } {
        let agg = handle.lock().await;
        if let Some(open_bar) = agg.current(tf_enum) {
            if out.last().map(|bar| bar.ts) != Some(open_bar.ts) {
                out.push(BarPayload::from_bar(open_bar, false));
            }
        }
    }

    Ok(out)
}

/// Return the common calendar start of the locally persisted history tails.
///
/// Inbox polling only needs a visibility cutoff; it must never open a
/// TradingView history session.  Reusing `get_history` here used to enqueue
/// three provider backfills every five seconds.  Those requests could occupy
/// the account's sole one-shot history connection and starve an actual chart
/// timeframe switch, leaving every pane with only its live rolling candle.
#[tauri::command]
fn get_history_cutoff(
    state: tauri::State<'_, AppState>,
    symbols: Vec<String>,
    tf: String,
    limit: i64,
) -> Result<i64, String> {
    let tf_enum = IctTimeframe::from_tag(&tf).ok_or_else(|| format!("unknown tf: {tf}"))?;
    if symbols.is_empty() {
        return Ok(0);
    }

    let mut common_start = 0_i64;
    for symbol in symbols {
        let rows = state
            .store
            .recent_bars(&symbol, tf_enum, limit.max(1))
            .map_err(|e| format!("cutoff query failed for {symbol} {tf}: {e}"))?;
        let Some(first) = rows.first() else {
            // No local audit window for one pane yet. Do not turn "missing"
            // into "now" and hide every historical Inbox row; the chart's
            // normal history loader will populate this pair independently.
            return Ok(0);
        };
        common_start = common_start.max(first.ts);
    }
    Ok(common_start)
}

#[tauri::command]
async fn add_symbol(app: tauri::AppHandle, state: tauri::State<'_, AppState>, symbol: String) -> Result<(), String> {
    ict_monitor::data_source::symbol_search::validate_symbol(&symbol)?;
    let sub = app.try_state::<Arc<AsyncMutex<SubManager>>>().ok_or("行情服务尚未就绪，请稍后重试")?;
    {
        let mut cfg = state.config.write();
        if !cfg.chart_symbols.contains(&symbol) {
            let mut next = cfg.clone();
            next.chart_symbols.push(symbol.clone());
            next.save().map_err(|_| "无法保存品种列表")?;
            *cfg = next;
        }
    }
    sub.lock().await.acquire(&symbol);
    Ok(())
}

#[tauri::command]
async fn remove_symbol(app: tauri::AppHandle, symbol: String) -> Result<(), String> {
    if let Some(sub) = app.try_state::<Arc<AsyncMutex<SubManager>>>() {
        sub.lock().await.release(&symbol);
    }
    Ok(())
}

/// Filter FVGs: keep only those consumed by an SMT (ID in the consumed
/// map), and set their  for frontend right-edge
/// truncation. Unconsumed FVGs are removed entirely.
fn filter_consumed_fvgs(
    list: &mut Vec<IctStructure>,
    consumed: &std::collections::HashMap<String, i64>,
) {
    // M3 FVG/OB display is controlled by the frontend toggle and state
    // filter - do NOT filter them out here. Only stamp consumed_exit_ts
    // on FVGs used as SMT PDAs so the frontend can truncate their right
    // edge at the exit bar. OBs are no longer PDA candidates
    // (user: "OB区先不作为pda") but should still display via the M3 toggle.
    for s in list.iter_mut() {
        if let IctStructure::Fvg(f) = s {
            if let Some(&exit_ts) = consumed.get(&f.id) {
                f.consumed_exit_ts = Some(exit_ts);
            }
        }
    }
}

/// Stamp the first 90%-penetration bar as the PDA chart endpoint.  Keep the
/// earlier of this eligibility endpoint and a genuine SMT-consumption exit.
/// The underlying FVG state is deliberately untouched.
fn stamp_depleted_fvg_ends(list: &mut [IctStructure], smt: &SmtEngine) {
    for structure in list {
        let IctStructure::Fvg(fvg) = structure else {
            continue;
        };
        let Some(depleted_ts) = smt.fvg_smt_depletion_ts(fvg) else {
            continue;
        };
        fvg.consumed_exit_ts = Some(
            fvg.consumed_exit_ts
                .map_or(depleted_ts, |current| current.min(depleted_ts)),
        );
    }
}

/// Session ranges are cross-timeframe display data. During startup or a
/// symbol-history replay the detector engine can briefly contain only a
/// partial structure set; SQLite still holds the last complete session
/// snapshot. Merge those rows by id so the chart never receives a non-empty
/// but session-less snapshot (which previously made the DXY toggle appear to
/// do nothing until `seed_complete`). In-memory rows win because they may
/// contain a newer high/low for the currently open session.
fn merge_persisted_session_structures(
    store: &SqliteStore,
    symbol: &str,
    list: &mut Vec<IctStructure>,
) {
    let persisted = match store.list_active_session_structures(symbol) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(?error, %symbol, "list persisted session structures failed");
            return;
        }
    };
    let mut ids: std::collections::HashSet<String> = list
        .iter()
        .map(|structure| structure.id().to_string())
        .collect();
    list.extend(
        persisted
            .into_iter()
            .filter(|structure| ids.insert(structure.id().to_string())),
    );
}

#[tauri::command]
async fn list_structures(
    state: tauri::State<'_, AppState>,
    symbol: String,
    tf: String,
    watchlist_id: Option<String>,
) -> Result<Vec<IctStructure>, String> {
    let tf_enum = IctTimeframe::from_tag(&tf).ok_or_else(|| format!("unknown tf: {tf}"))?;
    let cache_key = format!("{}|{symbol}|{tf}", watchlist_id.as_deref().unwrap_or("all"));

    // SMT divergences live in a separate engine (state.smt) with its
    // own Mutex, so they can always be read fresh regardless of whether
    // the main engine lock is available. Collect them up-front so the
    // cache-fallback path still returns current SMT data.
    let selected_groups = selected_engine_groups(&state.engine_groups, watchlist_id.as_deref())?;
    let selected_groups: Vec<_> = selected_groups
        .into_iter()
        .filter(|group| group.watchlist.symbols.iter().any(|item| item == &symbol))
        .collect();
    let mut all_smts = Vec::new();
    for group in &selected_groups {
        all_smts.extend(public_smts_for_group(group, &state.store));
    }
    let mut smt_list: Vec<IctStructure> = Vec::new();
    for d in &all_smts {
        // SMT shows on both MTF (comparison_timeframe) and HTF (context_timeframe) panes.
        // The storage and hydration layers already expose only current-rule
        // DXY-PDA SMTs. Return them on both the MTF and HTF panes.
        if (d.comparison_timeframe == tf_enum || d.context_timeframe == tf_enum)
            && d.chains.iter().any(|c| c.symbol == symbol)
        {
            smt_list.push(IctStructure::SmtDivergence(d.clone()));
        }
    }

    // Build a map of consumed PDA IDs -> exit_ts from ALL SMT divergences.
    // Used to stamp consumed_exit_ts on FVGs that are SMT PDAs so the
    // frontend truncates their right edge at the exit bar.
    let consumed_pdas: std::collections::HashMap<String, i64> = all_smts
        .iter()
        .filter(|d| {
            d.chains
                .iter()
                .find(|chain| chain.symbol == d.sweeper_symbol)
                .is_some_and(|chain| chain.detection_state == SmtDetectionState::C3Entry)
        })
        .filter_map(|d| d.htf_pda_ref.as_ref())
        .filter_map(|p| p.exit_ts.map(|ts| (p.id.clone(), ts)))
        .collect();

    // Try to acquire the engine lock with a short retry loop. When the
    // user switches TF, all panes call list_structures simultaneously.
    // A single try_lock often fails for 2 of 3 panes because the engine
    // is briefly busy. Retry a few times with tiny sleeps so all panes
    // get fresh data instead of falling back to a stale cache.
    for attempt in 0..5u8 {
        if let Some(engine) = state.engine.try_lock() {
            // During cold-start seed the engine has incomplete data
            // (truncated seed window + not-yet-re-hydrated structures).
            // Returning and caching this would overwrite the SQLite
            // fallback cache with a partial set, hiding CISD/MSS and
            // other structures until seed_complete forces a re-fetch.
            // Skip the fresh path while seeding so the fallback (cache
            // or SQLite) returns the complete persisted set.
            if engine.seeding.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let mut list = engine.list_active(&symbol, tf_enum);
            merge_persisted_session_structures(&state.store, &symbol, &mut list);
            filter_consumed_fvgs(&mut list, &consumed_pdas);
            for group in &selected_groups {
                stamp_depleted_fvg_ends(&mut list, &group.smt.lock());
            }
            list.extend(smt_list.clone());
            tracing::info!(%symbol, %tf, total = list.len(), smt = smt_list.len(), "list_structures fresh");
            {
                let mut cache = state.structures_cache.write();
                cache.insert(cache_key, list.clone());
            }
            return Ok(list);
        }
        if attempt < 4 {
            tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        }
    }

    // All retries failed - engine is busy for an extended period
    // (e.g. cold-start seed, PO3 replay, PDH/PDL param change).
    // Return cached structures if available; otherwise query SQLite
    // directly so the chart isn't blank during the multi-minute seed.
    let cached = state.structures_cache.read().get(&cache_key).cloned();
    let base: Vec<IctStructure> = match &cached {
        Some(c) if !c.is_empty() => c
            .iter()
            .filter(|s| s.kind_tag() != "smt_divergence")
            .cloned()
            .collect(),
        _ => {
            // Cache empty/stale - SQLite has the hydrated structures
            // from the previous session. This is a read-only query and
            // does not contend for the engine Mutex.
            match state.store.list_active_structures(&symbol, tf_enum) {
                Ok(rows) => {
                    tracing::info!(
                        %symbol, %tf, n = rows.len(),
                        "list_structures sqlite fallback"
                    );
                    rows
                }
                Err(e) => {
                    tracing::warn!(
                        error = ?e, %symbol, %tf,
                        "list_structures sqlite fallback failed"
                    );
                    Vec::new()
                }
            }
        }
    };
    tracing::info!(
        %symbol, %tf, base_len = base.len(), smt = smt_list.len(),
        cached = cached.as_ref().map_or(0, |c| c.len()),
        "list_structures fallback (cache or sqlite)"
    );
    let mut result = base;
    merge_persisted_session_structures(&state.store, &symbol, &mut result);
    filter_consumed_fvgs(&mut result, &consumed_pdas);
    for group in &selected_groups {
        stamp_depleted_fvg_ends(&mut result, &group.smt.lock());
    }
    result.extend(smt_list);
    Ok(result)
}

#[tauri::command]
async fn set_detector_enabled(
    state: tauri::State<'_, AppState>,
    name: String,
    enabled: bool,
) -> Result<(), String> {
    // Always log so MSS/CISD/etc toggle paths can be verified from
    // /tmp/ict-radar.log without flipping a per-detector trace flag.
    tracing::info!(
        stage = "set_detector_enabled:cmd_received",
        %name, enabled,
        "tauri command set_detector_enabled invoked"
    );
    let mut engine = state.engine.lock();
    let before = engine.structures_count_for(&name);
    engine.set_detector_enabled(&name, enabled);
    let after = engine.structures_count_for(&name);
    tracing::info!(
        stage = "set_detector_enabled:done",
        %name, enabled,
        structures_before = before,
        structures_after = after,
        "set_detector_enabled finished"
    );
    Ok(())
}

#[tauri::command]
async fn set_detector_param(
    state: tauri::State<'_, AppState>,
    name: String,
    key: String,
    value: serde_json::Value,
) -> Result<(), String> {
    if name == "pdh_pdl" {
        tracing::info!(
            target: "pdh_pdl_trace",
            stage = "set_detector_param:cmd_received",
            %name, %key, %value,
            "tauri command set_detector_param invoked"
        );
    }
    let applied = {
        if name == "candidate" {
            // Candidate engine params are applied directly (not via IctEngine).
            state
                .engine_groups
                .read()
                .values()
                .map(|group| apply_candidate_param(&mut group.candidates.lock(), &key, &value))
                .sum()
        } else {
            let mut engine = state.engine.lock();
            engine.apply_detector_param(&name, &key, &value)
        }
    };
    // Persist to SQLite so watchlist switches and restarts re-apply
    // the user's saved thresholds to newly bootstrapped detectors.
    let val_str = match &value {
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if let Err(e) = state.store.upsert_detector_config(&name, &key, &val_str) {
        tracing::warn!(error = ?e, detector = %name, key = %key, "persist detector_config failed");
    }
    if applied == 0 {
        tracing::warn!(detector = %name, key = %key, "set_detector_param applied to 0 instances (unknown name or key)");
    } else {
        tracing::info!(detector = %name, key = %key, applied, "detector param updated");
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct DetectorParamPayload {
    key: String,
    value: serde_json::Value,
}

#[tauri::command]
async fn set_detector_params(
    state: tauri::State<'_, AppState>,
    name: String,
    updates: Vec<DetectorParamPayload>,
) -> Result<(), String> {
    let updates: Vec<(String, serde_json::Value)> = updates
        .into_iter()
        .map(|update| (update.key, update.value))
        .collect();
    let applied = if name == "candidate" {
        state
            .engine_groups
            .read()
            .values()
            .map(|group| {
                let mut candidate = group.candidates.lock();
                updates
                    .iter()
                    .map(|(key, value)| apply_candidate_param(&mut candidate, key, value))
                    .sum::<usize>()
            })
            .sum()
    } else {
        let mut engine = state.engine.lock();
        engine.apply_detector_params(&name, &updates)
    };
    // Persist each param so watchlist switches and restarts re-apply them.
    for (key, value) in &updates {
        let val_str = match value {
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if let Err(e) = state.store.upsert_detector_config(&name, key, &val_str) {
            tracing::warn!(error = ?e, detector = %name, %key, "persist detector_config failed");
        }
    }
    if applied == 0 {
        tracing::info!(detector = %name, "set_detector_params made no engine changes");
    } else {
        tracing::info!(detector = %name, applied, "detector params updated as one batch");
    }
    Ok(())
}

#[tauri::command]
async fn set_detector_params_config_only(
    state: tauri::State<'_, AppState>,
    name: String,
    updates: Vec<DetectorParamPayload>,
) -> Result<(), String> {
    // Cold-start only: apply persisted detector config without replaying
    // history, so the hydrated structures (PO3 etc.) survive until the
    // `seed_complete` re-fetch delivers them. See
    // `apply_detector_params_config_only` for rationale.
    let updates: Vec<(String, serde_json::Value)> = updates
        .into_iter()
        .map(|update| (update.key, update.value))
        .collect();
    let applied = {
        let mut engine = state.engine.lock();
        engine.apply_detector_params_config_only(&name, &updates)
    };
    // Persist each param so watchlist switches and restarts re-apply them.
    for (key, value) in &updates {
        let val_str = match value {
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if let Err(e) = state.store.upsert_detector_config(&name, key, &val_str) {
            tracing::warn!(error = ?e, detector = %name, %key, "persist detector_config failed");
        }
    }
    tracing::info!(detector = %name, applied, "cold-start detector config applied (no replay)");
    Ok(())
}

// ---- M5 watchlist + SMT commands ------------------------------------------

#[tauri::command]
async fn list_watchlists(state: tauri::State<'_, AppState>) -> Result<Vec<Watchlist>, String> {
    Ok(state.config.read().watchlists.clone())
}

#[tauri::command]
async fn get_active_watchlist(
    state: tauri::State<'_, AppState>,
) -> Result<Option<Watchlist>, String> {
    Ok(state.active_watchlist.lock().await.clone())
}

#[tauri::command]
async fn create_watchlist(
    state: tauri::State<'_, AppState>,
    sub_manager: tauri::State<'_, Arc<AsyncMutex<SubManager>>>,
    app: tauri::AppHandle,
    input: WatchlistInput,
) -> Result<Watchlist, String> {
    let wl = Watchlist {
        id: new_id(),
        name: input.name,
        symbols: input.symbols,
        correlations: input.correlations,
    };
    wl.validate().map_err(|e| e)?;
    {
        let mut cfg = state.config.write();
        cfg.watchlists.push(wl.clone());
        cfg.save().map_err(|e| format!("save config: {e}"))?;
    }
    reconcile_watchlist_runtime(&state, &sub_manager, &app).await?;
    Ok(wl)
}

#[tauri::command]
async fn update_watchlist(
    state: tauri::State<'_, AppState>,
    sub_manager: tauri::State<'_, Arc<AsyncMutex<SubManager>>>,
    app: tauri::AppHandle,
    id: String,
    input: WatchlistInput,
) -> Result<Watchlist, String> {
    let wl = Watchlist {
        id: id.clone(),
        name: input.name,
        symbols: input.symbols,
        correlations: input.correlations,
    };
    wl.validate().map_err(|e| e)?;
    {
        let mut cfg = state.config.write();
        let target = cfg
            .watchlists
            .iter_mut()
            .find(|w| w.id == id)
            .ok_or_else(|| format!("watchlist {id} not found"))?;
        *target = wl.clone();
        cfg.save().map_err(|e| format!("save config: {e}"))?;
    }
    reconcile_watchlist_runtime(&state, &sub_manager, &app).await?;
    Ok(wl)
}

#[tauri::command]
async fn delete_watchlist(
    state: tauri::State<'_, AppState>,
    sub_manager: tauri::State<'_, Arc<AsyncMutex<SubManager>>>,
    app: tauri::AppHandle,
    id: String,
) -> Result<(), String> {
    {
        let mut cfg = state.config.write();
        let before = cfg.watchlists.len();
        cfg.watchlists.retain(|w| w.id != id);
        if cfg.watchlists.len() == before {
            return Err(format!("watchlist {id} not found"));
        }
        if cfg.watchlists.is_empty() {
            cfg.watchlists = merge_defaults(&[]);
        }
        if cfg.active_watchlist_id.as_deref() == Some(&id) {
            cfg.active_watchlist_id = cfg.watchlists.first().map(|watchlist| watchlist.id.clone());
        }
        cfg.save().map_err(|e| format!("save config: {e}"))?;
    }
    reconcile_watchlist_runtime(&state, &sub_manager, &app).await?;
    let active = {
        let cfg = state.config.read();
        cfg.active_watchlist_id.as_deref().and_then(|active_id| {
            cfg.watchlists
                .iter()
                .find(|watchlist| watchlist.id == active_id)
                .cloned()
        })
    };
    if active.is_some() {
        *state.active_watchlist.lock().await = active;
    }
    Ok(())
}

#[tauri::command]
async fn restore_default_watchlists(
    state: tauri::State<'_, AppState>,
    sub_manager: tauri::State<'_, Arc<AsyncMutex<SubManager>>>,
    app: tauri::AppHandle,
) -> Result<Vec<Watchlist>, String> {
    {
        let mut cfg = state.config.write();
        cfg.watchlists = merge_defaults(&cfg.watchlists);
        cfg.save().map_err(|e| format!("save config: {e}"))?;
    }
    reconcile_watchlist_runtime(&state, &sub_manager, &app).await?;
    Ok(state.config.read().watchlists.clone())
}

/// Reconcile config-backed strategy groups without restarting the app.
/// Group switching remains view-only; this path is used only by explicit
/// create/update/delete/restore commands. The shared market-data manager
/// performs a set diff, so DXY remains exactly one subscription.
async fn reconcile_watchlist_runtime(
    state: &tauri::State<'_, AppState>,
    sub_manager: &tauri::State<'_, Arc<AsyncMutex<SubManager>>>,
    app: &tauri::AppHandle,
) -> Result<(), String> {
    let (desired, feishu_notify_default) = {
        let cfg = state.config.read();
        (cfg.watchlists.clone(), cfg.alerts.feishu.enabled)
    };
    let desired_ids: std::collections::HashSet<String> = desired
        .iter()
        .map(|watchlist| watchlist.id.clone())
        .collect();

    let mut groups_to_replay = Vec::new();
    {
        let mut groups = state.engine_groups.write();
        groups.retain(|id, _| desired_ids.contains(id));

        for watchlist in &desired {
            let unchanged = groups
                .get(&watchlist.id)
                .is_some_and(|group| group.watchlist == *watchlist);
            if unchanged {
                continue;
            }

            let mut smt_engine = SmtEngine::new();
            smt_engine.set_watchlist(watchlist.clone());
            let smt = Arc::new(parking_lot::Mutex::new(smt_engine));
            let candidates = Arc::new(parking_lot::Mutex::new(CandidateEngine::new()));
            candidates.lock().set_watchlist(watchlist.id.clone());
            let alerts = Arc::new(parking_lot::Mutex::new(
                AlertEngine::with_store_and_global_cooldown(
                    state.store.clone(),
                    state.global_alert_cooldown.clone(),
                ),
            ));
            {
                let mut alert_engine = alerts.lock();
                alert_engine.set_feishu_notify(feishu_notify_default);
                alert_engine.add_channel(Box::new(InboxChannel::new(
                    state.store.clone(),
                    app.clone(),
                )));
                alert_engine.add_channel(Box::new(DesktopNotifyChannel::new(app.clone())));
                alert_engine.add_channel(Box::new(FeishuNotifyChannel::new(
                    state.feishu_sender.clone(),
                )));
            }
            let group = Arc::new(EngineGroup {
                watchlist: watchlist.clone(),
                smt,
                candidates,
                alerts,
                llm_decisions: state.llm_decisions.clone(),
            });
            groups.insert(watchlist.id.clone(), group.clone());
            groups_to_replay.push(group);
        }
    }

    let desired_symbols = monitored_symbol_union(&desired);
    let preflight_now = now_ms();
    {
        let _history_guard = state.history_fetch_lock.lock().await;
        if let Err(error) = backfill_native_h4_for_incomplete_windows(
            &state.store,
            &state.tv_client,
            &desired_symbols,
            preflight_now,
        )
        .await
        {
            tracing::warn!(?error, "runtime native H4 fallback preflight failed");
        }
    }
    for symbol in &desired_symbols {
        match canonicalize_forex_h4_from_h1(&state.store, symbol, preflight_now) {
            Ok(Some(rebuilt)) => tracing::info!(
                %symbol,
                rebuilt,
                "runtime group H4 canonicalized from retained H1"
            ),
            Ok(None) => tracing::warn!(
                %symbol,
                "runtime group H4 preflight skipped: insufficient canonical H1 coverage"
            ),
            Err(error) => tracing::warn!(
                ?error,
                %symbol,
                "runtime group H4 canonicalization failed"
            ),
        }
    }
    bootstrap_detectors(&state.engine, &desired_symbols);
    seed_symbol_history(&state.engine, &state.store, &desired_symbols);
    let mut subscribed_symbols = desired_symbols.clone();
    for symbol in &state.config.read().chart_symbols {
        if !subscribed_symbols.contains(symbol) { subscribed_symbols.push(symbol.clone()); }
    }
    sub_manager.lock().await.set_symbols(&subscribed_symbols);

    groups_to_replay.sort_by(|a, b| a.watchlist.id.cmp(&b.watchlist.id));
    for group in groups_to_replay {
        let started = Instant::now();
        load_and_apply_detector_config(
            &state.engine,
            &state.store,
            &group.candidates,
            &group.alerts,
        );
        hydrate_smt(
            &group.smt,
            &state.store,
            &group.watchlist.id,
            &group.watchlist.symbols,
        );
        replay_smt_history(
            &group.smt,
            &group.candidates,
            &group.alerts,
            group.llm_decisions.as_ref(),
            &state.engine,
            &state.store,
            app,
            &group.watchlist.symbols,
        );
        replay_candidates(
            &group.smt,
            &group.candidates,
            &group.alerts,
            &state.store,
            &group.watchlist.symbols,
        );
        tracing::info!(
            watchlist_id = %group.watchlist.id,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "M6c runtime group reconciliation complete"
        );
    }

    let _ = app.emit("seed_complete", ());
    Ok(())
}

#[tauri::command]
async fn default_correlation_cmd(
    symbol_a: String,
    symbol_b: String,
) -> Result<WlDefaultCorrelation, String> {
    Ok(wl_default_correlation(&symbol_a, &symbol_b))
}

/// Stable user-facing identity for retries of the same SMT episode.  Attempt
/// IDs intentionally include SMT-K time for replay/audit precision; the UI
/// identity instead follows the immutable PDA + HTF liquidity reference.
fn smt_episode_key(smt: &SmtDivergence) -> String {
    let pda_id = smt
        .htf_pda_ref
        .as_ref()
        .map(|pda| pda.id.as_str())
        .unwrap_or("no-pda");
    let sweeper_ref_ts = smt
        .liquidity_refs
        .iter()
        .find(|reference| reference.symbol == smt.sweeper_symbol)
        .map(|reference| reference.ref_ts)
        .unwrap_or(smt.observation_window.0);
    format!(
        "{}|{}|{}|{}|{:?}|{}|{}",
        smt.watchlist_id,
        smt.sweeper_symbol,
        smt.context_timeframe.tag(),
        smt.comparison_timeframe.tag(),
        smt.candidate_direction,
        pda_id,
        sweeper_ref_ts,
    )
}

fn sweeper_state(smt: &SmtDivergence) -> SmtDetectionState {
    smt.chains
        .iter()
        .find(|chain| chain.symbol == smt.sweeper_symbol)
        .map(|chain| chain.detection_state)
        .unwrap_or(SmtDetectionState::Invalidated)
}

fn public_smt_attempt(smt: &SmtDivergence) -> bool {
    if smt.rule_version != CURRENT_RULE_VERSION || smt.htf_pda_ref.is_none() {
        return false;
    }
    // A terminal partial-parent hypothesis was never a confirmed HTF SMT.
    // It remains in detector logs/SQLite audit but not in the three-table
    // product funnel. Confirmed SMTs that later fail C2/C3 remain visible X.
    !(sweeper_state(smt) == SmtDetectionState::Invalidated && !smt.htf_confirmed)
}

fn smt_attempt_rank(smt: &SmtDivergence) -> (u8, u8, i64) {
    let state_rank = match sweeper_state(smt) {
        SmtDetectionState::C3Entry => 4,
        SmtDetectionState::C2Confirmed => 3,
        SmtDetectionState::SmtKDetected => 2,
        SmtDetectionState::Invalidated => 1,
    };
    let smt_k_ts = smt
        .chains
        .iter()
        .find(|chain| chain.symbol == smt.sweeper_symbol)
        .map(|chain| chain.smt_k_candle.ts)
        .unwrap_or(i64::MIN);
    (state_rank, u8::from(smt.htf_confirmed), smt_k_ts)
}

fn canonical_public_smts(smts: Vec<SmtDivergence>) -> Vec<SmtDivergence> {
    let mut by_episode: std::collections::HashMap<String, SmtDivergence> =
        std::collections::HashMap::new();
    for smt in smts.into_iter().filter(public_smt_attempt) {
        let key = smt_episode_key(&smt);
        let replace = by_episode
            .get(&key)
            .is_none_or(|existing| smt_attempt_rank(&smt) > smt_attempt_rank(existing));
        if replace {
            by_episode.insert(key, smt);
        }
    }
    by_episode.into_values().collect()
}

fn selected_engine_groups(
    groups: &EngineGroups,
    watchlist_id: Option<&str>,
) -> Result<Vec<Arc<EngineGroup>>, String> {
    let groups = groups.read();
    if let Some(id) = watchlist_id {
        return groups
            .get(id)
            .cloned()
            .map(|group| vec![group])
            .ok_or_else(|| format!("watchlist {id} not found"));
    }
    let mut selected: Vec<_> = groups.values().cloned().collect();
    selected.sort_by(|a, b| a.watchlist.id.cmp(&b.watchlist.id));
    Ok(selected)
}

fn public_smts_for_group(group: &EngineGroup, store: &SqliteStore) -> Vec<SmtDivergence> {
    let mut mem_smts = group.smt.lock().list_active();
    if let Ok(db_smts) = store.list_all_smt(&group.watchlist.id) {
        let db_map: std::collections::HashMap<String, SmtDivergence> =
            db_smts.into_iter().map(|s| (s.id.clone(), s)).collect();
        for mem in &mut mem_smts {
            if let Some(db) = db_map.get(&mem.id) {
                let db_invalidated = db.chains.iter().any(|chain| {
                    chain.symbol == db.sweeper_symbol
                        && chain.detection_state == SmtDetectionState::Invalidated
                });
                if db_invalidated {
                    *mem = db.clone();
                } else {
                    if mem.htf_pda_ref.is_none() && db.htf_pda_ref.is_some() {
                        mem.htf_pda_ref = db.htf_pda_ref.clone();
                    }
                    if mem.mtf_ref_candle.is_none() && db.mtf_ref_candle.is_some() {
                        mem.mtf_ref_candle = db.mtf_ref_candle.clone();
                    }
                }
            }
        }
        let mem_ids: std::collections::HashSet<String> =
            mem_smts.iter().map(|s| s.id.clone()).collect();
        mem_smts.extend(
            db_map
                .into_values()
                .filter(|smt| !mem_ids.contains(&smt.id)),
        );
    }
    canonical_public_smts(mem_smts)
}

#[tauri::command]
async fn list_smt(
    state: tauri::State<'_, AppState>,
    watchlist_id: Option<String>,
) -> Result<Vec<SmtDivergence>, String> {
    // Inbox is the audit history for the current formation contract: include
    // its invalidated SMTs so every visible Candidate/Alert remains traceable,
    // while keeping superseded rule versions as SQLite-only internal history.
    let mut result = Vec::new();
    for group in selected_engine_groups(&state.engine_groups, watchlist_id.as_deref())? {
        result.extend(public_smts_for_group(&group, &state.store));
    }
    result.sort_by_key(|smt| std::cmp::Reverse(smt.observation_window.1));
    Ok(result)
}

/// Exact historical lookup for an existing inbox record. This deliberately
/// bypasses public episode/HTF filters without changing the SMT Inbox itself.
#[tauri::command]
async fn get_smt_snapshot(
    state: tauri::State<'_, AppState>,
    smt_id: String,
    watchlist_id: String,
) -> Result<Option<SmtDivergence>, String> {
    selected_engine_groups(&state.engine_groups, Some(&watchlist_id))?;
    let smts = state
        .store
        .list_all_smt(&watchlist_id)
        .map_err(|e| e.to_string())?;
    Ok(smts.into_iter().find(|smt| smt.id == smt_id))
}

#[tauri::command]
async fn list_runtime_logs() -> Result<Vec<runtime_log::LogEntry>, String> {
    let path = db_path()
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("logs/runtime.log");
    tauri::async_runtime::spawn_blocking(move || runtime_log::read_tail(&path))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

fn sort_alert_records_stably(records: &mut [AlertRecord]) {
    records.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.watchlist_id.cmp(&b.watchlist_id))
            .then_with(|| a.validation_symbol.cmp(&b.validation_symbol))
            .then_with(|| a.id.cmp(&b.id))
    });
}

#[tauri::command]
async fn list_alerts(
    state: tauri::State<'_, AppState>,
    limit: Option<i64>,
    watchlist_id: Option<String>,
) -> Result<Vec<AlertRecord>, String> {
    let selected = selected_engine_groups(&state.engine_groups, watchlist_id.as_deref())?;
    let selected_ids: std::collections::HashSet<String> = selected
        .iter()
        .map(|group| group.watchlist.id.clone())
        .collect();
    let source_smts: std::collections::HashMap<String, SmtDivergence> = selected
        .iter()
        .flat_map(|group| {
            state
                .store
                .list_all_smt(&group.watchlist.id)
                .unwrap_or_default()
        })
        .map(|smt| (smt.id.clone(), smt))
        .collect();
    let candidates: std::collections::HashMap<String, CandidateSetup> = state
        .store
        .list_all_candidates_for_inbox()
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|candidate| selected_ids.contains(&candidate.watchlist_id))
        .map(|candidate| (candidate.id.clone(), candidate))
        .collect();

    let alerts = state
        .store
        .list_alerts(None)
        .map_err(|error| error.to_string())?;
    let mut result = project_alert_history(alerts, &candidates, &source_smts);
    sort_alert_records_stably(&mut result);
    if let Some(limit) = limit {
        result.truncate(limit.max(0) as usize);
    }
    Ok(result)
}

// Durable C2 facts stay visible after a source becomes invalid or is revised.
// Current candidate/rule/group routing remains unchanged; only lifecycle is refreshed.
fn project_alert_history(
    alerts: Vec<AlertRecord>,
    candidates: &std::collections::HashMap<String, CandidateSetup>,
    source_smts: &std::collections::HashMap<String, SmtDivergence>,
) -> Vec<AlertRecord> {
    let mut result = Vec::new();
    for mut alert in alerts
        .into_iter()
        .filter(|a| a.trigger == AlertTrigger::C2Confirmed)
    {
        let Some(candidate) = candidates.get(&alert.candidate_id) else {
            continue;
        };
        let Some(symbol) = alert.validation_symbol.clone() else {
            continue;
        };
        let source_smt = source_smts.get(&alert.smt_id);
        let chain =
            source_smt.and_then(|smt| smt.chains.iter().find(|chain| chain.symbol == symbol));
        // Never replace the fired C2 timestamp/case/SMT-K with a revised chain.
        if let Some(chain) = chain.filter(|chain| {
            chain
                .c2_candle
                .as_ref()
                .is_some_and(|c2| c2.ts == alert.c2_candle_ts)
        }) {
            alert.c3_candle_ts = chain
                .c3_candle
                .as_ref()
                .map(|c| c.ts)
                .or(alert.c3_candle_ts);
        }
        alert.setup_status = if candidate.invalidated_symbols.contains(&symbol) {
            ict_monitor::candidate::SetupStatus::Invalidated
        } else {
            candidate.setup_status
        };
        alert.invalidation_reason = candidate.symbol_invalidation_reasons.get(&symbol).copied();
        // Backward compatibility for candidates persisted before per-symbol
        // reasons were recorded. A shared invalidation still has a useful
        // aggregate reason; a legacy single-leg invalidation remains generic.
        if alert.setup_status == ict_monitor::candidate::SetupStatus::Invalidated
            && alert.invalidation_reason.is_none()
            && candidate.setup_status == ict_monitor::candidate::SetupStatus::Invalidated
        {
            alert.invalidation_reason = candidate.expiry_reason;
        }
        if let Some(validation) = candidate
            .validations
            .iter()
            .find(|validation| validation.symbol == symbol)
        {
            alert.validation_kind = Some(validation.kind);
            alert.validation_ts = Some(validation.ts);
            alert.validation_direction = Some(validation.direction);
        }

        let revised_or_invalid = source_smt.is_some_and(|smt| {
            sweeper_state(smt) == SmtDetectionState::Invalidated
                || chain.is_none_or(|chain| {
                    chain
                        .c2_candle
                        .as_ref()
                        .is_none_or(|c2| c2.ts != alert.c2_candle_ts)
                })
                || candidate.context_pda_id.as_ref() != smt.htf_pda_ref.as_ref().map(|pda| &pda.id)
        });
        if revised_or_invalid {
            alert.setup_status = ict_monitor::candidate::SetupStatus::Invalidated;
            alert.invalidation_reason = alert
                .invalidation_reason
                .or(Some(ict_monitor::candidate::ExpiryReason::SmtInvalidated));
        }
        // One row per persisted alert; later attempts cannot erase an earlier alert.
        result.push(alert);
    }
    result
}

#[tauri::command]
async fn list_reversals(
    state: tauri::State<'_, AppState>,
    limit: Option<i64>,
    watchlist_id: Option<String>,
) -> Result<Vec<AlertRecord>, String> {
    let candidates = state
        .store
        .list_all_candidates_for_inbox()
        .map_err(|e| e.to_string())?;
    let selected = selected_engine_groups(&state.engine_groups, watchlist_id.as_deref())?;
    let selected_ids: std::collections::HashSet<String> = selected
        .iter()
        .map(|group| group.watchlist.id.clone())
        .collect();
    let source_smts: std::collections::HashMap<String, SmtDivergence> = selected
        .iter()
        .flat_map(|group| {
            state
                .store
                .list_all_smt(&group.watchlist.id)
                .unwrap_or_default()
        })
        .map(|smt| (smt.id.clone(), smt))
        .collect();
    let mut candidates_by_id: std::collections::HashMap<String, CandidateSetup> =
        std::collections::HashMap::new();
    for candidate in candidates.into_iter().filter(|candidate| {
        selected_ids.contains(&candidate.watchlist_id)
            && candidate
                .context_pda_id
                .as_ref()
                .is_some_and(|candidate_pda| {
                    source_smts
                        .get(&candidate.smt_id)
                        .and_then(|smt| smt.htf_pda_ref.as_ref())
                        .is_some_and(|smt_pda| &smt_pda.id == candidate_pda)
                })
    }) {
        candidates_by_id.insert(candidate.id.clone(), candidate);
    }
    let alerts = state.store.list_alerts(None).map_err(|e| e.to_string())?;
    let mut by_episode_symbol: std::collections::HashMap<String, AlertRecord> =
        std::collections::HashMap::new();
    for alert in alerts.into_iter().filter(|alert| {
        if alert.trigger != AlertTrigger::Validated {
            return false;
        }
        let Some(candidate) = candidates_by_id.get(&alert.candidate_id) else {
            return false;
        };
        let (Some(symbol), Some(ts), Some(kind), Some(direction)) = (
            alert.validation_symbol.as_ref(),
            alert.validation_ts,
            alert.validation_kind,
            alert.validation_direction,
        ) else {
            return false;
        };
        candidate.validations.iter().any(|validation| {
            &validation.symbol == symbol
                && validation.ts == ts
                && validation.kind == kind
                && validation.direction == direction
        }) || (candidate.validations.is_empty()
            && candidate.validation_symbol.as_ref() == Some(symbol)
            && candidate.validation_ts == Some(ts)
            && candidate.validation_kind == Some(kind)
            && candidate.validation_direction == Some(direction))
    }) {
        let Some(source_smt) = source_smts.get(&alert.smt_id) else {
            continue;
        };
        let key = format!(
            "{}|{}",
            smt_episode_key(source_smt),
            alert.validation_symbol.as_deref().unwrap_or("unknown")
        );
        let replace = by_episode_symbol
            .get(&key)
            .is_none_or(|existing| alert.created_at > existing.created_at);
        if replace {
            by_episode_symbol.insert(key, alert);
        }
    }
    let mut result: Vec<_> = by_episode_symbol.into_values().collect();
    sort_alert_records_stably(&mut result);
    if let Some(limit) = limit {
        result.truncate(limit.max(0) as usize);
    }
    Ok(result)
}

#[tauri::command]
async fn set_alert_param(
    state: tauri::State<'_, AppState>,
    key: String,
    value: serde_json::Value,
) -> Result<(), String> {
    for group in state.engine_groups.read().values() {
        apply_alert_param(&mut group.alerts.lock(), &key, &value);
    }
    let val_str = match &value {
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if let Err(e) = state.store.upsert_detector_config("alert", &key, &val_str) {
        tracing::warn!(error = ?e, "persist alert config failed");
    }
    Ok(())
}

#[tauri::command]
async fn clear_alerts(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.store.clear_alerts().map_err(|e| e.to_string())
}

#[tauri::command]
async fn test_desktop_notification(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    // Respect both the master alert toggle and the desktop notify
    // toggle (same gates as real alerts).
    let (enabled, desktop_notify) = state
        .engine_groups
        .read()
        .values()
        .next()
        .map(|group| {
            let alerts = group.alerts.lock();
            (alerts.enabled(), alerts.desktop_notify_enabled())
        })
        .unwrap_or((false, false));
    if !enabled {
        return Err("告警总开关已关闭，请在 Alerts 设置中打开后再测试".into());
    }
    if !desktop_notify {
        return Err("桌面通知已关闭，请在 Alerts 设置中打开后再测试".into());
    }
    ict_monitor::alert::send_desktop_notification(
        &app,
        "测试通知 / Test Alert",
        "M6b 告警管道已接通",
    )
}

#[tauri::command]
async fn test_feishu_notification(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let (enabled, feishu_notify) = state
        .engine_groups
        .read()
        .values()
        .next()
        .map(|group| {
            let alerts = group.alerts.lock();
            (alerts.enabled(), alerts.feishu_notify_enabled())
        })
        .unwrap_or((false, false));
    if !enabled {
        return Err("告警总开关已关闭，请在 Alerts 设置中打开后再测试".into());
    }
    if !feishu_notify {
        return Err("飞书通知已关闭，请在 Alerts 设置中打开后再测试".into());
    }
    state.feishu_sender.send_test_message().await
}

fn apply_alert_param(alerts: &mut AlertEngine, key: &str, value: &serde_json::Value) {
    match key {
        "schema_v1" => { /* migration marker, skip */ }
        "enabled" => {
            if let Some(b) = value.as_bool() {
                alerts.set_enabled(b);
            }
        }
        "desktop_notify_enabled" => {
            if let Some(b) = value.as_bool() {
                alerts.set_desktop_notify(b);
            }
        }
        "feishu_notify_enabled" => {
            if let Some(b) = value.as_bool() {
                alerts.set_feishu_notify(b);
            }
        }
        "cooldown_seconds" => {
            if let Some(n) = value.as_i64() {
                alerts.set_cooldown_seconds(n);
            }
        }
        _ => {
            tracing::warn!(key, "unknown alert param");
        }
    }
}

#[tauri::command]
async fn list_candidates(
    state: tauri::State<'_, AppState>,
    watchlist_id: Option<String>,
) -> Result<Vec<CandidateSetup>, String> {
    // Candidate is a strict historical subset of PDA-SMT. Keep every
    // lifecycle state, but never expose an orphan or a row whose PDA does
    // not match the immutable source SMT snapshot.
    let selected = selected_engine_groups(&state.engine_groups, watchlist_id.as_deref())?;
    let selected_ids: std::collections::HashSet<String> = selected
        .iter()
        .map(|group| group.watchlist.id.clone())
        .collect();
    let source_smts: std::collections::HashMap<String, SmtDivergence> = selected
        .iter()
        .flat_map(|group| {
            state
                .store
                .list_all_smt(&group.watchlist.id)
                .unwrap_or_default()
        })
        .map(|smt| (smt.id.clone(), smt))
        .collect();
    match state.store.list_all_candidates_for_inbox() {
        Ok(db_cands) if !db_cands.is_empty() => {
            let mut by_episode: std::collections::HashMap<String, CandidateSetup> =
                std::collections::HashMap::new();
            for candidate in db_cands.into_iter().filter(|candidate| {
                selected_ids.contains(&candidate.watchlist_id)
                    && candidate
                        .context_pda_id
                        .as_ref()
                        .is_some_and(|candidate_pda| {
                            source_smts
                                .get(&candidate.smt_id)
                                .and_then(|smt| smt.htf_pda_ref.as_ref())
                                .is_some_and(|smt_pda| &smt_pda.id == candidate_pda)
                        })
            }) {
                let Some(source_smt) = source_smts.get(&candidate.smt_id) else {
                    continue;
                };
                if !public_smt_attempt(source_smt) {
                    continue;
                }
                let episode = smt_episode_key(source_smt);
                let replace = by_episode.get(&episode).is_none_or(|existing| {
                    candidate.created_at > existing.created_at
                        || (candidate.created_at == existing.created_at
                            && candidate.id > existing.id)
                });
                if replace {
                    by_episode.insert(episode, candidate);
                }
            }
            Ok(by_episode.into_values().collect())
        }
        Ok(_) => Ok(selected
            .iter()
            .flat_map(|group| group.candidates.lock().list_active())
            .filter(|candidate| source_smts.contains_key(&candidate.smt_id))
            .collect()),
        Err(e) => {
            tracing::warn!(error = %e, "list_all_candidates_for_inbox failed, falling back to in-memory");
            Ok(selected
                .iter()
                .flat_map(|group| group.candidates.lock().list_active())
                .into_iter()
                .filter(|candidate| source_smts.contains_key(&candidate.smt_id))
                .collect())
        }
    }
}

#[tauri::command]
async fn list_decisions(
    state: tauri::State<'_, AppState>,
    candidate_id: String,
) -> Result<Vec<DecisionLogEntry>, String> {
    state
        .store
        .list_decisions(&candidate_id)
        .map_err(|e| format!("list_decisions: {e}"))
}

#[tauri::command]
async fn list_llm_decision_items(
    state: tauri::State<'_, AppState>,
    watchlist_id: String,
    limit: Option<usize>,
) -> Result<Vec<LlmDecisionListItem>, String> {
    let mut items: Vec<_> = state
        .store
        .list_llm_decisions_for_watchlist(&watchlist_id)
        .map_err(|e| format!("list_llm_decision_items: {e}"))?
        .into_iter()
        .map(LlmDecisionListItem::from_log_entry)
        .collect();
    sort_llm_decision_items(&mut items);
    items.truncate(limit.unwrap_or(500).min(2_000));
    Ok(items)
}

#[tauri::command]
async fn set_active_watchlist(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<Watchlist, String> {
    set_viewing_group_inner(&state, id).await
}

#[tauri::command]
async fn set_viewing_group(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<Watchlist, String> {
    set_viewing_group_inner(&state, id).await
}

async fn set_viewing_group_inner(
    state: &tauri::State<'_, AppState>,
    id: String,
) -> Result<Watchlist, String> {
    let wl = {
        let cfg = state.config.read();
        cfg.watchlists
            .iter()
            .find(|w| w.id == id)
            .cloned()
            .ok_or_else(|| format!("watchlist {id} not found"))?
    };

    // M6c: a group switch is view state only. All engines and the union
    // subscription remain live, so switching cannot drop or reseed signals.
    {
        let mut aw = state.active_watchlist.lock().await;
        *aw = Some(wl.clone());
    }
    {
        let mut cfg = state.config.write();
        cfg.active_watchlist_id = Some(id.clone());
        if let Err(e) = cfg.save() {
            tracing::warn!(error = ?e, "persist active_watchlist_id failed");
        }
    }

    Ok(wl)
}

/// Seed engine history from SQLite for symbols (called on watchlist switch).
/// Idempotent: re-feeding bars already in the engine's history is a no-op
/// because the continuity guard skips any bar with ts <= the last seen bar.
/// Bars are fed oldest-first (ASC) so new symbols build history correctly.
fn seed_symbol_history(engine: &EngineHandle, store: &SqliteStore, symbols: &[String]) {
    let tfs = [
        Timeframe::M1,
        Timeframe::M5,
        Timeframe::M15,
        Timeframe::M30,
        Timeframe::H1,
        Timeframe::H4,
        Timeframe::D1,
        Timeframe::W1,
    ];
    for sym in symbols {
        // Skip symbols that already have bar history in the engine
        // (cold-start seed or a previous warm-start). A freshly
        // bootstrapped symbol has detector buckets but no bars, so it
        // must still be seeded. This runs from the per-symbol
        // warm-start (before the live TV feed starts) so the long sync
        // engine lock never blocks live bar processing - the bug that
        // froze the chart when this ran concurrently in spawn_seeding.
        let already_seeded = engine.lock().has_bar_history(sym);
        if already_seeded {
            tracing::info!(%sym, "seed_symbol_history: skipping (already seeded)");
            continue;
        }
        for tf in tfs {
            // 1m needs 3000 (≈50h) so PdhPdlDetector::flush can scan a
            // full previous civil-day window; other TFs use 500.
            let limit: i64 = if matches!(tf, Timeframe::M1) {
                3000
            } else {
                500
            };
            let bars = match store.recent_bars(sym, tf, limit) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if bars.is_empty() {
                continue;
            }
            let mut eng = engine.lock();
            for bar in bars.iter() {
                eng.seed_closed_bar(bar);
            }
            tracing::info!(%sym, tf = %tf.tag(), n = bars.len(), "watchlist-switch seed");
        }
    }
}

fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    format!("wl-{now:x}")
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Append-only sink for front-end log forwarding. The webview script
/// patches `console.log/warn/error/debug` to mirror every call here so
/// we have a single grep-able file (`/tmp/ict-radar-ui.log`) for both
/// halves of the app, removing the need to copy-paste devtools output
/// during M3 acceptance debugging.
#[tauri::command]
async fn ui_log(level: String, args: Vec<serde_json::Value>) -> Result<(), String> {
    use std::io::Write;
    let path = "/tmp/ict-radar-ui.log";
    let line = {
        let parts: Vec<String> = args
            .iter()
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        format!("{ts} [{level}] {}\n", parts.join(" "))
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
    Ok(())
}

/// Persist structures created or updated while replay broadcasting was
/// suppressed. Live events are persisted by the emit task above.
///
/// This used to snapshot and UPSERT the entire engine map. Long-running user
/// databases can contain 300k+ historical structures, so a single history
/// repair monopolised SQLite for minutes and starved the live bar writer.
fn persist_engine_after_step(store: &SqliteStore, engine: &EngineHandle, prune: bool) {
    let now = now_ms();
    let dirty: Vec<_> = {
        let eng = engine.lock();
        eng.take_seed_dirty_structures()
    };
    if dirty.is_empty() && !prune {
        return;
    }
    let mut kind_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for s in &dirty {
        *kind_counts.entry(s.kind_tag()).or_default() += 1;
    }
    tracing::info!(total = dirty.len(), counts = ?kind_counts, "persist replay structure delta");
    // Batch all upserts in a single transaction to avoid freezing the
    // DB with 30k separate auto-committed INSERTs.
    if let Err(e) = store.upsert_structures_batch(&dirty, now) {
        tracing::warn!(error = ?e, "batch upsert failed, falling back to per-item");
        for s in &dirty {
            if let Err(e) = store.upsert_structure(s, now) {
                tracing::warn!(error = ?e, id = %s.id(), "upsert structure failed");
            }
        }
    }

    // Prune SQLite rows that are no longer in the engine. Only do this
    // when prune=true (live TV historical batch). During cold-start /
    // warm-start seed, the engine may have lost structures because the
    // truncated seed window (500 bars) doesn't give detectors enough
    // context to maintain them. Pruning would permanently delete these
    // valid structures from SQLite. Skip prune during seed; the
    // emit-task will properly mark invalidated structures during live
    // bar processing.
    if prune {
        let keep_ids: std::collections::HashSet<String> = engine
            .lock()
            .structures
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        match store.prune_structures_not_in(&keep_ids) {
            Ok(n) if n > 0 => tracing::info!(
                pruned = n,
                kept = keep_ids.len(),
                "pruned stale sqlite structures"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = ?e, "prune_structures_not_in failed"),
        }
    }
}

/// Load persisted detector config from SQLite and apply to all engine
/// detectors. Called after `bootstrap_detectors` so newly registered
/// detector instances adopt the user's saved thresholds instead of
/// `Po3Config::default()` etc. This is critical for watchlist switches
/// where new symbols get fresh detectors with default config.

/// Apply a candidate engine parameter (§8.1).
fn apply_candidate_param(
    candidates: &mut CandidateEngine,
    key: &str,
    value: &serde_json::Value,
) -> usize {
    match key {
        "schema_v1" => 0, /* migration marker */
        "expiry_mtf_bars" => {
            if let Some(n) = value.as_u64() {
                candidates.set_expiry_mtf_bars(n as usize);
                tracing::info!(key, value = n, "candidate param applied");
                1
            } else {
                0
            }
        }
        _ => {
            tracing::warn!(key, "unknown candidate param");
            0
        }
    }
}

fn load_and_apply_detector_config(
    engine: &EngineHandle,
    store: &SqliteStore,
    candidates: &CandidateHandle,
    alerts: &AlertHandle,
) {
    let configs = match store.load_detector_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = ?e, "load_detector_config failed");
            return;
        }
    };
    let mut total_applied = 0usize;
    for (detector, params) in configs {
        let updates: Vec<(String, serde_json::Value)> = params
            .iter()
            .map(|(k, v)| {
                let json_val = if v == "true" {
                    serde_json::Value::Bool(true)
                } else if v == "false" {
                    serde_json::Value::Bool(false)
                } else if let Ok(n) = v.parse::<f64>() {
                    serde_json::json!(n)
                } else {
                    serde_json::Value::String(v.clone())
                };
                (k.clone(), json_val)
            })
            .collect();
        let applied = if detector == "candidate" {
            let mut cand = candidates.lock();
            let mut count = 0;
            for (k, v) in &updates {
                count += apply_candidate_param(&mut cand, k, v);
            }
            count
        } else if detector == "alert" {
            let mut al = alerts.lock();
            for (k, v) in &updates {
                apply_alert_param(&mut al, k, v);
            }
            updates.len()
        } else {
            let mut eng = engine.lock();
            eng.apply_detector_params_config_only(&detector, &updates)
        };
        tracing::info!(
            detector = %detector, applied, params = params.len(),
            "loaded detector config from sqlite"
        );
        total_applied += applied;
    }
    if total_applied > 0 {
        tracing::info!(total_applied, "detector config restored from sqlite");
    }
}

fn bootstrap_detectors(engine: &EngineHandle, symbols: &[String]) {
    let mut eng = engine.lock();
    ict_monitor::detector::bootstrap::register_production_detectors(&mut eng, symbols);
}

#[cfg(test)]
#[test]
fn durable_alert_history_survives_invalidated_and_removed_c2() {
    let cases: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/alert_history.json")).unwrap();
    for case in cases.as_array().unwrap() {
        let alert: AlertRecord = serde_json::from_value(case["alert"].clone()).unwrap();
        let candidate: CandidateSetup = serde_json::from_value(case["candidate"].clone()).unwrap();
        let smt: SmtDivergence = serde_json::from_value(case["smt"].clone()).unwrap();
        let original_time = alert.c2_candle_ts;
        let original_case = alert.c2_case;
        let original_created = alert.created_at;
        let mut another = alert.clone();
        another.id.push_str("-earlier");
        another.created_at -= 1;
        let candidates = [(candidate.id.clone(), candidate)].into_iter().collect();
        let smts = [(smt.id.clone(), smt)].into_iter().collect();
        let result = project_alert_history(vec![alert, another], &candidates, &smts);
        assert_eq!(
            result.len(),
            2,
            "later attempt must not hide earlier durable record"
        );
        assert_eq!(result[0].c2_candle_ts, original_time);
        assert_eq!(result[0].c2_case, original_case);
        assert_eq!(result[0].created_at, original_created);
        assert_eq!(
            result[0].setup_status,
            ict_monitor::candidate::SetupStatus::Invalidated
        );
        assert!(result[0].invalidation_reason.is_some());
    }
}
