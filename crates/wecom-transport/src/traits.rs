//! Transport trait definitions — open extension points.
//!
//! This module defines the trait shape for transport backends.
//! Concrete implementations are provided by the built-in reference
//! transport (`HttpTransportBackend`) and, in the future,
//! by external crates.

use std::borrow::Cow;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;

use crate::http_client::HttpRequestPayload;
use crate::{Endpoint, ExecuteOutput, HttpResponse, RequestOptions, Result};

// ── Transport trait ──────────────────────────────────────────

/// Transport backend abstraction.
///
/// Custom transports only need to implement [`execute`](TransportBackend::execute).
///
/// # Object safety
///
/// This trait returns `Pin<Box<dyn Future>>` (instead of RPITIT) so it is
/// **dyn-compatible**. This enables [`crate::Transport`] to hold an
/// `Arc<dyn TransportBackend>` for dynamic dispatch.
pub trait TransportBackend: Debug + Send + Sync {
    /// Execute a request and return a unified [`TransportResponse`].
    ///
    /// Accepts both JSON and multipart form payloads via
    /// [`HttpRequestPayload`]. Implementations that only support JSON
    /// should return an error for [`HttpRequestPayload::Form`].
    fn execute<'a>(
        &'a self,
        endpoint: Cow<'a, Endpoint>,
        payload: HttpRequestPayload<'a>,
        options: RequestOptions,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse>> + Send + 'a>>;

    /// Human-readable label for logging. Default: `"unknown"`.
    fn name(&self) -> &str {
        "unknown"
    }
}

// ── TransportResponse ────────────────────────────────────────

/// Unified transport-level response covering both JSON and binary payloads.
///
/// Business code always handles both variants, eliminating the need to match
/// on transport type.
#[derive(Debug)]
pub enum TransportResponse {
    /// JSON business response with extracted `result` and `extra` side-channel fields.
    Json(ExecuteOutput),
    /// Binary response (file download etc.), carrying the raw [`HttpResponse`].
    Binary(HttpResponse),
}

impl TransportResponse {
    /// Extract [`ExecuteOutput`] from a JSON response, or return
    /// [`Error::Parse`] for a binary response.
    pub fn into_json(self) -> Result<ExecuteOutput> {
        match self {
            Self::Json(output) => Ok(output),
            Self::Binary(resp) => Err(crate::Error::Parse {
                message: "Expected JSON response, got binary".into(),
                endpoint: resp.endpoint().to_string(),
                body: Box::new(serde_json::Value::Null),
                source: None,
            }),
        }
    }

    /// Extract the business `result` field from a JSON response, or return
    /// [`Error::Parse`] for a binary response.
    pub fn into_result(self) -> Result<serde_json::Value> {
        self.into_json().map(|o| o.result)
    }

    /// Extract the raw [`HttpResponse`] from a binary response, or return
    /// [`Error::Parse`] for a JSON response.
    pub fn into_binary(self) -> Result<HttpResponse> {
        match self {
            Self::Binary(resp) => Ok(resp),
            Self::Json(_output) => Err(crate::Error::Parse {
                message: "Expected binary response, got JSON".into(),
                endpoint: String::new(),
                body: Box::new(serde_json::Value::Null),
                source: None,
            }),
        }
    }
}
