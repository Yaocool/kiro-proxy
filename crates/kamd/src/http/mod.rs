//! Public business HTTP plane.

mod handlers;
pub(crate) use handlers::fallback_models;
pub(crate) mod prompt_cache;
mod response;
pub(crate) mod stream;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::FutureExt;
use tracing::Instrument;
use uuid::Uuid;

use crate::state::AppState;

pub(crate) const TRACE_ID_HEADER: &str = "x-trace-id";

#[derive(Clone)]
pub(crate) struct RequestTrace {
    pub id: String,
}

pub(crate) fn request_trace_id(request: &axum::extract::Request) -> String {
    request
        .extensions()
        .get::<RequestTrace>()
        .map(|trace| trace.id.clone())
        .unwrap_or_else(|| format!("trace_{}", Uuid::new_v4().simple()))
}

pub fn router(state: Arc<AppState>) -> Router {
    let middleware_state = Arc::clone(&state);
    Router::new()
        .route("/", get(handlers::root))
        .route("/health", get(handlers::health))
        .route(
            "/v1/messages",
            post(handlers::claude_messages).fallback(handlers::claude_method_not_allowed),
        )
        .route(
            "/messages",
            post(handlers::claude_messages).fallback(handlers::claude_method_not_allowed),
        )
        .route(
            "/anthropic/v1/messages",
            post(handlers::claude_messages).fallback(handlers::claude_method_not_allowed),
        )
        .route(
            "/v1/messages/count_tokens",
            post(handlers::count_tokens).fallback(handlers::claude_method_not_allowed),
        )
        .route(
            "/messages/count_tokens",
            post(handlers::count_tokens).fallback(handlers::claude_method_not_allowed),
        )
        .route(
            "/anthropic/v1/messages/count_tokens",
            post(handlers::count_tokens).fallback(handlers::claude_method_not_allowed),
        )
        .route(
            "/v1/chat/completions",
            post(handlers::openai_chat).fallback(handlers::openai_method_not_allowed),
        )
        .route(
            "/chat/completions",
            post(handlers::openai_chat).fallback(handlers::openai_method_not_allowed),
        )
        .route(
            "/v1/models",
            get(handlers::models).fallback(handlers::openai_models_method_not_allowed),
        )
        .route(
            "/models",
            get(handlers::models).fallback(handlers::openai_models_method_not_allowed),
        )
        .route("/api/event_logging/batch", post(handlers::event_logging))
        .fallback(handlers::not_found)
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
        .layer(middleware::from_fn(catch_panics))
        .layer(middleware::from_fn_with_state(
            middleware_state,
            keep_alive_headers,
        ))
        .layer(middleware::from_fn(cors))
        .layer(middleware::from_fn(trace_requests))
        .with_state(state)
}

async fn trace_requests(mut request: axum::extract::Request, next: Next) -> Response {
    let trace_id = format!("trace_{}", Uuid::new_v4().simple());
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let started = Instant::now();
    request.extensions_mut().insert(RequestTrace {
        id: trace_id.clone(),
    });
    tracing::info!(
        trace_id = %trace_id,
        http_method = %method,
        http_path = %path,
        "client request received"
    );
    let span = tracing::info_span!(
        "client_request",
        trace_id = %trace_id,
        http_method = %method,
        http_path = %path
    );
    let mut response = next.run(request).instrument(span).await;
    let status = response.status();
    let duration_ms = started.elapsed().as_millis() as u64;
    if status.is_server_error() {
        tracing::error!(
            trace_id = %trace_id,
            http_status = status.as_u16(),
            duration_ms,
            "client response headers ready"
        );
    } else if status.is_client_error() {
        tracing::warn!(
            trace_id = %trace_id,
            http_status = status.as_u16(),
            duration_ms,
            "client response headers ready"
        );
    } else {
        tracing::info!(
            trace_id = %trace_id,
            http_status = status.as_u16(),
            duration_ms,
            "client response headers ready"
        );
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(&trace_id) {
        response
            .headers_mut()
            .insert(axum::http::HeaderName::from_static(TRACE_ID_HEADER), value);
    }
    response
}

async fn cors(request: axum::extract::Request, next: Next) -> Response {
    let preflight = request.method() == axum::http::Method::OPTIONS;
    let mut response = if preflight {
        axum::http::StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
        axum::http::HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
        axum::http::HeaderValue::from_static(
            "authorization, content-type, x-api-key, anthropic-api-key, anthropic-version, anthropic-beta",
        ),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_MAX_AGE,
        axum::http::HeaderValue::from_static("86400"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_EXPOSE_HEADERS,
        axum::http::HeaderValue::from_static(TRACE_ID_HEADER),
    );
    response
}

async fn keep_alive_headers(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let http1 = matches!(
        request.version(),
        axum::http::Version::HTTP_10 | axum::http::Version::HTTP_11
    );
    let mut response = next.run(request).await;
    if http1 {
        let seconds = state
            .config
            .current()
            .server
            .keep_alive_timeout_ms
            .div_ceil(1_000)
            .max(1);
        if let Ok(value) = axum::http::HeaderValue::from_str(&format!("timeout={seconds}")) {
            response.headers_mut().insert(
                axum::http::header::HeaderName::from_static("keep-alive"),
                value,
            );
        }
    }
    response
}

async fn catch_panics(request: axum::extract::Request, next: Next) -> Response {
    match std::panic::AssertUnwindSafe(next.run(request))
        .catch_unwind()
        .await
    {
        Ok(response) => response,
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error":{"type":"server_error","message":"Internal server error"}
            })),
        )
            .into_response(),
    }
}

pub async fn serve(
    state: Arc<AppState>,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    if std::env::var("KAM_DISABLE_HTTP").as_deref() == Ok("1") {
        tracing::info!("business HTTP plane disabled by KAM_DISABLE_HTTP");
        shutdown.cancelled().await;
        return Ok(());
    }
    let config = state.config.current();
    let port = std::env::var("KAM_HTTP_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(config.server.port);
    if config.server.tls.enabled {
        let address = resolve_address(&config.server.host, port).await?;
        let tls =
            if let (Some(cert), Some(key)) = (&config.server.tls.cert, &config.server.tls.key) {
                axum_server::tls_rustls::RustlsConfig::from_pem(
                    cert.as_bytes().to_vec(),
                    key.as_bytes().to_vec(),
                )
                .await?
            } else {
                let cert = config.server.tls.cert_path.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("server.tls.cert_path or inline cert is required")
                })?;
                let key = config.server.tls.key_path.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("server.tls.key_path or inline key is required")
                })?;
                axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await?
            };
        let handle = axum_server::Handle::new();
        state.install_tls_config(tls.clone());
        let stop_handle = handle.clone();
        tokio::spawn(async move {
            shutdown.cancelled().await;
            stop_handle.graceful_shutdown(Some(Duration::from_secs(10)));
        });
        tracing::info!(%address, "business HTTPS plane listening");
        axum_server::bind_rustls(address, tls)
            .handle(handle)
            .serve(router(state).into_make_service())
            .await?;
        return Ok(());
    }
    let address = format!("{}:{}", config.server.host, port);
    let listener = tokio::net::TcpListener::bind(&address).await?;
    tracing::info!(%address, "business HTTP plane listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await?;
    Ok(())
}

async fn resolve_address(host: &str, port: u16) -> anyhow::Result<std::net::SocketAddr> {
    tokio::net::lookup_host((host, port))
        .await?
        .next()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve listen host {host}"))
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Method, Request, StatusCode};
    use kam_core::config::{ApiKeyConfig, ApiKeyFormat, Config};
    use kam_core::paths::Paths;
    use kam_store::accounts::AccountStore;
    use kam_store::config_loader::ConfigHandle;
    use tower::ServiceExt;

    use super::*;

    async fn test_state(mut config: Config) -> (tempfile::TempDir, Arc<AppState>) {
        config.server.port = 5580;
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = Paths::from_env_values(
            Some(directory.path().to_str().expect("utf8")),
            None,
            None,
            None,
        );
        kam_store::bootstrap::ensure_layout(&paths)
            .await
            .expect("layout");
        let accounts = AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        let state = Arc::new(AppState::new(paths, ConfigHandle::new(config), accounts));
        (directory, state)
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    #[tokio::test]
    async fn every_response_has_a_unique_trace_id() {
        let (_directory, state) = test_state(Config::default()).await;
        let mut trace_ids = Vec::new();
        for _ in 0..2 {
            let response = router(Arc::clone(&state))
                .oneshot(
                    Request::get("/health")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            let trace_id = response
                .headers()
                .get(TRACE_ID_HEADER)
                .expect("trace header")
                .to_str()
                .expect("trace header text")
                .to_owned();
            assert!(trace_id.starts_with("trace_"));
            assert_eq!(trace_id.len(), 38);
            trace_ids.push(trace_id);
        }
        assert_ne!(trace_ids[0], trace_ids[1]);
    }

    #[tokio::test]
    async fn aliases_methods_and_model_timestamps_match_protocol_contract() {
        let (_directory, state) = test_state(Config::default()).await;
        for path in ["/v1/models", "/models"] {
            let response = router(Arc::clone(&state))
                .oneshot(Request::get(path).body(Body::empty()).expect("request"))
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK);
            let body = body_json(response).await;
            let created = body["data"][0]["created"].as_i64().expect("created");
            assert!(created > 1_700_000_000 && created < 100_000_000_000);
        }

        let response = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/anthropic/v1/messages")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            response.headers().get(header::ALLOW).expect("allow"),
            "POST"
        );
        assert_eq!(body_json(response).await["type"], "error");
    }

    #[tokio::test]
    async fn claude_user_agent_check_does_not_apply_to_openai_routes() {
        let (_directory, state) = test_state(Config::default()).await;
        let claude = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/messages/count_tokens")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"messages":[]}"#))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(claude.status(), StatusCode::BAD_REQUEST);
        assert!(body_json(claude).await["error"]["message"]
            .as_str()
            .expect("message")
            .contains("Claude Code"));

        let openai = router(state)
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"model":"test","messages":[{"role":"user","content":"hello"}],"max_tokens":1}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(openai.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(openai).await;
        assert!(body.get("type").is_none());
        assert!(!body["error"]["message"]
            .as_str()
            .expect("message")
            .contains("Claude Code"));
    }

    #[tokio::test]
    async fn api_key_challenge_and_bodyless_event_logging_are_stable() {
        let mut config = Config::default();
        config.api_key.push(ApiKeyConfig {
            id: Some("ak_test".into()),
            name: "test".into(),
            key: "sk-secret".into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            credits_limit: None,
        });
        let (_directory, state) = test_state(config).await;
        let unauthorized = router(Arc::clone(&state))
            .oneshot(
                Request::post("/api/event_logging/batch")
                    .body(Body::from("not-json"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauthorized
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .expect("challenge"),
            "Bearer"
        );

        let accepted = router(state)
            .oneshot(
                Request::post("/api/event_logging/batch")
                    .header(header::AUTHORIZATION, "Bearer sk-secret")
                    .body(Body::from("not-json-and-never-parsed"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(accepted.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn failed_request_stats_hide_unauthenticated_models_and_truncate_authenticated_ones() {
        let mut config = Config::default();
        config.api_key.push(ApiKeyConfig {
            id: Some("ak_test".into()),
            name: "test".into(),
            key: "sk-secret".into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            credits_limit: None,
        });
        let (_directory, state) = test_state(config).await;
        let attacker_model = "attacker-controlled-model";
        let unauthorized = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"model": attacker_model, "messages": []}).to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let unauthorized_trace = unauthorized
            .headers()
            .get(TRACE_ID_HEADER)
            .expect("trace header")
            .to_str()
            .expect("trace header text")
            .to_owned();

        let rejected_user_agent_model = "rejected-user-agent-model";
        let rejected_user_agent = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/messages")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model": rejected_user_agent_model,
                            "messages": []
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(rejected_user_agent.status(), StatusCode::BAD_REQUEST);

        let long_model = "界".repeat(200);
        let invalid = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer sk-secret")
                    .body(Body::from(
                        serde_json::json!({"model": long_model, "messages": []}).to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

        let snapshot = state.stats.snapshot(None);
        assert!(snapshot.by_model.contains_key("unknown"));
        assert!(!snapshot.by_model.contains_key(attacker_model));
        assert!(!snapshot.by_model.contains_key(rejected_user_agent_model));
        let truncated = "界".repeat(128);
        assert!(snapshot.by_model.contains_key(&truncated));
        assert!(snapshot
            .recent_requests
            .iter()
            .all(|request| request.model.chars().count() <= 128));
        assert!(snapshot
            .recent_requests
            .iter()
            .any(|request| request.trace_id == unauthorized_trace));
    }

    #[tokio::test]
    async fn cors_preflight_and_count_tokens_follow_the_public_contract() {
        let (_directory, state) = test_state(Config::default()).await;
        let preflight = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/v1/messages")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("preflight");
        assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            preflight
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .expect("cors"),
            "*"
        );

        let counted = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/messages/count_tokens")
                    .header(header::USER_AGENT, "claude-cli/1.0 (external, test)")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"model":"model","messages":[{"role":"user","content":"hello"}]}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("count");
        assert_eq!(counted.status(), StatusCode::OK);
        assert!(body_json(counted).await["input_tokens"]
            .as_u64()
            .is_some_and(|tokens| tokens > 0));

        let invalid = router(state)
            .oneshot(
                Request::post("/v1/messages/count_tokens")
                    .header(header::USER_AGENT, "claude-cli/1.0 (external, test)")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .expect("request"),
            )
            .await
            .expect("invalid");
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    }
}
