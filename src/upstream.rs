//! 上游选号与 URL / 模型名处理。
//!
//! 与原版逻辑一致：
//! - 新会话选号时向所有上游并行发起 `GET {base}/v1/account/select?model={映射后模型名}`，
//!   单请求超时 10 秒，超时或响应非法的提供商直接出局；
//! - 在有效候选中选取 `expire_at` 最早（最先过期）的提供商，实现账号雨露均沾。

use crate::config::ProviderConfig;
use futures_util::future;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

const SELECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 单个上游的选号结果。
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateAccount {
    pub provider_id: String,
    pub account_id: i64,
    pub expire_at: i64,
}

#[derive(Debug, Deserialize)]
struct AccountSelectResponse {
    account_id: serde_json::Value,
    expire_at: serde_json::Value,
}

/// 智能拼接上游 URL：基础地址带不带 `/v1`、端点带不带 `v1/` 均可，
/// 结果保证恰好含一段 `/v1`。
///
/// 例：`("https://api.a.com/v1", "chat/completions")` 与
/// `("https://api.a.com", "v1/chat/completions")` 产出相同结果。
pub fn build_provider_url(base_url: &str, endpoint_path: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    let path = endpoint_path.trim().trim_start_matches('/');

    let base_has_v1 = base
        .rsplit('/')
        .next()
        .map(|seg| seg.eq_ignore_ascii_case("v1"))
        .unwrap_or(false);
    let path_has_v1 = path.starts_with("v1/");

    if base_has_v1 {
        let path = path.strip_prefix("v1/").unwrap_or(path);
        format!("{}/{}", base, path)
    } else if path_has_v1 {
        format!("{}/{}", base, path)
    } else {
        format!("{}/v1/{}", base, path)
    }
}

/// 解析模型映射：命中映射表则返回目标模型名，否则原样返回。
pub fn resolve_target_model(requested_model: &str, provider: &ProviderConfig) -> String {
    provider
        .models
        .get(requested_model)
        .cloned()
        .unwrap_or_else(|| requested_model.to_string())
}

/// 查询单个上游的选号接口；失败、超时或响应非法时返回 `None`。
async fn query_account_select(
    client: &Client,
    provider_id: &str,
    provider: &ProviderConfig,
    model_id: &str,
) -> Option<CandidateAccount> {
    let actual_model = resolve_target_model(model_id, provider);
    let target_url = build_provider_url(
        &provider.url,
        &format!("/v1/account/select?model={}", urlencode(&actual_model)),
    );

    let response = client
        .get(&target_url)
        .timeout(SELECT_TIMEOUT)
        .bearer_auth(&provider.key)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let data: AccountSelectResponse = response.json().await.ok()?;
    let account_id = match data.account_id {
        serde_json::Value::Number(n) => n.as_i64()?,
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok()?,
        _ => return None,
    };
    let expire_at = data.expire_at.as_i64()?;

    Some(CandidateAccount {
        provider_id: provider_id.to_string(),
        account_id,
        expire_at,
    })
}

/// 向所有上游并行发起选号，返回 `expire_at` 最早的候选；全部失败返回 `None`。
pub async fn select_upstream_account(
    client: &Client,
    providers: &HashMap<String, ProviderConfig>,
    model_id: &str,
) -> Option<CandidateAccount> {
    let tasks = providers.iter().map(|(id, cfg)| {
        let client = client.clone();
        let id = id.clone();
        async move { query_account_select(&client, &id, cfg, model_id).await }
    });

    let candidates: Vec<CandidateAccount> = future::join_all(tasks)
        .await
        .into_iter()
        .flatten()
        .collect();

    pick_earliest_expire(candidates)
}

/// 从候选中选出 `expire_at` 最早者（纯函数，便于测试）。
pub fn pick_earliest_expire(candidates: Vec<CandidateAccount>) -> Option<CandidateAccount> {
    candidates.into_iter().min_by_key(|c| c.expire_at)
}

/// 极简百分号编码，覆盖查询串中的保留字符。
fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        let safe = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if safe {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(url: &str, models: &[(&str, &str)]) -> ProviderConfig {
        ProviderConfig {
            url: url.to_string(),
            key: "sk-test".to_string(),
            models: models
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn url_join_variants() {
        // 基础地址带 /v1、端点不带。
        assert_eq!(
            build_provider_url("https://api.a.com/v1/", "chat/completions"),
            "https://api.a.com/v1/chat/completions"
        );
        // 基础地址不带 /v1、端点带 v1/。
        assert_eq!(
            build_provider_url("https://api.b.com", "/v1/chat/completions"),
            "https://api.b.com/v1/chat/completions"
        );
        // 两边都不带：自动补 /v1。
        assert_eq!(
            build_provider_url("https://api.c.com", "models"),
            "https://api.c.com/v1/models"
        );
        // 两边都带：去重。
        assert_eq!(
            build_provider_url("https://api.d.com/v1", "v1/models"),
            "https://api.d.com/v1/models"
        );
    }

    #[test]
    fn model_mapping_hit_and_miss() {
        let p = provider("https://x.com", &[("gpt-4o", "gpt-4o-internal")]);
        assert_eq!(resolve_target_model("gpt-4o", &p), "gpt-4o-internal");
        assert_eq!(resolve_target_model("unknown-model", &p), "unknown-model");
    }

    #[test]
    fn earliest_expire_wins() {
        let candidates = vec![
            CandidateAccount {
                provider_id: "a".into(),
                account_id: 1,
                expire_at: 200,
            },
            CandidateAccount {
                provider_id: "b".into(),
                account_id: 2,
                expire_at: 100,
            },
            CandidateAccount {
                provider_id: "c".into(),
                account_id: 3,
                expire_at: 300,
            },
        ];
        let picked = pick_earliest_expire(candidates).unwrap();
        assert_eq!(picked.provider_id, "b");
        assert_eq!(picked.account_id, 2);
        assert_eq!(pick_earliest_expire(Vec::new()), None);
    }

    #[test]
    fn urlencode_escapes_reserved_chars() {
        assert_eq!(urlencode("gpt-4o_mini.v2~1"), "gpt-4o_mini.v2~1");
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
    }
}
