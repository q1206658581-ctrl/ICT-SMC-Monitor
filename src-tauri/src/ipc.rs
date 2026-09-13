//! Tauri IPC types & commands shared by every command handler.
//!
//! Keep this module **thin** — it should only contain serializable structs
//! and command functions; business logic lives in the libraries it calls.

use serde::{Deserialize, Serialize};

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::aggregator::SymbolAggregator;
use crate::candidate::CandidateEngine;
use crate::config::AppConfig;
use crate::data_source::tradingview::TvClient;
use crate::detector::smt::SmtEngine;
use crate::detector::types::IctStructure;
use crate::storage::SqliteStore;
use crate::types::{Bar, Timeframe};
use crate::watchlist::Watchlist;

pub type SmtHandle = Arc<parking_lot::Mutex<SmtEngine>>;
pub type CandidateHandle = Arc<parking_lot::Mutex<CandidateEngine>>;
pub type AlertHandle = Arc<parking_lot::Mutex<crate::alert::AlertEngine>>;
pub type LlmDecisionHandle = Arc<crate::llm::LlmDecisionPipeline>;
pub type AlertCooldownHandle = Arc<parking_lot::Mutex<i64>>;

/// One independent strategy pipeline. Market data is subscribed once and
/// routed into every group containing the symbol (DXY therefore fans out to
/// all built-in groups).
pub struct EngineGroup {
    pub watchlist: Watchlist,
    pub smt: SmtHandle,
    pub candidates: CandidateHandle,
    pub alerts: AlertHandle,
    /// M7 decision sidecar shared by every strategy group.
    pub llm_decisions: Option<LlmDecisionHandle>,
}

pub type EngineGroups = Arc<parking_lot::RwLock<HashMap<String, Arc<EngineGroup>>>>;

/// Wire-shape used for both `bar:update` and `bar:closed` events.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BarPayload {
    pub symbol: String,
    pub tf: String,
    pub ts: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub closed: bool,
}

impl BarPayload {
    pub fn from_bar(bar: &Bar, closed: bool) -> Self {
        Self {
            symbol: bar.symbol.clone(),
            tf: bar.tf.tag().to_string(),
            ts: bar.ts,
            open: bar.open,
            high: bar.high,
            low: bar.low,
            close: bar.close,
            volume: bar.volume,
            closed,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppStatusPayload {
    pub kind: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SymbolMeta {
    pub symbol: String,
    pub provider: String,
}

/// Shared application state injected into command handlers.
pub struct AppState {
    pub store: SqliteStore,
    pub symbols: Vec<String>,
    pub tv_client: TvClient,
    /// Serializes one-shot TradingView history sessions. The live union feed
    /// owns one of the account's two allowed connections, leaving capacity
    /// for exactly one temporary history connection at a time.
    pub history_fetch_lock: Arc<Mutex<()>>,
    pub aggregators: Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<SymbolAggregator>>>>>,
    pub engine: crate::detector::EngineHandle,
    /// Last-known-good structures per `"symbol|tf"`, updated whenever
    /// `list_structures` successfully acquires the engine lock. When the
    /// lock is busy (PO3 replay) `list_structures` returns the cached
    /// value so the chart doesn't freeze.
    pub structures_cache:
        Arc<parking_lot::RwLock<std::collections::HashMap<String, Vec<IctStructure>>>>,
    /// All concurrently monitored watchlist pipelines (M6c).
    pub engine_groups: EngineGroups,
    /// Production M7 provider used by runtime-created groups as well as the
    /// cold-start group set. None means cleanly disabled/degraded.
    pub llm_decisions: Option<LlmDecisionHandle>,
    /// One cooldown clock shared by every group. Runtime-created groups must
    /// join the same clock instead of silently gaining an independent quota.
    pub global_alert_cooldown: AlertCooldownHandle,
    pub feishu_sender: crate::alert::FeishuAlertSender,
    /// Currently active watchlist (M5).
    pub active_watchlist: Arc<Mutex<Option<Watchlist>>>,
    /// Full app config (watchlists + TV creds) for CRUD commands (M5).
    pub config: Arc<parking_lot::RwLock<AppConfig>>,
}

#[tauri::command]
pub async fn list_symbols(state: tauri::State<'_, AppState>) -> Result<Vec<SymbolMeta>, String> {
    Ok(state
        .symbols
        .iter()
        .map(|s| SymbolMeta {
            symbol: s.clone(),
            provider: "tradingview".to_string(),
        })
        .collect())
}

#[tauri::command]
pub async fn get_history(
    state: tauri::State<'_, AppState>,
    symbol: String,
    tf: String,
    limit: i64,
) -> Result<Vec<BarPayload>, String> {
    let tf_enum = Timeframe::from_tag(&tf).ok_or_else(|| format!("unknown tf: {tf}"))?;
    let rows = state
        .store
        .recent_bars(&symbol, tf_enum, limit)
        .map_err(|e| format!("query failed: {e}"))?;
    Ok(rows.iter().map(|b| BarPayload::from_bar(b, true)).collect())
}

#[tauri::command]
pub async fn add_symbol(_symbol: String) -> Result<(), String> {
    Err("M2: dynamic symbol add not implemented; only OANDA:EURUSD is wired".to_string())
}

#[tauri::command]
pub async fn remove_symbol(_symbol: String) -> Result<(), String> {
    Err("M2: dynamic symbol remove not implemented".to_string())
}
