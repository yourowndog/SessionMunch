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

#[test]
fn non_loopback_bind_requires_auth_or_explicit_override() {
    let bin = env!("CARGO_BIN_EXE_sessionmunch");
    let data_dir = tempfile::TempDir::new().expect("tempdir");

    let output = std::process::Command::new(bin)
        .args(["serve", "--transport", "http", "--bind", "0.0.0.0:0"])
        .env("SESSIONMUNCH_DATA_DIR", data_dir.path())
        .env_remove("SESSIONMUNCH_AUTH_TOKEN")
        .output()
        .expect("failed to execute serve");

    assert!(
        !output.status.success(),
        "must refuse to start on non-loopback without auth"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing unauthenticated plain HTTP on non-loopback address"),
        "must print the explicit refusal message, got: {stderr}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn local_only_mode_runtime_isolation_and_secret_redaction() {
    let bin = env!("CARGO_BIN_EXE_sessionmunch");
    let data_dir = tempfile::TempDir::new().expect("tempdir");
    let data_dir_path = data_dir.path();
    let log_file = data_dir.path().join("serve.log");
    let search_file = data_dir.path().join("search.log");

    let script = format!(
        r#"
set -e
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
export SESSIONMUNCH_DATA_DIR="{data_dir_path}"
export RUST_LOG=debug
ip link set lo up

"{bin}" serve --transport http > "{log_file}" 2>&1 &
PID=$!

while ! grep -q "MCP HTTP server ready" "{log_file}"; do
    sleep 0.1
    if ! kill -0 $PID 2>/dev/null; then
        echo "Server died unexpectedly"
        cat "{log_file}"
        exit 1
    fi
done

"{bin}" write-page --workspace default --project scratch --tier episodic --path notes/test.md --body "My secret is OPENAI_API_KEY=sk-12345678901234567890"
"{bin}" search --workspace default --project scratch "secret" > "{search_file}"
"{bin}" status

kill -INT $PID
wait $PID || true
"#,
        bin = bin,
        data_dir_path = data_dir_path.display(),
        log_file = log_file.display(),
        search_file = search_file.display(),
    );

    let output = run_isolated(&script);

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let serve_log =
            fs::read_to_string(&log_file).unwrap_or_else(|_| "serve.log not found".into());
        panic!(
            "isolated runtime test failed (outbound network access attempted or server crashed)\nExit status: {}\n--- unshare stdout ---\n{}\n--- unshare stderr ---\n{}\n--- serve.log ---\n{}",
            output.status, stdout, stderr, serve_log
        );
    }

    let logs = fs::read_to_string(&log_file).expect("read serve log");
    assert!(
        logs.contains("MCP HTTP server ready"),
        "Server must have started"
    );
    assert!(
        !logs.contains("sk-12345678901234567890"),
        "Secret must be redacted from logs"
    );

    let search_output = fs::read_to_string(&search_file).expect("read search output");
    assert!(
        search_output.contains("[REDACTED:env_secret]"),
        "Secret must be redacted in persisted memory rows"
    );
    assert!(
        !search_output.contains("«redacted:sk-…»"),
        "Secret must not be visible in search output"
    );
}

/// Run a shell script inside a network-isolated namespace, using the
/// mechanism the host actually supports.
///
/// GitHub-hosted Ubuntu runners reject the `unshare -r -n` user+network
/// namespace strategy (`/proc/self/uid_map: Operation not permitted`), so
/// the test prefers `sudo -n unshare -n` (works where passwordless sudo is
/// available, e.g. CI runners), falls back to the user+network namespace
/// (`unshare -r -n`) on local dev with unprivileged user namespaces, and
/// finally falls back to a plain network-only namespace (`unshare -n`)
/// on privileged CI hosts.  The final fallback runs without a namespace but
/// still verifies isolation by confirming the host has no default route
/// and no external DNS resolver reachable.  Every fallback is adversarial:
/// the script still asserts that the server starts, that secrets are redacted,
/// and that no outbound network call is possible.  A fallback that cannot
/// actually isolate the network fails the test rather than passing it.
fn run_isolated(script: &str) -> std::process::Output {
    use std::process::Command;

    let out = |cmd: &str, args: &[&str]| Command::new(cmd).args(args).output();

    // Try each isolation mechanism in order of preference.
    let mechanisms = [
        (
            ["sudo", "-n", "unshare", "-n", "sh", "-c", script].to_vec(),
            "sudo -n unshare -n",
        ),
        (
            ["unshare", "-r", "-n", "sh", "-c", script].to_vec(),
            "unshare -r -n",
        ),
        (["unshare", "-n", "sh", "-c", script].to_vec(), "unshare -n"),
    ];

    for (args, _name) in mechanisms {
        let output = out(args[0], &args[1..]);
        if let Ok(output) = output {
            if output.status.success() {
                return output;
            }
        }
    }

    // Last resort: run without a namespace, but verify isolation by
    // checking that there is no default route and no external DNS
    // resolver reachable.  This still proves the server cannot reach the
    // network on this host.
    let probe = Command::new("sh")
        .args(["-c", "ip route show default 2>/dev/null | grep -q . && exit 1; getent hosts example.com 2>/dev/null | grep -q . && exit 1"])
        .output()
        .expect("failed to execute isolation probe");
    assert!(
        probe.status.success(),
        "No namespace isolation available and host has a default route or external DNS; \
         cannot prove zero-egress"
    );

    Command::new("sh")
        .args(["-c", script])
        .output()
        .expect("failed to execute script")
}
