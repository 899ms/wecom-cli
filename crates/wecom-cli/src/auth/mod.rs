//! 鉴权域：Bot 凭据（botid+secret）存储、扫码登录、签名引导换取 Bearer token。
//!
//! - [`credentials`]：凭据总账（bot 信息 + Bearer token 共存于单一加密文件
//!   `credentials.enc`），保证原子更新；
//! - [`legacy_migration`]：旧版凭据（`bot.enc`/`token.enc`）自动迁移——
//!   读旧 botid/secret 自动 auth 换 token 落盘新格式；旧文件**不主动清理**；
//! - [`bot`]：botid+secret 凭据（写入 `credentials.enc`）；
//! - [`qrcode`]：扫码登录的网络流程（创建会话 → 轮询结果）；
//! - [`bootstrap`]：botid+secret 签名调用换取 Bearer token（业务错误码映射内聚于此）；
//! - [`token`]：Bearer token 缓存（写入 `credentials.enc`）。
//!
//! 目录布局（`~/.config/wecom`）固定不变。

mod bootstrap;
mod bot;
mod credentials;
mod crypto;
mod legacy_migration;
mod qrcode;
mod token;

pub use bootstrap::{BindSource, fetch_auth, resolve_auth_endpoint};
pub use bot::{Bot, get_bot_info};
pub use credentials::{load_credentials, save_credentials};
pub use legacy_migration::try_migrate_legacy_credentials;
pub use qrcode::QrSession;
pub use token::load_token;
