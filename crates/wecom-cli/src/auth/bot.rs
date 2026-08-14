use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::credentials::load_credentials;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bot {
    pub id: String,
    pub secret: String,
    pub create_time: u64,
}

impl Bot {
    /// Create a new Bot with `create_time` set to the current timestamp.
    pub fn new(id: String, secret: String) -> Self {
        let create_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            id,
            secret,
            create_time,
        }
    }
}

/// Read encrypted bot info from the credentials file.
pub fn get_bot_info() -> Option<Bot> {
    load_credentials().and_then(|c| c.bot)
}
