use ict_monitor::llm::DeterministicDecisionGuardrails;
use serde::{Deserialize, Serialize};

pub const VERSION: &str = "m8-lite-v8";
pub const MINUTE: i64 = 60_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitMode {
    #[default]
    Targets,
    FixedR,
    UserV1,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FixedRResult {
    pub multiple: u8,
    pub simulation: Simulation,
}

pub const USER_BUFFERS: [u8; 3] = [0, 5, 10];
pub const USER_MULTIPLES: [f64; 4] = [1.0, 1.5, 2.0, 3.0];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserResult {
    pub buffer_points: u8,
    pub multiple: f64,
    pub stop: Option<f64>,
    pub target: Option<f64>,
    pub target1_in_user_r: Option<f64>,
    pub simulation: Simulation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnchorGap {
    pub category: String,
    pub expected_ts: i64,
    pub first_m1: Option<i64>,
    pub last_m1: Option<i64>,
    pub previous_m1: Option<i64>,
    pub next_m1: Option<i64>,
    pub comparison_close_available: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Parameters {
    pub version: String,
    pub source: String,
    pub cohort: String,
    pub entry_bars: usize,
    pub holding_bars: Option<usize>,
    pub ttl_mtf_bars: usize,
    #[serde(default)]
    pub mode: ExitMode,
    #[serde(default)]
    pub guardrail_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Win,
    Loss,
    Skip,
    Ambiguous,
    NoFill,
    Expired,
    DataGap,
    InsufficientData,
    Excluded,
}
impl Outcome {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Win => "win",
            Self::Loss => "loss",
            Self::Skip => "skip",
            Self::Ambiguous => "ambiguous",
            Self::NoFill => "no_fill",
            Self::Expired => "expired",
            Self::DataGap => "data_gap",
            Self::InsufficientData => "insufficient_data",
            Self::Excluded => "excluded",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Simulation {
    pub outcome: Outcome,
    pub reason: String,
    pub entry: Option<f64>,
    pub entry_ts: Option<i64>,
    pub exit_ts: Option<i64>,
    pub realized_rr: Option<f64>,
    pub floating_rr: Option<f64>,
    pub mfe_rr: Option<f64>,
    pub mae_rr: Option<f64>,
    pub time_to_entry_ms: Option<i64>,
    pub time_to_target_ms: Option<i64>,
    pub time_to_stop_ms: Option<i64>,
}
impl Simulation {
    pub fn empty(outcome: Outcome, reason: impl Into<String>) -> Self {
        Self {
            outcome,
            reason: reason.into(),
            entry: None,
            entry_ts: None,
            exit_ts: None,
            realized_rr: None,
            floating_rr: None,
            mfe_rr: None,
            mae_rr: None,
            time_to_entry_ms: None,
            time_to_target_ms: None,
            time_to_stop_ms: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trade {
    pub alert_id: String,
    pub candidate_id: String,
    pub smt_id: String,
    pub watchlist_id: String,
    pub symbol: String,
    pub as_of_ts: i64,
    pub direction: String,
    pub confidence: Option<u8>,
    pub quality: String,
    pub session: String,
    pub weekly_slot: String,
    pub holding_bars: usize,
    pub data_issues: Vec<String>,
    pub available_future_m1: usize,
    pub guardrails: Option<DeterministicDecisionGuardrails>,
    pub evidence_hash: Option<String>,
    pub simulation: Simulation,
    #[serde(default)]
    pub fixed_r: Vec<FixedRResult>,
    #[serde(default)]
    pub anchor_gap: Option<AnchorGap>,
    #[serde(default)]
    pub user_v1: Vec<UserResult>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evaluation {
    pub run_id: String,
    pub parameters: Parameters,
    pub source_cutoff_ts: i64,
    #[serde(default)]
    pub symbol_cutoffs: std::collections::BTreeMap<String, i64>,
    pub c2_count: usize,
    pub other_alert_count: usize,
    pub trades: Vec<Trade>,
}
