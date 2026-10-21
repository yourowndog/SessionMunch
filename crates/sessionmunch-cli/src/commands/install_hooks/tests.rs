use super::*;

/// The marker-presence admit gate the shared `ts_capture_policy_v1`
/// template bakes into `capturePolicy` (#661). Under allowlist a
/// repository with no `.sessionmunch.toml` marker must emit nothing —
/// mirroring `repository_admits_capture` in
/// `sessionmunch-hooks::capture_policy`, the gate hook.rs's native path
/// runs before any per-event disposition. Keyed on `findMarker(cwd)`
/// presence rather than `config.state`, so a marker with an empty
/// `[capture]` section — `state` is "inactive" either way — still admits
/// capture.
const CAPTURE_ADMIT_GATE_TS: &str = "const markerPresent = !!findMarker(cwd); if (CAPTURE_MODE === \"allowlist\" && !markerPresent) return { disposition: \"drop\", payload };";

/// #446's binding requirement: "a protection that disappears on upgrade
/// without saying so is worse than no protection". A bare `--apply` — what
/// `sessionmunch upgrade` runs — must leave an existing opt-in alone.
#[test]
fn bare_apply_preserves_an_existing_allowlist_optin() {
    let tmp = tempfile::tempdir().unwrap();
    let mode = persist_capture_mode(tmp.path(), Some(CaptureModeArg::Allowlist)).unwrap();
    assert_eq!(mode, "allowlist");

    // The upgrade path: no flag at all.
    let after = persist_capture_mode(tmp.path(), None).unwrap();
    assert_eq!(
        after, "allowlist",
        "a bare re-apply must not revert the opt-in"
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join(crate::commands::hook::CAPTURE_MODE_FILE))
            .unwrap()
            .trim(),
        "allowlist"
    );
}

/// #446's opt-in is stored as a bare word, and every reader maps anything
/// it does not recognise onto `denylist` — the *less* private mode. That
/// fallback is deliberate and tested
/// (`hook.rs`: `unreadable_or_unknown_mode_falls_back_to_denylist_not_allowlist`);
/// what must never happen is the truncated file that triggers it. An
/// in-place write truncates before it writes, so a crash, a full disk or a
/// killed `upgrade` mid-write leaves exactly that — and the next hook run
/// reads it as capture-by-default, silently undoing the opt-in the doc
/// comment on `persist_capture_mode` promises to preserve.
///
/// Replacement rather than truncation is the property that rules the window
/// out, and it is observable: a rename gives the path a new inode, an
/// in-place rewrite keeps the old one. This fails against `fs::write`.
#[cfg(unix)]
#[test]
fn rewriting_the_capture_mode_replaces_the_file_rather_than_truncating_it() {
    use std::os::unix::fs::MetadataExt;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(crate::commands::hook::CAPTURE_MODE_FILE);

    persist_capture_mode(tmp.path(), Some(CaptureModeArg::Allowlist)).unwrap();
    let before = fs::metadata(&path).unwrap().ino();

    persist_capture_mode(tmp.path(), Some(CaptureModeArg::Denylist)).unwrap();
    let after = fs::metadata(&path).unwrap().ino();

    assert_ne!(
        before, after,
        "the capture mode must be replaced by rename, not rewritten in \
             place: an in-place write is observable truncated, and a truncated \
             file reads back as denylist"
    );

    let leftovers: Vec<String> = fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".sessionmunch-tmp."))
        .collect();
    assert!(
        leftovers.is_empty(),
        "the staging tempfile must not survive the write: {leftovers:?}"
    );
}

#[test]
fn absent_file_reports_the_historical_default() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(persist_capture_mode(tmp.path(), None).unwrap(), "denylist");
}

#[test]
fn explicit_denylist_downgrades_an_earlier_optin() {
    // The opt-in must be reversible, or operators cannot undo a mistake.
    let tmp = tempfile::tempdir().unwrap();
    persist_capture_mode(tmp.path(), Some(CaptureModeArg::Allowlist)).unwrap();
    assert_eq!(
        persist_capture_mode(tmp.path(), Some(CaptureModeArg::Denylist)).unwrap(),
        "denylist"
    );
    assert_eq!(persist_capture_mode(tmp.path(), None).unwrap(), "denylist");
}
use crate::cli::ProjectStrategyArg;
use crate::commands::render_shared::KIRO_CLI_V2_SESSION_START_MAX_OUTPUT;
use std::collections::BTreeMap;
use std::fs;
#[cfg(any(unix, windows))]
use std::process::Command;
use tempfile::TempDir;

#[test]
fn capture_assistant_allowed_only_for_claude_native() {
    use crate::cli::AgentChoice::*;
    // Every non-Claude agent is rejected regardless of platform (#196): the
    // opt-in cannot take effect for them, so the installer must bail.
    for agent in [
        Codex,
        CommandCode,
        Cursor,
        GeminiCli,
        OpenCode,
        Pi,
        Omp,
        Openclaw,
        AntigravityCli,
        Grok,
        Zero,
        Devin,
        KimiCode,
        KiroCli,
        KiroCliV3,
        Pool,
        Zcode,
    ] {
        assert!(
            !capture_assistant_allowed(agent),
            "{agent:?} must not allow --capture-assistant"
        );
    }
    // Claude Code tracks the native-platform gate exactly.
    assert_eq!(
        capture_assistant_allowed(ClaudeCode),
        local_hook_policy_v1_supported()
    );
}

#[test]
fn prompt_capture_options_are_claude_code_only() {
    use crate::cli::AgentChoice::*;
    assert!(prompt_capture_options_allowed(ClaudeCode));
    for agent in [
        Codex,
        CommandCode,
        Cursor,
        GeminiCli,
        OpenCode,
        Pi,
        Omp,
        Openclaw,
        AntigravityCli,
        Grok,
        Zero,
        Devin,
        KimiCode,
        KiroCli,
        KiroCliV3,
        Pool,
        Zcode,
    ] {
        assert!(!prompt_capture_options_allowed(agent), "{agent:?}");
    }
}

#[test]
fn baked_prompt_capture_reads_only_owned_claude_hooks() {
    let enabled = serde_json::json!({
        "hooks": {
            "SessionStart": [{ "hooks": [{ "command": "sessionmunch hook --event session-start --agent claude-code --server-url http://h" }] }],
            "UserPromptSubmit": [{ "hooks": [{ "command": "sessionmunch hook --event user-prompt --agent claude-code --server-url http://h" }] }]
        }
    });
    assert_eq!(
        baked_claude_prompt_capture(&enabled.to_string()),
        Some(true)
    );

    let disabled = serde_json::json!({
        "hooks": {
            "SessionStart": [{ "hooks": [{ "command": "sessionmunch hook --event session-start --agent claude-code --server-url http://h" }] }],
            "UserPromptSubmit": [{ "hooks": [{ "command": "third-party prompt guard" }] }]
        }
    });
    assert_eq!(
        baked_claude_prompt_capture(&disabled.to_string()),
        Some(false)
    );

    let unrelated = serde_json::json!({
        "hooks": {
            "UserPromptSubmit": [{ "hooks": [{ "command": "third-party prompt guard" }] }]
        }
    });
    assert_eq!(baked_claude_prompt_capture(&unrelated.to_string()), None);
}

#[cfg(unix)]
fn bash_program_for_installer_test() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from("bash"))
}

#[cfg(windows)]
fn bash_program_for_installer_test() -> Option<std::path::PathBuf> {
    let mut candidates = Vec::new();
    if let Some(root) = std::env::var_os("EXEPATH") {
        let root = std::path::PathBuf::from(root);
        candidates.push(root.join("bin").join("bash.exe"));
        candidates.push(root.join("usr").join("bin").join("bash.exe"));
    }
    for env_key in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(root) = std::env::var_os(env_key) {
            let root = std::path::PathBuf::from(root).join("Git");
            candidates.push(root.join("bin").join("bash.exe"));
            candidates.push(root.join("usr").join("bin").join("bash.exe"));
        }
    }
    candidates.sort();
    candidates.dedup();
    let found = candidates.into_iter().find(|candidate| candidate.is_file());
    if found.is_none() {
        eprintln!("skipping installer shell contract: Git for Windows bash.exe was not found");
    }
    found
}

#[test]
fn overlay_event_hooks_preserves_third_party_and_replaces_own() {
    // Regression for issue #80: install-hooks must MERGE into the event
    // array, not replace it. A third-party SessionStart hook (e.g.
    // context-mode) must survive while our own stale entry is swapped
    // for the fresh one.
    let mut hooks = serde_json::Map::new();
    hooks.insert(
            "SessionStart".into(),
            serde_json::json!([
                { "hooks": [ { "type": "command", "command": "node context-mode-cache-heal.mjs" } ] },
                { "matcher": "", "hooks": [ { "type": "command", "command": "/old/sessionmunch.exe hook --event session-start --agent claude-code --server-url http://old" } ] }
            ]),
        );
    let ours = serde_json::json!([
        { "matcher": "", "hooks": [ { "type": "command", "command": "/new/.cargo/bin/sessionmunch.exe hook --event session-start --agent claude-code --server-url http://new" } ] }
    ]);
    overlay_event_hooks(&mut hooks, "SessionStart", &ours);

    let arr = hooks["SessionStart"].as_array().unwrap();
    assert_eq!(
        arr.len(),
        2,
        "third-party + our single fresh entry: {arr:?}"
    );
    let joined = serde_json::to_string(arr).unwrap();
    assert!(
        joined.contains("context-mode-cache-heal"),
        "third-party hook must survive"
    );
    assert!(
        !joined.contains("/old/sessionmunch.exe"),
        "stale sessionmunch entry must be replaced"
    );
    assert!(
        joined.contains("/new/.cargo/bin/sessionmunch.exe"),
        "fresh sessionmunch entry must be present"
    );
}

#[test]
fn command_code_apply_uses_only_stable_events_and_preserves_user_settings() {
    let source = TempDir::new().unwrap();
    stub_scripts(
        source.path(),
        &[
            "session-start.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "stop.sh",
        ],
    );
    let staging_root = TempDir::new().unwrap();
    // A real data dir ends in `sessionmunch`; the entry-matching below keys
    // off that, so inject a realistic one rather than a bare temp path.
    let staging = staging_root.path().join("sessionmunch");
    let config_dir = TempDir::new().unwrap();
    let config_path = config_dir.path().join("settings.json");
    fs::write(
        &config_path,
        serde_json::json!({
            "theme": "dark",
            "hooks": {
                "SessionStart": [{
                    "hooks": [{"type": "command", "command": "third-party"}]
                }]
            }
        })
        .to_string(),
    )
    .unwrap();
    let args = InstallHooksArgs {
        agent: AgentChoice::CommandCode,
        config_file: Some(config_path.clone()),
        ..default_hook_args()
    };

    apply_to_command_code_settings_in(
        source.path(),
        "http://memory:49374",
        Some("token"),
        config_dir.path(),
        &staging,
        &args,
    )
    .unwrap();
    let first = fs::read_to_string(&config_path).unwrap();
    apply_to_command_code_settings_in(
        source.path(),
        "http://memory:49374",
        Some("token"),
        config_dir.path(),
        &staging,
        &args,
    )
    .unwrap();
    let second = fs::read_to_string(&config_path).unwrap();
    assert_eq!(first, second, "re-apply must be idempotent");

    let value: serde_json::Value = serde_json::from_str(&second).unwrap();
    assert_eq!(value["theme"], "dark");
    let hooks = value["hooks"].as_object().unwrap();
    for event in ["SessionStart", "PreToolUse", "PostToolUse", "Stop"] {
        let entries = hooks[event].as_array().unwrap();
        let ours = entries
            .iter()
            .find(|entry| {
                serde_json::to_string(entry)
                    .unwrap()
                    .contains("sessionmunch")
            })
            .unwrap_or_else(|| panic!("missing sessionmunch entry for {event}"));
        assert!(ours.get("matcher").is_none(), "event: {event}");
    }
    assert_eq!(
        hooks["SessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| serde_json::to_string(entry)
                .unwrap()
                .contains("third-party"))
            .count(),
        1,
        "third-party hook must survive"
    );
    assert_eq!(hooks.len(), COMMAND_CODE_PROFILE.events.len());
}

#[test]
fn overlay_event_hooks_inserts_when_event_absent() {
    let mut hooks = serde_json::Map::new();
    let ours = serde_json::json!([
        { "matcher": "", "hooks": [ { "type": "command", "command": "sessionmunch.exe hook --event stop --agent claude-code --server-url http://h" } ] }
    ]);
    overlay_event_hooks(&mut hooks, "Stop", &ours);
    assert_eq!(hooks["Stop"].as_array().unwrap().len(), 1);
}

#[test]
fn overlay_event_hooks_is_idempotent_on_reapply() {
    // Re-applying must not accumulate duplicate sessionmunch entries.
    let mut hooks = serde_json::Map::new();
    let ours = serde_json::json!([
        { "matcher": "", "hooks": [ { "type": "command", "command": "sessionmunch.exe hook --event pre-tool-use --agent claude-code --server-url http://h" } ] }
    ]);
    overlay_event_hooks(&mut hooks, "PreToolUse", &ours);
    overlay_event_hooks(&mut hooks, "PreToolUse", &ours);
    assert_eq!(
        hooks["PreToolUse"].as_array().unwrap().len(),
        1,
        "no duplicates on re-apply"
    );
}

#[test]
fn is_sessionmunch_hook_entry_detects_nested_flat_and_skips_third_party() {
    // Nested (Claude Code / Codex / Gemini)
    assert!(is_sessionmunch_hook_entry(&serde_json::json!(
        { "matcher": "", "hooks": [ { "type": "command", "command": "sessionmunch.exe hook --event session-start --agent claude-code --server-url http://h" } ] }
    )));
    // Flat (Cursor) + shell form
    assert!(is_sessionmunch_hook_entry(&serde_json::json!(
        { "type": "command", "command": "bash -c 'SESSIONMUNCH_HOOK_URL=x /c/x/sessionmunch/hooks/pre.sh'" }
    )));
    // Claude Code exec form
    assert!(is_sessionmunch_hook_entry(&serde_json::json!(
        { "matcher": "", "hooks": [ { "type": "command", "command": "C:\\bin\\sessionmunch.exe", "args": ["hook", "--event", "session-start", "--agent", "claude-code", "--server-url", "http://h"] } ] }
    )));
    // Third-party must NOT be flagged
    assert!(!is_sessionmunch_hook_entry(&serde_json::json!(
        { "hooks": [ { "type": "command", "command": "node context-mode-cache-heal.mjs" } ] }
    )));
    assert!(!is_sessionmunch_hook_entry(&serde_json::json!(
        { "hooks": [ { "type": "command", "command": "C:\\bin\\third-party.exe", "args": ["hook", "--event", "session-start", "--agent", "claude-code", "--server-url", "http://h"] } ] }
    )));
    assert!(!is_sessionmunch_hook_entry(&serde_json::json!(
        { "hooks": [ { "type": "command", "command": "C:\\bin\\sessionmunch-helper.exe", "args": ["--check", "project"] } ] }
    )));
    // A third-party string command may use the same executable name, but
    // without either sessionmunch hook signature it must not be replaced.
    assert!(!is_sessionmunch_hook_entry(&serde_json::json!(
        { "hooks": [ { "type": "command", "command": "/opt/alphaone/bin/sessionmunch boot --quiet --limit 10 --budget-tokens 4096" } ] }
    )));
    // Legacy script commands can use any path, so the exact env marker is
    // the ownership signal rather than the script basename.
    assert!(is_sessionmunch_hook_entry(&serde_json::json!(
        { "hooks": [ { "type": "command", "command": "SESSIONMUNCH_HOOK_URL=http://h /custom/session-start.sh" } ] }
    )));
}

#[test]
fn overlay_event_hooks_preserves_third_party_string_command_and_replaces_legacy() {
    let third_party = "/opt/alphaone/bin/sessionmunch boot --quiet --limit 10 --budget-tokens 4096";
    let legacy = "SESSIONMUNCH_HOOK_URL=http://old /custom/session-start.sh";
    let mut hooks = serde_json::Map::new();
    hooks.insert(
        "SessionStart".into(),
        serde_json::json!([
            { "matcher": "", "hooks": [ { "type": "command", "command": third_party } ] },
            { "matcher": "", "hooks": [ { "type": "command", "command": legacy } ] }
        ]),
    );
    let ours = serde_json::json!([
        { "matcher": "", "hooks": [ { "type": "command", "command": "SESSIONMUNCH_HOOK_URL=http://new /fresh/session-start.sh" } ] }
    ]);

    overlay_event_hooks(&mut hooks, "SessionStart", &ours);

    let commands: Vec<&str> = hooks["SessionStart"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| {
            entry
                .pointer("/hooks/0/command")
                .and_then(serde_json::Value::as_str)
        })
        .collect();
    assert_eq!(
        commands,
        [
            third_party,
            "SESSIONMUNCH_HOOK_URL=http://new /fresh/session-start.sh"
        ]
    );
    assert!(!commands.contains(&legacy), "legacy entry must be replaced");
}

fn stub_scripts(dir: &Path, names: &[&str]) {
    for name in names {
        let p = dir.join(name);
        fs::write(&p, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&p).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&p, perms).unwrap();
        }
    }
}

fn default_hook_args() -> InstallHooksArgs {
    InstallHooksArgs {
        profile: None,
        agent: AgentChoice::OpenCode,
        capture_assistant: false,
        no_capture_prompts: false,
        capture_mode: None,
        capture_prompts: false,
        hooks_dir: None,
        server_url: None,
        auth_token: None,
        as_user: None,
        apply: true,
        config_file: None,
        project_strategy: Some(ProjectStrategyArg::Basename),
    }
}

// ── #issue project-strategy preservation on re-apply ─────────────

#[test]
fn project_strategy_from_text_reads_every_form() {
    // Shell env prefix (Claude Code / POSIX script hooks).
    assert_eq!(
        project_strategy_from_text(
            "SESSIONMUNCH_HOOK_URL=http://h SESSIONMUNCH_PROJECT_STRATEGY=repo-root /x/s.sh"
        ),
        Some(ProjectStrategyArg::RepoRoot)
    );
    // PowerShell form.
    assert_eq!(
        project_strategy_from_text("$env:SESSIONMUNCH_PROJECT_STRATEGY='repo-root'; & /x/s.ps1"),
        Some(ProjectStrategyArg::RepoRoot)
    );
    // Native flag, both spellings.
    assert_eq!(
        project_strategy_from_text("sessionmunch hook --project-strategy repo-root run"),
        Some(ProjectStrategyArg::RepoRoot)
    );
    assert_eq!(
        project_strategy_from_text("sessionmunch hook --project-strategy=repo_root"),
        Some(ProjectStrategyArg::RepoRoot)
    );
    assert_eq!(
        project_strategy_from_text("const DEFAULT_PROJECT_STRATEGY = \"repo-root\";"),
        Some(ProjectStrategyArg::RepoRoot)
    );
}

#[test]
fn baked_project_strategy_reads_owned_json_for_every_json_agent() {
    let existing = serde_json::json!({
        "hooks": {
            "SessionStart": [
                { "hooks": [{
                    "command": "third-party",
                    "args": ["--project-strategy", "repo-root"]
                }]},
                { "hooks": [{
                    "command": "/bin/sessionmunch",
                    "args": [
                        "hook", "--event", "session-start", "--agent", "claude-code",
                        "--server-url", "http://h", "--project-strategy", "repo-root"
                    ]
                }]}
            ]
        }
    })
    .to_string();
    for agent in [
        AgentChoice::ClaudeCode,
        AgentChoice::Codex,
        AgentChoice::Cursor,
        AgentChoice::GeminiCli,
        AgentChoice::AntigravityCli,
        AgentChoice::Grok,
        AgentChoice::Zero,
        AgentChoice::Devin,
    ] {
        assert_eq!(
            baked_project_strategy(agent, &existing),
            Some(ProjectStrategyArg::RepoRoot),
            "{agent:?}"
        );
    }
}

#[test]
fn baked_project_strategy_ignores_unowned_json_entry() {
    let existing = serde_json::json!({
        "hooks": {
            "SessionStart": [{ "hooks": [{
                "command": "third-party",
                "args": ["--project-strategy", "repo-root"]
            }]}]
        }
    })
    .to_string();
    assert_eq!(
        baked_project_strategy(AgentChoice::ClaudeCode, &existing),
        None
    );
}

#[test]
fn baked_project_strategy_reads_owned_kimi_toml_only() {
    let existing = r#"
[[hooks]]
event = "UserPromptSubmit"
command = "third-party --project-strategy repo-root"

[[hooks]]
event = "UserPromptSubmit"
command = "SESSIONMUNCH_HOOK_URL=http://h SESSIONMUNCH_PROJECT_STRATEGY=repo-root /x/sessionmunch/session-start.sh"
"#;
    assert_eq!(
        baked_project_strategy(AgentChoice::KimiCode, existing),
        Some(ProjectStrategyArg::RepoRoot)
    );
}

#[test]
fn baked_project_strategy_reads_every_owned_generated_integration() {
    for (agent, name) in [
        (AgentChoice::OpenCode, "opencode"),
        (AgentChoice::OpenCode2, "opencode2"),
        (AgentChoice::Pi, "pi"),
        (AgentChoice::Omp, "omp"),
        (AgentChoice::Openclaw, "openclaw"),
    ] {
        let existing = format!(
            "// Auto-generated by `sessionmunch install-hooks --agent {name} --apply`.\n\
                 const DEFAULT_PROJECT_STRATEGY = \"repo-root\";\n"
        );
        assert_eq!(
            baked_project_strategy(agent, &existing),
            Some(ProjectStrategyArg::RepoRoot),
            "{agent:?}"
        );
    }

    let unowned = "// user extension\nconst DEFAULT_PROJECT_STRATEGY = \"repo-root\";\n";
    assert_eq!(baked_project_strategy(AgentChoice::OpenCode, unowned), None);
}

#[test]
fn install_project_strategy_preserves_baked_when_flag_absent() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("settings.json");
    std::fs::write(
            &cfg,
            serde_json::json!({
                "hooks": { "SessionStart": [{ "hooks": [{
                    "command": "SESSIONMUNCH_HOOK_URL=http://h SESSIONMUNCH_PROJECT_STRATEGY=repo-root /x/sessionmunch/session-start.sh"
                }]}] }
            })
            .to_string(),
        )
        .unwrap();
    let args = InstallHooksArgs {
        agent: AgentChoice::ClaudeCode,
        config_file: Some(cfg),
        project_strategy: None,
        ..default_hook_args()
    };
    // A bare re-apply (e.g. `sessionmunch upgrade`) must keep repo-root.
    assert_eq!(
        install_project_strategy(&args),
        Some(ProjectStrategyArg::RepoRoot)
    );
}

#[test]
fn install_project_strategy_explicit_basename_overrides_existing() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("settings.json");
    std::fs::write(
            &cfg,
            serde_json::json!({
                "hooks": { "SessionStart": [{ "hooks": [{
                    "command": "SESSIONMUNCH_HOOK_URL=http://h SESSIONMUNCH_PROJECT_STRATEGY=repo-root /x/sessionmunch/session-start.sh"
                }]}] }
            })
            .to_string(),
        )
        .unwrap();
    let args = InstallHooksArgs {
        agent: AgentChoice::ClaudeCode,
        config_file: Some(cfg),
        project_strategy: Some(ProjectStrategyArg::Basename),
        ..default_hook_args()
    };
    // Explicit basename is honored, not overridden by the baked repo-root.
    assert_eq!(
        install_project_strategy(&args),
        Some(ProjectStrategyArg::Basename)
    );
}

#[test]
fn install_project_strategy_none_when_target_absent() {
    let tmp = TempDir::new().unwrap();
    let args = InstallHooksArgs {
        agent: AgentChoice::ClaudeCode,
        config_file: Some(tmp.path().join("nope.json")),
        project_strategy: None,
        ..default_hook_args()
    };
    assert_eq!(install_project_strategy(&args), None);
}

#[test]
fn install_project_strategy_reads_openclaw_entrypoint_below_config_dir() {
    let tmp = TempDir::new().unwrap();
    let entrypoint = tmp.path().join(openclaw_plugin::ENTRYPOINT_TS);
    std::fs::write(
        entrypoint,
        "// Auto-generated by `sessionmunch install-hooks --agent openclaw --apply`.\n\
             const DEFAULT_PROJECT_STRATEGY = \"repo-root\";\n",
    )
    .unwrap();
    let args = InstallHooksArgs {
        agent: AgentChoice::Openclaw,
        config_file: Some(tmp.path().to_path_buf()),
        project_strategy: None,
        ..default_hook_args()
    };
    assert_eq!(
        install_project_strategy(&args),
        Some(ProjectStrategyArg::RepoRoot)
    );
}

// ── P1.8 validate_as_user ────────────────────────────────────────

/// No `--as-user` at all → always OK.
#[test]
fn validate_as_user_passes_when_not_set() {
    assert!(validate_as_user(None, None).is_ok());
    assert!(validate_as_user(None, Some("tok")).is_ok());
}

/// Empty / whitespace-only `--as-user` is treated as not-set.
/// Defensive: an accidental `--as-user ""` shouldn't bail.
#[test]
fn validate_as_user_treats_blank_as_unset() {
    assert!(validate_as_user(Some(""), None).is_ok());
    assert!(validate_as_user(Some("   "), None).is_ok());
}

/// `--as-user` with no `--auth-token` is the error case the v0.8
/// docs warn about — without a token the hook scripts authenticate
/// anonymously / as root, making the `--as-user X` label misleading.
#[test]
fn validate_as_user_bails_without_auth_token() {
    let err = validate_as_user(Some("alice"), None).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("--as-user 'alice'") && msg.contains("--auth-token"),
        "error must name both flags: {msg}"
    );
    // Empty auth token is treated the same as missing.
    assert!(validate_as_user(Some("alice"), Some("")).is_err());
    assert!(validate_as_user(Some("alice"), Some("   ")).is_err());
}

/// `--as-user X --auth-token <something>` passes — the install
/// proceeds with X as metadata and the supplied token as the
/// bearer.
#[test]
fn validate_as_user_passes_with_both_flags() {
    assert!(validate_as_user(Some("alice"), Some("some-token")).is_ok());
}

#[test]
fn manual_agent_render_mentions_repo_root_project_strategy() {
    let temp = TempDir::new().unwrap();
    stub_scripts(temp.path(), &["session-start.sh"]);
    for agent in ["codex", "cursor", "gemini-cli", "antigravity-cli"] {
        let output = render_agent_output(
            agent,
            temp.path(),
            "http://127.0.0.1:49374",
            None,
            Some("repo-root"),
            &[CODEX_PROFILE.events],
        );
        assert!(
            output.contains("SESSIONMUNCH_PROJECT_STRATEGY=repo-root"),
            "{agent} manual output must tell users to set the strategy env: {output}"
        );
    }
}

#[test]
fn manual_agent_render_omits_project_strategy_by_default() {
    let temp = TempDir::new().unwrap();
    stub_scripts(temp.path(), &["session-start.sh"]);
    let output = render_agent_output(
        "codex",
        temp.path(),
        "http://127.0.0.1:49374",
        None,
        None,
        &[CODEX_PROFILE.events],
    );
    assert!(!output.contains("SESSIONMUNCH_PROJECT_STRATEGY"));
}

#[test]
fn antigravity_manual_render_explains_explicit_finalization() {
    let temp = TempDir::new().unwrap();
    stub_scripts(temp.path(), &["session-start.sh", "stop.sh"]);
    let output = render_agent_output(
        "antigravity-cli",
        temp.path(),
        "http://127.0.0.1:49374",
        None,
        None,
        &[&ANTIGRAVITY_LIFECYCLE_EVENTS],
    );

    assert!(output.contains("`Stop` ends one execution loop, not the conversation"));
    assert!(
        output.contains("sessionmunch finalize-session --agent antigravity-cli"),
        "Antigravity install output must expose the supported finalizer: {output}"
    );
}

#[test]
fn manual_agent_render_uses_agent_profile_not_physical_bundle_listing() {
    let temp = TempDir::new().unwrap();
    stub_scripts(
        temp.path(),
        &[
            "session-start.sh",
            "session-end.sh",
            "user-prompt-submit.sh",
            "stop.sh",
            "subagent-start.sh",
            "subagent-stop.sh",
        ],
    );

    let gemini = render_agent_output(
        "gemini-cli",
        temp.path(),
        "http://127.0.0.1:49374",
        None,
        None,
        &[GEMINI_PROFILE.events],
    );
    assert!(gemini.contains("session-start"));
    assert!(gemini.contains("session-end"));
    assert!(
        !gemini.contains("user-prompt-submit")
            && !gemini.contains("subagent-start")
            && !gemini.contains("subagent-stop"),
        "Gemini manual output must omit scripts outside Gemini's hook vocabulary: {gemini}"
    );

    let codex = render_agent_output(
        "codex",
        temp.path(),
        "http://127.0.0.1:49374",
        None,
        None,
        &[CODEX_PROFILE.events],
    );
    assert!(codex.contains("stop"));
    assert!(codex.contains("session-end"));
    assert!(
        !codex.contains("subagent-start") && !codex.contains("subagent-stop"),
        "Codex manual output must omit scripts outside Codex's hook vocabulary: {codex}"
    );
}

#[test]
fn hook_server_url_defaults_to_configured_server_url() {
    let config = Config {
        server_url: "http://192.168.0.90:49374/".into(),
        ..Config::default()
    };
    let args = default_hook_args();

    assert_eq!(
        effective_hook_server_url(&config, &args, None),
        "http://192.168.0.90:49374"
    );
}

#[test]
fn hook_server_url_explicit_flag_wins_over_config() {
    let config = Config {
        server_url: "http://homelab:49374".into(),
        ..Config::default()
    };
    let mut args = default_hook_args();
    args.server_url = Some("http://explicit:49374/".into());

    assert_eq!(
        effective_hook_server_url(&config, &args, None),
        "http://explicit:49374"
    );
}

/// Regression (found 2026-07-12 during Devin real-acceptance A/B
/// testing): an explicit `--server-url` that happens to equal the
/// compiled-in `DEFAULT_SERVER_URL` must still win over a configured
/// (env/config.toml) server_url pointing somewhere else. Before the
/// `Option<String>` fix, `args.server_url` was a plain `String` with
/// `default_value_t = DEFAULT_SERVER_URL`, so clap couldn't
/// distinguish "operator explicitly typed the default value" from
/// "operator passed nothing at all" — both produced the same string,
/// so this exact case silently fell through to `SESSIONMUNCH_SERVER_URL`
/// / config.toml instead of honouring the explicit flag.
#[test]
fn hook_server_url_explicit_flag_matching_compiled_default_still_wins() {
    let config = Config {
        server_url: "http://127.0.0.1:49375".into(),
        ..Config::default()
    };
    let mut args = default_hook_args();
    args.server_url = Some(DEFAULT_SERVER_URL.to_string());

    assert_eq!(
        effective_hook_server_url(&config, &args, None),
        DEFAULT_SERVER_URL,
        "an explicit --server-url matching the compiled default must not be \
             silently overridden by a differently-configured server_url"
    );
}

/// Post-audit P1 — the new `sessionmunch hook` subcommand (#84) builds
/// its request URL by hand, skipping `Config::load` for latency. PR
/// #82 made thin-client commands respect `SESSIONMUNCH_BASE_PATH` via
/// `ServerEndpoint::build_url`, but the hook subcommand doesn't go
/// through there — so a deployment under `--base-path /wiki` with
/// the base set via env (not the URL path) had `sessionmunch status`
/// working and `sessionmunch hook` 404'ing. Fix: install-hooks bakes
/// the prefix into the URL it embeds, so hook.rs uses what it's
/// given and stays unchanged.
#[test]
fn hook_server_url_threads_base_path_when_url_has_no_path() {
    let config = Config {
        server_url: "http://homelab:49374".into(),
        base_path: "/wiki".into(),
        ..Config::default()
    };
    let args = default_hook_args();
    assert_eq!(
        effective_hook_server_url(&config, &args, None),
        "http://homelab:49374/wiki",
        "URL baked into the hook command must carry the base-path so \
             `sessionmunch hook` POSTs to /wiki/hook (not /hook)"
    );
}

/// If the operator already put the prefix into the URL itself, do
/// NOT append `base_path` on top — that would double the prefix to
/// `/wiki/wiki`.
#[test]
fn hook_server_url_does_not_double_base_path_when_already_in_url() {
    let config = Config {
        server_url: "http://homelab:49374/wiki".into(),
        base_path: "/wiki".into(),
        ..Config::default()
    };
    let args = default_hook_args();
    assert_eq!(
        effective_hook_server_url(&config, &args, None),
        "http://homelab:49374/wiki"
    );
}

#[test]
fn hook_server_url_falls_back_to_existing_mcp_entry() {
    let config = Config::default();
    let args = default_hook_args();
    let inferred = InferredMcpConfig {
        hook_server_url: Some("http://homelab:49374".into()),
        auth_token: Some("tok".into()),
    };

    assert_eq!(
        effective_hook_server_url(&config, &args, Some(&inferred)),
        "http://homelab:49374"
    );
}

#[test]
fn resolve_hooks_dir_uses_grok_bundle_for_grok() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join("grok")).unwrap();
    fs::create_dir_all(tmp.path().join("claude-code")).unwrap();

    let resolved = resolve_hooks_dir(Some(tmp.path()), AgentChoice::Grok, tmp.path()).unwrap();
    assert_eq!(resolved, tmp.path().join("grok"));
}

#[test]
fn resolve_hooks_dir_uses_pool_bundle_for_pool() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join("pool")).unwrap();
    fs::create_dir_all(tmp.path().join("claude-code")).unwrap();

    let resolved = resolve_hooks_dir(Some(tmp.path()), AgentChoice::Pool, tmp.path()).unwrap();
    assert_eq!(resolved, tmp.path().join("pool"));
}

#[test]
fn pool_settings_yaml_covers_all_events_with_bounded_entries() {
    let snippet = super::super::render_shared::build_pool_settings_yaml_with_data_dir(
        Path::new("/staging/pool"),
        "http://127.0.0.1:49374",
        Some("tok-test"),
        Some(Path::new("/data")),
        Some("repo-root"),
    );
    assert!(snippet.starts_with("hooks:\n"), "snippet: {snippet}");
    for (event, script) in POOL_EVENTS {
        assert!(
            snippet.contains(&format!("  {event}:\n")),
            "missing {event} in: {snippet}"
        );
        let stem = script.strip_suffix(".sh").unwrap();
        assert!(
            snippet.contains(&format!("- name: sessionmunch-{stem}\n")),
            "missing name for {event} in: {snippet}"
        );
    }
    assert_eq!(
        snippet.matches("matcher: \"*\"").count(),
        POOL_EVENTS.len(),
        "one wildcard matcher per event"
    );
    assert_eq!(
        snippet.matches("timeout: 20").count(),
        POOL_EVENTS.len(),
        "one bounded timeout per event"
    );
    // Every command is a YAML single-quoted scalar so the embedded POSIX
    // quoting survives; the auth token rides inside the command.
    assert_eq!(
        snippet.matches("command: '").count(),
        POOL_EVENTS.len(),
        "one single-quoted command per event"
    );
    assert!(snippet.contains("tok-test"), "auth token must be embedded");
}

#[test]
fn render_pool_output_states_manual_paste_and_finalize_path() {
    let out = render_pool_output(
        Path::new("/staging/pool"),
        "http://127.0.0.1:49374",
        None,
        Path::new("/data"),
        None,
    );
    assert!(out.contains(".poolside/settings.yaml"));
    assert!(out.contains("does not write project-local files"));
    assert!(out.contains("finalize-session --agent pool"));
    assert!(out.contains("memory_handoff_accept"));
    assert!(out.contains("hooks:\n"));
    // No token was passed, so none of the auth caution lines render.
    assert!(!out.contains("SESSIONMUNCH_AUTH_TOKEN embedded"));
}

// Issue #156: Zero hook install writes exec-form entries into Zero's
// hooks.json shape and merges around third-party hooks by id prefix.
#[test]
fn zero_hooks_config_covers_all_events_in_exec_form() {
    let payload = super::super::render_shared::build_zero_hooks_config(
        "http://127.0.0.1:49374",
        Some("tok-test"),
        Some(Path::new("/data")),
        Some("repo-root"),
    );
    assert_eq!(payload["enabled"], serde_json::json!(true));
    let hooks = payload["hooks"].as_array().unwrap();
    assert_eq!(hooks.len(), 6, "one entry per Zero lifecycle event");
    let events: Vec<&str> = hooks.iter().map(|h| h["event"].as_str().unwrap()).collect();
    for zero_event in [
        "sessionStart",
        "sessionEnd",
        "beforeTool",
        "afterTool",
        "specialistStart",
        "specialistStop",
    ] {
        assert!(events.contains(&zero_event), "missing {zero_event}");
    }
    for hook in hooks {
        let id = hook["id"].as_str().unwrap();
        assert!(id.starts_with("sessionmunch-"), "ownership prefix: {id}");
        assert!(hook["enabled"].as_bool().unwrap());
        let args: Vec<&str> = hook["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        // Exec form: the native hook subcommand with agent + auth +
        // strategy — never a shell string.
        assert!(args.contains(&"hook"), "{args:?}");
        assert!(
            args.contains(&"--agent") && args.contains(&"zero"),
            "{args:?}"
        );
        assert!(args.contains(&"--auth-token") && args.contains(&"tok-test"));
        assert!(args.contains(&"--project-strategy") && args.contains(&"repo-root"));
        assert!(args.contains(&"--data-dir") && args.contains(&"/data"));
    }
}

#[test]
fn zero_apply_merges_around_third_party_hooks_and_preserves_disabled_flag() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("hooks.json");
    fs::write(
        &path,
        r#"{"enabled": false, "hooks": [
                {"id": "my-custom-hook", "event": "beforeTool",
                 "command": "/usr/bin/true", "args": [], "enabled": true},
                {"id": "sessionmunch-session-start", "event": "sessionStart",
                 "command": "/old/sessionmunch", "args": [], "enabled": true}
            ]}"#,
    )
    .unwrap();
    let args = InstallHooksArgs {
        agent: AgentChoice::Zero,
        capture_assistant: false,
        config_file: Some(path.clone()),
        ..default_hook_args()
    };

    apply_to_zero_hooks("http://127.0.0.1:49374", None, Path::new("/data"), &args).unwrap();

    let root: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        root["enabled"],
        serde_json::json!(false),
        "a user-disabled hooks file must stay disabled (we warn instead)"
    );
    let hooks = root["hooks"].as_array().unwrap();
    assert!(
        hooks
            .iter()
            .any(|h| h["id"] == serde_json::json!("my-custom-hook")),
        "third-party hooks must survive the merge"
    );
    let ours: Vec<&serde_json::Value> = hooks
        .iter()
        .filter(|h| {
            h["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("sessionmunch-"))
        })
        .collect();
    assert_eq!(
        ours.len(),
        6,
        "stale sessionmunch entries replaced, not duplicated"
    );
    assert!(
        ours.iter()
            .all(|h| h["command"] != serde_json::json!("/old/sessionmunch")),
        "the stale command path must be replaced"
    );
}

#[test]
fn zcode_hooks_config_covers_all_events_with_strict_entries() {
    let payload = super::super::render_shared::build_zcode_hooks_config(
        "http://127.0.0.1:49374",
        Some("tok-test"),
        Some(Path::new("/data")),
        Some("repo-root"),
    );
    assert_eq!(payload["enabled"], serde_json::json!(true));
    // Raised above ZCode's 32 KiB default so a fetched handoff is not
    // truncated before injection.
    assert_eq!(payload["maxOutputBytes"], serde_json::json!(65536));
    let events = payload["events"].as_object().unwrap();
    assert_eq!(
        events.len(),
        super::super::render_shared::ZCODE_EVENTS.len(),
        "one entry per documented ZCode trigger"
    );
    for (zcode_event, our_event) in super::super::render_shared::ZCODE_EVENTS {
        let groups = events[zcode_event].as_array().unwrap();
        assert_eq!(groups.len(), 1, "{zcode_event}: one matcher group");
        let hooks = groups[0]["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 1, "{zcode_event}: one hook entry");
        let hook = &hooks[0];
        // Exec form: `type: "process"` spawns command + args with no
        // shell, so every key must be from ZCode's documented schema —
        // undocumented keys make it drop the entry.
        assert_eq!(hook["type"], serde_json::json!("process"), "{zcode_event}");
        assert_eq!(
            hook["statusMessage"].as_str().unwrap(),
            "sessionmunch capture",
            "{zcode_event}: ownership marker"
        );
        assert_eq!(hook["timeoutMs"], serde_json::json!(10_000));
        for key in hook.as_object().unwrap().keys() {
            assert!(
                matches!(
                    key.as_str(),
                    "type" | "command" | "args" | "enabled" | "timeoutMs" | "statusMessage"
                ),
                "{zcode_event}: undocumented hook key {key}"
            );
        }
        let args: Vec<&str> = hook["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert!(args.contains(&"hook"), "{args:?}");
        assert!(
            args.contains(&"--event") && args.contains(&our_event),
            "{args:?}"
        );
        assert!(
            args.contains(&"--agent") && args.contains(&"zcode"),
            "{args:?}"
        );
        assert!(args.contains(&"--server-url") && args.contains(&"http://127.0.0.1:49374"));
        assert!(args.contains(&"--auth-token") && args.contains(&"tok-test"));
        assert!(args.contains(&"--data-dir") && args.contains(&"/data"));
        assert!(args.contains(&"--project-strategy") && args.contains(&"repo-root"));
    }
    // PostToolUseFailure fires instead of PostToolUse when the tool
    // throws; the loop above already proved it forwards onto the same
    // `post-tool-use` channel as its success sibling.
    assert!(events.contains_key("PostToolUseFailure"));
}

#[test]
fn zcode_apply_preserves_siblings_and_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("config.json");
    fs::write(
        &path,
        r#"{
  "theme": "dark",
  "model": {"main": "zai/glm-5.1"},
  "mcp": {"servers": {"other": {"type": "http", "url": "https://other.example/mcp"}}},
  "hooks": {
    "enabled": true,
    "events": {
      "SessionStart": [
        {"matcher": "Bash", "hooks": [
          {"type": "process", "command": "/usr/bin/true", "args": [], "enabled": true}
        ]},
        {"hooks": [
          {"type": "process", "command": "/old/sessionmunch", "args": [],
           "enabled": true, "statusMessage": "sessionmunch capture"}
        ]}
      ]
    }
  }
}"#,
    )
    .unwrap();
    let args = InstallHooksArgs {
        agent: AgentChoice::Zcode,
        capture_assistant: false,
        config_file: Some(path.clone()),
        ..default_hook_args()
    };

    apply_to_zcode_hooks(
        "http://127.0.0.1:49374",
        Some("tok-test"),
        Path::new("/data"),
        &args,
    )
    .unwrap();
    let first = fs::read_to_string(&path).unwrap();
    apply_to_zcode_hooks(
        "http://127.0.0.1:49374",
        Some("tok-test"),
        Path::new("/data"),
        &args,
    )
    .unwrap();
    let second = fs::read_to_string(&path).unwrap();

    assert_eq!(first, second, "re-apply must be a no-op");
    let root: serde_json::Value = serde_json::from_str(&second).unwrap();
    // Sibling config keys survive: hooks share the file with the model
    // and MCP registration (#511).
    assert_eq!(root["theme"], serde_json::json!("dark"));
    assert_eq!(root["model"]["main"], serde_json::json!("zai/glm-5.1"));
    assert_eq!(
        root["mcp"]["servers"]["other"]["url"],
        serde_json::json!("https://other.example/mcp")
    );
    // The handoff-injection stdout ceiling lands with the hooks block.
    assert_eq!(root["hooks"]["maxOutputBytes"], serde_json::json!(65536));
    let session_start = root["hooks"]["events"]["SessionStart"].as_array().unwrap();
    assert_eq!(
        session_start.len(),
        2,
        "the third-party matcher group must survive the merge"
    );
    let third_party = &session_start[0];
    assert_eq!(
        third_party["matcher"],
        serde_json::json!("Bash"),
        "third-party matcher groups are preserved untouched"
    );
    assert_eq!(
        third_party["hooks"][0]["command"],
        serde_json::json!("/usr/bin/true")
    );
    let ours = &session_start[1];
    assert_eq!(
        ours["hooks"][0]["command"],
        super::super::render_shared::build_zcode_hooks_config(
            "http://127.0.0.1:49374",
            Some("tok-test"),
            Some(Path::new("/data")),
            None
        )["events"]["SessionStart"][0]["hooks"][0]["command"],
        "the stale sessionmunch command path must be replaced"
    );
    for (zcode_event, _) in super::super::render_shared::ZCODE_EVENTS {
        assert!(
            root["hooks"]["events"][zcode_event].is_array(),
            "missing {zcode_event}"
        );
    }
}
#[test]
fn zcode_apply_withdraws_our_hooks_from_foreign_keys_and_keeps_the_rest() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("config.json");
    // Keys outside `ZCODE_EVENTS`. ZCode rejects the whole block over
    // ones it does not recognize, which is how #600 stayed invisible.
    // `SessionEnd` runs nothing, `PreCompact` holds only our entry, and
    // `Notification` mixes our entry with a hook we did not write.
    fs::write(
        &path,
        r#"{
  "hooks": {
    "enabled": true,
    "events": {
      "SessionEnd": [],
      "PreCompact": [
        {"hooks": [
          {"type": "process", "command": "/old/sessionmunch", "args": [],
           "enabled": true, "statusMessage": "sessionmunch capture"}
        ]}
      ],
      "Notification": [
        {"hooks": [
          {"type": "process", "command": "/old/sessionmunch", "args": [],
           "enabled": true, "statusMessage": "sessionmunch capture"},
          {"type": "process", "command": "/usr/bin/true", "args": [], "enabled": true}
        ]}
      ]
    }
  }
}"#,
    )
    .unwrap();
    let args = InstallHooksArgs {
        agent: AgentChoice::Zcode,
        capture_assistant: false,
        config_file: Some(path.clone()),
        ..default_hook_args()
    };

    apply_to_zcode_hooks(
        "http://127.0.0.1:49374",
        Some("tok-test"),
        Path::new("/data"),
        &args,
    )
    .unwrap();

    let first = fs::read_to_string(&path).unwrap();
    let root: serde_json::Value = serde_json::from_str(&first).unwrap();
    let events = root["hooks"]["events"].as_object().unwrap();

    // Event keys are never deleted: they are not sessionmunch's to remove.
    assert!(events.contains_key("SessionEnd"));
    assert!(events.contains_key("PreCompact"));
    // Our own stale entry is withdrawn wherever it sits.
    assert_eq!(
        events["PreCompact"][0]["hooks"].as_array().unwrap().len(),
        0,
        "our entry under a key we no longer write must be withdrawn"
    );
    // The mixed group is the only case where the ownership filter
    // decides anything: the foreign hook survives, ours does not.
    let notification = events["Notification"][0]["hooks"].as_array().unwrap();
    assert_eq!(notification.len(), 1, "the hook we did not write must stay");
    assert_eq!(
        notification[0]["command"],
        serde_json::json!("/usr/bin/true")
    );
    for (zcode_event, _) in super::super::render_shared::ZCODE_EVENTS {
        assert!(
            root["hooks"]["events"][zcode_event].is_array(),
            "missing {zcode_event}"
        );
    }

    // Re-apply is still a no-op once the withdrawal has happened.
    apply_to_zcode_hooks(
        "http://127.0.0.1:49374",
        Some("tok-test"),
        Path::new("/data"),
        &args,
    )
    .unwrap();
    assert_eq!(first, fs::read_to_string(&path).unwrap());
}

#[test]
fn zcode_event_runs_nothing_covers_degenerate_matcher_groups() {
    // Each of these leaves the key present but unable to run anything,
    // which is what makes ZCode reject the block while the file still
    // looks settled (#600).
    for shape in [
        serde_json::json!([]),
        serde_json::json!([{}]),
        serde_json::json!([{"matcher": "*"}]),
        serde_json::json!([{"hooks": []}]),
        serde_json::json!([{"hooks": null}]),
    ] {
        assert!(
            zcode_event_runs_nothing(&shape),
            "expected {shape} to run nothing"
        );
    }
    assert!(!zcode_event_runs_nothing(&serde_json::json!([
        {"hooks": [{"type": "process", "command": "/usr/bin/true"}]}
    ])));
    // A value that is not a matcher-group array cannot run hooks either,
    // and is doubly invalid to ZCode. It is left unmodified, but it is
    // still reported: staying quiet about it is the #600 failure.
    assert!(zcode_event_runs_nothing(&serde_json::json!(42)));
}

#[test]
fn zcode_apply_flags_a_user_disabled_hooks_block() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("config.json");
    fs::write(&path, r#"{"hooks": {"enabled": false, "events": {}}}"#).unwrap();
    let args = InstallHooksArgs {
        agent: AgentChoice::Zcode,
        capture_assistant: false,
        config_file: Some(path.clone()),
        ..default_hook_args()
    };

    apply_to_zcode_hooks("http://127.0.0.1:49374", None, Path::new("/data"), &args).unwrap();

    let root: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        root["hooks"]["enabled"],
        serde_json::json!(false),
        "a user-disabled hooks block must stay disabled (we warn instead)"
    );
    assert_eq!(
        root["hooks"]["events"].as_object().unwrap().len(),
        super::super::render_shared::ZCODE_EVENTS.len()
    );
}

#[test]
fn zcode_apply_respects_a_user_chosen_output_ceiling() {
    // `maxOutputBytes` is block-level and would also bound third-party
    // hooks, so a value the user chose is never overwritten.
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("config.json");
    fs::write(
        &path,
        r#"{"hooks": {"enabled": true, "maxOutputBytes": 4096, "events": {}}}"#,
    )
    .unwrap();
    let args = InstallHooksArgs {
        agent: AgentChoice::Zcode,
        capture_assistant: false,
        config_file: Some(path.clone()),
        ..default_hook_args()
    };

    apply_to_zcode_hooks("http://127.0.0.1:49374", None, Path::new("/data"), &args).unwrap();

    let root: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(root["hooks"]["maxOutputBytes"], serde_json::json!(4096));
}

#[test]
fn resolve_hooks_dir_uses_devin_bundle_for_devin() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join("devin")).unwrap();
    fs::create_dir_all(tmp.path().join("claude-code")).unwrap();

    let resolved = resolve_hooks_dir(Some(tmp.path()), AgentChoice::Devin, tmp.path()).unwrap();
    assert_eq!(resolved, tmp.path().join("devin"));
}

#[test]
fn opencode_mcp_inference_supplies_hook_origin_and_token() {
    let inferred = infer_json_mcp_config(
        r#"{
              "mcp": {
                "sessionmunch": {
                  "type": "remote",
                  "url": "http://homelab:49374/mcp",
                  "headers": { "Authorization": "Bearer secret-token" }
                }
              }
            }"#,
        &["mcp", "sessionmunch"],
        "url",
    )
    .unwrap();

    assert_eq!(
        inferred.hook_server_url.as_deref(),
        Some("http://homelab:49374")
    );
    assert_eq!(inferred.auth_token.as_deref(), Some("secret-token"));
}

/// Inferring the hook URL from Kimi Code's flavored mcp.json entry
/// must yield the bare origin — hooks POST to `<origin>/hook`.
#[test]
fn kimi_code_mcp_inference_strips_flavor_query_and_mcp_path() {
    let inferred = infer_json_mcp_config(
        r#"{
              "mcpServers": {
                "sessionmunch": {
                  "url": "http://homelab:49374/mcp?flavor=moonshot",
                  "headers": { "Authorization": "Bearer secret-token" }
                }
              }
            }"#,
        &["mcpServers", "sessionmunch"],
        "url",
    )
    .unwrap();

    assert_eq!(
        inferred.hook_server_url.as_deref(),
        Some("http://homelab:49374")
    );
    assert_eq!(inferred.auth_token.as_deref(), Some("secret-token"));
}

#[test]
fn hook_server_url_from_mcp_url_strips_query_and_suffix() {
    for (input, expected) in [
        ("http://homelab:49374/mcp", Some("http://homelab:49374")),
        ("http://homelab:49374", Some("http://homelab:49374")),
        ("http://homelab:49374/", Some("http://homelab:49374")),
        (
            "http://homelab:49374/mcp?flavor=moonshot",
            Some("http://homelab:49374"),
        ),
        (
            "http://homelab:49374/mcp/?flavor=moonshot",
            Some("http://homelab:49374"),
        ),
        // Reverse-proxy prefix survives; hooks POST under it.
        (
            "http://homelab:49374/wiki/mcp",
            Some("http://homelab:49374/wiki"),
        ),
        (
            "http://homelab:49374/wiki/mcp?flavor=moonshot",
            Some("http://homelab:49374/wiki"),
        ),
        ("", None),
        ("   ", None),
    ] {
        assert_eq!(
            hook_server_url_from_mcp_url(input).as_deref(),
            expected,
            "input: {input:?}"
        );
    }
}

#[test]
fn codex_mcp_inference_accepts_block_form_config() {
    let inferred = infer_toml_mcp_config(
        r#"[mcp_servers.sessionmunch]
url = "http://homelab:49374/mcp"

[mcp_servers.sessionmunch.http_headers]
Authorization = "Bearer secret-token"
"#,
    )
    .unwrap();

    assert_eq!(
        inferred.hook_server_url.as_deref(),
        Some("http://homelab:49374")
    );
    assert_eq!(inferred.auth_token.as_deref(), Some("secret-token"));
}

#[test]
fn grok_mcp_inference_accepts_headers_key() {
    let inferred = infer_toml_mcp_config(
        r#"[mcp_servers.sessionmunch]
url = "http://homelab:49374/mcp"
enabled = true

[mcp_servers.sessionmunch.headers]
Authorization = "Bearer secret-token"
"#,
    )
    .unwrap();

    assert_eq!(
        inferred.hook_server_url.as_deref(),
        Some("http://homelab:49374")
    );
    assert_eq!(inferred.auth_token.as_deref(), Some("secret-token"));
}

/// Regression for issue #53 — `install-hooks --agent codex` used to
/// panic with "index not found" when `~/.codex/config.toml` had an
/// `[mcp_servers]` table populated with *other* servers (context7,
/// node_repl, …) but no sessionmunch entry. A perfectly valid setup —
/// sessionmunch can live in Codex via hooks only without being an MCP
/// server — must return None, not abort the whole install.
#[test]
fn codex_mcp_inference_returns_none_when_sessionmunch_entry_missing() {
    let inferred = infer_toml_mcp_config(
        r#"[mcp_servers.context7]
url = "http://localhost:9000/mcp"

[mcp_servers.node_repl]
command = "npx"
args = ["node-repl"]
"#,
    );
    assert!(
        inferred.is_none(),
        "missing [mcp_servers.sessionmunch] must yield None, got {inferred:?}"
    );
}

/// Same regression class — no `[mcp_servers]` table at all means
/// the user is on a hooks-only / fresh config; we should return
/// None rather than panic on the first index.
#[test]
fn codex_mcp_inference_returns_none_when_no_mcp_servers_table() {
    let inferred = infer_toml_mcp_config(
        r#"# fresh codex config
model = "gpt-5"
"#,
    );
    assert!(inferred.is_none());
}

/// And the empty-file edge case the parser still accepts.
#[test]
fn codex_mcp_inference_returns_none_for_empty_doc() {
    assert!(infer_toml_mcp_config("").is_none());
}

/// An sessionmunch entry that exists but ships neither a `url` nor an
/// `Authorization` header still falls back to None (caller infers
/// from defaults). Distinguishes "config absent" from "config
/// present but unhelpful" — both yield None, neither panics.
#[test]
fn codex_mcp_inference_returns_none_for_bare_sessionmunch_entry() {
    let inferred = infer_toml_mcp_config(
        r#"[mcp_servers.sessionmunch]
# intentionally empty — no url, no headers.
"#,
    );
    assert!(inferred.is_none());
}

#[test]
fn bundled_posix_and_powershell_hooks_stay_in_parity() {
    let hooks_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("hooks");
    assert!(
        hooks_root
            .join("lib")
            .join("sessionmunch-hook.ps1")
            .is_file(),
        "PowerShell hooks require the shared lib helper"
    );

    // Enumerate the bundles instead of listing them: a hardcoded
    // list silently skips whatever agent lands next, which is how
    // `command-code` and `kiro-cli` shipped uncovered. `lib/` holds
    // the shared PowerShell helper, not an agent bundle.
    let mut agent_dirs: Vec<String> = fs::read_dir(&hooks_root)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", hooks_root.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .filter_map(|path| path.file_name()?.to_str().map(str::to_string))
        .filter(|name| name != "lib")
        .collect();
    agent_dirs.sort();
    assert!(
        !agent_dirs.is_empty(),
        "no agent hook bundles found under {}",
        hooks_root.display()
    );

    for agent_dir in agent_dirs {
        let agent_dir = agent_dir.as_str();
        let dir = hooks_root.join(agent_dir);
        let mut sh = BTreeMap::new();
        let mut ps1 = BTreeMap::new();
        for entry in fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("failed to read bundled hook dir {}: {e}", dir.display()))
        {
            let path = entry.unwrap().path();
            if !path.is_file() {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            match path.extension().and_then(|s| s.to_str()) {
                Some("sh") => {
                    sh.insert(stem.to_string(), extract_sh_hook_metadata(&path));
                }
                Some("ps1") => {
                    ps1.insert(stem.to_string(), extract_ps1_hook_metadata(&path));
                }
                _ => {}
            }
        }
        assert_eq!(
            sh.keys().collect::<Vec<_>>(),
            ps1.keys().collect::<Vec<_>>(),
            "{agent_dir}: every .sh hook must have a .ps1 peer"
        );
        for (stem, sh_meta) in sh {
            assert_eq!(
                Some(sh_meta),
                ps1.remove(&stem),
                "{agent_dir}/{stem}: .sh and .ps1 must post the same event/agent"
            );
        }
    }
}

fn extract_sh_hook_metadata(path: &Path) -> (String, String) {
    let text = fs::read_to_string(path).unwrap();
    let marker = "hook?event=";
    let start = text
        .find(marker)
        .unwrap_or_else(|| panic!("{} missing hook endpoint", path.display()))
        + marker.len();
    let rest = &text[start..];
    let event = rest
        .split('&')
        .next()
        .unwrap_or_else(|| panic!("{} missing event", path.display()))
        .to_string();
    let agent_marker = "&agent=";
    let agent_start = rest
        .find(agent_marker)
        .unwrap_or_else(|| panic!("{} missing agent", path.display()))
        + agent_marker.len();
    let agent = rest[agent_start..]
        .split(['"', '\'', ' ', '\n', '\r', '$'])
        .next()
        .unwrap_or_else(|| panic!("{} missing agent value", path.display()))
        .to_string();
    (event, agent)
}

#[test]
fn devin_bundle_has_no_subagent_scripts() {
    let hooks_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("hooks");
    let devin_dir = hooks_root.join("devin");

    for entry in fs::read_dir(&devin_dir).unwrap() {
        let path = entry.unwrap().path();
        if !path.is_file() {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        assert!(
            !stem.contains("subagent"),
            "Devin bundle should not contain subagent scripts, found: {}",
            path.display()
        );
    }
}

#[test]
fn devin_bundle_has_post_compaction_not_pre_compact() {
    let hooks_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("hooks");
    let devin_dir = hooks_root.join("devin");

    assert!(
        devin_dir.join("post-compaction.sh").is_file(),
        "Devin bundle must have post-compaction.sh"
    );
    assert!(
        devin_dir.join("post-compaction.ps1").is_file(),
        "Devin bundle must have post-compaction.ps1"
    );
    assert!(
        !devin_dir.join("pre-compact.sh").exists(),
        "Devin bundle should not have pre-compact.sh"
    );
    assert!(
        !devin_dir.join("pre-compact.ps1").exists(),
        "Devin bundle should not have pre-compact.ps1"
    );
}

fn extract_ps1_hook_metadata(path: &Path) -> (String, String) {
    let text = fs::read_to_string(path).unwrap();
    let line = text
        .lines()
        .find(|line| line.contains("Invoke-SessionMunchHook"))
        .unwrap_or_else(|| panic!("{} missing Invoke-SessionMunchHook", path.display()));
    (
        extract_ps1_arg(line, "Event", path),
        extract_ps1_arg(line, "Agent", path),
    )
}

fn extract_ps1_arg(line: &str, name: &str, path: &Path) -> String {
    let marker = format!("-{name} \"");
    let start = line
        .find(&marker)
        .unwrap_or_else(|| panic!("{} missing {name} argument", path.display()))
        + marker.len();
    line[start..]
        .split('"')
        .next()
        .unwrap_or_else(|| panic!("{} missing {name} value", path.display()))
        .to_string()
}

// ----------------------------------------------------------------
// Shared `_lib.sh` staging
// ----------------------------------------------------------------

/// `stage_hook_scripts` copies the parent dir's `_lib.sh` alongside
/// the agent's event scripts so the runtime layout doesn't depend
/// on the source-tree shape. This is the only piece of evidence we
/// have that the marker-file walk-up helper actually ships — the
/// scripts themselves source it with `. "$(dirname "$0")/_lib.sh"`
/// and a missing helper would surface as a runtime "command not
/// found" much further from the cause.
/// #581: inside the docker wrapper the server data dir is a volume
/// the host cannot execute from; staging must fall back to the
/// home-based path the wrapper bind-mounts and reads. Natively, the
/// resolved data dir keeps winning (#554/#573).
#[test]
fn staging_root_leaves_the_container_volume_for_home() {
    let data = Path::new("/data");
    assert_eq!(
        hook_staging_root(data, false),
        PathBuf::from("/data"),
        "native installs stage under the resolved data dir"
    );
    let in_container = hook_staging_root(data, true);
    assert_ne!(
        in_container,
        PathBuf::from("/data"),
        "container staging must not target the volume"
    );
    assert_eq!(
        in_container,
        dirs::data_local_dir().unwrap().join("sessionmunch"),
        "container staging targets the wrapper's bind-mounted home contract"
    );
}

#[test]
fn stage_hook_scripts_copies_shared_lib_sh() {
    // Distinct agent_label per test: `stage_hook_scripts` writes
    // under `dirs::data_local_dir()/.../hooks/<agent_label>` and
    // the test binary runs cases in parallel, so two tests using
    // the same label race on the same staging dir.
    let tmp = TempDir::new().unwrap();
    let bundle = tmp.path().join("hooks");
    let agent_src = bundle.join("stage-shared-lib");
    fs::create_dir_all(&agent_src).unwrap();
    fs::write(bundle.join("_lib.sh"), "# shared helper\n").unwrap();
    stub_scripts(&agent_src, &["session-start.sh", "post-tool-use.sh"]);

    let data_dir = tmp.path().join("data");
    let staged = stage_hook_scripts_in(&agent_src, "stage-shared-lib", &data_dir).unwrap();
    assert!(staged.join("session-start.sh").exists());
    assert!(staged.join("post-tool-use.sh").exists());
    assert!(
        staged.join("_lib.sh").exists(),
        "_lib.sh must be staged alongside event scripts",
    );

    let lib = fs::read_to_string(staged.join("_lib.sh")).unwrap();
    assert!(
        lib.contains("shared helper"),
        "staged _lib.sh must match the source-of-truth"
    );
}

/// Skipping `_lib.sh` is fine — older source bundles without the
/// marker-walk-up feature should still install cleanly.
#[test]
fn stage_hook_scripts_tolerates_missing_lib_sh() {
    let tmp = TempDir::new().unwrap();
    let bundle = tmp.path().join("hooks");
    let agent_src = bundle.join("stage-no-lib");
    fs::create_dir_all(&agent_src).unwrap();
    // Note: no _lib.sh in `bundle`.
    stub_scripts(&agent_src, &["session-start.sh"]);

    let data_dir = tmp.path().join("data");
    let staged = stage_hook_scripts_in(&agent_src, "stage-no-lib", &data_dir).unwrap();
    assert!(staged.join("session-start.sh").exists());
    assert!(!staged.join("_lib.sh").exists());
}

/// Regression for issue #52 — when `resolve_hooks_dir` picks the
/// data-local dir as the source bundle (the docker `setup-agent`
/// flow extracts scripts there) AND the staging destination is
/// the *same* dir, the pre-fix wipe-then-copy loop would delete
/// every populated script and report `staged 0`. The same-path
/// branch must verify in place without wiping, so existing scripts
/// survive a re-run.
#[test]
fn stage_hook_scripts_preserves_in_place_scripts_when_source_equals_dest() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let agent_label = "stage-in-place";
    // Simulate "scripts already extracted into the data dir's hooks
    // dir by a prior `setup-agent` run".
    let in_place = data_dir.join("hooks").join(agent_label);
    fs::create_dir_all(&in_place).unwrap();
    stub_scripts(&in_place, &["session-start.sh", "post-tool-use.sh"]);

    // Source == destination (this is what resolve_hooks_dir hands
    // us when no other candidate exists).
    let staged = stage_hook_scripts_in(&in_place, agent_label, &data_dir).unwrap();

    assert_eq!(staged, in_place, "destination must canonicalize to source");
    assert!(
        staged.join("session-start.sh").is_file(),
        "in-place script must survive the same-path branch (not be wiped)"
    );
    assert!(
        staged.join("post-tool-use.sh").is_file(),
        "in-place script must survive the same-path branch (not be wiped)"
    );
}

/// Regression for issue #52 — the failure that the reporter actually
/// hit: `resolve_hooks_dir` resolved to a pre-existing but empty
/// data-local dir, so source == dest and there's nothing to verify.
/// The pre-fix code silently returned Ok with `copied = 0` and the
/// caller went on to rewrite `settings.json` against an empty dir,
/// disabling capture without any error. We must bail with an
/// actionable message instead.
#[test]
fn stage_hook_scripts_bails_when_source_equals_empty_dest() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let agent_label = "stage-empty-in-place";
    // Must equal what `stage_hook_scripts_in` computes, or this stops
    // being the source==dest case it exists to cover and duplicates the
    // different-paths test below (#554 moved the destination shape).
    let in_place = data_dir.join("hooks").join(agent_label);
    fs::create_dir_all(&in_place).unwrap();
    // Intentionally no scripts in `in_place`.

    let err = stage_hook_scripts_in(&in_place, agent_label, &data_dir)
        .expect_err("an empty source dir must produce a hard error, not Ok(0)");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no hook scripts"),
        "error should call out the empty source: {msg}"
    );
    assert!(
        msg.contains("--hooks-dir") || msg.contains("setup-agent"),
        "error should point at the workaround (--hooks-dir or setup-agent): {msg}"
    );
}

/// Regression for issue #52 — same fail-on-zero guard applies even
/// when source and dest are different paths (e.g. user pointed
/// `--hooks-dir` at the wrong dir). Previously this also silently
/// returned Ok with `copied = 0`.
#[test]
fn stage_hook_scripts_bails_when_source_dir_is_empty() {
    let tmp = TempDir::new().unwrap();
    let bundle = tmp.path().join("hooks");
    let agent_src = bundle.join("stage-empty-src");
    fs::create_dir_all(&agent_src).unwrap();
    // Source dir exists but has no scripts.

    let data_dir = tmp.path().join("data");
    let err = stage_hook_scripts_in(&agent_src, "stage-empty-src", &data_dir)
        .expect_err("zero scripts should be an error, not a silent success");
    assert!(format!("{err:#}").contains("no hook scripts"));
}

/// #554: with `SESSIONMUNCH_DATA_DIR` set, `install-hooks` probed and staged
/// under the *platform default* instead of the data dir in use — so the
/// server logged one path while `--apply` reported staging into another,
/// silently populating a directory the operator was not using.
#[test]
fn a_configured_data_dir_is_where_the_bundle_is_probed_and_staged() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("custom-data");
    let source = tmp.path().join("bundle");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("session-start.sh"), b"#!/bin/sh\n").unwrap();

    // Staged into the configured dir…
    let staged = stage_hook_scripts_in(&source, "claude-code", &data_dir).unwrap();
    assert_eq!(staged, data_dir.join("hooks").join("claude-code"));
    assert!(staged.join("session-start.sh").is_file());
    assert!(
        !tmp.path().join("sessionmunch").exists(),
        "nothing may be written under a platform-default path"
    );

    // …and the probe looks in the same place, so the two halves agree.
    let candidates = hook_source_candidates("claude-code", None, None, Some(data_dir.clone()));
    assert!(
        candidates.contains(&data_dir.join("hooks").join("claude-code")),
        "the configured data dir must be probed; got {candidates:?}"
    );
}

/// The default install must be byte-identical to the old behaviour:
/// `default_data_dir()` is `data_local_dir()/sessionmunch`, so
/// `<data_dir>/hooks/<agent>` is the same path the previous
/// `<data_local>/sessionmunch/hooks/<agent>` produced.
#[test]
fn the_default_data_dir_stages_to_the_same_path_as_before() {
    let tmp = TempDir::new().unwrap();
    let data_local = tmp.path().join(".local").join("share");
    let default_data_dir = data_local.join("sessionmunch");
    let source = tmp.path().join("bundle");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("session-start.sh"), b"#!/bin/sh\n").unwrap();

    let staged = stage_hook_scripts_in(&source, "claude-code", &default_data_dir).unwrap();
    assert_eq!(
        staged,
        data_local
            .join("sessionmunch")
            .join("hooks")
            .join("claude-code"),
        "existing installs must keep staging exactly where they did"
    );
}

/// #552, the regression guard: `--apply` must persist the bearer under the
/// data dir and leave it out of the agent config entirely.
///
/// Before this, the token was written into the rendered command — as
/// `--auth-token <token>` for native hooks — so it sat in the agent's
/// config file *and* in `/proc/<pid>/cmdline` for the lifetime of every
/// hook, on every tool call.
#[test]
fn apply_persists_the_bearer_and_keeps_it_out_of_the_agent_config() {
    let home = TempDir::new().unwrap();
    let cfg_dir = TempDir::new().unwrap();
    let settings = cfg_dir.path().join("settings.json");
    std::fs::write(&settings, "{}").unwrap();

    let config = crate::config::Config::load(None, Some(home.path().to_path_buf())).unwrap();
    let args = InstallHooksArgs {
        agent: AgentChoice::ClaudeCode,
        apply: true,
        server_url: Some("http://127.0.0.1:49374".to_string()),
        auth_token: Some("SEKRIT-BEARER-552".to_string()),
        config_file: Some(settings.clone()),
        // Point at the repo's bundle explicitly. Without this the test
        // leans on hook-dir discovery, which from `target/debug/deps`
        // walks to `<repo>/target` and then falls through to whatever the
        // machine happens to have staged — passing for a reason that has
        // nothing to do with what is under test.
        hooks_dir: Some(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks")),
        ..default_hook_args()
    };

    run(&config, args).expect("apply should succeed");

    let rendered = std::fs::read_to_string(&settings).unwrap();
    assert!(
        !rendered.contains("SEKRIT-BEARER-552"),
        "the bearer must not be written into the agent config: {rendered}"
    );
    assert!(
        !rendered.contains("--auth-token"),
        "no --auth-token flag may reach a hook command line: {rendered}"
    );

    assert_eq!(
        crate::config::read_hook_auth_token(&config.data_dir).as_deref(),
        Some("SEKRIT-BEARER-552"),
        "the bearer must be persisted where the hook can read it"
    );
    assert!(
        std::fs::read_to_string(crate::config::hook_auth_header_path_in(&config.data_dir))
            .unwrap()
            .contains("SEKRIT-BEARER-552"),
        "the curl header file must carry it too"
    );
}

#[test]
fn hook_source_candidates_include_native_package_dir() {
    let candidates = hook_source_candidates(
        "claude-code",
        Some(PathBuf::from("/repo")),
        Some(PathBuf::from("/opt/sessionmunch")),
        // The resolved data dir, not the platform data-local root (#554).
        Some(PathBuf::from("/home/alice/.local/share/sessionmunch")),
    );

    assert_eq!(candidates[0], PathBuf::from("/repo/hooks/claude-code"));
    assert_eq!(
        candidates[1],
        PathBuf::from("/opt/sessionmunch/hooks/claude-code")
    );
    assert_eq!(
        candidates[2],
        PathBuf::from("/usr/local/share/sessionmunch/hooks/claude-code")
    );
    assert_eq!(
        candidates[3],
        PathBuf::from("/usr/share/sessionmunch/hooks/claude-code")
    );
    assert_eq!(
        candidates[4],
        PathBuf::from("/home/alice/.local/share/sessionmunch/hooks/claude-code")
    );
}

/// #546: `~/.local/bin/sessionmunch -> <repo>/target/release/sessionmunch` is
/// what the quick start's `~/.local/bin` layout produces for a source
/// build. Discovery is derived from the binary's own location, so the
/// link has to be resolved first or every candidate misses the checkout.
#[cfg(unix)]
#[test]
fn hooks_are_found_through_a_symlinked_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let exe = repo.join("target").join("release").join("sessionmunch");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::write(&exe, b"").unwrap();
    let link_dir = tmp.path().join("home").join(".local").join("bin");
    fs::create_dir_all(&link_dir).unwrap();
    let link = link_dir.join("sessionmunch");
    std::os::unix::fs::symlink(&exe, &link).unwrap();

    let resolved = resolve_exe_path(link);
    let candidates = hook_source_candidates(
        "claude-code",
        repo_root_from_exe(&resolved),
        resolved.parent().map(Path::to_path_buf),
        None,
    );

    assert_eq!(
        candidates[0],
        fs::canonicalize(&repo)
            .unwrap()
            .join("hooks")
            .join("claude-code"),
        "the checkout must be the first candidate, not the symlink's ancestor"
    );
}

#[test]
fn resolve_exe_path_keeps_an_unresolvable_path() {
    let missing = PathBuf::from("/definitely/not/here/sessionmunch");
    assert_eq!(resolve_exe_path(missing.clone()), missing);
}

#[test]
fn hook_source_candidates_include_binary_sibling_for_flat_tarball() {
    // Extracted release tarball: no repo root, `hooks/` beside the binary
    // (issue #107). The sibling dir must be probed or discovery fails with
    // a bogus `/private/hooks/...` on macOS.
    let candidates = hook_source_candidates(
        "claude-code",
        None,
        Some(PathBuf::from("/private/tmp/sessionmunch-macos-aarch64")),
        None,
    );
    assert!(
        candidates.contains(&PathBuf::from(
            "/private/tmp/sessionmunch-macos-aarch64/hooks/claude-code"
        )),
        "binary-sibling hooks/ dir must be probed; got {candidates:?}"
    );
}

// ----------------------------------------------------------------
// OpenCode tests
// ----------------------------------------------------------------

fn assert_generated_ts_uses_bounded_hook_queue(generated: &str) {
    assert!(generated.contains("const HOOK_QUEUE_MAX = 100;"));
    assert!(generated.contains("const HOOK_FLUSH_INTERVAL_MS = 2000;"));
    assert!(generated.contains("const HOOK_FLUSH_THRESHOLD = 20;"));
    assert!(generated.contains("const HOOK_INTER_REQUEST_DELAY_MS = 50;"));
    assert!(generated.contains("const HOOK_REQUEST_TIMEOUT_MS = 2000;"));
    assert!(generated.contains("const HOOK_IMMEDIATE_EVENTS = new Set([\"session-start\", \"stop\", \"session-end\", \"pre-compact\"]);"));
    assert!(generated.contains("const hookQueue: HookQueueItem[] = [];"));
    assert!(generated.contains(
        "function enqueueHook(event: string, url: URL, payload: Record<string, unknown>): void"
    ));
    assert!(generated.contains("if (hookQueue.length >= HOOK_QUEUE_MAX) hookQueue.shift();"));
    assert!(
        generated.contains(
            "HOOK_IMMEDIATE_EVENTS.has(event) || hookQueue.length >= HOOK_FLUSH_THRESHOLD"
        )
    );
    assert!(generated.contains("function scheduleHookFlush(): void"));
    assert!(generated.contains("hookFlushTimer.unref?.();"));
    assert!(generated.contains("async function drainHookQueue(): Promise<void>"));
    assert!(generated.contains("signal: timeoutSignal(HOOK_REQUEST_TIMEOUT_MS)"));
    assert!(generated.contains("await sleep(HOOK_INTER_REQUEST_DELAY_MS)"));
    assert!(generated.contains("const policy = capturePolicy(payload"));
    assert!(generated.contains("if (policy.disposition === \"drop\") return;"));
    assert!(generated.contains("enqueueHook(event, url, policy.payload);"));
    assert!(generated.contains("const CAPTURE_POLICY_V1 = 1;"));
    assert!(generated.contains("const CAPTURE_MARKER_MAX_BYTES = 64 * 1024;"));
    assert!(generated.contains("async function fetchHandoff"));
    assert!(generated.contains("if (!process.env.SESSIONMUNCH_RUN_ID) {"));
    assert!(generated.contains("const response = await fetch(url, {"));
    assert!(generated.contains("signal: timeoutSignal(1000)"));
    assert!(!generated.contains("signal: timeoutSignal(500)"));
    assert!(!generated.contains("void fetch(url, {"));
}

#[test]
fn generated_ts_integrations_spool_failed_deliveries() {
    // #580: every queue-based TS integration must persist a failed
    // delivery instead of dropping it, and must self-drain the spool.
    for (name, source) in [
        (
            "opencode",
            build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist"),
        ),
        (
            "omp",
            build_omp_extension("http://127.0.0.1:49374", Some("tok"), None, "denylist"),
        ),
        (
            "pi",
            build_pi_extension("http://127.0.0.1:49374", Some("tok"), None, "denylist"),
        ),
    ] {
        // The fire-and-forget delivery must be gone...
        assert!(
            !source.contains("}).catch(() => undefined);\n      } catch (_e) {"),
            "{name}: still fire-and-forgets failed deliveries"
        );
        // ...replaced by capture + spool of network failures and 5xx.
        assert!(
            source.contains(
                "if (!resp || resp.status >= 500) spoolFailedHook(item.url, item.payload);"
            ),
            "{name}: missing spool-on-failure"
        );
        // The spool runtime is present and CLI-compatible.
        assert!(source.contains("function spoolFailedHook("), "{name}");
        assert!(source.contains("async function drainHookSpool()"), "{name}");
        assert!(
            source.contains(r#"return join(env, "hook-spool");"#),
            "{name}: spool dir must honour SESSIONMUNCH_DATA_DIR"
        );
        // Every queue flush also kicks a spool drain.
        assert!(
            source.contains("if (hookDraining) return;\n  requestSpoolDrain();"),
            "{name}: drainHookQueue must trigger the spool drain"
        );
        // The runtime's fs needs made it into the import line.
        for f in [
            "mkdirSync",
            "writeFileSync",
            "renameSync",
            "readdirSync",
            "unlinkSync",
        ] {
            assert!(
                source.contains(&format!("{f},")) || source.contains(&format!("{f} }}")),
                "{name}: node:fs import missing {f}"
            );
        }
    }
}

#[test]
fn ts_spool_entries_are_readable_by_the_cli_drain() {
    // The TS runtime writes the CLI hook-spool's exact on-disk contract.
    // Mirror the bytes the generated code produces and prove the CLI
    // side parses them: entry JSON deserializes, the filename's
    // timestamp is honoured, and the body survives round-tripping.
    let created_ms: u64 = 1_756_800_000_000;
    // Filename exactly as the TS builds it:
    // `${String(createdMs).padStart(13, "0")}-${process.pid}-${seq}.json`
    let name = format!("{created_ms:013}-4242-{:016x}.json", 7u64);
    // Entry JSON exactly as the TS `JSON.stringify` emits it (key order
    // irrelevant to serde, but the shapes and enum strings are load-bearing).
    let json = format!(
        r#"{{"url":"http://127.0.0.1:49374/hook?event=stop&agent=open-code&ingest_key=ts18f3","body":"{{\"session_id\":\"s1\"}}","created_ms":{created_ms},"auth_mode":"static","token":"tok","attempts":0}}"#
    );
    let entry: crate::commands::hook_spool::SpoolEntry =
        serde_json::from_str(&json).expect("CLI drain must parse a TS-written entry");
    assert_eq!(entry.created_ms, created_ms);
    assert_eq!(entry.token.as_deref(), Some("tok"));
    assert_eq!(entry.attempts, 0);
    assert!(entry.url.contains("ingest_key=ts18f3"));
    serde_json::from_str::<serde_json::Value>(&entry.body)
        .expect("TS body must be raw JSON, not double-encoded");

    // And the anonymous variant (no token installed).
    let anon = r#"{"url":"http://h/hook?event=stop&agent=omp","body":"{}","created_ms":1,"auth_mode":"none","attempts":0}"#;
    serde_json::from_str::<crate::commands::hook_spool::SpoolEntry>(anon)
        .expect("auth_mode none must parse");

    // The timestamp-bearing filename is one the spool reader accepts:
    // spool_health derives oldest-age from names without reading files.
    let dir = tempfile::tempdir().expect("tempdir");
    let spool = dir.path().join("hook-spool");
    std::fs::create_dir_all(&spool).expect("mkdir");
    std::fs::write(spool.join(&name), json.as_bytes()).expect("write");
    assert_eq!(crate::commands::hook_spool::spool_len(&spool), 1);
    let health = crate::commands::hook_spool::spool_health(&spool);
    assert_eq!(health.pending, 1);
    assert!(
        health.oldest_age_ms.is_some(),
        "TS filename's 13-digit timestamp must be readable: {name}"
    );
}

#[test]
fn opencode_plugin_uses_real_plugin_hooks() {
    let plugin = build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist");

    assert!(plugin.contains("event: async (input)"));
    assert!(plugin.contains(r#""chat.message": async"#));
    assert!(plugin.contains(r#""tool.execute.before": async"#));
    assert!(plugin.contains(r#""tool.execute.after": async"#));
    assert!(plugin.contains(r#""experimental.chat.system.transform": async"#));
    assert!(plugin.contains("export default SessionMunchHooks"));
    assert!(plugin.contains("const startedSessions = new Set<string>();"));
    assert!(
        plugin.contains("const handoffFetches = new Map<string, Promise<string | undefined>>();")
    );
    assert!(!plugin.contains("handoffChecked"));
    assert!(plugin.contains("function startSession"));
    assert!(plugin.contains("function endSession"));
    assert!(plugin.contains("fetchHandoff"));
    assert!(plugin.contains("function applyMarkerParams"));
    assert!(plugin.contains("readFileSync(marker, \"utf8\")"));
    assert!(plugin.contains("text.split(/\\r?\\n/)"));
    assert!(plugin.contains("tomlKey(body, \"project_strategy\")"));
    assert!(plugin.contains("tomlKey(body, \"drop_subagent_captures\")"));
    assert!(plugin.contains("url.searchParams.set(\"project_strategy\", projectStrategy)"));
    assert!(plugin.contains("url.searchParams.set(\"drop_subagent\", dropSubagent)"));
    assert!(plugin.contains("function tomlFlag"));
    assert!(plugin.contains("tomlFlag(body, \"default_global\")"));
    assert!(plugin.contains("tomlFlag(body, \"inject_on_session_start\")"));
    assert!(plugin.contains("url.searchParams.set(\"briefing_budget\", briefingBudget)"));
    // #668: applyMarkerParams resolves scope/settings via the
    // settings-walk, not the nearest-marker findMarker, so a nested
    // capture-only marker does not shadow an outer marker's scope.
    assert!(plugin.contains("function findSettingsMarker"));
    assert!(plugin.contains("function declaresSettings"));
    assert!(plugin.contains("const marker = findSettingsMarker(cwd);"));
    assert!(plugin.contains(
            "for (const key of [\"workspace\", \"project\", \"project_strategy\", \"drop_subagent_captures\"])"
        ));
    assert!(plugin.contains(
        "for (const key of [\"default_global\", \"inject_on_session_start\", \"max_chars\"])"
    ));
    assert!(
        plugin.contains("if (declaresSettings(readFileSync(marker, \"utf8\"))) return marker;")
    );
    assert!(plugin.contains(
        "applyMarkerParams(url, typeof payload.cwd === \"string\" ? payload.cwd : undefined);"
    ));
    assert!(plugin.contains("applyMarkerParams(url, cwd);"));
    assert!(plugin.contains("postPreCompact"));
    assert!(plugin.contains("dispose: async () =>"));
    assert!(plugin.contains("const HOOK_DISPOSE_DRAIN_BUDGET_MS = 2000;"));
    assert!(plugin.contains("let hookDrainPromise: Promise<void> | undefined;"));
    assert!(plugin.contains("function requestHookDrain(): Promise<void>"));
    assert!(plugin.contains("function disposeDrainTimeout(): Promise<void>"));
    assert!(plugin.contains("timer.unref?.();"));
    assert!(plugin.contains("async function drainHookQueueForDispose(): Promise<void>"));
    assert!(plugin.contains("for (const id of Array.from(startedSessions))"));
    assert!(plugin.contains("await drainHookQueueForDispose();"));
    assert!(plugin.contains("postHook(\"session-start\""));
    assert!(plugin.contains(r#""session.deleted")"#));
    assert_eq!(
        plugin.matches("postHook(\"session-end\"").count(),
        1,
        "OpenCode generated plugin must route session closes through one idempotent helper"
    );
    assert!(plugin.contains("!startedSessions.delete(id)"));
    assert!(plugin.contains("sessionCwds.delete(id);"));
    assert!(plugin.contains("handoffFetches.delete(id);"));
    assert!(plugin.contains("preCompactLast.delete(id);"));
    assert!(plugin.contains("postHook(\"user-prompt\""));
    assert!(plugin.contains("Bearer ${token}"));
    assert!(plugin.contains("tok"));
    assert!(
        !plugin.contains(r#""session.created": async"#),
        "OpenCode bus events must be handled through the `event` hook"
    );
    assert!(plugin.contains("import { execFileSync } from \"node:child_process\";"));
    assert!(
        plugin.contains("import { basename, dirname, join, resolve, sep } from \"node:path\";")
    );
    assert!(plugin.contains("if (existsSync(join(probe, \".git\")))"));
    assert!(plugin.contains("boundary ??= dir;"));
    assert!(plugin.contains("function repoRootProject"));
    assert!(plugin.contains("--git-common-dir"));
    assert!(
        plugin.contains("projectStrategy === \"repo-root\" || projectStrategy === \"repo_root\"")
    );
    assert!(plugin.contains("url.searchParams.set(\"project\", repoProject)"));
}

#[test]
fn opencode2_plugin_binds_the_v2_api() {
    let plugin =
        build_opencode2_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist").unwrap();

    // Ownership markers the uninstall gate keys on.
    assert!(plugin.contains("install-hooks --agent opencode2 --apply"));
    assert!(plugin.contains("const AGENT = \"opencode2\";"));
    // Type-only import: `Plugin.define` is identity, and a runtime
    // import does not resolve from the global plugins dir (the beta
    // refuses the load). The host only needs the `{ id, setup }` shape.
    assert!(plugin.contains("import type { Plugin } from \"@opencode-ai/plugin\";"));
    assert!(!plugin.contains("export default Plugin.define"));
    assert!(!plugin.contains("Plugin.define({"));
    assert!(plugin.contains("const SessionMunchOpencode2: Plugin = {"));
    assert!(plugin.contains("export default SessionMunchOpencode2;"));
    // Beta hooks, verified against `@opencode-ai/plugin@beta`.
    assert!(plugin.contains("ctx.event.subscribe"));
    assert!(plugin.contains("ctx.session.hook(\"prompt\""));
    assert!(plugin.contains("ctx.session.hook(\"context\""));
    assert!(plugin.contains("ctx.tool.hook(\"execute.before\""));
    assert!(plugin.contains("ctx.tool.hook(\"execute.after\""));
    // V2 lifecycle envelopes carry `{ type, data, location }`.
    for event in [
        "session.created",
        "session.idle",
        "session.deleted",
        "session.compacted",
    ] {
        assert!(plugin.contains(event), "missing {event}");
    }
    // Handoff injection moved off v1's removed experimental hook and
    // fires once per session (the context hook runs per model call).
    // System items are `{ type: "text", text }` objects on the V2 API, not strings.
    assert!(plugin.contains("handoffInjected"));
    assert!(plugin.contains("system.push({ type: \"text\", text: handoff })"));
    assert!(!plugin.contains("experimental."));
    // Hook registrations are disposed on unload so a reload cannot
    // leave stale callbacks capturing twice.
    assert!(plugin.contains("registration.dispose()"));
    // Pre-compaction arrives on its own start event; the completion
    // event stays as the consolidation trigger, like v1.
    assert!(plugin.contains("session.compaction.started"));
    // Tool failures share the channel with the reason preserved.
    assert!(plugin.contains("status === \"error\""));
    // No v1 remnants.
    assert!(!plugin.contains("export default SessionMunchHooks"));
    assert!(!plugin.contains("chat.message"));
    // Shared capture prelude survived the rewrite byte-identical.
    for shared in [
        "function startSession",
        "function endSession",
        "postHook(\"session-start\"",
        "postHook(\"user-prompt\"",
        "function applyMarkerParams",
        "requestSpoolDrain();",
    ] {
        assert!(plugin.contains(shared), "missing {shared}");
    }
    // #625 made the generated adapters resolve the bearer at runtime
    // (resolveToken() -> `${token}`) instead of the static `${TOKEN}`;
    // the opencode2 plugin derives from v1 so it inherits that.
    assert!(plugin.contains("Bearer ${token}"));
    assert!(plugin.contains("tok"));
}

#[test]
fn opencode2_plugin_rejects_v1_template_drift() {
    // The builder rewrites the v1 template, so a v1 edit that moves the
    // binding anchor must fail loudly instead of shipping a hybrid.
    let plugin = build_opencode2_plugin(
        "http://127.0.0.1:49374/",
        None,
        Some("repo-root"),
        "denylist",
    )
    .unwrap();
    assert!(plugin.contains("const TOKEN: string | null = null;"));
    assert!(
        plugin.contains("const DEFAULT_PROJECT_STRATEGY = \"repo-root\";"),
        "strategy bake-through must survive the v2 rewrite"
    );
}

#[test]
fn opencode2_anchor_rewrite_fails_loudly_on_drift() {
    let err = replace_opencode_anchor("no v1 anchors here", "missing", "x", "banner").unwrap_err();
    assert!(
        err.to_string().contains("banner"),
        "drift must name the moved anchor: {err:#}"
    );
}

#[test]
fn opencode2_plugin_bakes_allowlist_admit_gate() {
    // opencode2 reuses v1's capture prelude verbatim (see
    // `build_opencode2_plugin`'s doc comment), so the gate must survive
    // the anchor rewrite into the beta's `{ id, setup }` host binding.
    let plugin =
        build_opencode2_plugin("http://127.0.0.1:49374", Some("tok"), None, "allowlist").unwrap();
    assert!(
        plugin.contains("const CAPTURE_MODE: \"allowlist\" | \"denylist\" = \"allowlist\";"),
        "{plugin}"
    );
    assert!(plugin.contains(CAPTURE_ADMIT_GATE_TS), "{plugin}");
}

#[test]
fn opencode2_plugin_denylist_bakes_inert_gate() {
    let plugin =
        build_opencode2_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist").unwrap();
    assert!(
        plugin.contains("const CAPTURE_MODE: \"allowlist\" | \"denylist\" = \"denylist\";"),
        "{plugin}"
    );
    assert!(plugin.contains(CAPTURE_ADMIT_GATE_TS), "{plugin}");
}

#[test]
fn opencode_plugin_normalizes_payloads_without_legacy_wrapper() {
    let plugin = build_opencode_plugin("http://127.0.0.1:49374/", None, None, "denylist");

    assert!(plugin.contains("const SERVER = \"http://127.0.0.1:49374/\".replace"));
    assert!(plugin.contains("const TOKEN: string | null = null;"));
    assert!(plugin.contains("sessionID: id,"));
    assert!(plugin.contains("cwd,"));
    assert!(plugin.contains("prompt: textFromParts"));
    assert!(plugin.contains("output: (output as any).output"));
    assert!(plugin.contains("if (typeof AbortSignal === \"undefined\")"));
    assert!(
        !plugin.contains("hook_event_name"),
        "new plugin should send normalized top-level fields, not legacy wrappers"
    );
}

#[test]
fn opencode_plugin_bakes_repo_root_default() {
    let plugin = build_opencode_plugin(
        "http://127.0.0.1:49374",
        Some("tok"),
        Some("repo-root"),
        "denylist",
    );
    assert!(
        plugin.contains("const DEFAULT_PROJECT_STRATEGY = \"repo-root\";"),
        "repo-root install default must bake the const: {plugin}"
    );
    assert!(
        plugin.contains("if (!projectStrategy) projectStrategy = DEFAULT_PROJECT_STRATEGY;"),
        "must apply the default when a marker pins no strategy: {plugin}"
    );
    assert!(
        plugin.contains("if (repoProject) project = repoProject;"),
        "{plugin}"
    );
    assert!(
        plugin.contains("const marker = findSettingsMarker(cwd);"),
        "the default-strategy variant must also walk past a capture-only marker (#668): {plugin}"
    );
}

#[test]
fn opencode_plugin_default_omits_baked_strategy() {
    let plugin = build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist");
    assert!(
        !plugin.contains("DEFAULT_PROJECT_STRATEGY"),
        "basename default must bake no strategy: {plugin}"
    );
}

#[test]
fn opencode_plugin_bakes_allowlist_admit_gate() {
    let plugin = build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), None, "allowlist");
    assert!(
        plugin.contains("const CAPTURE_MODE: \"allowlist\" | \"denylist\" = \"allowlist\";"),
        "{plugin}"
    );
    assert!(plugin.contains(CAPTURE_ADMIT_GATE_TS), "{plugin}");
}

#[test]
fn opencode_plugin_denylist_bakes_inert_gate() {
    let plugin = build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist");
    assert!(
        plugin.contains("const CAPTURE_MODE: \"allowlist\" | \"denylist\" = \"denylist\";"),
        "{plugin}"
    );
    assert!(plugin.contains(CAPTURE_ADMIT_GATE_TS), "{plugin}");
}

#[test]
fn opencode_plugin_uses_bounded_hook_queue() {
    let plugin = build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist");

    assert_generated_ts_uses_bounded_hook_queue(&plugin);
}

#[test]
fn opencode_plugin_resolves_token_at_runtime_when_not_embedded() {
    let plugin = build_opencode_plugin("http://127.0.0.1:49374", None, None, "denylist");
    assert!(plugin.contains("function resolveToken("));
    assert!(plugin.contains("const token = resolveToken();"));
    assert!(plugin.contains("if (!response.ok) return undefined;"));
}

#[test]
fn omp_extension_resolves_token_at_runtime_when_not_embedded() {
    let extension = build_omp_extension("http://127.0.0.1:49374", None, None, "denylist");
    assert!(extension.contains("function resolveToken("));
    assert!(extension.contains("process.env.SESSIONMUNCH_AUTH_TOKEN"));
    assert!(extension.contains("auth-token"));
    assert!(extension.contains("if (!response.ok) return undefined;"));
}

#[test]
fn omp_extension_prefers_statically_embedded_token() {
    let extension = build_omp_extension("http://127.0.0.1:49374", Some("tok"), None, "denylist");
    assert!(extension.contains("function resolveToken("));
    assert!(extension.contains("if (TOKEN) return TOKEN;"));
}

// ----------------------------------------------------------------
// OMP tests
// ----------------------------------------------------------------

#[test]
fn omp_extension_bakes_allowlist_admit_gate() {
    let extension = build_omp_extension("http://127.0.0.1:49374", Some("tok"), None, "allowlist");
    assert!(
        extension.contains("const CAPTURE_MODE: \"allowlist\" | \"denylist\" = \"allowlist\";"),
        "{extension}"
    );
    assert!(extension.contains(CAPTURE_ADMIT_GATE_TS), "{extension}");
}

#[test]
fn omp_extension_denylist_bakes_inert_gate() {
    let extension = build_omp_extension("http://127.0.0.1:49374", Some("tok"), None, "denylist");
    assert!(
        extension.contains("const CAPTURE_MODE: \"allowlist\" | \"denylist\" = \"denylist\";"),
        "{extension}"
    );
    assert!(extension.contains(CAPTURE_ADMIT_GATE_TS), "{extension}");
}

#[test]
fn omp_extension_uses_native_lifecycle_events() {
    let extension = build_omp_extension("http://127.0.0.1:49374", Some("tok"), None, "denylist");

    assert!(extension.contains("export default function SessionMunchExtension"));
    assert!(extension.contains("const AGENT = \"omp\";"));
    assert!(extension.contains("api.on(\"session_start\""));
    assert!(extension.contains("api.on(\"before_agent_start\""));
    assert!(extension.contains("api.on(\"tool_call\""));
    assert!(extension.contains("api.on(\"tool_result\""));
    assert!(extension.contains("api.on(\"session_shutdown\""));
    assert!(extension.contains("postHook(\"session-start\""));
    assert!(extension.contains("postHook(\"user-prompt\""));
    assert!(extension.contains("fetchHandoff"));
    assert!(extension.contains("if (!process.env.SESSIONMUNCH_RUN_ID) {"));
    assert!(extension.contains("function applyMarkerParams"));
    assert!(extension.contains("readFileSync(marker, \"utf8\")"));
    assert!(extension.contains("text.split(/\\r?\\n/)"));
    assert!(extension.contains("tomlKey(body, \"project_strategy\")"));
    assert!(extension.contains("tomlKey(body, \"drop_subagent_captures\")"));
    assert!(extension.contains("url.searchParams.set(\"project_strategy\", projectStrategy)"));
    assert!(extension.contains("url.searchParams.set(\"drop_subagent\", dropSubagent)"));
    assert!(extension.contains("function tomlFlag"));
    assert!(extension.contains("tomlFlag(body, \"default_global\")"));
    assert!(extension.contains("tomlFlag(body, \"inject_on_session_start\")"));
    assert!(extension.contains("url.searchParams.set(\"briefing_budget\", briefingBudget)"));
    // #668: same settings-walk as the OpenCode plugin (shared
    // `ts_apply_marker_params`), so a nested capture-only marker does
    // not shadow an outer marker's scope for the OMP/pi extensions.
    assert!(extension.contains("function findSettingsMarker"));
    assert!(extension.contains("function declaresSettings"));
    assert!(extension.contains("const marker = findSettingsMarker(cwd);"));
    assert!(
        extension.contains("if (declaresSettings(readFileSync(marker, \"utf8\"))) return marker;")
    );
    assert!(extension.contains(
        "applyMarkerParams(url, typeof payload.cwd === \"string\" ? payload.cwd : undefined);"
    ));
    assert!(extension.contains("applyMarkerParams(url, cwd);"));
    assert!(extension.contains("Bearer ${token}"));
    assert!(extension.contains("tok"));
    assert!(
        extension.contains("import { basename, dirname, join, resolve, sep } from \"node:path\";")
    );
    assert!(extension.contains("if (existsSync(join(probe, \".git\")))"));
    assert!(extension.contains("boundary ??= dir;"));
    assert!(extension.contains("import { execFileSync } from \"node:child_process\";"));
    assert!(extension.contains("function repoRootProject"));
    assert!(extension.contains("--git-common-dir"));
    assert!(
        extension
            .contains("projectStrategy === \"repo-root\" || projectStrategy === \"repo_root\"")
    );
    assert!(extension.contains("url.searchParams.set(\"project\", repoProject)"));
    // #676: pi/omp await session_shutdown's dispose flush instead of
    // returning immediately, so the runtime teardown that follows a sync
    // handler no longer kills the in-flight session-end fetch.
    assert!(extension.contains("const HOOK_DISPOSE_DRAIN_BUDGET_MS = 2000;"));
    assert!(extension.contains("let hookDrainPromise: Promise<void> | undefined;"));
    assert!(extension.contains("function requestHookDrain(): Promise<void>"));
    assert!(extension.contains("function disposeDrainTimeout(): Promise<void>"));
    assert!(extension.contains("async function drainHookQueueForDispose(): Promise<void>"));
    assert!(extension.contains("api.on(\"session_shutdown\", async (_event: any, ctx: any) => {"));
    assert!(extension.contains("await drainHookQueueForDispose();"));
    assert!(
        !extension.contains("api.on(\"session_shutdown\", (_event: any, ctx: any) => {"),
        "session_shutdown must not regress to the sync fire-and-forget form: {extension}"
    );
}

#[test]
fn omp_extension_bakes_repo_root_default() {
    let extension = build_omp_extension(
        "http://127.0.0.1:49374",
        Some("tok"),
        Some("repo-root"),
        "denylist",
    );
    assert!(
        extension.contains("const DEFAULT_PROJECT_STRATEGY = \"repo-root\";"),
        "repo-root install default must bake the const: {extension}"
    );
    assert!(
        extension.contains("if (!projectStrategy) projectStrategy = DEFAULT_PROJECT_STRATEGY;"),
        "{extension}"
    );
    assert!(
        extension.contains("const marker = findSettingsMarker(cwd);"),
        "the default-strategy variant must also walk past a capture-only marker (#668): {extension}"
    );
}

#[test]
fn omp_extension_default_omits_baked_strategy() {
    let extension = build_omp_extension("http://127.0.0.1:49374", Some("tok"), None, "denylist");
    assert!(
        !extension.contains("DEFAULT_PROJECT_STRATEGY"),
        "{extension}"
    );
}

#[test]
fn omp_extension_uses_bounded_hook_queue() {
    let extension = build_omp_extension("http://127.0.0.1:49374", Some("tok"), None, "denylist");

    assert_generated_ts_uses_bounded_hook_queue(&extension);
}

/// The manual (non-`--apply`) instructions must name the same path
/// `--apply` would write to. Printing the `~/.pi/agent` default to an
/// operator who set `PI_CODING_AGENT_DIR` would tell them to hand-copy the
/// extension exactly where the agent never loads it — reintroducing, in
/// the manual path, the silent no-capture this change fixes.
#[test]
fn extension_hints_follow_pi_coding_agent_dir() {
    let custom = if cfg!(windows) {
        r"C:\custom\pi-agent"
    } else {
        "/custom/pi-agent"
    };
    let env = || Some(std::ffi::OsString::from(custom));

    for hint in [
        omp_extension_hint_in(env(), None),
        pi_extension_hint_in(env()),
    ] {
        assert!(
            hint.contains("custom")
                && (hint.ends_with("sessionmunch-omp.ts") || hint.ends_with("sessionmunch-pi.ts")),
            "hint must point at the relocated agent home, got: {hint}"
        );
        assert!(
            !hint.contains(".pi/agent") && !hint.contains(".omp/agent"),
            "hint must not advertise the default home when the var is set: {hint}"
        );
    }

    // Unset: the hints keep naming the documented defaults. Build the
    // expected tail from path components rather than hardcoding `/` —
    // `Path::display` renders `\` on Windows, so a slash-literal suffix
    // asserts a path shape that platform never produces.
    for (agent_home, filename, hint) in [
        (
            ".omp",
            "sessionmunch-omp.ts",
            omp_extension_hint_in(None, None),
        ),
        (".pi", "sessionmunch-pi.ts", pi_extension_hint_in(None)),
    ] {
        let tail: PathBuf = [agent_home, "agent", "extensions", filename]
            .iter()
            .collect();
        assert!(
            hint.ends_with(&tail.display().to_string()),
            "unset must keep the documented default, got: {hint}"
        );
    }
}

/// Precedence must not depend on what the override directory is *named*.
/// An earlier revision re-rooted only when the last component happened to
/// be "agent", so `/custom/agent` and `/custom/pi-agent` behaved
/// differently for the same inputs.
#[test]
fn omp_profile_precedence_is_independent_of_the_override_dir_name() {
    let a = omp_extension_path_in(
        Some(std::ffi::OsString::from("/custom/agent")),
        Some("work"),
    )
    .unwrap();
    let b = omp_extension_path_in(
        Some(std::ffi::OsString::from("/custom/pi-agent")),
        Some("work"),
    )
    .unwrap();

    // The override names the agent dir in both cases; the profile does
    // not re-root it, and neither result invents a `profiles/` segment.
    assert!(
        a.ends_with("agent/extensions/sessionmunch-omp.ts"),
        "got {a:?}"
    );
    assert!(
        b.ends_with("pi-agent/extensions/sessionmunch-omp.ts"),
        "got {b:?}"
    );
    for p in [&a, &b] {
        assert!(
            !p.to_string_lossy().contains("profiles"),
            "PI_CODING_AGENT_DIR names the agent dir outright: {p:?}"
        );
    }
}

/// With no override, `--profile` relocates to the documented
/// `~/.omp/profiles/<name>/agent/extensions`.
#[test]
fn omp_profile_relocates_when_no_override_is_set() {
    let path = omp_extension_path_in(None, Some("work")).unwrap();
    assert!(
        path.ends_with(".omp/profiles/work/agent/extensions/sessionmunch-omp.ts"),
        "got {path:?}"
    );

    let plain = omp_extension_path_in(None, None).unwrap();
    assert!(
        plain.ends_with(".omp/agent/extensions/sessionmunch-omp.ts"),
        "got {plain:?}"
    );
}

/// `remove_legacy_extension` must delete only what this tool generated.
/// `uninstall` gates every delete on the same marker
/// (`uninstall_omp_extension_deletes_only_generated_file`), so an
/// unconditional delete on the install path would destroy exactly what
/// uninstall refuses to touch.
#[test]
fn legacy_cleanup_spares_a_user_owned_file_of_the_same_name() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("sessionmunch.ts");
    let user_content = "// user-owned extension that happens to use this filename\n";
    std::fs::write(&legacy, user_content).unwrap();

    remove_legacy_extension(&dir.path().join("sessionmunch-omp.ts"), "omp");

    assert!(legacy.exists(), "user-owned file must survive");
    assert_eq!(std::fs::read_to_string(&legacy).unwrap(), user_content);
}

/// The generated marker (banner + agent constant) is what authorises
/// removal.
#[test]
fn legacy_cleanup_removes_our_own_superseded_file() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("sessionmunch.ts");
    std::fs::write(
            &legacy,
            "// Auto-generated by `sessionmunch install-hooks --agent omp --apply`\nconst AGENT = \"omp\";\n",
        )
        .unwrap();

    remove_legacy_extension(&dir.path().join("sessionmunch-omp.ts"), "omp");

    assert!(
        !legacy.exists(),
        "our own superseded file should be removed"
    );
}

/// Pi and OMP can share one extensions directory, so a Pi install must
/// not delete an OMP-generated legacy file — that would silently stop
/// OMP capturing until someone reinstalled it.
#[test]
fn legacy_cleanup_does_not_touch_the_other_agents_file() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("sessionmunch.ts");
    let omp_content = "// Auto-generated by `sessionmunch install-hooks --agent omp --apply`\nconst AGENT = \"omp\";\n";
    std::fs::write(&legacy, omp_content).unwrap();

    // A *Pi* install runs its cleanup over the shared directory.
    remove_legacy_extension(&dir.path().join("sessionmunch-pi.ts"), "pi");

    assert!(legacy.exists(), "OMP's file must survive a Pi install");
    assert_eq!(std::fs::read_to_string(&legacy).unwrap(), omp_content);
}

/// `PI_CODING_AGENT_DIR` relocates Pi's whole agent config home
/// (`~/.pi/agent`), not just session storage, so the extension must follow
/// the variable or Pi never loads it — the install reports success and
/// capture silently does nothing.
#[test]
fn pi_extension_path_honours_pi_coding_agent_dir() {
    let custom = if cfg!(windows) {
        r"C:\custom\pi-agent"
    } else {
        "/custom/pi-agent"
    };
    let path = pi_extension_path_in(Some(std::ffi::OsString::from(custom))).unwrap();
    assert_eq!(
        path,
        Path::new(custom)
            .join("extensions")
            .join("sessionmunch-pi.ts")
    );

    // Unset and blank both fall back to ~/.pi/agent/extensions/sessionmunch-pi.ts.
    // Blank counts as unset because an exported-but-empty variable is nearly
    // always a failed shell expansion, not a request to install into the
    // filesystem root.
    for env in [None, Some(std::ffi::OsString::new())] {
        let path = pi_extension_path_in(env).unwrap();
        assert!(
            path.ends_with(
                Path::new(".pi")
                    .join("agent")
                    .join("extensions")
                    .join("sessionmunch-pi.ts")
            ),
            "default must be ~/.pi/agent/extensions/sessionmunch-pi.ts, got {}",
            path.display()
        );
    }
}

/// Same relocation contract as Pi: OMP honors `PI_CODING_AGENT_DIR` for
/// the default profile and moves `~/.omp/agent` wholesale, extensions
/// included.
#[test]
fn omp_extension_path_honours_pi_coding_agent_dir() {
    let custom = if cfg!(windows) {
        r"C:\custom\omp-agent"
    } else {
        "/custom/omp-agent"
    };
    let path = omp_extension_path_in(Some(std::ffi::OsString::from(custom)), None).unwrap();
    assert_eq!(
        path,
        Path::new(custom)
            .join("extensions")
            .join("sessionmunch-omp.ts")
    );

    for env in [None, Some(std::ffi::OsString::new())] {
        let path = omp_extension_path_in(env, None).unwrap();
        assert!(
            path.ends_with(
                Path::new(".omp")
                    .join("agent")
                    .join("extensions")
                    .join("sessionmunch-omp.ts")
            ),
            "default must be ~/.omp/agent/extensions/sessionmunch-omp.ts, got {}",
            path.display()
        );
    }
}

#[test]
fn omp_extension_path_honours_profile() {
    let path = omp_extension_path_in(None, Some("my-profile")).unwrap();
    assert!(
        path.ends_with(
            Path::new(".omp")
                .join("profiles")
                .join("my-profile")
                .join("agent")
                .join("extensions")
                .join("sessionmunch-omp.ts")
        ),
        "profile path must be correct, got {}",
        path.display()
    );
}

#[test]
fn omp_extension_is_directly_discoverable_by_omp() {
    let tmp = TempDir::new().unwrap();
    let args = InstallHooksArgs {
        agent: AgentChoice::Omp,
        capture_assistant: false,
        no_capture_prompts: false,
        capture_mode: None,
        capture_prompts: false,
        hooks_dir: None,
        server_url: Some("http://127.0.0.1:49374".into()),
        auth_token: None,
        as_user: None,
        apply: true,
        config_file: Some(tmp.path().join("extensions").join("sessionmunch-omp.ts")),
        project_strategy: Some(ProjectStrategyArg::Basename),
        profile: None,
    };

    let path = resolve_omp_extension_path(&args).unwrap();
    assert_eq!(
        path.file_name().and_then(|s| s.to_str()),
        Some("sessionmunch-omp.ts")
    );
    assert_eq!(
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str()),
        Some("extensions")
    );
}

#[test]
fn pi_extension_is_directly_discoverable_by_pi() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("extensions").join("sessionmunch-pi.ts");
    let args = InstallHooksArgs {
        agent: AgentChoice::Pi,
        capture_assistant: false,
        no_capture_prompts: false,
        capture_mode: None,
        capture_prompts: false,
        hooks_dir: None,
        server_url: Some("http://127.0.0.1:49374".into()),
        auth_token: None,
        as_user: None,
        apply: true,
        config_file: Some(path.clone()),
        project_strategy: Some(ProjectStrategyArg::Basename),
        profile: None,
    };

    let resolved = resolve_pi_extension_path(&args).unwrap();

    assert_eq!(resolved, path);
    assert_eq!(
        resolved.file_name().and_then(|s| s.to_str()),
        Some("sessionmunch-pi.ts")
    );
    assert_eq!(
        resolved
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str()),
        Some("extensions")
    );
}

#[test]
fn pi_extension_bakes_allowlist_admit_gate() {
    // Pi is `build_omp_extension` with the AGENT constant swapped, so the
    // gate must survive that string-replace rewrite too.
    let extension = build_pi_extension("http://127.0.0.1:49374", Some("tok"), None, "allowlist");
    assert!(
        extension.contains("const CAPTURE_MODE: \"allowlist\" | \"denylist\" = \"allowlist\";"),
        "{extension}"
    );
    assert!(extension.contains(CAPTURE_ADMIT_GATE_TS), "{extension}");
}

#[test]
fn pi_extension_denylist_bakes_inert_gate() {
    let extension = build_pi_extension("http://127.0.0.1:49374", Some("tok"), None, "denylist");
    assert!(
        extension.contains("const CAPTURE_MODE: \"allowlist\" | \"denylist\" = \"denylist\";"),
        "{extension}"
    );
    assert!(extension.contains(CAPTURE_ADMIT_GATE_TS), "{extension}");
}

#[test]
fn pi_extension_contains_lifecycle_capture_and_mcp_bridge() {
    let extension =
        build_pi_extension("http://127.0.0.1:49374/base", Some("tok"), None, "denylist");

    assert!(extension.contains("export default function SessionMunchExtension(pi: any): void"));
    assert!(extension.contains("const AGENT = \"pi\";"));
    assert!(extension.contains("pi.on(\"session_start\""));
    assert!(extension.contains("pi.on(\"before_agent_start\""));
    assert!(extension.contains("pi.on(\"tool_call\""));
    assert!(extension.contains("pi.on(\"tool_result\""));
    assert!(extension.contains("pi.on(\"session_before_compact\""));
    assert!(extension.contains("pi.on(\"session_compact\""));
    assert!(!extension.contains("pi.on(\"session.compacting\""));
    assert!(extension.contains("pi.on(\"agent_end\""));
    assert!(extension.contains("pi.on(\"session_shutdown\""));
    assert!(extension.contains("postHook(\"session-start\""));
    assert!(extension.contains("postHook(\"user-prompt\""));
    assert!(extension.contains("postHook(\"pre-tool-use\""));
    assert!(extension.contains("postHook(\"post-tool-use\""));
    assert!(extension.contains("postHook(\"pre-compact\""));
    assert!(extension.contains("postHook(\"stop\""));
    assert!(extension.contains("postHook(\"session-end\""));
    assert!(extension.contains("fetchHandoff"));
    assert!(extension.contains("customType: \"sessionmunch-handoff\""));
    assert!(extension.contains("const MCP_SERVER = deriveMcpServer(SERVER);"));
    assert!(extension.contains("return trimmed.endsWith(\"/mcp\") ? trimmed : `${trimmed}/mcp`;"));
    assert!(extension.contains("\"Accept\": \"application/json, text/event-stream\""));
    assert!(extension.contains("...authHeaders()"));
    assert!(extension.contains("headers[\"X-Memory-Actor-Session-Id\"] = session;"));
    assert!(extension.contains("headers[\"Mcp-Session-Id\"] = session;"));
    assert!(extension.contains("function mcpSignal(signal?: AbortSignal)"));
    assert!(extension.contains("anyFactory([signal, timeout])"));
    assert!(extension.contains("mcpRpc(\"initialize\""));
    assert!(extension.contains("mcpRpc(\"notifications/initialized\""));
    assert!(extension.contains("mcpRpc(\"tools/list\""));
    assert!(extension.contains("pi.registerTool"));
    assert!(extension.contains("label: tool.name"));
    assert!(extension.contains("parameters: toolInputSchema(tool)"));
    assert!(extension.contains(
        "mcpRpc(\"tools/call\", { name: tool.name, arguments: params ?? {} }, ctx, signal)"
    ));
    assert!(extension.contains("payload?.error"));
    assert!(extension.contains("payload?.result?.isError"));
    assert!(extension.contains("response.ok"));
    assert!(extension.contains("signal: mcpSignal(signal)"));
    assert!(extension.contains("Bearer ${token}"));
    assert!(extension.contains("tok"));
    assert!(extension.contains("import { execFileSync } from \"node:child_process\";"));
    assert!(!extension.contains(".omp"));
    assert!(!extension.contains("serve --transport stdio"));
    assert!(!extension.contains("serve --stdio"));
    // #676: the pi string-transform (api.on( -> pi.on() must still
    // produce an async session_shutdown handler that awaits the bounded
    // dispose flush, mirroring the omp source it derives from.
    assert!(extension.contains("const HOOK_DISPOSE_DRAIN_BUDGET_MS = 2000;"));
    assert!(extension.contains("function requestHookDrain(): Promise<void>"));
    assert!(extension.contains("async function drainHookQueueForDispose(): Promise<void>"));
    assert!(extension.contains("pi.on(\"session_shutdown\", async (_event: any, ctx: any) => {"));
    assert!(extension.contains("await drainHookQueueForDispose();"));
    assert!(
        !extension.contains("pi.on(\"session_shutdown\", (_event: any, ctx: any) => {"),
        "session_shutdown must not regress to the sync fire-and-forget form: {extension}"
    );
}

// Windows 11 + Git Bash support matters for regulated enterprise setups
// where Git Bash is the approved shell available from the corporate
// repository, so this installer contract should be exercised anywhere
// Bash is the supported execution surface.
#[cfg(any(unix, windows))]
#[test]
fn curl_installer_accepts_generated_integration_agents() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts")
        .join("install-hooks.sh");
    let Some(bash) = bash_program_for_installer_test() else {
        return;
    };

    for alias in ["opencode", "openclaw", "omp", "oh-my-pi", "pi"] {
        let output = Command::new(&bash)
            .arg(&script)
            .arg("--agent")
            .arg(alias)
            .output()
            .unwrap_or_else(|e| {
                panic!("failed to run {} for alias {alias}: {e}", script.display())
            });

        assert!(
            output.status.success(),
            "script rejected generated integration alias {alias}: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        match alias {
            "opencode" => assert!(stdout.contains("install-hooks --agent opencode --apply")),
            "openclaw" => assert!(stdout.contains("install-hooks --agent openclaw --apply")),
            "omp" | "oh-my-pi" => {
                assert!(stdout.contains("install-hooks --agent omp --apply"));
                assert!(stdout.contains("~/.omp/agent/extensions/sessionmunch.ts"));
            }
            "pi" => {
                assert!(stdout.contains("install-hooks --agent pi --apply"));
                assert!(stdout.contains("~/.pi/agent/extensions/sessionmunch.ts"));
                assert!(stdout.contains("MCP tools come through the same generated bridge"));
                assert!(!stdout.contains("~/.omp/agent/extensions/sessionmunch.ts"));
            }
            _ => unreachable!(),
        }
    }
}

// ----------------------------------------------------------------
// Cursor tests
// ----------------------------------------------------------------

#[test]
fn cursor_preserves_existing_user_hooks_and_adds_ours() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(
        hooks_tmp.path(),
        &[
            "session-start.sh",
            "session-end.sh",
            "user-prompt-submit.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "pre-compact.sh",
            "stop.sh",
        ],
    );

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("hooks.json");
    // Pre-existing settings with a user hook under a different event.
    fs::write(
        &config_path,
        r#"{"version":1,"hooks":{"userHook":"something"}}"#,
    )
    .unwrap();

    merge_cursor_hooks(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        None,
        &config_path,
    )
    .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    // User's hook survives.
    assert_eq!(parsed["hooks"]["userHook"], "something");
    // Our hooks are present.
    assert!(
        parsed["hooks"]["sessionStart"].is_array(),
        "sessionStart hook should be present"
    );
    assert!(
        parsed["hooks"]["preToolUse"].is_array(),
        "preToolUse hook should be present"
    );
    assert_eq!(
        parsed["version"], 1,
        "version: 1 must be set at the top level"
    );
}

#[test]
fn cursor_apply_is_idempotent() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(
        hooks_tmp.path(),
        &[
            "session-start.sh",
            "session-end.sh",
            "user-prompt-submit.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "pre-compact.sh",
            "stop.sh",
        ],
    );

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("hooks.json");

    let first = merge_cursor_hooks(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        None,
        &config_path,
    )
    .unwrap();
    assert_ne!(
        first,
        ApplyOutcome::NoOp,
        "first apply should not be a no-op"
    );

    let second = merge_cursor_hooks(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        None,
        &config_path,
    )
    .unwrap();
    assert_eq!(second, ApplyOutcome::NoOp, "second apply must be a no-op");
}

// ----------------------------------------------------------------
// Codex tests
// ----------------------------------------------------------------

#[test]
fn codex_preserves_unrelated_keys_and_adds_hooks() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(
        hooks_tmp.path(),
        &[
            "session-start.sh",
            "user-prompt-submit.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "pre-compact.sh",
            "stop.sh",
            "session-end.sh",
        ],
    );

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("hooks.json");
    // Pre-existing settings with an unrelated key.
    fs::write(&config_path, r#"{"theme":"dark"}"#).unwrap();

    merge_codex_hooks(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        None,
        &config_path,
    )
    .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    // Unrelated key survives.
    assert_eq!(parsed["theme"], "dark");
    // Our hooks are present.
    assert!(
        parsed["hooks"]["SessionStart"].is_array(),
        "SessionStart hook should be present"
    );
    assert!(
        parsed["hooks"]["SessionEnd"].is_array(),
        "SessionEnd is wired for Codex since Codex CLI 0.145.0"
    );
}

#[test]
fn codex_session_end_preserves_third_party_and_adds_ours() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(
        hooks_tmp.path(),
        &[
            "session-start.sh",
            "user-prompt-submit.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "pre-compact.sh",
            "stop.sh",
            "session-end.sh",
        ],
    );

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("hooks.json");
    // A third-party SessionEnd hook the user wired up themselves.
    fs::write(
            &config_path,
            r#"{"hooks":{"SessionEnd":[{"matcher":"","hooks":[{"type":"command","command":"user-own.sh"}]}]}}"#,
        )
        .unwrap();

    merge_codex_hooks(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        None,
        &config_path,
    )
    .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let session_end = parsed["hooks"]["SessionEnd"].as_array().unwrap();
    let rendered = serde_json::to_string(&parsed["hooks"]["SessionEnd"]).unwrap();
    // Third-party entry survives.
    assert!(
        rendered.contains("user-own.sh"),
        "third-party SessionEnd entry must survive: {rendered}"
    );
    // Our managed entry was added alongside it.
    assert!(
        rendered.contains("session-end"),
        "sessionmunch SessionEnd entry must be added: {rendered}"
    );
    assert_eq!(session_end.len(), 2);
    assert!(parsed["hooks"]["SessionStart"].is_array());
}

/// The test-only Codex wrapper must stage scripts under its injected
/// data-local root and wire that stable path into the generated config.
#[test]
fn codex_apply_stages_into_injected_dir() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(
        hooks_tmp.path(),
        &[
            "session-start.sh",
            "user-prompt-submit.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "pre-compact.sh",
            "stop.sh",
        ],
    );

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("hooks.json");
    let staging_tmp = TempDir::new().unwrap();

    apply_to_codex_settings_in(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        staging_tmp.path(),
        &InstallHooksArgs {
            profile: None,
            agent: AgentChoice::Codex,
            capture_assistant: false,
            no_capture_prompts: false,
            capture_mode: None,
            capture_prompts: false,
            hooks_dir: Some(hooks_tmp.path().to_path_buf()),
            server_url: Some("http://127.0.0.1:49374".to_string()),
            auth_token: None,
            config_file: Some(config_path.clone()),
            project_strategy: Some(ProjectStrategyArg::Basename),
            as_user: None,
            apply: false,
        },
    )
    .unwrap();

    let staged_script = staging_tmp
        .path()
        .join("hooks")
        .join("codex")
        .join("session-start.sh");
    assert!(
        staged_script.is_file(),
        "expected hook script staged at {}, override was not honoured",
        staged_script.display()
    );

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let command = parsed
        .pointer("/hooks/SessionStart/0/hooks/0/command")
        .and_then(serde_json::Value::as_str)
        .expect("SessionStart command should be present");
    assert!(
        command.contains(&staged_script.to_string_lossy().into_owned()),
        "generated command must reference staged script {}: {command}",
        staged_script.display()
    );
}

// ----------------------------------------------------------------
// Gemini tests
// ----------------------------------------------------------------

#[test]
fn gemini_preserves_mcp_servers_and_adds_hooks() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(
        hooks_tmp.path(),
        &[
            "session-start.sh",
            "session-end.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "pre-compact.sh",
        ],
    );

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("settings.json");
    // Pre-existing settings with an mcpServers entry.
    fs::write(&config_path, r#"{"mcpServers":{"foo":{}}}"#).unwrap();

    merge_gemini_hooks(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        None,
        &config_path,
    )
    .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    // The pre-existing mcpServers.foo survives.
    assert!(
        parsed["mcpServers"]["foo"].is_object(),
        "mcpServers.foo must survive"
    );
    // Our hooks are present with Gemini-specific event names.
    assert!(
        parsed["hooks"]["SessionStart"].is_array(),
        "SessionStart hook should be present"
    );
    assert!(
        parsed["hooks"]["BeforeTool"].is_array(),
        "BeforeTool hook should be present"
    );
    // Claude-Code-only events must NOT appear.
    assert!(
        parsed["hooks"].get("PreToolUse").is_none(),
        "PreToolUse must not appear in Gemini config"
    );
}

// ----------------------------------------------------------------
// Antigravity tests
// ----------------------------------------------------------------

#[test]
fn antigravity_preserves_existing_hooks_and_adds_ours() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(
        hooks_tmp.path(),
        &[
            "session-start.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "stop.sh",
        ],
    );

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("hooks.json");
    // Pre-existing settings with another named hook group.
    fs::write(
            &config_path,
            r#"{"my-linter":{"PostToolUse":[{"matcher":"run_command","hooks":[{"type":"command","command":"lint.sh"}]}]}}"#,
        )
        .unwrap();

    merge_antigravity_hooks(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        None,
        &config_path,
    )
    .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    // The pre-existing my-linter group survives.
    assert!(
        parsed["my-linter"]["PostToolUse"].is_array(),
        "my-linter.PostToolUse must survive"
    );
    // Our named group "sessionmunch" is present.
    assert!(
        parsed["sessionmunch"]["PreInvocation"].is_array(),
        "PreInvocation hook should be present"
    );
    assert!(
        parsed["sessionmunch"]["PreToolUse"].is_array(),
        "PreToolUse hook should be present"
    );
    assert!(
        parsed["sessionmunch"]["PostToolUse"].is_array(),
        "PostToolUse hook should be present"
    );
    assert!(
        parsed["sessionmunch"]["Stop"].is_array(),
        "Stop hook should be present"
    );
}

#[test]
fn antigravity_apply_is_idempotent() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(
        hooks_tmp.path(),
        &[
            "session-start.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "stop.sh",
        ],
    );

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("hooks.json");

    let first = merge_antigravity_hooks(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        None,
        &config_path,
    )
    .unwrap();
    assert_ne!(
        first,
        ApplyOutcome::NoOp,
        "first apply should not be a no-op"
    );

    let second = merge_antigravity_hooks(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        None,
        &config_path,
    )
    .unwrap();
    assert_eq!(second, ApplyOutcome::NoOp, "second apply must be a no-op");
}

// ----------------------------------------------------------------
// Kimi Code tests
// ----------------------------------------------------------------

const KIMI_CODE_STUB_SCRIPTS: [&str; 9] = [
    "session-start.sh",
    "user-prompt-submit.sh",
    "pre-tool-use.sh",
    "post-tool-use.sh",
    "pre-compact.sh",
    "stop.sh",
    "session-end.sh",
    "subagent-start.sh",
    "subagent-stop.sh",
];

fn kimi_code_hooks_in(config_path: &Path) -> Vec<(String, String)> {
    let doc: toml_edit::DocumentMut = fs::read_to_string(config_path)
        .unwrap()
        .parse()
        .expect("config.toml must stay valid TOML");
    doc.get("hooks")
        .and_then(toml_edit::Item::as_array_of_tables)
        .expect("`hooks` must be an array of tables")
        .iter()
        .map(|entry| {
            let event = entry
                .get("event")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let command = entry
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            (event, command)
        })
        .collect()
}

#[test]
fn kimi_code_apply_writes_all_events_into_fresh_config() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(hooks_tmp.path(), &KIMI_CODE_STUB_SCRIPTS);

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("config.toml");

    let outcome = merge_kimi_code_hooks(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        None,
        &config_path,
    )
    .unwrap();
    assert_eq!(outcome, ApplyOutcome::Created);

    let entries = kimi_code_hooks_in(&config_path);
    assert_eq!(entries.len(), KIMI_CODE_EVENTS.len());
    for (i, (event, script)) in KIMI_CODE_EVENTS.iter().enumerate() {
        assert_eq!(entries[i].0, *event, "entry order follows KIMI_CODE_EVENTS");
        let stem = script.strip_suffix(".sh").unwrap();
        assert!(
            entries[i].1.contains(stem),
            "{event}: command must reference the staged script: {}",
            entries[i].1
        );
    }
    // Only the allowed keys are written — Kimi Code rejects unknown
    // fields by failing the whole config load.
    let doc: toml_edit::DocumentMut = fs::read_to_string(&config_path).unwrap().parse().unwrap();
    for entry in doc
        .get("hooks")
        .and_then(toml_edit::Item::as_array_of_tables)
        .unwrap()
    {
        let keys: Vec<&str> = entry.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["event", "command"], "only event + command allowed");
    }
}

#[test]
fn kimi_code_apply_preserves_providers_and_third_party_hooks() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(hooks_tmp.path(), &KIMI_CODE_STUB_SCRIPTS);

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("config.toml");
    fs::write(
        &config_path,
        r#"# Kimi Code user config
model = "kimi-k2"

[providers.moonshot]
api_key = "sk-secret"
base_url = "https://api.moonshot.cn/v1"

[[hooks]]
event = "SessionStart"
command = "echo third-party"

[[hooks]]
event = "SessionStart"
command = "/opt/alphaone/bin/sessionmunch boot --quiet --limit 10 --budget-tokens 4096"

[[hooks]]
event = "SessionStart"
command = "SESSIONMUNCH_HOOK_URL=http://old:1 /old/sessionmunch/hooks/kimi-code/session-start.sh"
"#,
    )
    .unwrap();

    merge_kimi_code_hooks(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        None,
        &config_path,
    )
    .unwrap();

    let content = fs::read_to_string(&config_path).unwrap();
    assert!(
        content.contains("# Kimi Code user config"),
        "comments survive"
    );
    assert!(
        content.contains(r#"api_key = "sk-secret""#),
        "providers table survives"
    );

    let doc: toml_edit::DocumentMut = content.parse().unwrap();
    assert_eq!(
        doc.get("model").and_then(|v| v.as_str()),
        Some("kimi-k2"),
        "model setting survives"
    );
    assert_eq!(
        doc.get("providers")
            .and_then(|p| p.get("moonshot"))
            .and_then(|m| m.get("base_url"))
            .and_then(|v| v.as_str()),
        Some("https://api.moonshot.cn/v1"),
        "providers.moonshot survives"
    );

    let entries = kimi_code_hooks_in(&config_path);
    // Two third-party entries + our 9: the stale sessionmunch entry must be
    // replaced, not duplicated.
    assert_eq!(entries.len(), 2 + KIMI_CODE_EVENTS.len());
    let ours: Vec<&(String, String)> = entries
        .iter()
        .filter(|(_, command)| hook_command_is_ours(command))
        .collect();
    assert_eq!(ours.len(), KIMI_CODE_EVENTS.len(), "no duplicated entries");
    assert!(
        entries
            .iter()
            .any(|(_, command)| command == "echo third-party"),
        "third-party hook survives: {entries:?}"
    );
    assert!(
        entries.iter().any(|(_, command)| {
            command == "/opt/alphaone/bin/sessionmunch boot --quiet --limit 10 --budget-tokens 4096"
        }),
        "third-party string hook survives: {entries:?}"
    );
    assert!(
        !entries.iter().any(|(_, command)| command.contains("/old/")),
        "stale sessionmunch entry must be replaced: {entries:?}"
    );
}

#[test]
fn kimi_code_apply_is_idempotent() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(hooks_tmp.path(), &KIMI_CODE_STUB_SCRIPTS);

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("config.toml");

    let first = merge_kimi_code_hooks(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        None,
        &config_path,
    )
    .unwrap();
    assert_ne!(
        first,
        ApplyOutcome::NoOp,
        "first apply should not be a no-op"
    );

    let second = merge_kimi_code_hooks(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        None,
        &config_path,
    )
    .unwrap();
    assert_eq!(second, ApplyOutcome::NoOp, "second apply must be a no-op");
}

#[test]
fn claude_settings_path_honours_claude_config_dir() {
    let custom = if cfg!(windows) {
        r"C:\custom\claude"
    } else {
        "/custom/claude"
    };
    let path = claude_settings_path_in(Some(std::ffi::OsString::from(custom))).unwrap();
    assert_eq!(path, Path::new(custom).join("settings.json"));

    // Empty override and unset var both fall back to ~/.claude/settings.json.
    for env in [None, Some(std::ffi::OsString::new())] {
        let path = claude_settings_path_in(env).unwrap();
        assert!(
            path.ends_with(Path::new(".claude").join("settings.json")),
            "default must be ~/.claude/settings.json, got {}",
            path.display()
        );
    }
}

/// `CODEX_HOME` relocates Codex's entire config home, so hooks written to
/// `~/.codex` are never loaded by a Codex configured that way — the install
/// reports success and capture silently does nothing. sessionmunch already
/// honors the variable when resolving Codex transcripts, so the two halves
/// of one install have to agree on where that home is.
#[test]
fn codex_hooks_path_honours_codex_home() {
    let custom = if cfg!(windows) {
        r"C:\custom\codex"
    } else {
        "/custom/codex"
    };
    let path = codex_hooks_path_in(Some(std::ffi::OsString::from(custom))).unwrap();
    assert_eq!(path, Path::new(custom).join("hooks.json"));

    // Unset and blank both fall back to ~/.codex/hooks.json. Blank counts as
    // unset because an exported-but-empty variable is nearly always a failed
    // shell expansion, not a request to install into the filesystem root.
    for env in [None, Some(std::ffi::OsString::new())] {
        let path = codex_hooks_path_in(env).unwrap();
        assert!(
            path.ends_with(Path::new(".codex").join("hooks.json")),
            "default must be ~/.codex/hooks.json, got {}",
            path.display()
        );
    }
}

/// The test-only Claude Code wrapper must stage scripts under its injected
/// data-local root and wire that stable path into the generated config.
#[test]
fn claude_code_apply_stages_into_injected_dir() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(
        hooks_tmp.path(),
        &[
            "session-start.sh",
            "session-end.sh",
            "user-prompt-submit.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "pre-compact.sh",
            "stop.sh",
        ],
    );

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("settings.json");
    let staging_tmp = TempDir::new().unwrap();

    apply_to_claude_code_settings_in(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        staging_tmp.path(),
        &InstallHooksArgs {
            profile: None,
            agent: AgentChoice::ClaudeCode,
            capture_assistant: false,
            no_capture_prompts: false,
            capture_mode: None,
            capture_prompts: false,
            hooks_dir: Some(hooks_tmp.path().to_path_buf()),
            server_url: Some("http://127.0.0.1:49374".to_string()),
            auth_token: None,
            config_file: Some(config_path.clone()),
            project_strategy: Some(ProjectStrategyArg::Basename),
            as_user: None,
            apply: false,
        },
    )
    .unwrap();

    let staged_script = staging_tmp
        .path()
        .join("hooks")
        .join("claude-code")
        .join("session-start.sh");
    assert!(
        staged_script.is_file(),
        "expected hook script staged at {}, override was not honoured",
        staged_script.display()
    );

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let command = parsed
        .pointer("/hooks/SessionStart/0/hooks/0/command")
        .and_then(serde_json::Value::as_str)
        .expect("SessionStart command should be present");
    assert!(
        command.contains(&staged_script.to_string_lossy().into_owned()),
        "generated command must reference staged script {}: {command}",
        staged_script.display()
    );
}

#[test]
fn claude_prompt_opt_out_survives_reapply_and_preserves_third_party_hook() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(
        hooks_tmp.path(),
        &[
            "session-start.sh",
            "session-end.sh",
            "user-prompt-submit.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "pre-compact.sh",
            "stop.sh",
        ],
    );

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("settings.json");
    let staging_tmp = TempDir::new().unwrap();
    fs::write(
            &config_path,
            serde_json::json!({
                "hooks": {
                    "UserPromptSubmit": [
                        { "hooks": [{ "command": "third-party prompt guard" }] },
                        { "hooks": [{ "command": "/old/sessionmunch hook --event user-prompt --agent claude-code --server-url http://old" }] }
                    ]
                }
            })
            .to_string(),
        )
        .unwrap();

    let disabled = InstallHooksArgs {
        agent: AgentChoice::ClaudeCode,
        config_file: Some(config_path.clone()),
        no_capture_prompts: true,
        capture_mode: None,
        ..default_hook_args()
    };
    apply_to_claude_code_settings_in(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        staging_tmp.path(),
        &disabled,
    )
    .unwrap();

    let assert_disabled = || {
        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        let prompts = parsed["hooks"][CLAUDE_PROMPT_EVENT]
            .as_array()
            .expect("third-party prompt hook keeps the event present");
        assert_eq!(prompts.len(), 1);
        assert!(
            serde_json::to_string(prompts)
                .unwrap()
                .contains("third-party prompt guard")
        );
        assert!(!prompts.iter().any(is_sessionmunch_hook_entry));
    };
    assert_disabled();

    let bare_reapply = InstallHooksArgs {
        agent: AgentChoice::ClaudeCode,
        config_file: Some(config_path.clone()),
        ..default_hook_args()
    };
    apply_to_claude_code_settings_in(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        staging_tmp.path(),
        &bare_reapply,
    )
    .unwrap();
    assert_disabled();

    let enabled = InstallHooksArgs {
        agent: AgentChoice::ClaudeCode,
        config_file: Some(config_path.clone()),
        capture_prompts: true,
        ..default_hook_args()
    };
    apply_to_claude_code_settings_in(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        staging_tmp.path(),
        &enabled,
    )
    .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let prompts = parsed["hooks"][CLAUDE_PROMPT_EVENT].as_array().unwrap();
    assert_eq!(prompts.len(), 2, "third-party + one sessionmunch hook");
    assert_eq!(
        prompts
            .iter()
            .filter(|entry| is_sessionmunch_hook_entry(entry))
            .count(),
        1
    );
}

#[test]
fn kimi_code_config_path_honours_kimi_code_home() {
    let custom = if cfg!(windows) {
        r"C:\custom\kimi"
    } else {
        "/custom/kimi"
    };
    let path = kimi_code_config_path_in(Some(std::ffi::OsString::from(custom))).unwrap();
    assert_eq!(path, Path::new(custom).join("config.toml"));

    // Empty override and unset var both fall back to ~/.kimi-code.
    for env in [None, Some(std::ffi::OsString::new())] {
        let path = kimi_code_config_path_in(env).unwrap();
        assert!(
            path.ends_with(Path::new(".kimi-code").join("config.toml")),
            "default must be ~/.kimi-code/config.toml, got {}",
            path.display()
        );
    }
}

// ----------------------------------------------------------------
// Kiro CLI v2 agent-config hook tests.
// ----------------------------------------------------------------

const KIRO_CLI_STUB_SCRIPTS: [&str; 5] = [
    "session-start.sh",
    "user-prompt-submit.sh",
    "pre-tool-use.sh",
    "post-tool-use.sh",
    "stop.sh",
];

#[test]
fn kiro_cli_apply_writes_all_v2_events_into_agent_config() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(hooks_tmp.path(), &KIRO_CLI_STUB_SCRIPTS);

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("dev.json");
    fs::write(
        &config_path,
        r#"{"name": "dev", "description": "my agent", "tools": ["*"]}"#,
    )
    .unwrap();

    let outcome = merge_kiro_cli_agent_hooks(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        None,
        &config_path,
    )
    .unwrap();
    assert_eq!(outcome, ApplyOutcome::Updated);

    let root: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    // Non-hook agent fields survive the merge.
    assert_eq!(root["name"], "dev");
    assert_eq!(root["tools"][0], "*");
    let hooks = root["hooks"].as_object().expect("hooks object");
    assert_eq!(hooks.len(), KIRO_CLI_V2_EVENTS.len());
    for (event, script) in KIRO_CLI_V2_EVENTS {
        let entries = hooks[event].as_array().expect("event array");
        assert_eq!(entries.len(), 1, "{event}");
        let entry = entries[0].as_object().expect("entry object");
        let command = entry["command"].as_str().unwrap();
        let stem = script.strip_suffix(".sh").unwrap();
        assert!(
            command.contains(stem),
            "{event}: command must reference the staged script: {command}"
        );
        // The v2 Hook schema has no `type` field, and an empty-string
        // matcher would match no tool at all (absent = every tool), so
        // neither key may be written.
        assert!(!entry.contains_key("type"), "{event}: no type key");
        assert!(!entry.contains_key("matcher"), "{event}: no matcher key");
        if event == "agentSpawn" {
            // The handoff + brief must not be truncated mid-injection.
            assert_eq!(
                entry["max_output_size"],
                serde_json::json!(KIRO_CLI_V2_SESSION_START_MAX_OUTPUT)
            );
        } else {
            assert!(!entry.contains_key("max_output_size"), "{event}");
        }
    }
}

#[test]
fn kiro_cli_apply_preserves_third_party_hooks_and_is_idempotent() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(hooks_tmp.path(), &KIRO_CLI_STUB_SCRIPTS);

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("dev.json");
    fs::write(
            &config_path,
            r#"{
  "name": "dev",
  "hooks": {
    "agentSpawn": [
      {"command": "git status"},
      {"command": "echo sessionmunch status"},
      {"command": "SESSIONMUNCH_HOOK_URL=http://old:1 /old/sessionmunch/hooks/kiro-cli/session-start.sh"}
    ],
    "preToolUse": [
      {"matcher": "execute_bash", "command": "echo audit >> /tmp/audit.log"}
    ]
  }
}"#,
        )
        .unwrap();

    let apply = || {
        merge_kiro_cli_agent_hooks(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            None,
            &config_path,
        )
    };
    assert_eq!(apply().unwrap(), ApplyOutcome::Updated);

    let root: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let spawn = root["hooks"]["agentSpawn"].as_array().unwrap();
    // Third-party entry first (preserved), our refreshed entry after;
    // the stale sessionmunch entry is gone.
    assert_eq!(spawn.len(), 3);
    assert_eq!(spawn[0]["command"], "git status");
    assert_eq!(spawn[1]["command"], "echo sessionmunch status");
    let ours = spawn[2]["command"].as_str().unwrap();
    assert!(ours.contains("session-start"), "{ours}");
    assert!(
        !ours.contains("http://old:1"),
        "stale entry replaced: {ours}"
    );
    // A third-party hook under a trigger we also write survives with
    // its matcher intact.
    let pre = root["hooks"]["preToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 2);
    assert_eq!(pre[0]["matcher"], "execute_bash");

    // Second apply with identical inputs must be a no-op.
    assert_eq!(apply().unwrap(), ApplyOutcome::NoOp);
}

#[test]
fn kiro_cli_paths_honour_kiro_home() {
    let custom = if cfg!(windows) {
        r"C:\custom\kiro"
    } else {
        "/custom/kiro"
    };
    let agents = kiro_cli_home_join(Some(std::ffi::OsString::from(custom)), "agents").unwrap();
    assert_eq!(agents, Path::new(custom).join("agents"));
    let hooks = kiro_cli_home_join(Some(std::ffi::OsString::from(custom)), "hooks")
        .unwrap()
        .join("sessionmunch.json");
    assert_eq!(hooks, Path::new(custom).join("hooks/sessionmunch.json"));

    // Empty override and unset var both fall back to ~/.kiro.
    for env in [None, Some(std::ffi::OsString::new())] {
        let agents = kiro_cli_home_join(env.clone(), "agents").unwrap();
        assert!(
            agents.ends_with(Path::new(".kiro").join("agents")),
            "default must be ~/.kiro/agents, got {}",
            agents.display()
        );
    }
}

#[test]
fn kiro_hooks_infer_the_managed_mcp_client() {
    assert_eq!(
        mcp_client_for_agent(AgentChoice::KiroCli),
        Some(McpClient::KiroCli)
    );
    assert_eq!(
        mcp_client_for_agent(AgentChoice::KiroCliV3),
        Some(McpClient::KiroCli)
    );
}

#[test]
fn kiro_cli_v3_apply_writes_documented_schema_and_is_idempotent() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(hooks_tmp.path(), &KIRO_CLI_STUB_SCRIPTS);
    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("hooks/sessionmunch.json");

    let apply = || {
        merge_kiro_cli_v3_hooks(
            hooks_tmp.path(),
            "https://memory.example",
            Some("test-token"),
            config_tmp.path(),
            Some("repo-root"),
            &config_path,
        )
    };
    assert_eq!(apply().unwrap(), ApplyOutcome::Created);

    let root: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(root["version"], "v1");
    let hooks = root["hooks"].as_array().expect("hooks array");
    assert_eq!(hooks.len(), KIRO_CLI_V3_EVENTS.len());
    for ((trigger, script), entry) in KIRO_CLI_V3_EVENTS.iter().zip(hooks) {
        assert_eq!(entry["trigger"], *trigger);
        assert_eq!(entry["action"]["type"], "command");
        assert_eq!(
            entry["timeout"],
            if *trigger == "SessionStart" { 5 } else { 1 }
        );
        assert_eq!(entry["enabled"], true);
        assert_eq!(
            entry["name"],
            format!("sessionmunch-{}", script.trim_end_matches(".sh"))
        );
        let command = entry["action"]["command"].as_str().unwrap();
        assert!(is_sessionmunch_kiro_hook_command(command), "{command}");
        assert!(command.contains("test-token"), "{command}");
        assert!(command.contains("repo-root"), "{command}");
        assert!(entry.get("matcher").is_none());
    }
    assert_eq!(apply().unwrap(), ApplyOutcome::NoOp);
}

#[test]
fn kiro_cli_v3_apply_preserves_third_party_hooks_and_replaces_only_ours() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("sessionmunch.json");
    fs::write(
            &config_path,
            r#"{
  "version": "v1",
  "owner": "user",
  "hooks": [
    {"name":"audit","trigger":"PreToolUse","action":{"type":"command","command":"audit-tool"}},
    {"name":"sessionmunch-session-start","trigger":"SessionStart","action":{"type":"command","command":"SESSIONMUNCH_HOOK_URL=http://old /old/hooks/kiro-cli/session-start.sh"}}
  ]
}"#,
        )
        .unwrap();

    assert_eq!(
        merge_kiro_cli_v3_hooks(
            tmp.path(),
            "https://memory.example",
            None,
            tmp.path(),
            None,
            &config_path,
        )
        .unwrap(),
        ApplyOutcome::Updated
    );
    let root: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(root["owner"], "user");
    let hooks = root["hooks"].as_array().unwrap();
    assert_eq!(hooks.len(), 1 + KIRO_CLI_V3_EVENTS.len());
    assert_eq!(hooks[0]["name"], "audit");
    assert!(hooks[1..].iter().all(is_sessionmunch_kiro_v3_hook_entry));
}

#[test]
fn kiro_cli_v3_apply_rejects_incompatible_schema_and_reserved_collision() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("sessionmunch.json");
    fs::write(&config_path, r#"{"version":"v2","hooks":[]}"#).unwrap();
    let error = merge_kiro_cli_v3_hooks(
        tmp.path(),
        "https://memory.example",
        None,
        tmp.path(),
        None,
        &config_path,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("unsupported Kiro hook schema version"));

    fs::write(
            &config_path,
            r#"{"version":"v1","hooks":[{"name":"sessionmunch-stop","trigger":"Stop","action":{"type":"command","command":"third-party"}}]}"#,
        )
        .unwrap();
    let error = merge_kiro_cli_v3_hooks(
        tmp.path(),
        "https://memory.example",
        None,
        tmp.path(),
        None,
        &config_path,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("reserved name"));
}

#[test]
fn kiro_cli_agent_config_listing_is_sorted_json_only() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("b.json"), "{}").unwrap();
    fs::write(tmp.path().join("a.json"), "{}").unwrap();
    fs::write(tmp.path().join("notes.md"), "not an agent").unwrap();
    fs::create_dir(tmp.path().join("sub.json")).unwrap();

    let listed = list_kiro_cli_agent_configs(tmp.path()).unwrap();
    assert_eq!(
        listed,
        vec![tmp.path().join("a.json"), tmp.path().join("b.json")]
    );

    // A missing directory is an empty list, not an error.
    assert!(
        list_kiro_cli_agent_configs(&tmp.path().join("missing"))
            .unwrap()
            .is_empty()
    );

    let not_a_directory = tmp.path().join("agents.json");
    fs::write(&not_a_directory, "{}").unwrap();
    let error = list_kiro_cli_agent_configs(&not_a_directory).unwrap_err();
    assert!(
        format!("{error:#}").contains("reading"),
        "filesystem failures must not look like an empty agent directory: {error:#}"
    );
}

#[test]
fn kiro_cli_v2_preflight_is_all_or_nothing_and_preserves_per_agent_strategy() {
    let tmp = TempDir::new().unwrap();
    let repo_root = tmp.path().join("a.json");
    let basename = tmp.path().join("b.json");
    // Script-form hooks always include the URL marker; the project strategy
    // is an additional setting, not the ownership signature.
    fs::write(
            &repo_root,
            r#"{"name":"a","hooks":{"agentSpawn":[{"command":"SESSIONMUNCH_HOOK_URL=http://old:1 SESSIONMUNCH_PROJECT_STRATEGY=repo-root /old/hooks/kiro-cli/session-start.sh"}]}}"#,
        )
        .unwrap();
    fs::write(&basename, r#"{"name":"b"}"#).unwrap();

    let prepared = preflight_kiro_cli_agent_configs(
        vec![repo_root.clone(), basename.clone()],
        tmp.path(),
        "http://127.0.0.1:49374",
        None,
        tmp.path(),
        None,
    )
    .unwrap();
    assert_eq!(prepared[0].1, Some(ProjectStrategyArg::RepoRoot));
    assert_eq!(prepared[1].1, None);

    let before = fs::read_to_string(&repo_root).unwrap();
    fs::write(&basename, "{not-json").unwrap();
    let error = preflight_kiro_cli_agent_configs(
        vec![repo_root.clone(), basename],
        tmp.path(),
        "http://127.0.0.1:49374",
        None,
        tmp.path(),
        None,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("valid JSON"));
    assert_eq!(
        fs::read_to_string(repo_root).unwrap(),
        before,
        "a malformed later config must not mutate an earlier agent"
    );
}

// ----------------------------------------------------------------
// Devin tests
// ----------------------------------------------------------------

#[test]
fn devin_apply_writes_hooks_v1_json_by_default() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(
        hooks_tmp.path(),
        &[
            "session-start.sh",
            "session-end.sh",
            "user-prompt-submit.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "post-compaction.sh",
            "stop.sh",
        ],
    );

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("hooks.v1.json");

    apply_to_devin_settings_in(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        config_tmp.path(),
        &InstallHooksArgs {
            profile: None,
            agent: AgentChoice::Devin,
            capture_assistant: false,
            no_capture_prompts: false,
            capture_mode: None,
            capture_prompts: false,
            hooks_dir: Some(hooks_tmp.path().to_path_buf()),
            server_url: Some("http://127.0.0.1:49374".to_string()),
            auth_token: None,
            config_file: Some(config_path.clone()),
            project_strategy: Some(crate::cli::ProjectStrategyArg::Basename),
            as_user: None,
            apply: false,
        },
    )
    .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    // hooks.v1.json: the entire file IS the hooks object (no wrapper)
    assert!(
        parsed["SessionStart"].is_array(),
        "SessionStart hook should be present"
    );
    assert!(
        parsed["PostCompaction"].is_array(),
        "PostCompaction hook should be present"
    );
    assert!(
        parsed.get("hooks").is_none(),
        "hooks.v1.json should not have a 'hooks' wrapper"
    );
}

#[test]
fn devin_apply_config_json_hooks_key_via_flag() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(
        hooks_tmp.path(),
        &[
            "session-start.sh",
            "session-end.sh",
            "user-prompt-submit.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "post-compaction.sh",
            "stop.sh",
        ],
    );

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("config.json");

    // Pre-existing config.json with mcpServers
    fs::write(
        &config_path,
        r#"{"mcpServers":{"other-server":{"url":"http://example.com"}}}"#,
    )
    .unwrap();

    apply_to_devin_settings_in(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        config_tmp.path(),
        &InstallHooksArgs {
            profile: None,
            agent: AgentChoice::Devin,
            capture_assistant: false,
            no_capture_prompts: false,
            capture_mode: None,
            capture_prompts: false,
            hooks_dir: Some(hooks_tmp.path().to_path_buf()),
            server_url: Some("http://127.0.0.1:49374".to_string()),
            auth_token: None,
            config_file: Some(config_path.clone()),
            project_strategy: Some(crate::cli::ProjectStrategyArg::Basename),
            as_user: None,
            apply: false,
        },
    )
    .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    // config.json: hooks are nested under the "hooks" key
    assert!(
        parsed["hooks"]["SessionStart"].is_array(),
        "SessionStart hook should be present"
    );
    assert!(
        parsed["hooks"]["PostCompaction"].is_array(),
        "PostCompaction hook should be present"
    );
    // mcpServers should be preserved
    assert!(
        parsed["mcpServers"]["other-server"].is_object(),
        "mcpServers should be preserved"
    );
}

#[test]
fn devin_apply_both_targets_are_idempotent() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(
        hooks_tmp.path(),
        &[
            "session-start.sh",
            "session-end.sh",
            "user-prompt-submit.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "post-compaction.sh",
            "stop.sh",
        ],
    );

    // Test hooks.v1.json idempotency
    let config_tmp = TempDir::new().unwrap();
    let hooks_v1_path = config_tmp.path().join("hooks.v1.json");

    let args_v1 = InstallHooksArgs {
        profile: None,
        agent: AgentChoice::Devin,
        capture_assistant: false,
        no_capture_prompts: false,
        capture_mode: None,
        capture_prompts: false,
        hooks_dir: Some(hooks_tmp.path().to_path_buf()),
        server_url: Some("http://127.0.0.1:49374".to_string()),
        auth_token: None,
        config_file: Some(hooks_v1_path.clone()),
        project_strategy: Some(crate::cli::ProjectStrategyArg::Basename),
        as_user: None,
        apply: false,
    };

    apply_to_devin_settings_in(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        config_tmp.path(),
        &args_v1,
    )
    .unwrap();

    apply_to_devin_settings_in(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        config_tmp.path(),
        &args_v1,
    )
    .unwrap();

    // Idempotency check: second apply should be a no-op
    let v1_first_content = fs::read_to_string(&hooks_v1_path).unwrap();
    let v1_second_content = fs::read_to_string(&hooks_v1_path).unwrap();
    assert_eq!(
        v1_first_content, v1_second_content,
        "hooks.v1.json should be unchanged after second apply"
    );

    // Test config.json hooks key idempotency
    let config_tmp2 = TempDir::new().unwrap();
    let config_path = config_tmp2.path().join("config.json");
    fs::write(&config_path, "{}").unwrap();

    let args_config = InstallHooksArgs {
        profile: None,
        agent: AgentChoice::Devin,
        capture_assistant: false,
        no_capture_prompts: false,
        capture_mode: None,
        capture_prompts: false,
        hooks_dir: Some(hooks_tmp.path().to_path_buf()),
        server_url: Some("http://127.0.0.1:49374".to_string()),
        auth_token: None,
        config_file: Some(config_path.clone()),
        project_strategy: Some(crate::cli::ProjectStrategyArg::Basename),
        as_user: None,
        apply: false,
    };

    apply_to_devin_settings_in(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp2.path(),
        config_tmp2.path(),
        &args_config,
    )
    .unwrap();

    apply_to_devin_settings_in(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp2.path(),
        config_tmp2.path(),
        &args_config,
    )
    .unwrap();

    // Idempotency check: second apply should be a no-op
    let config_first_content = fs::read_to_string(&config_path).unwrap();
    let config_second_content = fs::read_to_string(&config_path).unwrap();
    assert_eq!(
        config_first_content, config_second_content,
        "config.json should be unchanged after second apply"
    );
}

#[test]
fn devin_preserves_existing_hooks_and_adds_ours() {
    let hooks_tmp = TempDir::new().unwrap();
    stub_scripts(
        hooks_tmp.path(),
        &[
            "session-start.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "post-compaction.sh",
            "stop.sh",
        ],
    );

    let config_tmp = TempDir::new().unwrap();
    let config_path = config_tmp.path().join("hooks.v1.json");
    // Pre-existing settings with another named hook group.
    fs::write(
            &config_path,
            r#"{"my-linter":{"PostToolUse":[{"matcher":"run_command","hooks":[{"type":"command","command":"lint.sh"}]}]}}"#,
        )
        .unwrap();

    apply_to_devin_settings_in(
        hooks_tmp.path(),
        "http://127.0.0.1:49374",
        None,
        config_tmp.path(),
        config_tmp.path(),
        &InstallHooksArgs {
            profile: None,
            agent: AgentChoice::Devin,
            capture_assistant: false,
            no_capture_prompts: false,
            capture_mode: None,
            capture_prompts: false,
            hooks_dir: Some(hooks_tmp.path().to_path_buf()),
            server_url: Some("http://127.0.0.1:49374".to_string()),
            auth_token: None,
            config_file: Some(config_path.clone()),
            project_strategy: Some(crate::cli::ProjectStrategyArg::Basename),
            as_user: None,
            apply: false,
        },
    )
    .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    // The pre-existing my-linter group survives.
    assert!(
        parsed["my-linter"]["PostToolUse"].is_array(),
        "my-linter.PostToolUse must survive"
    );
    // Our hooks are present at the top level (hooks.v1.json format).
    assert!(
        parsed["SessionStart"].is_array(),
        "SessionStart hook should be present"
    );
    assert!(
        parsed["PostCompaction"].is_array(),
        "PostCompaction hook should be present"
    );
    assert!(
        parsed["PostToolUse"].is_array(),
        "PostToolUse hook should be present"
    );
}

#[test]
fn devin_session_start_injects_handoff_additional_context() {
    let hooks_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("hooks");
    let devin_session_start = hooks_root.join("devin").join("session-start.sh");

    // Normalized because this is the only assertion below that spans a
    // line break: on a Windows checkout with core.autocrlf=true the
    // working-tree copy has CRLF even though the committed blob is
    // LF-only, which breaks an exact multi-line substring match.
    let script_content = fs::read_to_string(&devin_session_start)
        .unwrap()
        .replace("\r\n", "\n");
    // Verify the script injects handoff via hookSpecificOutput.additionalContext
    assert!(
        script_content.contains("hookSpecificOutput"),
        "Devin session-start.sh must inject handoff via hookSpecificOutput"
    );
    assert!(
        script_content.contains("additionalContext"),
        "Devin session-start.sh must use additionalContext field"
    );
    assert!(
        script_content.contains("sessionmunch_get_handoff"),
        "Devin session-start.sh must fetch handoff"
    );
    assert!(
        script_content.contains("$SERVER/handoff?agent=devin${QS}${SID_QS}"),
        "Devin session-start.sh must bind the handoff claim to its generated session id"
    );
    assert!(
        !script_content.contains("/handoff/latest"),
        "Devin session-start.sh must not call the removed /handoff/latest route"
    );
    assert!(
        script_content.contains("sessionmunch_json_string"),
        "Devin session-start.sh must JSON-escape handoff text before embedding it"
    );
    assert!(
        script_content.contains("else\n    printf '{}\\n'\nfi"),
        "Devin session-start.sh must print {{}} only when no handoff is available"
    );
}

#[test]
fn startup_handoff_scripts_forward_native_session_ids() {
    let hooks_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("hooks");
    for agent in ["claude-code", "codex", "opencode", "cursor", "gemini-cli"] {
        let script = fs::read_to_string(hooks_root.join(agent).join("session-start.sh"))
            .unwrap()
            .replace("\r\n", "\n");
        assert!(
            script.contains("SESSION_ID=$(sessionmunch_extract_session_id \"$PAYLOAD\")"),
            "{agent} must extract the native receiver session id"
        );
        assert!(
            script.contains("${SESSION_QS}"),
            "{agent} must forward the native receiver session id to /handoff"
        );
    }
}

#[test]
fn grok_placeholder_expansion_resolves_values_and_defaults() {
    let expanded = expand_grok_placeholders_with("${HOST}/mcp ${TOKEN:-fallback}", |name| {
        (name == "HOST").then(|| "https://memory.example".to_string())
    })
    .unwrap();
    assert_eq!(expanded, "https://memory.example/mcp fallback");
}

#[test]
fn grok_placeholder_expansion_rejects_missing_variable() {
    let err = expand_grok_placeholders_with("${MISSING}", |_| None).unwrap_err();
    assert!(
        err.to_string()
            .contains("pass explicit --server-url and --auth-token")
    );
}

#[test]
fn grok_project_overlays_cover_repo_root_through_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let parent = root.join("nested");
    let cwd = parent.join("leaf");
    fs::create_dir_all(&cwd).unwrap();

    for directory in [&root, &parent, &cwd] {
        let overlay = directory.join(".grok/config.toml");
        fs::create_dir_all(overlay.parent().unwrap()).unwrap();
        fs::write(&overlay, "# override").unwrap();
        assert_eq!(find_grok_project_overlay(&cwd, Some(&root)), Some(overlay));
        fs::remove_file(directory.join(".grok/config.toml")).unwrap();
    }
    assert_eq!(find_grok_project_overlay(&cwd, Some(&root)), None);
}

#[test]
fn grok_project_overlay_outside_repo_checks_only_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("outside").join("leaf");
    fs::create_dir_all(&cwd).unwrap();
    let parent_overlay = cwd.parent().unwrap().join(".grok/config.toml");
    fs::create_dir_all(parent_overlay.parent().unwrap()).unwrap();
    fs::write(&parent_overlay, "# ignored").unwrap();
    assert_eq!(find_grok_project_overlay(&cwd, None), None);

    let cwd_overlay = cwd.join(".grok/config.toml");
    fs::create_dir_all(cwd_overlay.parent().unwrap()).unwrap();
    fs::write(&cwd_overlay, "# active").unwrap();
    assert_eq!(find_grok_project_overlay(&cwd, None), Some(cwd_overlay));
}
