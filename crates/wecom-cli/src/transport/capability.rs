//! 鉴权能力类型（wecom-cli 自定义）。
//!
//! 鉴权语义：`AuthRequirement` 作为能力标记挂在
//! [`Endpoint`](wecom_transport::Endpoint) 能力袋上。需要授权但无可用 token 的错误由
//! [`Error::Auth`](crate::error::Error) 承担。

/// 端点是否需要在调用时注入 `Authorization: Bearer <token>` 的能力标记。
///
/// 挂进 [`Endpoint`](wecom_transport::Endpoint) 能力袋（wecom-cli 自定义能力
/// 类型）——鉴权标识按 endpoint 单独声明。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuthRequirement {
    /// `true` → 调用时注入当前缓存 token；无 token 时报
    /// [`Error::Auth`](crate::error::Error)。
    pub need_auth: bool,
}

impl AuthRequirement {
    /// 构造带 `need_auth` 标记的能力。
    #[must_use]
    pub const fn new(need_auth: bool) -> Self {
        Self { need_auth }
    }
}
