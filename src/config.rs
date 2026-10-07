//! 配置加载与环境变量解析。
//!
//! 配置来源：
//! - JSON 配置文件（`CONFIG_FILE` 环境变量指定路径，默认 `./config.json`）：
//!   包含下游可见模型列表 `models` 与上游提供商字典 `providers`。
//! - 环境变量：`CLIENT_KEYS`（下游鉴权 key）、`SESSION_MAX`（会话绑定容量）、
//!   `PORT` / `BIND`（监听地址）。
//!
//! 全部采用 fail-fast 策略：任何解析失败在启动阶段即报错退出。

use rand::RngExt;
use serde::Deserialize;
use std::collections::HashMap;

/// 上游提供商配置。
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// 上游接口基础地址，允许带或不带 `/v1`，由网关智能拼接。
    pub url: String,
    /// 向该上游请求时使用的 Bearer Token。
    pub key: String,
    /// 模型映射表：键为下游请求的模型名，值为转发给该上游时实际使用的模型名。
    pub models: HashMap<String, String>,
}

/// JSON 配置文件结构。
#[derive(Debug, Clone)]
pub struct FileConfig {
    /// 允许向客户端暴露的模型列表（用于 `/v1/models`）。
    pub models: Vec<String>,
    /// 上游提供商字典，键为提供商 ID。
    pub providers: HashMap<String, ProviderConfig>,
}

/// 运行期完整配置。
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub file: FileConfig,
    /// 下游客户端鉴权 key；为空表示未配置，`main` 中会生成随机 key。
    pub client_key: Option<String>,
    /// 会话粘连绑定最大条数，超出后按 FIFO 逐出最旧条目。
    pub session_max: usize,
    /// 监听地址。
    pub bind_addr: String,
}

#[derive(Debug, Deserialize)]
struct RawProvider {
    url: String,
    key: String,
    #[serde(default)]
    models: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct RawFileConfig {
    #[serde(default)]
    models: Option<Vec<String>>,
    #[serde(default)]
    providers: Option<HashMap<String, RawProvider>>,
}

impl AppConfig {
    /// 从环境变量与配置文件加载完整配置，任一环节失败即返回错误描述。
    pub fn load() -> Result<Self, String> {
        let config_path =
            std::env::var("CONFIG_FILE").unwrap_or_else(|_| "config.json".to_string());
        let file = load_file_config(&config_path)?;

        let client_key = std::env::var("CLIENT_KEYS")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        let session_max = match std::env::var("SESSION_MAX") {
            Ok(raw) => raw
                .trim()
                .parse::<usize>()
                .map_err(|_| {
                    format!(
                        "Invalid SESSION_MAX value '{}': expected a positive integer",
                        raw
                    )
                })?
                .max(1),
            Err(_) => 10,
        };

        let port = match std::env::var("PORT") {
            Ok(raw) => raw
                .trim()
                .parse::<u16>()
                .map_err(|_| format!("Invalid PORT value '{}': expected 0-65535", raw))?,
            Err(_) => 8080,
        };
        let host = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0".to_string());
        let bind_addr = format!("{}:{}", host.trim(), port);

        Ok(Self {
            file,
            client_key,
            session_max,
            bind_addr,
        })
    }
}

/// 读取并解析 JSON 配置文件，字段级校验，任何问题返回带路径的错误信息。
pub fn load_file_config(path: &str) -> Result<FileConfig, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read config file '{}': {}", path, e))?;

    let parsed: RawFileConfig = serde_json::from_str(&raw)
        .map_err(|e| format!("Failed to parse config file '{}': {}", path, e))?;

    let mut models = Vec::new();
    if let Some(list) = parsed.models {
        for m in list {
            let m = m.trim().to_string();
            if !m.is_empty() {
                models.push(m);
            }
        }
    }

    let mut providers = HashMap::new();
    if let Some(map) = parsed.providers {
        for (id, raw_provider) in map {
            let id = id.trim().to_string();
            if id.is_empty() {
                return Err(format!(
                    "Config file '{}' contains an empty provider id",
                    path
                ));
            }
            let url = raw_provider.url.trim().trim_end_matches('/').to_string();
            if url.is_empty() {
                return Err(format!(
                    "Config file '{}': provider '{}' has an empty url",
                    path, id
                ));
            }
            let key = raw_provider.key.trim().to_string();
            if key.is_empty() {
                return Err(format!(
                    "Config file '{}': provider '{}' has an empty key",
                    path, id
                ));
            }
            let models_map = raw_provider.models.unwrap_or_default();
            providers.insert(
                id,
                ProviderConfig {
                    url,
                    key,
                    models: models_map,
                },
            );
        }
    }

    Ok(FileConfig { models, providers })
}

/// 生成 48 位十六进制字符（24 字节熵）的随机 API key，形如 `sk-<hex>`。
pub fn generate_client_key() -> String {
    let mut bytes = [0u8; 24];
    rand::rng().fill(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    format!("sk-{}", hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_config() {
        let dir = std::env::temp_dir().join(format!("gw_cfg_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("full.json");
        std::fs::write(
            &path,
            r#"{
  "models": ["deepseek-v4.1-flash", "  gpt-4o ", ""],
  "providers": {
    "provider_a": { "url": "https://api.a.com/v1/", "key": "sk-a" },
    "provider_b": {
      "url": "https://api.b.com",
      "key": "sk-b",
      "models": { "gpt-4o": "gpt-4o-internal" }
    }
  }
}"#,
        )
        .unwrap();

        let cfg = load_file_config(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.models, vec!["deepseek-v4.1-flash", "gpt-4o"]);
        assert_eq!(cfg.providers.len(), 2);
        let a = cfg.providers.get("provider_a").unwrap();
        assert_eq!(a.url, "https://api.a.com/v1");
        assert!(a.models.is_empty());
        let b = cfg.providers.get("provider_b").unwrap();
        assert_eq!(
            b.models.get("gpt-4o").map(String::as_str),
            Some("gpt-4o-internal")
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_error() {
        let err = load_file_config("/nonexistent/gateway-config.json").unwrap_err();
        assert!(err.contains("Failed to read config file"));
    }

    #[test]
    fn invalid_json_is_error() {
        let dir = std::env::temp_dir().join(format!("gw_cfg_bad_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.json");
        std::fs::write(&path, "{ not json }").unwrap();
        let err = load_file_config(path.to_str().unwrap()).unwrap_err();
        assert!(err.contains("Failed to parse config file"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn provider_missing_key_is_error() {
        let dir = std::env::temp_dir().join(format!("gw_cfg_nokey_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nokey.json");
        std::fs::write(
            &path,
            r#"{ "providers": { "a": { "url": "https://x.com" } } }"#,
        )
        .unwrap();
        let err = load_file_config(path.to_str().unwrap()).unwrap_err();
        assert!(err.contains("missing field `key`") || err.contains("Failed to parse"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_providers_and_models_allowed() {
        let dir = std::env::temp_dir().join(format!("gw_cfg_empty_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.json");
        std::fs::write(&path, "{}").unwrap();
        let cfg = load_file_config(path.to_str().unwrap()).unwrap();
        assert!(cfg.models.is_empty());
        assert!(cfg.providers.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generated_key_format() {
        let key = generate_client_key();
        assert!(key.starts_with("sk-"));
        assert_eq!(key.len(), 3 + 48);
        assert!(key[3..].bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(key, generate_client_key());
    }
}
