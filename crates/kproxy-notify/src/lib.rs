//! Non-blocking multi-target webhook notifications.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use hmac::{Hmac, Mac};
use kproxy_core::config::{NotifyConfig, WebhookConfig};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::sync::mpsc;
use tracing::{debug, warn};

type HmacSha256 = Hmac<Sha256>;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WebhookEventKind {
    AccountQuotaExhausted,
    ServiceQuotaExhausted,
    TokenRefreshFailed,
    Test,
}

impl WebhookEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AccountQuotaExhausted => "account-quota-exhausted",
            Self::ServiceQuotaExhausted => "service-quota-exhausted",
            Self::TokenRefreshFailed => "token-refresh-failed",
            Self::Test => "test",
        }
    }

    fn matches_subscription(self, configured: &str) -> bool {
        configured == self.as_str()
            || match self {
                // Keep existing webhook configurations working after the
                // alert policy is narrowed to the three incident types.
                Self::AccountQuotaExhausted => configured == "quota-exhausted",
                Self::ServiceQuotaExhausted => {
                    matches!(configured, "quota-exhausted" | "service-degraded")
                }
                Self::TokenRefreshFailed => {
                    matches!(configured, "token-expired" | "account-banned")
                }
                Self::Test => false,
            }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookEvent {
    pub kind: WebhookEventKind,
    pub title: String,
    pub message: String,
    #[serde(default)]
    pub account_id: Option<String>,
    pub timestamp: i64,
}

impl WebhookEvent {
    pub fn new(
        kind: WebhookEventKind,
        title: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            title: title.into(),
            message: message.into(),
            account_id: None,
            timestamp: now_secs(),
        }
    }
}

#[derive(Clone)]
pub struct Notifier {
    senders: Arc<HashMap<String, mpsc::Sender<Delivery>>>,
    targets: Arc<Vec<WebhookConfig>>,
    active_incidents: Arc<Mutex<HashMap<String, i64>>>,
    history: Arc<Mutex<VecDeque<DeliveryLog>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeliveryLog {
    pub timestamp: i64,
    pub target: String,
    pub event: WebhookEventKind,
    pub success: bool,
    pub attempts: u8,
    pub error: Option<String>,
}

#[derive(Debug)]
struct Delivery {
    target: WebhookConfig,
    event: WebhookEvent,
}

enum SuppressionMark {
    None,
    Incident { key: String, timestamp: i64 },
}

impl Notifier {
    pub fn new(targets: Vec<WebhookConfig>, config: NotifyConfig, queue_size: usize) -> Self {
        Self::with_state(
            targets,
            config,
            queue_size,
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(VecDeque::new())),
        )
    }

    fn with_state(
        targets: Vec<WebhookConfig>,
        _config: NotifyConfig,
        queue_size: usize,
        active_incidents: Arc<Mutex<HashMap<String, i64>>>,
        history: Arc<Mutex<VecDeque<DeliveryLog>>>,
    ) -> Self {
        let senders = targets
            .iter()
            .map(|target| {
                let (sender, receiver) = mpsc::channel(queue_size.max(1));
                tokio::spawn(worker(receiver, Arc::clone(&history)));
                (target.name.clone(), sender)
            })
            .collect();
        Self {
            senders: Arc::new(senders),
            targets: Arc::new(targets),
            active_incidents,
            history,
        }
    }

    /// Replace delivery targets while preserving active incident state and
    /// delivery history across config hot reloads.
    pub fn reconfigured(
        &self,
        targets: Vec<WebhookConfig>,
        config: NotifyConfig,
        queue_size: usize,
    ) -> Self {
        Self::with_state(
            targets,
            config,
            queue_size,
            Arc::clone(&self.active_incidents),
            Arc::clone(&self.history),
        )
    }

    pub fn emit(&self, event: WebhookEvent) -> usize {
        self.emit_matching(None, event)
    }

    pub fn emit_to(&self, name: &str, event: WebhookEvent) -> usize {
        self.emit_matching(Some(name), event)
    }

    /// Send an operator-requested test without requiring an event subscription
    /// or changing incident suppression state.
    pub fn emit_test(&self, name: Option<&str>, event: WebhookEvent) -> usize {
        self.emit_matching_inner(name, event, true)
    }

    fn emit_matching(&self, name: Option<&str>, event: WebhookEvent) -> usize {
        self.emit_matching_inner(name, event, false)
    }

    fn emit_matching_inner(&self, name: Option<&str>, event: WebhookEvent, is_test: bool) -> usize {
        let mut queued = 0;
        for target in self.targets.iter() {
            if name.is_some_and(|name| target.name != name)
                || !target.enabled
                || (!is_test
                    && !target
                        .events
                        .iter()
                        .any(|kind| event.kind.matches_subscription(kind)))
            {
                continue;
            }
            let mark = if is_test {
                SuppressionMark::None
            } else {
                let Some(mark) = self.mark_send(target, &event) else {
                    continue;
                };
                mark
            };
            let Some(sender) = self.senders.get(&target.name) else {
                self.rollback_mark(mark);
                warn!(webhook = %target.name, "webhook worker missing; dropping event");
                continue;
            };
            if sender
                .try_send(Delivery {
                    target: target.clone(),
                    event: event.clone(),
                })
                .is_ok()
            {
                queued += 1;
            } else {
                self.rollback_mark(mark);
                warn!(webhook = %target.name, "webhook queue full; dropping event");
            }
        }
        queued
    }

    fn mark_send(&self, target: &WebhookConfig, event: &WebhookEvent) -> Option<SuppressionMark> {
        let account = event.account_id.as_deref().unwrap_or("global");
        let key = format!("{}:{}:{account}", target.name, event.kind.as_str());
        let now = now_millis();
        let mut incidents = lock(&self.active_incidents);
        if incidents.contains_key(&key) {
            return None;
        }
        incidents.insert(key.clone(), now);
        Some(SuppressionMark::Incident {
            key,
            timestamp: now,
        })
    }

    fn rollback_mark(&self, mark: SuppressionMark) {
        match mark {
            SuppressionMark::None => {}
            SuppressionMark::Incident { key, timestamp } => {
                let mut incidents = lock(&self.active_incidents);
                if incidents.get(&key) == Some(&timestamp) {
                    incidents.remove(&key);
                }
            }
        }
    }

    /// Mark an incident as recovered so a future recurrence can alert once.
    pub fn resolve_incident(&self, kind: WebhookEventKind, account_id: Option<&str>) {
        let account = account_id.unwrap_or("global");
        let suffix = format!(":{}:{account}", kind.as_str());
        lock(&self.active_incidents).retain(|key, _| !key.ends_with(&suffix));
    }

    pub fn logs(&self, tail: usize) -> Vec<DeliveryLog> {
        lock(&self.history)
            .iter()
            .rev()
            .take(tail.min(1_000))
            .cloned()
            .collect()
    }
}

async fn worker(
    mut receiver: mpsc::Receiver<Delivery>,
    history: Arc<Mutex<VecDeque<DeliveryLog>>>,
) {
    let client = match Client::builder().timeout(Duration::from_secs(8)).build() {
        Ok(client) => client,
        Err(error) => {
            warn!(%error, "failed to initialize webhook client");
            return;
        }
    };
    while let Some(delivery) = receiver.recv().await {
        deliver(client.clone(), Arc::clone(&history), delivery).await;
    }
}

async fn deliver(client: Client, history: Arc<Mutex<VecDeque<DeliveryLog>>>, delivery: Delivery) {
    for attempt in 0..3 {
        match send(&client, &delivery.target, &delivery.event).await {
            Ok(()) => {
                debug!(webhook = %delivery.target.name, event = delivery.event.kind.as_str(), "webhook delivered");
                record_delivery(&history, &delivery, true, attempt + 1, None);
                break;
            }
            Err(error) if attempt < 2 => {
                warn!(webhook = %delivery.target.name, %error, attempt, "webhook retrying");
                tokio::time::sleep(Duration::from_millis(1_500 * (1 << attempt))).await;
            }
            Err(error) => {
                warn!(webhook = %delivery.target.name, %error, "webhook delivery failed");
                record_delivery(&history, &delivery, false, attempt + 1, Some(error));
            }
        }
    }
}

fn record_delivery(
    history: &Mutex<VecDeque<DeliveryLog>>,
    delivery: &Delivery,
    success: bool,
    attempts: u8,
    error: Option<String>,
) {
    let mut history = lock(history);
    history.push_back(DeliveryLog {
        timestamp: now_secs(),
        target: delivery.target.name.clone(),
        event: delivery.event.kind,
        success,
        attempts,
        error,
    });
    while history.len() > 1_000 {
        history.pop_front();
    }
}

async fn send(client: &Client, target: &WebhookConfig, event: &WebhookEvent) -> Result<(), String> {
    let (url, payload) = payload(target, event)?;
    let response = client
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {}", response.status()))
    }
}

fn payload(target: &WebhookConfig, event: &WebhookEvent) -> Result<(String, Value), String> {
    let markdown = format!("### {}\n\n{}", event.title, event.message);
    match target.kind.as_str() {
        "dingtalk" => {
            let url = if let Some(secret) = &target.dingtalk_sign {
                signed_dingtalk_url(&target.url, secret)?
            } else {
                target.url.clone()
            };
            Ok((
                url,
                json!({"msgtype":"markdown","markdown":{"title":event.title,"text":markdown}}),
            ))
        }
        "wechat-work" | "wechat" => Ok((
            target.url.clone(),
            json!({"msgtype":"markdown","markdown":{"content":markdown}}),
        )),
        "feishu" => Ok((
            target.url.clone(),
            json!({
                "msg_type":"interactive",
                "card":{
                    "header":{"title":{"tag":"plain_text","content":event.title}},
                    "elements":[{"tag":"markdown","content":event.message}]
                }
            }),
        )),
        "telegram" => Ok((
            target.url.clone(),
            json!({"chat_id":target.telegram_chat_id,"text":markdown,"parse_mode":"Markdown"}),
        )),
        "discord" => Ok((target.url.clone(), json!({"content":markdown}))),
        "custom" => {
            let value = if let Some(template) = target.custom_template.as_deref() {
                let json_body = template
                    .replace("{{event}}", &json_string_fragment(event.kind.as_str()))
                    .replace("{{title}}", &json_string_fragment(&event.title))
                    .replace("{{message}}", &json_string_fragment(&event.message));
                serde_json::from_str(&json_body).unwrap_or_else(|_| {
                    let text = template
                        .replace("{{event}}", event.kind.as_str())
                        .replace("{{title}}", &event.title)
                        .replace("{{message}}", &event.message);
                    json!({"text":text})
                })
            } else {
                json!({
                    "event":event.kind.as_str(),
                    "title":event.title,
                    "message":event.message,
                })
            };
            Ok((target.url.clone(), value))
        }
        kind => Err(format!("unknown webhook kind {kind}")),
    }
}

fn json_string_fragment(value: &str) -> String {
    serde_json::to_string(value)
        .map(|encoded| encoded[1..encoded.len().saturating_sub(1)].to_owned())
        .unwrap_or_default()
}

fn signed_dingtalk_url(url: &str, secret: &str) -> Result<String, String> {
    let timestamp = now_millis();
    let message = format!("{timestamp}\n{secret}");
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|error| error.to_string())?;
    mac.update(message.as_bytes());
    let signature = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    let encoded = url::form_urlencoded::byte_serialize(signature.as_bytes()).collect::<String>();
    let separator = if url.contains('?') { '&' } else { '?' };
    Ok(format!(
        "{url}{separator}timestamp={timestamp}&sign={encoded}"
    ))
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(kind: &str, events: &[&str]) -> WebhookConfig {
        WebhookConfig {
            name: kind.into(),
            kind: kind.into(),
            url: "https://example.com/hook".into(),
            enabled: true,
            events: events.iter().map(|event| (*event).into()).collect(),
            dingtalk_sign: None,
            telegram_chat_id: Some("1".into()),
            custom_template: None,
        }
    }

    #[tokio::test]
    async fn incidents_alert_once_until_resolved() {
        let target = target("dingtalk", &["account-quota-exhausted"]);
        let notifier = Notifier::new(vec![target.clone()], NotifyConfig::default(), 1);
        let mut event = WebhookEvent::new(WebhookEventKind::AccountQuotaExhausted, "title", "body");
        event.account_id = Some("acc_1".into());

        assert!(notifier.mark_send(&target, &event).is_some());
        assert!(notifier.mark_send(&target, &event).is_none());
        notifier.resolve_incident(WebhookEventKind::AccountQuotaExhausted, Some("acc_1"));
        assert!(notifier.mark_send(&target, &event).is_some());
    }

    #[test]
    fn legacy_subscriptions_map_to_the_narrowed_incidents() {
        assert!(WebhookEventKind::AccountQuotaExhausted.matches_subscription("quota-exhausted"));
        assert!(WebhookEventKind::ServiceQuotaExhausted.matches_subscription("service-degraded"));
        assert!(WebhookEventKind::TokenRefreshFailed.matches_subscription("token-expired"));
    }

    #[test]
    fn all_six_payload_kinds_are_supported() {
        let event = WebhookEvent::new(
            WebhookEventKind::ServiceQuotaExhausted,
            "title",
            "- **状态：** 异常",
        );
        for kind in [
            "dingtalk",
            "wechat-work",
            "feishu",
            "telegram",
            "discord",
            "custom",
        ] {
            let target = target(kind, &["service-quota-exhausted"]);
            assert!(payload(&target, &event).is_ok(), "{kind}");
        }
    }

    #[test]
    fn native_webhook_payloads_use_markdown() {
        let event = WebhookEvent::new(
            WebhookEventKind::TokenRefreshFailed,
            "Token 刷新失败",
            "- **账号：** `user@example.com`",
        );
        let (_, dingtalk) = payload(&target("dingtalk", &[]), &event).expect("dingtalk");
        assert_eq!(dingtalk["msgtype"], "markdown");
        assert!(dingtalk["markdown"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("**账号：**")));

        let (_, wechat) = payload(&target("wechat-work", &[]), &event).expect("wechat");
        assert_eq!(wechat["msgtype"], "markdown");
        let (_, feishu) = payload(&target("feishu", &[]), &event).expect("feishu");
        assert_eq!(feishu["msg_type"], "interactive");
        assert_eq!(feishu["card"]["elements"][0]["tag"], "markdown");

        let (_, custom) = payload(&target("custom", &[]), &event).expect("custom");
        assert_eq!(custom["event"], "token-refresh-failed");
        assert_eq!(custom["message"], "- **账号：** `user@example.com`");
    }

    #[test]
    fn dingtalk_signature_is_url_encoded() {
        let signed = signed_dingtalk_url("https://example.com/hook?a=1", "secret").expect("sign");
        assert!(signed.contains("&timestamp="));
        assert!(signed.contains("&sign="));
        assert!(!signed.contains('+'));
    }
}
