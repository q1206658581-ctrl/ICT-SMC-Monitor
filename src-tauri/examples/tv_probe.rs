//! Probe: compare OANDA:BTCUSD (broker CFD) vs COINBASE:BTCUSD (24/7
//! exchange) last-bar timestamps on a weekend. Reveals whether the CFD
//! feed is stuck at Friday while the exchange feed is live.
//!
//! Run (PTY so stdout is line-buffered):
//!   https_proxy=http://127.0.0.1:7897 cargo run --example tv_probe

use chrono::{FixedOffset, TimeZone, Utc};
use ict_monitor::config::AppConfig;
use ict_monitor::data_source::tradingview::{TvAuth, TvClient};
use ict_monitor::types::Timeframe;
use std::io::Write;

fn fmt(ms: i64) -> String {
    let utc = Utc.timestamp_millis_opt(ms).single();
    let sha = utc.and_then(|t| {
        let off = FixedOffset::east_opt(8 * 3600)?;
        Some(t.with_timezone(&off))
    });
    format!(
        "{} ({})",
        utc.map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| ms.to_string()),
        sha.map(|t| t.format("%m-%d %H:%M SH").to_string())
            .unwrap_or_default(),
    )
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let cfg = AppConfig::load().unwrap_or_default();
    let auth = TvAuth {
        sessionid: cfg.tradingview.sessionid.clone(),
        sessionid_sign: cfg.tradingview.sessionid_sign.clone(),
        proxy_url: cfg.tradingview.proxy_url.clone(),
    };
    let client = TvClient::with_auth(auth);
    let now = chrono::Utc::now().timestamp_millis();
    println!("=== now: {} ===", fmt(now));
    let _ = std::io::stdout().flush();

    let symbols = ["OANDA:BTCUSD", "COINBASE:BTCUSD", "INDEX:BTCUSD", "TVC:DXY"];
    for sym in symbols {
        let mut done = false;
        for attempt in 1..=4u8 {
            match client.fetch_history(sym, Timeframe::M1, 8).await {
                Ok(bars) => {
                    if let Some(last) = bars.last() {
                        let age_min = (now - last.ts) / 60_000;
                        println!(
                            "{:18} n={:2} last={} age={}min close={} (try {})",
                            sym,
                            bars.len(),
                            fmt(last.ts),
                            age_min,
                            last.close,
                            attempt
                        );
                    } else {
                        println!("{:18} n=0 (empty, try {})", sym, attempt);
                    }
                    let _ = std::io::stdout().flush();
                    done = true;
                    break;
                }
                Err(e) => {
                    eprintln!("{:18} try{} err: {:#}", sym, attempt, e);
                    let _ = std::io::stderr().flush();
                    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                }
            }
        }
        if !done {
            println!("{:18} FAILED after retries", sym);
            let _ = std::io::stdout().flush();
        }
    }
}
