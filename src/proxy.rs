//! 请求转发（反向代理核心）。
//!
//! 与原版逻辑一致：
//! - 复制客户端请求头并剔除逐跳 / 冲突头（authorization、host、content-length、
//!   `cf-` 前缀），替换 `Authorization` 为上游 key，注入 `x-account-id`；
//! - 命中模型映射时重写请求体中的 `model` 字段；
//! - 上游响应状态码、头（剔除 content-encoding / content-length / transfer-encoding）
//!   与流式 body（含 SSE）直接透传给下游；
//! - 响应统一附加 CORS 头。

use crate::config::ProviderConfig;
use crate::upstream::{build_provider_url, resolve_target_model};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use axum::response::Response;
use reqwest::Client;

/// 允许直接暴露给客户端的 CORS 响应头。
pub const CORS_HEADERS: [(&str, &str); 4] = [
    ("access-control-allow-origin", "*"),
    (
        "access-control-allow-methods",
        "GET, POST, PUT, DELETE, OPTIONS, HEAD",
    ),
    ("access-control-allow-headers", "*"),
    ("access-control-max-age", "86400"),
];

/// 构造 OpenAI 风格的 JSON 错误响应。
pub fn error_response(
    status: StatusCode,
    message: &str,
    error_type: &str,
    code: Option<&str>,
) -> Response {
    let payload = serde_json::json!({
        "error": {
            "message": message,
            "type": error_type,
            "param": null,
            "code": code,
        }
    });

    let mut response = Response::new(Body::from(payload.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        reqwest::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    for (name, value) in CORS_HEADERS {
        if let (Ok(n), Ok(v)) = (HeaderName::try_from(name), HeaderValue::from_str(value)) {
            response.headers_mut().insert(n, v);
        }
    }
    response
}

/// 判断客户端请求头是否应原样转发给上游。
fn forwardable_request_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "authorization"
            | "content-length"
            | "host"
            | "connection"
            | "transfer-encoding"
            | "keep-alive"
    ) && !lower.starts_with("cf-")
}

/// 判断上游响应头是否应透传给下游（剔除与流式 body 冲突的传输层头）。
fn forwardable_response_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "content-encoding" | "content-length" | "transfer-encoding" | "connection" | "keep-alive"
    )
}

/// 将请求体中的 `model` 字段按映射重写；未命中映射则原样返回。
pub fn rewrite_model_field(raw_body: &str, provider: &ProviderConfig) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(raw_body) {
        Ok(v) => v,
        Err(_) => return raw_body.to_string(),
    };

    let mut object = match parsed {
        serde_json::Value::Object(map) => map,
        other => {
            let text = serde_json::to_string(&other).unwrap_or_else(|_| raw_body.to_string());
            return text;
        }
    };

    if let Some(serde_json::Value::String(model)) = object.get("model") {
        let resolved = resolve_target_model(model, provider);
        object.insert("model".to_string(), serde_json::Value::String(resolved));
    }

    serde_json::to_string(&serde_json::Value::Object(object))
        .unwrap_or_else(|_| raw_body.to_string())
}

/// 转发参数。
pub struct ForwardRequest<'a> {
    pub method: reqwest::Method,
    pub path_and_query: &'a Uri,
    pub headers: &'a HeaderMap,
    /// 已读取的原始请求体（非 GET/HEAD 请求必填）。
    pub body: Option<String>,
    pub provider: &'a ProviderConfig,
    pub account_id: i64,
}

/// 执行转发并构造透传给下游的响应。
pub async fn forward_to_upstream(
    client: &Client,
    req: ForwardRequest<'_>,
) -> Result<Response, String> {
    let target_base = build_provider_url(&req.provider.url, req.path_and_query.path());
    let mut target_url = target_base;
    if let Some(query) = req.path_and_query.query() {
        target_url.push('?');
        target_url.push_str(query);
    }

    let mut upstream_request = client.request(req.method.clone(), &target_url);
    for (name, value) in req.headers.iter() {
        let name_str = name.as_str();
        if !forwardable_request_header(name_str) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            upstream_request = upstream_request.header(name_str, v);
        }
    }

    upstream_request = upstream_request
        .bearer_auth(&req.provider.key)
        .header("x-account-id", req.account_id.to_string());

    if let Some(body) = req.body {
        let final_body = rewrite_model_field(&body, req.provider);
        upstream_request =
            upstream_request.header(reqwest::header::CONTENT_TYPE, "application/json");
        upstream_request = upstream_request.body(final_body);
    }

    let upstream_response = upstream_request
        .send()
        .await
        .map_err(|e| format!("Upstream request failed: {}", e))?;

    let status = upstream_response.status();
    let mut response = Response::builder().status(status.as_u16());

    for (name, value) in upstream_response.headers().iter() {
        if !forwardable_response_header(name.as_str()) {
            continue;
        }
        if let Ok(v) = HeaderValue::from_bytes(value.as_bytes()) {
            response = response.header(name, v);
        }
    }
    for (name, value) in CORS_HEADERS {
        response = response.header(name, value);
    }

    let stream = upstream_response.bytes_stream();
    let body = Body::from_stream(stream);
    response
        .body(body)
        .map_err(|e| format!("Failed to build downstream response: {}", e))
}

/// 优雅返回上游 502 场景的便捷封装（保留统一错误格式）。
pub fn bad_gateway(message: &str) -> Response {
    error_response(
        StatusCode::BAD_GATEWAY,
        message,
        "bad_gateway",
        Some("upstream_error"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(models: &[(&str, &str)]) -> ProviderConfig {
        ProviderConfig {
            url: "https://upstream.test/v1".to_string(),
            key: "sk-upstream".to_string(),
            models: models
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn rewrites_model_when_mapping_exists() {
        let p = provider(&[("deepseek-v4.1-flash", "deepseek-flash")]);
        let body = r#"{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"hi"}],"stream":true}"#;
        let out = rewrite_model_field(body, &p);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["model"], "deepseek-flash");
        assert_eq!(parsed["messages"][0]["content"], "hi");
        assert_eq!(parsed["stream"], true);
    }

    #[test]
    fn keeps_model_when_no_mapping() {
        let p = provider(&[]);
        let body = r#"{"model":"gpt-4o"}"#;
        let out = rewrite_model_field(body, &p);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["model"], "gpt-4o");
    }

    #[test]
    fn non_object_body_passthrough() {
        let p = provider(&[("a", "b")]);
        assert_eq!(rewrite_model_field("[1,2,3]", &p), "[1,2,3]");
        assert_eq!(rewrite_model_field("not json", &p), "not json");
    }

    #[test]
    fn header_filters() {
        assert!(!forwardable_request_header("Authorization"));
        assert!(!forwardable_request_header("Host"));
        assert!(!forwardable_request_header("Content-Length"));
        assert!(!forwardable_request_header("CF-Connecting-IP"));
        assert!(forwardable_request_header("X-Custom-Header"));
        assert!(forwardable_request_header("session-id"));

        assert!(!forwardable_response_header("Content-Encoding"));
        assert!(!forwardable_response_header("Transfer-Encoding"));
        assert!(forwardable_response_header("Content-Type"));
    }

    #[test]
    fn error_response_shape() {
        let resp = error_response(
            StatusCode::UNAUTHORIZED,
            "Incorrect API key provided.",
            "invalid_request_error",
            Some("invalid_api_key"),
        );
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers().get("access-control-allow-origin").unwrap(),
            "*"
        );
    }
}
