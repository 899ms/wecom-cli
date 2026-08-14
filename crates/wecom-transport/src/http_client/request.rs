use std::borrow::Cow;
use std::future::IntoFuture;

use super::response;
use crate::{Endpoint, Error, HttpClient, IntoCowValue, Result};

/// HTTP 请求 payload 类型。
pub enum HttpRequestPayload<'a> {
    Json(Cow<'a, serde_json::Value>),
    Form(reqwest::multipart::Form),
}

impl std::fmt::Display for HttpRequestPayload<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(value) => write!(f, "{}", value.as_ref()),
            Self::Form(_) => write!(f, "[multipart/form-data]"),
        }
    }
}

/// Trait for types that can be converted into an [`HttpRequestPayload`].
///
/// Enables [`HttpTransportBackend::post`] to accept both JSON payloads
/// (`&Value`, `Value`, `Cow<Value>`) and `reqwest::multipart::Form`
/// through a single `payload` parameter.
pub trait IntoRequestPayload<'a> {
    fn into_http_request_payload(self) -> HttpRequestPayload<'a>;
}

// Blanket impl: any IntoCowValue can be used as a JSON payload.
impl<'a, V> IntoRequestPayload<'a> for V
where
    V: IntoCowValue<'a>,
{
    fn into_http_request_payload(self) -> HttpRequestPayload<'a> {
        HttpRequestPayload::Json(self.into_cow_value())
    }
}

impl<'a> IntoRequestPayload<'a> for reqwest::multipart::Form {
    fn into_http_request_payload(self) -> HttpRequestPayload<'a> {
        HttpRequestPayload::Form(self)
    }
}

impl<'a> IntoRequestPayload<'a> for HttpRequestPayload<'a> {
    fn into_http_request_payload(self) -> HttpRequestPayload<'a> {
        self
    }
}

/// Raw HTTP request builder implementing `IntoFuture`.
///
/// Created via [`HttpTransportBackend::post`]. Chain `.headers()`, `.header()`,
/// `.timeout()` or just `.await`.
pub struct HttpRequest<'a> {
    pub(crate) http_client: &'a dyn HttpClient,
    pub(crate) endpoint: Cow<'a, Endpoint>,
    pub(crate) payload: HttpRequestPayload<'a>,
    pub(crate) header_error: Option<Error>,
    pub(crate) options: crate::WireOptions,
}

crate::impl_request_builder!(HttpRequest<'a>, +wire);

impl<'a> HttpRequest<'a> {
    pub fn new(
        http_client: &'a dyn HttpClient,
        endpoint: Cow<'a, Endpoint>,
        payload: HttpRequestPayload<'a>,
    ) -> Self {
        Self {
            http_client,
            endpoint,
            payload,
            header_error: None,
            options: crate::WireOptions::default(),
        }
    }

    /// 可变访问底层 [`WireOptions`](crate::WireOptions) —— 供 pipeline 的
    /// `sign` hook（每轮 fresh 签名）注入鉴权头等。
    pub fn wire_mut(&mut self) -> &mut crate::WireOptions {
        &mut self.options
    }

    /// 可变访问目标 [`Endpoint`] —— 供 pipeline 的 `sign` hook 追加
    /// endpoint 级参数。
    pub fn endpoint_mut(&mut self) -> &mut Cow<'a, Endpoint> {
        &mut self.endpoint
    }

    /// Execute the HTTP request.
    ///
    /// This is called automatically when you `.await` the [`HttpRequest`],
    /// but can also be invoked explicitly if needed.
    pub async fn execute(self) -> Result<response::HttpResponse> {
        if let Some(e) = self.header_error {
            return Err(e);
        }
        self.http_client.send(self).await
    }
}

impl<'a> IntoFuture for HttpRequest<'a> {
    type Output = Result<super::response::HttpResponse>;
    type IntoFuture =
        std::pin::Pin<Box<dyn std::future::Future<Output = Self::Output> + Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.execute())
    }
}

#[allow(clippy::needless_update)]
#[cfg(test)]
mod tests {
    //! ## 模块摘要：request（HTTP 请求类型定义）
    //!
    //! ### 关键接口
    //! - [HttpRequest] — HTTP 请求 builder，支持 [IntoFuture]
    //! - [HttpRequest::headers] — 附加额外 HTTP 头
    //!
    //! ### 关键分支与异常路径
    //! - [HttpRequest::headers] 传入空 HeaderMap → headers 仍为 None
    //! - [HttpRequest::header] 非法 name/value → header_error 为 Some

    use serde_json::json;
    use wiremock::MockServer;

    use super::*;
    use crate::http::HttpEndpoint;

    /// 测试 helper：构造 HTTP `Endpoint`。
    fn ep(base: &str, path: &str) -> Endpoint {
        let http = HttpEndpoint::new(path).with_base_url(base);
        Endpoint::new().with(http)
    }

    // ── HttpRequest::headers ──

    /// P0：headers() 设置非空 headers 后 headers 为 Some 且内容正确
    /// 条件：创建 HttpRequest 后调用 .headers(&non_empty_map)
    /// 断言：headers 为 Some，且包含设置的 header 名和值
    #[test]
    fn headers_sets_headers() {
        let transport = crate::HttpTransportBackend::default();
        let endpoint = ep("http://test", "/");
        let payload = json!({});
        let mut extra = reqwest::header::HeaderMap::new();
        extra.insert(
            reqwest::header::HeaderName::from_static("x-extra"),
            reqwest::header::HeaderValue::from_static("val"),
        );

        let req = HttpRequest::new(
            &*transport.http_client,
            Cow::Borrowed(&endpoint),
            HttpRequestPayload::Json(Cow::Borrowed(&payload)),
        );
        let req = req.headers(&extra);
        let headers = req.get_headers();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers.get("x-extra").unwrap().to_str().unwrap(), "val");
    }

    /// P1：headers() 设置多个 headers 后内容全部正确
    /// 条件：创建 HttpRequest 后调用 .headers(&map_with_two_entries)
    /// 断言：headers 包含两个 header 且各值正确
    #[test]
    fn headers_sets_multiple_headers_correctly() {
        let transport = crate::HttpTransportBackend::default();
        let endpoint = ep("http://test", "/");
        let payload = json!({});
        let mut extra = reqwest::header::HeaderMap::new();
        extra.insert(
            reqwest::header::HeaderName::from_static("x-token"),
            reqwest::header::HeaderValue::from_static("abc123"),
        );
        extra.insert(
            reqwest::header::HeaderName::from_static("x-request-id"),
            reqwest::header::HeaderValue::from_static("req-001"),
        );

        let req = HttpRequest::new(
            &*transport.http_client,
            Cow::Borrowed(&endpoint),
            HttpRequestPayload::Json(Cow::Borrowed(&payload)),
        );
        let req = req.headers(&extra);
        let headers = req.get_headers();
        assert_eq!(headers.len(), 2);
        assert_eq!(headers.get("x-token").unwrap().to_str().unwrap(), "abc123");
        assert_eq!(
            headers.get("x-request-id").unwrap().to_str().unwrap(),
            "req-001"
        );
    }

    /// P1：headers() 传入空 HeaderMap 时 headers 保持 None
    /// 条件：创建 HttpRequest 后调用 .headers(&empty_map)
    /// 断言：headers 仍为 None
    #[test]
    fn headers_empty_map_stays_none() {
        let transport = crate::HttpTransportBackend::default();
        let endpoint = ep("http://test", "/");
        let payload = json!({});
        let empty = reqwest::header::HeaderMap::new();

        let req = HttpRequest::new(
            &*transport.http_client,
            Cow::Borrowed(&endpoint),
            HttpRequestPayload::Json(Cow::Borrowed(&payload)),
        );
        let req = req.headers(&empty);
        assert!(req.get_headers().is_empty());
    }

    /// P0：headers() 多次调用时 extend 追加而非替换
    /// 条件：先 headers({x-a}) 再 headers({x-b})
    /// 断言：headers 包含 x-a 和 x-b 两个 header
    #[test]
    fn headers_extends_instead_of_replace() {
        let transport = crate::HttpTransportBackend::default();
        let endpoint = ep("http://test", "/");
        let payload = json!({});
        let mut first = reqwest::header::HeaderMap::new();
        first.insert(
            reqwest::header::HeaderName::from_static("x-a"),
            reqwest::header::HeaderValue::from_static("a-val"),
        );
        let mut second = reqwest::header::HeaderMap::new();
        second.insert(
            reqwest::header::HeaderName::from_static("x-b"),
            reqwest::header::HeaderValue::from_static("b-val"),
        );

        let req = HttpRequest::new(
            &*transport.http_client,
            Cow::Borrowed(&endpoint),
            HttpRequestPayload::Json(Cow::Borrowed(&payload)),
        );
        let req = req.headers(&first).headers(&second);
        let headers = req.get_headers();
        assert_eq!(headers.len(), 2);
        assert_eq!(headers.get("x-a").unwrap().to_str().unwrap(), "a-val");
        assert_eq!(headers.get("x-b").unwrap().to_str().unwrap(), "b-val");
    }

    // ── HttpRequest::header ──

    /// P0：header() 添加单个 header 到 headers
    /// 条件：创建 HttpRequest 后调用 .header("x-single", "val")
    /// 断言：headers 包含该 header 且值正确
    #[test]
    fn header_adds_single_header() {
        let transport = crate::HttpTransportBackend::default();
        let endpoint = ep("http://test", "/");
        let payload = json!({});

        let req = HttpRequest::new(
            &*transport.http_client,
            Cow::Borrowed(&endpoint),
            HttpRequestPayload::Json(Cow::Borrowed(&payload)),
        );
        let req = req.header("x-single", "val");
        assert!(req.header_error.is_none());
        let headers = req.get_headers();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers.get("x-single").unwrap().to_str().unwrap(), "val");
    }

    /// P0：header() 可链式调用多次，追加多个 header
    /// 条件：连续调用 .header("x-a", "1").header("x-b", "2")
    /// 断言：headers 包含两个 header
    #[test]
    fn header_chain_multiple() {
        let transport = crate::HttpTransportBackend::default();
        let endpoint = ep("http://test", "/");
        let payload = json!({});

        let req = HttpRequest::new(
            &*transport.http_client,
            Cow::Borrowed(&endpoint),
            HttpRequestPayload::Json(Cow::Borrowed(&payload)),
        );
        let req = req.header("x-a", "1").header("x-b", "2");
        assert!(req.header_error.is_none());
        let headers = req.get_headers();
        assert_eq!(headers.len(), 2);
        assert_eq!(headers.get("x-a").unwrap().to_str().unwrap(), "1");
        assert_eq!(headers.get("x-b").unwrap().to_str().unwrap(), "2");
    }

    /// P1：header() 和 headers() 混合使用时全部追加
    /// 条件：先 header("x-a"), 再 headers({x-b}), 再 header("x-c")
    /// 断言：headers 包含三个 header
    #[test]
    fn header_and_headers_mixed() {
        let transport = crate::HttpTransportBackend::default();
        let endpoint = ep("http://test", "/");
        let payload = json!({});
        let mut batch = reqwest::header::HeaderMap::new();
        batch.insert(
            reqwest::header::HeaderName::from_static("x-b"),
            reqwest::header::HeaderValue::from_static("b-val"),
        );

        let req = HttpRequest::new(
            &*transport.http_client,
            Cow::Borrowed(&endpoint),
            HttpRequestPayload::Json(Cow::Borrowed(&payload)),
        );
        let req = req
            .header("x-a", "a-val")
            .headers(&batch)
            .header("x-c", "c-val");
        assert!(req.header_error.is_none());
        let headers = req.get_headers();
        assert_eq!(headers.len(), 3);
        assert_eq!(headers.get("x-a").unwrap().to_str().unwrap(), "a-val");
        assert_eq!(headers.get("x-b").unwrap().to_str().unwrap(), "b-val");
        assert_eq!(headers.get("x-c").unwrap().to_str().unwrap(), "c-val");
    }

    /// P1：header() 非法 header name 时错误延迟存储
    /// 条件：传入空字符串 "" 作为 header name
    /// 断言：header_error 为 Some
    #[test]
    fn header_rejects_invalid_name() {
        let transport = crate::HttpTransportBackend::default();
        let endpoint = ep("http://test", "/");
        let payload = json!({});

        let req = HttpRequest::new(
            &*transport.http_client,
            Cow::Borrowed(&endpoint),
            HttpRequestPayload::Json(Cow::Borrowed(&payload)),
        );
        let req = req.header("", "value");
        assert!(req.header_error.is_some());
    }

    /// P1：header() 非法 header value 时错误延迟存储
    /// 条件：传入含 null 字节的 value
    /// 断言：header_error 为 Some
    #[test]
    fn header_rejects_invalid_value() {
        let transport = crate::HttpTransportBackend::default();
        let endpoint = ep("http://test", "/");
        let payload = json!({});

        let req = HttpRequest::new(
            &*transport.http_client,
            Cow::Borrowed(&endpoint),
            HttpRequestPayload::Json(Cow::Borrowed(&payload)),
        );
        let req = req.header("x-name", "\0\0\0invalid");
        assert!(req.header_error.is_some());
    }

    // ── HttpRequest::timeout ──

    /// P0：[HttpRequest::timeout] timeout() 正确设置 timeout 字段
    /// 条件：创建 HttpRequest 后调用 .timeout(Duration::from_secs(30))
    /// 断言：timeout 字段为 Some(30s)
    #[test]
    fn timeout_sets_field() {
        let transport = crate::HttpTransportBackend::default();
        let endpoint = ep("http://test", "/");
        let payload = json!({});

        let req = HttpRequest::new(
            &*transport.http_client,
            Cow::Borrowed(&endpoint),
            HttpRequestPayload::Json(Cow::Borrowed(&payload)),
        );
        let req = req.timeout(std::time::Duration::from_secs(30));
        assert_eq!(req.get_timeout(), Some(std::time::Duration::from_secs(30)));
    }

    /// P0：[HttpRequest::timeout] timeout 默认为 None
    /// 条件：创建 HttpRequest 不调用 timeout()
    /// 断言：timeout 字段为 None
    #[test]
    fn timeout_default_none() {
        let transport = crate::HttpTransportBackend::default();
        let endpoint = ep("http://test", "/");
        let payload = json!({});

        let req = HttpRequest::new(
            &*transport.http_client,
            Cow::Borrowed(&endpoint),
            HttpRequestPayload::Json(Cow::Borrowed(&payload)),
        );
        assert!(req.get_timeout().is_none());
    }

    /// P1：[HttpRequest::timeout] timeout() 和 headers() 可链式调用
    /// 条件：先 timeout(10s) 再 headers(&map)
    /// 断言：timeout 和 headers 均正确设置
    #[test]
    fn timeout_and_headers_chain() {
        let transport = crate::HttpTransportBackend::default();
        let endpoint = ep("http://test", "/");
        let payload = json!({});
        let mut extra = reqwest::header::HeaderMap::new();
        extra.insert(
            reqwest::header::HeaderName::from_static("x-chain"),
            reqwest::header::HeaderValue::from_static("val"),
        );

        let req = HttpRequest::new(
            &*transport.http_client,
            Cow::Borrowed(&endpoint),
            HttpRequestPayload::Json(Cow::Borrowed(&payload)),
        );
        let req = req
            .timeout(std::time::Duration::from_secs(10))
            .headers(&extra);
        assert_eq!(req.get_timeout(), Some(std::time::Duration::from_secs(10)));
        assert!(!req.get_headers().is_empty());
    }

    // ── HttpRequest::into_future headers 合并语义 ──

    /// P0：[HttpRequest::into_future] 仅 .headers(...) 设置的 header 会被发送
    /// 条件：transport 只提供 http_client（不含任何 headers）；.headers 追加 x-extra=extra-val
    /// 断言：wire 请求包含 x-extra（transport 级 headers 不再由 HttpRequest::execute 自动合并）
    #[tokio::test]
    async fn into_future_merges_transport_and_headers() {
        use wiremock::matchers::{method, path};
        use wiremock::{Match, Mock, Request, ResponseTemplate};

        struct ExtraOnlyMatcher;
        impl Match for ExtraOnlyMatcher {
            fn matches(&self, request: &Request) -> bool {
                request.headers.get("x-extra").and_then(|v| v.to_str().ok()) == Some("extra-val")
            }
        }

        let server = MockServer::start().await;
        let transport = crate::HttpTransportBackend::default();

        let mut extra = reqwest::header::HeaderMap::new();
        extra.insert(
            reqwest::header::HeaderName::from_static("x-extra"),
            reqwest::header::HeaderValue::from_static("extra-val"),
        );

        Mock::given(method("POST"))
            .and(path("/merge"))
            .and(ExtraOnlyMatcher)
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "{\"ok\":true}"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let payload = json!({});
        let endpoint = ep(&server.uri(), "/merge");
        let req = HttpRequest::new(
            &*transport.http_client,
            Cow::Borrowed(&endpoint),
            HttpRequestPayload::Json(Cow::Borrowed(&payload)),
        );
        let _ = req.headers(&extra).await.unwrap();
    }

    /// P1：[HttpRequest::into_future] 不调用 .headers() 时，请求不携带 transport 级 header
    /// 条件：transport.headers 含 x-only；直接通过 HttpRequest::execute 发起请求（跳过 invoke 合并）
    /// 断言：wire 请求中 **不含** x-only——验证 request 子层不再自动合并 transport.headers
    #[tokio::test]
    async fn into_future_only_transport_headers() {
        use wiremock::matchers::{method, path};
        use wiremock::{Match, Mock, Request, ResponseTemplate};

        struct NoTransportHeaderMatcher;
        impl Match for NoTransportHeaderMatcher {
            fn matches(&self, request: &Request) -> bool {
                request.headers.get("x-only").is_none()
            }
        }

        let server = MockServer::start().await;
        let mut base_headers = reqwest::header::HeaderMap::new();
        base_headers.insert(
            reqwest::header::HeaderName::from_static("x-only"),
            reqwest::header::HeaderValue::from_static("only-val"),
        );
        let transport = crate::HttpTransportBackend::default();

        Mock::given(method("POST"))
            .and(path("/only"))
            .and(NoTransportHeaderMatcher)
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "{\"ok\":true}"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let payload = json!({});
        let endpoint = ep(&server.uri(), "/only");
        let req = HttpRequest::new(
            &*transport.http_client,
            Cow::Borrowed(&endpoint),
            HttpRequestPayload::Json(Cow::Borrowed(&payload)),
        );
        let _ = req.await.unwrap();
    }

    /// P1：[HttpRequest::into_future] .headers() 多次调用为 extend 语义，不会丢失先前 header
    /// 条件：先 .headers({x-first:val1}) 再 .headers({x-second:val2})
    /// 断言：wire 请求同时包含 x-first 和 x-second
    #[tokio::test]
    async fn into_future_additional_overrides_transport() {
        use wiremock::matchers::{method, path};
        use wiremock::{Match, Mock, Request, ResponseTemplate};

        struct BothMatcher;
        impl Match for BothMatcher {
            fn matches(&self, request: &Request) -> bool {
                let first =
                    request.headers.get("x-first").and_then(|v| v.to_str().ok()) == Some("val1");
                let second = request
                    .headers
                    .get("x-second")
                    .and_then(|v| v.to_str().ok())
                    == Some("val2");
                first && second
            }
        }

        let server = MockServer::start().await;
        let transport = crate::HttpTransportBackend::default();

        let mut first = reqwest::header::HeaderMap::new();
        first.insert(
            reqwest::header::HeaderName::from_static("x-first"),
            reqwest::header::HeaderValue::from_static("val1"),
        );
        let mut second = reqwest::header::HeaderMap::new();
        second.insert(
            reqwest::header::HeaderName::from_static("x-second"),
            reqwest::header::HeaderValue::from_static("val2"),
        );

        Mock::given(method("POST"))
            .and(path("/override"))
            .and(BothMatcher)
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "{\"ok\":true}"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let payload = json!({});
        let endpoint = ep(&server.uri(), "/override");
        let req = HttpRequest::new(
            &*transport.http_client,
            Cow::Borrowed(&endpoint),
            HttpRequestPayload::Json(Cow::Borrowed(&payload)),
        );
        let _ = req.headers(&first).headers(&second).await.unwrap();
    }

    // ── HttpRequestPayload::Display ──

    /// P1：[HttpRequestPayload::Display] Form 变体显示为 [multipart/form-data]
    /// 条件：构造 HttpRequestPayload::Form 包含一个 text 字段
    /// 断言：Display 格式化为 "[multipart/form-data]"
    #[test]
    fn display_form_variant() {
        let form = reqwest::multipart::Form::new().text("key", "value");
        let payload = HttpRequestPayload::Form(form);
        let s = format!("{payload}");
        assert_eq!(s, "[multipart/form-data]");
    }

    // ── IntoRequestPayload ──

    /// P1：[IntoRequestPayload] reqwest::multipart::Form 转换为 Form 变体
    /// 条件：构造 reqwest::multipart::Form 并调用 into_http_request_payload()
    /// 断言：返回 HttpRequestPayload::Form 变体
    #[test]
    fn into_request_payload_for_form() {
        let form = reqwest::multipart::Form::new().text("field", "data");
        let payload = form.into_http_request_payload();
        assert!(matches!(payload, HttpRequestPayload::Form(_)));
    }

    /// P1：[IntoRequestPayload] HttpRequestPayload 透传（identity）
    /// 条件：构造 HttpRequestPayload::Json 并调用 into_http_request_payload()
    /// 断言：返回相同 HttpRequestPayload::Json 变体
    #[test]
    fn into_request_payload_identity() {
        let payload = HttpRequestPayload::Json(Cow::Owned(json!({"a": 1})));
        let same = payload.into_http_request_payload();
        assert!(matches!(same, HttpRequestPayload::Json(_)));
    }
}
