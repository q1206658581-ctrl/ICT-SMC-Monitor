//! M7 LLM decision sidecar.
//!
//! C2 alerts remain authoritative synchronous facts. Provider calls are a
//! best-effort asynchronous sidecar and can never gate alerts or notices.

mod context;
mod liquidity_targets;
mod pipeline;
mod provider;
mod smoke;
mod types;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::config::LlmConfig;
use crate::storage::SqliteStore;

pub use pipeline::{DispatchOutcome, LlmDecisionPipeline, LlmSingleCallStrategy};
pub use provider::{
    LlmProvider, MockLlmProvider, MockProviderReply, OpenAiCompatibleOptions,
    OpenAiCompatibleProvider, StructuredOutputMode,
};
pub use smoke::{run_provider_smoke, ProviderSmokeConfig, ProviderSmokeReport};
pub use types::*;

#[cfg(test)]
mod tests;
pub use context::*;

/// Build the optional production sidecar. Every failure is isolated to M7:
/// callers log it and continue booting the authoritative market pipeline.
pub fn build_configured_pipeline(
    config: &LlmConfig,
    store: SqliteStore,
) -> Result<Option<Arc<LlmDecisionPipeline>>> {
    if !config.enabled {
        tracing::info!("LLM sidecar disabled by configuration");
        return Ok(None);
    }
    config
        .validate()
        .map_err(|error| anyhow::anyhow!("invalid [llm] configuration: {error}"))?;
    let Some((api_key, api_key_source)) = config
        .resolve_api_key()
        .map_err(|error| anyhow::anyhow!("resolve LLM API key: {error}"))?
    else {
        tracing::warn!(
            env = %config.api_key_env,
            keychain_service = %crate::config::LLM_KEYCHAIN_SERVICE,
            "LLM sidecar disabled because API key is missing from environment and macOS Keychain"
        );
        return Ok(None);
    };
    tracing::info!(source = api_key_source.as_str(), "LLM API key resolved");
    let structured_output = match config.resolved_response_format() {
        "json_object" => StructuredOutputMode::JsonObject,
        "prompt_only" => StructuredOutputMode::PromptOnly,
        _ => StructuredOutputMode::JsonSchema,
    };
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleOptions {
        base_url: config.base_url.clone(),
        api_key,
        model: config.model.clone(),
        temperature: config.temperature,
        timeout: Duration::from_secs(config.timeout_secs),
        max_retries: config.max_retries,
        max_output_tokens: config.max_output_tokens,
        proxy_url: (!config.proxy_url.trim().is_empty()).then(|| config.proxy_url.clone()),
        structured_output,
        reasoning_effort: (!config.reasoning_effort.is_empty())
            .then(|| config.reasoning_effort.clone()),
    })?;
    let strategy = LlmSingleCallStrategy::new(
        Arc::new(provider),
        config.provider.clone(),
        Some(config.model.clone()),
    )
    .with_packer(ContextPacker::new(config.bars_per_tf));
    Ok(Some(Arc::new(
        LlmDecisionPipeline::new(store, Arc::new(strategy))
            .with_max_calls_per_day(config.max_calls_per_day),
    )))
}
