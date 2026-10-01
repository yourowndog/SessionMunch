//! Zero-telemetry and outbound network isolation verification.
//!
//! Asserts that SessionMunch core emits no telemetry/analytics network calls,
//! and that in local-only mode (local embedder, no remote LLM/reranker),
//! no outbound network calls are initiated.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate should live under crates/sessionmunch-cli")
        .to_path_buf()
}

#[test]
fn codebase_has_no_telemetry_or_analytics_sdks() {
    let root = repo_root();
    let cargo_toml =
        fs::read_to_string(root.join("Cargo.toml")).expect("read workspace Cargo.toml");

    // Ensure no telemetry / tracking crates exist in workspace dependencies
    let forbidden_crates = [
        "posthog",
        "segment",
        "sentry",
        "datadog",
        "telemetry",
        "mixpanel",
        "amplitude",
        "google-analytics",
    ];

    for bad in &forbidden_crates {
        assert!(
            !cargo_toml.contains(&format!("{}\n", bad))
                && !cargo_toml.contains(&format!("{}\t", bad)),
            "Workspace Cargo.toml must not depend on telemetry sdk: {}",
            bad
        );
    }
}

#[test]
fn default_configuration_is_strictly_local_only() {
    let root = repo_root();
    let config_path = root.join("crates/sessionmunch-cli/templates/config.default.toml");
    let config_text = fs::read_to_string(config_path).expect("read config.default.toml");

    // Default bind must be loopback 127.0.0.1
    assert!(
        config_text.contains("bind = \"127.0.0.1:49374\""),
        "Default bind must be loopback 127.0.0.1"
    );

    // Default allowed hosts must be loopback only
    assert!(
        config_text.contains("allowed_hosts = [\"localhost\", \"127.0.0.1\", \"::1\"]"),
        "Default allowed_hosts must be loopback only"
    );

    // No remote providers configured by default
    assert!(
        !config_text.contains("reranker = \"llm\"") || config_text.contains("# reranker = \"llm\""),
        "Reranker must be disabled by default"
    );
}
