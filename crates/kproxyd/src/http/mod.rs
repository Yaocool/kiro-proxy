//! Public business HTTP plane.

mod handlers;
pub(crate) use handlers::fallback_models;
pub(crate) mod prompt_cache;
mod response;
pub(crate) mod stream;
mod usage;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::FutureExt;
use tracing::Instrument;
use uuid::Uuid;

use kproxy_core::config::ProxyServiceConfig;
use kproxy_ipc::protocol::ProxyServiceView;

use crate::state::AppState;

pub(crate) const TRACE_ID_HEADER: &str = "x-trace-id";
pub(crate) const REQUEST_ID_HEADER: &str = "request-id";

#[derive(Clone)]
pub(crate) struct RequestTrace {
    pub id: String,
}

/// Router state scoped to one configured proxy service.
#[derive(Clone)]
pub(crate) struct ServiceHttpState {
    pub app: Arc<AppState>,
    pub service: Arc<ProxyServiceConfig>,
    pub allowed_api_key_ids: Arc<HashSet<String>>,
}

pub(crate) fn request_trace_id(request: &axum::extract::Request) -> String {
    request
        .extensions()
        .get::<RequestTrace>()
        .map(|trace| trace.id.clone())
        .unwrap_or_else(|| format!("trace_{}", Uuid::new_v4().simple()))
}

#[cfg(test)]
pub fn router(state: Arc<AppState>) -> Router {
    let config = state.config.current();
    let service = ProxyServiceConfig {
        id: "test".into(),
        name: "test".into(),
        host: config.server.host.clone(),
        port: config.server.port,
        enabled: true,
        api_key_ids: config
            .api_key
            .iter()
            .filter_map(|key| key.id.clone())
            .collect(),
        created_at: 0,
    };
    router_for_service(state, service, false)
}

fn router_for_service(
    state: Arc<AppState>,
    service: ProxyServiceConfig,
    enforce_service_keys: bool,
) -> Router {
    let mut allowed_api_key_ids = service.api_key_ids.iter().cloned().collect::<HashSet<_>>();
    if !enforce_service_keys {
        allowed_api_key_ids.clear();
    }
    let router_state = ServiceHttpState {
        app: state,
        service: Arc::new(service),
        allowed_api_key_ids: Arc::new(allowed_api_key_ids),
    };
    let middleware_state = router_state.clone();
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
        .with_state(router_state)
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
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(TRACE_ID_HEADER),
            value.clone(),
        );
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(REQUEST_ID_HEADER),
            value,
        );
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
        axum::http::HeaderValue::from_static("x-trace-id, request-id"),
    );
    response
}

async fn keep_alive_headers(
    State(state): State<ServiceHttpState>,
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
            .app
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
    let request_id = request_trace_id(&request);
    match std::panic::AssertUnwindSafe(next.run(request))
        .catch_unwind()
        .await
    {
        Ok(response) => response,
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "type": "error",
                "error":{"type":"api_error","message":"Internal server error"},
                "request_id": request_id
            })),
        )
            .into_response(),
    }
}

struct RunningService {
    config: ProxyServiceConfig,
    cancel: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<()>,
    error: Arc<RwLock<Option<String>>>,
}

/// Reconciles configured proxy listeners independently from the admin plane.
#[derive(Default)]
pub struct ProxyServiceManager {
    reconcile_lock: tokio::sync::Mutex<()>,
    running: tokio::sync::Mutex<HashMap<String, RunningService>>,
}

impl ProxyServiceManager {
    pub async fn reconcile(
        &self,
        state: Arc<AppState>,
        services: &[ProxyServiceConfig],
    ) -> Vec<(String, String)> {
        let _reconcile = self.reconcile_lock.lock().await;
        let desired = services
            .iter()
            .filter(|service| service.enabled)
            .cloned()
            .map(|service| (service.id.clone(), service))
            .collect::<HashMap<_, _>>();

        let stopped = {
            let mut running = self.running.lock().await;
            let ids = running
                .iter()
                .filter_map(|(id, current)| {
                    (current.task.is_finished()
                        || desired.get(id).is_none_or(|next| next != &current.config))
                    .then_some(id.clone())
                })
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| running.remove(&id))
                .collect::<Vec<_>>()
        };
        for mut service in stopped {
            if service.task.is_finished() && desired.contains_key(&service.config.id) {
                let error = read_lock(&service.error)
                    .clone()
                    .unwrap_or_else(|| "listener task exited unexpectedly".into());
                tracing::warn!(
                    service_id = %service.config.id,
                    service_name = %service.config.name,
                    %error,
                    "proxy service stopped; scheduling restart"
                );
            }
            service.cancel.cancel();
            if tokio::time::timeout(Duration::from_secs(10), &mut service.task)
                .await
                .is_err()
            {
                service.task.abort();
            }
        }

        if std::env::var("KPROXY_DISABLE_HTTP").as_deref() == Ok("1") {
            if !desired.is_empty() {
                tracing::info!("proxy services disabled by KPROXY_DISABLE_HTTP");
            }
            return Vec::new();
        }

        let existing = self
            .running
            .lock()
            .await
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let mut failures = Vec::new();
        for (id, service) in desired {
            if existing.contains(&id) {
                continue;
            }
            match start_service(Arc::clone(&state), service.clone()).await {
                Ok(running) => {
                    self.running.lock().await.insert(id, running);
                }
                Err(error) => {
                    tracing::error!(service_id = %service.id, %error, "proxy service failed to start");
                    failures.push((service.id, error.to_string()));
                }
            }
        }
        failures
    }

    pub async fn views(&self, services: &[ProxyServiceConfig]) -> Vec<ProxyServiceView> {
        let running = self.running.lock().await;
        services
            .iter()
            .map(|service| {
                let runtime = running.get(&service.id);
                let is_running = runtime.is_some_and(|runtime| !runtime.task.is_finished());
                let error = runtime.and_then(|runtime| read_lock(&runtime.error).clone());
                ProxyServiceView {
                    id: service.id.clone(),
                    name: service.name.clone(),
                    host: service.host.clone(),
                    port: service.port,
                    enabled: service.enabled,
                    running: is_running,
                    api_key_ids: service.api_key_ids.clone(),
                    created_at: service.created_at,
                    error,
                }
            })
            .collect()
    }
}

async fn start_service(
    state: Arc<AppState>,
    service: ProxyServiceConfig,
) -> anyhow::Result<RunningService> {
    let config = state.config.current();
    let port = effective_port(&config, &service);
    let address = resolve_address(&service.host, port).await?;
    let cancel = state.shutdown.child_token();
    let error = Arc::new(RwLock::new(None));
    let task_error = Arc::clone(&error);
    let task_service = service.clone();
    let service_id = service.id.clone();
    let service_name = service.name.clone();

    let task = if config.server.tls.enabled {
        let probe = std::net::TcpListener::bind(address)?;
        drop(probe);
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
        state.install_tls_config(tls.clone());
        let handle = axum_server::Handle::new();
        let stop_handle = handle.clone();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::spawn(async move {
                stop.cancelled().await;
                stop_handle.graceful_shutdown(Some(Duration::from_secs(10)));
            });
            tracing::info!(%address, %service_id, %service_name, "proxy HTTPS service listening");
            if let Err(failure) = axum_server::bind_rustls(address, tls)
                .handle(handle)
                .serve(router_for_service(state, task_service, true).into_make_service())
                .await
            {
                *write_lock(&task_error) = Some(failure.to_string());
            }
        })
    } else {
        let listener = tokio::net::TcpListener::bind(address).await?;
        let serve_cancel = cancel.clone();
        tokio::spawn(async move {
            tracing::info!(%address, %service_id, %service_name, "proxy HTTP service listening");
            if let Err(failure) =
                axum::serve(listener, router_for_service(state, task_service, true))
                    .with_graceful_shutdown(serve_cancel.cancelled_owned())
                    .await
            {
                *write_lock(&task_error) = Some(failure.to_string());
            }
        })
    };
    Ok(RunningService {
        config: service,
        cancel,
        task,
        error,
    })
}

fn effective_port(config: &kproxy_core::config::Config, service: &ProxyServiceConfig) -> u16 {
    if service.port == config.server.port {
        std::env::var("KPROXY_HTTP_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|port| *port > 0)
            .unwrap_or(service.port)
    } else {
        service.port
    }
}

async fn resolve_address(host: &str, port: u16) -> anyhow::Result<std::net::SocketAddr> {
    tokio::net::lookup_host((host, port))
        .await?
        .next()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve listen host {host}"))
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Method, Request, StatusCode};
    use kproxy_core::config::{ApiKeyConfig, ApiKeyFormat, Config, ProxyServiceConfig};
    use kproxy_core::paths::Paths;
    use kproxy_store::accounts::AccountStore;
    use kproxy_store::config_loader::ConfigHandle;
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
        kproxy_store::bootstrap::ensure_layout(&paths)
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
            assert_eq!(
                response
                    .headers()
                    .get(REQUEST_ID_HEADER)
                    .expect("request id header"),
                trace_id.as_str()
            );
            assert!(trace_id.starts_with("trace_"));
            assert_eq!(trace_id.len(), 38);
            trace_ids.push(trace_id);
        }
        assert_ne!(trace_ids[0], trace_ids[1]);
    }

    #[tokio::test]
    async fn context_limit_errors_are_self_contained_for_intermediate_relays() {
        let mut config = Config::default();
        config.context.max_input_tokens = 128;
        config.context.safe_input_ratio = 1.0;
        let (_directory, state) = test_state(config).await;
        let response = router(state)
            .oneshot(
                Request::post("/v1/messages")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::USER_AGENT, "claude-cli/2.1.220 (external, test)")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "claude-sonnet-4.6",
                            "max_tokens": 1,
                            "stream": true,
                            "messages": [{"role": "user", "content": "long ".repeat(500)}]
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let request_id = response
            .headers()
            .get(REQUEST_ID_HEADER)
            .expect("request id header")
            .to_str()
            .expect("request id header text")
            .to_owned();
        assert_eq!(
            response
                .headers()
                .get(TRACE_ID_HEADER)
                .expect("trace id header"),
            request_id.as_str()
        );
        let body = body_json(response).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["request_id"], request_id);
        let message = body["error"]["message"].as_str().expect("message");
        assert!(message.starts_with("prompt is too long: "));
        assert!(message.ends_with(" tokens > 128"));
    }

    #[tokio::test]
    async fn ordinary_claude_tools_are_not_rejected_by_the_tool_search_budget() {
        let mut config = Config::default();
        config.context.max_tool_input_tokens = 1;
        let (_directory, state) = test_state(config).await;
        let response = router(state)
            .oneshot(
                Request::post("/v1/messages")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::USER_AGENT, "claude-cli/2.1.220 (external, test)")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "claude-opus-5",
                            "max_tokens": 1,
                            "messages": [{"role": "user", "content": "Who are you?"}],
                            "tools": [{
                                "name": "lookup",
                                "description": "Look up a value in the local workspace",
                                "input_schema": {
                                    "type": "object",
                                    "properties": {"query": {"type": "string"}},
                                    "required": ["query"]
                                }
                            }]
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        // No test account is configured, so reaching the pool proves the
        // ordinary tool request passed local payload admission. Before the
        // fix this returned 413 from max_tool_input_tokens.
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn claude_accepts_more_than_the_legacy_128_tool_limit() {
        let (_directory, state) = test_state(Config::default()).await;
        let tools = (0..129)
            .map(|index| {
                serde_json::json!({
                    "name": format!("tool_{index}"),
                    "description": "small test tool",
                    "input_schema": {"type": "object"}
                })
            })
            .collect::<Vec<_>>();
        let response = router(state)
            .oneshot(
                Request::post("/v1/messages")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::USER_AGENT, "claude-cli/2.1.220 (external, test)")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "claude-opus-5",
                            "max_tokens": 1,
                            "messages": [{"role": "user", "content": "hello"}],
                            "tools": tools
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        // No test account is configured, so reaching the pool proves that
        // the request passed the proxy's tool-count admission checks.
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn claude_tool_capacity_errors_do_not_masquerade_as_32mb_body_errors() {
        let (_directory, state) = test_state(Config::default()).await;
        let tools = (0..=kproxy_translate::validate::MAX_TOOLS)
            .map(|index| {
                serde_json::json!({
                    "name": format!("tool_{index}"),
                    "description": "small test tool",
                    "input_schema": {"type": "object"}
                })
            })
            .collect::<Vec<_>>();
        let response = router(state)
            .oneshot(
                Request::post("/v1/messages")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::USER_AGENT, "claude-cli/2.1.220 (external, test)")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "claude-opus-5",
                            "max_tokens": 1,
                            "messages": [{"role": "user", "content": "hello"}],
                            "tools": tools
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers()["x-kproxy-error-code"],
            "tool_budget_exceeded"
        );
        let body = body_json(response).await;
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(body["error"]["message"]
            .as_str()
            .expect("message")
            .contains("too many tools"));
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

    #[tokio::test]
    async fn count_tokens_applies_existing_compaction_without_triggering_a_new_one() {
        let (_directory, state) = test_state(Config::default()).await;
        let old_context = "old context ".repeat(30_000);
        let request = serde_json::json!({
            "model":"model",
            "messages":[
                {"role":"user","content":old_context.clone()},
                {"role":"assistant","content":[
                    {"type":"compaction","content":"durable summary"},
                    {"type":"text","text":"continued response"}
                ]},
                {"role":"user","content":"new request"}
            ],
            "context_management":{"edits":[{
                "type":"compact_20260112",
                "trigger":{"type":"input_tokens","value":50_000}
            }]}
        });
        let counted = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/messages/count_tokens")
                    .header(header::USER_AGENT, "claude-cli/1.0 (external, test)")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(request.to_string()))
                    .expect("request"),
            )
            .await
            .expect("count");
        assert_eq!(counted.status(), StatusCode::OK);
        let counted = body_json(counted).await;
        assert!(
            counted["context_management"]["original_input_tokens"]
                .as_u64()
                .expect("original")
                > counted["input_tokens"].as_u64().expect("effective")
        );

        let trigger_only = serde_json::json!({
            "model":"model",
            "messages":[{"role":"user","content":old_context}],
            "context_management":{"edits":[{
                "type":"compact_next",
                "trigger":{"type":"input_tokens","value":50_000}
            }]}
        });
        let counted = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/messages/count_tokens")
                    .header(header::USER_AGENT, "claude-cli/1.0 (external, test)")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(trigger_only.to_string()))
                    .expect("request"),
            )
            .await
            .expect("count");
        let counted = body_json(counted).await;
        assert_eq!(
            counted["context_management"]["original_input_tokens"],
            counted["input_tokens"]
        );
    }

    #[tokio::test]
    async fn count_tokens_accepts_claude_code_context_edits() {
        let (_directory, state) = test_state(Config::default()).await;
        let clear_thinking = serde_json::json!({
            "model":"claude-opus-5",
            "messages":[{"role":"user","content":"hello"}],
            "context_management":{"edits":[{
                "type":"clear_thinking_20251015",
                "keep":"all"
            }]}
        });
        let counted = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/messages/count_tokens")
                    .header(header::USER_AGENT, "claude-cli/2.1.220 (external, test)")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("anthropic-beta", "context-management-2025-06-27")
                    .body(Body::from(clear_thinking.to_string()))
                    .expect("request"),
            )
            .await
            .expect("count");
        assert_eq!(counted.status(), StatusCode::OK);
        let counted = body_json(counted).await;
        assert_eq!(
            counted["context_management"]["original_input_tokens"],
            counted["input_tokens"]
        );

        let clear_tools = serde_json::json!({
            "model":"claude-opus-5",
            "messages":[
                {"role":"user","content":"read the file"},
                {"role":"assistant","content":[{
                    "type":"tool_use","id":"toolu_old","name":"Read",
                    "input":{"path":"/tmp/large"}
                }]},
                {"role":"user","content":[{
                    "type":"tool_result","tool_use_id":"toolu_old",
                    "content":"old output ".repeat(10_000)
                }]},
                {"role":"user","content":"continue"}
            ],
            "context_management":{"edits":[{
                "type":"clear_tool_uses_20250919",
                "keep":{"type":"tool_uses","value":0},
                "clear_tool_inputs":true
            }]}
        });
        let counted = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/messages/count_tokens")
                    .header(header::USER_AGENT, "claude-cli/2.1.220 (external, test)")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("anthropic-beta", "context-management-2025-06-27")
                    .body(Body::from(clear_tools.to_string()))
                    .expect("request"),
            )
            .await
            .expect("count");
        assert_eq!(counted.status(), StatusCode::OK);
        let counted = body_json(counted).await;
        assert!(
            counted["context_management"]["original_input_tokens"]
                .as_u64()
                .expect("original")
                > counted["input_tokens"].as_u64().expect("effective")
        );
    }

    #[tokio::test]
    async fn reconcile_restarts_a_finished_proxy_listener() {
        let (_directory, state) = test_state(Config::default()).await;
        let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
        let port = probe.local_addr().expect("address").port();
        drop(probe);
        let service = ProxyServiceConfig {
            id: "svc_restart".into(),
            name: "restart".into(),
            host: "127.0.0.1".into(),
            port,
            enabled: true,
            api_key_ids: Vec::new(),
            created_at: 0,
        };
        let finished = tokio::spawn(async {});
        while !finished.is_finished() {
            tokio::task::yield_now().await;
        }
        state.proxy_services.running.lock().await.insert(
            service.id.clone(),
            RunningService {
                config: service.clone(),
                cancel: tokio_util::sync::CancellationToken::new(),
                task: finished,
                error: Arc::new(RwLock::new(Some("accept loop failed".into()))),
            },
        );

        let failures = state
            .proxy_services
            .reconcile(Arc::clone(&state), std::slice::from_ref(&service))
            .await;
        assert!(failures.is_empty(), "{failures:?}");
        let views = state
            .proxy_services
            .views(std::slice::from_ref(&service))
            .await;
        assert!(views[0].running);
        let health = reqwest::get(format!("http://127.0.0.1:{port}/health"))
            .await
            .expect("restarted listener");
        assert_eq!(health.status(), StatusCode::OK);

        state.shutdown.cancel();
        state
            .proxy_services
            .reconcile(Arc::clone(&state), &[])
            .await;
    }
}
