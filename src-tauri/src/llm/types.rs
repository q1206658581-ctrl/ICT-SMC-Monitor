use serde::{Deserialize, Serialize};

use crate::candidate::DecisionLogEntry;
use crate::types::Timeframe;

pub const M7A_PROMPT_VERSION: &str = "m7a.mock.v1";
pub const M7A_STRATEGY_VERSION: &str = "m7a.mock.v1";
pub const M7A_CONTEXT_VERSION: &str = "m7a.mock.v1";
pub const M7B_CONTEXT_VERSION: &str = "m7b.context.v1";
pub const M7C_PROMPT_VERSION: &str = "m7c.prompt.v1";
pub const M7C_STRATEGY_VERSION: &str = "m7c.single_call.v1";
pub const M7D_PROMPT_VERSION: &str = "m7d.prompt.v1";
pub const M7D_STRATEGY_VERSION: &str = "m7d.decision_rules.v2";
pub const M7D_CONTEXT_VERSION: &str = "m7d.context.v1";
pub const MAX_REASONING_SUMMARY_CHARS: usize = 1_200;
pub const MAX_ADVISORY_TEXT_CHARS: usize = 240;
pub const MAX_MODEL_ADVISORY_ITEMS: usize = 12;
pub const MAX_FINAL_ADVISORY_ITEMS: usize = 16;
pub const MAX_EVIDENCE_STRUCTURE_IDS: usize = 64;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LlmDecisionDirection {
    Bullish,
    Bearish,
    Neutral,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum LlmDecisionQuality {
    A,
    B,
    C,
    #[serde(rename = "skip")]
    Skip,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LlmEntryZone {
    pub low: f64,
    pub high: f64,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LlmTarget {
    pub price: f64,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecisionQualityRubric {
    pub quality_a_min_confidence: u8,
    pub quality_b_min_confidence: u8,
    pub quality_c_min_confidence: u8,
    pub alert_min_confidence: u8,
}

impl Default for DecisionQualityRubric {
    fn default() -> Self {
        Self {
            quality_a_min_confidence: 80,
            quality_b_min_confidence: 65,
            quality_c_min_confidence: 50,
            alert_min_confidence: 50,
        }
    }
}

/// Values owned by the deterministic engine. The LLM may explain or decline
/// an otherwise eligible setup, but it cannot replace these values.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DeterministicDecisionGuardrails {
    pub rule_version: String,
    pub direction: LlmDecisionDirection,
    pub confidence: u8,
    pub quality: LlmDecisionQuality,
    pub alert_eligible: bool,
    pub entry_zone: Option<LlmEntryZone>,
    pub invalidation_price: Option<f64>,
    pub targets: Vec<LlmTarget>,
    pub risk_reward: Option<f64>,
    pub required_evidence_ids: Vec<String>,
    pub allowed_evidence_ids: Vec<String>,
    pub should_wait_for: Vec<String>,
    pub warnings: Vec<String>,
    pub calculation_notes: Vec<String>,
    pub quality_rubric: DecisionQualityRubric,
}

impl Default for DeterministicDecisionGuardrails {
    fn default() -> Self {
        Self {
            rule_version: String::new(),
            direction: LlmDecisionDirection::Neutral,
            confidence: 0,
            quality: LlmDecisionQuality::Skip,
            alert_eligible: false,
            entry_zone: None,
            invalidation_price: None,
            targets: Vec::new(),
            risk_reward: None,
            required_evidence_ids: Vec::new(),
            allowed_evidence_ids: Vec::new(),
            should_wait_for: Vec::new(),
            warnings: Vec::new(),
            calculation_notes: Vec::new(),
            quality_rubric: DecisionQualityRubric::default(),
        }
    }
}

/// Strict M7 decision schema. Descriptive values are expected to be Chinese;
/// JSON keys remain stable English wire names.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LlmDecision {
    pub candidate_id: String,
    pub alert: bool,
    pub direction: LlmDecisionDirection,
    pub confidence: u8,
    pub quality: LlmDecisionQuality,
    pub reasoning_summary: String,
    pub evidence_structure_ids: Vec<String>,
    pub invalidation_price: Option<f64>,
    pub entry_zone: Option<LlmEntryZone>,
    pub targets: Vec<LlmTarget>,
    pub risk_reward: Option<f64>,
    pub should_wait_for: Vec<String>,
    pub warnings: Vec<String>,
}

impl LlmDecision {
    pub fn validate(&self, expected_candidate_id: &str) -> Result<(), String> {
        if self.candidate_id != expected_candidate_id {
            return Err(format!(
                "candidate_id mismatch: expected {expected_candidate_id}, got {}",
                self.candidate_id
            ));
        }
        if self.confidence > 100 {
            return Err("confidence must be between 0 and 100".into());
        }
        if let Some(zone) = &self.entry_zone {
            if !zone.low.is_finite() || !zone.high.is_finite() || zone.low > zone.high {
                return Err("entry_zone must contain finite low <= high".into());
            }
        }
        if self
            .invalidation_price
            .is_some_and(|price| !price.is_finite())
        {
            return Err("invalidation_price must be finite".into());
        }
        if self.targets.iter().any(|target| !target.price.is_finite()) {
            return Err("target price must be finite".into());
        }
        if self
            .risk_reward
            .is_some_and(|ratio| !ratio.is_finite() || ratio < 0.0)
        {
            return Err("risk_reward must be finite and non-negative".into());
        }
        Ok(())
    }

    /// Apply the deterministic contract before a model response is persisted.
    /// Legacy M7c contexts have no guardrail version and retain their old
    /// validation behaviour for audit/UI compatibility.
    pub fn apply_guardrails(mut self, context: &super::LlmDecisionContext) -> Result<Self, String> {
        self.validate(&context.identity.candidate_id)?;
        let guardrails = &context.deterministic_guardrails;
        if guardrails.rule_version.is_empty() {
            self.normalize_free_text_fields(&[]);
            return Ok(self);
        }

        if self.reasoning_summary.trim().is_empty() {
            return Err("reasoning_summary must not be empty".into());
        }
        if let Some(unknown) = self
            .evidence_structure_ids
            .iter()
            .find(|id| !guardrails.allowed_evidence_ids.contains(id))
        {
            return Err(format!("unknown evidence_structure_id: {unknown}"));
        }
        if self.alert {
            if let Some(missing) = guardrails
                .required_evidence_ids
                .iter()
                .find(|id| !self.evidence_structure_ids.contains(id))
            {
                return Err(format!("missing required evidence_structure_id: {missing}"));
            }
        }

        let model_values_differ = self.direction != guardrails.direction
            || self.confidence != guardrails.confidence
            || self.quality != guardrails.quality
            || self.entry_zone != guardrails.entry_zone
            || self.invalidation_price != guardrails.invalidation_price
            || self.targets != guardrails.targets
            || self.risk_reward != guardrails.risk_reward;

        self.alert &= guardrails.alert_eligible;
        self.direction = guardrails.direction;
        self.confidence = guardrails.confidence;
        self.quality = guardrails.quality;
        self.entry_zone.clone_from(&guardrails.entry_zone);
        self.invalidation_price = guardrails.invalidation_price;
        self.targets.clone_from(&guardrails.targets);
        self.risk_reward = guardrails.risk_reward;
        let mut deterministic_waits = guardrails.should_wait_for.clone();
        let mut deterministic_warnings = guardrails.warnings.clone();
        if model_values_differ {
            deterministic_warnings.push("模型数值字段已由本地确定性决策规则归一化".into());
        }
        if !guardrails.alert_eligible {
            deterministic_waits.push("等待确定性决策护栏满足后再人工关注".into());
        }
        self.should_wait_for = merge_bounded_advisories(self.should_wait_for, deterministic_waits);
        self.warnings = merge_bounded_advisories(self.warnings, deterministic_warnings);
        self.normalize_free_text_fields(&guardrails.required_evidence_ids);
        self.validate(&context.identity.candidate_id)?;
        Ok(self)
    }

    fn normalize_free_text_fields(&mut self, required_evidence_ids: &[String]) {
        self.reasoning_summary =
            truncate_chars(self.reasoning_summary.trim(), MAX_REASONING_SUMMARY_CHARS);
        self.evidence_structure_ids = bounded_evidence_ids(
            std::mem::take(&mut self.evidence_structure_ids),
            required_evidence_ids,
        );
        self.should_wait_for = normalize_advisories(std::mem::take(&mut self.should_wait_for));
        self.warnings = normalize_advisories(std::mem::take(&mut self.warnings));
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    if max_chars == 0 {
        return String::new();
    }
    let mut truncated = value.chars().take(max_chars - 1).collect::<String>();
    truncated.push('…');
    truncated
}

fn normalize_advisories(values: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for value in values {
        let value = truncate_chars(value.trim(), MAX_ADVISORY_TEXT_CHARS);
        if !value.is_empty() && !normalized.contains(&value) {
            normalized.push(value);
            if normalized.len() == MAX_FINAL_ADVISORY_ITEMS {
                break;
            }
        }
    }
    normalized
}

fn merge_bounded_advisories(model: Vec<String>, deterministic: Vec<String>) -> Vec<String> {
    let mut merged = normalize_advisories(deterministic);
    for value in normalize_advisories(model)
        .into_iter()
        .take(MAX_MODEL_ADVISORY_ITEMS)
    {
        if !merged.contains(&value) && merged.len() < MAX_FINAL_ADVISORY_ITEMS {
            merged.push(value);
        }
    }
    merged
}

fn bounded_evidence_ids(values: Vec<String>, required: &[String]) -> Vec<String> {
    let mut bounded = Vec::new();
    for required_id in required {
        if values.contains(required_id) && !bounded.contains(required_id) {
            bounded.push(required_id.clone());
            if bounded.len() == MAX_EVIDENCE_STRUCTURE_IDS {
                return bounded;
            }
        }
    }
    for value in values {
        if !value.is_empty() && !bounded.contains(&value) {
            bounded.push(value);
            if bounded.len() == MAX_EVIDENCE_STRUCTURE_IDS {
                break;
            }
        }
    }
    bounded
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LlmDecisionRequest {
    pub context: super::LlmDecisionContext,
}

#[derive(Clone, Debug)]
pub struct LlmDecisionResponse {
    pub raw_response: String,
}

/// Product-facing lifecycle for one durable LLM sidecar call.
///
/// This is deliberately separate from `DecisionLogEntry`: the audit row owns
/// request/response payloads, while the UI may only receive the validated
/// structured decision and a bounded error summary.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LlmDecisionUiStatus {
    Pending,
    Approved,
    Rejected,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LlmDecisionListItem {
    pub id: String,
    pub candidate_id: String,
    pub watchlist_id: String,
    pub alert_id: Option<String>,
    pub trade_symbol: Option<String>,
    pub market_anchor_ts: i64,
    pub validation_timeframe: Option<Timeframe>,
    pub provider: String,
    pub model: Option<String>,
    pub created_at: i64,
    pub status: LlmDecisionUiStatus,
    pub decision: Option<LlmDecision>,
    pub error_summary: Option<String>,
}

impl LlmDecisionListItem {
    pub fn from_log_entry(entry: DecisionLogEntry) -> Self {
        let request = serde_json::from_str::<LlmDecisionRequest>(&entry.request_json).ok();
        let expected_candidate_id = request
            .as_ref()
            .map(|request| request.context.identity.candidate_id.as_str())
            .filter(|candidate_id| !candidate_id.is_empty())
            .unwrap_or(&entry.candidate_id);
        let parsed = entry
            .parse_ok
            .then(|| serde_json::from_str::<LlmDecision>(&entry.parsed_decision_json).ok())
            .flatten()
            .and_then(|mut decision| {
                if let Some(request) = request.as_ref() {
                    decision.apply_guardrails(&request.context).ok()
                } else if decision.validate(expected_candidate_id).is_ok() {
                    // Legacy rows may predate the persisted M7 request context.
                    // They still need a strict presentation boundary so stored
                    // model text cannot grow the UI payload without limit.
                    decision.normalize_free_text_fields(&[]);
                    Some(decision)
                } else {
                    None
                }
            });
        let pending = !entry.parse_ok && entry.raw_response.is_none() && entry.error.is_none();
        let status = if pending {
            LlmDecisionUiStatus::Pending
        } else if let Some(decision) = parsed.as_ref() {
            if decision.alert {
                LlmDecisionUiStatus::Approved
            } else {
                LlmDecisionUiStatus::Rejected
            }
        } else {
            LlmDecisionUiStatus::Error
        };
        let error_summary = (status == LlmDecisionUiStatus::Error).then(|| {
            let message = entry.error.as_deref().unwrap_or(if entry.parse_ok {
                "结构化决策无法通过本地校验"
            } else {
                "模型响应无法解析为结构化决策"
            });
            bounded_summary(message, 240)
        });
        let identity = request.as_ref().map(|request| &request.context.identity);
        let validation_timeframe = request
            .as_ref()
            .map(|request| request.context.l2_strategy_reading.validation_timeframe);

        Self {
            id: entry.id,
            candidate_id: entry.candidate_id,
            watchlist_id: if entry.watchlist_id.is_empty() {
                identity
                    .map(|value| value.watchlist_id.clone())
                    .unwrap_or_default()
            } else {
                entry.watchlist_id
            },
            alert_id: entry.alert_id.or_else(|| {
                identity
                    .map(|value| value.alert_id.clone())
                    .filter(|value| !value.is_empty())
            }),
            trade_symbol: entry.trade_symbol.or_else(|| {
                identity
                    .map(|value| value.trade_symbol.clone())
                    .filter(|value| !value.is_empty())
            }),
            market_anchor_ts: identity
                .map(|value| value.as_of_ts)
                .unwrap_or(entry.created_at),
            validation_timeframe,
            provider: entry.provider,
            model: entry.model,
            created_at: entry.created_at,
            status,
            decision: parsed,
            error_summary,
        }
    }
}

/// Stable product ordering for the Decisions table.
///
/// The final identity keys are intentional: provider polling may return rows
/// with identical market and creation timestamps, and a partial ordering would
/// otherwise make equal rows jump between refreshes.
pub fn sort_llm_decision_items(items: &mut [LlmDecisionListItem]) {
    items.sort_by(|a, b| {
        b.market_anchor_ts
            .cmp(&a.market_anchor_ts)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| a.trade_symbol.cmp(&b.trade_symbol))
            .then_with(|| a.alert_id.cmp(&b.alert_id))
            .then_with(|| a.id.cmp(&b.id))
    });
}

fn bounded_summary(value: &str, max_chars: usize) -> String {
    truncate_chars(value.trim(), max_chars)
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use crate::candidate::DecisionMode;

    fn log_entry(request_json: String) -> DecisionLogEntry {
        DecisionLogEntry {
            id: "decision-1".into(),
            candidate_id: "candidate-1".into(),
            watchlist_id: "eu-gu-dxy".into(),
            alert_id: Some("alert-1".into()),
            trade_symbol: Some("OANDA:EURUSD".into()),
            parent_id: None,
            provider: "mock".into(),
            model: Some("mock-v1".into()),
            decision_mode: DecisionMode::LlmSingleCall,
            prompt_version: Some(M7C_PROMPT_VERSION.into()),
            strategy_version: M7C_STRATEGY_VERSION.into(),
            context_version: M7B_CONTEXT_VERSION.into(),
            request_json,
            raw_response: None,
            parsed_decision_json: String::new(),
            parse_ok: false,
            error: None,
            created_at: 123,
        }
    }

    #[test]
    fn pending_ui_item_never_exposes_audit_payloads() {
        let item = LlmDecisionListItem::from_log_entry(log_entry("{}".into()));
        assert_eq!(item.status, LlmDecisionUiStatus::Pending);
        assert!(item.decision.is_none());
        let json = serde_json::to_string(&item).unwrap();
        assert!(!json.contains("request_json"));
        assert!(!json.contains("raw_response"));
    }

    #[test]
    fn invalid_structured_output_is_an_error_not_an_approval() {
        let mut entry = log_entry("{}".into());
        entry.parse_ok = true;
        entry.parsed_decision_json = "{\"alert\":true}".into();
        let item = LlmDecisionListItem::from_log_entry(entry);
        assert_eq!(item.status, LlmDecisionUiStatus::Error);
        assert!(item.error_summary.is_some());
    }

    #[test]
    fn historical_decision_text_is_bounded_before_reaching_ui() {
        let mut entry = log_entry("{}".into());
        entry.parse_ok = true;
        let decision = LlmDecision {
            candidate_id: "candidate-1".into(),
            alert: false,
            direction: LlmDecisionDirection::Neutral,
            confidence: 0,
            quality: LlmDecisionQuality::Skip,
            reasoning_summary: "推".repeat(MAX_REASONING_SUMMARY_CHARS + 100),
            evidence_structure_ids: (0..80)
                .map(|index| format!("evidence-{index:03}"))
                .collect(),
            invalidation_price: None,
            entry_zone: None,
            targets: Vec::new(),
            risk_reward: None,
            should_wait_for: (0..20)
                .map(|index| format!("等待-{index:02}-{}", "长".repeat(300)))
                .collect(),
            warnings: (0..20)
                .map(|index| format!("风险-{index:02}-{}", "长".repeat(300)))
                .collect(),
        };
        entry.parsed_decision_json = serde_json::to_string(&decision).unwrap();

        let item = LlmDecisionListItem::from_log_entry(entry);
        let decision = item.decision.expect("valid legacy decision");
        assert!(decision.reasoning_summary.chars().count() <= MAX_REASONING_SUMMARY_CHARS);
        assert!(decision.evidence_structure_ids.len() <= MAX_EVIDENCE_STRUCTURE_IDS);
        assert!(decision.should_wait_for.len() <= MAX_FINAL_ADVISORY_ITEMS);
        assert!(decision.warnings.len() <= MAX_FINAL_ADVISORY_ITEMS);
        assert!(decision
            .should_wait_for
            .iter()
            .chain(&decision.warnings)
            .all(|value| value.chars().count() <= MAX_ADVISORY_TEXT_CHARS));
    }

    #[test]
    fn display_order_is_deterministic_when_timestamps_tie() {
        let mut items = [
            LlmDecisionListItem {
                id: "decision-b".into(),
                candidate_id: "candidate-1".into(),
                watchlist_id: "eu-gu-dxy".into(),
                alert_id: Some("alert-b".into()),
                trade_symbol: Some("OANDA:GBPUSD".into()),
                market_anchor_ts: 200,
                validation_timeframe: Some(Timeframe::M30),
                provider: "mock".into(),
                model: Some("mock-v1".into()),
                created_at: 300,
                status: LlmDecisionUiStatus::Pending,
                decision: None,
                error_summary: None,
            },
            LlmDecisionListItem {
                id: "decision-a".into(),
                candidate_id: "candidate-2".into(),
                watchlist_id: "eu-gu-dxy".into(),
                alert_id: Some("alert-a".into()),
                trade_symbol: Some("OANDA:EURUSD".into()),
                market_anchor_ts: 200,
                validation_timeframe: Some(Timeframe::M30),
                provider: "mock".into(),
                model: Some("mock-v1".into()),
                created_at: 300,
                status: LlmDecisionUiStatus::Pending,
                decision: None,
                error_summary: None,
            },
            LlmDecisionListItem {
                id: "decision-c".into(),
                candidate_id: "candidate-3".into(),
                watchlist_id: "eu-gu-dxy".into(),
                alert_id: Some("alert-c".into()),
                trade_symbol: Some("OANDA:AUDUSD".into()),
                market_anchor_ts: 100,
                validation_timeframe: Some(Timeframe::M30),
                provider: "mock".into(),
                model: Some("mock-v1".into()),
                created_at: 400,
                status: LlmDecisionUiStatus::Pending,
                decision: None,
                error_summary: None,
            },
        ];

        sort_llm_decision_items(&mut items);
        assert_eq!(
            items.map(|item| item.id),
            ["decision-a", "decision-b", "decision-c"]
        );
    }
}
