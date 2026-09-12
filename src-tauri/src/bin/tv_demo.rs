//! M1 demo: subscribe to OANDA:EURUSD 1m bars from TradingView, log every
//! event with `tracing`, persist closed bars to SQLite.
//!
//! Run with:  cargo run --bin tv-demo
//! DB path :  $HOME/.ict-monitor/ict.db (override via $ICT_DB_PATH)

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use tracing_subscriber::EnvFilter;

use ict_monitor::config::AppConfig;
use ict_monitor::data_source::tradingview::{TvAuth, TvClient};
use ict_monitor::data_source::MarketDataProvider;
use ict_monitor::storage::SqliteStore;
use ict_monitor::types::{BarEvent, Timeframe};

fn db_path() -> PathBuf {
    if let Ok(p) = env::var("ICT_DB_PATH") {
        return PathBuf::from(p);
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".ict-monitor").join("ict.db")
}

fn fmt_ts(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| ms.to_string())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let db_path = db_path();
    let store = SqliteStore::open(&db_path).context("opening sqlite store")?;
    tracing::info!(path = %db_path.display(), "sqlite store ready");

    let symbol = env::var("ICT_SYMBOL").unwrap_or_else(|_| "OANDA:EURUSD".into());
    let symbols: Vec<String> = env::var("ICT_SYMBOLS")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .filter(|symbols: &Vec<String>| !symbols.is_empty())
        .unwrap_or_else(|| vec![symbol.clone()]);
    let tf = Timeframe::M1;
    tracing::info!(symbol = %symbol, tf = ?tf, "starting subscription");

    let cfg = AppConfig::load().unwrap_or_default();
    let auth = TvAuth {
        sessionid: cfg.tradingview.sessionid.clone(),
        sessionid_sign: cfg.tradingview.sessionid_sign.clone(),
        proxy_url: cfg.tradingview.proxy_url.clone(),
    };
    if auth.sessionid.is_some() {
        tracing::info!("using TradingView session cookies from config");
    } else {
        tracing::info!("no TV cookies configured; running as guest");
    }
    let client = TvClient::with_auth(auth);
    let mut rx = if symbols.len() == 1 {
        client
            .subscribe_bars(&symbols[0], tf)
            .await
            .context("subscribe_bars")?
    } else {
        client
            .subscribe_many_bars(&symbols, tf)
            .await
            .context("subscribe_many_bars")?
    };

    while let Some(evt) = rx.recv().await {
        match evt {
            BarEvent::Historical(bars) => {
                tracing::info!(n = bars.len(), "historical batch received");
                let inserted = store.insert_bars(&bars).unwrap_or_else(|e| {
                    tracing::error!(error = ?e, "historical insert failed");
                    0
                });
                tracing::info!(
                    inserted,
                    total_in_db = store.count(&symbol, tf.tag()).unwrap_or(-1),
                    "historical persisted"
                );
            }
            BarEvent::BarUpdate(bar) => {
                tracing::debug!(
                    ts = %fmt_ts(bar.ts),
                    o = bar.open, h = bar.high, l = bar.low, c = bar.close,
                    "bar update"
                );
            }
            BarEvent::BarClosed(bar) => {
                tracing::info!(
                    symbol = %bar.symbol,
                    ts = %fmt_ts(bar.ts),
                    o = bar.open, h = bar.high, l = bar.low, c = bar.close, v = bar.volume,
                    "BAR CLOSED"
                );
                if let Err(e) = store.insert_bar(&bar) {
                    tracing::error!(error = ?e, "insert closed bar failed");
                }
            }
        }
    }

    tracing::warn!("stream ended");
    Ok(())
}
