//! Pluggable market-data source layer.
//!
//! M1 ships only the TradingView WS implementation; the `MarketDataProvider`
//! trait is stubbed out so future providers (OANDA v20, etc.) can drop in
//! without touching upstream code.

pub mod tradingview;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::types::{BarEvent, Timeframe};

/// Common interface every data source must satisfy.
///
/// Note for M1: only `subscribe_bars` is exercised. The shape is intentionally
/// minimal — extra knobs (tick streams, historical-only fetch, multi-tf in one
/// call) will land alongside the consumers that need them.
#[async_trait]
pub trait MarketDataProvider: Send + Sync {
    /// Begin streaming bars for `(symbol, tf)`. The returned receiver yields
    /// historical backfill first, then live updates / closes.
    async fn subscribe_bars(&self, symbol: &str, tf: Timeframe)
        -> Result<mpsc::Receiver<BarEvent>>;
}

pub mod symbol_search;
