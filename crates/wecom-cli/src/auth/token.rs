//! Bearer token 的本地读取（凭据文件 `credentials.enc`）。

use super::credentials::load_credentials;

/// Read the cached Bearer token from the credentials file.
pub fn load_token() -> Option<String> {
    load_credentials().and_then(|c| c.token)
}
