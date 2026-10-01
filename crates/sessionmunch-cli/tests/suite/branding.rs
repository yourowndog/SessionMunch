//! SessionMunch namespace-migration gate (t_3f5184b0).
//!
//! A fresh install must encounter SessionMunch, not development archaeology:
//! `init` into an empty dir must produce a tree with no `ai-memory`
//! branding in paths or bytes, while pre-rename `AI_MEMORY_*` env still
//! resolves (read-only compat) until operators reinstall.

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sessionmunch")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate should live under crates/sessionmunch-cli")
        .to_path_buf()
}

fn is_branding(text: &str) -> bool {
    text.contains("ai-memory")
        || text.contains("ai_memory")
        || text.contains("AI_MEMORY")
        || text.contains("AiMemory")
}

/// A child process with the developer's own install scrubbed out: pinned
/// `$HOME`, no inherited `SESSIONMUNCH_*`/`AI_MEMORY_*` except `extra`.
fn child(data_dir: &Path, home: &Path, extra: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(bin());
    cmd.env("HOME", home).env("SESSIONMUNCH_DATA_DIR", data_dir);
    for (key, _) in std::env::vars() {
        if key.starts_with("SESSIONMUNCH_") || key.starts_with("AI_MEMORY_") {
            cmd.env_remove(&key);
        }
    }
    cmd.env("SESSIONMUNCH_DATA_DIR", data_dir);
    for (key, value) in extra {
        cmd.env(key, value);
    }
    cmd
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn fresh_install_tree_has_no_aimemory_branding() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&home).unwrap();

    let output = child(&data, &home, &[])
        .arg("init")
        .arg("--data-dir")
        .arg(&data)
        .output()
        .expect("running sessionmunch init");
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut problems = Vec::new();
    for path in walk_files(&data) {
        let rel = path.strip_prefix(&data).unwrap_or(&path);
        if is_branding(&rel.to_string_lossy()) {
            problems.push(format!("branded path: {}", rel.display()));
        }
        if let Ok(text) = std::fs::read_to_string(&path)
            && is_branding(&text)
        {
            problems.push(format!("branded bytes: {}", rel.display()));
        }
    }
    assert!(
        problems.is_empty(),
        "fresh install must not emit ai-memory branding:\n{}",
        problems.join("\n")
    );

    let status = child(&data, &home, &[])
        .arg("status")
        .output()
        .expect("running sessionmunch status");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        !is_branding(&combined),
        "status output must not contain ai-memory branding:\n{combined}"
    );
}

#[test]
fn version_identifies_as_sessionmunch() {
    let tmp = TempDir::new().expect("tempdir");
    let output = child(tmp.path(), tmp.path(), &[])
        .arg("--version")
        .output()
        .expect("running sessionmunch --version");
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        text.starts_with("sessionmunch "),
        "unexpected version line: {text}"
    );
    assert!(
        !is_branding(&text),
        "version line leaks old branding: {text}"
    );
}

#[test]
fn legacy_ai_memory_data_dir_is_honored_until_reinstall() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let legacy = tmp.path().join("legacy-data");
    std::fs::create_dir_all(&home).unwrap();

    // Old-only env: the pre-rename variable still selects the data dir.
    let mut cmd = child(&tmp.path().join("unused"), &home, &[]);
    cmd.env_remove("SESSIONMUNCH_DATA_DIR");
    cmd.env(
        "AI_MEMORY_DATA_DIR",
        legacy.to_str().expect("utf8 legacy path"),
    );
    let output = cmd.arg("init").output().expect("running init");
    assert!(
        output.status.success(),
        "init with AI_MEMORY_DATA_DIR failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        legacy.join("config.toml").is_file(),
        "AI_MEMORY_DATA_DIR fallback was not honored"
    );

    // New wins when both are set; nothing new is ever written to the old path.
    let fresh = tmp.path().join("fresh-data");
    let mut cmd = child(&fresh, &home, &[]);
    cmd.env(
        "AI_MEMORY_DATA_DIR",
        legacy.to_str().expect("utf8 legacy path"),
    );
    let output = cmd
        .arg("init")
        .arg("--data-dir")
        .arg(&fresh)
        .output()
        .expect("running init");
    assert!(output.status.success());
    assert!(fresh.join("config.toml").is_file());
}

#[test]
fn canonical_surfaces_carry_the_sessionmunch_name() {
    let root = repo_root();
    let checks = [
        (
            "crates/sessionmunch-cli/src/cli.rs",
            r#"#[command(name = "sessionmunch""#,
        ),
        (
            "crates/sessionmunch-mcp/src/server.rs",
            r#"implementation.name = "sessionmunch""#,
        ),
        (
            "crates/sessionmunch-mcp/src/auth.rs",
            r#"const AUTH_REALM: &str = "sessionmunch""#,
        ),
        (
            "crates/sessionmunch-cli/src/config.rs",
            r#".join("sessionmunch")"#,
        ),
        (
            "packaging/systemd/sessionmunch.service",
            "ExecStart=/usr/bin/sessionmunch",
        ),
        (
            "packaging/systemd/sessionmunch-user.service",
            "sessionmunch",
        ),
    ];
    for (rel, needle) in checks {
        let text =
            std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        assert!(
            text.contains(needle),
            "{rel} must carry the SessionMunch name ({needle})"
        );
    }
    // The only `ai-memory` executable a fresh install ships is the
    // deprecated forwarding shim — and it must forward, not serve.
    for shim in ["bin/ai-memory", "bin/ai-memory.cmd", "bin/ai-memory.ps1"] {
        let text =
            std::fs::read_to_string(root.join(shim)).unwrap_or_else(|e| panic!("read {shim}: {e}"));
        assert!(
            text.to_lowercase().contains("renamed to sessionmunch"),
            "{shim} must say it is a rename shim"
        );
    }
}
