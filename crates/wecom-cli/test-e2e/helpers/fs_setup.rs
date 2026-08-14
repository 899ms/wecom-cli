// Used by config tests gated behind feature flags.
#![allow(dead_code)]

use std::path::Path;

/// Write a `config.json` file in the given directory.
pub fn setup_config_json(dir: &Path, config: &serde_json::Value) {
    #[allow(clippy::disallowed_methods)]
    // Test fixture: writing to tempdir, not through CLI sandbox.
    std::fs::write(
        dir.join("config.json"),
        serde_json::to_string_pretty(config).unwrap(),
    )
    .unwrap();
}
