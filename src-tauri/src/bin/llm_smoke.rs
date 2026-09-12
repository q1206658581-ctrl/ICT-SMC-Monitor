//! Isolated M7c real-provider acceptance command.
//!
//! Example (the key is read from the named environment variable only):
//! ICT_LLM_API_KEY='...' cargo run -p ict-monitor --bin llm-smoke

use std::env;
use std::time::Duration;

use anyhow::{Context, Result};
use ict_monitor::llm::{run_provider_smoke, ProviderSmokeConfig, StructuredOutputMode};

fn optional_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn output_mode(value: &str, provider: &str) -> Result<StructuredOutputMode> {
    Ok(match value {
        "auto" if provider == "deepseek_official" => StructuredOutputMode::JsonObject,
        "auto" if provider == "volcengine_ark_plan" => StructuredOutputMode::PromptOnly,
        "auto" | "json_schema" => StructuredOutputMode::JsonSchema,
        "json_object" => StructuredOutputMode::JsonObject,
        "prompt_only" => StructuredOutputMode::PromptOnly,
        other => anyhow::bail!("unsupported ICT_LLM_SMOKE_RESPONSE_FORMAT: {other}"),
    })
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let provider =
        optional_env("ICT_LLM_SMOKE_PROVIDER").unwrap_or_else(|| "volcengine_ark_plan".into());
    let base_url = optional_env("ICT_LLM_SMOKE_BASE_URL")
        .unwrap_or_else(|| "https://ark.cn-beijing.volces.com/api/coding/v3".into());
    let model = optional_env("ICT_LLM_SMOKE_MODEL").unwrap_or_else(|| "ark-code-latest".into());
    let key_env =
        optional_env("ICT_LLM_SMOKE_API_KEY_ENV").unwrap_or_else(|| "ICT_LLM_API_KEY".into());
    let api_key = env::var(&key_env)
        .with_context(|| format!("missing API key environment variable {key_env}"))?;
    let response_format =
        optional_env("ICT_LLM_SMOKE_RESPONSE_FORMAT").unwrap_or_else(|| "auto".into());
    let timeout_secs = optional_env("ICT_LLM_SMOKE_TIMEOUT_SECS")
        .map(|value| value.parse::<u64>())
        .transpose()
        .context("ICT_LLM_SMOKE_TIMEOUT_SECS must be an integer")?
        .unwrap_or(90);
    let max_retries = optional_env("ICT_LLM_SMOKE_MAX_RETRIES")
        .map(|value| value.parse::<usize>())
        .transpose()
        .context("ICT_LLM_SMOKE_MAX_RETRIES must be an integer")?
        .unwrap_or(1);
    let max_output_tokens = optional_env("ICT_LLM_SMOKE_MAX_OUTPUT_TOKENS")
        .map(|value| value.parse::<u32>())
        .transpose()
        .context("ICT_LLM_SMOKE_MAX_OUTPUT_TOKENS must be an integer")?
        .unwrap_or(4096);

    let report = run_provider_smoke(ProviderSmokeConfig {
        provider_name: provider.clone(),
        base_url,
        api_key,
        model,
        response_format: output_mode(&response_format, &provider)?,
        timeout: Duration::from_secs(timeout_secs),
        max_retries,
        max_output_tokens,
        proxy_url: optional_env("ICT_LLM_SMOKE_PROXY_URL"),
    })
    .await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
