//! Configuration file parsing and env-var loading for the wecom CLI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use wecom::ClientBuilder;
use wecom::transport::{TransportBackend, TransportBuilder};

use crate::{Error, Result};

// ── ConfigFile ─────────────────────────────────────────────────

/// Schema for `~/.config/wecom/config.json`.
///
/// All fields are optional; only the ones present in the file will be applied.
/// `base_url` / `auth_endpoint` 仅在 `custom-endpoint` feature 下编译生效。
///
/// Example `config.json`:
/// ```json
/// {
///     "base_url": "https://custom.example.com/api/",
///     "auth_endpoint": "https://custom.example.com/auth",
///     "headers": {
///         "X-Custom": "value"
///     },
///     "tmp_dir": "/tmp/wecom-custom"
/// }
/// ```
///
/// Note: the access token is intentionally NOT configurable via `config.json`;
/// it comes from the encrypted credentials cache (`credentials.enc`).
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ConfigFile {
    /// Override the default base URL (`custom-endpoint` feature 下生效)。
    #[cfg(feature = "custom-endpoint")]
    pub base_url: Option<String>,
    /// Override the default auth bootstrap endpoint (`custom-endpoint` feature 下生效)。
    #[cfg(feature = "custom-endpoint")]
    pub auth_endpoint: Option<String>,
    /// Extra HTTP headers added to every request.
    #[serde(alias = "additional_headers")]
    pub headers: Option<HashMap<String, String>>,
    /// Override the temporary directory.
    pub tmp_dir: Option<String>,
}

// ── Path helpers ───────────────────────────────────────────────

/// Returns the default home directory for the wecom CLI.
///
/// Resolution order:
/// 1. `WECOM_CLI_CONFIG_DIR` env var
/// 2. `~/.config/wecom`
pub fn default_home_dir() -> PathBuf {
    std::env::var(crate::env::CONFIG_DIR)
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
                .join("wecom")
        })
}

/// Returns the default config file path.
///
/// Equivalent to [`default_home_dir()`] joined with `config.json`.
pub fn default_config_path() -> PathBuf {
    default_home_dir().join("config.json")
}

// ── Config file loading ────────────────────────────────────────

/// Try to load and parse a `ConfigFile` from the given path.
///
/// Returns `Ok(None)` if the file does not exist.
/// Returns `Err` if the file exists but cannot be read or parsed.
///
/// SAFETY: Config loading runs before `Fs` construction — no sandbox
/// is available yet, so direct `std::fs` is the only option.
#[allow(clippy::disallowed_methods)]
pub fn load_config_file(path: &Path) -> Result<Option<ConfigFile>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let cfg: ConfigFile = serde_json::from_str(&contents)
                .map_err(|e| {
                    Error::from(wecom::Error::Config(format!(
                        "Failed to parse config file {}: {e}",
                        path.display()
                    )))
                })
                .inspect_err(|e| tracing::error!(error = %e, "parse config file failed"))?;
            tracing::debug!(path = %path.to_string_lossy(), "Loading config file");
            Ok(Some(cfg))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(wecom::Error::io(
            format!("Failed to read config file {}", path.display()),
            e,
        )
        .into())
        .inspect_err(|e| tracing::error!(error = %e, "read config file failed")),
    }
}

// ── Builder helpers (moved from ClientBuilder) ─────────────────

/// Apply config-file and environment variable settings to a [`ClientBuilder`].
///
/// Config-file values are applied first; environment variables take precedence
/// and override them when both are present.
///
/// Only handles non-transport settings (paths).
/// Transport settings (base_url / auth_endpoint) must be configured via
/// [`crate::transport::build`] / [`crate::auth`].
pub fn apply_config(mut builder: ClientBuilder, cfg: &ConfigFile) -> Result<ClientBuilder> {
    // ── Config file ──
    if let Some(dir) = &cfg.tmp_dir {
        builder = builder.tmp_dir(dir);
    }

    // ── Environment variables (highest priority) ──
    if let Ok(v) = std::env::var(crate::env::CONFIG_DIR) {
        builder = builder.home_dir(v);
    }
    if let Ok(v) = std::env::var(crate::env::TMP_DIR) {
        builder = builder.tmp_dir(v);
    }
    Ok(builder)
}

// ── Resolution helpers ───────────────────────────────────────────

/// Resolve: non-empty env var > non-empty config value.
#[cfg(feature = "custom-endpoint")]
pub fn env_or_config(env_name: &str, cfg_val: Option<&str>) -> Option<String> {
    std::env::var(env_name)
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| cfg_val.filter(|v| !v.is_empty()).map(|s| s.to_owned()))
}

// ── Transport builder helpers ─────────────────────────────────────

/// Apply transport-agnostic config (headers, CLI info) to a
/// [`TransportBuilder`], regardless of its backend type.
///
/// The `Authorization` token is intentionally NOT baked here: it is injected
/// per-request by the transport backend when the endpoint carries the
/// [`AuthRequirement`](crate::transport::capability::AuthRequirement) capability
/// (see [`crate::transport::backend::WecomBackend`]).
///
/// All header validation errors are deferred to [`TransportBuilder::build`].
pub fn apply_transport_config<B: TransportBackend + 'static>(
    mut builder: TransportBuilder<B>,
    cfg: &ConfigFile,
) -> Result<TransportBuilder<B>> {
    // ── Config file (headers only; token is not configurable via config.json) ──
    if let Some(headers) = &cfg.headers {
        for (k, v) in headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
    }

    let prefix = format!("{}_", crate::env::ADDITIONAL_HEADERS);
    for (key, value) in std::env::vars() {
        if key != crate::env::ADDITIONAL_HEADERS && !key.starts_with(&prefix) {
            continue;
        }
        if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&value) {
            for (k, v) in map {
                builder = builder.header(k, v);
            }
        }
    }

    Ok(builder)
}

#[cfg(test)]
mod tests {
    //! ## 模块摘要：config（CLI 配置文件解析与环境变量加载）
    //!
    //! ### 关键接口
    //! - [ConfigFile] — 配置文件 `~/.config/wecom/config.json` 的 serde 反序列化目标，字段全部可选
    //! - [default_config_path] — 返回默认配置文件路径
    //! - [load_config_file] — 从文件路径加载并解析 ConfigFile
    //!
    //! ### 关键分支与异常路径
    //! - ConfigFile 反序列化：完整 JSON → 全部字段解析为 Some；空 JSON → 全 None；含未知字段 → 忽略
    //! - load_config_file：文件存在且合法 JSON → Ok(Some(cfg))；文件不存在 → Ok(None)；非法 JSON → Err(Error::Config)
    //!
    //! ### 上下游交互
    //! - 上游：CLI 入口（`main.rs`），在构造 `Client` 前加载配置
    //! - 下游：`wecom::ClientBuilder`（通过 `apply_config` 应用配置项与环境变量）

    use serde_json::json;

    use super::*;

    // ========== ConfigFile 反序列化 ==========

    /// P0：[ConfigFile] 完整 ConfigFile JSON 反序列化
    /// 条件：JSON 包含全部字段（base_url / auth_endpoint / headers / tmp_dir）
    /// 断言：所有字段均为 Some 且值正确；access_token 字段被静默忽略（不允许经 config.json 配置）
    #[test]
    fn config_file_deserialize_full() {
        let raw = json!({
            "base_url": "https://api.example.com/",
            "auth_endpoint": "https://api.example.com/auth",
            "access_token": "tok123",
            "headers": {"X-Custom": "val"},
            "tmp_dir": "/tmp/my-wecom"
        });
        let cfg: ConfigFile = serde_json::from_value(raw).unwrap();
        #[cfg(feature = "custom-endpoint")]
        assert_eq!(cfg.base_url, Some("https://api.example.com/".to_string()));
        #[cfg(feature = "custom-endpoint")]
        assert_eq!(
            cfg.auth_endpoint,
            Some("https://api.example.com/auth".to_string())
        );
        assert!(cfg.headers.is_some());
        assert_eq!(cfg.tmp_dir, Some("/tmp/my-wecom".to_string()));
    }

    /// P0：[ConfigFile] 空对象反序列化为全 None
    /// 条件：JSON 为 {}
    /// 断言：所有字段均为 None
    #[test]
    fn config_file_deserialize_empty_is_default() {
        let cfg: ConfigFile = serde_json::from_value(json!({})).unwrap();
        #[cfg(feature = "custom-endpoint")]
        assert!(cfg.base_url.is_none());
        #[cfg(feature = "custom-endpoint")]
        assert!(cfg.auth_endpoint.is_none());
        assert!(cfg.headers.is_none());
        assert!(cfg.tmp_dir.is_none());
    }

    /// P1：[ConfigFile] 反序列化时忽略未知字段
    /// 条件：JSON 包含已知字段 base_url 和未知字段 unknown_field
    /// 断言：base_url 正确解析，未知字段被静默忽略
    #[test]
    fn config_file_deserialize_unknown_fields_ignored() {
        let raw = json!({
            "base_url": "http://a.com",
            "unknown_field": "ignored"
        });
        #[cfg_attr(not(feature = "custom-endpoint"), allow(unused_variables))]
        let cfg: ConfigFile = serde_json::from_value(raw).unwrap();
        #[cfg(feature = "custom-endpoint")]
        assert_eq!(cfg.base_url, Some("http://a.com".to_string()));
    }

    // ========== default_config_path ==========

    /// P0：[default_config_path] 以 config.json 结尾
    /// 条件：调用 default_config_path()
    /// 断言：返回路径的 file_name() == "config.json"
    #[test]
    fn default_config_path_ends_with_config_json() {
        let path = default_config_path();
        assert!(
            path.file_name().unwrap() == "config.json",
            "expected config.json, got {:?}",
            path.file_name()
        );
    }

    // ========== load_config_file ==========

    /// P0：[load_config_file] 加载不存在的配置文件返回 None
    /// 条件：传入不存在的文件路径
    /// 断言：返回 Ok(None)
    #[test]
    fn load_config_file_nonexistent_returns_none() {
        let result = load_config_file(PathBuf::from("/nonexistent/path/to/config.json").as_path());
        assert!(result.unwrap().is_none());
    }

    /// P0：[load_config_file] 加载有效 JSON 正确解析
    /// 条件：临时文件写入有效 JSON（base_url + tmp_dir）
    /// 断言：返回 Ok(Some(cfg))，tmp_dir 值与写入一致
    #[test]
    #[allow(clippy::disallowed_methods)]
    fn load_config_file_valid_json_returns_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"base_url": "https://custom.test", "tmp_dir": "/tmp/custom"}"#,
        )
        .unwrap();

        let result = load_config_file(&path).unwrap();
        assert!(result.is_some());
        let cfg = result.unwrap();
        #[cfg(feature = "custom-endpoint")]
        assert_eq!(cfg.base_url, Some("https://custom.test".to_string()));
        assert_eq!(cfg.tmp_dir, Some("/tmp/custom".to_string()));
    }

    /// P1：[load_config_file] 加载非法 JSON 返回错误
    /// 条件：临时文件写入非法 JSON "{invalid json!!!"
    /// 断言：返回 Err(Error::Config(_))
    #[test]
    #[allow(clippy::disallowed_methods)]
    fn load_config_file_invalid_json_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "{invalid json!!!").unwrap();

        let result = load_config_file(&path);
        assert!(result.is_err());
        match result.unwrap_err() {
            Error::Wecom(wecom::Error::Config(_)) => {}
            other => panic!("Expected Config error for invalid JSON, got {other:?}"),
        }
    }
}
