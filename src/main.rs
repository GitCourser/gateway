//! Gateway 入口：配置加载、路由装配与服务启动。
//!
//! 复刻自 Cloudflare Workers 版聚合网关，网关行为保持一致：
//! - `GET /health`：健康检查；
//! - `GET /v1/models`：返回配置文件 `models` 列表（OpenAI 格式）；
//! - `GET /v1/models/{id}`：返回单个模型信息；
//! - 其余路径全部透传到选定的上游（`/v1` 前缀智能拼接）；
//! - 会话粘连：`session-id`（或任意 `*-session-id`）请求头存在时，
//!   同会话固定路由到同一上游，绑定保存在内存中，容量 `SESSION_MAX`（默认 10），
//!   超出后 FIFO 逐出；无会话头则每次请求重新选号，不落缓存。

mod config;
mod proxy;
mod session;
mod upstream;

use crate::config::AppConfig;
use crate::session::{SessionBinding, SessionStore};
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::Response;
use axum::routing::{any, get};
use serde_json::json;
use std::sync::Arc;

/// 应用共享状态。
struct AppState {
    config: AppConfig,
    sessions: SessionStore,
    http: reqwest::Client,
}

/// 提取请求头中的会话 ID：优先 `session-id`，其次任意 `*-session-id` 结尾的头。
fn extract_session_id(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("session-id").and_then(|v| v.to_str().ok()) {
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    for (name, value) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if lower != "session-id"
            && lower.ends_with("-session-id")
            && let Ok(text) = value.to_str()
        {
            let text = text.trim();
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}

/// 校验下游客户端 `Authorization: Bearer <key>`。
fn validate_client_auth(headers: &HeaderMap, expected: &str) -> bool {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
        .map(str::trim);
    matches!(token, Some(t) if t == expected)
}

fn health_response() -> Response {
    json_response(
        StatusCode::OK,
        &json!({ "status": "ok", "service": "gateway" }),
    )
}

fn unauthorized_response() -> Response {
    proxy::error_response(
        StatusCode::UNAUTHORIZED,
        "Incorrect API key provided.",
        "invalid_request_error",
        Some("invalid_api_key"),
    )
}

fn json_response(status: StatusCode, payload: &serde_json::Value) -> Response {
    let mut response = Response::new(Body::from(payload.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    for (name, value) in proxy::CORS_HEADERS {
        if let (Ok(n), Ok(v)) = (
            axum::http::HeaderName::try_from(name),
            HeaderValue::from_str(value),
        ) {
            response.headers_mut().insert(n, v);
        }
    }
    response
}

fn model_item(model_id: &str) -> serde_json::Value {
    json!({
        "id": model_id,
        "object": "model",
        "created": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default(),
        "owned_by": "gateway",
    })
}

async fn handle_cors_preflight() -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    for (name, value) in proxy::CORS_HEADERS {
        if let (Ok(n), Ok(v)) = (
            axum::http::HeaderName::try_from(name),
            HeaderValue::from_str(value),
        ) {
            response.headers_mut().insert(n, v);
        }
    }
    response
}

async fn handle_health() -> Response {
    health_response()
}

async fn handle_models_list(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    if !validate_client_auth(&headers, state.config.client_key.as_deref().unwrap_or("")) {
        return unauthorized_response();
    }
    let data: Vec<serde_json::Value> = state
        .config
        .file
        .models
        .iter()
        .map(|m| model_item(m))
        .collect();
    json_response(StatusCode::OK, &json!({ "object": "list", "data": data }))
}

async fn handle_models_get(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(model_id): axum::extract::Path<String>,
) -> Response {
    if !validate_client_auth(&headers, state.config.client_key.as_deref().unwrap_or("")) {
        return unauthorized_response();
    }
    if !state.config.file.models.contains(&model_id) {
        return proxy::error_response(
            StatusCode::NOT_FOUND,
            &format!("The model '{}' does not exist", model_id),
            "invalid_request_error",
            Some("model_not_found"),
        );
    }
    json_response(StatusCode::OK, &model_item(&model_id))
}

/// 核心调度：鉴权、会话解析、选号与转发。
async fn handle_proxy(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let headers = request.headers().clone();
    let method = request.method().clone();
    let uri: Uri = request.uri().clone();

    if !validate_client_auth(&headers, state.config.client_key.as_deref().unwrap_or("")) {
        return unauthorized_response();
    }

    if state.config.file.providers.is_empty() {
        return proxy::error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "No upstream providers configured.",
            "server_error",
            Some("no_providers_configured"),
        );
    }

    // 读取请求体（非 GET/HEAD），同时解析出 model 字段供选号使用。
    let raw_body = if method != axum::http::Method::GET && method != axum::http::Method::HEAD {
        let bytes = match axum::body::to_bytes(request.into_body(), 64 * 1024 * 1024).await {
            Ok(b) => b,
            Err(_) => {
                return proxy::error_response(
                    StatusCode::BAD_REQUEST,
                    "Failed to read request body.",
                    "invalid_request_error",
                    Some("invalid_body"),
                );
            }
        };
        let text = String::from_utf8_lossy(&bytes).to_string();
        if !text.trim().is_empty() && serde_json::from_str::<serde_json::Value>(&text).is_err() {
            return proxy::error_response(
                StatusCode::BAD_REQUEST,
                "Invalid JSON body in request.",
                "invalid_request_error",
                Some("invalid_json"),
            );
        }
        Some(text)
    } else {
        None
    };

    let requested_model = raw_body
        .as_deref()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .and_then(|v| v.get("model").cloned())
        .and_then(|m| m.as_str().map(str::to_string));

    let session_id = extract_session_id(&headers);

    // 1) 内存命中直接复用绑定。
    let mut binding: Option<SessionBinding> = None;
    if let Some(id) = &session_id {
        binding = state.sessions.get(id).await;
    }

    // 2) 未命中：锁内二次检查并选号（同会话并发请求串行化，保证同会话同上游）。
    if binding.is_none() {
        let Some(model) = requested_model.clone() else {
            return proxy::error_response(
                StatusCode::BAD_REQUEST,
                "Missing required 'model' parameter in request body.",
                "invalid_request_error",
                Some("missing_required_parameter"),
            );
        };

        if let Some(id) = session_id.clone() {
            let http = state.http.clone();
            let providers = state.config.file.providers.clone();
            let result = state
                .sessions
                .entry_or_insert_with(id.clone(), || {
                    let http = http.clone();
                    let providers = providers.clone();
                    let model = model.clone();
                    async move {
                        upstream::select_upstream_account(&http, &providers, &model)
                            .await
                            .ok_or(())
                            .map(|candidate| SessionBinding {
                                provider: candidate.provider_id,
                                account_id: candidate.account_id,
                            })
                    }
                })
                .await;
            match result {
                Ok((found, inserted)) => {
                    if inserted {
                        tracing::info!(
                            session = %id,
                            provider = %found.provider,
                            account_id = found.account_id,
                            "session bound to upstream"
                        );
                    }
                    binding = Some(found);
                }
                Err(()) => {
                    return proxy::error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        &format!("No available upstream provider for model '{}'.", model),
                        "server_error",
                        Some("upstream_unavailable"),
                    );
                }
            }
        } else {
            // 无会话头：直接选号，不落缓存。
            let Some(candidate) = upstream::select_upstream_account(
                &state.http,
                &state.config.file.providers,
                &model,
            )
            .await
            else {
                return proxy::error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &format!("No available upstream provider for model '{}'.", model),
                    "server_error",
                    Some("upstream_unavailable"),
                );
            };
            binding = Some(SessionBinding {
                provider: candidate.provider_id,
                account_id: candidate.account_id,
            });
        }
    }

    let Some(SessionBinding {
        provider: provider_id,
        account_id,
    }) = binding
    else {
        return proxy::error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Failed to resolve upstream provider.",
            "server_error",
            Some("provider_not_found"),
        );
    };

    let Some(provider_config) = state.config.file.providers.get(&provider_id) else {
        return proxy::error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("Provider '{}' configuration not found.", provider_id),
            "server_error",
            Some("provider_not_found"),
        );
    };

    let forward = proxy::ForwardRequest {
        method,
        path_and_query: &uri,
        headers: &headers,
        body: raw_body,
        provider: provider_config,
        account_id,
    };

    match proxy::forward_to_upstream(&state.http, forward).await {
        Ok(response) => response,
        Err(message) => {
            tracing::warn!(provider = %provider_id, error = %message, "upstream forward failed");
            proxy::bad_gateway(&message)
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = match AppConfig::load() {
        Ok(c) => c,
        Err(message) => {
            eprintln!("{}", message);
            std::process::exit(1);
        }
    };

    // 下游鉴权 key：未设置 CLIENT_KEYS 时自动生成并打印。
    let config = match config.client_key {
        Some(key) => AppConfig {
            client_key: Some(key),
            ..config
        },
        None => {
            let key = config::generate_client_key();
            tracing::warn!(
                "CLIENT_KEYS not set. Generated a random API key for this run: {}",
                key
            );
            AppConfig {
                client_key: Some(key),
                ..config
            }
        }
    };
    run(config).await;
}

async fn run(config: AppConfig) {
    let session_max = config.session_max;
    let bind_addr = config.bind_addr.clone();
    let state = Arc::new(AppState {
        config,
        sessions: SessionStore::new(session_max),
        http: reqwest::Client::builder()
            .build()
            .expect("failed to build upstream HTTP client"),
    });

    let app = Router::new()
        .route("/health", get(handle_health))
        .route("/v1/models", get(handle_models_list))
        .route("/v1/models/{id}", get(handle_models_get))
        .route("/{*path}", any(handle_proxy))
        .layer(axum::middleware::from_fn(
            |req: Request, next: axum::middleware::Next| async move {
                // CORS 预检直接响应，不再向下透传。
                if req.method() == axum::http::Method::OPTIONS {
                    return handle_cors_preflight().await;
                }
                let mut response = next.run(req).await;
                for (name, value) in proxy::CORS_HEADERS {
                    if let (Ok(n), Ok(v)) = (
                        axum::http::HeaderName::try_from(name),
                        HeaderValue::from_str(value),
                    ) {
                        response.headers_mut().insert(n, v);
                    }
                }
                response
            },
        ))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {}: {}", bind_addr, e));
    tracing::info!(
        "gateway listening on {} (SESSION_MAX={}, providers={})",
        bind_addr,
        session_max,
        state.config.file.providers.len()
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await
        .expect("server error");
}
