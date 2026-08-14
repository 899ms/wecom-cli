//! Building blocks for implementing custom [`TransportBackend`](crate::TransportBackend)s.
//!
//! The crate root exposes the **consumer** API — [`Transport`](crate::Transport),
//! [`Endpoint`](crate::Endpoint), [`TransportRequest`](crate::TransportRequest),
//! [`HttpTransportBackend`](crate::HttpTransportBackend), etc. — used to *send*
//! requests.
//!
//! This module gathers the **implementer** API: the response protocol types,
//! the long-task polling framework, the resumable-download state machine, and
//! the request-envelope helpers that a custom backend reuses inside its
//! [`TransportBackend::execute`](crate::TransportBackend::execute). The built-in
//! `HttpTransportBackend` is assembled from these pieces.

/// WeCom API response protocol: response shape plus business error-code validation.
///
/// [`validate_api_response`] turns an `error.code != 0` body into
/// [`Error::Api`](crate::Error::Api); deserialization itself is done by the
/// caller via `HttpResponse::json::<ApiResponse>()`.
pub mod protocol {
    pub use crate::http::protocol::{ApiErrorInfo, ApiResponse, validate_api_response};
}

/// Generic long-task polling framework shared by the HTTP backend and custom backends.
///
/// A backend injects a per-round `fetch` closure into [`poll_long_task`]; the
/// framework owns retry / backoff / timeout / `done` detection.
/// [`LongTaskPollData`] adapts a backend's response type into the framework,
/// and [`PollMode`] selects the wire mode.
pub mod polling {
    pub use crate::polling::{LongTaskPollData, LongTaskPollInfo, PollMode, poll_long_task};
}

/// HTTP Range resumable-download assembly.
///
/// [`into_resumable`] wraps a partial (`206` / `Content-Range`) first response
/// into an auto-resuming stream; the caller builds each segment's `Range`
/// header via [`range_header_value`] and owns per-request signing / routing.
pub mod resumable {
    pub use crate::http::resumable::{into_resumable, range_header_value};
}

/// WeCom HTTP gateway request-envelope helpers, applied before a request goes out.
///
/// [`apply_request_envelope`] applies the endpoint's request-side envelope
/// wrapping (e.g. `{"payload": "<json-string>"}`); [`compute_ranged`]
/// precomputes the clamped chunk size and replay payload for an eligible
/// ranged download.
pub mod envelope {
    pub use crate::http::{apply_request_envelope, compute_ranged};
}

/// 唯一一份 HTTP 协议流水线。
///
/// [`pipeline_execute`] 统一承载：请求信封 wrap → 发送（签名等发送前加工由
/// [`crate::HttpClient`] 实现内联）→ 二进制/续传 → 响应信封 parse（从 endpoint
/// 读取）→ 长任务轮询 → 抽取。`HttpTransportBackend` 及自定义后端均复用它。
pub mod pipeline {
    pub use crate::http::request::{PollDefaults, pipeline_execute};
}

/// Fill an endpoint's `None` `base_url` with the
/// backend's transport-level defaults, so downstream code sees concrete
/// values. Used by custom backends that embed an
/// [`HttpTransportBackend`](crate::HttpTransportBackend).
pub use crate::http::request::resolve_endpoint_defaults;
