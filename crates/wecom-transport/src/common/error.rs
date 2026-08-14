use serde_json::{Value, json};

// Transport-owned slice of the CLI error-code scheme (893000 - 893999).
//
// Each variant maps to a stable category code emitted in JSON error output.
// The `wecom` crate owns the remaining (non-transport) codes and re-exports
// these for a single, coherent scheme.
//
// Error code range: 893000 - 893999, this crate uses 893100 - 893199.

/// Network-level error code (DNS, TLS, timeout, connection refused).
pub const E_NETWORK: i64 = 893101;
/// HTTP error code (non-2xx status).
pub const E_HTTP: i64 = 893102;
/// Parse error code (deserialization failure).
pub const E_PARSE: i64 = 893103;
/// Transport configuration error code.
pub const E_CONFIG_TRANSPORT: i64 = 893106;
/// Catch-all error code for transport-layer failures.
pub const E_OTHER: i64 = 893999;

/// Transport-layer errors — network, HTTP, parse, API, and other failures.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Transport / builder configuration error.
    Config(String),

    /// A network-level error (DNS, TLS, timeout, connection refused, …).
    Network {
        message: String,
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },

    /// The HTTP server returned a non-2xx status code.
    Http {
        message: String,
        endpoint: String,
        status: u16,
    },

    /// The server returned a valid response but it could not be deserialized
    /// into the expected type.
    Parse {
        message: String,
        endpoint: String,
        body: Box<serde_json::Value>,
        #[source]
        source: Option<serde_json::Error>,
    },

    /// The server returned an API-level error.
    Api {
        message: String,
        action: String,
        code: Option<i64>,
        body: Box<serde_json::Value>,
    },

    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl Error {
    /// Category error code for this variant.
    ///
    /// Each variant maps to its corresponding `E_NETWORK` / `E_HTTP` /
    /// `E_PARSE` / `E_OTHER` constant. For `Api` this
    /// passes through the backend error code directly (0 if absent).
    #[must_use]
    pub fn code(&self) -> i64 {
        match self {
            Error::Config(_) => E_CONFIG_TRANSPORT,
            Error::Network { .. } => E_NETWORK,
            Error::Http { .. } => E_HTTP,
            Error::Parse { .. } => E_PARSE,
            Error::Api { code, .. } => code.unwrap_or_default(),
            Error::Other(_) => E_OTHER,
        }
    }

    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Error::Config(message) => message.clone(),
            Error::Network { message, .. } => message.clone(),
            Error::Http { message, .. } => message.clone(),
            Error::Parse { message, .. } => message.clone(),
            Error::Api { message, .. } => message.clone(),
            Error::Other(error) => error.to_string(),
        }
    }

    /// Render this error as a `serde_json::Value` for CLI output.
    ///
    /// - `Api` → the raw response body as-is (callers surface the server's own
    ///   `errcode` / `errmsg` without an extra wrapper).
    /// - All other variants → a structured `{ "error": { ... } }` object whose
    ///   shape carries `code` / `message` plus the variant-specific context.
    ///   `Other` intentionally omits the `type` field (no stable category).
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Error::Config(message) => json!({
                "error": {
                    "type": "ConfigError",
                    "code": self.code(),
                    "message": message,
                },
            }),

            Error::Network {
                message,
                endpoint,
                source,
            } => {
                let source_label = network_source_label(source);
                json!({
                    "error": {
                        "type": "NetworkError",
                        "code": self.code(),
                        "message": message,
                        "endpoint": endpoint,
                        "source": source_label,
                    },
                })
            }

            Error::Http {
                message,
                endpoint,
                status,
            } => json!({
                "error": {
                    "type": "HTTPError",
                    "code": self.code(),
                    "message": message,
                    "endpoint": endpoint,
                    "status": status,
                },
            }),

            Error::Parse {
                message,
                endpoint,
                body,
                ..
            } => json!({
                "error": {
                    "type": "ParseError",
                    "code": self.code(),
                    "message": message,
                    "endpoint": endpoint,
                    "body": body.as_ref(),
                },
            }),

            Error::Api { body, .. } => body.as_ref().clone(),

            Error::Other(e) => json!({
                "error": {
                    "type": "UnknownError",
                    "code": self.code(),
                    "message": e.to_string(),
                },
            }),
        }
    }

    /// Render this error as a ready-to-display, pretty-printed JSON string.
    ///
    /// Thin wrapper over [`Error::to_json`]; falls back to the `Display`
    /// representation if serialization unexpectedly fails.
    #[must_use]
    pub fn render(&self) -> String {
        serde_json::to_string_pretty(&self.to_json()).unwrap_or_else(|_| self.to_string())
    }
}

/// Format an `Option<T: Display>`: `Some(v)` → `"v"`, `None` → `"?"`.
pub fn fmt_opt<T: std::fmt::Display>(opt: &Option<T>) -> String {
    match opt {
        Some(v) => v.to_string(),
        None => "?".to_string(),
    }
}

/// Truncate a string for Display: first line only, max `max_len` chars.
///
/// - `[TRUNC]` when the first line exceeds `max_len`.
/// - `\n[TRUNC]` when multiline and first line is within `max_len`.
pub fn trunc_display(s: &str, max_len: usize) -> String {
    let first_line = s.lines().next().unwrap_or("");
    let multiline = s.contains('\n');

    // Overflow: always truncate to max_len and append [TRUNC].
    if first_line.len() > max_len {
        let mut end = max_len;
        while end > 0 && !first_line.is_char_boundary(end) {
            end -= 1;
        }
        return format!("{}[TRUNC]", &first_line[..end]);
    }

    // Within limit + multiline → append \n[TRUNC].
    if multiline {
        return format!("{first_line}\n[TRUNC]");
    }

    first_line.to_string()
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = self.code();
        match self {
            Error::Config(msg) => {
                write!(f, "ConfigError: {msg} [code={code}]")
            }
            Error::Network {
                message,
                endpoint,
                source,
            } => {
                let source_label = network_source_label(source);
                write!(
                    f,
                    "NetworkError: {message} [code={code}, endpoint={endpoint}, source={source_label}]"
                )
            }
            Error::Http {
                message,
                endpoint,
                status,
            } => {
                write!(
                    f,
                    "HttpError: {message} [code={code}, endpoint={endpoint}, status={status}]"
                )
            }
            Error::Parse {
                message,
                endpoint,
                body,
                ..
            } => {
                let body_display = trunc_display(&body.to_string(), 150);
                write!(
                    f,
                    "ParseError: {message} [code={code}, endpoint={endpoint}, body={body_display}]"
                )
            }
            Error::Api { action, body, .. } => {
                write!(f, "ApiError: {body} [code={code}, action={action}]")
            }
            Error::Other(e) => {
                write!(f, "{e} [code={code}]")
            }
        }
    }
}

/// Maximum number of `source()` levels to traverse, guarding against
/// pathological or cyclic error chains.
const MAX_SOURCE_CHAIN_DEPTH: usize = 8;

/// Build a rich, single-line diagnostic label for a [`reqwest::Error`].
///
/// Rather than collapsing everything to a coarse label such as
/// `"connect error"`, this surfaces *every* piece of information we can reach:
/// reqwest's own high-level classification, the HTTP status (if any) and the
/// full underlying `source()` chain. For [`io::Error`](std::io::Error) levels
/// the [`io::ErrorKind`](std::io::ErrorKind) is appended so the precise OS-level
/// reason (e.g. `ConnectionRefused`) is always visible.
///
/// Segments are joined with `"; "`, e.g.:
///   `kind=connect; tcp connect error: Connection refused (os error 111) (io: ConnectionRefused)`
fn network_source_label(source: &reqwest::Error) -> String {
    let mut parts: Vec<String> = Vec::new();

    // reqwest's own high-level classification of the failure.
    let kind = if source.is_timeout() {
        "timeout"
    } else if source.is_connect() {
        "connect"
    } else if source.is_body() {
        "body"
    } else if source.is_decode() {
        "decode"
    } else if source.is_redirect() {
        "redirect"
    } else if source.is_status() {
        "status"
    } else if source.is_request() {
        "request"
    } else if source.is_builder() {
        "builder"
    } else {
        "unknown"
    };
    parts.push(format!("kind={kind}"));

    // HTTP status code, when the error carries one.
    if let Some(status) = source.status() {
        parts.push(format!("status={}", status.as_u16()));
    }

    // The full underlying cause chain — this is where the actionable detail
    // lives (DNS / TLS / I/O reasons that reqwest's coarse flags hide).
    parts.extend(error_chain_detail(source));

    // Guarantee at least one descriptive segment beyond `kind=...` even when
    // the source chain is empty.
    if parts.len() == 1 {
        parts.push(source.to_string());
    }

    parts.join("; ")
}

/// Walk the `source()` chain of `err`, collecting a descriptive label for each
/// level below it.
///
/// For [`io::Error`](std::io::Error) levels the
/// [`io::ErrorKind`](std::io::ErrorKind) is appended in `(io: <Kind>)` form so
/// the precise classification is preserved alongside the human-readable
/// message. Traversal is bounded by [`MAX_SOURCE_CHAIN_DEPTH`].
fn error_chain_detail(err: &dyn std::error::Error) -> Vec<String> {
    let mut detail: Vec<String> = Vec::new();
    let mut current = err.source();
    let mut depth = 0usize;
    while let Some(e) = current {
        if depth >= MAX_SOURCE_CHAIN_DEPTH {
            detail.push("...".to_string());
            break;
        }
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            detail.push(format!("{e} (io: {:?})", io.kind()));
        } else {
            detail.push(e.to_string());
        }
        current = e.source();
        depth += 1;
    }
    detail
}

#[cfg(test)]
mod tests {
    //! ## 模块摘要：Error（Transport 错误类型）
    //!
    //! ### 关键接口
    //! - [Error::Network] { message, url, source } — 网络层错误（DNS/TLS/超时/连接拒绝）
    //! - [Error::Http] { message, url, status } — HTTP 非 2xx 状态码错误
    //! - [Error::Parse] { message, action, body, source } — 响应反序列化失败
    //! - [Error::Api] { message, action, code, body } — 服务端 API 层业务错误
    //! - [Error::Other] — 通用包装错误
    //! - [Error::code] — 返回该变体对应的 893xxx 分类码
    //! - [Error::to_json] / [Error::render] — 将错误渲染为结构化 JSON（Api 直接返回原始 body）
    //!
    //! ### 关键分支与异常路径
    //! - 各变体的 Display 实现包含 message 及关键上下文（endpoint/status/action/code 等）
    //! - Http 变体保留 status 字段可 match 提取
    //! - 所有变体均实现 Debug trait
    //! - render/to_json：Api 返回原始 body（无 `{error:{}}` 包裹）；Other 不含 `type` 字段
    //!
    //! ### 上下游交互
    //! - 上游：整个 wecom-transport crate 的所有模块（polling、request、protocol 等）产生 Error
    //! - 下游：Error 传播至调用方（wecom crate），由上层统一处理/展示

    use assert_json_diff::assert_json_eq;
    use serde_json::json;

    use super::*;

    // ── Error Display ──

    /// P0：Error::Http 的 Display 包含 message 字段
    /// 条件：构造 Http 错误，message="network-like error"
    /// 断言：格式化字符串包含 "network-like"
    #[test]
    fn http_error_display_includes_message() {
        let err = Error::Http {
            message: "network-like error".into(),
            endpoint: "http://example.com".into(),
            status: 503,
        };
        let msg = format!("{err}");
        assert!(msg.contains("network-like"));
    }

    /// P1：Error::Http 变体保留 status 字段可访问
    /// 条件：构造 Http 错误，status=502
    /// 断言：match 提取 status == 502
    #[test]
    fn http_error_holds_status_field() {
        let err = Error::Http {
            message: "bad gateway".into(),
            endpoint: "http://example.com/api".into(),
            status: 502,
        };
        match &err {
            Error::Http { status, .. } => assert_eq!(*status, 502),
            _ => panic!("Expected Http variant"),
        }
    }

    /// P1：Error::Parse 的 Display 包含 message
    /// 条件：构造 Parse 错误，message="invalid json"
    /// 断言：格式化字符串包含 "invalid json"
    #[test]
    fn parse_error_display_includes_message() {
        let err = Error::Parse {
            message: "invalid json".into(),
            endpoint: "/api/test".into(),
            body: Box::new(serde_json::json!({"raw": "data"})),
            source: None,
        };
        let msg = format!("{err}");
        assert!(msg.contains("invalid json"));
    }

    /// P1：Error::Api 的 Display 包含 body 内容
    /// 条件：构造 Api 错误，body 含 errmsg="not found"，code=404
    /// 断言：格式化字符串包含 "not found"
    #[test]
    fn api_error_display_includes_message() {
        let err = Error::Api {
            message: "not found".into(),
            action: "/api/resource".into(),
            code: Some(404),
            body: Box::new(serde_json::json!({"errcode":404,"errmsg":"not found"})),
        };
        let msg = format!("{err}");
        assert!(msg.contains("not found"));
    }

    /// P1：[Error] Error::Other 变体正确包装内部错误信息
    /// 条件：用 "something went wrong" 构造 Other 错误
    /// 断言：格式化字符串包含 "something went wrong"
    #[test]
    fn other_error_display_wraps_inner() {
        let err = Error::Other("something went wrong".into());
        let msg = format!("{err}");
        assert!(msg.contains("something went wrong"));
    }

    /// P1：[Error::Display] Http 变体包含 endpoint 和 status 上下文
    /// 条件：构造 Http 错误，endpoint="/api/foo"，status=502
    /// 断言：Display 输出同时包含 error code、endpoint 和 status
    #[test]
    fn http_display_includes_endpoint_and_status() {
        let err = Error::Http {
            message: "bad gateway".into(),
            endpoint: "/api/foo".into(),
            status: 502,
        };
        let msg = format!("{err}");
        assert!(msg.contains("code=893102"));
        assert!(msg.contains("endpoint=/api/foo"));
        assert!(msg.contains("status=502"));
    }

    /// P1：[Error::Display] Parse 变体包含 endpoint 上下文
    /// 条件：构造 Parse 错误，endpoint="/api/data"
    /// 断言：Display 输出包含 error code 和 endpoint 值
    #[test]
    fn parse_display_includes_endpoint() {
        let err = Error::Parse {
            message: "missing field".into(),
            endpoint: "/api/data".into(),
            body: Box::new(Value::Null),
            source: None,
        };
        let msg = format!("{err}");
        assert!(msg.contains("code=893103"));
        assert!(msg.contains("endpoint=/api/data"));
    }

    /// P1：[Error::Display] Api 变体包含 action 和 code 上下文
    /// 条件：构造 Api 错误，action="get_user"，code=Some(40001)
    /// 断言：Display 输出包含 code=40001 和 action=get_user
    #[test]
    fn api_display_includes_action_and_code() {
        let err = Error::Api {
            message: "invalid credential".into(),
            action: "get_user".into(),
            code: Some(40001),
            body: Box::new(Value::Null),
        };
        let msg = format!("{err}");
        assert!(msg.contains("code=40001"));
        assert!(msg.contains("action=get_user"));
    }

    /// P2：[Error::Display] Api 变体 code 为 None 时默认显示 code=0
    /// 条件：构造 Api 错误，code=None
    /// 断言：Display 输出包含 code=0（code() 对 None 返回 unwrap_or_default()）
    #[test]
    fn api_display_without_code() {
        let err = Error::Api {
            message: "unknown error".into(),
            action: "list".into(),
            code: None,
            body: Box::new(Value::Null),
        };
        let msg = format!("{err}");
        assert!(msg.contains("code=0"));
    }

    // ========== Debug trait ==========

    /// P0：[Error] 所有 Error 变体均实现 Debug trait（编译期检查）
    /// 条件：构造 Http、Parse、Api、Other 四种变体实例
    /// 断言：每个变体都能成功调用 format!("{:?}", v) 不 panic
    #[test]
    fn all_variants_implement_debug() {
        // 编译期检查：确保每个 variant 都能被 debug 格式化
        let variants: Vec<Error> = vec![
            Error::Http {
                message: "".into(),
                endpoint: "".into(),
                status: 0,
            },
            Error::Parse {
                message: "".into(),
                endpoint: "".into(),
                body: Box::new(serde_json::Value::Null),
                source: None,
            },
            Error::Api {
                message: "".into(),
                action: "".into(),
                code: None,
                body: Box::new(serde_json::Value::Null),
            },
            Error::Other("test".into()),
        ];
        for v in variants {
            let _dbg = format!("{v:?}");
        }
    }

    // ── code() ──

    /// P0：[Error::code] 各变体映射到正确的分类码
    /// 条件：分别构造 Http / Parse / Api / Other
    /// 断言：code() 分别返回 E_HTTP / E_PARSE / 透传后台错误码 / E_OTHER
    #[test]
    fn code_maps_each_variant() {
        assert_eq!(
            Error::Http {
                message: "x".into(),
                endpoint: "http://e".into(),
                status: 500,
            }
            .code(),
            E_HTTP
        );
        assert_eq!(
            Error::Parse {
                message: "x".into(),
                endpoint: "/e".into(),
                body: Box::new(Value::Null),
                source: None,
            }
            .code(),
            E_PARSE
        );
        // Api should pass through the backend error code directly.
        assert_eq!(
            Error::Api {
                message: "x".into(),
                action: "/a".into(),
                code: Some(40001),
                body: Box::new(Value::Null),
            }
            .code(),
            40001
        );
        // Api with no code defaults to 0.
        assert_eq!(
            Error::Api {
                message: "x".into(),
                action: "/a".into(),
                code: None,
                body: Box::new(Value::Null),
            }
            .code(),
            0
        );
        assert_eq!(Error::Other("x".into()).code(), E_OTHER);
    }

    // ── render() / to_json() ──

    /// P0：[Error::render] Http 错误渲染含 type / status / endpoint / code
    /// 条件：构造 Http 错误，status=404
    /// 断言：render 反序列化后等于 {error:{type:HTTPError, code:E_HTTP, message, endpoint, status:404}}
    #[test]
    fn render_http_structured() {
        let e = Error::Http {
            message: "not found".into(),
            endpoint: "https://example.com/api".into(),
            status: 404,
        };
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

    /// P0：[Error::render] Api 错误直接返回原始响应体（无 `{error:{}}` 包裹）
    /// 条件：构造 Api 错误，body 含 errcode / errmsg
    /// 断言：render 反序列化后等于原始 body
    #[test]
    fn render_api_returns_raw_body() {
        let body = serde_json::json!({"errcode":40001,"errmsg":"invalid credential"});
        let e = Error::Api {
            message: "invalid credential".into(),
            action: "test".into(),
            code: Some(40001),
            body: Box::new(body.clone()),
        };
        let rendered: Value = serde_json::from_str(&e.render()).unwrap();
        assert_json_eq!(rendered, body);
    }

    /// P1：[Error::render] Other 错误含 message 和 type=UnknownError
    /// 条件：构造 Other("something went wrong")
    /// 断言：render 反序列化后等于 {error:{type:UnknownError, code:E_OTHER, message}}
    #[test]
    fn render_other_omits_type() {
        let e = Error::Other("something went wrong".into());
        let v: Value = serde_json::from_str(&e.render()).unwrap();
        assert_json_eq!(
            v,
            json!({
                "error": {
                    "type": "UnknownError",
                    "code": E_OTHER,
                    "message": "something went wrong"
                }
            })
        );
    }

    /// P1：[Error::render] Parse 错误渲染含 type / code / endpoint / body
    /// 条件：构造 Parse 错误
    /// 断言：render 反序列化后等于 {error:{type:ParseError, code:E_PARSE, message, endpoint, body}}
    #[test]
    fn render_parse_structured() {
        let e = Error::Parse {
            message: "missing field".into(),
            endpoint: "/api/test".into(),
            body: Box::new(serde_json::json!({"unexpected":"data"})),
            source: None,
        };
        let v: Value = serde_json::from_str(&e.render()).unwrap();
        assert_json_eq!(
            v,
            json!({
                "error": {
                    "type": "ParseError",
                    "code": E_PARSE,
                    "message": "missing field",
                    "endpoint": "/api/test",
                    "body": {"unexpected": "data"}
                }
            })
        );
    }

    // ── network_source_label / error_chain_detail ──

    /// Minimal `std::error::Error` with a chainable `source`, used to exercise
    /// [`error_chain_detail`] without constructing a real `reqwest::Error`.
    #[derive(Debug)]
    struct ChainErr {
        msg: String,
        source: Option<Box<dyn std::error::Error + 'static>>,
    }

    impl std::fmt::Display for ChainErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.msg)
        }
    }

    impl std::error::Error for ChainErr {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source.as_deref()
        }
    }

    /// P0：[error_chain_detail] 暴露完整 source 链，并对 io 错误附带 ErrorKind
    /// 条件：顶层错误 -> 中间层错误 -> io::Error(ConnectionRefused)
    /// 断言：返回三段，最内层包含 "(io: ConnectionRefused)" 且保留原始 message
    #[test]
    fn error_chain_detail_exposes_full_chain_with_io_kind() {
        let io = std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "Connection refused (os error 111)",
        );
        let mid = ChainErr {
            msg: "tcp connect error".into(),
            source: Some(Box::new(io)),
        };
        let top = ChainErr {
            msg: "client error (Connect)".into(),
            source: Some(Box::new(mid)),
        };

        let detail = error_chain_detail(&top as &dyn std::error::Error);
        assert_eq!(detail.len(), 2);
        assert_eq!(detail[0], "tcp connect error");
        assert!(detail[1].contains("Connection refused (os error 111)"));
        assert!(detail[1].contains("(io: ConnectionRefused)"));
    }

    /// P1：[error_chain_detail] 无 source 时返回空 vec
    /// 条件：构造无 source 的错误
    /// 断言：返回的 detail 为空
    #[test]
    fn error_chain_detail_empty_without_source() {
        let leaf = ChainErr {
            msg: "leaf".into(),
            source: None,
        };
        assert!(error_chain_detail(&leaf as &dyn std::error::Error).is_empty());
    }

    /// P2：[error_chain_detail] 遍历深度受 MAX_SOURCE_CHAIN_DEPTH 限制
    /// 条件：构造超过上限层数的链
    /// 断言：detail 长度不超过 MAX_SOURCE_CHAIN_DEPTH+1 且末段为 "..."
    #[test]
    fn error_chain_detail_is_depth_bounded() {
        // Build a chain longer than the cap.
        let mut node: Box<dyn std::error::Error + 'static> = Box::new(ChainErr {
            msg: "deepest".into(),
            source: None,
        });
        for i in 0..(MAX_SOURCE_CHAIN_DEPTH + 5) {
            node = Box::new(ChainErr {
                msg: format!("level-{i}"),
                source: Some(node),
            });
        }
        let detail = error_chain_detail(node.as_ref());
        assert_eq!(detail.len(), MAX_SOURCE_CHAIN_DEPTH + 1);
        assert_eq!(detail.last().map(String::as_str), Some("..."));
    }

    // ── trunc_display ──

    /// P0：[trunc_display] short single-line → returned as-is
    /// condition: msg="ok" / "", max_len=30
    /// assert: no [TRUNC] marker
    #[test]
    fn trunc_display_short() {
        assert_eq!(trunc_display("ok", 30), "ok");
        assert_eq!(trunc_display("", 30), "");
    }

    /// P0：[trunc_display] exactly max_len → returned as-is
    /// condition: msg=30×"A", max_len=30
    /// assert: 返回原串（无 [TRUNC] 标记）
    #[test]
    fn trunc_display_exactly_max() {
        let msg = "A".repeat(30);
        assert_eq!(trunc_display(&msg, 30), msg);
    }

    /// P0：[trunc_display] single line exceeds max_len → truncated + [TRUNC]
    /// condition: msg=100×"A", max_len=30
    /// assert: 返回 30×"A" + "[TRUNC]"
    #[test]
    fn trunc_display_long() {
        let msg = "A".repeat(100);
        assert_eq!(
            trunc_display(&msg, 30),
            format!("{}[TRUNC]", "A".repeat(30))
        );
    }

    /// P0：[trunc_display] multiline with first line ≤ max_len → first line + \n[TRUNC]
    /// condition: msg="first\nsecond\nthird", max_len=30
    /// assert: 返回 "first\n[TRUNC]"
    #[test]
    fn trunc_display_multiline() {
        assert_eq!(trunc_display("first\nsecond\nthird", 30), "first\n[TRUNC]");
    }

    /// P0：[trunc_display] multiline with first line > max_len → truncated + [TRUNC]
    /// condition: msg=80×"A" + "\nsecond", max_len=30
    /// assert: 返回 30×"A" + "[TRUNC]"（截断的是首行）
    #[test]
    fn trunc_display_multiline_long_first() {
        let first = "A".repeat(80);
        let msg = format!("{first}\nsecond");
        assert_eq!(
            trunc_display(&msg, 30),
            format!("{}[TRUNC]", "A".repeat(30))
        );
    }

    // ── Config variant ──

    /// P0：[Error::Config] Display 包含消息和 code
    /// 条件：构造 Config("bad endpoint")
    /// 断言：Display 输出含 "ConfigError"、"bad endpoint"、code=893106
    #[test]
    fn config_display_includes_message_and_code() {
        let err = Error::Config("bad endpoint".into());
        let msg = format!("{err}");
        assert!(msg.contains("ConfigError"));
        assert!(msg.contains("bad endpoint"));
        assert!(msg.contains("code=893106"));
    }

    /// P0：[Error::Config] code() 返回 E_CONFIG_TRANSPORT
    /// 条件：构造 Config("x")
    /// 断言：code() == E_CONFIG_TRANSPORT
    #[test]
    fn config_code() {
        assert_eq!(Error::Config("x".into()).code(), E_CONFIG_TRANSPORT);
    }

    /// P0：[Error::Config] message() 返回消息
    /// 条件：构造 Config("bad config")
    /// 断言：message() == "bad config"
    #[test]
    fn config_message() {
        assert_eq!(Error::Config("bad config".into()).message(), "bad config");
    }

    /// P0：[Error::Config] to_json / render 结构化输出
    /// 条件：构造 Config("bad endpoint")
    /// 断言：to_json() 含 type=ConfigError、code=E_CONFIG_TRANSPORT、message
    #[test]
    fn config_to_json() {
        let e = Error::Config("bad endpoint".into());
        assert_json_eq!(
            e.to_json(),
            json!({
                "error": {
                    "type": "ConfigError",
                    "code": E_CONFIG_TRANSPORT,
                    "message": "bad endpoint",
                },
            })
        );
    }

    // ── Network variant ──
    // reqwest::Error has no public constructor (new() is pub(crate)).
    // We use a real (non-routable) connection attempt to obtain an
    // authentic reqwest::Error for testing the Network variant paths.

    /// Helper: create a real `reqwest::Error` by attempting to connect to a
    /// non-routable address.
    fn make_reqwest_error() -> reqwest::Error {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async { reqwest::get("http://0.0.0.0:1").await.unwrap_err() })
    }

    /// P0：[Error::Network] code() 返回 E_NETWORK
    /// 条件：构造 Network 错误（source 为真实 reqwest::Error）
    /// 断言：code() == E_NETWORK
    #[test]
    fn network_code() {
        let err = Error::Network {
            message: "x".into(),
            endpoint: "/e".into(),
            source: make_reqwest_error(),
        };
        assert_eq!(err.code(), E_NETWORK);
    }

    /// P0：[Error::Network] message() 返回消息
    /// 条件：构造 Network 错误，message="timeout"
    /// 断言：message() == "timeout"
    #[test]
    fn network_message() {
        let err = Error::Network {
            message: "timeout".into(),
            endpoint: "/e".into(),
            source: make_reqwest_error(),
        };
        assert_eq!(err.message(), "timeout");
    }

    /// P0：[Error::Network] to_json 包含 type / code / message / endpoint / source
    /// 条件：构造 Network 错误，message="connect error"
    /// 断言：to_json() 中 type=NetworkError、code=E_NETWORK、message/endpoint 透传、source 非空
    #[test]
    fn network_to_json() {
        let e = Error::Network {
            message: "connect error".into(),
            endpoint: "https://api.example.com".into(),
            source: make_reqwest_error(),
        };
        let v = e.to_json();
        assert_eq!(v["error"]["type"], "NetworkError");
        assert_eq!(v["error"]["code"], E_NETWORK);
        assert_eq!(v["error"]["message"], "connect error");
        assert_eq!(v["error"]["endpoint"], "https://api.example.com");
        assert!(!v["error"]["source"].as_str().unwrap().is_empty());
    }

    /// P1：[Error::Network] Display 包含 NetworkError 类型名和 code
    /// 条件：构造 Network 错误，message="connection failed"
    /// 断言：Display 输出含 "NetworkError"、message、code=893101、endpoint
    #[test]
    fn network_display_includes_context() {
        let err = Error::Network {
            message: "connection failed".into(),
            endpoint: "https://api.example.com".into(),
            source: make_reqwest_error(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("NetworkError"));
        assert!(msg.contains("connection failed"));
        assert!(msg.contains("code=893101"));
        assert!(msg.contains("endpoint=https://api.example.com"));
    }

    // ── message() for all variants ──

    /// P1：[Error::message] Http / Parse / Api 变体 message
    /// 条件：分别构造 Http / Parse / Api 错误
    /// 断言：三个变体 message() 均返回各自构造时的 message
    #[test]
    fn message_http_parse_api() {
        assert_eq!(
            Error::Http {
                message: "not found".into(),
                endpoint: "/api".into(),
                status: 404,
            }
            .message(),
            "not found"
        );
        assert_eq!(
            Error::Parse {
                message: "invalid json".into(),
                endpoint: "/api".into(),
                body: Box::new(Value::Null),
                source: None,
            }
            .message(),
            "invalid json"
        );
        assert_eq!(
            Error::Api {
                message: "bad request".into(),
                action: "/action".into(),
                code: Some(400),
                body: Box::new(Value::Null),
            }
            .message(),
            "bad request"
        );
    }

    /// P1：[Error::message] Other 变体返回内部错误的 to_string
    /// 条件：用 io::Error("wrapped io error") 构造 Other
    /// 断言：message() == "wrapped io error"
    #[test]
    fn message_other() {
        let inner = std::io::Error::other("wrapped io error");
        let e = Error::Other(Box::new(inner));
        assert_eq!(e.message(), "wrapped io error");
    }

    // ── fmt_opt ──

    /// P1：[fmt_opt] Some 返回 Display 内容
    /// 条件：Some(42) / Some("hello")
    /// 断言：分别返回 "42" / "hello"
    #[test]
    fn fmt_opt_some() {
        assert_eq!(fmt_opt(&Some(42)), "42");
        assert_eq!(fmt_opt(&Some("hello")), "hello");
    }

    /// P1：[fmt_opt] None 返回 "?"
    /// 条件：Option::<i32>::None
    /// 断言：返回 "?"
    #[test]
    fn fmt_opt_none() {
        let opt: Option<i32> = None;
        assert_eq!(fmt_opt(&opt), "?");
    }
}
