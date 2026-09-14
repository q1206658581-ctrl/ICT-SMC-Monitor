//! Feishu custom bot alert channel.
//!
//! MVP scope: one global Feishu group custom bot webhook, text messages only,
//! no signing, no delivery outbox.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use url::Url;

use super::types::{AlertRecord, AlertTrigger, ChannelKind};
use super::AlertChannel;
use crate::config::FeishuAlertConfig;

const FEISHU_QUEUE_CAPACITY: usize = 256;
const FEISHU_HOOK_PATH_PREFIX: &str = "/open-apis/bot/v2/hook/";

#[derive(Clone)]
pub struct FeishuAlertSender {
    inner: Arc<FeishuAlertSenderInner>,
}

struct FeishuAlertSenderInner {
    webhook_url: Option<String>,
    client: reqwest::Client,
    tx: mpsc::Sender<FeishuSendTask>,
    rx: parking_lot::Mutex<Option<mpsc::Receiver<FeishuSendTask>>>,
}

#[derive(Debug)]
struct FeishuSendTask {
    alert_id: String,
    watchlist_id: String,
    trade_symbol: String,
    text: String,
}

impl FeishuAlertSender {
    pub fn new(config: FeishuAlertConfig) -> Self {
        let timeout = Duration::from_millis(config.timeout_ms.max(1));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|error| {
                tracing::warn!(
                    ?error,
                    "build Feishu HTTP client failed; using default client"
                );
                reqwest::Client::new()
            });
        let (tx, rx) = mpsc::channel(FEISHU_QUEUE_CAPACITY);
        Self {
            inner: Arc::new(FeishuAlertSenderInner {
                webhook_url: config.webhook_url,
                client,
                tx,
                rx: parking_lot::Mutex::new(Some(rx)),
            }),
        }
    }

    pub fn start_worker(&self) {
        let Some(mut rx) = self.inner.rx.lock().take() else {
            tracing::debug!("Feishu alert worker already started");
            return;
        };
        let inner = self.inner.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(task) = rx.recv().await {
                let Some(webhook_url) = inner.configured_webhook_url() else {
                    tracing::warn!(
                        alert_id = %task.alert_id,
                        watchlist_id = %task.watchlist_id,
                        trade_symbol = %task.trade_symbol,
                        "Feishu webhook URL is not configured; dropping alert"
                    );
                    continue;
                };
                if let Err(error) = post_text_payload(&inner.client, webhook_url, &task.text).await
                {
                    tracing::warn!(
                        alert_id = %task.alert_id,
                        watchlist_id = %task.watchlist_id,
                        trade_symbol = %task.trade_symbol,
                        error = %error,
                        "Feishu alert send failed"
                    );
                } else {
                    tracing::info!(
                        alert_id = %task.alert_id,
                        watchlist_id = %task.watchlist_id,
                        trade_symbol = %task.trade_symbol,
                        "Feishu alert sent"
                    );
                }
            }
        });
    }

    pub fn enqueue_alert(&self, alert: &AlertRecord) -> Result<(), String> {
        self.validate_configured_webhook()?;
        let task = FeishuSendTask {
            alert_id: alert.id.clone(),
            watchlist_id: alert.watchlist_id.clone(),
            trade_symbol: display_trade_symbol(alert).to_owned(),
            text: format_feishu_alert_text(alert),
        };
        match self.inner.tx.try_send(task) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(task)) => {
                tracing::warn!(
                    alert_id = %task.alert_id,
                    watchlist_id = %task.watchlist_id,
                    trade_symbol = %task.trade_symbol,
                    "Feishu alert queue full; dropping alert"
                );
                Err("飞书通知队列已满，本次推送已丢弃".to_owned())
            }
            Err(mpsc::error::TrySendError::Closed(task)) => {
                tracing::warn!(
                    alert_id = %task.alert_id,
                    watchlist_id = %task.watchlist_id,
                    trade_symbol = %task.trade_symbol,
                    "Feishu alert queue closed; dropping alert"
                );
                Err("飞书通知队列已关闭，本次推送已丢弃".to_owned())
            }
        }
    }

    pub async fn send_test_message(&self) -> Result<(), String> {
        let webhook_url = self
            .validate_configured_webhook()
            .and_then(|_| {
                self.inner
                    .configured_webhook_url()
                    .ok_or_else(|| "飞书 webhook URL 未配置".to_owned())
            })?
            .to_owned();
        let text = format!(
            "[ICT Radar] Test alert\nFeishu webhook connected\nTime: {}",
            format_timestamp_ms(current_time_ms())
        );
        post_text_payload(&self.inner.client, &webhook_url, &text).await
    }

    fn validate_configured_webhook(&self) -> Result<(), String> {
        let Some(webhook_url) = self.inner.configured_webhook_url() else {
            return Err("飞书 webhook URL 未配置，请先在 config.toml 中填写".to_owned());
        };
        validate_feishu_webhook_url(webhook_url)
    }
}

impl FeishuAlertSenderInner {
    fn configured_webhook_url(&self) -> Option<&str> {
        self.webhook_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
    }
}

pub struct FeishuNotifyChannel {
    sender: FeishuAlertSender,
}

impl FeishuNotifyChannel {
    pub fn new(sender: FeishuAlertSender) -> Self {
        Self { sender }
    }
}

impl AlertChannel for FeishuNotifyChannel {
    fn kind(&self) -> ChannelKind {
        ChannelKind::FeishuNotify
    }

    fn deliver(&self, alert: &AlertRecord) -> Result<(), String> {
        if alert.trigger != AlertTrigger::C2Confirmed {
            return Ok(());
        }
        self.sender.enqueue_alert(alert)
    }
}

#[derive(Serialize)]
struct FeishuTextPayload<'a> {
    msg_type: &'static str,
    content: FeishuTextContent<'a>,
}

#[derive(Serialize)]
struct FeishuTextContent<'a> {
    text: &'a str,
}

#[derive(Deserialize)]
struct FeishuWebhookResponse {
    code: i64,
    msg: Option<String>,
}

pub fn format_feishu_alert_text(alert: &AlertRecord) -> String {
    format!(
        "[ICT Radar] C2 confirmed\nWatchlist: {}\nSymbol: {}\nDirection: {:?}\nCase: {} · Score: {:.2}\nTF: {}/{}/{}\nC2 close: {}",
        alert.watchlist_id,
        display_trade_symbol(alert),
        alert.candidate_direction,
        alert.c2_case,
        alert.deterministic_score,
        alert.context_timeframe.tag(),
        alert.comparison_timeframe.tag(),
        alert.validation_timeframe.tag(),
        format_timestamp_ms(alert.created_at),
    )
}

pub fn redact_webhook_url(webhook_url: &str) -> String {
    match Url::parse(webhook_url) {
        Ok(url) => {
            let scheme = url.scheme();
            let host = url.host_str().unwrap_or("unknown-host");
            if url.path().starts_with(FEISHU_HOOK_PATH_PREFIX) {
                format!("{scheme}://{host}{FEISHU_HOOK_PATH_PREFIX}<redacted>")
            } else {
                format!("{scheme}://{host}/<redacted>")
            }
        }
        Err(_) => "<redacted webhook url>".to_owned(),
    }
}

pub fn validate_feishu_webhook_url(webhook_url: &str) -> Result<(), String> {
    let trimmed = webhook_url.trim();
    if trimmed.is_empty() {
        return Err("飞书 webhook URL 未配置".to_owned());
    }
    let parsed = Url::parse(trimmed).map_err(|_| "飞书 webhook URL 格式无效".to_owned())?;
    if parsed.scheme() != "https" {
        return Err("飞书 webhook URL 必须使用 https".to_owned());
    }
    if parsed.host_str() != Some("open.feishu.cn") {
        return Err("飞书 webhook URL host 必须是 open.feishu.cn".to_owned());
    }
    let path = parsed.path();
    if !path.starts_with(FEISHU_HOOK_PATH_PREFIX) || path.len() <= FEISHU_HOOK_PATH_PREFIX.len() {
        return Err(format!(
            "飞书 webhook URL path 必须以 {FEISHU_HOOK_PATH_PREFIX} 开头"
        ));
    }
    Ok(())
}

async fn post_text_payload(
    client: &reqwest::Client,
    webhook_url: &str,
    text: &str,
) -> Result<(), String> {
    let payload = FeishuTextPayload {
        msg_type: "text",
        content: FeishuTextContent { text },
    };
    let body = serde_json::to_vec(&payload).map_err(|_| "飞书消息序列化失败".to_owned())?;
    let response = client
        .post(webhook_url)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|error| reqwest_error_message(&error))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| reqwest_error_message(&error))?;
    if !status.is_success() {
        return Err(format!("飞书 webhook HTTP 失败: status={status}"));
    }
    let parsed: FeishuWebhookResponse =
        serde_json::from_slice(&body).map_err(|_| "飞书 webhook 响应不是合法 JSON".to_owned())?;
    if parsed.code == 0 {
        return Ok(());
    }
    Err(format!(
        "飞书 webhook 业务失败: code={}, msg={}",
        parsed.code,
        parsed.msg.unwrap_or_else(|| "<empty>".to_owned())
    ))
}

fn reqwest_error_message(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "飞书 webhook 请求超时".to_owned()
    } else if error.is_connect() {
        "飞书 webhook 连接失败".to_owned()
    } else if error.is_request() {
        "飞书 webhook 请求构造失败".to_owned()
    } else if error.is_body() || error.is_decode() {
        "飞书 webhook 响应读取失败".to_owned()
    } else {
        "飞书 webhook 请求失败".to_owned()
    }
}

fn display_trade_symbol(alert: &AlertRecord) -> &str {
    alert
        .trade_symbols
        .first()
        .or(alert.validation_symbol.as_ref())
        .map(|symbol| symbol.split(':').last().unwrap_or(symbol))
        .unwrap_or("unknown")
}

fn format_timestamp_ms(ts: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| ts.to_string())
}

fn current_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::types::AlertRecord;
    use crate::candidate::SetupStatus;
    use crate::detector::types::Direction;
    use crate::types::Timeframe;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn sample_alert(trigger: AlertTrigger) -> AlertRecord {
        AlertRecord {
            id: "alert-1".to_owned(),
            watchlist_id: "eu-gu-dxy".to_owned(),
            candidate_id: "candidate-1".to_owned(),
            smt_id: "smt-1".to_owned(),
            rule_version: "alert_v1".to_owned(),
            trigger,
            setup_status: SetupStatus::C2Confirmed,
            invalidation_reason: None,
            symbol_set: vec!["TVC:DXY".to_owned(), "OANDA:EURUSD".to_owned()],
            sweeper_symbol: "TVC:DXY".to_owned(),
            trade_symbols: vec!["OANDA:EURUSD".to_owned()],
            candidate_direction: Direction::Bullish,
            deterministic_score: 0.83,
            c2_case: 2,
            context_timeframe: Timeframe::H4,
            comparison_timeframe: Timeframe::H1,
            validation_timeframe: Timeframe::M5,
            c2_candle_ts: 1_700_000_000_000,
            c3_candle_ts: None,
            smt_k_candle_ts: Some(1_699_999_000_000),
            context_pda_id: Some("pda-1".to_owned()),
            validation_kind: None,
            validation_symbol: Some("OANDA:EURUSD".to_owned()),
            validation_ts: None,
            validation_direction: Some(Direction::Bullish),
            channels_fired: vec![ChannelKind::Inbox, ChannelKind::FeishuNotify],
            created_at: 1_700_000_000_000,
        }
    }

    #[test]
    fn format_feishu_alert_text_contains_required_fields() {
        let text = format_feishu_alert_text(&sample_alert(AlertTrigger::C2Confirmed));
        assert!(text.contains("[ICT Radar] C2 confirmed"));
        assert!(text.contains("Watchlist: eu-gu-dxy"));
        assert!(text.contains("Symbol: EURUSD"));
        assert!(text.contains("Direction: Bullish"));
        assert!(text.contains("Case: 2"));
        assert!(text.contains("Score: 0.83"));
        assert!(text.contains("TF: 4h/1h/5m"));
        assert!(text.contains("C2 close:"));
        assert!(!text.contains("webhook"));
        assert!(!text.contains("entry"));
        assert!(!text.contains("sl"));
        assert!(!text.contains("tp"));
    }

    #[test]
    fn feishu_channel_ignores_validated_alerts() {
        let sender = FeishuAlertSender::new(FeishuAlertConfig::default());
        let channel = FeishuNotifyChannel::new(sender);
        assert!(channel
            .deliver(&sample_alert(AlertTrigger::Validated))
            .is_ok());
    }

    #[test]
    fn validates_feishu_webhook_url_shape() {
        assert!(
            validate_feishu_webhook_url("https://open.feishu.cn/open-apis/bot/v2/hook/token")
                .is_ok()
        );
        assert!(
            validate_feishu_webhook_url("http://open.feishu.cn/open-apis/bot/v2/hook/token")
                .is_err()
        );
        assert!(
            validate_feishu_webhook_url("https://example.com/open-apis/bot/v2/hook/token").is_err()
        );
        assert!(
            validate_feishu_webhook_url("https://open.feishu.cn/open-apis/bot/v1/hook/token")
                .is_err()
        );
    }

    #[test]
    fn redacts_webhook_url_token() {
        let raw = "https://open.feishu.cn/open-apis/bot/v2/hook/secret-token";
        let redacted = redact_webhook_url(raw);
        assert_ne!(redacted, raw);
        assert!(!redacted.contains("secret-token"));
        assert!(redacted.contains("<redacted>"));
    }

    #[tokio::test]
    async fn post_text_payload_accepts_code_zero() {
        let url = serve_once(200, r#"{"code":0,"msg":"success"}"#, None).await;
        let client = reqwest::Client::new();
        post_text_payload(&client, &url, "hello").await.unwrap();
    }

    #[tokio::test]
    async fn post_text_payload_rejects_business_error() {
        let url = serve_once(200, r#"{"code":19024,"msg":"Key Words Not Found"}"#, None).await;
        let client = reqwest::Client::new();
        let error = post_text_payload(&client, &url, "hello").await.unwrap_err();
        assert!(error.contains("code=19024"));
        assert!(!error.contains(&url));
    }

    #[tokio::test]
    async fn post_text_payload_rejects_non_json() {
        let url = serve_once(200, "not-json", None).await;
        let client = reqwest::Client::new();
        let error = post_text_payload(&client, &url, "hello").await.unwrap_err();
        assert!(error.contains("不是合法 JSON"));
        assert!(!error.contains(&url));
    }

    #[tokio::test]
    async fn post_text_payload_rejects_http_error() {
        let url = serve_once(500, r#"{"code":999,"msg":"boom"}"#, None).await;
        let client = reqwest::Client::new();
        let error = post_text_payload(&client, &url, "hello").await.unwrap_err();
        assert!(error.contains("status=500"));
        assert!(!error.contains(&url));
    }

    #[tokio::test]
    async fn post_text_payload_reports_timeout_without_url() {
        let url = serve_once(
            200,
            r#"{"code":0,"msg":"success"}"#,
            Some(Duration::from_millis(100)),
        )
        .await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(10))
            .build()
            .unwrap();
        let error = post_text_payload(&client, &url, "hello").await.unwrap_err();
        assert!(error.contains("超时"));
        assert!(!error.contains(&url));
    }

    async fn serve_once(status: u16, body: &'static str, delay: Option<Duration>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 4096];
            let _ = stream.read(&mut buf).await;
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        format!("http://{addr}/webhook")
    }
}
