//! Alert types (M6b).
//!
//! Pure-fact records for per-symbol C2 alerts and later LTF validations.
//! §13 compliant: no entry/sl/tp/position/selected_symbol fields.

use crate::candidate::{
    effective_ltf_dir, CandidateSetup, ExpiryReason, SetupStatus, SymbolValidation,
};
use crate::detector::types::{Direction, ReversalConfirmKind, SmtDivergence};
use crate::types::Timeframe;
use serde::{Deserialize, Serialize};

/// Alert rule version.
pub const ALERT_RULE_VERSION: &str = "alert_v1";

/// What created the durable inbox record.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AlertTrigger {
    /// The DXY anchor and this executable trade symbol both confirmed C2.
    C2Confirmed,
    /// A later LTF MSS/CISD fact. Stored in Reversal Inbox; never notifies.
    Validated,
}

/// Which channels the alert was delivered to.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChannelKind {
    Inbox,
    DesktopNotify,
    FeishuNotify,
}

/// A fired alert record (§13 compliant: pure facts only).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlertRecord {
    pub id: String,
    /// Factual routing identity for multi-group monitoring (M6c).
    #[serde(default)]
    pub watchlist_id: String,
    pub candidate_id: String,
    pub smt_id: String,
    pub rule_version: String,
    pub trigger: AlertTrigger,
    pub setup_status: SetupStatus,
    /// Per-trade-symbol terminal reason. This is refreshed from the current
    /// Candidate lifecycle when Alert Inbox is queried and is not a separate
    /// alert-delivery fact.
    #[serde(default)]
    pub invalidation_reason: Option<ExpiryReason>,
    pub symbol_set: Vec<String>,
    pub sweeper_symbol: String,
    pub trade_symbols: Vec<String>,
    pub candidate_direction: Direction,
    pub deterministic_score: f32,
    pub c2_case: u8,
    pub context_timeframe: Timeframe,
    pub comparison_timeframe: Timeframe,
    pub validation_timeframe: Timeframe,
    pub c2_candle_ts: i64,
    pub c3_candle_ts: Option<i64>,
    /// Source SMT K and PDA audit snapshot used by the three-table funnel.
    #[serde(default)]
    pub smt_k_candle_ts: Option<i64>,
    #[serde(default)]
    pub context_pda_id: Option<String>,
    pub validation_kind: Option<ReversalConfirmKind>,
    pub validation_symbol: Option<String>,
    pub validation_ts: Option<i64>,
    /// Direction of the actual validation CISD/MSS on the trade symbol.
    pub validation_direction: Option<Direction>,
    pub channels_fired: Vec<ChannelKind>,
    pub created_at: i64,
}

/// Alert configuration (detector_config `alert` namespace).
#[derive(Clone, Debug)]
pub struct AlertConfig {
    pub enabled: bool,
    pub desktop_notify_enabled: bool,
    pub feishu_notify_enabled: bool,
    pub cooldown_seconds: i64,
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            desktop_notify_enabled: true,
            feishu_notify_enabled: false,
            cooldown_seconds: 0,
        }
    }
}

/// Build an AlertRecord from a validated candidate.
pub fn build_alert(c: &CandidateSetup, now_ms: i64, channels: Vec<ChannelKind>) -> AlertRecord {
    let validation = c.validation_symbol.as_ref().and_then(|symbol| {
        Some(SymbolValidation {
            event_id: c
                .validations
                .iter()
                .find(|validation| &validation.symbol == symbol)
                .map(|validation| validation.event_id.clone())
                .unwrap_or_else(|| {
                    format!("legacy-{symbol}-{}", c.validation_ts.unwrap_or(now_ms))
                }),
            symbol: symbol.clone(),
            kind: c.validation_kind?,
            direction: c.validation_direction?,
            ts: c.validation_ts?,
            price: c
                .validations
                .iter()
                .find(|validation| &validation.symbol == symbol)
                .map(|validation| validation.price)
                .unwrap_or_default(),
        })
    });
    build_alert_for_validation(c, validation.as_ref(), now_ms, channels)
}

pub fn build_alert_for_validation(
    c: &CandidateSetup,
    validation: Option<&SymbolValidation>,
    now_ms: i64,
    channels: Vec<ChannelKind>,
) -> AlertRecord {
    use crate::detector::types::structure_id;
    let validation_key = validation
        .map(|event| event.symbol.as_str())
        .or(c.validation_symbol.as_deref())
        .unwrap_or("unknown");
    let episode_start = c.observation_window.0.to_string();
    let direction = format!("{:?}", c.candidate_direction);
    let context_tf = c.context_timeframe.tag();
    let comparison_tf = c.comparison_timeframe.tag();
    let pda_id = c.context_pda_id.as_deref().unwrap_or("no-pda");
    AlertRecord {
        // Candidate IDs follow internal SMT attempts. Alert identity follows
        // the public episode so a stricter retry cannot notify twice.
        id: structure_id(&[
            "alert_episode",
            &c.smt_rule_version,
            &c.watchlist_id,
            pda_id,
            context_tf,
            comparison_tf,
            &direction,
            &episode_start,
            validation_key,
        ]),
        watchlist_id: c.watchlist_id.clone(),
        candidate_id: c.id.clone(),
        smt_id: c.smt_id.clone(),
        rule_version: ALERT_RULE_VERSION.into(),
        trigger: AlertTrigger::Validated,
        setup_status: c.setup_status,
        invalidation_reason: None,
        symbol_set: c.symbol_set.clone(),
        sweeper_symbol: c.sweeper_symbol.clone(),
        trade_symbols: c.trade_symbols.clone(),
        candidate_direction: c.candidate_direction,
        deterministic_score: c.deterministic_score,
        c2_case: c.c2_case,
        context_timeframe: c.context_timeframe,
        comparison_timeframe: c.comparison_timeframe,
        validation_timeframe: c.validation_timeframe,
        c2_candle_ts: c.c2_candle.ts,
        c3_candle_ts: c.c3_candle.as_ref().map(|c| c.ts),
        smt_k_candle_ts: Some(c.smt_k_candle.ts),
        context_pda_id: c.context_pda_id.clone(),
        validation_kind: validation.map(|event| event.kind).or(c.validation_kind),
        validation_symbol: validation
            .map(|event| event.symbol.clone())
            .or_else(|| c.validation_symbol.clone()),
        validation_ts: validation.map(|event| event.ts).or(c.validation_ts),
        validation_direction: validation
            .map(|event| event.direction)
            .or(c.validation_direction),
        channels_fired: channels,
        created_at: now_ms,
    }
}

/// Build one setup alert row for one executable trade symbol.
///
/// A public SMT episode can therefore create two independent rows (for
/// example EURUSD and GBPUSD), each with that symbol's own C2/C3 facts.
pub fn build_c2_alert(
    c: &CandidateSetup,
    smt: &SmtDivergence,
    trade_symbol: &str,
    alert_ts: i64,
    channels: Vec<ChannelKind>,
) -> Option<AlertRecord> {
    use crate::detector::types::structure_id;

    let sweeper_chain = smt
        .chains
        .iter()
        .find(|chain| chain.symbol == smt.sweeper_symbol)?;
    let trade_chain = smt
        .chains
        .iter()
        .find(|chain| chain.symbol == trade_symbol)?;
    // Candidate existence proves the sweeper C2 gate, but retaining this
    // explicit check prevents malformed historical rows from entering Inbox.
    sweeper_chain.c2_candle.as_ref()?;
    let trade_c2 = trade_chain.c2_candle.as_ref()?;

    let episode_start = c.observation_window.0.to_string();
    let direction = format!("{:?}", c.candidate_direction);
    let pda_id = c.context_pda_id.as_deref().unwrap_or("no-pda");
    let trade_direction = effective_ltf_dir(
        c.candidate_direction,
        trade_symbol,
        &c.sweeper_symbol,
        smt.relationship,
    );

    Some(AlertRecord {
        id: structure_id(&[
            "c2_alert_episode",
            &c.smt_rule_version,
            &c.watchlist_id,
            pda_id,
            c.context_timeframe.tag(),
            c.comparison_timeframe.tag(),
            &direction,
            &episode_start,
            trade_symbol,
        ]),
        watchlist_id: c.watchlist_id.clone(),
        candidate_id: c.id.clone(),
        smt_id: c.smt_id.clone(),
        rule_version: ALERT_RULE_VERSION.into(),
        trigger: AlertTrigger::C2Confirmed,
        setup_status: c.setup_status,
        invalidation_reason: None,
        symbol_set: c.symbol_set.clone(),
        sweeper_symbol: c.sweeper_symbol.clone(),
        trade_symbols: vec![trade_symbol.to_owned()],
        // For a per-trade-symbol alert this is the actionable EU/GU (or
        // AUD/NZD, CHF/CAD) direction, not the DXY anchor direction.
        candidate_direction: trade_direction,
        deterministic_score: c.deterministic_score,
        c2_case: trade_chain.c2_case.unwrap_or(0),
        context_timeframe: c.context_timeframe,
        comparison_timeframe: c.comparison_timeframe,
        validation_timeframe: c.validation_timeframe,
        c2_candle_ts: trade_c2.ts,
        c3_candle_ts: trade_chain.c3_candle.as_ref().map(|candle| candle.ts),
        smt_k_candle_ts: Some(trade_chain.smt_k_candle.ts),
        context_pda_id: c.context_pda_id.clone(),
        validation_kind: None,
        // The field remains the navigation symbol for wire compatibility.
        validation_symbol: Some(trade_symbol.to_owned()),
        validation_ts: None,
        validation_direction: Some(trade_direction),
        channels_fired: channels,
        // C2 alerts use the trade symbol's confirmation boundary (the C2
        // candle close), which is also the first moment it can be delivered.
        created_at: alert_ts,
    })
}
