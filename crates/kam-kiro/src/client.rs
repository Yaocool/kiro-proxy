//! Upstream HTTP client with independent short-request and streaming pools.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use futures::future::{BoxFuture, FutureExt, Shared};
use futures::{Stream, StreamExt};
use kam_core::account::{Account, Subscription, SubscriptionKind, Usage};
use kam_core::config::{AgentMode, UpstreamConfig};
use kam_translate::KiroPayload;
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::codec::Decoder;
use tracing::debug;
use uuid::Uuid;

use crate::endpoint::{
    endpoint_for_auth, EndpointCache, EndpointDefinition, EndpointKey, EndpointOverrides,
    EndpointPurpose,
};
use crate::event_stream::{EventStreamDecoder, KiroEvent};

#[derive(Debug, Clone, Error)]
#[error("Kiro {endpoint} returned {status:?}: {message}")]
pub struct KiroError {
    pub status: Option<u16>,
    pub endpoint: String,
    pub message: String,
}

impl KiroError {
    pub fn is_auth(&self) -> bool {
        matches!(self.status, Some(401 | 403)) || text_is_auth_error(&self.message)
    }

    pub fn is_quota(&self) -> bool {
        self.status == Some(402) || text_is_quota_error(&self.message)
    }

    pub fn is_throttle(&self) -> bool {
        self.status == Some(429) || text_is_throttle_error(&self.message)
    }

    pub fn is_retriable(&self) -> bool {
        self.status.is_none() || self.is_throttle() || matches!(self.status, Some(500..=599))
    }
}

pub fn text_is_auth_error(message: &str) -> bool {
    let text = message.to_ascii_lowercase();
    [
        "unauthorized",
        "unauthenticated",
        "invalid token",
        "expired token",
        "token expired",
        "access denied",
        "authentication",
    ]
    .iter()
    .any(|marker| text.contains(marker))
        || contains_status(&text, 401)
        || contains_status(&text, 403)
}

pub fn text_is_quota_error(message: &str) -> bool {
    let text = message.to_ascii_lowercase();
    [
        "quota",
        "credit exhausted",
        "credits exhausted",
        "insufficient credit",
        "usage limit",
    ]
    .iter()
    .any(|marker| text.contains(marker))
        || contains_status(&text, 402)
}

pub fn text_is_throttle_error(message: &str) -> bool {
    let text = message.to_ascii_lowercase();
    [
        "throttl",
        "too many requests",
        "rate limit",
        "rate exceeded",
    ]
    .iter()
    .any(|marker| text.contains(marker))
        || contains_status(&text, 429)
}

fn contains_status(text: &str, status: u16) -> bool {
    let status = status.to_string();
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .any(|word| word == status)
}

pub struct KiroResponse {
    pub endpoint: EndpointDefinition,
    pub response: Response,
    permit: tokio::sync::OwnedSemaphorePermit,
}

impl KiroResponse {
    pub fn into_parts(
        self,
    ) -> (
        EndpointDefinition,
        Response,
        tokio::sync::OwnedSemaphorePermit,
    ) {
        (self.endpoint, self.response, self.permit)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub model_id: String,
    #[serde(default)]
    pub model_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub rate_multiplier: Option<f64>,
    #[serde(default)]
    pub token_limits: Option<TokenLimits>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenLimits {
    #[serde(default)]
    pub max_input_tokens: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionList {
    #[serde(default)]
    pub disclaimer: Vec<String>,
    #[serde(default)]
    pub subscription_plans: Vec<SubscriptionPlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionPlan {
    pub name: String,
    #[serde(default)]
    pub q_subscription_type: String,
    #[serde(default)]
    pub description: SubscriptionDescription,
    #[serde(default)]
    pub pricing: SubscriptionPricing,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionDescription {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub billing_interval: String,
    #[serde(default)]
    pub feature_header: String,
    #[serde(default)]
    pub features: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubscriptionPricing {
    #[serde(default)]
    pub amount: f64,
    #[serde(default)]
    pub currency: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageInfo {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub credits: f64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimits {
    #[serde(default)]
    pub usage_breakdown_list: Vec<UsageBreakdown>,
    #[serde(default)]
    pub next_date_reset: Option<serde_json::Value>,
    #[serde(default)]
    pub days_until_reset: Option<i64>,
    #[serde(default)]
    pub subscription_info: Option<UsageSubscription>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBreakdown {
    #[serde(default, alias = "type")]
    pub resource_type: String,
    #[serde(default)]
    pub current_usage: Option<f64>,
    #[serde(default)]
    pub current_usage_with_precision: Option<f64>,
    #[serde(default)]
    pub usage_limit: Option<f64>,
    #[serde(default)]
    pub usage_limit_with_precision: Option<f64>,
    #[serde(default)]
    pub free_trial_info: Option<UsageAllowance>,
    #[serde(default)]
    pub free_trial_usage: Option<UsageAllowance>,
    #[serde(default)]
    pub bonuses: Vec<UsageAllowance>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageAllowance {
    #[serde(default)]
    pub current_usage: Option<f64>,
    #[serde(default)]
    pub current_usage_with_precision: Option<f64>,
    #[serde(default)]
    pub usage_limit: Option<f64>,
    #[serde(default)]
    pub usage_limit_with_precision: Option<f64>,
    #[serde(default)]
    pub free_trial_status: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSubscription {
    #[serde(default)]
    pub subscription_title: String,
    #[serde(default, alias = "type")]
    pub subscription_type: String,
}

impl UsageLimits {
    pub fn normalized_usage(&self, now: i64) -> Option<Usage> {
        let credit = self
            .usage_breakdown_list
            .iter()
            .find(|item| matches!(item.resource_type.as_str(), "CREDIT" | "AGENTIC_REQUEST"))?;
        let mut current = precise(credit.current_usage_with_precision, credit.current_usage);
        let mut limit = precise(credit.usage_limit_with_precision, credit.usage_limit);
        if let Some(trial) = credit
            .free_trial_info
            .as_ref()
            .or(credit.free_trial_usage.as_ref())
            .filter(|trial| trial.free_trial_status.as_deref() == Some("ACTIVE"))
        {
            current += precise(trial.current_usage_with_precision, trial.current_usage);
            limit += precise(trial.usage_limit_with_precision, trial.usage_limit);
        }
        for bonus in &credit.bonuses {
            current += precise(bonus.current_usage_with_precision, bonus.current_usage);
            limit += precise(bonus.usage_limit_with_precision, bonus.usage_limit);
        }
        Some(Usage {
            current,
            limit,
            percent_used: if limit > 0.0 {
                (current / limit * 100.0).clamp(0.0, 100.0)
            } else {
                0.0
            },
            next_reset_date: self.next_date_reset.as_ref().map(display_reset),
            updated_at: now,
        })
    }

    pub fn normalized_subscription(&self) -> Option<Subscription> {
        let source = self.subscription_info.as_ref()?;
        let raw = if source.subscription_type.is_empty() {
            source.subscription_title.clone()
        } else {
            source.subscription_type.clone()
        };
        Some(Subscription {
            kind: subscription_kind(&raw, &source.subscription_title),
            title: (!source.subscription_title.is_empty())
                .then(|| source.subscription_title.clone()),
            raw_type: (!raw.is_empty()).then_some(raw),
            expires_at: self
                .next_date_reset
                .as_ref()
                .and_then(serde_json::Value::as_i64),
            days_remaining: self.days_until_reset,
        })
    }
}

fn precise(precise: Option<f64>, fallback: Option<f64>) -> f64 {
    precise.or(fallback).unwrap_or(0.0)
}

fn display_reset(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn subscription_kind(raw: &str, title: &str) -> SubscriptionKind {
    let value = format!("{raw} {title}").to_ascii_uppercase();
    if value.contains("PRO_PLUS") || value.contains("PRO+") {
        SubscriptionKind::ProPlus
    } else if value.contains("POWER") {
        SubscriptionKind::Power
    } else if value.contains("ENTERPRISE") {
        SubscriptionKind::Enterprise
    } else if value.contains("TEAM") {
        SubscriptionKind::Teams
    } else if value.contains("PRO") {
        SubscriptionKind::Pro
    } else if value.contains("FREE") {
        SubscriptionKind::Free
    } else {
        SubscriptionKind::Unknown
    }
}

#[derive(Clone)]
pub struct KiroClient {
    short: Client,
    stream: Client,
    cache: Arc<EndpointCache>,
    overrides: EndpointOverrides,
    upstream: UpstreamConfig,
    short_slots: Arc<tokio::sync::Semaphore>,
    stream_slots: Arc<tokio::sync::Semaphore>,
    model_fetches: Arc<DashMap<(String, String), InFlightModelFetch>>,
}

type SharedModelFetch = Shared<BoxFuture<'static, Result<Vec<ModelInfo>, KiroError>>>;

struct InFlightModelFetch {
    id: Uuid,
    future: SharedModelFetch,
}

impl KiroClient {
    pub fn new(upstream: UpstreamConfig, overrides: EndpointOverrides) -> Result<Self, KiroError> {
        let idle = Duration::from_millis(
            upstream
                .pool
                .keep_alive_idle_ms
                .saturating_sub(2_000)
                .max(1)
                .min(upstream.pool.keep_alive_max_ms.max(1)),
        );
        let short = Client::builder()
            .pool_max_idle_per_host(upstream.pool.http_max_connections)
            .pool_idle_timeout(idle)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(build_error)?;
        // No total/read timeout for streams: legitimate thinking pauses may be long.
        // hyper does not pipeline HTTP/1.1 requests, avoiding long-stream HOL blocking.
        let stream = Client::builder()
            .pool_max_idle_per_host(upstream.pool.stream_max_connections)
            .pool_idle_timeout(idle)
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(build_error)?;
        Ok(Self {
            short,
            stream,
            cache: Arc::new(EndpointCache::default()),
            overrides,
            short_slots: Arc::new(tokio::sync::Semaphore::new(
                upstream
                    .pool
                    .http_max_connections
                    .saturating_mul(upstream.pool.http_pipelining)
                    .max(1),
            )),
            stream_slots: Arc::new(tokio::sync::Semaphore::new(
                upstream
                    .pool
                    .stream_max_connections
                    .saturating_mul(upstream.pool.stream_pipelining)
                    .max(1),
            )),
            model_fetches: Arc::new(DashMap::new()),
            upstream,
        })
    }

    pub fn endpoint_cache(&self) -> Arc<EndpointCache> {
        Arc::clone(&self.cache)
    }

    pub async fn generate(
        &self,
        account: &Account,
        payload: &KiroPayload,
        explicit: Option<EndpointKey>,
    ) -> Result<KiroResponse, KiroError> {
        let explicit = explicit.or(self.upstream.preferred_endpoint.map(EndpointKey::from));
        let order = self
            .cache
            .order(account, explicit, EndpointPurpose::Generation);
        if order.is_empty() {
            return Err(KiroError {
                status: Some(403),
                endpoint: "all".into(),
                message: "All Kiro endpoints previously returned 403; token refresh required"
                    .into(),
            });
        }
        let mut auth_rejections = 0usize;
        let mut forbidden_rejections = 0usize;
        let attempt_count = order.len();
        for key in order {
            let endpoint = EndpointDefinition::for_key(key, &self.overrides);
            match self
                .send_generation(account, payload, endpoint.clone())
                .await
            {
                Ok(response) => {
                    self.cache
                        .mark_success(&account.id, EndpointPurpose::Generation, endpoint.key);
                    return Ok(response);
                }
                Err(mut error) => {
                    if error.status == Some(401) {
                        auth_rejections += 1;
                        continue;
                    }
                    if error.status == Some(403) {
                        if error.is_quota() || error.is_throttle() {
                            error.status = Some(429);
                            return Err(error);
                        }
                        self.cache.mark_disabled(
                            &account.id,
                            EndpointPurpose::Generation,
                            endpoint.key,
                        );
                        auth_rejections += 1;
                        forbidden_rejections += 1;
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        if auth_rejections == attempt_count {
            return Err(KiroError {
                status: Some(403),
                endpoint: "all".into(),
                message: if forbidden_rejections == attempt_count {
                    "All Kiro endpoints returned 403; token refresh required"
                } else {
                    "All Kiro endpoints returned 401/403; token refresh required"
                }
                .into(),
            });
        }
        Err(KiroError {
            status: None,
            endpoint: "none".into(),
            message: "no usable upstream endpoint".into(),
        })
    }

    async fn send_generation(
        &self,
        account: &Account,
        payload: &KiroPayload,
        endpoint: EndpointDefinition,
    ) -> Result<KiroResponse, KiroError> {
        let permit = Arc::clone(&self.stream_slots)
            .acquire_owned()
            .await
            .map_err(build_error)?;
        let mut payload = payload.clone();
        set_payload_origin(&mut payload, endpoint.origin);
        let response = self
            .stream
            .post(&endpoint.url)
            .headers(headers(account, &endpoint, self.upstream.agent_mode)?)
            .json(&payload)
            .send()
            .await
            .map_err(|error| KiroError {
                status: None,
                endpoint: endpoint.name.into(),
                message: error.to_string(),
            })?;
        if response.status().is_success() {
            return Ok(KiroResponse {
                endpoint,
                response,
                permit,
            });
        }
        Err(response_error(response, &endpoint).await)
    }

    pub async fn collect_events(&self, response: Response) -> Result<Vec<KiroEvent>, KiroError> {
        let mut source = response.bytes_stream();
        let mut buffer = bytes::BytesMut::new();
        let mut decoder = EventStreamDecoder;
        let mut output = Vec::new();
        while let Some(chunk) = source.next().await {
            let chunk = chunk.map_err(build_error)?;
            buffer.extend_from_slice(&chunk);
            while let Some(event) = decoder.decode(&mut buffer).map_err(build_error)? {
                if let KiroEvent::Error { kind, message } = &event {
                    return Err(KiroError {
                        status: None,
                        endpoint: "event-stream".into(),
                        message: format!("{kind}: {message}"),
                    });
                }
                output.push(event);
            }
        }
        while let Some(event) = decoder.decode_eof(&mut buffer).map_err(build_error)? {
            if let KiroEvent::Error { kind, message } = &event {
                return Err(KiroError {
                    status: None,
                    endpoint: "event-stream".into(),
                    message: format!("{kind}: {message}"),
                });
            }
            output.push(event);
        }
        Ok(output)
    }

    pub fn event_stream(
        response: Response,
    ) -> impl Stream<Item = Result<Bytes, reqwest::Error>> + Send {
        response.bytes_stream()
    }

    pub async fn get_usage_limits(&self, account: &Account) -> Result<UsageLimits, KiroError> {
        let _permit = Arc::clone(&self.short_slots)
            .acquire_owned()
            .await
            .map_err(build_error)?;
        let endpoint = EndpointDefinition::for_key(EndpointKey::Amazonq, &self.overrides);
        let base = endpoint
            .url
            .split("/generateAssistantResponse")
            .next()
            .unwrap_or(&endpoint.url);
        let mut url = url::Url::parse(&format!(
            "{base}/getUsageLimits?origin=AI_EDITOR&isEmailRequired=true"
        ))
        .map_err(build_error)?;
        if let Some(profile_arn) = account.profile_arn.as_deref() {
            url.query_pairs_mut().append_pair("profileArn", profile_arn);
        }
        let response = self
            .short
            .get(url)
            .header("accept", "application/json")
            .header(
                "authorization",
                format!("Bearer {}", account.credentials.access_token),
            )
            .header(
                "user-agent",
                format!(
                    "aws-sdk-js/1.0.18 ua/2.1 os/windows lang/js md/nodejs#20.16.0 api/codewhispererstreaming#1.0.18 m/E KiroIDE-0.6.18-{}",
                    account.machine_id
                ),
            )
            .header(
                "x-amz-user-agent",
                format!("aws-sdk-js/1.0.18 KiroIDE 0.6.18 {}", account.machine_id),
            )
            .send()
            .await
            .map_err(build_error)?;
        if !response.status().is_success() {
            return Err(response_error(response, &endpoint).await);
        }
        response.json::<UsageLimits>().await.map_err(build_error)
    }

    pub async fn list_models(&self, account: &Account) -> Result<Vec<ModelInfo>, KiroError> {
        let fetch_key = (account.id.clone(), account.credentials.access_token.clone());
        let fetch_id = Uuid::new_v4();
        let fetch = match self.model_fetches.entry(fetch_key.clone()) {
            Entry::Occupied(entry) => entry.get().future.clone(),
            Entry::Vacant(entry) => {
                let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
                let future = async move {
                    result_receiver.await.unwrap_or_else(|_| {
                        Err(KiroError {
                            status: None,
                            endpoint: "models".into(),
                            message: "model discovery task stopped unexpectedly".into(),
                        })
                    })
                }
                .boxed()
                .shared();
                entry.insert(InFlightModelFetch {
                    id: fetch_id,
                    future: future.clone(),
                });

                let client = self.clone();
                let account = account.clone();
                let fetches = Arc::clone(&self.model_fetches);
                tokio::spawn(async move {
                    let result = client.fetch_models_once(&account).await;
                    let _sent = result_sender.send(result);
                    fetches.remove_if(&fetch_key, |_, entry| entry.id == fetch_id);
                });
                future
            }
        };
        fetch.await
    }

    async fn fetch_models_once(&self, account: &Account) -> Result<Vec<ModelInfo>, KiroError> {
        let endpoint_revision = self.cache.revision(&account.id);
        let order = self.cache.order(
            account,
            self.upstream.preferred_endpoint.map(EndpointKey::from),
            EndpointPurpose::Models,
        );
        let mut last_error = None;
        for key in order {
            let _permit = Arc::clone(&self.short_slots)
                .acquire_owned()
                .await
                .map_err(build_error)?;
            let endpoint = EndpointDefinition::for_key(key, &self.overrides);
            let base = endpoint
                .url
                .split("/generateAssistantResponse")
                .next()
                .unwrap_or(&endpoint.url);
            let url = format!(
                "{base}/ListAvailableModels?origin={}&maxResults=50",
                endpoint.origin
            );
            let request = self.short.get(url).headers(metadata_headers(
                account,
                &endpoint,
                self.upstream.agent_mode,
                false,
            )?);
            let response = match tokio::time::timeout(Duration::from_secs(15), request.send()).await
            {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => return Err(build_error(error)),
                Err(_) => {
                    return Err(KiroError {
                        status: None,
                        endpoint: endpoint.name.into(),
                        message: "ListAvailableModels timed out after 15 seconds".into(),
                    });
                }
            };
            if response.status().is_success() {
                #[derive(Deserialize)]
                struct Models {
                    #[serde(default)]
                    models: Vec<ModelInfo>,
                }
                let models = response.json::<Models>().await.map_err(build_error)?.models;
                if !models.is_empty() {
                    self.cache.mark_success_if_revision(
                        &account.id,
                        EndpointPurpose::Models,
                        key,
                        endpoint_revision,
                    );
                    return Ok(models);
                }
                last_error = Some(KiroError {
                    status: None,
                    endpoint: endpoint.name.into(),
                    message: "upstream returned an empty model list".into(),
                });
                continue;
            } else {
                let mut error = response_error(response, &endpoint).await;
                match error.status {
                    Some(401) => {
                        last_error = Some(error);
                        continue;
                    }
                    Some(403) if error.is_quota() || error.is_throttle() => {
                        error.status = Some(429);
                        return Err(error);
                    }
                    Some(403) => {
                        self.cache.mark_disabled_if_revision(
                            &account.id,
                            EndpointPurpose::Models,
                            key,
                            endpoint_revision,
                        );
                        last_error = Some(error);
                        continue;
                    }
                    _ => return Err(error),
                }
            }
        }
        Err(last_error.unwrap_or_else(|| KiroError {
            status: None,
            endpoint: "models".into(),
            message: "upstream returned no models".into(),
        }))
    }

    pub async fn list_subscriptions(
        &self,
        account: &Account,
    ) -> Result<SubscriptionList, KiroError> {
        let _permit = Arc::clone(&self.short_slots)
            .acquire_owned()
            .await
            .map_err(build_error)?;
        let endpoint = EndpointDefinition::for_key(
            endpoint_for_auth(account.credentials.auth_method),
            &self.overrides,
        );
        let base = endpoint
            .url
            .split("/generateAssistantResponse")
            .next()
            .unwrap_or(&endpoint.url);
        let response = match self
            .short
            .post(format!("{base}/listAvailableSubscriptions"))
            .headers(metadata_headers(
                account,
                &endpoint,
                self.upstream.agent_mode,
                true,
            )?)
            .json(&serde_json::json!({}))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(
                    account = %account.id,
                    endpoint = endpoint.name,
                    %error,
                    "subscription discovery failed; returning an empty result"
                );
                return Ok(SubscriptionList::default());
            }
        };
        if !response.status().is_success() {
            tracing::warn!(
                account = %account.id,
                endpoint = endpoint.name,
                status = %response.status(),
                "subscription discovery was rejected; returning an empty result"
            );
            return Ok(SubscriptionList::default());
        }
        match response.json::<SubscriptionList>().await {
            Ok(subscriptions) => Ok(subscriptions),
            Err(error) => {
                tracing::warn!(
                    account = %account.id,
                    endpoint = endpoint.name,
                    %error,
                    "subscription response was invalid; returning an empty result"
                );
                Ok(SubscriptionList::default())
            }
        }
    }
}

fn set_payload_origin(payload: &mut KiroPayload, origin: &str) {
    payload
        .conversation_state
        .current_message
        .user_input_message
        .origin = origin.into();
    for message in &mut payload.conversation_state.history {
        if let Some(user) = &mut message.user_input_message {
            user.origin = origin.into();
        }
    }
}

fn headers(
    account: &Account,
    endpoint: &EndpointDefinition,
    mode: AgentMode,
) -> Result<reqwest::header::HeaderMap, KiroError> {
    use reqwest::header::{
        HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT,
    };
    let mut headers = HeaderMap::new();
    let cli = endpoint.origin == "CLI";
    let user_agent = if cli {
        "aws-sdk-rust/1.3.9 os/macos lang/rust/1.87.0".to_string()
    } else {
        format!(
            "aws-sdk-js/1.0.18 ua/2.1 os/windows lang/js md/nodejs#20.16.0 api/codewhispererstreaming#1.0.18 m/E KiroIDE-0.6.18-{}",
            account.machine_id
        )
    };
    let amz_user_agent = if cli {
        "aws-sdk-rust/1.3.9 ua/2.1 api/ssooidc/1.88.0 os/macos lang/rust/1.87.0 m/E app/AmazonQ-For-CLI".to_string()
    } else {
        format!("aws-sdk-js/1.0.18 KiroIDE 0.6.18 {}", account.machine_id)
    };
    let mode = match mode {
        AgentMode::Auto if cli => "vibe",
        AgentMode::Auto => "spec",
        AgentMode::Vibe => "vibe",
        AgentMode::Spec => "spec",
    };
    let authorization = format!("Bearer {}", account.credentials.access_token);
    let pairs = [
        (CONTENT_TYPE, "application/json"),
        (ACCEPT, "*/*"),
        (USER_AGENT, &user_agent),
        (AUTHORIZATION, &authorization),
    ];
    for (name, value) in pairs {
        headers.insert(name, HeaderValue::from_str(value).map_err(build_error)?);
    }
    let invocation_id = Uuid::new_v4().to_string();
    for (name, value) in [
        ("x-amz-target", endpoint.amz_target),
        ("x-amz-user-agent", amz_user_agent.as_str()),
        ("x-amzn-kiro-agent-mode", mode),
        ("x-amzn-codewhisperer-optout", "true"),
        ("amz-sdk-request", "attempt=1; max=3"),
        ("amz-sdk-invocation-id", invocation_id.as_str()),
    ] {
        headers.insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(build_error)?,
            HeaderValue::from_str(value).map_err(build_error)?,
        );
    }
    debug!(account = %account.id, endpoint = endpoint.name, auth = ?account.credentials.auth_method, "built upstream headers");
    Ok(headers)
}

fn metadata_headers(
    account: &Account,
    endpoint: &EndpointDefinition,
    mode: AgentMode,
    subscriptions: bool,
) -> Result<reqwest::header::HeaderMap, KiroError> {
    let mut output = headers(account, endpoint, mode)?;
    output.remove("x-amz-target");
    if subscriptions {
        output.insert(
            reqwest::header::HeaderName::from_static("x-amzn-codewhisperer-optout-preference"),
            reqwest::header::HeaderValue::from_static("OPTIN"),
        );
    }
    Ok(output)
}

async fn response_error(response: Response, endpoint: &EndpointDefinition) -> KiroError {
    let status = response.status();
    let text = response.text().await.unwrap_or_else(|_| status.to_string());
    KiroError {
        status: Some(status.as_u16()),
        endpoint: endpoint.name.into(),
        message: text.chars().take(2_000).collect(),
    }
}

fn build_error(error: impl std::fmt::Display) -> KiroError {
    KiroError {
        status: None,
        endpoint: "transport".into(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use kam_core::account::{AuthMethod, Credentials};
    use kam_translate::{KiroConversationState, KiroCurrentMessage, KiroUserInputMessage};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn account(method: AuthMethod) -> Account {
        Account {
            id: "acc_test".into(),
            email: "test@example.com".into(),
            label: None,
            enabled: true,
            machine_id: "a".repeat(64),
            profile_arn: None,
            credentials: Credentials {
                access_token: "access-token".into(),
                refresh_token: None,
                client_id: None,
                client_secret: None,
                region: "us-east-1".into(),
                expires_at: i64::MAX,
                auth_method: method,
            },
            usage: None,
            subscription: None,
            tags: Vec::new(),
            created_at: 0,
            credit_exhausted: false,
        }
    }

    fn payload() -> KiroPayload {
        KiroPayload {
            conversation_state: KiroConversationState {
                chat_trigger_type: "MANUAL".into(),
                conversation_id: "conversation".into(),
                current_message: KiroCurrentMessage {
                    user_input_message: KiroUserInputMessage {
                        content: "hello".into(),
                        model_id: "claude-sonnet-4".into(),
                        origin: "AI_EDITOR".into(),
                        images: Vec::new(),
                        user_input_message_context: None,
                    },
                },
                history: Vec::new(),
            },
            profile_arn: None,
            inference_config: None,
        }
    }

    fn test_client(server: &MockServer) -> KiroClient {
        KiroClient::new(
            UpstreamConfig::default(),
            EndpointOverrides {
                amazonq_url: Some(format!("{}/amazon/generateAssistantResponse", server.uri())),
                codewhisperer_url: Some(format!(
                    "{}/codewhisperer/generateAssistantResponse",
                    server.uri()
                )),
            },
        )
        .expect("client")
    }

    #[tokio::test]
    async fn generation_tries_401_fallback_serially_and_caches_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/amazon/generateAssistantResponse"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Auth error"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/codewhisperer/generateAssistantResponse"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let client = test_client(&server);
        let account = account(AuthMethod::Idc);

        let response = client
            .generate(&account, &payload(), None)
            .await
            .expect("fallback succeeds");

        assert_eq!(response.endpoint.key, EndpointKey::Codewhisperer);
        assert_eq!(
            client
                .endpoint_cache()
                .preferred(&account.id, EndpointPurpose::Generation),
            Some(EndpointKey::Codewhisperer)
        );
        let paths = server
            .received_requests()
            .await
            .expect("requests")
            .into_iter()
            .map(|request| request.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                "/amazon/generateAssistantResponse",
                "/codewhisperer/generateAssistantResponse"
            ]
        );
    }

    #[tokio::test]
    async fn generation_maps_throttled_403_to_429_without_switching_or_disabling() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/amazon/generateAssistantResponse"))
            .respond_with(
                ResponseTemplate::new(403).set_body_string("ThrottlingException: rate exceeded"),
            )
            .mount(&server)
            .await;
        let client = test_client(&server);
        let account = account(AuthMethod::Idc);

        let error = match client.generate(&account, &payload(), None).await {
            Ok(_) => panic!("throttled request unexpectedly succeeded"),
            Err(error) => error,
        };

        assert_eq!(error.status, Some(429));
        assert!(error.is_throttle());
        assert_eq!(
            client
                .endpoint_cache()
                .order(&account, None, EndpointPurpose::Generation)[0],
            EndpointKey::Amazonq
        );
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/amazon/generateAssistantResponse");
    }

    #[tokio::test]
    async fn generation_disables_plain_403_and_uses_cached_fallback_next_time() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/amazon/generateAssistantResponse"))
            .respond_with(ResponseTemplate::new(403).set_body_string("AccessDeniedException"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/codewhisperer/generateAssistantResponse"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let client = test_client(&server);
        let account = account(AuthMethod::Idc);

        client
            .generate(&account, &payload(), None)
            .await
            .expect("first fallback succeeds");
        client
            .generate(&account, &payload(), None)
            .await
            .expect("cached fallback succeeds");

        let paths = server
            .received_requests()
            .await
            .expect("requests")
            .into_iter()
            .map(|request| request.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                "/amazon/generateAssistantResponse",
                "/codewhisperer/generateAssistantResponse",
                "/codewhisperer/generateAssistantResponse"
            ]
        );
    }

    #[tokio::test]
    async fn concurrent_model_discovery_is_collapsed_and_401_uses_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/amazon/ListAvailableModels"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_delay(Duration::from_millis(25))
                    .set_body_string("Auth error"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/codewhisperer/ListAvailableModels"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"modelId": "claude-sonnet-4"}]
            })))
            .mount(&server)
            .await;
        let client = test_client(&server);
        let account = account(AuthMethod::Idc);

        let (first, second) =
            tokio::join!(client.list_models(&account), client.list_models(&account));

        assert_eq!(first.expect("first fetch")[0].model_id, "claude-sonnet-4");
        assert_eq!(second.expect("shared fetch")[0].model_id, "claude-sonnet-4");
        let paths = server
            .received_requests()
            .await
            .expect("requests")
            .into_iter()
            .map(|request| request.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                "/amazon/ListAvailableModels",
                "/codewhisperer/ListAvailableModels"
            ]
        );
    }

    #[tokio::test]
    async fn model_discovery_does_not_switch_endpoints_for_server_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/amazon/ListAvailableModels"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = test_client(&server);
        let account = account(AuthMethod::Idc);

        let error = client
            .list_models(&account)
            .await
            .expect_err("server error fails the request");

        assert_eq!(error.status, Some(500));
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/amazon/ListAvailableModels");
    }

    #[tokio::test]
    async fn subscriptions_use_only_the_auth_inferred_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/amazon/listAvailableSubscriptions"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/codewhisperer/listAvailableSubscriptions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let client = test_client(&server);
        let account = account(AuthMethod::Idc);

        let subscriptions = client
            .list_subscriptions(&account)
            .await
            .expect("subscription failures degrade to an empty response");

        assert!(subscriptions.subscription_plans.is_empty());
        assert!(subscriptions.disclaimer.is_empty());
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/amazon/listAvailableSubscriptions");
    }

    #[test]
    fn usage_limits_normalize_base_trial_bonus_and_subscription() {
        let limits: UsageLimits = serde_json::from_value(serde_json::json!({
            "usageBreakdownList":[{
                "resourceType":"CREDIT",
                "currentUsageWithPrecision":12.5,
                "usageLimitWithPrecision":100.0,
                "freeTrialInfo":{
                    "freeTrialStatus":"ACTIVE",
                    "currentUsage":2.0,
                    "usageLimit":10.0
                },
                "bonuses":[{"currentUsage":1.0,"usageLimit":5.0,"status":"ACTIVE"}]
            }],
            "nextDateReset":2_000_000_000i64,
            "daysUntilReset":7,
            "subscriptionInfo":{
                "subscriptionTitle":"Kiro Pro+",
                "type":"Q_DEVELOPER_STANDALONE_PRO_PLUS"
            }
        }))
        .expect("usage response");
        let usage = limits.normalized_usage(123).expect("credit usage");
        assert_eq!(usage.current, 15.5);
        assert_eq!(usage.limit, 115.0);
        assert_eq!(usage.updated_at, 123);
        let subscription = limits.normalized_subscription().expect("subscription");
        assert_eq!(subscription.kind, SubscriptionKind::ProPlus);
        assert_eq!(subscription.days_remaining, Some(7));
        assert_eq!(subscription.expires_at, Some(2_000_000_000));
    }

    #[test]
    fn framed_error_text_is_classified_like_http_statuses() {
        let auth = KiroError {
            status: None,
            endpoint: "event-stream".into(),
            message: "AccessDeniedException: expired token".into(),
        };
        assert!(auth.is_auth());
        let throttle = KiroError {
            status: None,
            endpoint: "event-stream".into(),
            message: "ThrottlingException: rate exceeded".into(),
        };
        assert!(throttle.is_throttle());
        assert!(throttle.is_retriable());
        let quota = KiroError {
            status: None,
            endpoint: "event-stream".into(),
            message: "credits exhausted".into(),
        };
        assert!(quota.is_quota());
    }
}
