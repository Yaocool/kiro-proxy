//! Non-blocking multi-target webhook notifications.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use hmac::{Hmac, Mac};
use kam_core::config::{NotifyConfig, WebhookConfig};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::sync::mpsc;
use tracing::{debug, warn};

type HmacSha256 = Hmac<Sha256>;
type LowCreditLevels = HashMap<(String, String), HashSet<usize>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WebhookEventKind {
    LowCredit,
    AccountBanned,
    TokenExpired,
    QuotaExhausted,
    ServiceDegraded,
}

impl WebhookEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LowCredit => "low-credit",
            Self::AccountBanned => "account-banned",
            Self::TokenExpired => "token-expired",
            Self::QuotaExhausted => "quota-exhausted",
            Self::ServiceDegraded => "service-degraded",
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
    #[serde(default)]
    pub remaining_percent: Option<f64>,
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
            remaining_percent: None,
            timestamp: now_secs(),
        }
    }
}

#[derive(Clone)]
pub struct Notifier {
    senders: Arc<HashMap<String, mpsc::Sender<Delivery>>>,
    targets: Arc<Vec<WebhookConfig>>,
    config: NotifyConfig,
    suppression: Arc<Mutex<HashMap<String, i64>>>,
    low_credit_levels: Arc<Mutex<LowCreditLevels>>,
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
    LowCredit {
        target: String,
        account: String,
        level: usize,
    },
    Window {
        key: String,
        timestamp: i64,
    },
}

impl Notifier {
    pub fn new(targets: Vec<WebhookConfig>, config: NotifyConfig, queue_size: usize) -> Self {
        Self::with_state(
            targets,
            config,
            queue_size,
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(VecDeque::new())),
        )
    }

    fn with_state(
        targets: Vec<WebhookConfig>,
        config: NotifyConfig,
        queue_size: usize,
        suppression: Arc<Mutex<HashMap<String, i64>>>,
        low_credit_levels: Arc<Mutex<LowCreditLevels>>,
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
            config,
            suppression,
            low_credit_levels,
            history,
        }
    }

    /// Replace delivery targets while preserving suppression/progressive state
    /// and delivery history across config hot reloads.
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
            Arc::clone(&self.suppression),
            Arc::clone(&self.low_credit_levels),
            Arc::clone(&self.history),
        )
    }

    pub fn emit(&self, event: WebhookEvent) -> usize {
        self.emit_matching(None, event)
    }

    pub fn emit_to(&self, name: &str, event: WebhookEvent) -> usize {
        self.emit_matching(Some(name), event)
    }

    fn emit_matching(&self, name: Option<&str>, event: WebhookEvent) -> usize {
        let mut queued = 0;
        for target in self.targets.iter() {
            if name.is_some_and(|name| target.name != name)
                || !target.enabled
                || !target.events.iter().any(|kind| kind == event.kind.as_str())
            {
                continue;
            }
            let Some(mark) = self.mark_send(target, &event) else {
                continue;
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
        if event.kind == WebhookEventKind::LowCredit {
            let Some(remaining) = event.remaining_percent else {
                return Some(SuppressionMark::None);
            };
            let levels = progressive_levels(
                self.config.low_credit_threshold_percent,
                self.config.max_notifications,
            );
            let crossed = levels
                .iter()
                .enumerate()
                .filter(|(_, threshold)| remaining <= **threshold)
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            if crossed.is_empty() {
                self.reset_low_credit(account);
                return None;
            }
            let mut state = lock(&self.low_credit_levels);
            let notified = state
                .entry((target.name.clone(), account.into()))
                .or_default();
            if let Some(level) = crossed.into_iter().find(|level| !notified.contains(level)) {
                notified.insert(level);
                return Some(SuppressionMark::LowCredit {
                    target: target.name.clone(),
                    account: account.into(),
                    level,
                });
            }
            return None;
        }
        let key = format!("{}:{}:{account}", target.name, event.kind.as_str());
        let now = now_millis();
        let mut suppression = lock(&self.suppression);
        if suppression
            .get(&key)
            .is_some_and(|last| now.saturating_sub(*last) < self.config.suppress_window_ms as i64)
        {
            return None;
        }
        suppression.insert(key.clone(), now);
        Some(SuppressionMark::Window {
            key,
            timestamp: now,
        })
    }

    fn rollback_mark(&self, mark: SuppressionMark) {
        match mark {
            SuppressionMark::None => {}
            SuppressionMark::LowCredit {
                target,
                account,
                level,
            } => {
                let mut levels = lock(&self.low_credit_levels);
                if let Some(notified) = levels.get_mut(&(target.clone(), account.clone())) {
                    notified.remove(&level);
                    if notified.is_empty() {
                        levels.remove(&(target, account));
                    }
                }
            }
            SuppressionMark::Window { key, timestamp } => {
                let mut suppression = lock(&self.suppression);
                if suppression.get(&key) == Some(&timestamp) {
                    suppression.remove(&key);
                }
            }
        }
    }

    pub fn reset_low_credit(&self, account_id: &str) {
        lock(&self.low_credit_levels).retain(|(_, account), _| account != account_id);
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

pub fn progressive_levels(threshold: f64, maximum: u32) -> Vec<f64> {
    if threshold <= 0.0 || maximum == 0 {
        return Vec::new();
    }
    (0..maximum)
        .map(|index| threshold * (maximum - index) as f64 / maximum as f64)
        .collect()
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
    let text = format!("{}\n{}", event.title, event.message);
    match target.kind.as_str() {
        "dingtalk" => {
            let url = if let Some(secret) = &target.dingtalk_sign {
                signed_dingtalk_url(&target.url, secret)?
            } else {
                target.url.clone()
            };
            Ok((url, json!({"msgtype":"text","text":{"content":text}})))
        }
        "wechat-work" | "wechat" => Ok((
            target.url.clone(),
            json!({"msgtype":"text","text":{"content":text}}),
        )),
        "feishu" => Ok((
            target.url.clone(),
            json!({"msg_type":"text","content":{"text":text}}),
        )),
        "telegram" => Ok((
            target.url.clone(),
            json!({"chat_id":target.telegram_chat_id,"text":text}),
        )),
        "discord" => Ok((target.url.clone(), json!({"content":text}))),
        "custom" => {
            let body = target
                .custom_template
                .as_deref()
                .unwrap_or(r#"{"event":"{{event}}","title":"{{title}}","message":"{{message}}"}"#)
                .replace("{{event}}", event.kind.as_str())
                .replace("{{title}}", &event.title)
                .replace("{{message}}", &event.message);
            let value = serde_json::from_str(&body).unwrap_or_else(|_| json!({"text":body}));
            Ok((target.url.clone(), value))
        }
        kind => Err(format!("unknown webhook kind {kind}")),
    }
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

    #[test]
    fn progressive_alert_levels_match_typescript_behavior() {
        assert_eq!(progressive_levels(10.0, 5), vec![10.0, 8.0, 6.0, 4.0, 2.0]);
    }

    #[test]
    fn all_six_payload_kinds_are_supported() {
        let event = WebhookEvent::new(WebhookEventKind::ServiceDegraded, "title", "body");
        for kind in [
            "dingtalk",
            "wechat-work",
            "feishu",
            "telegram",
            "discord",
            "custom",
        ] {
            let target = WebhookConfig {
                name: kind.into(),
                kind: kind.into(),
                url: "https://example.com/hook".into(),
                enabled: true,
                events: vec!["service-degraded".into()],
                dingtalk_sign: None,
                telegram_chat_id: Some("1".into()),
                custom_template: None,
            };
            assert!(payload(&target, &event).is_ok(), "{kind}");
        }
    }

    #[test]
    fn dingtalk_signature_is_url_encoded() {
        let signed = signed_dingtalk_url("https://example.com/hook?a=1", "secret").expect("sign");
        assert!(signed.contains("&timestamp="));
        assert!(signed.contains("&sign="));
        assert!(!signed.contains('+'));
    }
}
