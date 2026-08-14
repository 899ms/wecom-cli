//! HTTP transport request execution — envelope application, ranged download
//! resumption, long-task polling, protocol decoding, and the per-request
//! pipeline helpers that back [`super::HttpTransportBackend`]'s `execute()`.

use std::borrow::Cow;
use std::sync::Arc;

use super::endpoint::{EndpointHttpExt, HttpEndpoint};
use super::envelope::ResponseEnvelope;
use super::{HttpTransportBackend, polling, protocol, resumable};
use crate::http_client::{HttpRequest, HttpRequestPayload};
use crate::traits::TransportResponse;
use crate::{Endpoint, Error, ExecuteOutput, HttpClient, RequestOptions, Result, WireOptions};

// ── Request envelope ──────────────────────────────────────────────

/// Apply the endpoint's request-side [`RequestEnvelope`](super::envelope::RequestEnvelope)
/// wrapping to a JSON payload (e.g. the wecom HTTP gateway expects the
/// business JSON serialized as a string under a `payload` key, i.e.
/// `{"payload": "<json-string>"}`). Endpoints without a request envelope and
/// multipart forms are passed through unchanged. Applied before ranged replay
/// is computed so resumed segments replay the wrapped body.
pub fn apply_request_envelope<'a>(
    endpoint: &Endpoint,
    payload: HttpRequestPayload<'a>,
) -> HttpRequestPayload<'a> {
    let value = match payload {
        HttpRequestPayload::Json(value) => value,
        form => return form,
    };
    HttpRequestPayload::Json(Cow::Owned(
        endpoint.req_envelope().encode(value.into_owned()),
    ))
}

// ── Ranged download ───────────────────────────────────────────────

/// Compute `(clamped chunk size, replay payload)` for an eligible ranged
/// download. Eligible only for JSON payloads (multipart cannot be replayed)
/// carrying a positive `range_size` on the endpoint's [`HttpEndpoint`].
pub fn compute_ranged(
    endpoint: &Endpoint,
    payload: &HttpRequestPayload<'_>,
) -> Option<(u64, serde_json::Value)> {
    match (endpoint.range_size(), payload) {
        (Some(raw), HttpRequestPayload::Json(cow)) if raw > 0 => {
            Some((resumable::clamp_size(raw), (**cow).clone()))
        }
        _ => None,
    }
}

// ── Endpoint defaults resolution ──────────────────────────────────

/// Fill `None` `base_url` on the endpoint's
/// [`HttpEndpoint`] with transport-level defaults, so downstream code sees
/// concrete values.
pub fn resolve_endpoint_defaults(
    backend: &HttpTransportBackend,
    endpoint: Cow<'_, Endpoint>,
) -> Endpoint {
    endpoint.into_owned().map::<HttpEndpoint>(|http| {
        let mut http = http;
        if http.base_url().is_none() && !backend.base_url.is_empty() {
            http = http.with_base_url(backend.base_url.clone());
        }
        http
    })
}

// ── Unified pipeline ──────────────────────────────────────────────

/// Transport 级默认值（仅用于长任务轮询 endpoint 回填）。
///
/// 轮询的 `/task/query` 等 endpoint 必须落到 **transport 级** base_url（而非
/// 请求 endpoint 的 base_url）——见 `dispatch` 用例
/// `on_poll_uses_backend_base_url_not_endpoint_base_url`。
#[derive(Clone, Copy, Default)]
pub struct PollDefaults<'a> {
    /// Transport 级默认 base_url（poll endpoint 缺省时回填）。
    pub base_url: &'a str,
}

/// 唯一一份 HTTP 协议流水线：请求信封 wrap → range 回填 → 发送（签名等
/// 发送前加工由 [`HttpClient`] 实现自行内联）→ 二进制/续传 → 响应信封 parse →
/// 长任务轮询 → 抽取。
pub async fn pipeline_execute<'a>(
    client: Arc<dyn HttpClient>,
    endpoint: &'a Endpoint,
    payload: HttpRequestPayload<'a>,
    options: RequestOptions,
    defaults: PollDefaults<'a>,
) -> Result<TransportResponse> {
    // 请求信封 wrap + range 回填。
    let payload = apply_request_envelope(endpoint, payload);
    let ranged = compute_ranged(endpoint, &payload);

    let mut wire = options.wire.clone();
    if let Some((size, _)) = ranged.as_ref() {
        wire.headers.insert(
            reqwest::header::RANGE,
            resumable::range_header_value(0, *size)?,
        );
    }

    let req = HttpRequest::new(&*client, Cow::Borrowed(endpoint), payload).with_options(wire);
    let response = req.await?;

    // 非 JSON → 二进制透传（可选续传）。
    if !response.is_json() {
        return Ok(pipeline_binary(
            client,
            response,
            ranged,
            endpoint.clone(),
            options.wire,
        ));
    }

    decode_response(&*client, endpoint, options, response, defaults).await
}

/// Turn a non-JSON first response into a [`TransportResponse::Binary`].
///
/// When the first response is partial (`206` or carries `Content-Range`)
/// and a ranged download was requested, its body is replaced with an
/// auto-resuming stream that replays the request (through the cloned
/// [`HttpClient`]) for each Range segment; otherwise the response is passed
/// through untouched.
pub(super) fn pipeline_binary(
    client: Arc<dyn HttpClient>,
    response: crate::http_client::HttpResponse,
    ranged: Option<(u64, serde_json::Value)>,
    endpoint: Endpoint,
    wire: WireOptions,
) -> TransportResponse {
    let is_partial = response.status().as_u16() == 206 || response.content_range().is_some();
    let Some((size, replay)) = ranged.filter(|_| is_partial) else {
        return TransportResponse::Binary(response);
    };

    let resumable = resumable::into_resumable(response, size, move |start, chunk| {
        let client = client.clone();
        let endpoint = endpoint.clone();
        let payload = replay.clone();
        let mut wire = wire.clone();
        async move {
            wire.headers.insert(
                reqwest::header::RANGE,
                resumable::range_header_value(start, chunk)?,
            );
            client.post(endpoint, payload).with_options(wire).await
        }
    });
    TransportResponse::Binary(resumable)
}

// ── Long-task polling ─────────────────────────────────────────────

/// Follow a long-task poll loop when the first response carries a non-empty
/// `taskid`; otherwise return `data` unchanged.
pub(super) async fn poll_if_long_task<'a>(
    client: &'a dyn HttpClient,
    data: protocol::ApiResponse,
    endpoint: &'a Endpoint,
    options: RequestOptions,
    res: &'a dyn ResponseEnvelope,
    defaults: PollDefaults<'a>,
) -> Result<protocol::ApiResponse> {
    let Some(taskid) = data.taskid.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(data);
    };
    let ctx = polling::HttpPollContext {
        http_client: client,
        options,
        poll_mode: data.poll_mode.unwrap_or_default(),
        base_url: defaults.base_url,
        request_endpoint: endpoint,
        res_envelope: res,
    };
    polling::poll_long_task(&ctx, taskid).await
}

// ── Response decoding ─────────────────────────────────────────────

/// Decode pipeline for JSON responses: read the endpoint's response-side
/// [`ResponseEnvelope`] strategy (default [`GatewayRes`](super::envelope::GatewayRes))
/// to parse the body into a normalized [`protocol::ApiResponse`], follow
/// long-task polling (no-op without a `taskid`), then extract `result`.
pub(super) async fn decode_response<'a>(
    client: &'a dyn HttpClient,
    endpoint: &'a Endpoint,
    options: RequestOptions,
    response: crate::http_client::HttpResponse,
    defaults: PollDefaults<'a>,
) -> Result<TransportResponse> {
    let endpoint_url = endpoint.full_url();

    // Response-side envelope: parse body JSON into a normalized ApiResponse
    // (protocol unwrap + error validation). `json_parse` keeps the
    // `http.request` span alive through the parse and records `error` on it.
    let res = endpoint.res_envelope();
    let data = response
        .json_parse(|body| res.decode(&endpoint_url, body))
        .await?;

    // Long-task polling (no-op when there is no taskid).
    let data = poll_if_long_task(client, data, endpoint, options, res, defaults).await?;

    // Extract `result` and decode it into a business Value.
    decode_result(data, endpoint_url)
}

// ── Result decoding ───────────────────────────────────────────────

/// Extract the protocol `result` field and decode it into a business
/// [`TransportResponse::Json`]. Missing or malformed `result` maps to
/// [`Error::Parse`].
pub(super) fn decode_result(
    mut data: protocol::ApiResponse,
    endpoint_url: String,
) -> Result<TransportResponse> {
    let result_str = data.result.take().unwrap_or_default();
    if result_str.is_empty() {
        return Err(Error::Parse {
            message: "API response missing `result` field".to_string(),
            endpoint: endpoint_url,
            body: Box::new(serde_json::to_value(&data).unwrap_or_default()),
            source: None,
        })
        .inspect_err(|e| tracing::error!(error = %e, "API response missing result field"));
    }
    match serde_json::from_str::<serde_json::Value>(&result_str) {
        Ok(value) => Ok(TransportResponse::Json(ExecuteOutput {
            result: value,
            extra: data.extra,
        })),
        Err(e) => Err(Error::Parse {
            message: format!("Parse `result` JSON failed: {e:#}"),
            endpoint: endpoint_url,
            body: Box::new(serde_json::Value::String(result_str)),
            source: Some(e),
        })
        .inspect_err(|e| tracing::error!(error = %e, "parse result JSON failed")),
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[allow(clippy::needless_update)]
#[cfg(test)]
mod tests {
    //! ## 模块摘要：http::request（执行流水线）
    //!
    //! ### 关键接口
    //! - [apply_request_envelope] — 应用 endpoint 信封策略的请求侧封装
    //! - [compute_ranged] — 计算分片下载的 chunk size 与可重放 payload
    //! - [resolve_endpoint_defaults] — 用 transport 级默认值填充 endpoint
    //! - [pipeline_binary] — 非 JSON 响应→ Binary，进入续拉流（可选）
    //! - [poll_if_long_task] — 长任务轮询，无 taskid 时 no-op
    //! - [decode_result] — 提取 API 响应 `result` 字段并解析为业务 JSON
    //!
    //! ### 关键分支与异常路径
    //! - execute 普通 JSON 成功 → TransportResponse::Json
    //! - execute 非 JSON Content-Type → TransportResponse::Binary（含 Range 续拉）
    //! - execute error.code != 0 → Error::Api
    //! - execute result 缺失/非法 JSON → Error::Parse
    //! - execute header_error 短路 → 立即返回错误
    //! - 长任务轮询透传 transport.headers + 请求级 headers
    //! - ranged 下载：range_size=Some(n>0) + JSON payload 进入续拉，0/multipart 为 no-op
    //!
    //! ### 上下游交互
    //! - 上游：[super::HttpTransportBackend] 的 execute() 方法
    //! - 下游：polling、protocol、resumable 子模块

    use std::borrow::Cow;
    use std::sync::Arc;

    use futures_util::StreamExt;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::http::envelope::RequestEnvelope;
    use crate::http::{EndpointHttpExt, HttpEndpoint};
    use crate::traits::TransportResponse;
    use crate::{Endpoint, HttpTransportBackend, PollEvent, Transport};

    /// 测试用自定义请求侧信封（见 03 §4.2）。
    #[derive(Debug, Clone, Copy, Default)]
    struct WrapPayloadReq;
    impl RequestEnvelope for WrapPayloadReq {
        fn encode(&self, payload: serde_json::Value) -> serde_json::Value {
            serde_json::json!({ "payload": payload.to_string() })
        }
        fn name(&self) -> &'static str {
            "wrap-payload"
        }
    }

    // ── test helpers ──────────────────────────────────────────

    fn ep(base: &str, path: &str) -> Endpoint {
        let http = HttpEndpoint::new(path).with_service(base);
        Endpoint::new().with(http)
    }

    fn make_transport() -> HttpTransportBackend {
        HttpTransportBackend::default()
    }

    fn make_transport_with_header(
        name: &'static str,
        value: &'static str,
        base_url: &str,
    ) -> Transport {
        HttpTransportBackend::builder()
            .base_url(base_url)
            .header(name, value)
            .build()
            .expect("valid header name/value")
    }

    struct HeaderEq {
        name: &'static str,
        value: &'static str,
    }
    impl Match for HeaderEq {
        fn matches(&self, request: &Request) -> bool {
            request.headers.get(self.name).and_then(|v| v.to_str().ok()) == Some(self.value)
        }
    }

    struct AllHeadersEq(Vec<(&'static str, &'static str)>);
    impl Match for AllHeadersEq {
        fn matches(&self, request: &Request) -> bool {
            self.0
                .iter()
                .all(|(n, v)| request.headers.get(*n).and_then(|x| x.to_str().ok()) == Some(*v))
        }
    }

    // ── execute() main flow: JSON success ─────────────────────

    /// P0：[TransportRequest] 普通 JSON 成功响应返回 Json variant
    #[tokio::test]
    async fn execute_returns_json_on_simple_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cgi/x"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "{\"ok\":true}"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let transport = make_transport();
        let endpoint = ep(&server.uri(), "/cgi/x");
        let resp = transport.invoke(&endpoint, json!({})).await.unwrap();
        let v = resp.into_result().unwrap();
        assert_json_diff::assert_json_eq!(v, json!({"ok": true}));
    }

    // ── execute() main flow: Binary (non-JSON) ────────────────

    /// P0：[TransportRequest] 非 JSON Content-Type 响应包装为 Binary 透传
    #[tokio::test]
    async fn execute_returns_binary_on_non_json_content_type() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cgi/csv"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(b"a,b\n1,2".to_vec())
                    .insert_header("Content-Type", "text/csv"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let transport = make_transport();
        let endpoint = ep(&server.uri(), "/cgi/csv");
        let resp = transport.invoke(&endpoint, json!({})).await.unwrap();
        assert!(matches!(resp, TransportResponse::Binary(_)));
        let err = resp.into_json().unwrap_err();
        assert!(matches!(err, Error::Parse { .. }));
    }

    // ── Ranged download: segmented resume opt-in ──────────────

    /// P1：[TransportRequest] ranged 请求遇 200 无 Content-Range 时原样透传
    #[tokio::test]
    async fn execute_ranged_200_without_content_range_passes_through() {
        let server = MockServer::start().await;
        let body = vec![0x5Au8; 12];
        Mock::given(method("POST"))
            .and(path("/dl"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/octet-stream")
                    .insert_header("Content-Length", "12")
                    .set_body_bytes(body.clone()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let transport = make_transport();
        let endpoint = ep(&server.uri(), "/dl").with_range_size(Some(10));
        let resp = transport.invoke(&endpoint, json!({})).await.unwrap();
        assert!(matches!(resp, TransportResponse::Binary(_)));

        let mut stream = resp.into_binary().unwrap().bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(buf, body);
    }

    /// P0：[TransportRequest] ranged 请求遇 206 进入续拉并拼接多段
    #[tokio::test]
    async fn execute_ranged_206_resumes_and_concatenates() {
        let server = MockServer::start().await;
        let seg0 = vec![0xA5u8; 10];
        let seg1 = vec![0x5Au8; 10];
        let mut expected = seg0.clone();
        expected.extend_from_slice(&seg1);

        Mock::given(method("POST"))
            .and(path("/dl"))
            .and(HeaderEq {
                name: "range",
                value: "bytes=0-9",
            })
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Type", "application/octet-stream")
                    .insert_header("Content-Range", "bytes 0-9/20")
                    .set_body_bytes(seg0.clone()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/dl"))
            .and(HeaderEq {
                name: "range",
                value: "bytes=10-19",
            })
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Type", "application/octet-stream")
                    .insert_header("Content-Range", "bytes 10-19/20")
                    .set_body_bytes(seg1),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let transport = make_transport();
        let endpoint = ep(&server.uri(), "/dl").with_range_size(Some(10));
        let resp = transport.invoke(&endpoint, json!({})).await.unwrap();

        let mut stream = resp.into_binary().unwrap().bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(buf, expected);
        assert_eq!(buf.len(), 20);
    }

    /// P1：[TransportRequest] ranged(0) 为 no-op，无 Range 头且不进续拉
    #[tokio::test]
    async fn execute_ranged_zero_size_is_noop() {
        let server = MockServer::start().await;
        let body = vec![0xB0u8; 8];
        Mock::given(method("POST"))
            .and(path("/dl"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Type", "application/octet-stream")
                    .insert_header("Content-Range", "bytes 0-7/8")
                    .set_body_bytes(body.clone()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let transport = make_transport();
        let endpoint = ep(&server.uri(), "/dl").with_range_size(Some(0));
        let resp = transport.invoke(&endpoint, json!({})).await.unwrap();
        assert!(matches!(resp, TransportResponse::Binary(_)));

        let mut stream = resp.into_binary().unwrap().bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(buf, body);
        assert_eq!(buf.len(), 8);
    }

    /// P1：[TransportRequest] ranged(1) 最小分片单字节精确终止
    #[tokio::test]
    async fn execute_ranged_minimum_size_one_byte_exact_termination() {
        let server = MockServer::start().await;
        let body = vec![0xCCu8; 1];

        Mock::given(method("POST"))
            .and(path("/dl"))
            .and(HeaderEq {
                name: "range",
                value: "bytes=0-0",
            })
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Type", "application/octet-stream")
                    .insert_header("Content-Range", "bytes 0-0/1")
                    .set_body_bytes(body.clone()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let transport = make_transport();
        let endpoint = ep(&server.uri(), "/dl").with_range_size(Some(1));
        let resp = transport.invoke(&endpoint, json!({})).await.unwrap();
        assert!(matches!(resp, TransportResponse::Binary(_)));

        let mut stream = resp.into_binary().unwrap().bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(buf, body);
        assert_eq!(buf.len(), 1);
    }

    /// P1：[TransportRequest] multipart 负载上 ranged 为 no-op
    #[tokio::test]
    async fn execute_ranged_multipart_payload_is_noop() {
        let server = MockServer::start().await;
        let body = vec![0xEEu8; 8];
        Mock::given(method("POST"))
            .and(path("/dl"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Type", "application/octet-stream")
                    .set_body_bytes(body.clone()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let transport = make_transport();
        let endpoint = ep(&server.uri(), "/dl").with_range_size(Some(10));
        let form = reqwest::multipart::Form::new().text("k", "v");
        let resp = transport.invoke(&endpoint, form).await.unwrap();
        assert!(matches!(resp, TransportResponse::Binary(_)));

        let mut stream = resp.into_binary().unwrap().bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(buf, body);
    }

    /// P1：[TransportRequest] ranged 遇 200 + Content-Range 仍进续拉
    #[tokio::test]
    async fn execute_ranged_200_with_content_range_resumes() {
        let server = MockServer::start().await;
        let seg0 = vec![0xABu8; 10];
        let seg1 = vec![0xBAu8; 10];
        let mut expected = seg0.clone();
        expected.extend_from_slice(&seg1);

        Mock::given(method("POST"))
            .and(path("/dl"))
            .and(HeaderEq {
                name: "range",
                value: "bytes=0-9",
            })
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/octet-stream")
                    .insert_header("Content-Range", "bytes 0-9/20")
                    .set_body_bytes(seg0.clone()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/dl"))
            .and(HeaderEq {
                name: "range",
                value: "bytes=10-19",
            })
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Type", "application/octet-stream")
                    .insert_header("Content-Range", "bytes 10-19/20")
                    .set_body_bytes(seg1),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let transport = make_transport();
        let endpoint = ep(&server.uri(), "/dl").with_range_size(Some(10));
        let resp = transport.invoke(&endpoint, json!({})).await.unwrap();
        assert!(matches!(resp, TransportResponse::Binary(_)));

        let mut stream = resp.into_binary().unwrap().bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(buf, expected);
        assert_eq!(buf.len(), 20);
    }

    /// P1：[TransportRequest] ranged 遇 Content-Range: x-y/* 靠 416 探测终止
    #[tokio::test]
    async fn execute_ranged_unknown_total_416_confirms_end() {
        let server = MockServer::start().await;
        let body = vec![0x7Fu8; 10];

        Mock::given(method("POST"))
            .and(path("/dl"))
            .and(HeaderEq {
                name: "range",
                value: "bytes=0-9",
            })
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Type", "application/octet-stream")
                    .insert_header("Content-Range", "bytes 0-9/*")
                    .set_body_bytes(body.clone()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/dl"))
            .and(HeaderEq {
                name: "range",
                value: "bytes=10-19",
            })
            .respond_with(ResponseTemplate::new(416))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let transport = make_transport();
        let endpoint = ep(&server.uri(), "/dl").with_range_size(Some(10));
        let resp = transport.invoke(&endpoint, json!({})).await.unwrap();
        assert!(matches!(resp, TransportResponse::Binary(_)));

        let mut stream = resp.into_binary().unwrap().bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(buf, body);
    }

    // ── execute() protocol error: error.code != 0 ─────────────

    /// P1：[TransportRequest] error.code != 0 触发 Error::Api
    #[tokio::test]
    async fn execute_maps_error_code_to_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cgi/err"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": {"code": 40001, "message": "bad"},
                "result": ""
            })))
            .expect(1)
            .mount(&server)
            .await;

        let transport = make_transport();
        let endpoint = ep(&server.uri(), "/cgi/err");
        let err = transport.invoke(&endpoint, json!({})).await.unwrap_err();
        match err {
            Error::Api { code, message, .. } => {
                assert_eq!(code, Some(40001));
                assert_eq!(message, "bad");
            }
            other => panic!("expected Error::Api, got {other:?}"),
        }
    }

    // ── execute() Parse error: result missing / invalid JSON ──

    /// P1：[TransportRequest] result 字段为空 → Error::Parse
    #[tokio::test]
    async fn execute_missing_result_field_returns_parse_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cgi/empty"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": {"code": 0}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let transport = make_transport();
        let endpoint = ep(&server.uri(), "/cgi/empty");
        let err = transport.invoke(&endpoint, json!({})).await.unwrap_err();
        match err {
            Error::Parse { message, .. } => {
                assert!(
                    message.contains("missing `result`"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected Error::Parse, got {other:?}"),
        }
    }

    /// P1：[TransportRequest] result 非合法 JSON → Error::Parse
    #[tokio::test]
    async fn execute_invalid_result_json_returns_parse_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cgi/bad"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "not json{"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let transport = make_transport();
        let endpoint = ep(&server.uri(), "/cgi/bad");
        let err = transport.invoke(&endpoint, json!({})).await.unwrap_err();
        match err {
            Error::Parse { message, .. } => {
                assert!(
                    message.contains("Parse `result` JSON failed"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected Error::Parse, got {other:?}"),
        }
    }

    // ── execute() header_error short-circuit ──────────────────

    /// P1：[TransportRequest] self.header_error 短路，立即返回错误
    #[tokio::test]
    async fn execute_short_circuits_on_header_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cgi/never"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let transport = make_transport();
        let endpoint = ep(&server.uri(), "/cgi/never");
        let err = transport
            .invoke(&endpoint, json!({}))
            .header("x-bad", "\u{0001}\u{0002}")
            .await
            .unwrap_err();
        let _ = err;
    }

    // ── Long-task polling: header forwarding ──────────────────

    /// P0：[TransportRequest] 长任务轮询请求透传 transport.headers
    #[tokio::test]
    async fn execute_polling_request_carries_transport_headers() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/cgi/start"))
            .and(HeaderEq {
                name: "x-base",
                value: "base-val",
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "taskid": "tid-001"
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/task/query"))
            .and(HeaderEq {
                name: "x-base",
                value: "base-val",
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "{\"done\":1}",
                "long_task_poll": {"done": true}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let transport = {
            let uri = server.uri();
            make_transport_with_header("x-base", "base-val", &uri)
        };
        let endpoint = ep(&server.uri(), "/cgi/start");
        let resp = transport.invoke(&endpoint, json!({})).await.unwrap();
        let v = resp.into_result().unwrap();
        assert_json_diff::assert_json_eq!(v, json!({"done": 1}));
    }

    /// P0：[TransportRequest] 轮询请求透传请求级 .headers() 设置的 header
    #[tokio::test]
    async fn execute_polling_request_carries_request_level_headers() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/cgi/start"))
            .and(HeaderEq {
                name: "x-extra",
                value: "extra-val",
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "taskid": "tid-002"
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/task/query"))
            .and(HeaderEq {
                name: "x-extra",
                value: "extra-val",
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "{\"k\":\"v\"}",
                "long_task_poll": {"done": true}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let transport = {
            let uri = server.uri();
            HttpTransportBackend {
                http_client: std::sync::Arc::new(reqwest::Client::new()),
                base_url: uri,
                ..Default::default()
            }
        };
        let endpoint = ep(&server.uri(), "/cgi/start");
        let resp = transport
            .invoke(&endpoint, json!({}))
            .header("x-extra", "extra-val")
            .await
            .unwrap();
        let v = resp.into_result().unwrap();
        assert_json_diff::assert_json_eq!(v, json!({"k": "v"}));
    }

    /// P0：[TransportRequest] 轮询请求同时透传 transport.headers + 请求级 headers
    #[tokio::test]
    async fn execute_polling_request_carries_combined_headers() {
        let server = MockServer::start().await;

        let combined = AllHeadersEq(vec![("x-base", "b"), ("x-extra", "e")]);
        Mock::given(method("POST"))
            .and(path("/cgi/start"))
            .and(AllHeadersEq(vec![("x-base", "b"), ("x-extra", "e")]))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "taskid": "tid-003"
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/task/query"))
            .and(combined)
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "{}",
                "long_task_poll": {"done": true}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let transport = {
            let uri = server.uri();
            make_transport_with_header("x-base", "b", &uri)
        };
        let endpoint = ep(&server.uri(), "/cgi/start");
        let resp = transport
            .invoke(&endpoint, json!({}))
            .header("x-extra", "e")
            .await
            .unwrap();
        assert_json_diff::assert_json_eq!(resp.into_result().unwrap(), json!({}));
    }

    /// P1：[TransportRequest] 多轮轮询请求每一轮都透传 headers
    #[tokio::test]
    async fn execute_multi_round_polling_each_carries_headers() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/cgi/start"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "taskid": "tid-multi"
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/task/query"))
            .and(HeaderEq {
                name: "x-base",
                value: "b",
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "long_task_poll": {"done": false}
            })))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/task/query"))
            .and(HeaderEq {
                name: "x-base",
                value: "b",
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "{\"done\":true}",
                "long_task_poll": {"done": true}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let transport = {
            let uri = server.uri();
            make_transport_with_header("x-base", "b", &uri)
        };
        let endpoint = ep(&server.uri(), "/cgi/start");
        let resp = transport.invoke(&endpoint, json!({})).await.unwrap();
        assert_json_diff::assert_json_eq!(resp.into_result().unwrap(), json!({"done": true}));
    }

    /// P1：[TransportRequest] on_poll 在轮询过程中被回调
    #[tokio::test]
    async fn execute_on_poll_invoked_during_polling() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/cgi/start"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "taskid": "tid-tick"
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/task/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "long_task_poll": {"done": false}
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/task/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "{\"partial\":1}",
                "long_task_poll": {"done": false}
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/task/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "{\"final\":2}",
                "long_task_poll": {"done": true}
            })))
            .mount(&server)
            .await;

        let events: Arc<std::sync::Mutex<Vec<Option<serde_json::Value>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_cb = events.clone();
        let transport = {
            let uri = server.uri();
            HttpTransportBackend {
                http_client: std::sync::Arc::new(reqwest::Client::new()),
                base_url: uri,
                ..Default::default()
            }
        };
        let endpoint = ep(&server.uri(), "/cgi/start");

        let resp = transport
            .invoke(&endpoint, json!({}))
            .on_poll(move |ev: &PollEvent<'_>| {
                events_cb.lock().unwrap().push(ev.result.cloned());
            })
            .await
            .unwrap();

        assert_json_diff::assert_json_eq!(resp.into_result().unwrap(), json!({"final": 2}));

        let got = events.lock().unwrap().clone();
        assert_eq!(got.len(), 2, "应在 2 个非终态轮触发，实际：{got:?}");
        assert!(
            got[0].is_none(),
            "第 1 轮 result 缺失，event.result 应为 None"
        );
        assert_eq!(got[1], Some(json!({"partial": 1})));
    }

    // ── Envelope-driven body wrapping ─────────────────────────

    /// P0：[HttpTransportBackend::execute] endpoint 挂自定义请求信封（wrap-payload）时把
    ///      JSON body 包裹为 `{"payload": "<json-string>"}`
    #[tokio::test]
    async fn execute_wraps_payload_when_endpoint_opts_in() {
        struct BodyMatcher(serde_json::Value);
        impl Match for BodyMatcher {
            fn matches(&self, req: &Request) -> bool {
                serde_json::from_slice::<serde_json::Value>(&req.body)
                    .map(|b| b == self.0)
                    .unwrap_or(false)
            }
        }

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cgi/act"))
            .and(BodyMatcher(
                json!({ "payload": json!({"a": 1}).to_string() }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": {"code": 0},
                "result": json!({"ok": true}).to_string()
            })))
            .mount(&server)
            .await;

        let endpoint = ep(&server.uri(), "/cgi/act").with_req_envelope(WrapPayloadReq);
        let resp = make_transport()
            .invoke(&endpoint, json!({"a": 1}))
            .await
            .unwrap();
        assert_json_diff::assert_json_eq!(resp.into_result().unwrap(), json!({"ok": true}));
    }

    /// P0：[HttpTransportBackend::execute] 默认（无 req envelope）时 body 原样发送
    #[tokio::test]
    async fn execute_does_not_wrap_payload_by_default() {
        use wiremock::matchers::body_json;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cgi/act"))
            .and(body_json(json!({"a": 1})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": {"code": 0},
                "result": json!({"ok": true}).to_string()
            })))
            .mount(&server)
            .await;

        let endpoint = ep(&server.uri(), "/cgi/act");
        assert_eq!(endpoint.req_envelope().name(), "passthrough");
        let resp = make_transport()
            .invoke(&endpoint, json!({"a": 1}))
            .await
            .unwrap();
        assert_json_diff::assert_json_eq!(resp.into_result().unwrap(), json!({"ok": true}));
    }

    // ── apply_request_envelope ────────────────────────────────

    /// P0：[apply_request_envelope] 自定义请求信封（wrap-payload）的 endpoint 将 JSON 包裹进信封
    #[test]
    fn wrap_payload_enabled_wraps_json_envelope() {
        use crate::http_client::HttpRequestPayload;
        let endpoint = ep("https://x.com", "/p").with_req_envelope(WrapPayloadReq);
        let payload = HttpRequestPayload::Json(Cow::Owned(json!({"a": 1})));
        let result = apply_request_envelope(&endpoint, payload);
        match result {
            HttpRequestPayload::Json(cow) => {
                assert_eq!(cow["payload"], json!({"a": 1}).to_string());
            }
            _ => panic!("expected Json payload, got Form"),
        }
    }

    /// P0：[apply_request_envelope] 无 envelope（默认）时不做包裹
    #[test]
    fn wrap_payload_disabled_passes_through() {
        use crate::http_client::HttpRequestPayload;
        let endpoint = ep("https://x.com", "/p");
        let payload = HttpRequestPayload::Json(Cow::Owned(json!({"a": 1})));
        let result = apply_request_envelope(&endpoint, payload);
        match result {
            HttpRequestPayload::Json(cow) => {
                assert_eq!(*cow, json!({"a": 1}));
            }
            _ => panic!("expected Json payload"),
        }
    }

    // ── compute_ranged ────────────────────────────────────────

    /// P0：[compute_ranged] range_size=Some(n>0) + JSON payload 时返回 Some
    #[test]
    fn compute_ranged_returns_some_for_json_with_positive_range() {
        use crate::http_client::HttpRequestPayload;
        let endpoint = ep("https://x.com", "/p").with_range_size(Some(10));
        let payload = HttpRequestPayload::Json(Cow::Owned(json!({"a": 1})));
        let result = compute_ranged(&endpoint, &payload);
        assert!(result.is_some());
        let (size, replay) = result.unwrap();
        assert_eq!(size, 10);
        assert_eq!(replay, json!({"a": 1}));
    }

    /// P0：[compute_ranged] range_size=None 时返回 None
    #[test]
    fn compute_ranged_returns_none_for_missing_range_size() {
        use crate::http_client::HttpRequestPayload;
        let endpoint = ep("https://x.com", "/p");
        let payload = HttpRequestPayload::Json(Cow::Owned(json!({"a": 1})));
        assert!(compute_ranged(&endpoint, &payload).is_none());
    }

    /// P0：[compute_ranged] range_size=Some(0) 时返回 None
    #[test]
    fn compute_ranged_zero_size_is_none() {
        use crate::http_client::HttpRequestPayload;
        let endpoint = ep("https://x.com", "/p").with_range_size(Some(0));
        let payload = HttpRequestPayload::Json(Cow::Owned(json!({"a": 1})));
        assert!(compute_ranged(&endpoint, &payload).is_none());
    }

    /// P1：[compute_ranged] Form payload + range_size=Some(n) 时返回 None
    #[test]
    fn compute_ranged_returns_none_for_form_payload() {
        use crate::http_client::HttpRequestPayload;
        let endpoint = ep("https://x.com", "/p").with_range_size(Some(10));
        let form = reqwest::multipart::Form::new();
        let payload = HttpRequestPayload::Form(form);
        assert!(compute_ranged(&endpoint, &payload).is_none());
    }

    // ── decode_result ─────────────────────────────────────────

    /// P0：[decode_result] 合法 JSON result 字段正确解析
    #[test]
    fn decode_result_parses_valid_json() {
        let data = protocol::ApiResponse {
            result: Some("{\"ok\":true}".to_string()),
            error: None,
            taskid: None,
            long_task_poll: None,
            poll_mode: None,
            extra: indexmap::IndexMap::new(),
        };
        let outcome = decode_result(data, "https://x.test/cgi".into()).unwrap();
        let output = outcome.into_result().unwrap();
        assert_eq!(output, json!({"ok": true}));
    }

    /// P0：[decode_result] result 缺失时返回 Error::Parse
    #[test]
    fn decode_result_errors_when_result_missing() {
        let data = protocol::ApiResponse {
            result: None,
            error: None,
            taskid: None,
            long_task_poll: None,
            poll_mode: None,
            extra: indexmap::IndexMap::new(),
        };
        let err = decode_result(data, "https://x.test/cgi".into()).unwrap_err();
        match err {
            Error::Parse { message, .. } => {
                assert!(message.contains("missing `result`"), "got: {message}");
            }
            _ => panic!("expected Error::Parse, got: {err:?}"),
        }
    }

    /// P0：[decode_result] result 为空字符串时返回 Error::Parse
    #[test]
    fn decode_result_errors_when_result_empty() {
        let data = protocol::ApiResponse {
            result: Some(String::new()),
            error: None,
            taskid: None,
            long_task_poll: None,
            poll_mode: None,
            extra: indexmap::IndexMap::new(),
        };
        let err = decode_result(data, "https://x.test/cgi".into()).unwrap_err();
        assert!(matches!(err, Error::Parse { .. }));
    }

    /// P0：[decode_result] result 不为合法 JSON 时返回 Error::Parse
    #[test]
    fn decode_result_errors_when_result_invalid_json() {
        let data = protocol::ApiResponse {
            result: Some("not json".to_string()),
            error: None,
            taskid: None,
            long_task_poll: None,
            poll_mode: None,
            extra: indexmap::IndexMap::new(),
        };
        let err = decode_result(data, "https://x.test/cgi".into()).unwrap_err();
        match err {
            Error::Parse { message, .. } => {
                assert!(message.contains("Parse `result`"), "got: {message}");
            }
            _ => panic!("expected Error::Parse, got: {err:?}"),
        }
    }

    // ── resolve_endpoint_defaults ─────────────────────────────

    /// P0：[resolve_endpoint_defaults] endpoint 缺 base_url 时使用 transport 默认值
    #[test]
    fn resolve_endpoint_defaults_fills_missing_base_url() {
        let transport = HttpTransportBackend {
            http_client: std::sync::Arc::new(reqwest::Client::new()),
            base_url: "https://default.example.com".into(),
            ..Default::default()
        };
        let endpoint = Endpoint::new().with(HttpEndpoint::new("/cgi/test"));
        let resolved = resolve_endpoint_defaults(&transport, Cow::Owned(endpoint));
        assert_eq!(resolved.base_url(), "https://default.example.com");
    }

    /// P0：[resolve_endpoint_defaults] endpoint 已有 base_url 时不覆盖
    #[test]
    fn resolve_endpoint_defaults_preserves_existing_base_url() {
        let transport = HttpTransportBackend {
            http_client: std::sync::Arc::new(reqwest::Client::new()),
            base_url: "https://default.example.com".into(),
            ..Default::default()
        };
        let endpoint = Endpoint::new()
            .with(HttpEndpoint::new("/cgi/test").with_base_url("https://explicit.example.com"));
        let resolved = resolve_endpoint_defaults(&transport, Cow::Owned(endpoint));
        assert_eq!(resolved.base_url(), "https://explicit.example.com");
    }

    /// P1：[resolve_endpoint_defaults] transport 无 base_url 且 endpoint 也无时保持 None
    #[test]
    fn resolve_endpoint_defaults_empty_transport_base_url_is_noop() {
        let transport = HttpTransportBackend::default();
        let endpoint = Endpoint::new().with(HttpEndpoint::new("/cgi/test"));
        let resolved = resolve_endpoint_defaults(&transport, Cow::Owned(endpoint));
        assert_eq!(resolved.base_url(), "");
    }

    // ── pipeline_binary ───────────────────────────────────────

    /// P1：[pipeline_binary] 200 不经 Range 续拉，直接透传
    #[tokio::test]
    async fn pipeline_binary_pass_through_non_partial() {
        let transport = HttpTransportBackend::default();
        let resp = transport
            .post(ep("http://0.0.0.0:1", "/nop"), json!({}))
            .with_options(WireOptions {
                headers: {
                    let mut h = reqwest::header::HeaderMap::new();
                    h.insert("x-test", reqwest::header::HeaderValue::from_static("1"));
                    h
                },
                ..Default::default()
            })
            .execute()
            .await;
        // A connection refused error is fine here — we only test that the
        // function path compiles and the type signatures are correct. The
        // real behaviour is covered by execute_ranged_* tests.
        assert!(
            resp.is_err(),
            "expected network error connecting to 0.0.0.0:1"
        );
    }
}
