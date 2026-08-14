//! Deferred-error builder for [`Transport`](crate::Transport).
//!
//! Provides a unified chain to configure both a backend (`B`) and
//! transport-level options (headers, timeout) in a single call chain.
//! Header validation errors are accumulated and surfaced only at
//! [`build()`](TransportBuilder::build).

use std::sync::Arc;

use crate::transport::Transport;
use crate::{Error, IntoHeaderName, IntoHeaderValue, RequestOptions, TransportBackend};

/// Deferred-error builder for [`Transport`].
///
/// Configures both the backend (`B`) and transport-level options in one chain;
/// the first error is captured and surfaced at [`build`](Self::build).
pub struct TransportBuilder<B> {
    backend: B,
    options: RequestOptions,
    build_error: Option<Error>,
}

impl<B> TransportBuilder<B> {
    /// Generic entry point — wrap an already-constructed backend instance.
    ///
    /// External-crate backends (which may carry required runtime deps that
    /// cannot be `Default`) use this to obtain a builder.
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            options: RequestOptions::default(),
            build_error: None,
        }
    }

    /// Backend-configuration escape hatch — public so backends defined in
    /// **other crates** can mount their own strongly-typed setters on
    /// `TransportBuilder<TheirBackend>`.
    ///
    /// No-op if a previous step already recorded an error.
    #[must_use]
    pub fn map_backend(mut self, f: impl FnOnce(B) -> B) -> Self {
        if self.build_error.is_none() {
            self.backend = f(self.backend);
        }
        self
    }
}

// ── Transport-level generic capabilities: available for all backends ──

impl<B: TransportBackend + 'static> TransportBuilder<B> {
    /// Add a request header.
    ///
    /// The first invalid header name or value is recorded and surfaced at
    /// [`build()`](Self::build).
    #[must_use]
    pub fn header(self, name: impl IntoHeaderName, value: impl IntoHeaderValue) -> Self {
        self.header_sensitive(name, value, false)
    }

    /// Like [`header`](Self::header), but marks the value as sensitive for
    /// debug / log output (e.g. auth tokens).
    #[must_use]
    pub fn header_sensitive(
        mut self,
        name: impl IntoHeaderName,
        value: impl IntoHeaderValue,
        sensitive: bool,
    ) -> Self {
        if self.build_error.is_some() {
            return self;
        }
        match (name.try_into_header_name(), value.try_into_header_value()) {
            (Ok(n), Ok(mut v)) => {
                if sensitive {
                    v.set_sensitive(true);
                }
                self.options.wire.headers.insert(n, v);
            }
            (Err(e), _) | (_, Err(e)) => self.build_error = Some(Error::Other(e)),
        }
        self
    }

    /// Bulk-add request headers from a [`reqwest::header::HeaderMap`].
    #[must_use]
    pub fn headers(mut self, headers: reqwest::header::HeaderMap) -> Self {
        self.options.wire.headers.extend(
            headers
                .into_iter()
                .filter_map(|(name, value)| name.map(|n| (n, value))),
        );
        self
    }

    /// Set a per-request timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.options.wire.timeout = Some(timeout);
        self
    }

    /// Set a build-time default extension value, applied to every request
    /// made through the built transport.
    ///
    /// Same-type values set later via run-level or per-request
    /// `.extension()` override this default (per-TypeId, later layer wins).
    /// Inserting cannot fail, so this stays in the deferred-error style.
    #[must_use]
    pub fn extension<T>(mut self, value: T) -> Self
    where
        T: std::any::Any + std::fmt::Debug + Send + Sync + 'static,
    {
        self.options.extensions.insert(value);
        self
    }

    /// The only fallible step — construct the final [`Transport`].
    ///
    /// Returns the first recorded error (from [`header`](Self::header) /
    /// [`header_sensitive`](Self::header_sensitive)), or the finished
    /// [`Transport`] wrapping the configured backend and options.
    pub fn build(self) -> Result<Transport, Error> {
        if let Some(e) = self.build_error {
            return Err(e);
        }
        Ok(Transport::new(Arc::new(self.backend), self.options))
    }
}

#[cfg(test)]
mod tests {
    //! ## 模块摘要：TransportBuilder（transport 构建器）
    //!
    //! ### 关键接口
    //! - [TransportBuilder::new] — 通用入口，包装已构建的后端
    //! - [TransportBuilder::extension] — 构建期默认扩展值，随每请求透传
    //! - [TransportBuilder::build] — 唯一可失败的步骤，产出 [Transport](crate::Transport)
    //!
    //! ### 关键分支与异常路径
    //! - 构建期扩展值经 `invoke()` 到达 `TransportBackend::execute`（CaptureBackend 验证）
    //!
    //! ### 上下游交互
    //! - 上游：调用方通过 `TransportBackend::builder()` / [TransportBuilder::new] 配置
    //! - 下游：[build](TransportBuilder::build) 产出 [Transport](crate::Transport)

    use std::borrow::Cow;
    use std::future::Future;
    use std::pin::Pin;

    use super::*;
    use crate::http_client::HttpRequestPayload;
    use crate::traits::TransportResponse;
    use crate::{Endpoint, ExecuteOutput, RequestOptions, Result, TransportBackend};

    /// 记录 `execute` 收到 options 的捕获型后端。
    #[derive(Debug)]
    struct CaptureBackend {
        captured: std::sync::Arc<std::sync::Mutex<Option<RequestOptions>>>,
    }

    impl TransportBackend for CaptureBackend {
        fn execute<'a>(
            &'a self,
            _endpoint: Cow<'a, Endpoint>,
            _payload: HttpRequestPayload<'a>,
            options: RequestOptions,
        ) -> Pin<Box<dyn Future<Output = Result<TransportResponse>> + Send + 'a>> {
            *self.captured.lock().unwrap() = Some(options);
            Box::pin(async move {
                Ok(TransportResponse::Json(ExecuteOutput {
                    result: serde_json::json!({}),
                    extra: indexmap::IndexMap::new(),
                }))
            })
        }
    }

    /// 测试夹具：扩展袋值。
    #[derive(Debug, PartialEq)]
    struct BExt(u32);

    fn capture_backend() -> (
        CaptureBackend,
        std::sync::Arc<std::sync::Mutex<Option<RequestOptions>>>,
    ) {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            CaptureBackend {
                captured: captured.clone(),
            },
            captured,
        )
    }

    // ── TransportBuilder::extension ──

    /// P0：[TransportBuilder::extension] 写入构建期 options 扩展袋
    /// 条件：TransportBuilder::new(backend).extension(BExt(2))
    /// 断言：builder.options.extensions.get::<BExt>() 返回 Some(2)
    #[test]
    fn extension_writes_options_bag() {
        let (backend, _) = capture_backend();
        let b = TransportBuilder::new(backend).extension(BExt(2));
        assert_eq!(b.options.extensions.get::<BExt>(), Some(&BExt(2)));
    }

    /// P0：构建期扩展值端到端到达 `execute`
    /// 条件：TransportBuilder::extension(BExt(1)) → build → invoke().await
    /// 断言：execute 收到的 options.extensions.get::<BExt>() 为 Some(1)
    #[tokio::test]
    async fn extension_reaches_execute() {
        let (backend, captured) = capture_backend();
        let transport = TransportBuilder::new(backend)
            .extension(BExt(1))
            .build()
            .unwrap();
        transport
            .invoke(Endpoint::new(), serde_json::json!({}))
            .await
            .unwrap();
        let options = captured.lock().unwrap().take().expect("execute ran");
        assert_eq!(options.extensions.get::<BExt>(), Some(&BExt(1)));
    }

    /// P1：`invoke()` 时构建期默认扩展可被请求级 `.extension()` 覆盖（请求级 > 构建期）
    /// 条件：builder.extension(BExt(1))，请求级 .extension(BExt(2))
    /// 断言：execute 收到 BExt(2)
    #[tokio::test]
    async fn request_level_overrides_build_default() {
        let (backend, captured) = capture_backend();
        let transport = TransportBuilder::new(backend)
            .extension(BExt(1))
            .build()
            .unwrap();
        transport
            .invoke(Endpoint::new(), serde_json::json!({}))
            .extension(BExt(2))
            .await
            .unwrap();
        let options = captured.lock().unwrap().take().expect("execute ran");
        assert_eq!(options.extensions.get::<BExt>(), Some(&BExt(2)));
    }
}
