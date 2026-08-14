//! wecom-cli 自有出网后端：动态 Authorization 注入 + 853004 静默刷新。

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use wecom_transport::{
    Endpoint, HttpRequestPayload, RequestOptions, Transport, TransportBackend, TransportResponse,
};

use super::capability::AuthRequirement;
use crate::auth;
use crate::error::Error;

/// token 失效业务错误码（后台下发）：命中后经 botid+signature 静默换 token 并重试。
pub(crate) const TOKEN_EXPIRED_ERRCODE: i64 = 853004;

// ── wecom-cli 自有 transport backend ──────────────────────────

/// wecom-cli 统一出网后端：所有请求都经它转发，负责
/// - 按 endpoint 携带的 [`AuthRequirement`] 在调用时动态注入
///   `Authorization: Bearer <token>`（未挂该能力不注入；`need_auth=true`
///   无 token 时报 [`Error::Auth`](crate::error::Error)）；
/// - 捕获 853004（token 失效）→ 用 botid+secret 签名重新换取 token
///   （落盘 + 内存缓存）→ 重放原请求一次。
///
/// 扁平响应等请求/响应封装由 wecom-transport 的 endpoint envelope 驱动，
/// 本层不做特殊分流。
///
/// 仅 JSON 载荷可重放；multipart 表单无法克隆，命中 853004 时直接返回原错误。
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
    /// 鉴权引导端点（`transport::build` 时从已加载的 ConfigFile 解析，token 刷新时复用）。
    auth_endpoint: String,
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
        auth_endpoint: String,
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
                &self.auth_endpoint,
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
        payload: HttpRequestPayload<'a>,
        options: RequestOptions,
    ) -> Pin<
        Box<
            dyn Future<Output = std::result::Result<TransportResponse, wecom_transport::Error>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            // 仅 JSON 载荷可克隆重放。
            let replay_payload = match &payload {
                HttpRequestPayload::Json(value) => Some(value.clone()),
                HttpRequestPayload::Form(_) => None,
            };

            let mut options = options;
            // 端点声明 need_auth 时注入 Bearer token，并记下本次发送值供 853004 刷新去重。
            let sent_token = if endpoint
                .as_ref()
                .get::<AuthRequirement>()
                .is_some_and(|a| a.need_auth)
            {
                let Some(token) = self.cached_token() else {
                    tracing::debug!("endpoint requires auth but no token available");
                    return Err(Error::Auth(format!(
                        "该请求需要授权，请先运行 `{} auth init` 登录",
                        env!("CARGO_BIN_NAME")
                    ))
                    .into());
                };
                set_bearer_token(&mut options, &token);
                Some(token)
            } else {
                None
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
            tracing::info!("token expired (853004), attempting silent refresh");
            let Some(json) = replay_payload else {
                tracing::warn!("no replay payload, cannot refresh token");
                return Err(err);
            };
            // 无 bot 凭据时无法签名换取新 token，不参与自动刷新。
            if self.bot.is_none() {
                tracing::warn!("missing bot credentials, cannot refresh token");
                return Err(err);
            }

            match self
                .refresh_token(sent_token.as_deref(), options.clone())
                .await
            {
                Ok(token) => {
                    tracing::info!("token refreshed, retrying the original request");
                    set_bearer_token(&mut options, &token);
                    self.inner
                        .execute(endpoint, HttpRequestPayload::Json(json), options)
                        .await
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
