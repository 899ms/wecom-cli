//! wecom-cli 自有出网后端：动态 Authorization 注入 + 853004 静默刷新。

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use wecom_transport::{
    Endpoint, EndpointHttpExt, HttpRequestPayload, RequestOptions, Transport, TransportBackend,
    TransportResponse,
};

use super::capability::{RequireAuth, SuppressAuth};
use crate::auth;
use crate::error::Error;

/// token 失效业务错误码（后台下发）：命中后经 botid+signature 静默换 token 并重试。
pub(crate) const TOKEN_EXPIRED_ERRCODE: i64 = 853004;

// ── wecom-cli 自有 transport backend ──────────────────────────

/// wecom-cli 统一出网后端：所有请求都经它转发，负责
/// - 持有 token 即注入 `Authorization: Bearer <token>`（无论端点是否挂
///   [`RequireAuth`]；无 token 则忽略不注入）；挂 [`RequireAuth`] 的端点在
///   **前置门禁** 校验：无可用 token 直接报 [`Error::Auth`]，请求不发出。
///   携带 [`SuppressAuth`] 的端点（换取 token 的引导接口）即使有 token 也不注入；
/// - 捕获 853004（token 失效）→ 用 botid+secret 签名重新换取 token
///   （落盘 + 内存缓存）→ 重放原请求一次（未注入 token 的请求不参与刷新）。
///
/// 扁平响应等请求/响应封装由 wecom-transport 的 endpoint envelope 驱动，
/// 本层不做特殊分流。
///
/// 所有载荷均可重放：经 [`HttpRequestPayload`](wecom_transport::HttpRequestPayload)
/// 工厂克隆（Arc 零成本），重放 = 再次 build。
#[derive(Clone)]
pub(crate) struct WecomBackend {
    /// 底层 HTTP 传输（信封解析 + 长任务轮询路径）。
    inner: Arc<dyn TransportBackend>,
    /// `auth init` 时持久化到 `credentials.enc` 的 bot 凭据（无凭据时为 None）。
    pub(crate) bot: Option<auth::Bot>,
    /// 缓存的 token（初始来自 credentials，刷新后更新）：
    /// 需要授权的请求在调用时经它注入 Authorization 头。
    token: Arc<RwLock<Option<String>>>,
    /// 串行化刷新，避免并发请求重复换取 token。
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
    /// 鉴权引导端点（`transport::build` 时经 `auth::resolve_auth_endpoint` 装配，
    /// token 刷新时复用）。
    auth_endpoint: Endpoint,
}

// 不输出 bot secret 与缓存 token。
impl std::fmt::Debug for WecomBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WecomBackend")
            .field("backend", &self.inner.name())
            .finish_non_exhaustive()
    }
}

impl WecomBackend {
    pub(crate) fn new(
        inner: Arc<dyn TransportBackend>,
        bot: Option<auth::Bot>,
        token: Option<String>,
        auth_endpoint: Endpoint,
    ) -> Self {
        Self {
            inner,
            bot,
            token: Arc::new(RwLock::new(token)),
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            auth_endpoint,
        }
    }

    pub(crate) fn cached_token(&self) -> Option<String> {
        self.token.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn store_token(&self, token: &str) {
        *self.token.write().unwrap_or_else(|e| e.into_inner()) = Some(token.to_owned());
    }

    /// 经 botid+signature 重新换取 token：落盘 + 写入内存缓存，返回新 token。
    ///
    /// `stale_token` 为本次失败请求所用的 token；锁内双检——若凭据中的 token
    /// 已不同于它，说明并发请求已完成刷新，直接复用、不再重复换取。
    ///
    /// `options` 为触发刷新的请求携带的请求选项（含 transport 默认叠加的
    /// headers / timeout / extensions），引导请求复用它们，保证传输配置一致。
    async fn refresh_token(
        &self,
        stale_token: Option<&str>,
        mut options: RequestOptions,
    ) -> std::result::Result<String, Error> {
        let _guard = self.refresh_lock.lock().await;

        if let Some(stored) = auth::load_token()
            && Some(stored.as_str()) != stale_token
        {
            tracing::debug!("token already refreshed by a concurrent request, reusing it");
            self.store_token(&stored);
            return Ok(stored);
        }

        let Some(bot) = self.bot.clone() else {
            return Err(Error::Auth(format!(
                "无 bot 凭据，无法静默刷新 token，请重新运行 `{} auth init`",
                env!("CARGO_BIN_NAME")
            )));
        };

        // 静默刷新复用 Interactive 来源（原始绑定方式未持久化）。
        // 复用自身（含扁平响应/授权管理）发起引导请求——同一连接池与配置，
        // 且复用触发刷新的请求的 options（headers / timeout / extensions）。
        // 引导请求不携带业务 token：剥离其中注入的失效 Authorization 头。
        options.headers_mut().remove(reqwest::header::AUTHORIZATION);
        let transport = Transport::new(Arc::new(self.clone()), options);
        let resp = auth::fetch_auth(
            &transport,
            &bot,
            auth::BindSource::Interactive,
            &self.auth_endpoint,
        )
        .await?;
        let token = resp.token.filter(|t| !t.is_empty()).ok_or_else(|| {
            Error::protocol(
                "token 刷新响应缺少访问令牌",
                self.auth_endpoint.full_url(),
                serde_json::Value::Null,
            )
        })?;

        // 落盘：bot 凭据保持不变，原子更新 token。
        let mut creds = auth::load_credentials().unwrap_or_default();
        creds.token = Some(token.clone());
        auth::save_credentials(&creds).await?;

        self.store_token(&token);
        tracing::info!("access token refreshed (853004) and persisted");
        Ok(token)
    }
}

impl TransportBackend for WecomBackend {
    fn execute<'a>(
        &'a self,
        endpoint: Cow<'a, Endpoint>,
        payload: HttpRequestPayload,
        options: RequestOptions,
    ) -> Pin<
        Box<
            dyn Future<Output = std::result::Result<TransportResponse, wecom_transport::Error>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            // 所有载荷均可重放：clone 工厂（Arc 零成本），重放 = 再次 build。
            let replay_payload = payload.clone();

            let mut options = options;

            // 抑制注入：换取 token 的引导端点即使持有 token 也不携带 Authorization。
            let sent_token = if endpoint.as_ref().get::<SuppressAuth>().is_some() {
                None
            } else {
                let token = self.cached_token();

                // 门禁前置：挂 RequireAuth 的端点必须已有可用 token，否则请求不发出。
                if endpoint.as_ref().get::<RequireAuth>().is_some() && token.is_none() {
                    tracing::debug!("endpoint requires auth but no token available");
                    return Err(Error::Auth(format!(
                        "该请求需要授权，请先运行 `{} auth init` 登录",
                        env!("CARGO_BIN_NAME")
                    ))
                    .into());
                }

                // 有 token 就注入（无论是否挂 RequireAuth），无 token 则忽略；
                // 记下本次发送值供 853004 刷新去重。
                token
                    .clone()
                    .inspect(|token| set_bearer_token(&mut options, token))
            };

            let err = match self
                .inner
                .execute(endpoint.clone(), payload, options.clone())
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(err) => err,
            };
            if !is_token_expired(&err) {
                return Err(err);
            }
            // 未注入 token 的请求不可能因 token 过期失败（无 token / 抑制注入的
            // 引导端点）——不参与刷新，直接返回原错误。
            if sent_token.is_none() {
                tracing::warn!("token expired but no token was sent");
                return Err(err);
            }
            // 无 bot 凭据时无法签名换取新 token，不参与自动刷新。
            if self.bot.is_none() {
                tracing::warn!("missing bot credentials, cannot refresh token");
                return Err(err);
            }

            tracing::info!("token expired (853004), attempting silent refresh");
            match self
                .refresh_token(sent_token.as_deref(), options.clone())
                .await
            {
                Ok(token) => {
                    tracing::info!("token refreshed, retrying the original request");
                    set_bearer_token(&mut options, &token);
                    // 重放 = 重新走完整流水线：发送链会再次 build。
                    self.inner.execute(endpoint, replay_payload, options).await
                }
                Err(refresh_err) => {
                    tracing::warn!(error = %refresh_err, "token refresh failed, returning the original error");
                    Err(err)
                }
            }
        })
    }

    fn name(&self) -> &str {
        self.inner.name()
    }
}

/// 是否为可触发静默刷新的 token 失效错误（ApiError 853004）。
pub(crate) fn is_token_expired(err: &wecom_transport::Error) -> bool {
    matches!(
        err,
        wecom_transport::Error::Api {
            code: Some(TOKEN_EXPIRED_ERRCODE),
            ..
        }
    )
}

/// 在请求选项上覆写 `Authorization: Bearer <token>` 头（标记敏感）。
pub(crate) fn set_bearer_token(options: &mut RequestOptions, token: &str) {
    let Ok(mut value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")) else {
        return;
    };
    value.set_sensitive(true);
    options
        .wire
        .headers
        .insert(reqwest::header::AUTHORIZATION, value);
}
