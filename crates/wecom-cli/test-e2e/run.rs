//! E2E tests for wecom-cli.
//!
//! These are binary-process-level tests that spawn the `wecom` binary and
//! test main.rs behaviors: .env loading, config.json parsing, env overrides,
//! and process-level logging.
//!
//! All library-level behavior is tested in `crates/wecom/test-e2e/`.

#![allow(unused_imports)]

mod helpers;
use helpers::*;

// ── startup ─────────────────────────────────────────────────

mod startup {
    use super::*;
    mod version {
        use super::*;
        include!("cases/startup/001-version/test.rs");
    }
}

// ── config ──────────────────────────────────────────────────

mod config {
    use super::*;
    mod invalid_config_json {
        use super::*;
        include!("cases/config/004-invalid-config-json/test.rs");
    }
}

// ── auth ────────────────────────────────────────────────────

mod auth {
    use super::*;
    mod legacy_migration {
        use super::*;
        include!("cases/auth/001-legacy-migration/test.rs");
    }
}

// ── logging ─────────────────────────────────────────────────

mod logging {
    use super::*;
    mod stderr_log {
        use super::*;
        include!("cases/logging/001-stderr-log/test.rs");
    }
    mod log_file {
        use super::*;
        include!("cases/logging/002-log-file/test.rs");
    }
}

// ── json repair ─────────────────────────────────────────────

mod json_repair {
    use super::*;
    mod stderr_hint {
        use super::*;
        include!("cases/repair/001-json-repair-stderr/test.rs");
    }
}
