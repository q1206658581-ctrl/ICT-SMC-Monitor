//! Alert Engine (M6b).
//!
//! Creates one actionable C2 alert per executable trade symbol and records
//! later LTF Validated facts separately. C2 alerts bypass the legacy global
//! cooldown so two symbols confirmed together can both notify; reversal facts
//! retain the existing dedupe/cooldown path and never use DesktopNotify.

pub mod channels;
pub use channels::*;
#[cfg(test)]
mod tests;
pub mod types;

use std::collections::HashMap;
use std::sync::Arc;

use crate::candidate::{CandidateChange, CandidateSetup, SetupStatus};
use crate::detector::types::SmtDivergence;
use crate::storage::SqliteStore;
pub use types::*;

/// A channel that delivers alerts (§3.3, extensible seam for M7+).
pub trait AlertChannel: Send + Sync {
    fn kind(&self) -> ChannelKind;
    fn deliver(&self, alert: &AlertRecord) -> Result<(), String>;
}

/// The alert engine: transition detection + cooldown + dispatch.
pub struct AlertEngine {
    config: types::AlertConfig,
    prev_status: HashMap<String, SetupStatus>,
    /// Shared across every watchlist engine. M6c keeps the user-facing
    /// cooldown global even though transition/dedupe state is per group.
    last_global_alert_ts: Arc<parking_lot::Mutex<i64>>,
    store: Option<SqliteStore>,
    channels: Vec<Box<dyn AlertChannel>>,
}

impl AlertEngine {
    pub fn new() -> Self {
        Self {
            config: AlertConfig::default(),
            prev_status: HashMap::new(),
            last_global_alert_ts: Arc::new(parking_lot::Mutex::new(0)),
            store: None,
            channels: Vec::new(),
        }
    }

    pub fn with_store(store: SqliteStore) -> Self {
        Self {
            config: AlertConfig::default(),
            prev_status: HashMap::new(),
            last_global_alert_ts: Arc::new(parking_lot::Mutex::new(0)),
            store: Some(store),
            channels: Vec::new(),
        }
    }

    pub fn with_store_and_global_cooldown(
        store: SqliteStore,
        last_global_alert_ts: Arc<parking_lot::Mutex<i64>>,
    ) -> Self {
        Self {
            config: AlertConfig::default(),
            prev_status: HashMap::new(),
            last_global_alert_ts,
            store: Some(store),
            channels: Vec::new(),
        }
    }

    pub fn set_enabled(&mut self, v: bool) {
        self.config.enabled = v;
    }

    pub fn set_desktop_notify(&mut self, v: bool) {
        self.config.desktop_notify_enabled = v;
    }

    pub fn desktop_notify_enabled(&self) -> bool {
        self.config.desktop_notify_enabled
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn set_cooldown_seconds(&mut self, s: i64) {
        self.config.cooldown_seconds = s;
    }

    pub fn add_channel(&mut self, ch: Box<dyn AlertChannel>) {
        self.channels.push(ch);
    }

    /// Process a candidate change. Returns Some(AlertRecord) if an alert
    /// should fire, None otherwise.
    ///
    /// - `seeding=true`: only updates prev_status, no alert.
    /// - Transition detection: prev != Validated && new == Validated.
    /// - Cooldown: per-candidate (has_fired_for_candidate) + global.
    pub fn on_candidate_change(
        &mut self,
        change: &CandidateChange,
        now: i64,
        seeding: bool,
    ) -> Option<AlertRecord> {
        let cid = &change.candidate.id;
        let new_status = change.candidate.setup_status;
        let prev = self.prev_status.get(cid).copied();

        // Always update prev_status.
        self.prev_status.insert(cid.clone(), new_status);

        if seeding {
            return None;
        }
        if !self.config.enabled {
            return None;
        }

        // Every newly accepted symbol validation can fire once. The legacy
        // aggregate status transition remains as a compatibility fallback.
        let validation = change.validation_event.as_ref();
        let is_legacy_validated_transition = validation.is_none()
            && new_status == SetupStatus::Validated
            && prev != Some(SetupStatus::Validated);
        if validation.is_none() && !is_legacy_validated_transition {
            return None;
        }

        // Cooldown check.
        let validation_symbol = validation
            .map(|event| event.symbol.as_str())
            .or(change.candidate.validation_symbol.as_deref());
        if !self.should_fire_for_candidate_episode_symbol(&change.candidate, validation_symbol, now)
        {
            return None;
        }

        // Determine which channels will fire (pre-filter so we can
        // build the alert with the correct channels_fired before
        // delivering -- avoids emitting an alert with empty
        // channels_fired and avoids a double-write to SQLite).
        // Reversal confirmations are durable facts for Reversal Inbox, not
        // user-facing alerts. Only the Inbox channel receives them.
        let active_channels: Vec<&Box<dyn AlertChannel>> = self
            .channels
            .iter()
            .filter(|ch| ch.kind() == ChannelKind::Inbox)
            .collect();
        let fired_kinds: Vec<ChannelKind> = active_channels.iter().map(|ch| ch.kind()).collect();

        // Build alert once with the correct channels_fired.
        let alert = build_alert_from_change(change, now, fired_kinds);

        // Deliver to each channel. InboxChannel::deliver persists the
        // alert to SQLite (INSERT OR REPLACE) + emits ict:alert:fired.
        // No second insert here -- that was a redundant double-write.
        for ch in &active_channels {
            if let Err(e) = ch.deliver(&alert) {
                tracing::warn!(error = %e, channel = ?ch.kind(), "alert channel deliver failed");
            }
        }

        // Fallback: if no InboxChannel was registered, persist here.
        if !active_channels
            .iter()
            .any(|ch| ch.kind() == ChannelKind::Inbox)
        {
            if let Some(ref store) = self.store {
                if let Err(e) = store.insert_alert(&alert) {
                    tracing::warn!(error = ?e, "insert_alert fallback failed");
                }
            }
        }

        // Update global cooldown.
        *self.last_global_alert_ts.lock() = now;

        Some(alert)
    }

    /// Reconcile per-trade-symbol C2 facts into the new Alert Inbox.
    ///
    /// This deliberately bypasses the legacy global cooldown: EURUSD and
    /// GBPUSD confirming C2 at the same instant are two distinct setups and
    /// must each create a row and notification. Deterministic record IDs make
    /// repeated SMT snapshots idempotent.
    pub fn on_c2_confirmed(
        &mut self,
        candidate: &CandidateSetup,
        smt: &SmtDivergence,
        seeding: bool,
    ) -> Vec<AlertRecord> {
        let mut created = Vec::new();
        for symbol in &candidate.trade_symbols {
            // During live processing an already-invalidated leg must not
            // suddenly notify. Cold replay is different: the leg did pass
            // C2 earlier, so reconstruct its historical Alert Inbox row
            // silently and expose the latest invalidated status at query time.
            if !seeding && candidate.invalidated_symbols.contains(symbol) {
                continue;
            }
            let Some(symbol_c2) = smt
                .chains
                .iter()
                .find(|chain| &chain.symbol == symbol)
                .and_then(|chain| chain.c2_candle.as_ref())
            else {
                continue;
            };
            // Alert Inbox is one row per trade symbol. Its first alert must
            // therefore follow that symbol's own C2 confirmation boundary,
            // rather than wait for the sweeper (normally DXY) to finish a
            // later C2 candle. For example, a 30m Case-3 C2 candle starting
            // at 08:00 confirms at 08:30; the alert must be 08:30, not 09:00.
            let alert_ts = symbol_c2
                .ts
                .saturating_add(smt.comparison_timeframe.duration_ms());
            let channel_kinds = if seeding || !self.config.enabled {
                Vec::new()
            } else {
                self.channels
                    .iter()
                    .filter(|channel| {
                        channel.kind() == ChannelKind::Inbox
                            || (channel.kind() == ChannelKind::DesktopNotify
                                && self.config.desktop_notify_enabled)
                    })
                    .map(|channel| channel.kind())
                    .collect()
            };
            let Some(alert) = build_c2_alert(candidate, smt, symbol, alert_ts, channel_kinds)
            else {
                continue;
            };
            if self
                .store
                .as_ref()
                .is_some_and(|store| store.has_alert_id(&alert.id).unwrap_or(false))
            {
                continue;
            }

            if seeding || !self.config.enabled {
                if let Some(store) = &self.store {
                    if let Err(error) = store.insert_alert(&alert) {
                        tracing::warn!(?error, "persist C2 alert failed");
                        continue;
                    }
                }
            } else {
                let active_channels: Vec<&Box<dyn AlertChannel>> = self
                    .channels
                    .iter()
                    .filter(|channel| alert.channels_fired.contains(&channel.kind()))
                    .collect();
                for channel in &active_channels {
                    if let Err(error) = channel.deliver(&alert) {
                        tracing::warn!(?error, channel = ?channel.kind(), "C2 alert delivery failed");
                    }
                }
                if !active_channels
                    .iter()
                    .any(|channel| channel.kind() == ChannelKind::Inbox)
                {
                    if let Some(store) = &self.store {
                        if let Err(error) = store.insert_alert(&alert) {
                            tracing::warn!(?error, "persist C2 alert fallback failed");
                        }
                    }
                }
            }
            tracing::info!(
                alert_id = %alert.id,
                watchlist_id = %alert.watchlist_id,
                symbol = %symbol,
                market_ts = alert.created_at,
                historical = seeding,
                enabled = self.config.enabled,
                attempted_channels = ?alert.channels_fired,
                "C2 alert processed"
            );
            created.push(alert);
        }
        created
    }

    /// M7 seam: check if an alert should fire for a candidate.
    /// Per-candidate (has_fired_for_candidate) + global cooldown.
    pub fn should_fire(&self, candidate_id: &str, now: i64) -> bool {
        self.should_fire_for_symbol(candidate_id, None, now)
    }

    pub fn should_fire_for_symbol(
        &self,
        candidate_id: &str,
        validation_symbol: Option<&str>,
        now: i64,
    ) -> bool {
        // Per-candidate: already fired?
        if let Some(ref store) = self.store {
            let fired = match validation_symbol {
                Some(symbol) => store.has_fired_for_candidate_symbol(candidate_id, symbol),
                None => store.has_fired_for_candidate(candidate_id),
            };
            if let Ok(true) = fired {
                return false;
            }
        }
        // Global cooldown.
        let last_global_alert_ts = *self.last_global_alert_ts.lock();
        if self.config.cooldown_seconds > 0 && last_global_alert_ts > 0 {
            if now - last_global_alert_ts < self.config.cooldown_seconds * 1000 {
                return false;
            }
        }
        true
    }

    fn should_fire_for_candidate_episode_symbol(
        &self,
        candidate: &CandidateSetup,
        validation_symbol: Option<&str>,
        now: i64,
    ) -> bool {
        if let (Some(store), Some(symbol)) = (&self.store, validation_symbol) {
            if let Ok(true) = store.has_fired_for_candidate_episode_symbol(candidate, symbol) {
                return false;
            }
            if let Ok(true) = store.has_fired_for_candidate_group_symbol(
                &candidate.id,
                &candidate.watchlist_id,
                symbol,
            ) {
                return false;
            }
        } else if let Some(store) = &self.store {
            if let Ok(true) = store.has_fired_for_candidate(&candidate.id) {
                return false;
            }
        }

        // Cooldown is intentionally shared across every group, while episode
        // and candidate deduplication above remain group-scoped.
        let last_global_alert_ts = *self.last_global_alert_ts.lock();
        !(self.config.cooldown_seconds > 0
            && last_global_alert_ts > 0
            && now - last_global_alert_ts < self.config.cooldown_seconds * 1000)
    }

    /// Rebuild prev_status from active candidates after seeding.
    pub fn hydrate(&mut self, candidates: &[CandidateSetup]) {
        self.prev_status.clear();
        for c in candidates {
            self.prev_status.insert(c.id.clone(), c.setup_status);
        }
        tracing::info!(
            hydrated = self.prev_status.len(),
            "alert engine hydrated prev_status"
        );
    }

    /// Fill prev_status for candidates not already tracked, without
    /// clearing existing entries. Used after on_candidate_change calls
    /// during replay to set prev_status for candidates that had no
    /// changes, so future transitions are detected correctly.
    pub fn hydrate_missing(&mut self, candidates: &[CandidateSetup]) {
        let mut added = 0;
        for c in candidates {
            if !self.prev_status.contains_key(&c.id) {
                self.prev_status.insert(c.id.clone(), c.setup_status);
                added += 1;
            }
        }
        if added > 0 {
            tracing::info!(added, "alert engine hydrate_missing filled gaps");
        }
    }
}

fn build_alert_from_change(
    change: &CandidateChange,
    now: i64,
    channels: Vec<types::ChannelKind>,
) -> types::AlertRecord {
    types::build_alert_for_validation(
        &change.candidate,
        change.validation_event.as_ref(),
        now,
        channels,
    )
}
