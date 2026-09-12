use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::{Client, Proxy, StatusCode};
use serde::Deserialize;
use serde_json::json;

use super::{
    LlmDecisionRequest, LlmDecisionResponse, MAX_ADVISORY_TEXT_CHARS, MAX_EVIDENCE_STRUCTURE_IDS,
    MAX_MODEL_ADVISORY_ITEMS, MAX_REASONING_SUMMARY_CHARS,
};

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn decide(&self, request: LlmDecisionRequest) -> Result<LlmDecisionResponse>;
}

const SYSTEM_PROMPT: &str = include_str!("../../../prompts/system.md");
const STRATEGY_PROMPT: &str = include_str!("../../../prompts/strategy_m7_v1.md");

// Intentionally not `Debug`: options contain the API key before it is moved
// into the provider's private in-memory storage.
#[derive(Clone)]
pub struct OpenAiCompatibleOptions {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f64,
    pub timeout: Duration,
    pub max_retries: usize,
    /// Upper bound sent as OpenAI-compatible `max_tokens`. Keeping this
    /// explicit avoids provider-specific low defaults truncating valid JSON.
    pub max_output_tokens: u32,
    pub proxy_url: Option<String>,
    pub structured_output: StructuredOutputMode,
    pub reasoning_effort: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructuredOutputMode {
    JsonSchema,
    JsonObject,
    /// Put the schema in the system prompt but omit the wire-level
    /// `response_format` field for gateways/models that reject it.
    PromptOnly,
}

/// OpenAI-compatible `chat/completions` provider. The secret is held only in
/// memory and this type intentionally does not implement `Debug`.
#[derive(Clone)]
pub struct OpenAiCompatibleProvider {
    client: Client,
    endpoint: String,
    api_key: Arc<str>,
    model: String,
    temperature: f64,
    max_retries: usize,
    max_output_tokens: u32,
    structured_output: StructuredOutputMode,
    reasoning_effort: Option<String>,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
    usage: Option<CompletionUsage>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct CompletionUsage {
    completion_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: Option<String>,
}

impl OpenAiCompatibleProvider {
    pub fn new(options: OpenAiCompatibleOptions) -> Result<Self> {
        let mut builder = Client::builder().timeout(options.timeout);
        if let Some(proxy_url) = options
            .proxy_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            builder = builder.proxy(Proxy::all(proxy_url)?);
        }
        let endpoint = format!(
            "{}/chat/completions",
            options.base_url.trim().trim_end_matches('/')
        );
        Ok(Self {
            client: builder.build()?,
            endpoint,
            api_key: Arc::from(options.api_key),
            model: options.model,
            temperature: options.temperature,
            max_retries: options.max_retries,
            max_output_tokens: options.max_output_tokens,
            structured_output: options.structured_output,
            reasoning_effort: options.reasoning_effort,
        })
    }

    fn response_schema() -> serde_json::Value {
        json!({
            "name": "ict_llm_decision",
            "strict": true,
            "schema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["candidate_id", "alert", "direction", "confidence", "quality",
                    "reasoning_summary", "evidence_structure_ids", "invalidation_price",
                    "entry_zone", "targets", "risk_reward", "should_wait_for", "warnings"],
                "properties": {
                    "candidate_id": {"type": "string"},
                    "alert": {"type": "boolean"},
                    "direction": {"enum": ["bullish", "bearish", "neutral"]},
                    "confidence": {"type": "integer", "minimum": 0, "maximum": 100},
                    "quality": {"enum": ["A", "B", "C", "skip"]},
                    "reasoning_summary": {"type": "string", "minLength": 1, "maxLength": MAX_REASONING_SUMMARY_CHARS},
                    "evidence_structure_ids": {"type": "array", "maxItems": MAX_EVIDENCE_STRUCTURE_IDS,
                        "uniqueItems": true, "items": {"type": "string", "minLength": 1}},
                    "invalidation_price": {"type": ["number", "null"]},
                    "entry_zone": {
                        "anyOf": [
                            {"type": "null"},
                            {"type": "object", "additionalProperties": false,
                             "required": ["low", "high", "source"],
                             "properties": {"low": {"type": "number"}, "high": {"type": "number"}, "source": {"type": "string"}}}
                        ]
                    },
                    "targets": {"type": "array", "items": {"type": "object", "additionalProperties": false,
                        "required": ["price", "reason"], "properties": {"price": {"type": "number"}, "reason": {"type": "string"}}}},
                    "risk_reward": {"type": ["number", "null"]},
                    "should_wait_for": {"type": "array", "maxItems": MAX_MODEL_ADVISORY_ITEMS,
                        "items": {"type": "string", "minLength": 1, "maxLength": MAX_ADVISORY_TEXT_CHARS}},
                    "warnings": {"type": "array", "maxItems": MAX_MODEL_ADVISORY_ITEMS,
                        "items": {"type": "string", "minLength": 1, "maxLength": MAX_ADVISORY_TEXT_CHARS}}
                }
            }
        })
    }

    fn transient(error: &reqwest::Error) -> bool {
        error.is_connect()
            || error.is_timeout()
            || error.is_decode()
            || error.status().is_some_and(|status| {
                matches!(
                    status,
                    StatusCode::REQUEST_TIMEOUT
                        | StatusCode::CONFLICT
                        | StatusCode::TOO_MANY_REQUESTS
                ) || status.is_server_error()
            })
    }

    async fn call_once(&self, request: &LlmDecisionRequest) -> Result<LlmDecisionResponse> {
        let context = serde_json::to_string(request)?;
        if context.len() > 196_608 {
            return Err(anyhow!(
                "LLM context exceeds local 192 KiB input budget ({} bytes); request not sent",
                context.len()
            ));
        }
        let mut messages = vec![
            json!({"role": "system", "content": SYSTEM_PROMPT}),
            json!({"role": "system", "content": STRATEGY_PROMPT}),
        ];
        if matches!(
            self.structured_output,
            StructuredOutputMode::JsonObject | StructuredOutputMode::PromptOnly
        ) {
            let schema = Self::response_schema()["schema"].clone();
            messages.push(json!({
                "role": "system",
                "content": format!(
                    "只返回一个 JSON 对象，不得输出 Markdown 或额外文字。JSON 必须严格满足以下 schema；字段不得缺失，也不得增加字段：{}",
                    schema
                )
            }));
        }
        messages.push(json!({"role": "user", "content": context}));
        let mut body = json!({
            "model": self.model,
            "temperature": self.temperature,
            "max_tokens": self.max_output_tokens,
            "messages": messages
        });
        if let Some(effort) = &self.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        match self.structured_output {
            StructuredOutputMode::JsonSchema => {
                body["response_format"] = json!({
                    "type": "json_schema",
                    "json_schema": Self::response_schema()
                });
            }
            StructuredOutputMode::JsonObject => {
                body["response_format"] = json!({"type": "json_object"});
            }
            StructuredOutputMode::PromptOnly => {}
        }
        let mut response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(self.api_key.as_ref())
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                let kind = if error.is_timeout() {
                    "LLM request timed out"
                } else if error.is_connect() {
                    "LLM connection failed"
                } else {
                    "LLM transport failed"
                };
                anyhow::Error::new(error.without_url()).context(kind)
            })?;
        if let Err(status_error) = response.error_for_status_ref() {
            // Preserve reqwest as the source so status-based retry policy remains intact.
            // Bound error reads; never persist an arbitrary HTML page or echoed request.
            let mut bytes = Vec::new();
            while bytes.len() < 16_384 {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        let remaining = 16_384 - bytes.len();
                        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                    }
                    _ => break,
                }
            }
            let detail = safe_error_detail(&bytes, self.api_key.as_ref());
            let status = response.status();
            return Err(anyhow::Error::new(status_error.without_url())
                .context(format!("HTTP {status}; {detail}")));
        }
        let response: ChatCompletionResponse = response.json().await?;
        let choice = response.choices.into_iter().next();
        let finish_reason = choice
            .as_ref()
            .and_then(|v| v.finish_reason.as_deref())
            .unwrap_or("unknown");
        let raw_response = choice
            .as_ref()
            .and_then(|v| v.message.content.as_deref())
            .unwrap_or_default();
        if raw_response.trim().is_empty() {
            return Err(anyhow!("LLM returned empty content; finish_reason={}; completion_tokens={:?}; max_output_tokens={}; reasoning_effort={}",
                safe_diagnostic_text(finish_reason, self.api_key.as_ref(), 64),
                response.usage.and_then(|v| v.completion_tokens), self.max_output_tokens,
                self.reasoning_effort.as_deref().unwrap_or("provider default")));
        }
        let raw_response = raw_response.to_owned();
        Ok(LlmDecisionResponse { raw_response })
    }

    fn valid_local_output(request: &LlmDecisionRequest, raw_response: &str) -> bool {
        serde_json::from_str::<super::LlmDecision>(raw_response)
            .ok()
            .is_some_and(|decision| decision.apply_guardrails(&request.context).is_ok())
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleProvider {
    async fn decide(&self, request: LlmDecisionRequest) -> Result<LlmDecisionResponse> {
        let mut attempt = 0usize;
        loop {
            match self.call_once(&request).await {
                Ok(response) => {
                    if Self::valid_local_output(&request, &response.raw_response)
                        || attempt >= self.max_retries
                    {
                        // Exhausted invalid output is returned to the pipeline
                        // so the original body remains available for auditing.
                        return Ok(response);
                    }
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
                Err(error) => {
                    let retry = error
                        .downcast_ref::<reqwest::Error>()
                        .is_some_and(Self::transient)
                        && attempt < self.max_retries;
                    if !retry {
                        return Err(error);
                    }
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            }
        }
    }
}

// Redact the complete input before truncation, so a long echoed key cannot
// survive as a partial prefix. Shared by status errors and empty-body diagnostics.
fn safe_diagnostic_text(text: &str, api_key: &str, limit: usize) -> String {
    let redacted = if api_key.is_empty() { text.to_owned() } else { text.replace(api_key, "[REDACTED]") };
    redacted.chars().filter(|c| !c.is_control()).take(limit).collect()
}

// Only selected diagnostic fields are retained; unknown bodies are omitted.
fn safe_error_detail(bytes: &[u8], api_key: &str) -> String {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return "provider error body unavailable or non-JSON".into();
    };
    let error = value.get("error").unwrap_or(&value);
    let mut fields = Vec::new();
    for name in ["code", "type", "param", "message"] {
        if let Some(text) = error.get(name).and_then(|v| v.as_str()) {
            let clean = safe_diagnostic_text(text, api_key, 1000);
            fields.push(format!("{name}={clean}"));
        }
    }
    if fields.is_empty() {
        "provider supplied no diagnostic fields".into()
    } else {
        fields.join("; ")
    }
}

#[derive(Clone, Debug)]
pub enum MockProviderReply {
    Json(String),
    Error(String),
}

/// Deterministic, injectable M7a provider. It performs no network I/O.
#[derive(Clone, Debug)]
pub struct MockLlmProvider {
    reply: MockProviderReply,
    delay: Duration,
    calls: Arc<AtomicUsize>,
}

impl MockLlmProvider {
    pub fn new(reply: MockProviderReply) -> Self {
        Self {
            reply,
            delay: Duration::ZERO,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmProvider for MockLlmProvider {
    async fn decide(&self, _request: LlmDecisionRequest) -> Result<LlmDecisionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        match &self.reply {
            MockProviderReply::Json(raw_response) => Ok(LlmDecisionResponse {
                raw_response: raw_response.clone(),
            }),
            MockProviderReply::Error(message) => Err(anyhow!(message.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_details_are_redacted_bounded_and_allowlisted() {
        let body = json!({"error": {"code": "InvalidModel", "message": "bad secret-key\nmodel",
            "request": "private context", "headers": "secret-key"}});
        let detail = safe_error_detail(body.to_string().as_bytes(), "secret-key");
        assert_eq!(detail, "code=InvalidModel; message=bad [REDACTED]model");
        assert!(!safe_error_detail(b"<html>private</html>", "key").contains("private"));
        let long = json!({"error": {"message": "界".repeat(2000)}});
        assert_eq!(
            safe_error_detail(long.to_string().as_bytes(), "key")
                .chars()
                .count(),
            1008
        );
    }

    #[test]
    fn response_schema_bounds_model_authored_text() {
        let schema = OpenAiCompatibleProvider::response_schema();
        let properties = &schema["schema"]["properties"];

        assert_eq!(
            properties["reasoning_summary"]["maxLength"].as_u64(),
            Some(MAX_REASONING_SUMMARY_CHARS as u64)
        );
        assert_eq!(
            properties["evidence_structure_ids"]["maxItems"].as_u64(),
            Some(MAX_EVIDENCE_STRUCTURE_IDS as u64)
        );
        for field in ["should_wait_for", "warnings"] {
            assert_eq!(
                properties[field]["maxItems"].as_u64(),
                Some(MAX_MODEL_ADVISORY_ITEMS as u64)
            );
            assert_eq!(
                properties[field]["items"]["maxLength"].as_u64(),
                Some(MAX_ADVISORY_TEXT_CHARS as u64)
            );
        }
    }
}
