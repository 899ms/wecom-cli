use std::sync::Arc;

use crate::http_client::IntoRequestPayload;
use crate::{RequestOptions, TransportBackend};

// ── Transport struct ──────────────────────────────────────────

/// Unified transport handle dispatching to any type implementing
/// [`TransportBackend`] via `Arc<dyn TransportBackend>`.
///
/// Cheap to clone — `Arc`-shared backend + cloned options. Cloned instances are
/// independent; later mutations on the original do not propagate to clones.
///
/// # Construction
///
/// ```ignore
/// // From an existing Arc:
/// let transport = Transport::new(Arc::new(MyBackend::new()), RequestOptions::default());
///
/// // From any TransportBackend implementor (auto-wraps in Arc):
/// let transport: Transport = HttpTransportBackend::default().into();
/// ```
///
/// # Example
///
/// ```ignore
/// let t = client.transport().clone();
/// tokio::spawn(async move { t.headers(); });
/// ```
#[derive(Clone)]
pub struct Transport {
    pub(crate) backend: Arc<dyn TransportBackend>,
    pub default_options: crate::RequestOptions,
}

impl<T: TransportBackend + 'static> From<T> for Transport {
    fn from(t: T) -> Self {
        Self {
            backend: Arc::new(t),
            default_options: RequestOptions::default(),
        }
    }
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transport")
            .field("name", &self.backend.name())
            .field("default_options", &self.default_options)
            .finish()
    }
}

impl Transport {
    /// Construct a [`Transport`] from an existing `Arc<dyn TransportBackend>`.
    ///
    /// Used for custom/dynamic transports. For owned backends, prefer
    /// [`From<T>`](Self) (e.g. `Transport::from(backend)` or `backend.into()`),
    /// which auto-wraps in `Arc`.
    pub fn new(backend: Arc<dyn TransportBackend>, default_options: crate::RequestOptions) -> Self {
        Self {
            backend,
            default_options,
        }
    }

    /// Wrap the underlying backend with a middleware closure, keeping
    /// `default_options`.
    ///
    /// This is the runtime counterpart of
    /// [`TransportBuilder::map_backend`](crate::TransportBuilder::map_backend):
    /// the builder mutates the concrete backend in-place at construction time,
    /// while this method wraps the already type-erased backend at runtime
    /// (e.g. to layer cross-cutting concerns such as auth-header injection or
    /// error-driven retry on top of an existing backend).
    ///
    /// ```ignore
    /// let transport = transport.wrap_backend(|inner| Arc::new(MyBackend::new(inner)));
    /// ```
    #[must_use]
    pub fn wrap_backend(
        mut self,
        f: impl FnOnce(Arc<dyn TransportBackend>) -> Arc<dyn TransportBackend>,
    ) -> Self {
        self.backend = f(self.backend);
        self
    }

    /// Human-readable label for logging.
    ///
    /// Returns `"http"` or the name of a custom transport.
    pub fn name(&self) -> &str {
        self.backend.name()
    }

    /// Borrow the transport's default headers immutably.
    ///
    /// These headers are applied to every request made through this transport
    /// (per-request headers added via [`TransportRequest::header`](crate::TransportRequest) take precedence).
    pub fn headers(&self) -> &reqwest::header::HeaderMap {
        &self.default_options.wire.headers
    }

    /// Borrow the transport's default headers mutably.
    ///
    /// Use this to modify headers in-place (e.g. insert, remove, or update
    /// a header) without consuming the transport. For chaining, prefer
    /// [`with_header`](Self::with_header) or [`with_headers`](Self::with_headers).
    pub fn headers_mut(&mut self) -> &mut reqwest::header::HeaderMap {
        &mut self.default_options.wire.headers
    }

    /// Set a default header applied to every request from this transport.
    ///
    /// Consumes and returns `self` for chaining. Unlike the builder's
    /// deferred-error `.header()`, this validates eagerly and returns
    /// [`crate::Error`] on an invalid name/value, since a [`Transport`] is
    /// already built and there is no later `build()` to surface the error.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let transport = transport.with_header("X-Trace", trace_id)?;
    /// ```
    pub fn with_header(
        self,
        name: impl crate::IntoHeaderName,
        value: impl crate::IntoHeaderValue,
    ) -> crate::Result<Self> {
        self.with_header_sensitive(name, value, false)
    }

    /// Like [`with_header`](Self::with_header), but marks the value as
    /// sensitive so it is redacted from debug/log output (e.g. auth tokens).
    pub fn with_header_sensitive(
        mut self,
        name: impl crate::IntoHeaderName,
        value: impl crate::IntoHeaderValue,
        sensitive: bool,
    ) -> crate::Result<Self> {
        let name = name.try_into_header_name().map_err(crate::Error::Other)?;
        let mut value = value.try_into_header_value().map_err(crate::Error::Other)?;
        if sensitive {
            value.set_sensitive(true);
        }
        self.default_options.wire.headers.insert(name, value);
        Ok(self)
    }

    /// Set multiple default headers applied to every request from this transport.
    ///
    /// Consumes and returns `self` for chaining. Inserts all entries from the
    /// provided [`HeaderMap`](reqwest::header::HeaderMap) into the transport's
    /// default headers. Existing headers with the same name are overwritten.
    ///
    /// Unlike [`with_header`](Self::with_header), this method is infallible
    /// because [`HeaderMap`](reqwest::header::HeaderMap) entries are already
    /// validated `HeaderName` / `HeaderValue` pairs.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut headers = reqwest::header::HeaderMap::new();
    /// headers.insert(
    ///     reqwest::header::HeaderName::from_static("x-trace"),
    ///     reqwest::header::HeaderValue::from_static("abc"),
    /// );
    /// let transport = transport.with_headers(headers);
    /// ```
    #[must_use]
    pub fn with_headers(mut self, headers: reqwest::header::HeaderMap) -> Self {
        self.default_options.wire.headers.extend(
            headers
                .into_iter()
                .filter_map(|(name, value)| name.map(|n| (n, value))),
        );
        self
    }

    /// Borrow the transport's default extension bag.
    ///
    /// These values are cloned (Arc-shared) into every request made through
    /// this transport and may be overridden per-request.
    pub fn extensions(&self) -> &crate::Extensions {
        &self.default_options.extensions
    }

    /// Borrow the transport's default extension bag mutably.
    ///
    /// Use this to add/remove values in-place; for chaining prefer
    /// [`with_extension`](Self::with_extension).
    pub fn extensions_mut(&mut self) -> &mut crate::Extensions {
        &mut self.default_options.extensions
    }

    /// Set a default extension value applied to every request from this
    /// transport.
    ///
    /// Same-type values set later via `CliRun` / per-request `.extension()`
    /// override this default (per-TypeId, later layer wins).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let transport = transport.with_extension(RetryConfig { max_retries: 3 });
    /// ```
    #[must_use]
    pub fn with_extension<T>(mut self, value: T) -> Self
    where
        T: std::any::Any + std::fmt::Debug + Send + Sync + 'static,
    {
        self.default_options.extensions.insert(value);
        self
    }

    /// Merge an external [`Extensions`](crate::Extensions) bag into the
    /// transport's default bag (per-TypeId, incoming wins).
    ///
    /// This is the batch counterpart of [`with_extension`](Self::with_extension),
    /// mirroring [`with_headers`](Self::with_headers).
    #[must_use]
    pub fn with_extensions(mut self, ext: &crate::Extensions) -> Self {
        self.default_options.extensions.extend(ext);
        self
    }

    /// Transport-level default per-request timeout, applied to every request.
    ///
    /// A per-request `.timeout(...)` on [`crate::TransportRequest`] still
    /// overrides this default.
    pub fn timeout(&self) -> Option<std::time::Duration> {
        self.default_options.wire.timeout
    }

    /// Set the transport-level default per-request timeout.
    ///
    /// Consumes and returns `self` for chaining. A per-request `.timeout(...)`
    /// still overrides this default. See [`timeout`](Self::timeout).
    #[must_use]
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.default_options.wire.timeout = Some(timeout);
        self
    }

    /// Build a transport-level request, dispatching through the trait object.
    ///
    /// Returns [`crate::TransportRequest`] — chain `.headers()`, `.timeout()`,
    /// or just `.await`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// transport.invoke(&endpoint, &payload).await?;
    /// ```
    pub fn invoke<'a, E, P>(&'a self, endpoint: E, payload: P) -> crate::TransportRequest<'a>
    where
        E: crate::IntoCowEndpoint<'a>,
        P: IntoRequestPayload<'a>,
    {
        let endpoint = endpoint.into_cow_endpoint();
        let payload = payload.into_http_request_payload();

        crate::TransportRequest {
            backend: self.backend.as_ref(),
            endpoint,
            payload,
            header_error: None,
            options: self.default_options.clone(),
        }
    }
}

#[allow(clippy::needless_update)]
#[cfg(test)]
mod tests {
    //! ## 模块摘要：Transport（传输层统一句柄）
    //!
    //! ### 关键接口
    //! - [Transport::headers] / [Transport::headers_mut] — 获取传输层的 HTTP 头
    //! - [Transport::extensions] / [Transport::extensions_mut] / [Transport::with_extension] —
    //!   读取 / 修改 / 链式设置传输层默认扩展袋
    //! - [Transport::name] — 返回传输后端名称
    //! - [`From<T: TransportBackend> for Transport`] — 将任意 TransportBackend 包装为 [Transport]
    //!
    //! ### 关键分支与异常路径
    //! - [Transport::headers] / [Transport::headers_mut] 操作 default_options 中的 headers
    //! - [Transport::name] 委托给内部 trait object
    //!
    //! ### 上下游交互
    //! - 上游：`wecom/client/builder.rs` 的 [build] 方法创建 Transport 实例
    //! - 下游：`wecom/error.rs` 的 [render] 方法使用 Transport 相关信息

    use super::*;
    use crate::http::HttpTransportBackend;
    use crate::{Endpoint, HttpEndpoint};

    /// 测试 helper：构造一个 HTTP `Endpoint`。
    fn ep(base: &str, path: &str) -> Endpoint {
        let http = HttpEndpoint::new(path).with_service(base);
        Endpoint::new().with(http)
    }

    fn http_transport() -> Transport {
        Transport::from(HttpTransportBackend::default())
    }

    /// 测试夹具：扩展袋值。
    #[derive(Debug, PartialEq)]
    struct ExtFixture(u32);

    // ── Transport::headers() / headers_mut() ──

    /// P0：[Transport::headers] Transport::headers() 对 Http transport 返回正确 headers
    /// 条件：构造 Transport 并设置 headers
    /// 断言：headers() 返回设置的 headers
    #[test]
    fn transport_headers_http() {
        use reqwest::header;
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        let default_options = crate::RequestOptions {
            wire: crate::WireOptions {
                headers,
                timeout: None,
                ..crate::WireOptions::default()
            },
            ..crate::RequestOptions::default()
        };
        let transport = Transport::new(
            Arc::new(HttpTransportBackend {
                http_client: std::sync::Arc::new(reqwest::Client::new()),
                base_url: String::new(),
                ..Default::default()
            }),
            default_options,
        );
        assert_eq!(
            transport.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    /// P1：[Transport::headers_mut] Transport::headers_mut() 可修改 headers
    /// 条件：通过 headers_mut() 插入新 header
    /// 断言：headers() 能读取到新插入的 header
    #[test]
    fn transport_headers_mut_http() {
        use reqwest::header;
        let mut transport = Transport::new(
            Arc::new(HttpTransportBackend {
                http_client: std::sync::Arc::new(reqwest::Client::new()),
                base_url: String::new(),
                ..Default::default()
            }),
            crate::RequestOptions::default(),
        );
        transport.headers_mut().insert(
            header::HOST,
            header::HeaderValue::from_static("example.com"),
        );
        assert_eq!(
            transport.headers().get(header::HOST).unwrap(),
            "example.com"
        );
    }

    // ── Transport::name ──

    /// P0：[Transport::name] Http transport name 返回 "http"
    /// 条件：构造 Http Transport
    /// 断言：name() 返回 "http"
    #[test]
    fn name_http_returns_http() {
        let t = http_transport();
        assert_eq!(t.name(), "http");
    }

    // ── From<T: TransportBackend> for Transport ──

    /// P1：[`From<T: TransportBackend> for Transport`] HttpTransportBackend 可以 .into() 成 Transport
    /// 条件：HttpTransportBackend::default() 构造后调用 .into()
    /// 断言：name() 返回 "http"
    #[test]
    fn from_http_transport_yields_http_transport() {
        let http = HttpTransportBackend::default();
        let transport: Transport = http.into();
        assert_eq!(transport.name(), "http");
    }

    // ── Debug ──

    /// P2：[Transport::Debug] Http transport 格式化含 "Transport"
    /// 条件：构造 Http Transport
    /// 断言：format!("{:?}", t) 包含 "Transport"
    #[test]
    fn debug_transport_http() {
        let t = Transport::new(
            Arc::new(HttpTransportBackend::default()),
            crate::RequestOptions::default(),
        );
        let s = format!("{t:?}");
        assert!(s.contains("Transport"));
    }

    // ── Transport::invoke ──

    /// P0：[Transport::invoke] 构造 TransportRequest 不会 panic，且 Debug 输出包含 TransportRequest
    /// 条件：Http transport 上调用 invoke()
    /// 断言：Debug 字符串包含 "TransportRequest"
    #[test]
    fn invoke_http_constructs_request() {
        let transport = http_transport();
        let payload = serde_json::json!({});
        let endpoint = ep("http://test", "/cgi-bin/x");
        let req = transport.invoke(&endpoint, &payload);
        let dbg = format!("{req:?}");
        assert!(
            dbg.contains("TransportRequest"),
            "expected TransportRequest, got: {dbg}"
        );
    }

    /// P0：[Transport::invoke] 同时接受 &Endpoint（Borrowed）与 Endpoint（Owned）
    /// 条件：分别用借用与所有权两种形式调用
    /// 断言：两次构造均成功（不 panic）
    #[test]
    fn invoke_accepts_borrowed_and_owned_endpoint() {
        let transport = http_transport();
        let payload = serde_json::json!({});
        let endpoint = ep("http://x.com", "/y");
        let _ = transport.invoke(&endpoint, &payload);
        let _ = transport.invoke(ep("http://x.com", "/y"), &payload);
    }

    /// P0：[Transport::invoke] payload 接受 `&Value` / owned `Value` / `Cow<Value>` 三种形态
    /// 条件：分别用三种形式传入 payload
    /// 断言：三次构造均成功（不 panic），覆盖 IntoCowValue 三个 impl
    #[test]
    fn invoke_accepts_all_payload_forms() {
        use std::borrow::Cow;

        let transport = http_transport();
        let endpoint = ep("http://x.com", "/y");

        // &Value → Cow::Borrowed
        let borrowed = serde_json::json!({"a": 1});
        let _ = transport.invoke(&endpoint, &borrowed);

        // Value → Cow::Owned
        let _ = transport.invoke(&endpoint, serde_json::json!({"a": 1}));

        // Cow<Value> 透传
        let cow: Cow<'_, serde_json::Value> = Cow::Owned(serde_json::json!({"a": 1}));
        let _ = transport.invoke(&endpoint, cow);
    }

    // ── Transport::extensions ──

    /// P0：[Transport::with_extension] 链式设置默认扩展并读回
    /// 条件：Transport::from(http).with_extension(ExtFixture(7))
    /// 断言：extensions().get::<ExtFixture>() 返回 Some(7)
    #[test]
    fn with_extension_sets_default_bag() {
        let t = http_transport().with_extension(ExtFixture(7));
        assert_eq!(t.extensions().get::<ExtFixture>(), Some(&ExtFixture(7)));
    }

    /// P1：[Transport::extensions_mut] 可变访问插入后读回
    /// 条件：extensions_mut().insert(ExtFixture(3))
    /// 断言：extensions().get::<ExtFixture>() 返回 Some(3)
    #[test]
    fn extensions_mut_inserts_in_place() {
        let mut t = http_transport();
        t.extensions_mut().insert(ExtFixture(3));
        assert_eq!(t.extensions().get::<ExtFixture>(), Some(&ExtFixture(3)));
    }

    /// P1：[Transport::with_extensions] 批量合并外部袋且传入方覆盖同型
    /// 条件：先 with_extension(ExtFixture(1))，再 with_extensions 合并含
    ///       ExtFixture(2) 的外部袋
    /// 断言：extensions().get::<ExtFixture>() 返回 Some(2)
    #[test]
    fn with_extensions_merges_and_overrides() {
        let mut ext = crate::Extensions::new();
        ext.insert(ExtFixture(2));
        let t = http_transport()
            .with_extension(ExtFixture(1))
            .with_extensions(&ext);
        assert_eq!(t.extensions().get::<ExtFixture>(), Some(&ExtFixture(2)));
    }

    /// P1：[Transport::invoke] 默认扩展随 invoke 进入请求 options
    /// 条件：with_extension(ExtFixture(9)) 后 invoke()
    /// 断言：TransportRequest.get_extensions().get::<ExtFixture>() 返回 Some(9)
    #[test]
    fn invoke_carries_default_extensions() {
        let transport = http_transport().with_extension(ExtFixture(9));
        let endpoint = ep("http://x.com", "/y");
        let req = transport.invoke(&endpoint, serde_json::json!({}));
        assert_eq!(
            req.get_extensions().get::<ExtFixture>(),
            Some(&ExtFixture(9))
        );
    }
}
