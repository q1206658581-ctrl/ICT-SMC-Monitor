//! User-facing config loader.
//!
//! Reads `~/.ict-monitor/config.toml` (path overridable via `ICT_CONFIG_PATH`).
//! M5 adds the `[[watchlists]]` section (see `watchlist` module).

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::watchlist::{self, Watchlist};

/// Stable service name used for LLM API keys in the macOS login Keychain.
/// The account name is the configured `api_key_env`, so multiple providers
/// can use separate secrets without ever placing them in config.toml.
pub const LLM_KEYCHAIN_SERVICE: &str = "com.ict-monitor.app.llm-api-key";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LlmApiKeySource {
    Environment,
    MacOsKeychain,
}

impl LlmApiKeySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::MacOsKeychain => "macos_keychain",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AppConfig {
    pub tradingview: TradingViewConfig,
    pub llm: LlmConfig,
    pub alerts: AlertsConfig,
    #[serde(default)]
    pub watchlists: Vec<Watchlist>,
    /// Symbols saved for chart viewing; no strategy group is inferred.
    #[serde(default)]
    pub chart_symbols: Vec<String>,
    /// id of the watchlist the user last selected; cold-start seeds this
    /// one so a restart doesn't waste minutes seeding the first config
    /// watchlist and then re-seeding the real one on switch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_watchlist_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct LlmConfig {
    pub enabled: bool,
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    /// `auto`, `json_schema`, `json_object`, or `prompt_only`. `auto` selects
    /// the safest format known for the configured provider.
    pub response_format: String,
    pub temperature: f64,
    pub timeout_secs: u64,
    pub max_retries: usize,
    pub max_output_tokens: u32,
    /// Optional provider-supported reasoning budget; empty omits the parameter.
    pub reasoning_effort: String,
    pub proxy_url: String,
    pub bars_per_tf: [usize; 3],
    pub max_calls_per_day: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            // An absent [llm] section is deliberately safe and free.
            enabled: false,
            provider: "openai_compatible".into(),
            model: "gpt-5.2".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key_env: "ICT_LLM_API_KEY".into(),
            response_format: "auto".into(),
            temperature: 0.0,
            timeout_secs: 60,
            max_retries: 1,
            max_output_tokens: 4096,
            reasoning_effort: String::new(),
            proxy_url: String::new(),
            bars_per_tf: [100, 100, 120],
            max_calls_per_day: 500,
        }
    }
}

impl LlmConfig {
    /// Validate non-secret configuration. Invalid configuration degrades the
    /// LLM sidecar only; callers must keep the market/alert pipeline alive.
    pub fn validate(&self) -> Result<(), String> {
        if !matches!(
            self.provider.as_str(),
            "openai_compatible" | "deepseek_official" | "volcengine_ark_plan"
        ) {
            return Err(format!("unsupported provider: {}", self.provider));
        }
        if self.model.trim().is_empty() {
            return Err("model must not be empty".into());
        }
        let base = self.base_url.trim();
        if !(base.starts_with("https://") || base.starts_with("http://")) {
            return Err("base_url must be an http(s) URL".into());
        }
        if self.api_key_env.trim().is_empty() {
            return Err("api_key_env must not be empty".into());
        }
        if !matches!(
            self.response_format.as_str(),
            "auto" | "json_schema" | "json_object" | "prompt_only"
        ) {
            return Err(format!(
                "unsupported response_format: {}",
                self.response_format
            ));
        }
        if !self.temperature.is_finite() || self.temperature < 0.0 {
            return Err("temperature must be finite and non-negative".into());
        }
        if self.timeout_secs == 0 {
            return Err("timeout_secs must be greater than zero".into());
        }
        if !matches!(self.reasoning_effort.as_str(), "" | "low" | "high" | "max") {
            return Err("reasoning_effort must be empty, low, high, or max".into());
        }
        if self.max_output_tokens == 0 {
            return Err("max_output_tokens must be greater than zero".into());
        }
        if self.bars_per_tf.iter().any(|count| *count == 0) {
            return Err("bars_per_tf entries must be greater than zero".into());
        }
        if self.max_calls_per_day == 0 {
            return Err("max_calls_per_day must be greater than zero".into());
        }
        Ok(())
    }

    pub fn resolved_response_format(&self) -> &'static str {
        match self.response_format.as_str() {
            "json_schema" => "json_schema",
            "json_object" => "json_object",
            "prompt_only" => "prompt_only",
            // DeepSeek Chat Completions officially exposes JSON Object mode.
            "auto" if self.provider == "deepseek_official" => "json_object",
            // Coding Plan routes can reject or return empty/truncated content
            // with wire-level response_format. The schema remains in the
            // system prompt and is still enforced by the local validator.
            "auto" if self.provider == "volcengine_ark_plan" => "prompt_only",
            _ => "json_schema",
        }
    }

    pub fn api_key_from_env(&self) -> Option<String> {
        self.api_key_from_lookup(|name| std::env::var(name).ok())
    }

    /// Resolve an environment-backed secret through an injectable lookup.
    /// Keeping the parsing policy here lets tests cover whitespace and missing
    /// values without mutating the process environment or touching Keychain.
    fn api_key_from_lookup<F>(&self, lookup: F) -> Option<String>
    where
        F: FnOnce(&str) -> Option<String>,
    {
        let name = self.api_key_env.trim();
        if name.is_empty() {
            return None;
        }
        lookup(name).and_then(|value| {
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_owned())
        })
    }

    /// Resolve an API key without exposing it to config, logs, SQLite, or git.
    ///
    /// An explicitly supplied environment variable wins. On macOS it is also
    /// migrated into the login Keychain, so future GUI cold starts do not
    /// depend on launchd inheriting a transient shell environment.
    pub fn resolve_api_key(&self) -> Result<Option<(String, LlmApiKeySource)>, String> {
        if let Some(api_key) = self.api_key_from_env() {
            #[cfg(all(target_os = "macos", not(test)))]
            if let Err(error) = self.persist_api_key_to_keychain(&api_key) {
                // Persistence failure must not disable a valid in-memory key.
                tracing::warn!(error = %error, "could not persist LLM API key in macOS Keychain");
            }
            return Ok(Some((api_key, LlmApiKeySource::Environment)));
        }

        #[cfg(all(target_os = "macos", not(test)))]
        {
            return self
                .api_key_from_keychain()
                .map(|key| key.map(|value| (value, LlmApiKeySource::MacOsKeychain)));
        }

        // Unit tests must remain hermetic and must never trigger an operating
        // system Keychain prompt. The real macOS application uses the branch
        // above; tests exercise the environment/configuration behavior only.
        #[cfg(any(not(target_os = "macos"), test))]
        Ok(None)
    }

    #[cfg(all(target_os = "macos", not(test)))]
    fn persist_api_key_to_keychain(&self, api_key: &str) -> Result<(), String> {
        use security_framework::passwords::{get_generic_password, set_generic_password};

        let account = self.api_key_env.trim();
        let bytes = api_key.as_bytes();
        if get_generic_password(LLM_KEYCHAIN_SERVICE, account)
            .ok()
            .as_deref()
            == Some(bytes)
        {
            return Ok(());
        }
        set_generic_password(LLM_KEYCHAIN_SERVICE, account, bytes)
            .map_err(|error| format!("Keychain write failed: {error}"))
    }

    #[cfg(all(target_os = "macos", not(test)))]
    fn api_key_from_keychain(&self) -> Result<Option<String>, String> {
        use security_framework::passwords::get_generic_password;

        const ERR_SEC_ITEM_NOT_FOUND: i32 = -25_300;
        let account = self.api_key_env.trim();
        let bytes = match get_generic_password(LLM_KEYCHAIN_SERVICE, account) {
            Ok(bytes) => bytes,
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => return Ok(None),
            Err(error) => return Err(format!("Keychain read failed: {error}")),
        };
        let api_key =
            String::from_utf8(bytes).map_err(|_| "Keychain item is not valid UTF-8".to_string())?;
        if api_key.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(api_key))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TradingViewConfig {
    /// `sessionid` cookie from a logged-in tradingview.com session.
    pub sessionid: Option<String>,
    /// Companion cookie required by TV.
    pub sessionid_sign: Option<String>,
    /// Optional app-local HTTP CONNECT proxy. This is intentionally separate
    /// from macOS global proxy settings so a local proxy can be used by ICT
    /// Radar without changing networking for the rest of the machine.
    pub proxy_url: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AlertsConfig {
    pub feishu: FeishuAlertConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct FeishuAlertConfig {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    pub timeout_ms: u64,
}

impl Default for FeishuAlertConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            webhook_url: None,
            timeout_ms: 3_000,
        }
    }
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            tracing::info!(path = %path.display(), "config not found; using defaults");
            let mut cfg = Self::default();
            cfg.watchlists = watchlist::default_watchlists();
            return Ok(cfg);
        }
        let body =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let cfg: AppConfig =
            toml::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
        let cfg = Self::normalize(cfg);
        tracing::info!(path = %path.display(), n_watchlists = cfg.watchlists.len(), "config loaded");
        Ok(cfg)
    }

    /// Validate every watchlist; drop invalid ones (with a warning) and fall
    /// back to built-in defaults if none survive. Does not mutate disk.
    fn normalize(mut cfg: AppConfig) -> AppConfig {
        let mut kept = Vec::new();
        for w in cfg.watchlists.drain(..) {
            match w.validate() {
                Ok(()) => kept.push(w),
                Err(e) => tracing::warn!(id = %w.id, error = %e, "skipping invalid watchlist"),
            }
        }
        if kept.is_empty() {
            tracing::warn!("no valid watchlists in config; using built-in defaults");
            kept = watchlist::default_watchlists();
        }
        cfg.watchlists = kept;
        // Merge in any built-in watchlists that are missing from the
        // config file (e.g. a new default added in a code update).
        cfg.watchlists = watchlist::merge_defaults(&cfg.watchlists);
        cfg.alerts.feishu.webhook_url = cfg.alerts.feishu.webhook_url.and_then(|url| {
            let trimmed = url.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        });
        if cfg.alerts.feishu.timeout_ms == 0 {
            cfg.alerts.feishu.timeout_ms = FeishuAlertConfig::default().timeout_ms;
        }
        cfg
    }

    /// Persist the full config (tradingview + watchlists) back to disk.
    pub fn save(&self) -> Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let out = toml::to_string_pretty(self).context("serialize config")?;
        std::fs::write(&path, out).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod alert_config_tests {
    use super::*;

    #[test]
    fn feishu_alert_config_defaults_to_disabled() {
        let cfg = AppConfig::default();
        assert!(!cfg.alerts.feishu.enabled);
        assert!(cfg.alerts.feishu.webhook_url.is_none());
        assert_eq!(cfg.alerts.feishu.timeout_ms, 3_000);
    }

    #[test]
    fn feishu_empty_webhook_is_normalized_to_none() {
        let cfg: AppConfig = toml::from_str(
            r#"
            [alerts.feishu]
            enabled = true
            webhook_url = ""
            timeout_ms = 0
            "#,
        )
        .expect("parse config");
        let cfg = AppConfig::normalize(cfg);
        assert!(cfg.alerts.feishu.enabled);
        assert!(cfg.alerts.feishu.webhook_url.is_none());
        assert_eq!(cfg.alerts.feishu.timeout_ms, 3_000);
    }
}

fn config_path() -> PathBuf {
    if let Ok(p) = env::var("ICT_CONFIG_PATH") {
        return PathBuf::from(p);
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".ict-monitor").join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_effort_accepts_only_the_supported_values() {
        let mut config = LlmConfig::default();
        for valid in ["", "low", "high", "max"] {
            config.reasoning_effort = valid.into();
            assert!(config.validate().is_ok(), "{valid}");
        }
        for invalid in ["medium", "LOW", " low", "high ", "invalid", "0"] {
            config.reasoning_effort = invalid.into();
            assert_eq!(config.validate().unwrap_err(), "reasoning_effort must be empty, low, high, or max");
        }
    }

    #[test]
    fn llm_api_key_lookup_trims_name_and_value() {
        let config = LlmConfig {
            api_key_env: "  TEST_LLM_KEY  ".into(),
            ..LlmConfig::default()
        };

        let value = config.api_key_from_lookup(|name| {
            assert_eq!(name, "TEST_LLM_KEY");
            Some("  secret-value  ".into())
        });

        assert_eq!(value.as_deref(), Some("secret-value"));
    }

    #[test]
    fn llm_api_key_lookup_rejects_blank_name_and_value() {
        let blank_name = LlmConfig {
            api_key_env: "   ".into(),
            ..LlmConfig::default()
        };
        assert_eq!(
            blank_name.api_key_from_lookup(|_| panic!("lookup called")),
            None
        );

        let config = LlmConfig::default();
        assert_eq!(config.api_key_from_lookup(|_| Some(" \n\t ".into())), None);
        assert_eq!(config.api_key_from_lookup(|_| None), None);
    }

    #[test]
    fn absent_llm_section_uses_safe_valid_defaults() {
        let config: AppConfig = toml::from_str("").expect("empty config uses serde defaults");

        assert!(!config.llm.enabled);
        assert_eq!(config.llm.provider, "openai_compatible");
        assert_eq!(config.llm.api_key_env, "ICT_LLM_API_KEY");
        assert_eq!(config.llm.response_format, "auto");
        assert_eq!(config.llm.bars_per_tf, [100, 100, 120]);
        config.llm.validate().expect("default LLM config is valid");
    }
}
