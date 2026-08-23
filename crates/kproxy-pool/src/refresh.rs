//! IdC refresh-token flow with per-account stampede prevention.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kproxy_core::account::AuthMethod;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{AccountHealth, AccountPool};

#[derive(Debug, Error)]
pub enum RefreshError {
    #[error("account not found")]
    NotFound,
    #[error("account does not contain refreshable credentials")]
    NotRefreshable,
    #[error("token refresh failed: {0}")]
    Upstream(String),
}

#[derive(Clone)]
pub struct TokenRefresher {
    client: Client,
    before_expiry_secs: i64,
    endpoint_override: Option<String>,
}

impl TokenRefresher {
    pub fn new(before_expiry_secs: i64) -> Result<Self, RefreshError> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|error| RefreshError::Upstream(error.to_string()))?,
            before_expiry_secs,
            endpoint_override: None,
        })
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint_override = Some(endpoint.into());
        self
    }

    pub async fn refresh_account(
        &self,
        pool: &AccountPool,
        account_id: &str,
        force: bool,
    ) -> Result<bool, RefreshError> {
        let state = pool.get(account_id).await.ok_or(RefreshError::NotFound)?;
        let _singleflight = state.refresh_lock.lock().await;
        let snapshot = state.account.read().await.clone();
        if !force && !snapshot.is_token_expiring(now_secs(), self.before_expiry_secs) {
            return Ok(false);
        }
        let refresh_token = snapshot
            .credentials
            .refresh_token
            .as_deref()
            .ok_or(RefreshError::NotRefreshable)?;
        if snapshot.credentials.auth_method == AuthMethod::Idc
            && (snapshot.credentials.client_id.is_none()
                || snapshot.credentials.client_secret.is_none())
        {
            return Err(RefreshError::NotRefreshable);
        }
        let health_before_refresh = state.health();
        state.set_health(AccountHealth::Refreshing);
        let response = match snapshot.credentials.auth_method {
            AuthMethod::Idc => {
                let request = RefreshRequest {
                    client_id: snapshot
                        .credentials
                        .client_id
                        .as_deref()
                        .ok_or(RefreshError::NotRefreshable)?,
                    client_secret: snapshot
                        .credentials
                        .client_secret
                        .as_deref()
                        .ok_or(RefreshError::NotRefreshable)?,
                    grant_type: "refresh_token",
                    refresh_token,
                };
                let endpoint = self.endpoint_override.clone().unwrap_or_else(|| {
                    format!(
                        "https://oidc.{}.amazonaws.com/token",
                        snapshot.credentials.region
                    )
                });
                self.client.post(endpoint).json(&request).send().await
            }
            AuthMethod::Social => {
                let endpoint = self.endpoint_override.clone().unwrap_or_else(|| {
                    "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken".into()
                });
                self.client
                    .post(endpoint)
                    .header(
                        reqwest::header::USER_AGENT,
                        format!("aws-sdk-js/1.0.18 KiroIDE-0.6.18-{}", snapshot.machine_id),
                    )
                    .json(&SocialRefreshRequest { refresh_token })
                    .send()
                    .await
            }
        };
        let result = match response {
            Ok(response) if response.status().is_success() => response
                .json::<RefreshResponse>()
                .await
                .map_err(|error| RefreshError::Upstream(error.to_string())),
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                Err(RefreshError::Upstream(format!(
                    "HTTP {status}: {}",
                    body.chars().take(500).collect::<String>()
                )))
            }
            Err(error) => Err(RefreshError::Upstream(error.to_string())),
        };
        match result {
            Ok(tokens) => {
                let mut account = state.account.write().await;
                account.credentials.access_token = tokens.access_token;
                if tokens.refresh_token.is_some() {
                    account.credentials.refresh_token = tokens.refresh_token;
                }
                account.credentials.expires_at = now_secs() + tokens.expires_in;
                state.restore_health_after_refresh(health_before_refresh);
                Ok(true)
            }
            Err(error) => {
                state.set_health(AccountHealth::Banned);
                Err(error)
            }
        }
    }

    pub async fn refresh_expiring(&self, pool: &AccountPool) -> Vec<(String, RefreshError)> {
        let accounts = pool.snapshot().await;
        let mut failures = Vec::new();
        for account in accounts {
            if account.is_token_expiring(now_secs(), self.before_expiry_secs) {
                if let Err(error) = self.refresh_account(pool, &account.id, false).await {
                    failures.push((account.id, error));
                }
            }
        }
        failures
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RefreshRequest<'a> {
    client_id: &'a str,
    client_secret: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SocialRefreshRequest<'a> {
    refresh_token: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use kproxy_core::account::{Account, Credentials};
    use kproxy_core::config::PoolConfig;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn account() -> Account {
        Account {
            id: "acc_refresh".into(),
            email: "refresh@example.com".into(),
            label: None,
            enabled: true,
            machine_id: "a".repeat(64),
            profile_arn: None,
            upstream_user_id: None,
            credentials: Credentials {
                access_token: "old-access-token".into(),
                refresh_token: Some("refresh-token".into()),
                client_id: None,
                client_secret: None,
                region: "us-east-1".into(),
                expires_at: 0,
                auth_method: AuthMethod::Social,
            },
            usage: None,
            subscription: None,
            tags: Vec::new(),
            created_at: 0,
            credit_exhausted: false,
        }
    }

    #[tokio::test]
    async fn successful_refresh_preserves_cooling_and_exhausted_health() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"accessToken":"new-access-token","refreshToken":"new-refresh-token","expiresIn":3600}"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let config = PoolConfig {
            cooldown: kproxy_core::config::CooldownConfig {
                quota_error_threshold: 1,
                ..kproxy_core::config::CooldownConfig::default()
            },
            ..PoolConfig::default()
        };
        let pool = AccountPool::new(vec![account()], config);
        let state = pool.get("acc_refresh").await.expect("account state");
        let refresher = TokenRefresher::new(300)
            .expect("refresher")
            .with_endpoint(format!("{}/refresh", server.uri()));

        pool.record_error("acc_refresh").await;
        assert_eq!(state.health(), AccountHealth::Cooling);
        assert!(refresher.refresh_expiring(&pool).await.is_empty());
        assert_eq!(state.health(), AccountHealth::Cooling);
        assert_eq!(
            state
                .consecutive_errors
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert!(state.cooling_until.lock().await.is_some());

        state.reset_health().await;
        pool.record_quota_error("acc_refresh").await;
        assert_eq!(state.health(), AccountHealth::Exhausted);
        assert!(refresher
            .refresh_account(&pool, "acc_refresh", true)
            .await
            .expect("exhausted refresh"));
        assert_eq!(state.health(), AccountHealth::Exhausted);
        assert!(state.account.read().await.credit_exhausted);
        assert_eq!(state.quota_errors.lock().await.len(), 1);

        state.reset_health().await;
        pool.mark_banned("acc_refresh").await;
        assert!(refresher
            .refresh_account(&pool, "acc_refresh", true)
            .await
            .expect("banned refresh"));
        assert_eq!(state.health(), AccountHealth::Available);
        assert_eq!(
            state.account.read().await.credentials.access_token,
            "new-access-token"
        );
    }
}
