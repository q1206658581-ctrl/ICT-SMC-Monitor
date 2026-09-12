//! TradingView WebSocket data source.
//!
//! Implements §3.1–3.3 of `docs/技术设计文档.md`:
//! * fetch a guest `auth_token` over HTTPS,
//! * open `wss://data.tradingview.com/socket.io/websocket?from=chart&date=...`,
//! * speak the `~m~<len>~m~<payload>` framing, echo `~h~N` heartbeats,
//! * drive the `set_auth_token` → `chart_create_session` → `quote_create_session`
//!   → `resolve_symbol` → `create_series` handshake,
//! * emit `BarEvent::Historical / BarUpdate / BarClosed`,
//! * exponential-backoff reconnect with auto-resubscribe.
//!
//! Live M1 data uses one union connection. Because the authenticated account
//! permits only one chart series per session, that series rotates across the
//! configured symbols while their quote subscriptions remain on the session.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use rand::distributions::Alphanumeric;
use rand::Rng;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
use tracing::{debug, info, warn};

use crate::data_source::MarketDataProvider;
use crate::types::{Bar, BarEvent, Timeframe};

#[path = "union_bars.rs"]
mod union_bars;
use union_bars::{apply_authoritative_bars, next_union_frame, publish_union_preview};

const WS_URL_TEMPLATE: &str =
    "wss://data.tradingview.com/socket.io/websocket?from=chart&date={build}";
const HOMEPAGE_URL: &str = "https://www.tradingview.com/";
const ORIGIN: &str = "https://data.tradingview.com";
const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
const DEFAULT_BUILD_DATE: &str = "2024_05_01-12_00";
const HISTORY_BARS: u32 = 5_000;
/// The logged-in TV account used by the desktop app allows only one chart
/// series per session. After every symbol has received its initial 5,000-bar
/// snapshot, reuse that sole series with a small rolling window. Closed bars
/// are therefore recovered even when a symbol had no quote tick while it was
/// not the active series.
const ROTATION_BARS: u32 = 10;
/// After the initial history seed, the sole chart series is only a periodic
/// authoritative correction path. `series_completed` means that the snapshot
/// was delivered; it is not a signal to recreate the next series immediately.
/// Pace the seven-symbol cycle so quote-driven live bars remain responsive and
/// the indicator pipeline is not flooded with the same ten bars hundreds of
/// times per minute.
const ROTATION_INTERVAL: Duration = Duration::from_secs(8);
/// TradingView normally sends heartbeat frames every few seconds.  A TCP
/// connection can nevertheless remain half-open after a laptop/network
/// transition, leaving the receiver pending forever and the UI showing a
/// frozen candle.  Force the outer subscription loop to reconnect when no
/// frame at all has arrived for this interval.
const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Quote updates can keep the union socket busy even after TradingView has
/// stopped serving the active chart series.  Give every rotating series its
/// own absolute deadline so unrelated quote frames cannot hide a stalled
/// `series_completed` response forever.
const SERIES_COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);
/// A one-shot history request can complete with only the provider's first
/// retained page (notably for newly-added symbols on D1/W1). Keep asking the
/// same series for older data until the requested depth is satisfied or the
/// provider proves that no older page exists.
const HISTORY_FETCH_TIMEOUT: Duration = Duration::from_secs(45);
const HISTORY_MAX_MORE_REQUESTS: usize = 8;
/// Quote sessions can publish several updates per second for every symbol.
/// Sending every tick through the full aggregation/indicator pipeline causes
/// an ever-growing queue once all M6c symbols are active. Keep accumulating
/// exact OHLC locally, but publish at most one in-progress update per symbol
/// per second. A new minute is always published immediately.
const QUOTE_EMIT_INTERVAL_MS: i64 = 1_000;

/// Public handle. Cheap to clone; spawns one background task per `subscribe_bars`.
#[derive(Clone, Debug, Default)]
pub struct TvClient {
    auth: TvAuth,
}

#[derive(Clone, Debug, Default)]
pub struct TvAuth {
    /// `sessionid` cookie from a logged-in tradingview.com tab.
    pub sessionid: Option<String>,
    /// Companion `sessionid_sign` cookie (required since 2023).
    pub sessionid_sign: Option<String>,
    /// Optional app-local HTTP CONNECT proxy.
    pub proxy_url: Option<String>,
}

impl TvClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_auth(auth: TvAuth) -> Self {
        Self { auth }
    }

    /// One-shot history fetcher: opens a fresh WS session, asks TradingView
    /// for up to `limit` bars at the given resolution, and tears the session
    /// down. Used by the Tauri `get_history` command to backfill higher-TF
    /// charts on demand without disturbing the live 1m subscription.
    pub async fn fetch_history(&self, symbol: &str, tf: Timeframe, limit: u32) -> Result<Vec<Bar>> {
        run_history_session(symbol, tf, &self.auth, limit).await
    }

    /// Subscribe several symbols through one TradingView chart session.
    ///
    /// TradingView limits the number of concurrent chart WebSockets. M6c has
    /// seven unique symbols, so opening one socket per symbol lets the first
    /// group connect while later groups remain permanently stale. A chart
    /// session natively supports multiple resolved symbols/series; expose
    /// that capability and keep emitting the existing symbol-bearing
    /// `BarEvent`s so downstream aggregation remains isolated per symbol.
    pub async fn subscribe_many_bars(
        &self,
        symbols: &[String],
        tf: Timeframe,
    ) -> Result<mpsc::Receiver<BarEvent>> {
        if symbols.is_empty() {
            bail!("at least one TradingView symbol is required");
        }
        let (tx, rx) = mpsc::channel::<BarEvent>(4096);
        let symbols = symbols.to_vec();
        let auth = self.auth.clone();
        tokio::spawn(async move {
            run_multi_subscription(symbols, tf, auth, tx).await;
        });
        Ok(rx)
    }
}

#[async_trait]
impl MarketDataProvider for TvClient {
    async fn subscribe_bars(
        &self,
        symbol: &str,
        tf: Timeframe,
    ) -> Result<mpsc::Receiver<BarEvent>> {
        let (tx, rx) = mpsc::channel::<BarEvent>(1024);
        let symbol = symbol.to_string();
        let auth = self.auth.clone();
        tokio::spawn(async move {
            run_subscription(symbol, tf, auth, tx).await;
        });
        Ok(rx)
    }
}

/// Loop forever: connect, drive the session, tear down on error, back off, retry.
async fn run_subscription(symbol: String, tf: Timeframe, auth: TvAuth, tx: mpsc::Sender<BarEvent>) {
    let mut backoff_ms: u64 = 1_000;
    loop {
        match run_session(&symbol, tf, &auth, &tx).await {
            Ok(()) => {
                info!(symbol = %symbol, "tv session ended cleanly; reconnecting");
                backoff_ms = 1_000;
            }
            Err(err) => {
                warn!(symbol = %symbol, error = ?err, "tv session ended; will reconnect");
            }
        }
        if tx.is_closed() {
            info!(symbol = %symbol, "consumer dropped; stopping subscription");
            return;
        }
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms * 2).min(30_000);
    }
}

/// Loop forever for a union subscription. One connection carries all M6c
/// quote symbols and rotates its one permitted chart series across them. This
/// respects both the account's one-series/session and two-connection limits,
/// while DXY still exists only once in the symbol cycle.
async fn run_multi_subscription(
    symbols: Vec<String>,
    tf: Timeframe,
    auth: TvAuth,
    tx: mpsc::Sender<BarEvent>,
) {
    let mut backoff_ms: u64 = 1_000;
    // Preserve the active cursor across reconnects. If one symbol times out,
    // resume at the following symbol instead of replaying the cycle from zero
    // and starving every symbol that follows the bad one.
    let mut resume_index = 0usize;
    loop {
        match run_multi_session(&symbols, tf, &auth, &tx, &mut resume_index).await {
            Ok(()) => {
                info!(
                    symbols = symbols.len(),
                    "tv union session ended cleanly; reconnecting"
                );
                backoff_ms = 1_000;
            }
            Err(err) => {
                warn!(symbols = symbols.len(), error = ?err, "tv union session ended; will reconnect");
            }
        }
        if tx.is_closed() {
            info!("union consumer dropped; stopping subscription");
            return;
        }
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms * 2).min(30_000);
    }
}

#[derive(Debug)]
struct MultiSeriesState {
    symbol: String,
    current_open: Option<Bar>,
    last_seen_ts: i64,
    /// Only formal chart candles advance the finalized cursor; quotes never do.
    last_closed_ts: i64,
    last_quote_emit_ms: i64,
    historical_seeded: bool,
}

async fn run_multi_session(
    symbols: &[String],
    tf: Timeframe,
    auth: &TvAuth,
    tx: &mpsc::Sender<BarEvent>,
    resume_index: &mut usize,
) -> Result<()> {
    let auth_token = fetch_auth_token(auth).await.unwrap_or_else(|err| {
        warn!(
            ?err,
            "failed to fetch union auth_token; using empty token (guest mode)"
        );
        String::new()
    });

    let url = WS_URL_TEMPLATE.replace("{build}", DEFAULT_BUILD_DATE);
    let mut req = url
        .as_str()
        .into_client_request()
        .context("union ws request")?;
    {
        let h = req.headers_mut();
        h.insert("Origin", ORIGIN.parse()?);
        h.insert("User-Agent", USER_AGENT.parse()?);
        if let Some(cookie) = build_cookie_header(auth) {
            h.insert("Cookie", cookie.parse()?);
        }
    }

    info!(
        symbols = symbols.len(),
        "connecting to TradingView union WS"
    );
    let (ws_stream, resp) = connect_ws(req, auth.proxy_url.as_deref())
        .await
        .context("union ws connect")?;
    info!(
        status = resp.status().as_u16(),
        symbols = symbols.len(),
        "union ws upgraded"
    );
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    let chart_session = format!("cs_{}", random_token(12));
    let quote_session = format!("qs_{}", random_token(12));
    send_command(&mut ws_tx, "set_auth_token", json!([auth_token])).await?;
    send_command(
        &mut ws_tx,
        "chart_create_session",
        json!([chart_session, ""]),
    )
    .await?;
    send_command(&mut ws_tx, "quote_create_session", json!([quote_session])).await?;
    send_command(
        &mut ws_tx,
        "quote_set_fields",
        json!([
            quote_session,
            "lp",
            "lp_time",
            "volume",
            "bid",
            "ask",
            "ch",
            "chp",
            "open_price",
            "high_price",
            "low_price",
            "prev_close_price"
        ]),
    )
    .await?;

    let mut states = HashMap::<String, MultiSeriesState>::new();
    let mut symbol_ids = Vec::with_capacity(symbols.len());
    for (index, symbol) in symbols.iter().enumerate() {
        // Symbols are added individually. `quote_fast_symbols`, however,
        // replaces the session's fast-symbol set, so it must not be sent once
        // per symbol here.
        send_command(
            &mut ws_tx,
            "quote_add_symbols",
            json!([quote_session, symbol]),
        )
        .await?;
        let ordinal = index + 1;
        let symbol_id = format!("sds_sym_{ordinal}");
        symbol_ids.push(symbol_id.clone());
        let resolve_payload = format!(r#"={{"symbol":"{symbol}","adjustment":"splits"}}"#);
        send_command(
            &mut ws_tx,
            "resolve_symbol",
            json!([chart_session, symbol_id, resolve_payload]),
        )
        .await?;
        states.insert(
            symbol.clone(),
            MultiSeriesState {
                symbol: symbol.clone(),
                current_open: None,
                last_seen_ts: i64::MIN,
                last_closed_ts: i64::MIN,
                last_quote_emit_ms: i64::MIN,
                historical_seeded: false,
            },
        );
    }

    // Register the complete set atomically. TradingView treats this command as
    // replacement state rather than an additive operation.
    let mut fast_symbols = Vec::with_capacity(symbols.len() + 1);
    fast_symbols.push(Value::String(quote_session.clone()));
    fast_symbols.extend(symbols.iter().cloned().map(Value::String));
    send_command(&mut ws_tx, "quote_fast_symbols", Value::Array(fast_symbols)).await?;

    // The account token exposes max_charts=1, and TradingView enforces that
    // as one series per chart session. Reuse one stable series id instead of
    // attempting seven concurrent series (which closes the entire session).
    let mut active_index = *resume_index % symbols.len();
    let mut generation = 1u64;
    let mut active_series_id = format!("sds_{generation}");
    let mut series_routes =
        HashMap::from([(active_series_id.clone(), symbols[active_index].clone())]);
    let mut series_route_order = VecDeque::from([active_series_id.clone()]);
    create_rotating_series(
        &mut ws_tx,
        &chart_session,
        &active_series_id,
        generation,
        &symbol_ids[active_index],
        tf,
        HISTORY_BARS,
    )
    .await?;
    let mut series_deadline = Some(Instant::now() + SERIES_COMPLETION_TIMEOUT);
    let mut next_rotation_at = None;

    loop {
        let wait_timeout =
            union_frame_wait_timeout(Instant::now(), series_deadline, next_rotation_at);
        let next_frame = match next_union_frame(&mut ws_rx, wait_timeout).await {
            Ok(frame) => frame,
            Err(_) if next_rotation_at.is_some_and(|deadline| Instant::now() >= deadline) => {
                send_command(
                    &mut ws_tx,
                    "remove_series",
                    json!([chart_session, &active_series_id]),
                )
                .await?;
                active_index = next_symbol_index(active_index, symbols.len());
                *resume_index = active_index;
                generation += 1;
                active_series_id = format!("sds_{generation}");
                let next_symbol = &symbols[active_index];
                series_routes.insert(active_series_id.clone(), next_symbol.clone());
                series_route_order.push_back(active_series_id.clone());
                // Keep enough retired ids to route frames that were already
                // in flight when `remove_series` completed, without allowing
                // the map to grow for the lifetime of the desktop app.
                while series_route_order.len() > 64 {
                    if let Some(retired) = series_route_order.pop_front() {
                        series_routes.remove(&retired);
                    }
                }
                let requested_bars = if states
                    .get(next_symbol)
                    .is_some_and(|state| state.historical_seeded)
                {
                    ROTATION_BARS
                } else {
                    HISTORY_BARS
                };
                create_rotating_series(
                    &mut ws_tx,
                    &chart_session,
                    &active_series_id,
                    generation,
                    &symbol_ids[active_index],
                    tf,
                    requested_bars,
                )
                .await?;
                next_rotation_at = None;
                series_deadline = Some(Instant::now() + SERIES_COMPLETION_TIMEOUT);
                continue;
            }
            Err(_) if series_deadline.is_some_and(|deadline| Instant::now() >= deadline) => {
                let stalled_symbol = &symbols[active_index];
                warn!(
                    series = %active_series_id,
                    symbol = %stalled_symbol,
                    timeout_seconds = SERIES_COMPLETION_TIMEOUT.as_secs(),
                    "TradingView correction series stalled; preserving quote feed and rotating"
                );
                // The quote session is independent from the single chart
                // series. A slow history response must not tear down qsd and
                // freeze all seven live symbols. Retire only the stalled
                // series and continue the correction rotation.
                send_command(
                    &mut ws_tx,
                    "remove_series",
                    json!([chart_session, &active_series_id]),
                )
                .await?;
                active_index = next_symbol_index(active_index, symbols.len());
                *resume_index = active_index;
                generation += 1;
                active_series_id = format!("sds_{generation}");
                let next_symbol = &symbols[active_index];
                series_routes.insert(active_series_id.clone(), next_symbol.clone());
                series_route_order.push_back(active_series_id.clone());
                while series_route_order.len() > 64 {
                    if let Some(retired) = series_route_order.pop_front() {
                        series_routes.remove(&retired);
                    }
                }
                let requested_bars = if states
                    .get(next_symbol)
                    .is_some_and(|state| state.historical_seeded)
                {
                    ROTATION_BARS
                } else {
                    HISTORY_BARS
                };
                create_rotating_series(
                    &mut ws_tx,
                    &chart_session,
                    &active_series_id,
                    generation,
                    &symbol_ids[active_index],
                    tf,
                    requested_bars,
                )
                .await?;
                next_rotation_at = None;
                series_deadline = Some(Instant::now() + SERIES_COMPLETION_TIMEOUT);
                continue;
            }
            Err(_) => {
                bail!(
                    "TradingView union websocket idle for {} seconds",
                    WS_IDLE_TIMEOUT.as_secs()
                );
            }
        };
        let Some(frame) = next_frame else {
            bail!("TradingView union websocket stream ended");
        };
        let frame = frame.context("union ws recv")?;
        let text = match frame {
            Message::Text(t) => t,
            Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            Message::Ping(p) => {
                ws_tx.send(Message::Pong(p)).await.ok();
                continue;
            }
            Message::Close(_) => bail!("server closed union connection"),
            _ => continue,
        };

        for packet in split_frames(&text) {
            if packet.starts_with("~h~") {
                ws_tx.send(Message::Text(encode_frame(packet))).await?;
                continue;
            }
            let value: Value = match serde_json::from_str(packet) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if let Some((fault, skip_active)) = union_protocol_fault(&value, &active_series_id) {
                if skip_active {
                    *resume_index = next_symbol_index(active_index, symbols.len());
                }
                bail!(
                    "TradingView union {fault} while loading {} ({active_series_id})",
                    symbols[active_index]
                );
            }
            handle_multi_message(tf, &value, &series_routes, &mut states, tx).await?;

            if series_completed_for(&value, &active_series_id) {
                series_deadline = None;
                // Seed every symbol as quickly as possible on a fresh
                // connection. Once all snapshots exist, keep the completed
                // series alive briefly and rotate at a fixed cadence; qsd
                // quote ticks continue updating all seven symbols meanwhile.
                next_rotation_at = Some(
                    Instant::now()
                        + if states.values().all(|state| state.historical_seeded) {
                            ROTATION_INTERVAL
                        } else {
                            Duration::ZERO
                        },
                );
            }
        }
    }
}

fn next_symbol_index(active_index: usize, symbol_count: usize) -> usize {
    (active_index + 1) % symbol_count
}

fn union_frame_wait_timeout(
    now: Instant,
    series_deadline: Option<Instant>,
    next_rotation_at: Option<Instant>,
) -> Duration {
    [series_deadline, next_rotation_at]
        .into_iter()
        .flatten()
        .map(|deadline| deadline.saturating_duration_since(now))
        .min()
        .unwrap_or(WS_IDLE_TIMEOUT)
        .min(WS_IDLE_TIMEOUT)
}

/// Return the protocol fault and whether reconnect should continue with the
/// next symbol. Session-wide faults retry the same symbol; a series-specific
/// fault advances the cursor so one rejected symbol cannot starve the cycle.
fn union_protocol_fault(value: &Value, active_series_id: &str) -> Option<(&'static str, bool)> {
    match value.get("m").and_then(Value::as_str) {
        Some("critical_error") => Some(("critical_error", false)),
        Some("protocol_error") => Some(("protocol_error", false)),
        Some("series_error")
            if value
                .get("p")
                .and_then(Value::as_array)
                .is_some_and(|params| {
                    params
                        .iter()
                        .any(|item| item.as_str() == Some(active_series_id))
                }) =>
        {
            Some(("series_error", true))
        }
        _ => None,
    }
}

async fn create_rotating_series<S>(
    ws_tx: &mut S,
    chart_session: &str,
    series_id: &str,
    generation: u64,
    symbol_id: &str,
    tf: Timeframe,
    bars: u32,
) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    send_command(
        ws_tx,
        "create_series",
        json!([
            chart_session,
            series_id,
            format!("s{generation}"),
            symbol_id,
            tf.tv_resolution(),
            bars,
            ""
        ]),
    )
    .await
}

fn series_completed_for(value: &Value, series_id: &str) -> bool {
    value.get("m").and_then(Value::as_str) == Some("series_completed")
        && value
            .get("p")
            .and_then(Value::as_array)
            .is_some_and(|params| params.iter().any(|item| item.as_str() == Some(series_id)))
}

async fn handle_multi_message(
    tf: Timeframe,
    value: &Value,
    series_routes: &HashMap<String, String>,
    states: &mut HashMap<String, MultiSeriesState>,
    tx: &mpsc::Sender<BarEvent>,
) -> Result<()> {
    let message = value.get("m").and_then(Value::as_str).unwrap_or("");
    if matches!(
        message,
        "critical_error" | "protocol_error" | "series_error"
    ) {
        warn!(payload = %value, "TradingView union protocol error");
    } else if matches!(message, "series_completed" | "symbol_resolved") {
        debug!(payload = %value, "TradingView union protocol state");
    }

    // Every configured symbol is already registered through
    // `quote_fast_symbols`, even though the account only permits one rotating
    // chart series. Use those quote ticks to keep each symbol's rolling M1 bar
    // current in real time; the chart series remains authoritative for the
    // historical snapshot and later corrects any sparse-tick OHLC differences.
    // Without this path one slow rotating series makes the other six symbols
    // appear frozen for an entire cycle.
    if message == "qsd" {
        tracing::trace!(payload = %value, "TradingView union quote update");
        let received_at_ms = unix_now_ms();
        // Quote deltas are sparse: a packet can advance `lp_time` without
        // repeating `lp`/bid/ask. Reuse the current close in that case so a
        // flat market still advances into a new minute instead of appearing
        // frozen until the next price-changing packet.
        let fallback_price = value
            .get("p")
            .and_then(Value::as_array)
            .and_then(|params| params.get(1))
            .and_then(Value::as_object)
            .and_then(|quote| quote.get("n"))
            .and_then(Value::as_str)
            .and_then(|symbol| states.get(symbol))
            .and_then(|state| state.current_open.as_ref())
            .map(|bar| bar.close);
        if let Some(tick) = extract_quote_tick(value, received_at_ms, fallback_price) {
            if let Some(state) = states.get_mut(&tick.symbol) {
                apply_quote_tick(state, tf, tick, received_at_ms, tx).await;
            } else {
                tracing::debug!(symbol = %tick.symbol, "quote update did not match a configured symbol");
            }
        } else {
            tracing::debug!(payload = %value, "unable to parse TradingView quote update");
        }
        return Ok(());
    }
    if message != "timescale_update" && message != "du" {
        return Ok(());
    }
    let Some(series_map) = value
        .get("p")
        .and_then(Value::as_array)
        .and_then(|params| params.get(1))
        .and_then(Value::as_object)
    else {
        return Ok(());
    };

    for (series_id, entry) in series_map {
        let Some(symbol) = series_routes.get(series_id) else {
            continue;
        };
        let Some(state) = states.get_mut(symbol) else {
            continue;
        };
        let Some(bars) = extract_bars_from_series_entry(entry, &state.symbol, tf) else {
            continue;
        };
        if bars.is_empty() {
            continue;
        }
        apply_authoritative_bars(state, tf, bars, unix_now_ms(), tx).await;
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
struct QuoteTick {
    symbol: String,
    price: f64,
    ts_ms: i64,
}

fn json_number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse().ok()))
        .filter(|number| number.is_finite())
}

/// Parse TradingView's quote-symbol-data packet. `lp_time` is preferred over
/// the local clock so reconnect snapshots carrying an old last price cannot
/// fabricate a candle in the current minute.
fn extract_quote_tick(
    value: &Value,
    fallback_now_ms: i64,
    fallback_price: Option<f64>,
) -> Option<QuoteTick> {
    if value.get("m").and_then(Value::as_str) != Some("qsd") {
        return None;
    }
    let quote = value.get("p")?.as_array()?.get(1)?.as_object()?;
    let symbol = quote.get("n")?.as_str()?.to_string();
    let fields = quote.get("v")?.as_object()?;
    // Index quotes normally carry `lp`; OANDA FX quote deltas commonly carry
    // only bid/ask after the initial snapshot. Use their midpoint so those
    // symbols continue updating instead of silently falling back to the
    // rotating chart series.
    let price = fields
        .get("lp")
        .and_then(json_number)
        .or_else(|| {
            let bid = fields.get("bid").and_then(json_number)?;
            let ask = fields.get("ask").and_then(json_number)?;
            Some((bid + ask) / 2.0)
        })
        .or(fallback_price)?;
    if price <= 0.0 {
        return None;
    }
    let raw_time = fields.get("lp_time").and_then(json_number);
    let ts_ms = raw_time
        .map(|timestamp| {
            if timestamp >= 10_000_000_000.0 {
                timestamp as i64
            } else {
                (timestamp * 1_000.0) as i64
            }
        })
        .unwrap_or(fallback_now_ms);
    Some(QuoteTick {
        symbol,
        price,
        ts_ms,
    })
}

fn quote_tick_bar(state: &MultiSeriesState, tf: Timeframe, price: f64, ts_ms: i64) -> Bar {
    let duration = tf.duration_ms();
    let bucket_ts = ts_ms.div_euclid(duration) * duration;
    if let Some(open) = state
        .current_open
        .as_ref()
        .filter(|bar| bar.ts == bucket_ts)
    {
        return Bar {
            symbol: state.symbol.clone(),
            tf,
            ts: bucket_ts,
            open: open.open,
            high: open.high.max(price),
            low: open.low.min(price),
            close: price,
            volume: open.volume,
        };
    }
    Bar {
        symbol: state.symbol.clone(),
        tf,
        ts: bucket_ts,
        open: price,
        high: price,
        low: price,
        close: price,
        volume: 0.0,
    }
}

async fn apply_quote_tick(
    state: &mut MultiSeriesState,
    tf: Timeframe,
    tick: QuoteTick,
    received_at_ms: i64,
    tx: &mpsc::Sender<BarEvent>,
) {
    let bar = quote_tick_bar(state, tf, tick.price, tick.ts_ms);
    if bar.ts < state.last_seen_ts || bar.ts <= state.last_closed_ts {
        return;
    }

    let is_new_bucket = state
        .current_open
        .as_ref()
        .is_none_or(|current| current.ts != bar.ts);
    let throttle_elapsed =
        received_at_ms.saturating_sub(state.last_quote_emit_ms) >= QUOTE_EMIT_INTERVAL_MS;

    if is_new_bucket || throttle_elapsed {
        // Quotes keep the chart responsive but are not complete OHLC candles.
        // Never close a minute here: the rotating formal series owns closes.
        publish_union_preview(state, bar, tx).await;
        state.last_quote_emit_ms = received_at_ms;
    } else {
        // Preserve every tick's close and extrema even when the expensive
        // downstream update is coalesced. The next emitted update (or the
        // minute transition) therefore still contains exact rolling OHLC.
        let ts = bar.ts;
        state.current_open = Some(bar);
        state.last_seen_ts = state.last_seen_ts.max(ts);
    }
}

/// One-shot history fetcher used by `TvClient::fetch_history`. Mirrors
/// `run_session`'s handshake and follows short initial snapshots with
/// `request_more_data`, which is required for some newly-added symbols and
/// calendar timeframes.
async fn run_history_session(
    symbol: &str,
    tf: Timeframe,
    auth: &TvAuth,
    limit: u32,
) -> Result<Vec<Bar>> {
    let auth_token = fetch_auth_token(auth).await.unwrap_or_else(|err| {
        warn!(?err, "history: failed to fetch auth_token; using empty");
        String::new()
    });

    let url = WS_URL_TEMPLATE.replace("{build}", DEFAULT_BUILD_DATE);
    let mut req = url.as_str().into_client_request().context("ws request")?;
    {
        let h = req.headers_mut();
        h.insert("Origin", ORIGIN.parse()?);
        h.insert("User-Agent", USER_AGENT.parse()?);
        if let Some(cookie) = build_cookie_header(auth) {
            h.insert("Cookie", cookie.parse()?);
        }
    }

    info!(symbol = %symbol, tf = %tf.tag(), limit, "history: connecting");
    let (ws_stream, _resp) = connect_ws(req, auth.proxy_url.as_deref())
        .await
        .context("ws connect")?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    let chart_session = format!("cs_{}", random_token(12));
    let quote_session = format!("qs_{}", random_token(12));
    let series_id = "sds_1";
    let symbol_id = "sds_sym_1";

    send_command(&mut ws_tx, "set_auth_token", json!([auth_token])).await?;
    send_command(
        &mut ws_tx,
        "chart_create_session",
        json!([chart_session, ""]),
    )
    .await?;
    send_command(&mut ws_tx, "quote_create_session", json!([quote_session])).await?;
    let resolve_payload = format!(r#"={{"symbol":"{symbol}","adjustment":"splits"}}"#);
    send_command(
        &mut ws_tx,
        "resolve_symbol",
        json!([chart_session, symbol_id, resolve_payload]),
    )
    .await?;
    send_command(
        &mut ws_tx,
        "create_series",
        json!([
            chart_session,
            series_id,
            "s1",
            symbol_id,
            tf.tv_resolution(),
            limit,
            ""
        ]),
    )
    .await?;

    let mut collected: Vec<Bar> = Vec::new();
    let deadline = tokio::time::Instant::now() + HISTORY_FETCH_TIMEOUT;
    let mut more_requests = 0usize;
    let mut count_before_more = None;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let frame = match tokio::time::timeout(remaining, ws_rx.next()).await {
            Ok(Some(Ok(f))) => f,
            Ok(Some(Err(e))) => return Err(anyhow!("ws recv: {e}")),
            Ok(None) => break,
            Err(_) => break,
        };
        let text = match frame {
            Message::Text(t) => t,
            Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            Message::Ping(p) => {
                ws_tx.send(Message::Pong(p)).await.ok();
                continue;
            }
            Message::Close(_) => break,
            _ => continue,
        };
        for packet in split_frames(&text) {
            if packet.starts_with("~h~") {
                let framed = encode_frame(packet);
                ws_tx.send(Message::Text(framed)).await.ok();
                continue;
            }
            let value: Value = match serde_json::from_str(packet) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let m = value.get("m").and_then(Value::as_str).unwrap_or("");
            match m {
                "timescale_update" => {
                    if let Some(bars) = extract_bars_from_timescale(&value, symbol, tf) {
                        collected.extend(bars);
                    }
                }
                "series_completed" if series_completed_for(&value, series_id) => {
                    let deduped = dedup_sorted(std::mem::take(&mut collected));
                    let current_count = deduped.len();
                    collected = deduped;
                    let made_progress =
                        count_before_more.is_none_or(|before| current_count > before);
                    if current_count < limit as usize
                        && more_requests < HISTORY_MAX_MORE_REQUESTS
                        && made_progress
                    {
                        let remaining = (limit as usize - current_count).max(1) as u32;
                        count_before_more = Some(current_count);
                        more_requests += 1;
                        info!(
                            symbol = %symbol,
                            tf = %tf.tag(),
                            current_count,
                            remaining,
                            request = more_requests,
                            "history: requesting older provider page"
                        );
                        send_command(
                            &mut ws_tx,
                            "request_more_data",
                            json!([chart_session, series_id, remaining]),
                        )
                        .await?;
                        continue;
                    }
                    let _ = ws_tx.send(Message::Close(None)).await;
                    return Ok(collected);
                }
                _ => {}
            }
        }
    }
    Ok(dedup_sorted(collected))
}

fn dedup_sorted(mut bars: Vec<Bar>) -> Vec<Bar> {
    // TradingView can send the same timestamp more than once as the edge bar
    // is corrected/finalized. Preserve the last payload received for a
    // timestamp rather than the first stale snapshot.
    let mut by_ts = std::collections::BTreeMap::new();
    for bar in bars.drain(..) {
        by_ts.insert(bar.ts, bar);
    }
    bars = by_ts.into_values().collect();
    // Log first/last timestamps so we can verify the TV-native 4H grid.
    if bars.first().map_or(false, |b| b.tf == Timeframe::H4) {
        let f = bars.first().unwrap();
        let l = bars.last().unwrap();
        tracing::info!(
            symbol = %f.symbol,
            tf = "4h",
            first_ts = f.ts,
            last_ts = l.ts,
            count = bars.len(),
            "TV native 4h grid check"
        );
    }
    bars
}

async fn run_session(
    symbol: &str,
    tf: Timeframe,
    auth: &TvAuth,
    tx: &mpsc::Sender<BarEvent>,
) -> Result<()> {
    let auth_token = fetch_auth_token(auth).await.unwrap_or_else(|err| {
        warn!(
            ?err,
            "failed to fetch auth_token; using empty token (guest mode)"
        );
        String::new()
    });
    tracing::info!(token_len = auth_token.len(), "using auth token");

    let url = WS_URL_TEMPLATE.replace("{build}", DEFAULT_BUILD_DATE);
    let mut req = url.as_str().into_client_request().context("ws request")?;
    {
        let h = req.headers_mut();
        h.insert("Origin", ORIGIN.parse()?);
        h.insert("User-Agent", USER_AGENT.parse()?);
        if let Some(cookie) = build_cookie_header(auth) {
            h.insert("Cookie", cookie.parse()?);
        }
    }

    info!(symbol = %symbol, "connecting to TradingView WS");
    let (ws_stream, resp) = connect_ws(req, auth.proxy_url.as_deref())
        .await
        .context("ws connect")?;
    info!(status = resp.status().as_u16(), "ws upgraded");
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    let chart_session = format!("cs_{}", random_token(12));
    let quote_session = format!("qs_{}", random_token(12));
    let series_id = "sds_1";
    let symbol_id = "sds_sym_1";

    // Handshake
    send_command(&mut ws_tx, "set_auth_token", json!([auth_token])).await?;
    send_command(
        &mut ws_tx,
        "chart_create_session",
        json!([chart_session, ""]),
    )
    .await?;
    send_command(&mut ws_tx, "quote_create_session", json!([quote_session])).await?;
    send_command(
        &mut ws_tx,
        "quote_set_fields",
        json!([
            quote_session,
            "lp",
            "lp_time",
            "volume",
            "bid",
            "ask",
            "ch",
            "chp",
            "rch",
            "rchp",
            "open_price",
            "high_price",
            "low_price",
            "prev_close_price"
        ]),
    )
    .await?;
    send_command(
        &mut ws_tx,
        "quote_add_symbols",
        json!([quote_session, symbol]),
    )
    .await?;
    send_command(
        &mut ws_tx,
        "quote_fast_symbols",
        json!([quote_session, symbol]),
    )
    .await?;
    let resolve_payload = format!(r#"={{"symbol":"{symbol}","adjustment":"splits"}}"#);
    send_command(
        &mut ws_tx,
        "resolve_symbol",
        json!([chart_session, symbol_id, resolve_payload]),
    )
    .await?;
    send_command(
        &mut ws_tx,
        "create_series",
        json!([
            chart_session,
            series_id,
            "s1",
            symbol_id,
            tf.tv_resolution(),
            HISTORY_BARS,
            ""
        ]),
    )
    .await?;

    // Track the in-progress bar so we can emit BarClosed on rollover.
    let mut current_open: Option<Bar> = None;
    // `current_open` is legitimately None while a market is closed.  Keep a
    // separate watermark so a repeated snapshot cannot replay thousands of
    // already-finalized bars as live updates.
    let mut last_seen_ts = i64::MIN;

    loop {
        let next_frame = tokio::time::timeout(WS_IDLE_TIMEOUT, ws_rx.next())
            .await
            .map_err(|_| anyhow!("TradingView websocket idle for 90 seconds"))?;
        let Some(frame) = next_frame else {
            bail!("TradingView websocket stream ended");
        };
        let frame = frame.context("ws recv")?;
        let text = match frame {
            Message::Text(t) => t,
            Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            Message::Ping(p) => {
                ws_tx.send(Message::Pong(p)).await.ok();
                continue;
            }
            Message::Close(_) => {
                bail!("server closed connection");
            }
            _ => continue,
        };

        for packet in split_frames(&text) {
            // Heartbeat (~h~N) → echo verbatim.
            if packet.starts_with("~h~") {
                debug!(packet = %packet, "heartbeat");
                let framed = encode_frame(packet);
                ws_tx
                    .send(Message::Text(framed))
                    .await
                    .context("send heartbeat")?;
                continue;
            }

            let value: Value = match serde_json::from_str(packet) {
                Ok(v) => v,
                Err(_) => {
                    debug!(packet = %packet, "non-json packet ignored");
                    continue;
                }
            };
            handle_message(symbol, tf, &value, &mut current_open, &mut last_seen_ts, tx).await?;
        }
    }
}

async fn handle_message(
    symbol: &str,
    tf: Timeframe,
    value: &Value,
    current_open: &mut Option<Bar>,
    last_seen_ts: &mut i64,
    tx: &mpsc::Sender<BarEvent>,
) -> Result<()> {
    let m = value.get("m").and_then(Value::as_str).unwrap_or("");
    match m {
        "timescale_update" => {
            tracing::debug!("got timescale_update");
            // p = [chart_session, { sds_1: { s: [ {i, v:[ts, o, h, l, c, vol]}, ... ] } }, ...]
            if let Some(bars) = extract_bars_from_timescale(value, symbol, tf) {
                if !bars.is_empty() {
                    info!(symbol = %symbol, n = bars.len(), "historical backfill");
                    if *last_seen_ts != i64::MIN {
                        // After the initial snapshot, TradingView may keep
                        // sending `timescale_update` with just the latest bar
                        // (especially after reconnect / recovery). Treat it
                        // like live input so a newer ts closes the previous
                        // 1m bar instead of silently replacing current_open.
                        for bar in bars {
                            apply_session_bar(bar, tf, current_open, last_seen_ts, tx).await;
                        }
                        return Ok(());
                    }
                    let latest_ts = bars.last().map(|bar| bar.ts).unwrap_or(i64::MIN);
                    let (closed, latest) = split_initial_snapshot(bars, tf, unix_now_ms());
                    *last_seen_ts = latest_ts;
                    *current_open = latest.clone();
                    if !closed.is_empty() {
                        let _ = tx.send(BarEvent::Historical(closed)).await;
                    }
                    // If the edge candle is still open, emit it immediately:
                    // on a quiet market TV may not send another `du` before
                    // the next trade.  If it has already closed (weekend or a
                    // delayed packet), it belongs in Historical instead.
                    if let Some(open) = latest {
                        let _ = tx.send(BarEvent::BarUpdate(open)).await;
                    }
                }
            }
        }
        "du" => {
            tracing::debug!("got du");
            // p = [chart_session, { sds_1: { s: [ {i, v:[ts, o, h, l, c, vol]} ] } }]
            match extract_bars_from_du(value, symbol, tf) {
                Some(bars) if !bars.is_empty() => {
                    tracing::debug!(n = bars.len(), "du parsed bars");
                    for bar in bars {
                        apply_session_bar(bar, tf, current_open, last_seen_ts, tx).await;
                    }
                }
                Some(_) => tracing::warn!("du had no bars"),
                None => tracing::warn!(payload = %value, "du parse failed"),
            }
        }
        "series_completed" => {
            debug!("series_completed");
        }
        "qsd" | "quote_completed" => {
            tracing::debug!("qsd/quote_completed");
        }
        "" => { /* server ack with no `m` */ }
        other => {
            tracing::debug!(m = %other, "unhandled message type");
        }
    }
    Ok(())
}

fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Split the first TV snapshot into finalized history and a possible rolling
/// edge candle.  Blindly dropping the last row loses Friday's final minute
/// for the entire weekend and delays any detector signal until Sunday open.
fn split_initial_snapshot(
    mut bars: Vec<Bar>,
    tf: Timeframe,
    now_ms: i64,
) -> (Vec<Bar>, Option<Bar>) {
    let edge_is_closed = bars
        .last()
        .and_then(|bar| bar.ts.checked_add(tf.duration_ms()))
        .is_some_and(|close_ts| close_ts <= now_ms);
    if edge_is_closed {
        return (bars, None);
    }
    let open = bars.pop();
    (bars, open)
}

async fn apply_session_bar(
    bar: Bar,
    tf: Timeframe,
    current_open: &mut Option<Bar>,
    last_seen_ts: &mut i64,
    tx: &mpsc::Sender<BarEvent>,
) {
    if bar.ts < *last_seen_ts || (current_open.is_none() && bar.ts == *last_seen_ts) {
        return;
    }
    let ts = bar.ts;
    apply_live_bar(bar, tf, current_open, tx).await;
    *last_seen_ts = (*last_seen_ts).max(ts);
}

async fn apply_live_bar(
    new_bar: Bar,
    tf: Timeframe,
    current_open: &mut Option<Bar>,
    tx: &mpsc::Sender<BarEvent>,
) {
    let bar_ms = tf.duration_ms();
    match current_open.take() {
        Some(prev) if new_bar.ts > prev.ts => {
            // New bar started — previous one is now closed.
            info!(symbol = %prev.symbol, ts = prev.ts, o = prev.open, h = prev.high,
                  l = prev.low, c = prev.close, "bar closed");
            let _ = tx.send(BarEvent::BarClosed(prev)).await;
            let _ = tx.send(BarEvent::BarUpdate(new_bar.clone())).await;
            *current_open = Some(new_bar);
        }
        Some(prev) if new_bar.ts == prev.ts => {
            // In-progress update — log at debug (high frequency).
            tracing::debug!(symbol = %new_bar.symbol, ts = new_bar.ts,
                c = new_bar.close, "bar tick");
            let _ = tx.send(BarEvent::BarUpdate(new_bar.clone())).await;
            *current_open = Some(new_bar);
        }
        Some(prev) => {
            // Out-of-order — keep the newer one.
            warn!(
                prev_ts = prev.ts,
                new_ts = new_bar.ts,
                "out-of-order bar update"
            );
            *current_open = Some(prev);
        }
        None => {
            tracing::info!(symbol = %new_bar.symbol, ts = new_bar.ts,
                o = new_bar.open, "live bar started");
            let _ = tx.send(BarEvent::BarUpdate(new_bar.clone())).await;
            *current_open = Some(new_bar);
        }
    }
    // Sanity: warn if a single bar's ts span exceeds 2x its tf (clock drift).
    if let Some(b) = current_open.as_ref() {
        let _ = bar_ms; // reserved for future invariant checks
        let _ = b;
    }
}

fn extract_bars_from_timescale(value: &Value, symbol: &str, tf: Timeframe) -> Option<Vec<Bar>> {
    let p = value.get("p")?.as_array()?;
    // p[1] is a map keyed by series id (e.g. "sds_1"); take its first entry.
    let map = p.get(1)?.as_object()?;
    let entry = map.values().next()?;
    let mut out = extract_bars_from_series_entry(entry, symbol, tf)?;
    out.sort_by_key(|b| b.ts);
    Some(out)
}

fn extract_bars_from_du(value: &Value, symbol: &str, tf: Timeframe) -> Option<Vec<Bar>> {
    let p = value.get("p")?.as_array()?;
    let map = p.get(1)?.as_object()?;
    let entry = map.values().next()?;
    extract_bars_from_series_entry(entry, symbol, tf)
}

fn extract_bars_from_series_entry(entry: &Value, symbol: &str, tf: Timeframe) -> Option<Vec<Bar>> {
    let s = entry.get("s")?.as_array()?;
    let mut out = Vec::with_capacity(s.len());
    for item in s {
        if let Some(bar) = parse_bar_value(item, symbol, tf) {
            out.push(bar);
        }
    }
    out.sort_by_key(|bar| bar.ts);
    Some(out)
}

fn parse_bar_value(item: &Value, symbol: &str, tf: Timeframe) -> Option<Bar> {
    let v = item.get("v")?.as_array()?;
    if v.len() < 5 {
        return None;
    }
    let ts_sec = v.get(0)?.as_f64()?;
    let open = v.get(1)?.as_f64()?;
    let high = v.get(2)?.as_f64()?;
    let low = v.get(3)?.as_f64()?;
    let close = v.get(4)?.as_f64()?;
    let volume = v.get(5).and_then(Value::as_f64).unwrap_or(0.0);
    Some(Bar {
        symbol: symbol.to_string(),
        tf,
        ts: (ts_sec * 1000.0) as i64,
        open,
        high,
        low,
        close,
        volume,
    })
}

// ---------- Framing helpers ----------

fn encode_frame(payload: &str) -> String {
    format!("~m~{}~m~{}", payload.len(), payload)
}

fn split_frames(input: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = input;
    while let Some(stripped) = rest.strip_prefix("~m~") {
        let Some(idx) = stripped.find("~m~") else {
            break;
        };
        let len_str = &stripped[..idx];
        let Ok(len) = len_str.parse::<usize>() else {
            break;
        };
        let after = &stripped[idx + 3..];
        if after.len() < len {
            break;
        }
        out.push(&after[..len]);
        rest = &after[len..];
    }
    out
}

async fn send_command<S>(ws_tx: &mut S, method: &str, params: Value) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let payload = json!({ "m": method, "p": params }).to_string();
    let framed = encode_frame(&payload);
    if method == "set_auth_token" {
        debug!(method = %method, "→ [redacted]");
    } else {
        debug!(method = %method, "→ {}", payload);
    }
    ws_tx
        .send(Message::Text(framed))
        .await
        .map_err(|e| anyhow!("ws send failed: {e}"))?;
    Ok(())
}

fn random_token(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

async fn fetch_auth_token(auth: &TvAuth) -> Result<String> {
    let mut builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(20));
    if let Some(proxy_url) = configured_http_proxy(auth.proxy_url.as_deref()) {
        builder = builder.proxy(reqwest::Proxy::all(&proxy_url)?);
    }
    builder = builder.default_headers({
        let mut h = reqwest::header::HeaderMap::new();
        if let Some(cookie) = build_cookie_header(auth) {
            h.insert("Cookie", cookie.parse()?);
        }
        h.insert("Referer", "https://www.tradingview.com/".parse()?);
        h
    });
    let client = builder.build()?;

    // Logged-in path: TV embeds the JWT auth_token in the /chart/ page HTML
    // (`var user = {...,"auth_token":"eyJ..."}`). Tested 2025-2026.
    if auth.sessionid.is_some() && auth.sessionid_sign.is_some() {
        let resp = client
            .get("https://www.tradingview.com/chart/")
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        tracing::info!(
            status = status.as_u16(),
            len = body.len(),
            "/chart/ fetched"
        );
        if let Some(tok) = extract_quoted_field(&body, "auth_token") {
            if tok.len() > 20 {
                tracing::info!(len = tok.len(), "got logged-in auth_token");
                return Ok(tok);
            }
        }
        let username = extract_quoted_field(&body, "username");
        tracing::warn!(username = ?username, "no auth_token in /chart/ html");
    }

    // Guest fallback: scrape homepage for unauthorized_user_token.
    let resp = client.get(HOMEPAGE_URL).send().await?;
    let body = resp.text().await?;
    tracing::info!(html_len = body.len(), "homepage fetched (guest fallback)");
    for field in ["unauthorized_user_token", "auth_token"] {
        if let Some(tok) = extract_quoted_field(&body, field) {
            tracing::info!(field = %field, len = tok.len(), "found token in homepage");
            return Ok(tok);
        }
    }
    bail!("auth_token not found via /chart/ or homepage")
}

fn extract_quoted_field(body: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":\"");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn build_cookie_header(auth: &TvAuth) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(s) = auth.sessionid.as_deref() {
        parts.push(format!("sessionid={s}"));
    }
    if let Some(s) = auth.sessionid_sign.as_deref() {
        parts.push(format!("sessionid_sign={s}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn test_bar(ts: i64, close: f64) -> Bar {
        Bar {
            symbol: "OANDA:EURUSD".into(),
            tf: Timeframe::M1,
            ts,
            open: close,
            high: close,
            low: close,
            close,
            volume: 1.0,
        }
    }

    #[test]
    fn frame_roundtrip() {
        let f = encode_frame("hello");
        assert_eq!(f, "~m~5~m~hello");
        let parts = split_frames(&f);
        assert_eq!(parts, vec!["hello"]);
    }

    #[test]
    fn split_multi_frames() {
        let s = "~m~5~m~hello~m~3~m~hi!";
        // "hi!" is 3 chars
        let parts = split_frames(s);
        assert_eq!(parts, vec!["hello", "hi!"]);
    }

    #[test]
    fn parse_bar_minimum() {
        let item = serde_json::json!({"i": 0, "v": [1_700_000_000.0, 1.0, 1.1, 0.9, 1.05, 42.0]});
        let bar = parse_bar_value(&item, "OANDA:EURUSD", Timeframe::M1).unwrap();
        assert_eq!(bar.ts, 1_700_000_000_000);
        assert_eq!(bar.open, 1.0);
        assert_eq!(bar.volume, 42.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parses_enabled_macos_https_proxy() {
        let output = r#"<dictionary> {
  HTTPEnable : 1
  HTTPPort : 7897
  HTTPProxy : 127.0.0.1
  HTTPSEnable : 1
  HTTPSPort : 7897
  HTTPSProxy : 127.0.0.1
}"#;
        assert_eq!(
            parse_macos_proxy(output).as_deref(),
            Some("http://127.0.0.1:7897")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ignores_disabled_macos_proxy() {
        let output = r#"<dictionary> {
  HTTPEnable : 0
  HTTPSEnable : 0
}"#;
        assert_eq!(parse_macos_proxy(output), None);
    }

    #[tokio::test]
    async fn newer_live_bar_closes_previous_open_bar() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut current_open = Some(test_bar(60_000, 1.0));

        apply_live_bar(
            test_bar(120_000, 1.1),
            Timeframe::M1,
            &mut current_open,
            &tx,
        )
        .await;

        match rx.recv().await.expect("closed event") {
            BarEvent::BarClosed(bar) => assert_eq!(bar.ts, 60_000),
            other => panic!("expected closed event, got {other:?}"),
        }
        match rx.recv().await.expect("update event") {
            BarEvent::BarUpdate(bar) => assert_eq!(bar.ts, 120_000),
            other => panic!("expected update event, got {other:?}"),
        }
        assert_eq!(current_open.map(|bar| bar.ts), Some(120_000));
    }

    #[test]
    fn initial_snapshot_keeps_a_finalized_edge_bar_in_history() {
        let bars = vec![test_bar(60_000, 1.0), test_bar(120_000, 1.1)];
        let (closed, open) = split_initial_snapshot(bars, Timeframe::M1, 180_000);
        assert_eq!(closed.len(), 2);
        assert!(open.is_none());
    }

    #[test]
    fn initial_snapshot_exposes_a_still_open_edge_bar() {
        let bars = vec![test_bar(60_000, 1.0), test_bar(120_000, 1.1)];
        let (closed, open) = split_initial_snapshot(bars, Timeframe::M1, 179_999);
        assert_eq!(closed.len(), 1);
        assert_eq!(open.map(|bar| bar.ts), Some(120_000));
    }

    #[test]
    fn qsd_tick_uses_provider_time_instead_of_local_reconnect_time() {
        let value = serde_json::json!({
            "m": "qsd",
            "p": ["qs_test", {
                "n": "TVC:DXY",
                "s": "ok",
                "v": {"lp": 99.173, "lp_time": 1_787_888_123.5}
            }]
        });

        let tick = extract_quote_tick(&value, 9_999_999_999_999, None).unwrap();
        assert_eq!(tick.symbol, "TVC:DXY");
        assert_eq!(tick.price, 99.173);
        assert_eq!(tick.ts_ms, 1_787_888_123_500);
    }

    #[test]
    fn qsd_tick_uses_fx_bid_ask_midpoint_when_last_price_is_omitted() {
        let value = serde_json::json!({
            "m": "qsd",
            "p": ["qs_test", {
                "n": "OANDA:USDCHF",
                "s": "ok",
                "v": {"bid": 0.80411, "ask": 0.80426}
            }]
        });

        let tick = extract_quote_tick(&value, 1_787_888_123_500, None).unwrap();
        assert_eq!(tick.symbol, "OANDA:USDCHF");
        assert!((tick.price - 0.804185).abs() < 1e-12);
        assert_eq!(tick.ts_ms, 1_787_888_123_500);
    }

    #[test]
    fn qsd_time_only_delta_reuses_the_previous_close() {
        let value = serde_json::json!({
            "m": "qsd",
            "p": ["qs_test", {
                "n": "TVC:DXY",
                "s": "ok",
                "v": {"lp_time": 1_787_888_180.0}
            }]
        });

        let tick = extract_quote_tick(&value, 9_999_999_999_999, Some(99.173)).unwrap();
        assert_eq!(tick.symbol, "TVC:DXY");
        assert_eq!(tick.price, 99.173);
        assert_eq!(tick.ts_ms, 1_787_888_180_000);
    }

    #[test]
    fn quote_tick_extends_existing_ohlc_without_resetting_open() {
        let mut open = test_bar(120_000, 1.05);
        open.open = 1.0;
        open.high = 1.1;
        open.low = 0.9;
        let state = MultiSeriesState {
            symbol: "OANDA:EURUSD".into(),
            current_open: Some(open),
            last_seen_ts: 120_000,
            last_closed_ts: i64::MIN,
            last_quote_emit_ms: i64::MIN,
            historical_seeded: true,
        };

        let bar = quote_tick_bar(&state, Timeframe::M1, 1.2, 179_500);
        assert_eq!(bar.ts, 120_000);
        assert_eq!(bar.open, 1.0);
        assert_eq!(bar.high, 1.2);
        assert_eq!(bar.low, 0.9);
        assert_eq!(bar.close, 1.2);
    }

    #[tokio::test]
    async fn qsd_tick_routes_live_bar_without_waiting_for_rotating_series() {
        let value = serde_json::json!({
            "m": "qsd",
            "p": ["qs_test", {
                "n": "OANDA:NZDUSD",
                "s": "ok",
                "v": {"lp": "0.59580", "lp_time": 120.5}
            }]
        });
        let mut states = HashMap::from([
            (
                "OANDA:AUDUSD".to_string(),
                MultiSeriesState {
                    symbol: "OANDA:AUDUSD".to_string(),
                    current_open: None,
                    last_seen_ts: i64::MIN,
                    last_closed_ts: i64::MIN,
                    last_quote_emit_ms: i64::MIN,
                    historical_seeded: false,
                },
            ),
            (
                "OANDA:NZDUSD".to_string(),
                MultiSeriesState {
                    symbol: "OANDA:NZDUSD".to_string(),
                    current_open: None,
                    last_seen_ts: i64::MIN,
                    last_closed_ts: i64::MIN,
                    last_quote_emit_ms: i64::MIN,
                    historical_seeded: false,
                },
            ),
        ]);
        let (tx, mut rx) = mpsc::channel(4);

        handle_multi_message(Timeframe::M1, &value, &HashMap::new(), &mut states, &tx)
            .await
            .unwrap();

        match rx.recv().await.unwrap() {
            BarEvent::BarUpdate(bar) => {
                assert_eq!(bar.symbol, "OANDA:NZDUSD");
                assert_eq!(bar.ts, 120_000);
                assert_eq!(bar.close, 0.5958);
            }
            other => panic!("expected quote-driven bar update, got {other:?}"),
        }
        assert!(states["OANDA:AUDUSD"].current_open.is_none());
    }

    #[tokio::test]
    async fn quote_ticks_are_coalesced_without_losing_ohlc_or_minute_boundary() {
        let mut state = MultiSeriesState {
            symbol: "OANDA:EURUSD".to_string(),
            current_open: None,
            last_seen_ts: i64::MIN,
            last_closed_ts: i64::MIN,
            last_quote_emit_ms: i64::MIN,
            historical_seeded: false,
        };
        let (tx, mut rx) = mpsc::channel(8);

        apply_quote_tick(
            &mut state,
            Timeframe::M1,
            QuoteTick {
                symbol: "OANDA:EURUSD".to_string(),
                price: 1.0,
                ts_ms: 120_100,
            },
            10_000,
            &tx,
        )
        .await;
        assert!(matches!(rx.recv().await, Some(BarEvent::BarUpdate(_))));

        apply_quote_tick(
            &mut state,
            Timeframe::M1,
            QuoteTick {
                symbol: "OANDA:EURUSD".to_string(),
                price: 1.2,
                ts_ms: 120_500,
            },
            10_500,
            &tx,
        )
        .await;
        assert!(rx.try_recv().is_err());
        assert_eq!(state.current_open.as_ref().unwrap().high, 1.2);

        apply_quote_tick(
            &mut state,
            Timeframe::M1,
            QuoteTick {
                symbol: "OANDA:EURUSD".to_string(),
                price: 1.1,
                ts_ms: 121_100,
            },
            11_000,
            &tx,
        )
        .await;
        match rx.recv().await {
            Some(BarEvent::BarUpdate(bar)) => {
                assert_eq!(bar.high, 1.2);
                assert_eq!(bar.close, 1.1);
            }
            other => panic!("expected coalesced quote update, got {other:?}"),
        }

        // A new minute bypasses the throttle, but only the formal series
        // may close the previous candle.
        apply_quote_tick(
            &mut state,
            Timeframe::M1,
            QuoteTick {
                symbol: "OANDA:EURUSD".to_string(),
                price: 1.3,
                ts_ms: 180_000,
            },
            11_001,
            &tx,
        )
        .await;
        assert!(matches!(rx.recv().await, Some(BarEvent::BarUpdate(bar)) if bar.ts == 180_000));
        assert!(rx.try_recv().is_err(), "quotes must not finalize OHLC");
    }

    #[tokio::test]
    async fn multi_series_snapshot_routes_each_symbol_independently() {
        let value = serde_json::json!({
            "m": "timescale_update",
            "p": ["cs_test", {
                "sds_1": {"s": [{"i": 0, "v": [60.0, 1.0, 1.1, 0.9, 1.05, 1.0]}]},
                "sds_2": {"s": [{"i": 0, "v": [60.0, 2.0, 2.1, 1.9, 2.05, 1.0]}]}
            }]
        });
        let routes = HashMap::from([
            ("sds_1".to_string(), "OANDA:AUDUSD".to_string()),
            ("sds_2".to_string(), "OANDA:NZDUSD".to_string()),
        ]);
        let mut states = HashMap::from([
            (
                "OANDA:AUDUSD".to_string(),
                MultiSeriesState {
                    symbol: "OANDA:AUDUSD".to_string(),
                    current_open: None,
                    last_seen_ts: i64::MIN,
                    last_closed_ts: i64::MIN,
                    last_quote_emit_ms: i64::MIN,
                    historical_seeded: false,
                },
            ),
            (
                "OANDA:NZDUSD".to_string(),
                MultiSeriesState {
                    symbol: "OANDA:NZDUSD".to_string(),
                    current_open: None,
                    last_seen_ts: i64::MIN,
                    last_closed_ts: i64::MIN,
                    last_quote_emit_ms: i64::MIN,
                    historical_seeded: false,
                },
            ),
        ]);
        let (tx, mut rx) = mpsc::channel(4);

        handle_multi_message(Timeframe::M1, &value, &routes, &mut states, &tx)
            .await
            .unwrap();

        let mut symbols = Vec::new();
        for _ in 0..2 {
            match rx.recv().await.unwrap() {
                BarEvent::Historical(bars) => symbols.push(bars[0].symbol.clone()),
                other => panic!("expected historical event, got {other:?}"),
            }
        }
        symbols.sort();
        assert_eq!(symbols, vec!["OANDA:AUDUSD", "OANDA:NZDUSD"]);
    }

    #[test]
    fn rotating_series_completion_matches_only_the_active_series() {
        let completed = serde_json::json!({
            "m": "series_completed",
            "p": ["cs_test", "sds_1", "streaming"]
        });
        assert!(series_completed_for(&completed, "sds_1"));
        assert!(!series_completed_for(&completed, "sds_2"));
    }

    #[test]
    fn union_series_deadline_is_absolute_across_unrelated_frames() {
        let started = Instant::now();
        let deadline = started + Duration::from_secs(30);

        assert_eq!(
            union_frame_wait_timeout(started, Some(deadline), None),
            Duration::from_secs(30)
        );
        // Receiving a quote frame twenty seconds later must not reset the
        // series deadline back to thirty seconds.
        assert_eq!(
            union_frame_wait_timeout(started + Duration::from_secs(20), Some(deadline), None),
            Duration::from_secs(10)
        );
        assert_eq!(
            union_frame_wait_timeout(started + Duration::from_secs(31), Some(deadline), None),
            Duration::ZERO
        );
    }

    #[test]
    fn scheduled_rotation_wakes_before_idle_timeout() {
        let started = Instant::now();
        let rotation_at = started + ROTATION_INTERVAL;

        assert_eq!(
            union_frame_wait_timeout(started, None, Some(rotation_at)),
            ROTATION_INTERVAL
        );
        assert_eq!(
            union_frame_wait_timeout(started + ROTATION_INTERVAL, None, Some(rotation_at)),
            Duration::ZERO
        );
    }

    #[test]
    fn active_series_error_reconnects_and_advances_the_cursor() {
        let active_error = serde_json::json!({
            "m": "series_error",
            "p": ["cs_test", "sds_4", "series failed"]
        });
        let unrelated_error = serde_json::json!({
            "m": "series_error",
            "p": ["cs_test", "sds_3", "series failed"]
        });
        let protocol_error = serde_json::json!({
            "m": "protocol_error",
            "p": ["cs_test", "bad request"]
        });

        assert_eq!(
            union_protocol_fault(&active_error, "sds_4"),
            Some(("series_error", true))
        );
        assert_eq!(union_protocol_fault(&unrelated_error, "sds_4"), None);
        assert_eq!(
            union_protocol_fault(&protocol_error, "sds_4"),
            Some(("protocol_error", false))
        );
        assert_eq!(next_symbol_index(3, 7), 4);
        assert_eq!(next_symbol_index(6, 7), 0);
    }
}

// ---------- Proxy-aware connect ----------
//
// `tokio_tungstenite::connect_async` ignores proxy environment variables and
// the macOS System Configuration proxy. Resolve both sources, tunnel through
// HTTP CONNECT, then hand the resulting stream to `client_async_tls`.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::http::Response;
use tokio_tungstenite::{client_async_tls, MaybeTlsStream, WebSocketStream};
use url::Url;

fn configured_http_proxy(explicit: Option<&str>) -> Option<String> {
    if let Some(proxy) = explicit.map(str::trim).filter(|proxy| !proxy.is_empty()) {
        return Some(proxy.to_string());
    }
    let from_env = std::env::var("https_proxy")
        .ok()
        .or_else(|| std::env::var("HTTPS_PROXY").ok())
        .or_else(|| std::env::var("http_proxy").ok())
        .or_else(|| std::env::var("HTTP_PROXY").ok())
        .or_else(|| std::env::var("all_proxy").ok())
        .or_else(|| std::env::var("ALL_PROXY").ok());
    if from_env.is_some() {
        return from_env;
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/sbin/scutil")
            .arg("--proxy")
            .output()
            .ok()?;
        if output.status.success() {
            return parse_macos_proxy(&String::from_utf8_lossy(&output.stdout));
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn parse_macos_proxy(output: &str) -> Option<String> {
    fn value<'a>(output: &'a str, key: &str) -> Option<&'a str> {
        output.lines().find_map(|line| {
            let (name, value) = line.trim().split_once(" : ")?;
            (name == key).then_some(value.trim())
        })
    }
    let (enabled, host, port) = if value(output, "HTTPSEnable") == Some("1") {
        ("HTTPSEnable", "HTTPSProxy", "HTTPSPort")
    } else {
        ("HTTPEnable", "HTTPProxy", "HTTPPort")
    };
    if value(output, enabled) != Some("1") {
        return None;
    }
    Some(format!(
        "http://{}:{}",
        value(output, host)?,
        value(output, port)?.parse::<u16>().ok()?
    ))
}

async fn connect_ws(
    req: Request,
    explicit_proxy: Option<&str>,
) -> Result<(
    WebSocketStream<MaybeTlsStream<TcpStream>>,
    Response<Option<Vec<u8>>>,
)> {
    let proxy = configured_http_proxy(explicit_proxy);

    let uri = req.uri().clone();
    let host = uri
        .host()
        .ok_or_else(|| anyhow!("ws uri missing host"))?
        .to_string();
    let port = uri.port_u16().unwrap_or(443);

    let tcp = if let Some(proxy_url) = proxy {
        info!(proxy = %proxy_url, "ws via HTTP CONNECT proxy");
        connect_via_http_proxy(&proxy_url, &host, port).await?
    } else {
        tokio::time::timeout(
            Duration::from_secs(10),
            TcpStream::connect((host.as_str(), port)),
        )
        .await
        .context("direct connect timed out")??
    };

    let (ws, resp) = client_async_tls(req, tcp)
        .await
        .map_err(|e| anyhow!("client_async_tls: {e}"))?;
    Ok((ws, resp))
}

async fn connect_via_http_proxy(proxy_url: &str, host: &str, port: u16) -> Result<TcpStream> {
    let parsed = Url::parse(proxy_url).context("parse proxy url")?;
    let phost = parsed
        .host_str()
        .ok_or_else(|| anyhow!("proxy missing host"))?;
    let pport = parsed.port().unwrap_or(match parsed.scheme() {
        "https" => 443,
        _ => 80,
    });
    let mut tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((phost, pport)))
        .await
        .context("proxy connect timed out")?
        .with_context(|| format!("connect proxy {phost}:{pport}"))?;

    let connect_req = format!(
        "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\nProxy-Connection: keep-alive\r\nUser-Agent: {USER_AGENT}\r\n\r\n"
    );
    tcp.write_all(connect_req.as_bytes())
        .await
        .context("write CONNECT")?;

    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    loop {
        let n = tcp.read(&mut tmp).await.context("read CONNECT response")?;
        if n == 0 {
            bail!("proxy closed during CONNECT");
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            bail!("proxy CONNECT response too large");
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let status_line = head.lines().next().unwrap_or("");
    if !status_line.contains(" 200") {
        bail!("proxy CONNECT failed: {status_line}");
    }
    Ok(tcp)
}

#[cfg(test)]
#[path = "union_tests.rs"]
mod union_tests;
