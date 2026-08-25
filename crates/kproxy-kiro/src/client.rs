//! Upstream HTTP client with independent short-request and streaming pools.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use futures::future::{BoxFuture, FutureExt, Shared};
use futures::{Stream, StreamExt};
use kproxy_core::account::{Account, AuthMethod, Subscription, SubscriptionKind, Usage};
use kproxy_core::config::{AgentMode, UpstreamConfig};
use kproxy_translate::{KiroPayload, WebSearchResults};
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

const KIRO_BUILDER_ID_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";
const KIRO_SOCIAL_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK";

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

    pub fn is_request_rejection(&self) -> bool {
        matches!(self.status, Some(400 | 413 | 422)) || text_is_request_rejection(&self.message)
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

pub fn text_is_request_rejection(message: &str) -> bool {
    let text = message.to_ascii_lowercase();
    [
        "too many tools",
        "tool definitions are too large",
        "tool schema",
        "payload too large",
        "request too large",
        "request entity too large",
        "context length",
        "prompt is too long",
        "input is too long",
        "validationexception",
    ]
    .iter()
    .any(|marker| text.contains(marker))
        || contains_status(&text, 413)
        || contains_status(&text, 422)
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
    stream_slot_wait_ms: u64,
}

impl KiroResponse {
    /// Time spent waiting for an upstream streaming connection slot.
    pub fn stream_slot_wait_ms(&self) -> u64 {
        self.stream_slot_wait_ms
    }

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
    #[serde(default)]
    pub user_info: Option<UsageUserInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageUserInfo {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub user_id: String,
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
    mcp: Client,
    stream: Client,
    cache: Arc<EndpointCache>,
    overrides: EndpointOverrides,
    upstream: UpstreamConfig,
    short_slots: Arc<tokio::sync::Semaphore>,
    stream_slots: Arc<tokio::sync::Semaphore>,
    model_fetches: Arc<DashMap<(String, String), InFlightModelFetch>>,
    profile_fetches: Arc<DashMap<(String, String), InFlightProfileFetch>>,
}

type SharedModelFetch = Shared<BoxFuture<'static, Result<Vec<ModelInfo>, KiroError>>>;

struct InFlightModelFetch {
    id: Uuid,
    future: SharedModelFetch,
}

type SharedProfileFetch = Shared<BoxFuture<'static, Result<String, KiroError>>>;

struct InFlightProfileFetch {
    id: Uuid,
    future: SharedProfileFetch,
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
        let mcp = Client::builder()
            .pool_max_idle_per_host(upstream.pool.http_max_connections)
            .pool_idle_timeout(idle)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_millis(
                upstream.web_search_timeout_ms.clamp(1_000, 300_000),
            ))
            .build()
            .map_err(build_error)?;
        // There is deliberately no total timeout: legitimate generations may be long.
        // A finite per-read timeout still prevents a silent peer from pinning a slot forever.
        // hyper does not pipeline HTTP/1.1 requests, avoiding long-stream HOL blocking.
        let stream = Client::builder()
            .pool_max_idle_per_host(upstream.pool.stream_max_connections)
            .pool_idle_timeout(idle)
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_millis(upstream.stream_read_timeout_ms))
            .build()
            .map_err(build_error)?;
        Ok(Self {
            short,
            mcp,
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
            profile_fetches: Arc::new(DashMap::new()),
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

    /// Resolves the profile ARN required by Kiro MCP requests. Existing ARN
    /// values and the fixed Social profile do not require a network request;
    /// missing IdC values are discovered once per access token.
    pub async fn resolve_profile_arn(&self, account: &Account) -> Result<String, KiroError> {
        if let Some(profile_arn) = account
            .profile_arn
            .as_deref()
            .map(str::trim)
            .filter(|profile_arn| !profile_arn.is_empty())
        {
            return Ok(profile_arn.to_owned());
        }
        if account.credentials.auth_method == kproxy_core::account::AuthMethod::Social {
            return Ok(KIRO_SOCIAL_PROFILE_ARN.into());
        }

        let fetch_key = (account.id.clone(), account.credentials.access_token.clone());
        let fetch_id = Uuid::new_v4();
        let fetch = match self.profile_fetches.entry(fetch_key.clone()) {
            Entry::Occupied(entry) => entry.get().future.clone(),
            Entry::Vacant(entry) => {
                let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
                let future = async move {
                    result_receiver.await.unwrap_or_else(|_| {
                        Err(KiroError {
                            status: None,
                            endpoint: "ListAvailableProfiles".into(),
                            message: "profile ARN discovery task stopped unexpectedly".into(),
                        })
                    })
                }
                .boxed()
                .shared();
                entry.insert(InFlightProfileFetch {
                    id: fetch_id,
                    future: future.clone(),
                });

                let client = self.clone();
                let account = account.clone();
                let fetches = Arc::clone(&self.profile_fetches);
                tokio::spawn(async move {
                    let result = client.fetch_profile_arn_once(&account).await;
                    let _sent = result_sender.send(result);
                    fetches.remove_if(&fetch_key, |_, entry| entry.id == fetch_id);
                });
                future
            }
        };
        fetch.await
    }

    async fn fetch_profile_arn_once(&self, account: &Account) -> Result<String, KiroError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Profiles {
            #[serde(default)]
            profiles: Vec<Profile>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Profile {
            #[serde(default)]
            arn: Option<String>,
        }

        let _permit = Arc::clone(&self.short_slots)
            .acquire_owned()
            .await
            .map_err(build_error)?;
        let urls = self.profile_discovery_urls(account)?;
        let mut fallback_allowed = false;
        let mut last_error = None;
        for url in urls {
            let response = self
                .short
                .post(url.clone())
                .headers(profile_headers(account)?)
                .json(&serde_json::json!({}))
                .send()
                .await
                .map_err(|error| KiroError {
                    status: None,
                    endpoint: "ListAvailableProfiles".into(),
                    message: error.to_string(),
                })?;
            let status = response.status();
            let body =
                bounded_response_text(response, 1024 * 1024, "ListAvailableProfiles").await?;
            if status.is_success() {
                let profiles =
                    serde_json::from_str::<Profiles>(&body).map_err(|error| KiroError {
                        status: Some(502),
                        endpoint: "ListAvailableProfiles".into(),
                        message: format!("invalid profile discovery response: {error}"),
                    })?;
                if let Some(profile_arn) = profiles
                    .profiles
                    .into_iter()
                    .filter_map(|profile| profile.arn)
                    .map(|profile_arn| profile_arn.trim().to_owned())
                    .find(|profile_arn| {
                        profile_arn.starts_with("arn:") && profile_arn.len() <= 2_048
                    })
                {
                    debug!(
                        account = %account.id,
                        endpoint = %url,
                        "Kiro profile ARN discovered"
                    );
                    return Ok(profile_arn);
                }
                fallback_allowed = true;
                continue;
            }

            let error = KiroError {
                status: Some(status.as_u16()),
                endpoint: "ListAvailableProfiles".into(),
                message: nonempty_error_body(status.as_u16(), &body),
            };
            if status.as_u16() == 403 && !text_is_auth_error(&error.message) {
                // Builder ID accounts cannot list Enterprise profiles. Kiro's
                // own clients use the fixed Builder ID profile in this case.
                fallback_allowed = true;
                last_error = Some(error);
                continue;
            }
            return Err(error);
        }

        if fallback_allowed {
            debug!(account = %account.id, "using Kiro Builder ID profile ARN");
            return Ok(KIRO_BUILDER_ID_PROFILE_ARN.into());
        }
        Err(last_error.unwrap_or_else(|| KiroError {
            status: Some(502),
            endpoint: "ListAvailableProfiles".into(),
            message: "profile discovery returned no usable profile ARN".into(),
        }))
    }

    fn profile_discovery_urls(&self, account: &Account) -> Result<Vec<url::Url>, KiroError> {
        if let Some(endpoint) = self.overrides.amazonq_url.as_deref() {
            return Ok(vec![operation_url(endpoint, "ListAvailableProfiles")?]);
        }

        let region = account.credentials.region.trim();
        if region.is_empty()
            || !region
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        {
            return Err(KiroError {
                status: None,
                endpoint: "ListAvailableProfiles".into(),
                message: "account has an invalid API region".into(),
            });
        }
        let regions = if region.starts_with("eu-") {
            ["eu-central-1", "us-east-1"]
        } else {
            ["us-east-1", "eu-central-1"]
        };
        regions
            .into_iter()
            .map(|region| {
                operation_url(
                    &format!("https://q.{region}.amazonaws.com"),
                    "ListAvailableProfiles",
                )
            })
            .collect()
    }

    /// Executes Kiro's JSON-RPC MCP web search. The nested MCP text block is
    /// JSON itself, so both envelope layers are validated before results are
    /// exposed to the response/continuation pipeline.
    pub async fn web_search(
        &self,
        account: &Account,
        query: &str,
    ) -> Result<WebSearchResults, KiroError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(KiroError {
                status: Some(400),
                endpoint: "MCP web_search".into(),
                message: "web search query must not be empty".into(),
            });
        }
        if query.chars().count() > 2_000 {
            return Err(KiroError {
                status: Some(400),
                endpoint: "MCP web_search".into(),
                message: "web search query exceeds 2000 characters".into(),
            });
        }
        let _permit = Arc::clone(&self.short_slots)
            .acquire_owned()
            .await
            .map_err(build_error)?;
        let url = self.web_search_url(account)?;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let request = serde_json::json!({
            "id":format!("web_search_tooluse_{}_{}", Uuid::new_v4().simple(), timestamp),
            "jsonrpc":"2.0",
            "method":"tools/call",
            "params":{"name":"web_search","arguments":{"query":query}}
        });
        let response = self
            .mcp
            .post(url.clone())
            .headers(mcp_headers(account)?)
            .json(&request)
            .send()
            .await
            .map_err(|error| KiroError {
                status: None,
                endpoint: "MCP web_search".into(),
                message: error.to_string(),
            })?;
        let status = response.status();
        let body = bounded_response_text(response, 8 * 1024 * 1024, "MCP web_search").await?;
        if !status.is_success() {
            return Err(KiroError {
                status: Some(status.as_u16()),
                endpoint: "MCP web_search".into(),
                message: nonempty_error_body(status.as_u16(), &body),
            });
        }
        let envelope: serde_json::Value =
            serde_json::from_str(&body).map_err(|error| KiroError {
                status: Some(502),
                endpoint: "MCP web_search".into(),
                message: format!("invalid JSON-RPC response: {error}"),
            })?;
        if let Some(error) = envelope.get("error").filter(|error| !error.is_null()) {
            return Err(KiroError {
                status: Some(502),
                endpoint: "MCP web_search".into(),
                message: format!(
                    "JSON-RPC error: {}",
                    truncate_chars(&error.to_string(), 1_000)
                ),
            });
        }
        if envelope
            .pointer("/result/isError")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            return Err(KiroError {
                status: Some(502),
                endpoint: "MCP web_search".into(),
                message: "Kiro MCP web_search returned an error result".into(),
            });
        }
        let content = envelope
            .pointer("/result/content")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| KiroError {
                status: Some(502),
                endpoint: "MCP web_search".into(),
                message: "MCP response is missing result.content".into(),
            })?;
        let mut parsed = None;
        for block in content {
            if block.get("type").and_then(serde_json::Value::as_str) != Some("text") {
                continue;
            }
            let Some(text) = block.get("text").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if let Ok(results) = serde_json::from_str::<WebSearchResults>(text) {
                parsed = Some(results);
                break;
            }
        }
        let mut results = parsed.ok_or_else(|| KiroError {
            status: Some(502),
            endpoint: "MCP web_search".into(),
            message: "MCP response contains no valid web search JSON text block".into(),
        })?;
        if results.query.is_empty() {
            results.query = query.to_owned();
        }
        sanitize_web_search_results(&mut results);
        debug!(
            account = %account.id,
            endpoint = %url,
            query_chars = query.chars().count(),
            result_count = results.results.len(),
            total_results = results.total_results,
            "Kiro MCP web search completed"
        );
        Ok(results)
    }

    fn web_search_url(&self, account: &Account) -> Result<url::Url, KiroError> {
        let region = account.credentials.region.trim();
        if region.is_empty()
            || !region
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        {
            return Err(KiroError {
                status: None,
                endpoint: "MCP web_search".into(),
                message: "account has an invalid API region".into(),
            });
        }
        let template = self
            .overrides
            .mcp_url
            .as_deref()
            .or(self.upstream.web_search_endpoint.as_deref())
            .unwrap_or("https://runtime.{region}.kiro.dev/mcp");
        let url = template.replace("{region}", region);
        let parsed = url::Url::parse(&url).map_err(build_error)?;
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(KiroError {
                status: None,
                endpoint: "MCP web_search".into(),
                message: "web search endpoint must be an HTTP(S) URL without credentials".into(),
            });
        }
        Ok(parsed)
    }

    async fn send_generation(
        &self,
        account: &Account,
        payload: &KiroPayload,
        endpoint: EndpointDefinition,
    ) -> Result<KiroResponse, KiroError> {
        let slot_wait_started = Instant::now();
        let permit = tokio::time::timeout(
            Duration::from_millis(self.upstream.stream_slot_wait_timeout_ms),
            Arc::clone(&self.stream_slots).acquire_owned(),
        )
        .await
        .map_err(|_| KiroError {
            status: None,
            endpoint: endpoint.name.into(),
            message: format!(
                "timed out after {} ms waiting for an upstream stream slot",
                self.upstream.stream_slot_wait_timeout_ms
            ),
        })?
        .map_err(build_error)?;
        let stream_slot_wait_ms = slot_wait_started.elapsed().as_millis() as u64;
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
                stream_slot_wait_ms,
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
        let urls = self.usage_limits_urls(account)?;
        self.get_usage_limits_from_urls(account, urls).await
    }

    async fn get_usage_limits_from_urls(
        &self,
        account: &Account,
        urls: Vec<url::Url>,
    ) -> Result<UsageLimits, KiroError> {
        let attempt_count = urls.len();
        let mut last_error = None;
        for (attempt, url) in urls.into_iter().enumerate() {
            let endpoint = usage_endpoint_label(&url);
            let response = self
                .short
                .get(url)
                .header("accept", "application/json")
                .header(
                    "authorization",
                    format!("Bearer {}", account.credentials.access_token),
                )
                .header("user-agent", kiro_user_agent(&account.machine_id))
                .header("x-amz-user-agent", kiro_amz_user_agent(&account.machine_id))
                .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
                .header("amz-sdk-request", "attempt=1; max=1")
                .send()
                .await
                .map_err(|error| KiroError {
                    status: None,
                    endpoint: endpoint.clone(),
                    message: error.to_string(),
                })?;
            let status = response.status();
            let body = bounded_response_text(response, 1024 * 1024, &endpoint).await?;
            if status.is_success() {
                return serde_json::from_str::<UsageLimits>(&body).map_err(|error| KiroError {
                    status: Some(502),
                    endpoint,
                    message: format!("invalid usage limits response: {error}"),
                });
            }
            let error = KiroError {
                status: Some(status.as_u16()),
                endpoint: endpoint.clone(),
                message: nonempty_error_body(status.as_u16(), &body),
            };
            if status.as_u16() == 403 && attempt + 1 < attempt_count {
                debug!(
                    account = %account.id,
                    endpoint = %endpoint,
                    "Kiro usage endpoint rejected the account; trying the regional fallback"
                );
                last_error = Some(error);
                continue;
            }
            return Err(error);
        }
        Err(last_error.unwrap_or_else(|| KiroError {
            status: None,
            endpoint: "getUsageLimits".into(),
            message: "no usable Kiro usage endpoint".into(),
        }))
    }

    fn usage_limits_urls(&self, account: &Account) -> Result<Vec<url::Url>, KiroError> {
        if let Some(endpoint) = self.overrides.amazonq_url.as_deref() {
            return Ok(vec![usage_limits_url(endpoint, account)?]);
        }
        usage_api_regions(account)
            .into_iter()
            .map(|region| usage_limits_url(&format!("https://q.{region}.amazonaws.com"), account))
            .collect()
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
        kiro_user_agent(&account.machine_id)
    };
    let amz_user_agent = if cli {
        "aws-sdk-rust/1.3.9 ua/2.1 api/ssooidc/1.88.0 os/macos lang/rust/1.87.0 m/E app/AmazonQ-For-CLI".to_string()
    } else {
        kiro_amz_user_agent(&account.machine_id)
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

fn mcp_headers(account: &Account) -> Result<reqwest::header::HeaderMap, KiroError> {
    use reqwest::header::{
        HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT,
    };

    let mut headers = HeaderMap::new();
    let authorization = format!("Bearer {}", account.credentials.access_token);
    let user_agent = kiro_user_agent(&account.machine_id);
    for (name, value) in [
        (CONTENT_TYPE, "application/json"),
        (ACCEPT, "application/json"),
        (USER_AGENT, user_agent.as_str()),
        (AUTHORIZATION, authorization.as_str()),
    ] {
        headers.insert(name, HeaderValue::from_str(value).map_err(build_error)?);
    }
    headers.insert(
        reqwest::header::HeaderName::from_static("x-amzn-codewhisperer-optout"),
        HeaderValue::from_static("false"),
    );
    let profile_arn = account
        .profile_arn
        .as_deref()
        .map(str::trim)
        .filter(|profile_arn| !profile_arn.is_empty())
        .ok_or_else(|| KiroError {
            status: None,
            endpoint: "MCP web_search".into(),
            message: "account profile ARN was not resolved before web search".into(),
        })?;
    headers.insert(
        reqwest::header::HeaderName::from_static("x-amzn-kiro-profile-arn"),
        HeaderValue::from_str(profile_arn).map_err(build_error)?,
    );
    headers.insert(
        reqwest::header::HeaderName::from_static("x-amz-user-agent"),
        HeaderValue::from_str(&kiro_amz_user_agent(&account.machine_id)).map_err(build_error)?,
    );
    headers.insert(
        reqwest::header::HeaderName::from_static("amz-sdk-invocation-id"),
        HeaderValue::from_str(&Uuid::new_v4().to_string()).map_err(build_error)?,
    );
    headers.insert(
        reqwest::header::HeaderName::from_static("amz-sdk-request"),
        HeaderValue::from_static("attempt=1; max=3"),
    );
    Ok(headers)
}

fn profile_headers(account: &Account) -> Result<reqwest::header::HeaderMap, KiroError> {
    use reqwest::header::{
        HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT,
    };

    let mut headers = HeaderMap::new();
    let authorization = format!("Bearer {}", account.credentials.access_token);
    let user_agent = kiro_user_agent(&account.machine_id);
    let amz_user_agent = kiro_amz_user_agent(&account.machine_id);
    for (name, value) in [
        (CONTENT_TYPE, "application/json"),
        (ACCEPT, "application/json"),
        (USER_AGENT, user_agent.as_str()),
        (AUTHORIZATION, authorization.as_str()),
    ] {
        headers.insert(name, HeaderValue::from_str(value).map_err(build_error)?);
    }
    for (name, value) in [
        ("x-amz-user-agent", amz_user_agent),
        ("amz-sdk-invocation-id", Uuid::new_v4().to_string()),
        ("amz-sdk-request", "attempt=1; max=1".into()),
    ] {
        headers.insert(
            reqwest::header::HeaderName::from_static(name),
            HeaderValue::from_str(&value).map_err(build_error)?,
        );
    }
    Ok(headers)
}

fn kiro_user_agent(machine_id: &str) -> String {
    format!(
        "aws-sdk-js/1.0.27 ua/2.1 os/win32#10.0.19044 lang/js md/nodejs#22.21.1 api/codewhispererstreaming#1.0.27 m/E KiroIDE-0.7.45-{machine_id}"
    )
}

fn kiro_amz_user_agent(machine_id: &str) -> String {
    format!("aws-sdk-js/1.0.27 KiroIDE-0.7.45-{machine_id}")
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
    let text = if text.trim().is_empty() {
        format!(
            "upstream returned HTTP {} without an error message",
            status.as_u16()
        )
    } else {
        text
    };
    KiroError {
        status: Some(status.as_u16()),
        endpoint: endpoint.name.into(),
        message: text.chars().take(2_000).collect(),
    }
}

fn nonempty_error_body(status: u16, body: &str) -> String {
    if body.trim().is_empty() {
        format!("upstream returned HTTP {status} without an error message")
    } else {
        truncate_chars(body, 2_000)
    }
}

fn operation_url(endpoint: &str, operation: &str) -> Result<url::Url, KiroError> {
    let mut url = url::Url::parse(endpoint).map_err(build_error)?;
    let current_path = url.path().trim_end_matches('/');
    let base_path = current_path
        .strip_suffix("/generateAssistantResponse")
        .unwrap_or(current_path);
    let path = if base_path.is_empty() {
        format!("/{operation}")
    } else {
        format!("{base_path}/{operation}")
    };
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn usage_limits_url(endpoint: &str, account: &Account) -> Result<url::Url, KiroError> {
    let mut url = operation_url(endpoint, "getUsageLimits")?;
    let mut query = url.query_pairs_mut();
    query.append_pair("origin", "AI_EDITOR");
    query.append_pair("resourceType", "AGENTIC_REQUEST");
    query.append_pair("isEmailRequired", "true");
    if let Some(profile_arn) = usage_profile_arn(account) {
        query.append_pair("profileArn", profile_arn);
    }
    drop(query);
    Ok(url)
}

fn usage_profile_arn(account: &Account) -> Option<&str> {
    account
        .profile_arn
        .as_deref()
        .map(str::trim)
        .filter(|profile_arn| !profile_arn.is_empty())
        .filter(|profile_arn| {
            account.credentials.auth_method != AuthMethod::Idc
                || *profile_arn != KIRO_BUILDER_ID_PROFILE_ARN
        })
}

fn usage_api_regions(account: &Account) -> Vec<String> {
    let profile_region = account.profile_arn.as_deref().and_then(profile_arn_region);
    let primary = profile_region
        .map(str::to_owned)
        .unwrap_or_else(|| canonical_api_region(&account.credentials.region).to_owned());
    let fallback = if primary == "us-east-1" {
        "eu-central-1"
    } else {
        "us-east-1"
    };
    let mut regions = vec![primary];
    if regions[0] != fallback {
        regions.push(fallback.to_owned());
    }
    regions
}

fn canonical_api_region(region: &str) -> &'static str {
    if region.trim().starts_with("eu-") {
        "eu-central-1"
    } else {
        "us-east-1"
    }
}

fn profile_arn_region(profile_arn: &str) -> Option<&str> {
    let mut parts = profile_arn.split(':');
    let valid_prefix = parts.next() == Some("arn")
        && parts
            .next()
            .is_some_and(|partition| partition.starts_with("aws"))
        && parts.next() == Some("codewhisperer");
    let region = parts.next()?.trim();
    (valid_prefix
        && !region.is_empty()
        && region
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-'))
    .then_some(region)
}

fn usage_endpoint_label(url: &url::Url) -> String {
    match url.host_str() {
        Some(host) => format!("getUsageLimits ({host})"),
        None => "getUsageLimits".into(),
    }
}

fn truncate_chars(value: &str, maximum: usize) -> String {
    value.chars().take(maximum).collect()
}

async fn bounded_response_text(
    response: Response,
    maximum: usize,
    endpoint: &str,
) -> Result<String, KiroError> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(KiroError {
            status: Some(502),
            endpoint: endpoint.to_owned(),
            message: format!("response exceeds the {maximum} byte safety limit"),
        });
    }
    let mut source = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = source.next().await {
        let chunk = chunk.map_err(|error| KiroError {
            status: None,
            endpoint: endpoint.to_owned(),
            message: error.to_string(),
        })?;
        if body.len().saturating_add(chunk.len()) > maximum {
            return Err(KiroError {
                status: Some(502),
                endpoint: endpoint.to_owned(),
                message: format!("response exceeds the {maximum} byte safety limit"),
            });
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|error| KiroError {
        status: Some(502),
        endpoint: endpoint.to_owned(),
        message: format!("response is not valid UTF-8: {error}"),
    })
}

fn sanitize_web_search_results(results: &mut WebSearchResults) {
    const MAX_RESULTS: usize = 50;
    results.query = truncate_chars(&results.query, 2_000);
    results.results.retain(|result| {
        result.url.chars().count() <= 8_192
            && url::Url::parse(&result.url)
                .ok()
                .is_some_and(|url| matches!(url.scheme(), "http" | "https"))
    });
    results.results.truncate(MAX_RESULTS);
    for result in &mut results.results {
        if result.title.trim().is_empty() {
            result.title = url::Url::parse(&result.url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_else(|| result.url.clone());
        }
        result.title = truncate_chars(&result.title, 1_000);
        result.snippet = truncate_chars(&result.snippet, 32_000);
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
    use kproxy_core::account::{AuthMethod, Credentials};
    use kproxy_translate::{KiroConversationState, KiroCurrentMessage, KiroUserInputMessage};
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
            upstream_user_id: None,
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

    #[test]
    fn kiro_ide_user_agents_match_the_current_desktop_client() {
        let machine_id = "a".repeat(64);
        assert_eq!(
            kiro_user_agent(&machine_id),
            format!(
                "aws-sdk-js/1.0.27 ua/2.1 os/win32#10.0.19044 lang/js md/nodejs#22.21.1 api/codewhispererstreaming#1.0.27 m/E KiroIDE-0.7.45-{machine_id}"
            )
        );
        assert_eq!(
            kiro_amz_user_agent(&machine_id),
            format!("aws-sdk-js/1.0.27 KiroIDE-0.7.45-{machine_id}")
        );
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
                mcp_url: Some(format!("{}/mcp", server.uri())),
            },
        )
        .expect("client")
    }

    fn generation_endpoint(server: &MockServer) -> EndpointDefinition {
        EndpointDefinition {
            key: EndpointKey::Amazonq,
            url: format!("{}/amazon/generateAssistantResponse", server.uri()),
            origin: "CLI",
            amz_target: "AmazonQDeveloperStreamingService.SendMessage",
            name: "AmazonQ",
        }
    }

    #[tokio::test]
    async fn generation_times_out_when_waiting_for_a_stream_slot() {
        let server = MockServer::start().await;
        let mut upstream = UpstreamConfig::default();
        upstream.pool.stream_max_connections = 1;
        upstream.stream_slot_wait_timeout_ms = 20;
        let client = KiroClient::new(upstream, EndpointOverrides::default()).expect("client");
        let _held = client.stream_slots.acquire().await.expect("held slot");

        let error = match client
            .send_generation(
                &account(AuthMethod::Idc),
                &payload(),
                generation_endpoint(&server),
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("slot wait must time out"),
        };

        assert!(error
            .message
            .contains("waiting for an upstream stream slot"));
    }

    #[tokio::test]
    async fn generation_times_out_when_upstream_sends_no_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/amazon/generateAssistantResponse"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(500)))
            .mount(&server)
            .await;
        let upstream = UpstreamConfig {
            stream_read_timeout_ms: 20,
            ..UpstreamConfig::default()
        };
        let client = KiroClient::new(upstream, EndpointOverrides::default()).expect("client");

        let started = Instant::now();
        let error = match client
            .send_generation(
                &account(AuthMethod::Idc),
                &payload(),
                generation_endpoint(&server),
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("silent upstream must time out"),
        };

        assert!(error.status.is_none());
        assert!(started.elapsed() < Duration::from_millis(300));
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

    #[tokio::test]
    async fn web_search_calls_mcp_and_decodes_nested_json_text() {
        let server = MockServer::start().await;
        let nested = serde_json::json!({
            "query":"rust async",
            "totalResults":3,
            "results":[
                {"title":"Tokio","url":"https://tokio.rs/","snippet":"runtime","publishedDate":123},
                {"title":"","url":"https://example.org/page","snippet":"title fallback"},
                {"title":"unsafe","url":"javascript:alert(1)","snippet":"drop me"}
            ]
        })
        .to_string();
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc":"2.0",
                "id":"response",
                "result":{"content":[{"type":"text","text":nested}],"isError":false}
            })))
            .mount(&server)
            .await;
        let client = test_client(&server);
        let mut account = account(AuthMethod::Idc);
        account.profile_arn =
            Some("arn:aws:codewhisperer:us-east-1:123456789012:profile/test-profile".into());
        let results = client
            .web_search(&account, "rust async")
            .await
            .expect("search");

        assert_eq!(results.total_results, 3);
        assert_eq!(results.results.len(), 2);
        assert_eq!(results.results[0].title, "Tokio");
        assert_eq!(results.results[1].title, "example.org");
        let requests = server.received_requests().await.expect("requests");
        let request = requests.last().expect("MCP request");
        assert_eq!(request.url.path(), "/mcp");
        assert_eq!(
            request
                .headers
                .get("x-amzn-codewhisperer-optout")
                .and_then(|value| value.to_str().ok()),
            Some("false")
        );
        assert_eq!(
            request
                .headers
                .get("x-amzn-kiro-profile-arn")
                .and_then(|value| value.to_str().ok()),
            account.profile_arn.as_deref()
        );
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("JSON body");
        assert_eq!(body["method"], "tools/call");
        assert_eq!(body["params"]["name"], "web_search");
        assert_eq!(body["params"]["arguments"]["query"], "rust async");
    }

    #[tokio::test]
    async fn web_search_surfaces_json_rpc_errors_without_generation_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc":"2.0",
                "id":"response",
                "error":{"code":-32000,"message":"unavailable"}
            })))
            .mount(&server)
            .await;
        let client = test_client(&server);
        let mut account = account(AuthMethod::Idc);
        account.profile_arn =
            Some("arn:aws:codewhisperer:us-east-1:123456789012:profile/test-profile".into());
        let error = client
            .web_search(&account, "news")
            .await
            .expect_err("JSON-RPC error");

        assert_eq!(error.status, Some(502));
        assert!(error.message.contains("JSON-RPC error"));
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/mcp");
    }

    #[tokio::test]
    async fn missing_idc_profile_is_discovered_once_per_access_token() {
        let server = MockServer::start().await;
        let discovered = "arn:aws:codewhisperer:us-east-1:123456789012:profile/discovered-profile";
        Mock::given(method("POST"))
            .and(path("/amazon/ListAvailableProfiles"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(50))
                    .set_body_json(serde_json::json!({
                        "profiles":[{"arn":discovered}]
                    })),
            )
            .mount(&server)
            .await;
        let client = test_client(&server);
        let account = account(AuthMethod::Idc);

        let (left, right) = tokio::join!(
            client.resolve_profile_arn(&account),
            client.resolve_profile_arn(&account)
        );

        assert_eq!(left.expect("left profile"), discovered);
        assert_eq!(right.expect("right profile"), discovered);
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/amazon/ListAvailableProfiles");
        assert_eq!(
            requests[0]
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer access-token")
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[0].body).expect("JSON body"),
            serde_json::json!({})
        );
    }

    #[tokio::test]
    async fn builder_id_profile_is_used_when_profile_listing_is_forbidden() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/amazon/ListAvailableProfiles"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string("User is not authorized to make this call"),
            )
            .mount(&server)
            .await;
        let client = test_client(&server);

        let profile = client
            .resolve_profile_arn(&account(AuthMethod::Idc))
            .await
            .expect("Builder ID fallback");

        assert_eq!(profile, KIRO_BUILDER_ID_PROFILE_ARN);
        assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    }

    #[tokio::test]
    async fn usage_limits_send_complete_enterprise_query_and_runtime_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/amazon/getUsageLimits"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "userInfo":{"userId":"stable-user","email":"test@example.com"},
                "usageBreakdownList":[]
            })))
            .mount(&server)
            .await;
        let client = test_client(&server);
        let mut account = account(AuthMethod::Idc);
        let profile_arn = "arn:aws:codewhisperer:us-east-1:123456789012:profile/enterprise-profile";
        account.profile_arn = Some(profile_arn.into());

        let limits = client
            .get_usage_limits(&account)
            .await
            .expect("usage limits");

        assert_eq!(
            limits.user_info.as_ref().map(|user| user.user_id.as_str()),
            Some("stable-user")
        );
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        let query = request
            .url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query.get("origin").map(String::as_str), Some("AI_EDITOR"));
        assert_eq!(
            query.get("resourceType").map(String::as_str),
            Some("AGENTIC_REQUEST")
        );
        assert_eq!(
            query.get("isEmailRequired").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            query.get("profileArn").map(String::as_str),
            Some(profile_arn)
        );
        assert!(request.headers.contains_key("amz-sdk-invocation-id"));
        assert_eq!(
            request
                .headers
                .get("amz-sdk-request")
                .and_then(|value| value.to_str().ok()),
            Some("attempt=1; max=1")
        );
    }

    #[tokio::test]
    async fn builder_id_usage_omits_the_synthetic_profile_arn() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/amazon/getUsageLimits"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let client = test_client(&server);
        let mut account = account(AuthMethod::Idc);
        account.profile_arn = Some(KIRO_BUILDER_ID_PROFILE_ARN.into());

        client
            .get_usage_limits(&account)
            .await
            .expect("Builder ID usage limits");

        let requests = server.received_requests().await.expect("requests");
        let query = requests[0]
            .url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<std::collections::HashMap<_, _>>();
        assert!(!query.contains_key("profileArn"));
        assert_eq!(
            query.get("isEmailRequired").map(String::as_str),
            Some("true")
        );
    }

    #[tokio::test]
    async fn usage_limits_retry_the_regional_endpoint_after_a_403() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/primary/getUsageLimits"))
            .respond_with(ResponseTemplate::new(403).set_body_string("AccessDeniedException"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/fallback/getUsageLimits"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "userInfo":{"userId":"stable-user"}
            })))
            .mount(&server)
            .await;
        let client = test_client(&server);
        let account = account(AuthMethod::Idc);
        let urls = ["primary", "fallback"]
            .into_iter()
            .map(|path| {
                usage_limits_url(&format!("{}/{path}", server.uri()), &account).expect("usage URL")
            })
            .collect();

        let limits = client
            .get_usage_limits_from_urls(&account, urls)
            .await
            .expect("regional fallback");

        assert_eq!(
            limits.user_info.as_ref().map(|user| user.user_id.as_str()),
            Some("stable-user")
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
            vec!["/primary/getUsageLimits", "/fallback/getUsageLimits"]
        );
    }

    #[test]
    fn usage_endpoint_regions_follow_profile_then_sso_region() {
        let mut account = account(AuthMethod::Idc);
        account.credentials.region = "eu-west-1".into();
        assert_eq!(
            usage_api_regions(&account),
            vec!["eu-central-1", "us-east-1"]
        );

        account.profile_arn =
            Some("arn:aws:codewhisperer:ap-southeast-1:123456789012:profile/enterprise".into());
        assert_eq!(
            usage_api_regions(&account),
            vec!["ap-southeast-1", "us-east-1"]
        );
    }

    #[tokio::test]
    async fn social_profile_does_not_require_discovery() {
        let server = MockServer::start().await;
        let client = test_client(&server);

        let profile = client
            .resolve_profile_arn(&account(AuthMethod::Social))
            .await
            .expect("Social profile");

        assert_eq!(profile, KIRO_SOCIAL_PROFILE_ARN);
        assert!(server
            .received_requests()
            .await
            .expect("requests")
            .is_empty());
    }

    #[tokio::test]
    async fn web_search_rejects_an_unresolved_profile_before_network_io() {
        let server = MockServer::start().await;
        let client = test_client(&server);

        let error = client
            .web_search(&account(AuthMethod::Idc), "news")
            .await
            .expect_err("unresolved profile");

        assert!(error.message.contains("profile ARN was not resolved"));
        assert!(server
            .received_requests()
            .await
            .expect("requests")
            .is_empty());
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
            },
            "userInfo":{
                "email":"alice@example.com",
                "userId":"user-123"
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
        let identity = limits.user_info.expect("authenticated identity");
        assert_eq!(identity.email, "alice@example.com");
        assert_eq!(identity.user_id, "user-123");
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

        let payload = KiroError {
            status: Some(500),
            endpoint: "AmazonQ".into(),
            message: "ValidationException: tool schema request too large".into(),
        };
        assert!(payload.is_request_rejection());
        assert!(payload.is_retriable());
    }
}
