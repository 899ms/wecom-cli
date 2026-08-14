use serde_json::json;
use thiserror::Error;
use wecom_transport::trunc_display;

// Transport error codes (E_NETWORK / E_HTTP / E_PARSE) are owned
// by the `wecom-transport` crate. This crate delegates to them via
// `Error::Transport`.
//
// Error code range: 893000 - 893999, this crate uses 893000 - 893099.

/// Input validation error code (missing required field, empty method path, etc.).
pub const E_VALIDATION: i64 = 893001;
// Method / service not found error code.
pub const E_SUBCMD: i64 = 893002;
/// Filesystem I/O error code.
pub const E_IO: i64 = 893003;
/// CLI output error code (help, version, usage error).
pub const E_CLI: i64 = 893004;
/// Client / builder configuration error code.
pub const E_CONFIG_CLIENT: i64 = 893005;
/// Permission denied error code (sandbox path violation).
pub const E_PERMISSION: i64 = 893006;
/// Catch-all error code for wecom-layer failures.
pub const E_OTHER: i64 = 893999;

/// 后台接口返回该错误码时，视为参数/用法错误并展示当前命令的 help。
///
/// 该错误码表示「参数/用法问题」（如存在未知字段），`CliRun::execute` 会
/// 渲染当前叶子子命令的 help，并以 [`Error::CliOutput`]（exit code 2）返回，
/// 与正常 help/用法错误走同一条处理路径。
pub const ERRCODE_SHOW_HELP: i64 = 10021;

/// Errors returned by the wecom library.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// Transport-layer error (network, HTTP, parse, API, other).
    Transport(#[from] wecom_transport::Error),

    /// Input validation failed (e.g. missing required field, empty method path).
    Validation(String),

    /// Client / builder configuration error (e.g. invalid access token,
    /// unknown transport type, malformed config file).
    Config(String),

    /// Permission denied (e.g. path outside sandbox roots).
    Permission(String),

    /// I/O errors (filesystem, temp files, etc.).
    Io {
        message: String,
        #[source]
        source: std::io::Error,
    },

    /// Pre-rendered CLI output (e.g. `--help`, `--version`, or usage error).
    ///
    /// The lib never writes this itself — callers decide how to display it.
    /// - `code` is `0` for help / version, `2` for usage errors.
    /// - `message` is the already-rendered, ready-to-display text (ANSI-colored
    ///   when the caller enabled `force_color`).
    /// - `source` carries the original [`clap::Error`] for usage errors so that
    ///   downstream code can introspect `ErrorKind` / context if needed; it is
    ///   `None` for non-clap-originated CLI output.
    CliOutput {
        code: i32,
        message: String,
        #[source]
        source: Option<clap::Error>,
    },

    /// Catch-all for errors that don't fit other variants.
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io {
            message: e.to_string(),
            source: e,
        }
    }
}

impl Error {
    /// Create an `Io` variant with the `context` followed by the error reason.
    ///
    /// Produces messages like `"Failed to open /path: No such file (os error 2)"`.
    #[must_use]
    pub fn io(context: impl std::fmt::Display, source: std::io::Error) -> Self {
        Error::Io {
            message: format!("{context}: {source}"),
            source,
        }
    }

    /// Convert this error into a structured JSON [`Value`].
    ///
    /// - `Transport` → delegates to [`wecom_transport::Error::to_json`]
    ///   (each inner variant produces its own JSON shape; `Api` returns the
    ///   raw server body).
    /// - `CliOutput` → `{"error": {"code": …, "message": …}}`.
    /// - All other variants → structured JSON with `type`, `message`, `code`.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Error::Transport(inner) => inner.to_json(),

            Error::CliOutput {
                code,
                message,
                source,
                ..
            } => json!({
                "error": {
                    "type": "CliOutput",
                    "code": self.code(),
                    "message": wecom_transport::trunc_display(message, 100),
                    "kind": source.as_ref().and_then(|e| e.kind().as_str()),
                    "exit_code": code,
                },
            }),

            Error::Validation(message) => json!({
                "error": {
                    "type": "ValidationError",
                    "code": self.code(),
                    "message": message,
                },
            }),

            Error::Config(message) => json!({
                "error": {
                    "type": "ConfigError",
                    "code": self.code(),
                    "message": message,
                },
            }),

            Error::Permission(message) => json!({
                "error": {
                    "type": "PermissionError",
                    "code": self.code(),
                    "message": message,
                },
            }),

            Error::Io { message, source } => json!({
                "error": {
                    "type": "IOError",
                    "code": self.code(),
                    "message": message,
                    "kind": format!("{:?}", source.kind()),
                },
            }),

            Error::Other(e) => json!({
                "error": {
                    "type": "UnknownError",
                    "code": self.code(),
                    "message": e.to_string(),
                },
            }),
        }
    }

    /// Render the error as a ready-to-display string.
    ///
    /// - `Transport` → delegates to [`wecom_transport::Error::render`]
    ///   (structured JSON per inner variant; `Api` returns the raw body).
    /// - `CliOutput` → returns the pre-rendered `message` as-is
    ///   (the `source` clap error is intentionally ignored — the rendered
    ///   text already contains all user-facing information).
    /// - All other variants → pretty-printed JSON via [`Error::to_json`].
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Error::Transport(inner) => return inner.render(),
            Error::CliOutput { message, .. } => return message.clone(),
            _ => {}
        }
        serde_json::to_string_pretty(&self.to_json()).unwrap_or_else(|_| self.to_string())
    }

    /// Suggested process exit code.
    ///
    /// - `CliOutput` → its `code` field (`0` for help/version, `2` for usage error).
    /// - All other variants → `1`.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::CliOutput { code, .. } => *code,
            _ => 1,
        }
    }

    /// Category error code for this variant.
    ///
    /// Returns one of the `E_*` constants. For [`Error::Transport`] this
    /// delegates to [`wecom_transport::Error::code`], which maps each inner
    /// variant to `E_NETWORK` / `E_HTTP` / `E_PARSE` /
    /// `E_OTHER`. For [`wecom_transport::Error::Api`] this passes through
    /// the backend error code directly (defaults to 0).
    #[must_use]
    pub fn code(&self) -> i64 {
        match self {
            Error::Transport(inner) => inner.code(),
            Error::Validation(_) => E_VALIDATION,
            Error::Permission(_) => E_PERMISSION,
            Error::Config(_) => E_CONFIG_CLIENT,
            Error::Io { .. } => E_IO,
            Error::CliOutput { source, .. } => match source.as_ref().map(|e| e.kind()) {
                Some(clap::error::ErrorKind::InvalidSubcommand) => E_SUBCMD,
                _ => E_CLI,
            },
            Error::Other(_) => E_OTHER,
        }
    }

    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Error::Transport(inner) => inner.message(),
            Error::Validation(message) => message.clone(),
            Error::Permission(message) => message.clone(),
            Error::Config(message) => message.clone(),
            Error::Io { message, .. } => message.clone(),
            Error::CliOutput { message, .. } => message.clone(),
            Error::Other(e) => e.to_string(),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = self.code();
        match self {
            Error::Transport(inner) => write!(f, "{inner}"),
            Error::Validation(msg) => {
                write!(f, "ValidationError: {msg} [code={code}]")
            }
            Error::Config(msg) => {
                write!(f, "ConfigError: {msg} [code={code}]")
            }
            Error::Permission(msg) => {
                write!(f, "PermissionError: {msg} [code={code}]")
            }
            Error::Io { message, source } => {
                let kind = format!("{:?}", source.kind());
                write!(f, "IoError: {message} [code={code}, kind={kind}]")
            }
            Error::CliOutput {
                code: exit_code,
                message,
                source,
            } => {
                let message = trunc_display(message, 100);
                let kind = source.as_ref().and_then(|e| e.kind().as_str());
                let kind_display = kind.unwrap_or("?");
                write!(
                    f,
                    "CliOutput: {message} [code={code}, exit={exit_code}, kind={kind_display}]"
                )
            }
            Error::Other(e) => {
                write!(f, "UnknownError: {e} [code={code}]")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! ## 模块摘要：Error（统一错误类型）
    //!
    //! ### 关键接口
    //! - [Error::to_json] — 将错误转换为结构化 JSON Value（Transport 委托
    //!   wecom_transport::Error::to_json）
    //! - [Error::render] — 将错误渲染为可展示字符串（JSON 或预渲染消息）；
    //!   对 JSON 变体内部调用 [to_json] 后格式化
    //! - [Error::exit_code] — 返回建议的进程退出码（CliOutput 用自身 code，其余为 1）
    //! - [Error::code] — 返回该错误对应的 893xxx 分类码（Transport 子变体映射到 E_NETWORK/E_HTTP/E_PARSE/E_OTHER，Api 直接透传后台错误码）
    //! - `From<std::io::Error> for Error` — 将 io::Error 自动转换为 Error::Io 变体
    //!
    //! ### 关键分支与异常路径
    //! - to_json：Transport 委托内层；CliOutput 返回结构化 JSON（含 code/type/字段）
    //! - render：Transport 委托内层 render；CliOutput 直接返回预渲染 message（忽略 source）；其余调用 to_json → to_string_pretty
    //! - exit_code：CliOutput 返回 code 字段，其他变体统一返回 1
    //! - code：Transport(Api) 直接透传后台错误码（无则默认 0）；未知 Transport 变体兜底 E_OTHER
    //! - From impl：io::Error 包装为 Error::Io { message, source }
    //! - Validation：CLI 用户输入校验失败；Config：ClientBuilder 配置 / 环境变量 / 配置文件格式错误
    //!
    //! ### 上下游交互
    //! - 上游：整个 wecom crate 各模块通过 `?` 操作符产生 Error；CliOutput 由 [crate::client::run] 在 clap 解析失败时构造，并把原始 `clap::Error` 放入 `source`
    //! - 下游：依赖 wecom_transport::Error（Transport 变体）、std::io::Error（Io 变体）、clap::Error（CliOutput.source 字段）

    use assert_json_diff::assert_json_eq;
    use serde_json::Value;
    // 传输层分类码由 transport crate 拥有，测试断言直接从其引入。
    use wecom_transport::{E_HTTP, E_PARSE};

    use super::*;

    // ── render() ──

    /// P0：Validation 错误的 render 输出包含正确的类型、消息和错误码
    /// 条件：创建 Error::Validation("field is required")
    /// 断言：JSON 结构为 {"error": {"type":"ValidationError","code":893001,"message":"field is required"}}
    #[test]
    fn render_validation() {
        let e = Error::Validation("field is required".into());
        let v: Value = serde_json::from_str(&e.render()).unwrap();
        assert_json_eq!(
            v,
            json!({
                "error": {
                    "type": "ValidationError",
                    "code": E_VALIDATION,
                    "message": "field is required"
                }
            })
        );
    }

    /// P0：Config 错误的 render 输出包含正确的类型、消息和错误码
    /// 条件：创建 Error::Config("invalid transport type")
    /// 断言：JSON 结构为 {"error": {"type":"ConfigError","code":893005,"message":"invalid transport type"}}
    #[test]
    fn render_config() {
        let e = Error::Config("invalid transport type".into());
        let v: Value = serde_json::from_str(&e.render()).unwrap();
        assert_json_eq!(
            v,
            json!({
                "error": {
                    "type": "ConfigError",
                    "code": E_CONFIG_CLIENT,
                    "message": "invalid transport type"
                }
            })
        );
    }

    /// P0：Permission 错误的 render 输出包含正确的类型、消息和错误码
    /// 条件：创建 Error::Permission("路径超出沙箱")
    /// 断言：JSON 结构为 {"error": {"type":"PermissionError","code":893006,"message":"路径超出沙箱"}}
    #[test]
    fn render_permission() {
        let e = Error::Permission("路径超出沙箱".into());
        let v: Value = serde_json::from_str(&e.render()).unwrap();
        assert_json_eq!(
            v,
            json!({
                "error": {
                    "type": "PermissionError",
                    "code": E_PERMISSION,
                    "message": "路径超出沙箱"
                }
            })
        );
    }

    /// P1：网络错误（Transport::Other）的 render 输出包含原始消息
    /// 条件：创建 Transport::Error::Other("connection refused")
    /// 断言：render 结果的 message 字段匹配 "connection refused"
    #[test]
    fn render_network() {
        let e = Error::Transport(wecom_transport::Error::Other("connection refused".into()));
        let v: Value = serde_json::from_str(&e.render()).unwrap();
        assert_json_eq!(
            v["error"]["message"],
            serde_json::json!("connection refused")
        );
    }

    /// P1：HTTP 错误的 render 输出包含类型、状态码和 endpoint
    /// 条件：创建 Transport::Error::Http，status=404，endpoint 为 example.com/api
    /// 断言：JSON 结构为 {"error":{"type":"HTTPError","code":893102,"message":"not found","endpoint":"https://example.com/api","status":404}}
    #[test]
    fn render_http() {
        let e = Error::Transport(wecom_transport::Error::Http {
            message: "not found".into(),
            endpoint: "https://example.com/api".into(),
            status: 404,
        });
        let v: Value = serde_json::from_str(&e.render()).unwrap();
        assert_json_eq!(
            v,
            json!({
                "error": {
                    "type": "HTTPError",
                    "code": E_HTTP,
                    "message": "not found",
                    "endpoint": "https://example.com/api",
                    "status": 404
                }
            })
        );
    }

    /// P1：API 错误的 render 直接返回原始响应体
    /// 条件：创建 Transport::Error::Api，body 含 errcode 和 errmsg
    /// 断言：render 结果等于原始 body JSON
    #[test]
    fn render_api_returns_body() {
        let body = serde_json::json!({"errcode":40001,"errmsg":"invalid credential"});
        let e = Error::Transport(wecom_transport::Error::Api {
            message: "invalid credential".into(),
            action: "test".into(),
            code: Some(40001),
            body: Box::new(body.clone()),
        });
        let rendered: Value = serde_json::from_str(&e.render()).unwrap();
        assert_json_eq!(rendered, body);
    }

    /// P0：IO 错误的 render 输出包含 type、message 和 kind
    /// 条件：创建 Error::Io，message 为 "disk full"
    /// 断言：JSON 中 type 为 IOError，message 和 kind 匹配
    #[test]
    fn render_io() {
        let e = Error::Io {
            message: "disk full".into(),
            source: std::io::Error::other("disk full"),
        };
        let v: Value = serde_json::from_str(&e.render()).unwrap();
        assert_json_eq!(v["error"]["type"], serde_json::json!("IOError"));
        assert_json_eq!(v["error"]["message"], serde_json::json!("disk full"));
        assert_json_eq!(v["error"]["kind"], serde_json::json!("Other"));
    }

    /// P1：解析错误的 render 输出包含类型、错误码、消息、endpoint 和 body
    /// 条件：创建 Transport::Error::Parse，消息为 "missing field 'media_id'"
    /// 断言：JSON 结构含 type=ParseError, code=893103, endpoint, body
    #[test]
    fn render_parse() {
        let e = Error::Transport(wecom_transport::Error::Parse {
            message: "missing field 'media_id'".into(),
            endpoint: "test".into(),
            body: Box::new(serde_json::json!({"unexpected":"data"})),
            source: None,
        });
        let v: Value = serde_json::from_str(&e.render()).unwrap();
        assert_json_eq!(
            v,
            json!({
                "error": {
                    "type": "ParseError",
                    "code": E_PARSE,
                    "message": "missing field 'media_id'",
                    "endpoint": "test",
                    "body": {"unexpected": "data"}
                }
            })
        );
    }

    /// P1：Transport::Other 错误的 render 输出含 type=UnknownError
    /// 条件：创建 Transport::Error::Other("something went wrong")
    /// 断言：message 匹配，且 type 为 UnknownError
    #[test]
    fn render_other() {
        let e = Error::Transport(wecom_transport::Error::Other("something went wrong".into()));
        let v: Value = serde_json::from_str(&e.render()).unwrap();
        assert_json_eq!(
            v["error"]["message"],
            serde_json::json!("something went wrong")
        );
        assert_json_eq!(v["error"]["type"], serde_json::json!("UnknownError"));
    }

    /// P1：CliOutput 错误直接返回预渲染消息
    /// 条件：创建 Error::CliOutput 含 code=0 和 version 消息
    /// 断言：render 直接返回原始 message 字符串
    #[test]
    fn render_cli_output_returns_message_as_is() {
        let e = Error::CliOutput {
            code: 0,
            message: "wecom 1.0.0".into(),
            source: None,
        };
        assert_eq!(e.render(), "wecom 1.0.0");
    }

    /// P1：[Error::render] 对 CliOutput 仅返回预渲染 message，忽略 source 中的 clap::Error
    /// 条件：构造一个携带 `Some(clap::Error)` 的 Error::CliOutput，message 为预渲染文本
    /// 断言：render 输出严格等于 message（不混入 clap::Error 文案），且 std::error::Error::source 暴露原 clap 错误
    #[test]
    fn render_cli_output_ignores_clap_source() {
        use std::error::Error as _;
        let clap_err = clap::Error::raw(clap::error::ErrorKind::InvalidValue, "raw clap message\n");
        let e = Error::CliOutput {
            code: 2,
            message: "pre-rendered usage error".into(),
            source: Some(clap_err),
        };
        assert_eq!(e.render(), "pre-rendered usage error");
        // source 应可被 std::error::Error::source 透出，便于上层自省 ErrorKind
        let src = e.source().expect("CliOutput.source should be exposed");
        assert!(src.is::<clap::Error>());
    }

    // ── exit_code() ──

    /// P0：CliOutput 错误返回其自身携带的退出码
    /// 条件：分别创建 code=0（帮助/版本）和 code=2（用法错误）的 CliOutput
    /// 断言：exit_code 分别为 0 和 2
    #[test]
    fn exit_code_cli_output() {
        assert_eq!(
            Error::CliOutput {
                code: 0,
                message: String::new(),
                source: None,
            }
            .exit_code(),
            0
        );
        assert_eq!(
            Error::CliOutput {
                code: 2,
                message: String::new(),
                source: None,
            }
            .exit_code(),
            2
        );
    }

    /// P1：[Error::exit_code] 非 CliOutput 错误统一返回退出码 1
    /// 条件：分别创建 Validation 和 Transport::Other 错误
    /// 断言：exit_code 均为 1
    #[test]
    fn exit_code_non_cli_output_is_1() {
        assert_eq!(Error::Validation("x".into()).exit_code(), 1);
        assert_eq!(
            Error::Transport(wecom_transport::Error::Other("x".into())).exit_code(),
            1
        );
    }

    // ── code() ──

    /// P0：[Error::code] 顶层非 Transport 变体返回各自的分类码
    /// 条件：分别构造 Validation / Permission / Config / Io / CliOutput / Other
    /// 断言：code() 分别返回对应分类码
    #[test]
    fn code_top_level_variants() {
        assert_eq!(Error::Validation("x".into()).code(), E_VALIDATION);
        assert_eq!(Error::Permission("x".into()).code(), E_PERMISSION);
        assert_eq!(Error::Config("x".into()).code(), E_CONFIG_CLIENT);
        assert_eq!(
            Error::Io {
                message: "x".into(),
                source: std::io::Error::other("x"),
            }
            .code(),
            E_IO
        );
        assert_eq!(
            Error::CliOutput {
                code: 0,
                message: String::new(),
                source: None,
            }
            .code(),
            E_CLI
        );
        assert_eq!(Error::Other("x".into()).code(), E_OTHER);
    }

    /// P0：[Error::code] Transport 子变体映射到正确分类码
    /// 条件：分别构造 Transport(Http) / Transport(Parse) / Transport(Api) / Transport(Other)
    /// 断言：code() 分别返回 E_HTTP / E_PARSE / 透传后台错误码 / E_OTHER
    #[test]
    fn code_transport_variants() {
        assert_eq!(
            Error::Transport(wecom_transport::Error::Http {
                message: "x".into(),
                endpoint: "http://e".into(),
                status: 500,
            })
            .code(),
            E_HTTP
        );
        assert_eq!(
            Error::Transport(wecom_transport::Error::Parse {
                message: "x".into(),
                endpoint: "/e".into(),
                body: Box::new(serde_json::Value::Null),
                source: None,
            })
            .code(),
            E_PARSE
        );
        // Api should pass through the backend error code directly.
        assert_eq!(
            Error::Transport(wecom_transport::Error::Api {
                message: "x".into(),
                action: "/a".into(),
                code: Some(40001),
                body: Box::new(serde_json::Value::Null),
            })
            .code(),
            40001
        );
        // Api with no code defaults to 0.
        assert_eq!(
            Error::Transport(wecom_transport::Error::Api {
                message: "x".into(),
                action: "/a".into(),
                code: None,
                body: Box::new(serde_json::Value::Null),
            })
            .code(),
            0
        );
        assert_eq!(
            Error::Transport(wecom_transport::Error::Other("x".into())).code(),
            E_OTHER
        );
    }

    // ── From impls ──

    /// P0：std::io::Error 到 Error::Io 的 From 转换
    /// 条件：创建 NotFound 类型的 io::Error 并通过 .into() 转换
    /// 断言：转换结果匹配 Error::Io 变体，message 为原 io::Error 消息
    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let e: Error = io_err.into();
        assert!(matches!(e, Error::Io { .. }));
        assert_eq!(e.exit_code(), 1);
        assert_eq!(e.message(), "no such file");
    }

    // ── to_json() ──

    /// P0：[Error::to_json] Validation 错误返回正确的 type、message 和 code
    /// 条件：Error::Validation("field is required")
    /// 断言：to_json() 含 type=ValidationError、code=E_VALIDATION、message 透传
    #[test]
    fn to_json_validation() {
        let e = Error::Validation("field is required".into());
        assert_eq!(
            e.to_json(),
            json!({
                "error": {
                    "type": "ValidationError",
                    "code": E_VALIDATION,
                    "message": "field is required"
                }
            })
        );
    }

    /// P0：[Error::to_json] Config 错误返回正确的 type、message 和 code
    /// 条件：Error::Config("invalid transport type")
    /// 断言：to_json() 含 type=ConfigError、code=E_CONFIG_CLIENT、message 透传
    #[test]
    fn to_json_config() {
        let e = Error::Config("invalid transport type".into());
        assert_eq!(
            e.to_json(),
            json!({
                "error": {
                    "type": "ConfigError",
                    "code": E_CONFIG_CLIENT,
                    "message": "invalid transport type"
                }
            })
        );
    }

    /// P0：[Error::to_json] Permission 错误返回正确的 type、message 和 code
    /// 条件：Error::Permission("路径超出沙箱")
    /// 断言：to_json() 含 type=PermissionError、code=E_PERMISSION、message 透传
    #[test]
    fn to_json_permission() {
        let e = Error::Permission("路径超出沙箱".into());
        assert_eq!(
            e.to_json(),
            json!({
                "error": {
                    "type": "PermissionError",
                    "code": E_PERMISSION,
                    "message": "路径超出沙箱"
                }
            })
        );
    }

    /// P0：[Error::to_json] IO 错误返回正确的 type、message、code 和 kind
    /// 条件：Error::Io { message:"disk full", source: io::Error::other("disk full") }
    /// 断言：to_json() 含 type=IOError、code=E_IO、message 与 kind=Other
    #[test]
    fn to_json_io() {
        let e = Error::Io {
            message: "disk full".into(),
            source: std::io::Error::other("disk full"),
        };
        assert_eq!(
            e.to_json(),
            json!({
                "error": {
                    "type": "IOError",
                    "code": E_IO,
                    "message": "disk full",
                    "kind": "Other",
                }
            })
        );
    }

    /// P1：[Error::to_json] Other 错误返回正确的 code、message 和 type
    /// 条件：Error::Other("something went wrong")
    /// 断言：to_json() 含 type=UnknownError、code=E_OTHER、message 透传
    #[test]
    fn to_json_other() {
        let e = Error::Other("something went wrong".into());
        assert_eq!(
            e.to_json(),
            json!({
                "error": {
                    "type": "UnknownError",
                    "code": E_OTHER,
                    "message": "something went wrong"
                }
            })
        );
    }

    /// P1：[Error::to_json] CliOutput 返回 code、message 和 type
    /// 条件：Error::CliOutput { code:2, message:"usage error", source:None }
    /// 断言：to_json() 含 type=CliOutput、code=E_CLI、exit_code=2、message、kind=null
    #[test]
    fn to_json_cli_output() {
        let e = Error::CliOutput {
            code: 2,
            message: "usage error".into(),
            source: None,
        };
        assert_eq!(
            e.to_json(),
            json!({
                "error": {
                    "type": "CliOutput",
                    "code": E_CLI,
                    "exit_code": 2,
                    "message": "usage error",
                    "kind": null
                }
            })
        );
    }

    /// P1：[Error::to_json] Transport(Http) 委托内层 to_json，保留 type=HTTPError
    /// 条件：Transport(wecom_transport::Error::Http { status:404, endpoint:"https://example.com/api" })
    /// 断言：to_json() 含 type=HTTPError、code=E_HTTP、status=404、endpoint 透传
    #[test]
    fn to_json_transport_http() {
        let e = Error::Transport(wecom_transport::Error::Http {
            message: "not found".into(),
            endpoint: "https://example.com/api".into(),
            status: 404,
        });
        assert_eq!(
            e.to_json(),
            json!({
                "error": {
                    "type": "HTTPError",
                    "code": E_HTTP,
                    "message": "not found",
                    "endpoint": "https://example.com/api",
                    "status": 404
                }
            })
        );
    }

    /// P1：[Error::to_json] Transport(Api) 委托内层 to_json，返回原始 body
    /// 条件：Transport(wecom_transport::Error::Api { code:40001, body:{errcode,errmsg} })
    /// 断言：to_json() 直接等于原始 body（无 {error:{}} 包裹）
    #[test]
    fn to_json_transport_api_returns_body() {
        let body = serde_json::json!({"errcode": 40001, "errmsg": "invalid credential"});
        let e = Error::Transport(wecom_transport::Error::Api {
            message: "invalid credential".into(),
            action: "test".into(),
            code: Some(40001),
            body: Box::new(body.clone()),
        });
        let rendered = e.to_json();
        assert_eq!(rendered, body);
    }

    // ── CliOutput to_json truncation ──

    /// P1：[Error::to_json] CliOutput 多行消息只保留首行并标记
    /// 条件：Error::CliOutput，message = "usage error\nmore details\nand more"
    /// 断言：to_json 中 message 字段为 "usage error\n[TRUNC]"
    #[test]
    fn to_json_cli_output_trunc_multiline() {
        let e = Error::CliOutput {
            code: 2,
            message: "usage error\nmore details\nand more".into(),
            source: None,
        };
        assert_eq!(
            e.to_json(),
            json!({
                "error": {
                    "type": "CliOutput",
                    "code": E_CLI,
                    "exit_code": 2,
                    "message": "usage error\n[TRUNC]",
                    "kind": null
                }
            })
        );
    }

    /// P1：[Error::to_json] CliOutput 单行超 100 字符消息截断并标记
    /// 条件：Error::CliOutput，message = "X"×101（单行）
    /// 断言：message 以 [TRUNC] 结尾，总长度为 100 + len("[TRUNC]")
    #[test]
    fn to_json_cli_output_trunc_long() {
        let long = "X".repeat(101);
        let e = Error::CliOutput {
            code: 2,
            message: long,
            source: None,
        };
        let v = e.to_json();
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.ends_with("[TRUNC]"));
        assert_eq!(msg.len(), 100 + "[TRUNC]".len());
        assert!(v["error"]["kind"].is_null());
    }

    // ── render() for top-level Other ──

    /// P1：[Error::render] 顶层 Error::Other 输出包含 type=UnknownError
    /// 条件：构造 Error::Other("unexpected failure")
    /// 断言：render 输出 JSON 含 UnknownError type 和正确的 code
    #[test]
    fn render_top_level_other() {
        let e = Error::Other("unexpected failure".into());
        let v: Value = serde_json::from_str(&e.render()).unwrap();
        assert_json_eq!(v["error"]["type"], serde_json::json!("UnknownError"));
        assert_json_eq!(
            v["error"]["message"],
            serde_json::json!("unexpected failure")
        );
        assert_json_eq!(v["error"]["code"], serde_json::json!(E_OTHER));
    }

    // ── message() ──

    /// P0：[Error::message] 所有变体返回正确的 message 字符串
    /// 条件：分别构造 Validation / Config / Permission / Io / CliOutput / Other
    /// 断言：各变体 message() 均返回构造时的 message
    #[test]
    fn message_all_variants() {
        assert_eq!(
            Error::Validation("invalid input".into()).message(),
            "invalid input"
        );
        assert_eq!(Error::Config("bad config".into()).message(), "bad config");
        assert_eq!(Error::Permission("denied".into()).message(), "denied");
        assert_eq!(
            Error::Io {
                message: "disk error".into(),
                source: std::io::Error::other("e"),
            }
            .message(),
            "disk error"
        );
        assert_eq!(
            Error::CliOutput {
                code: 0,
                message: "cli msg".into(),
                source: None,
            }
            .message(),
            "cli msg"
        );
        assert_eq!(Error::Other("other error".into()).message(), "other error");
    }

    /// P1：[Error::message] Transport 变体委托内层 message
    /// 条件：Transport(wecom_transport::Error::Http { message:"not found", status:404 })
    /// 断言：message() == "not found"
    #[test]
    fn message_transport_variant() {
        let e = Error::Transport(wecom_transport::Error::Http {
            message: "not found".into(),
            endpoint: "/api".into(),
            status: 404,
        });
        assert_eq!(e.message(), "not found");
    }

    // ── Error::io() 构造器 ──

    /// P0：[Error::io] 便利构造器生成带 context 前缀的 Io 变体
    /// 条件：Error::io("Failed to open /path", io::Error::new(NotFound, "no such file"))
    /// 断言：匹配 Error::Io；message()=="Failed to open /path: no such file"；code()==E_IO
    #[test]
    fn io_constructor_with_context() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let e = Error::io("Failed to open /path", io_err);
        assert!(matches!(e, Error::Io { .. }));
        assert_eq!(e.message(), "Failed to open /path: no such file");
        assert_eq!(e.code(), E_IO);
    }

    // ── Display impl ──

    /// P0：[Error::Display] Validation 变体格式化包含类型名、消息和错误码
    /// 条件：Error::Validation("field required")
    /// 断言：Display 含 "ValidationError"、"field required"、code=893001
    #[test]
    fn display_validation() {
        let e = Error::Validation("field required".into());
        let s = format!("{e}");
        assert!(s.contains("ValidationError"));
        assert!(s.contains("field required"));
        assert!(s.contains("code=893001"));
    }

    /// P0：[Error::Display] Config 变体格式化包含类型名、消息和错误码
    /// 条件：Error::Config("bad transport")
    /// 断言：Display 含 "ConfigError"、"bad transport"、code=893005
    #[test]
    fn display_config() {
        let e = Error::Config("bad transport".into());
        let s = format!("{e}");
        assert!(s.contains("ConfigError"));
        assert!(s.contains("bad transport"));
        assert!(s.contains("code=893005"));
    }

    /// P0：[Error::Display] Permission 变体格式化包含类型名、消息和错误码
    /// 条件：Error::Permission("path denied")
    /// 断言：Display 含 "PermissionError"、"path denied"、code=893006
    #[test]
    fn display_permission() {
        let e = Error::Permission("path denied".into());
        let s = format!("{e}");
        assert!(s.contains("PermissionError"));
        assert!(s.contains("path denied"));
        assert!(s.contains("code=893006"));
    }

    /// P0：[Error::Display] Io 变体格式化包含消息、错误码和 kind
    /// 条件：Error::Io { message:"write failed", source: io::Error::new(PermissionDenied) }
    /// 断言：Display 含 "IoError"、"write failed"、code=893003、kind=PermissionDenied
    #[test]
    fn display_io() {
        let e = Error::Io {
            message: "write failed".into(),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };
        let s = format!("{e}");
        assert!(s.contains("IoError"));
        assert!(s.contains("write failed"));
        assert!(s.contains("code=893003"));
        assert!(s.contains("kind=PermissionDenied"));
    }

    /// P1：[Error::Display] CliOutput 变体格式化包含消息、code、exit_code 和 kind
    /// 条件：Error::CliOutput { code:2, message:"usage error", source:None }
    /// 断言：Display 含 "CliOutput"、"usage error"、code=893004、exit=2、kind=?
    #[test]
    fn display_cli_output() {
        let e = Error::CliOutput {
            code: 2,
            message: "usage error".into(),
            source: None,
        };
        let s = format!("{e}");
        assert!(s.contains("CliOutput"));
        assert!(s.contains("usage error"));
        assert!(s.contains("code=893004"));
        assert!(s.contains("exit=2"));
        assert!(s.contains("kind=?"));
    }

    /// P1：[Error::Display] CliOutput 携带 clap source 时显示其 kind
    /// 条件：CliOutput { source: Some(clap::Error::raw(InvalidSubcommand)) }
    /// 断言：Display 含 "kind=unrecognized subcommand"
    #[test]
    fn display_cli_output_with_clap_source() {
        let clap_err = clap::Error::raw(clap::error::ErrorKind::InvalidSubcommand, "no such cmd");
        let e = Error::CliOutput {
            code: 2,
            message: "unknown subcommand".into(),
            source: Some(clap_err),
        };
        let s = format!("{e}");
        assert!(s.contains("kind=unrecognized subcommand"));
    }

    /// P1：[Error::Display] UnknownError 变体包含消息和 code
    /// 条件：Error::Other("unexpected")
    /// 断言：Display 含 "UnknownError"、"unexpected"、code=893999
    #[test]
    fn display_other() {
        let e = Error::Other("unexpected".into());
        let s = format!("{e}");
        assert!(s.contains("UnknownError"));
        assert!(s.contains("unexpected"));
        assert!(s.contains("code=893999"));
    }

    /// P1：[Error::Display] Transport 变体委托内层 Display
    /// 条件：Transport(wecom_transport::Error::Other("inner error"))
    /// 断言：Display 含 "inner error"
    #[test]
    fn display_transport() {
        let e = Error::Transport(wecom_transport::Error::Other("inner error".into()));
        let s = format!("{e}");
        assert!(s.contains("inner error"));
    }

    // ── From<wecom_transport::Error> ──

    /// P1：[From] wecom_transport::Error 自动转换为 Error::Transport
    /// 条件：用 wecom_transport::Error::Other("wrapped") 触发 .into()
    /// 断言：匹配 Error::Transport；message()=="wrapped"
    #[test]
    fn from_transport_error() {
        let transport_err = wecom_transport::Error::Other("wrapped".into());
        let e: Error = transport_err.into();
        assert!(matches!(e, Error::Transport(_)));
        assert_eq!(e.message(), "wrapped");
    }

    // ── CliOutput code 分支 ──

    /// P2：[Error::code] CliOutput 子命令错误返回 E_SUBCMD
    /// 条件：CliOutput { source: Some(clap::Error::raw(InvalidSubcommand)) }
    /// 断言：code() == E_SUBCMD
    #[test]
    fn code_cli_output_subcommand() {
        let clap_err = clap::Error::raw(
            clap::error::ErrorKind::InvalidSubcommand,
            "no such subcommand",
        );
        let e = Error::CliOutput {
            code: 2,
            message: "unknown subcommand".into(),
            source: Some(clap_err),
        };
        assert_eq!(e.code(), E_SUBCMD);
    }

    // ── code: CliOutput with non-subcommand error kind ──

    /// P2：[Error::code] CliOutput 携带 None source 时返回 E_CLI（非子命令错误）
    /// 条件：source 为 None
    /// 断言：code() == E_CLI
    #[test]
    fn code_cli_output_without_clap_source_returns_e_cli() {
        let e = Error::CliOutput {
            code: 0,
            message: "version info".into(),
            source: None,
        };
        assert_eq!(e.code(), E_CLI);
    }
}
