//! `sessionmunch install-hooks` — install lifecycle-hook configuration for
//! the chosen agent CLI.
//!
//! Two modes:
//!
//! - **Default (print):** renders the JSON/TOML/TypeScript snippet the
//!   user should merge into their agent CLI's settings file, plus the
//!   absolute paths to the vendored shell scripts. Nothing is written to
//!   disk.
//!
//! - **`--apply` (recommended):** performs an atomic in-place merge into
//!   the target config file. A timestamped backup (`.bak-<unix-ts>`) is
//!   written next to the file before any mutation. Re-runs are
//!   idempotent — a second `--apply` with unchanged content is a no-op
//!   and produces no backup.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::{AgentChoice, CaptureModeArg, InstallHooksArgs, McpClient, ProjectStrategyArg};
use crate::commands::apply_shared::{ApplyOutcome, apply_atomic, mutate_json, mutate_toml};
use crate::commands::install_mcp;
use crate::commands::openclaw_plugin;
use crate::commands::path_util::home_dir;
use crate::commands::render_shared::{
    ANTIGRAVITY_LIFECYCLE_EVENTS, ANTIGRAVITY_TOOL_EVENTS, CODEX_PROFILE, COMMAND_CODE_PROFILE,
    CURSOR_PROFILE, GEMINI_PROFILE, KIMI_CODE_EVENTS, KIRO_CLI_V2_EVENTS, KIRO_CLI_V3_EVENTS,
    POOL_EVENTS, build_antigravity_payload_with_data_dir, build_claude_code_payload_with_data_dir,
    build_devin_payload_with_data_dir, build_grok_payload_with_data_dir,
    build_kiro_cli_v2_hooks_value, build_kiro_cli_v3_hooks_value,
    build_pool_settings_yaml_with_data_dir, build_profile_payload_for_agent,
    hook_script_for_claude_code, hook_script_for_current_platform, kimi_code_hook_commands,
    local_hook_policy_v1_supported, ts_capture_policy_v1, ts_string_literal,
};
use crate::commands::uninstall::hook_command_is_ours;
use crate::config::{Config, DEFAULT_SERVER_URL};

const CLAUDE_PROMPT_EVENT: &str = "UserPromptSubmit";

/// Claude Code's settings file — hooks live under `hooks`.
/// `$CLAUDE_CONFIG_DIR/settings.json` when the var is set, else
/// `~/.claude/settings.json`.
pub(crate) fn claude_settings_path() -> anyhow::Result<std::path::PathBuf> {
    claude_settings_path_in(std::env::var_os("CLAUDE_CONFIG_DIR"))
}

/// The env value comes in as a parameter so tests can exercise both
/// branches without mutating process env (mirrors
/// `install_mcp::kimi_code_home`).
fn claude_settings_path_in(
    env_override: Option<std::ffi::OsString>,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(dir) = crate::commands::path_util::claude_config_dir(env_override) {
        return Ok(dir.join("settings.json"));
    }
    Ok(home_dir()
        .context("could not locate $HOME for ~/.claude/settings.json")?
        .join(".claude")
        .join("settings.json"))
}

/// Codex's hooks file — `$CODEX_HOME/hooks.json` when the var is set, else
/// `~/.codex/hooks.json`.
///
/// `CODEX_HOME` relocates Codex's whole config home, so hooks written to the
/// default path are simply never loaded by a Codex configured that way: capture
/// silently does nothing, with a successful-looking install. sessionmunch already
/// honors the variable when it resolves that Codex's transcripts
/// (`ManagedHarness::Codex` in `sessionmunch-workstream`), so ignoring it here left
/// one half of the same install pointing somewhere the other half did not.
pub(crate) fn codex_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    codex_hooks_path_in(std::env::var_os("CODEX_HOME"))
}

/// The env value comes in as a parameter so tests can exercise both branches
/// without mutating process env (mirrors [`claude_settings_path_in`]).
fn codex_hooks_path_in(
    env_override: Option<std::ffi::OsString>,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(dir) = crate::commands::path_util::agent_config_home(env_override) {
        return Ok(dir.join("hooks.json"));
    }
    Ok(home_dir()
        .context("could not locate $HOME for ~/.codex/hooks.json")?
        .join(".codex")
        .join("hooks.json"))
}

/// `~/.commandcode/settings.json`.
pub(crate) fn command_code_settings_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.commandcode/settings.json")?
        .join(".commandcode")
        .join("settings.json"))
}

/// `~/.cursor/hooks.json`.
pub(crate) fn cursor_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.cursor/hooks.json")?
        .join(".cursor")
        .join("hooks.json"))
}

/// `~/.gemini/settings.json`.
pub(crate) fn gemini_settings_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.gemini/settings.json")?
        .join(".gemini")
        .join("settings.json"))
}

/// `~/.gemini/config/hooks.json` — Antigravity CLI lifecycle hooks.
pub(crate) fn antigravity_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.gemini/config/hooks.json")?
        .join(".gemini")
        .join("config")
        .join("hooks.json"))
}

/// `~/.grok/hooks/sessionmunch.json` — Grok Build CLI lifecycle hooks.
pub(crate) fn grok_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(install_mcp::grok_home()?
        .join("hooks")
        .join("sessionmunch.json"))
}

/// `~/.config/zero/hooks.json` — Zero's user-level lifecycle hook config
/// (issue #156). Zero resolves it under `$XDG_CONFIG_HOME` falling back to
/// `~/.config`; like the OpenCode plugin path below we target the default
/// location and `--config-file` covers non-default XDG setups.
pub(crate) fn zero_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.config/zero/hooks.json")?
        .join(".config")
        .join("zero")
        .join("hooks.json"))
}

/// `~/.zcode/cli/config.json` — ZCode's user-scope config. The root
/// `hooks` block lives in the same file `install-mcp --client zcode`
/// registers MCP servers in; user scope runs headless with no trust
/// gate, while ZCode's workspace scopes (`.zcode/config.json`,
/// `zcode.json`) are trust-gated and off by default, so they are never
/// targeted (#512).
pub(crate) fn zcode_config_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.zcode/cli/config.json")?
        .join(".zcode")
        .join("cli")
        .join("config.json"))
}

/// `~/.devin/hooks.v1.json` — Devin CLI lifecycle hooks (default target).
pub(crate) fn devin_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.devin/hooks.v1.json")?
        .join(".devin")
        .join("hooks.v1.json"))
}

/// `~/.devin/config.json` — Devin CLI lifecycle hooks (alternative target under `hooks` key).
pub(crate) fn devin_config_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.devin/config.json")?
        .join(".devin")
        .join("config.json"))
}

/// `~/.config/opencode/plugins/sessionmunch.ts` — OpenCode's plugin file.
pub(crate) fn opencode_plugin_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.config/opencode")?
        .join(".config")
        .join("opencode")
        .join("plugins")
        .join("sessionmunch.ts"))
}

/// `$PI_CODING_AGENT_DIR/extensions/sessionmunch.ts` when the var is set, else
/// `~/.omp/agent/extensions/sessionmunch.ts` — OMP lifecycle extension.
///
/// `PI_CODING_AGENT_DIR` relocates OMP's whole agent config home
/// (`~/.omp/agent`), so extensions written to the default path are never
/// loaded by an OMP configured that way: capture silently does nothing, with
/// a successful-looking install. sessionmunch already honors the variable when
/// it resolves OMP transcripts (`ManagedHarness::Omp` in
/// `sessionmunch-workstream`), so ignoring it here left one half of the same
/// install pointing somewhere the other half did not.
pub(crate) fn omp_extension_path(profile: Option<&str>) -> anyhow::Result<std::path::PathBuf> {
    // Bind the OsString before borrowing from it: `var_os(..).as_ref()` in a
    // single expression drops the temporary at the end of the statement.
    let env_profile = std::env::var_os("OMP_PROFILE");
    let profile = profile.or_else(|| env_profile.as_ref().and_then(|s| s.to_str()));
    omp_extension_path_in(std::env::var_os("PI_CODING_AGENT_DIR"), profile)
}

/// The env value comes in as a parameter so tests can exercise both branches
/// without mutating process env (mirrors [`codex_hooks_path_in`]).
fn omp_extension_path_in(
    env_override: Option<std::ffi::OsString>,
    profile: Option<&str>,
) -> anyhow::Result<std::path::PathBuf> {
    // Precedence: `PI_CODING_AGENT_DIR` wins over `--profile`.
    //
    // OMP documents each separately — the variable "can override the agent
    // directory", and `--profile <name>` moves it to
    // `~/.omp/profiles/<name>/agent` — but does not define what happens when
    // both are set. Rather than guess at a composition (an earlier revision
    // derived `$DIR/profiles/<name>/agent`, and re-rooted differently
    // depending on whether the path's last component happened to be named
    // "agent", so the result changed with the directory's *name*), this
    // takes the reading each is documented to have: the variable names the
    // agent directory outright, so when it is set that is the agent
    // directory. The profile then has nothing left to relocate, and the
    // caller says so instead of silently picking one.
    let base_dir = if let Some(dir) = crate::commands::path_util::agent_config_home(env_override) {
        dir
    } else {
        let root = home_dir()
            .context("could not locate $HOME for ~/.omp")?
            .join(".omp");
        if let Some(profile_name) = profile {
            root.join("profiles").join(profile_name).join("agent")
        } else {
            root.join("agent")
        }
    };
    Ok(base_dir.join("extensions").join("sessionmunch-omp.ts"))
}

/// `$PI_CODING_AGENT_DIR/extensions/sessionmunch-pi.ts` when the var is set, else
/// `~/.pi/agent/extensions/sessionmunch-pi.ts` — Pi lifecycle + MCP bridge extension.
///
/// `PI_CODING_AGENT_DIR` relocates Pi's whole agent config home
/// (`~/.pi/agent`), so extensions written to the default path are never
/// loaded by a Pi configured that way: capture silently does nothing, with a
/// successful-looking install. sessionmunch already honors the variable when it
/// resolves Pi transcripts (`ManagedHarness::Pi` in `sessionmunch-workstream`),
/// so ignoring it here left one half of the same install pointing somewhere
/// the other half did not.
pub(crate) fn pi_extension_path() -> anyhow::Result<std::path::PathBuf> {
    pi_extension_path_in(std::env::var_os("PI_CODING_AGENT_DIR"))
}

/// The env value comes in as a parameter so tests can exercise both branches
/// without mutating process env (mirrors [`codex_hooks_path_in`]).
fn pi_extension_path_in(
    env_override: Option<std::ffi::OsString>,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(dir) = crate::commands::path_util::agent_config_home(env_override) {
        return Ok(dir.join("extensions").join("sessionmunch-pi.ts"));
    }
    Ok(home_dir()
        .context("could not locate $HOME for ~/.pi/agent/extensions")?
        .join(".pi")
        .join("agent")
        .join("extensions")
        .join("sessionmunch-pi.ts"))
}

/// `$KIMI_CODE_HOME/config.toml` when set, else `~/.kimi-code/config.toml`.
/// Kimi Code keeps hooks as `[[hooks]]` entries inside the same
/// config.toml that stores the user's providers/model, so the apply
/// path merges TOML-aware instead of rewriting the file.
pub(crate) fn kimi_code_config_path() -> anyhow::Result<std::path::PathBuf> {
    kimi_code_config_path_in(std::env::var_os("KIMI_CODE_HOME"))
}

/// The env value comes in as a parameter so tests can exercise both
/// branches without mutating process env (mirrors
/// `install_mcp::kimi_code_home`).
fn kimi_code_config_path_in(
    env_override: Option<std::ffi::OsString>,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(dir) = env_override.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir).join("config.toml"));
    }
    Ok(home_dir()
        .context("could not locate $HOME for ~/.kimi-code/config.toml")?
        .join(".kimi-code")
        .join("config.toml"))
}

/// `$KIRO_HOME/agents` when set, else `~/.kiro/agents`. The Kiro CLI v2
/// engine embeds hooks inside per-agent config JSONs in this directory;
/// there is no global v2 hook surface, and the built-in default agent
/// has no file on disk. `KIRO_HOME` relocation is honored by the real
/// binary (verified against kiro-cli 2.16.0: settings are written under
/// `$KIRO_HOME` when set).
pub(crate) fn kiro_cli_agents_dir() -> anyhow::Result<std::path::PathBuf> {
    kiro_cli_home_join(std::env::var_os("KIRO_HOME"), "agents")
}

/// `$KIRO_HOME/hooks/sessionmunch.json` when set, else
/// `~/.kiro/hooks/sessionmunch.json`. Kiro v3 loads each standalone JSON file in
/// this directory; using one sessionmunch-owned file avoids mutating unrelated
/// registrations.
pub(crate) fn kiro_cli_v3_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(kiro_cli_home_join(std::env::var_os("KIRO_HOME"), "hooks")?.join("sessionmunch.json"))
}

/// The env value comes in as a parameter so tests can exercise both
/// branches without mutating process env (mirrors
/// `kimi_code_config_path_in`).
fn kiro_cli_home_join(
    env_override: Option<std::ffi::OsString>,
    child: &str,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(dir) = env_override.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir).join(child));
    }
    Ok(home_dir()
        .context("could not locate $HOME for ~/.kiro")?
        .join(".kiro")
        .join(child))
}

/// Run the `install-hooks` subcommand.
///
/// # Errors
/// Returns an error if the hook script directory cannot be located.
pub fn run(config: &Config, mut args: InstallHooksArgs) -> Result<()> {
    let inferred = if args.server_url.is_none() {
        infer_installed_mcp_config(args.agent)?
    } else {
        None
    };
    let server_url = effective_hook_server_url(config, &args, inferred.as_ref());
    let auth_token_owned = args
        .auth_token
        .clone()
        .or_else(|| config.auth.bearer_token.clone())
        .or_else(|| inferred.as_ref().and_then(|mcp| mcp.auth_token.clone()));
    // #552: persist the bearer under the data dir (0600) and keep it OUT of
    // the rendered config. Before this it went onto every hook's command line —
    // as `--auth-token <token>` for native hooks, as an `SESSIONMUNCH_AUTH_TOKEN=`
    // shell prefix for the script hooks — so it was readable through
    // `/proc/<pid>/cmdline` by any local user for as long as each hook ran, on
    // every tool call.
    //
    // Only `--apply` persists: a printed snippet is the operator's to place, and
    // writing a credential as a side effect of a dry run would be a surprise.
    let persisted = if args.apply
        && let Some(token) = auth_token_owned.as_deref()
    {
        crate::config::store_hook_auth_token(&config.data_dir, token).with_context(|| {
            format!(
                "storing the hook auth token under {}",
                config.data_dir.display()
            )
        })?;
        true
    } else {
        false
    };
    // Renderers take `Option<&str>` and omit the credential entirely when it is
    // `None`, so one decision here covers every agent instead of twenty edits.
    let auth = if persisted {
        None
    } else {
        auth_token_owned.as_deref()
    };
    // P1.8 multi-user attribution: `--as-user` is metadata only — the
    // token stamped into the hook env block is whatever the operator
    // passed via `--auth-token` (typically a native key from
    // `sessionmunch api-key add`). We surface the username to stderr so the
    // operator can confirm which identity their writes will attribute
    // to. Mismatch between `--as-user` and the actual token's owner is
    // the operator's concern; we don't reach back to the server to
    // verify (keeps install-hooks offline-capable).
    validate_as_user(args.as_user.as_deref(), auth)?;
    if let Some(user) = args.as_user.as_deref().filter(|s| !s.trim().is_empty()) {
        eprintln!("[sessionmunch] hooks installing for user: {user}");
    }
    let generated = matches!(
        args.agent,
        AgentChoice::OpenCode
            | AgentChoice::OpenCode2
            | AgentChoice::Omp
            | AgentChoice::Pi
            | AgentChoice::Openclaw
    );
    if generated || local_hook_policy_v1_supported() {
        eprintln!(
            "[sessionmunch] capture-policy capability v1 enforced by this selected integration; re-run --apply to refresh existing installs."
        );
    } else {
        eprintln!(
            "[sessionmunch] selected shell/PowerShell compatibility path does not enforce capture-policy v1; use a native platform selection or generated integration."
        );
    }
    // Assistant/Stop capture is Claude Code + native-platform only (#196). No
    // silent fallback: bail so an operator on a script-fallback platform or a
    // different agent is told the flag has no effect instead of installing a
    // command whose capture would be silently dropped.
    if args.capture_assistant && !capture_assistant_allowed(args.agent) {
        anyhow::bail!(
            "--capture-assistant requires --agent claude-code on a native hook platform \
             (PosixNative/WindowsNative). The current selection uses the script fallback or a \
             different agent, where the opt-in cannot take effect. Remove --capture-assistant or \
             switch to a native Claude Code install."
        );
    }
    if (args.no_capture_prompts || args.capture_prompts)
        && !prompt_capture_options_allowed(args.agent)
    {
        anyhow::bail!(
            "--no-capture-prompts and --capture-prompts require --agent claude-code. Other \
             agents may use their prompt hook to deliver handoff context, so removing it could \
             break cross-agent continuity."
        );
    }
    if args.apply {
        // #446: settle the capture failure mode before any agent-specific
        // work, and say which mode is in force. A protection the operator
        // cannot see is one they cannot trust.
        let capture_mode = persist_capture_mode(&config.data_dir, args.capture_mode)?;
        println!("capture mode: {capture_mode}");
        if capture_mode == "allowlist" {
            println!("  repositories without a .sessionmunch.toml marker emit no lifecycle events");
            // The gate runs inside the native hook binary (immediately before
            // the spool write) and, since #661, inside the generated
            // TypeScript integrations' shared `capturePolicy` (before any
            // POST). The only install left that reaches the server with no
            // gate at all is the raw script fallback: it POSTs directly and
            // never runs either enforcement point. Saying nothing would leave
            // an operator trusting a protection this install does not have —
            // the exact failure #446 is about.
            if !generated && !local_hook_policy_v1_supported() {
                println!(
                    "  WARNING: this install uses script hooks, which POST directly and \
                     cannot enforce the mode. Allowlist is stored but NOT in force here."
                );
            }
        }
        // Preserve a project-strategy an earlier `--apply` baked when this run
        // did not pass `--project-strategy`. Without this, a bare re-apply —
        // notably the auto-refresh in `sessionmunch upgrade` — re-renders the hook
        // commands with no strategy and silently reverts a `repo-root` install
        // to `basename`. An explicit `--project-strategy` (including `basename`)
        // is honored as-is, so an intentional downgrade still works. Resolving
        // into `args` here means every downstream renderer picks it up with no
        // per-agent plumbing.
        args.project_strategy = install_project_strategy(&args);
        return match args.agent {
            AgentChoice::OpenCode => {
                apply_to_opencode_plugin(&server_url, auth, &args, &capture_mode)
            }
            AgentChoice::OpenCode2 => {
                apply_to_opencode2_plugin(&server_url, auth, &args, &capture_mode)
            }
            AgentChoice::Pi => apply_to_pi_extension(&server_url, auth, &args, &capture_mode),
            AgentChoice::Omp => apply_to_omp_extension(&server_url, auth, &args, &capture_mode),
            AgentChoice::ClaudeCode => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_claude_code_settings(
                    &hooks_dir,
                    &server_url,
                    auth,
                    &config.data_dir,
                    &args,
                )
            }
            AgentChoice::Codex => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_codex_settings(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::CommandCode => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_command_code_settings(
                    &hooks_dir,
                    &server_url,
                    auth,
                    &config.data_dir,
                    &args,
                )
            }
            AgentChoice::Cursor => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_cursor_settings(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::GeminiCli => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_gemini_settings(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::AntigravityCli => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_antigravity_settings(
                    &hooks_dir,
                    &server_url,
                    auth,
                    &config.data_dir,
                    &args,
                )
            }
            AgentChoice::Grok => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_grok_settings(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::Zero => apply_to_zero_hooks(&server_url, auth, &config.data_dir, &args),
            AgentChoice::Zcode => apply_to_zcode_hooks(&server_url, auth, &config.data_dir, &args),
            AgentChoice::Devin => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_devin_settings(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::Openclaw => {
                openclaw_plugin::apply(&server_url, auth, &args, &capture_mode)
            }
            AgentChoice::KimiCode => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_kimi_code_config(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::KiroCli => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_kiro_cli_agent_configs(
                    &hooks_dir,
                    &server_url,
                    auth,
                    &config.data_dir,
                    &args,
                )
            }
            AgentChoice::KiroCliV3 => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_kiro_cli_v3_hooks(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::Pool => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_pool(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
        };
    }
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    // Preview must bake the same `CAPTURE_MODE` the next `--apply` would
    // persist (#661) — resolved read-only so a bare preview never writes.
    let preview_capture_mode = resolve_capture_mode(&config.data_dir, args.capture_mode);
    match args.agent {
        AgentChoice::OpenCode => {
            render_opencode_plugin(&server_url, auth, strategy, &preview_capture_mode)
        }
        AgentChoice::OpenCode2 => {
            render_opencode2_plugin(&server_url, auth, strategy, &preview_capture_mode)
        }
        AgentChoice::Pi => render_pi_extension(&server_url, auth, strategy, &preview_capture_mode),
        AgentChoice::Omp => render_omp_extension(
            &server_url,
            auth,
            strategy,
            args.profile.as_deref(),
            &preview_capture_mode,
        ),
        AgentChoice::ClaudeCode => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            let settings_path = match &args.config_file {
                Some(p) => p.clone(),
                None => claude_settings_path()?,
            };
            render_claude_code(
                &hooks_dir,
                &server_url,
                auth,
                &config.data_dir,
                strategy,
                &settings_path,
                ClaudeCaptureScope {
                    assistant: args.capture_assistant,
                    prompts: install_claude_prompt_capture(&args),
                },
            )
        }
        AgentChoice::Codex => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_agent(
                "codex",
                &hooks_dir,
                &server_url,
                auth,
                strategy,
                &[CODEX_PROFILE.events],
            )
        }
        AgentChoice::CommandCode => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_agent(
                "command-code",
                &hooks_dir,
                &server_url,
                auth,
                strategy,
                &[COMMAND_CODE_PROFILE.events],
            )
        }
        AgentChoice::Cursor => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_agent(
                "cursor",
                &hooks_dir,
                &server_url,
                auth,
                strategy,
                &[CURSOR_PROFILE.events],
            )
        }
        AgentChoice::GeminiCli => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_agent(
                "gemini-cli",
                &hooks_dir,
                &server_url,
                auth,
                strategy,
                &[GEMINI_PROFILE.events],
            )
        }
        AgentChoice::AntigravityCli => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_agent(
                "antigravity-cli",
                &hooks_dir,
                &server_url,
                auth,
                strategy,
                &[&ANTIGRAVITY_TOOL_EVENTS, &ANTIGRAVITY_LIFECYCLE_EVENTS],
            )
        }
        AgentChoice::Grok => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_grok(&hooks_dir, &server_url, auth, &config.data_dir, strategy)
        }
        AgentChoice::Zero => render_zero(&server_url, auth, &config.data_dir, strategy),
        AgentChoice::Zcode => render_zcode(&server_url, auth, &config.data_dir, strategy),
        AgentChoice::Devin => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_devin(&hooks_dir, &server_url, auth, &config.data_dir, strategy)
        }
        AgentChoice::Openclaw => {
            openclaw_plugin::render(&server_url, auth, strategy, &preview_capture_mode);
            Ok(())
        }
        AgentChoice::KimiCode => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_kimi_code(&hooks_dir, &server_url, auth, &config.data_dir, strategy)
        }
        AgentChoice::KiroCli => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_kiro_cli(&hooks_dir, &server_url, auth, &config.data_dir, strategy)
        }
        AgentChoice::KiroCliV3 => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_kiro_cli_v3(&hooks_dir, &server_url, auth, &config.data_dir, strategy)
        }
        AgentChoice::Pool => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_pool(&hooks_dir, &server_url, auth, &config.data_dir, strategy)
        }
    }
}

#[derive(Debug, Clone, Default)]
struct InferredMcpConfig {
    hook_server_url: Option<String>,
    auth_token: Option<String>,
}

/// The project-strategy to install for `args`: the explicit `--project-strategy`
/// when one was given, otherwise the strategy an earlier `--apply` baked into
/// the agent's existing config (so a bare re-apply preserves it instead of
/// reverting to basename). `None` means "bake nothing", unchanged from before.
fn install_project_strategy(args: &InstallHooksArgs) -> Option<ProjectStrategyArg> {
    if args.project_strategy.is_some() {
        return args.project_strategy;
    }
    // Kiro v2 can update several agent configs in one run. Each config may
    // carry a different previously-baked strategy, so recover it per target in
    // `apply_to_kiro_cli_agent_configs` instead of choosing one globally.
    if args.agent == AgentChoice::KiroCli {
        return None;
    }
    existing_agent_config(args)
        .as_deref()
        .and_then(|existing| baked_project_strategy(args.agent, existing))
}

/// The capture failure mode (#446) currently in force: an explicit
/// `--capture-mode` flag wins, otherwise the value stored under `data_dir`
/// from an earlier `--apply`, otherwise the historical `denylist` default.
/// Read-only — used by both [`persist_capture_mode`] and the print-only
/// preview path so a preview's baked `CAPTURE_MODE` matches what the next
/// `--apply` would actually persist (#661).
fn resolve_capture_mode(data_dir: &Path, requested: Option<CaptureModeArg>) -> String {
    let Some(requested) = requested else {
        let path = data_dir.join(crate::commands::hook::CAPTURE_MODE_FILE);
        return match fs::read_to_string(&path) {
            Ok(text) if text.trim().eq_ignore_ascii_case("allowlist") => "allowlist".to_string(),
            _ => "denylist".to_string(),
        };
    };
    match requested {
        CaptureModeArg::Allowlist => "allowlist".to_string(),
        CaptureModeArg::Denylist => "denylist".to_string(),
    }
}

/// Settle and persist the capture failure mode (#446), returning the mode now
/// in force.
///
/// An explicit flag writes the file. Omitting the flag *reads* the stored
/// value rather than defaulting to it, so a bare `--apply` — including the
/// auto-refresh inside `sessionmunch upgrade` — can never quietly downgrade an
/// existing opt-in back to capture-by-default.
fn persist_capture_mode(data_dir: &Path, requested: Option<CaptureModeArg>) -> Result<String> {
    let value = resolve_capture_mode(data_dir, requested);
    if requested.is_some() {
        let path = data_dir.join(crate::commands::hook::CAPTURE_MODE_FILE);
        fs::create_dir_all(data_dir)
            .with_context(|| format!("creating data dir {}", data_dir.display()))?;
        // Atomically, because every reader maps an unrecognised value onto
        // `denylist`: a torn write would not corrupt the opt-in, it would
        // silently revert it to capture-by-default (`CONTRIBUTING.md`,
        // "Atomic file writes only").
        sessionmunch_wiki::write_atomic(&path, format!("{value}\n").as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(value)
}

/// Whether the Claude Code install should include its prompt-capture hook.
/// Explicit flags win. A bare apply preserves the state of an existing
/// sessionmunch install so `upgrade` cannot silently restore a privacy opt-out.
fn install_claude_prompt_capture(args: &InstallHooksArgs) -> bool {
    if args.no_capture_prompts {
        return false;
    }
    if args.capture_prompts || !args.apply {
        return true;
    }
    existing_agent_config(args)
        .as_deref()
        .and_then(baked_claude_prompt_capture)
        .unwrap_or(true)
}

/// Recover prompt-capture state only from hook entries owned by sessionmunch.
/// `None` means this is not an existing sessionmunch Claude Code install.
fn baked_claude_prompt_capture(existing: &str) -> Option<bool> {
    let document: serde_json::Value = serde_json::from_str(existing).ok()?;
    let hooks = document.get("hooks")?.as_object()?;
    let has_sessionmunch_hooks = hooks.values().any(|value| {
        value
            .as_array()
            .is_some_and(|entries| entries.iter().any(is_sessionmunch_hook_entry))
    });
    if !has_sessionmunch_hooks {
        return None;
    }
    Some(
        hooks
            .get(CLAUDE_PROMPT_EVENT)
            .and_then(serde_json::Value::as_array)
            .is_some_and(|entries| entries.iter().any(is_sessionmunch_hook_entry)),
    )
}

/// Read the config file `--apply` will update for the selected agent.
fn existing_agent_config(args: &InstallHooksArgs) -> Option<String> {
    let path = if let Some(path) = &args.config_file {
        if args.agent == AgentChoice::Openclaw {
            path.join(openclaw_plugin::ENTRYPOINT_TS)
        } else {
            path.clone()
        }
    } else {
        match args.agent {
            AgentChoice::ClaudeCode => claude_settings_path().ok()?,
            AgentChoice::Codex => codex_hooks_path().ok()?,
            AgentChoice::CommandCode => command_code_settings_path().ok()?,
            AgentChoice::Cursor => cursor_hooks_path().ok()?,
            AgentChoice::GeminiCli => gemini_settings_path().ok()?,
            AgentChoice::OpenCode => opencode_plugin_path().ok()?,
            AgentChoice::OpenCode2 => opencode2_plugin_path().ok()?,
            AgentChoice::Pi => pi_extension_path().ok()?,
            AgentChoice::Omp => omp_extension_path(args.profile.as_deref()).ok()?,
            AgentChoice::Openclaw => openclaw_plugin::default_plugin_dir()
                .ok()?
                .join(openclaw_plugin::ENTRYPOINT_TS),
            AgentChoice::AntigravityCli => antigravity_hooks_path().ok()?,
            AgentChoice::Grok => grok_hooks_path().ok()?,
            AgentChoice::Zero => zero_hooks_path().ok()?,
            AgentChoice::Zcode => zcode_config_path().ok()?,
            AgentChoice::Devin => devin_hooks_path().ok()?,
            AgentChoice::KimiCode => kimi_code_config_path().ok()?,
            AgentChoice::KiroCli => return None,
            // Pool's hook config is per-repo (`.poolside/settings.yaml`), not a
            // user-global file the installer could re-read a baked strategy from.
            AgentChoice::Pool => return None,
            AgentChoice::KiroCliV3 => kiro_cli_v3_hooks_path().ok()?,
        }
    };
    std::fs::read_to_string(path).ok()
}

/// Recover a strategy only from configuration entries sessionmunch owns. Shared
/// JSON/TOML config files may contain unrelated hooks, while generated
/// TypeScript files carry an explicit ownership header.
fn baked_project_strategy(agent: AgentChoice, existing: &str) -> Option<ProjectStrategyArg> {
    match agent {
        AgentChoice::OpenCode
        | AgentChoice::OpenCode2
        | AgentChoice::Pi
        | AgentChoice::Omp
        | AgentChoice::Openclaw => {
            let marker = match agent {
                AgentChoice::OpenCode => "--agent opencode --apply`.",
                AgentChoice::OpenCode2 => "--agent opencode2 --apply`.",
                AgentChoice::Pi => "--agent pi --apply`.",
                AgentChoice::Omp => "--agent omp --apply`.",
                AgentChoice::Openclaw => "--agent openclaw --apply`.",
                _ => return None,
            };
            existing
                .lines()
                .next()
                .filter(|line| {
                    line.starts_with("// Auto-generated by `sessionmunch install-hooks ")
                        && line.ends_with(marker)
                })
                .and_then(|_| project_strategy_from_text(existing))
        }
        AgentChoice::KimiCode => {
            let document: toml_edit::DocumentMut = existing.parse().ok()?;
            document
                .get("hooks")
                .and_then(toml_edit::Item::as_array_of_tables)
                .and_then(|hooks| {
                    hooks.iter().find_map(|entry| {
                        if !is_sessionmunch_toml_hook_entry(entry) {
                            return None;
                        }
                        entry
                            .get("command")
                            .and_then(|item| item.as_str())
                            .and_then(project_strategy_from_text)
                    })
                })
        }
        AgentChoice::KiroCliV3 => {
            let root: serde_json::Value = serde_json::from_str(existing).ok()?;
            root.get("hooks")
                .and_then(serde_json::Value::as_array)
                .and_then(|hooks| {
                    hooks.iter().find_map(|entry| {
                        is_sessionmunch_kiro_v3_hook_entry(entry)
                            .then(|| {
                                entry
                                    .pointer("/action/command")
                                    .and_then(serde_json::Value::as_str)
                                    .and_then(project_strategy_from_text)
                            })
                            .flatten()
                    })
                })
        }
        _ => serde_json::from_str(existing)
            .ok()
            .as_ref()
            .and_then(project_strategy_from_json),
    }
}

fn project_strategy_from_json(value: &serde_json::Value) -> Option<ProjectStrategyArg> {
    if is_sessionmunch_hook_entry(value) {
        if let Some(strategy) = value
            .get("command")
            .and_then(|command| command.as_str())
            .and_then(project_strategy_from_text)
        {
            return Some(strategy);
        }
        if let Some(args) = value.get("args").and_then(|args| args.as_array()) {
            let args: Vec<&str> = args.iter().filter_map(|arg| arg.as_str()).collect();
            for (index, arg) in args.iter().enumerate() {
                if let Some(value) = arg.strip_prefix("--project-strategy=")
                    && is_repo_root_strategy(value)
                {
                    return Some(ProjectStrategyArg::RepoRoot);
                }
                if *arg == "--project-strategy"
                    && args
                        .get(index + 1)
                        .is_some_and(|value| is_repo_root_strategy(value))
                {
                    return Some(ProjectStrategyArg::RepoRoot);
                }
            }
        }
    }

    match value {
        serde_json::Value::Array(values) => values.iter().find_map(project_strategy_from_json),
        serde_json::Value::Object(values) => values.values().find_map(project_strategy_from_json),
        _ => None,
    }
}

/// Only `repo-root` is baked (`basename` removes the marker), so it is the sole
/// value recovered from legacy shell commands and generated source.
fn project_strategy_from_text(existing: &str) -> Option<ProjectStrategyArg> {
    for marker in [
        "SESSIONMUNCH_PROJECT_STRATEGY=",
        "--project-strategy=",
        "--project-strategy ",
        "const DEFAULT_PROJECT_STRATEGY =",
    ] {
        for rest in existing.split(marker).skip(1) {
            let token: String = rest
                .trim_start_matches(|character: char| {
                    character.is_ascii_whitespace() || matches!(character, '\'' | '"')
                })
                .chars()
                .take_while(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                })
                .collect();
            if is_repo_root_strategy(&token) {
                return Some(ProjectStrategyArg::RepoRoot);
            }
        }
    }
    None
}

fn is_repo_root_strategy(value: &str) -> bool {
    matches!(value, "repo-root" | "repo_root")
}

/// Reject `--as-user X` without a usable `--auth-token`. P1.8
/// metadata flag — without a token, the hook scripts would still
/// authenticate anonymously (or as root if the operator reused the
/// config bearer), so the `--as-user X` label would be misleading.
/// Trims whitespace; empty / whitespace-only `--as-user` is treated
/// as not-set so an accidental `--as-user ""` doesn't bail.
///
/// # Errors
/// Returns an error when `as_user` is set but `auth_token` is `None`
/// (or whitespace-only). The error message names the user so
/// operators see which arg they meant to pair with `--auth-token`.
fn validate_as_user(as_user: Option<&str>, auth_token: Option<&str>) -> Result<()> {
    let Some(user) = as_user.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    if auth_token.map(str::trim).is_none_or(str::is_empty) {
        anyhow::bail!(
            "--as-user '{user}' requires --auth-token \
             (issue one with `sessionmunch api-key add --username {user} --label <label>`)"
        );
    }
    Ok(())
}

fn effective_hook_server_url(
    config: &Config,
    args: &InstallHooksArgs,
    inferred: Option<&InferredMcpConfig>,
) -> String {
    let raw = if let Some(url) = &args.server_url {
        url.clone()
    } else if config.server_url_configured() {
        config.server_url.clone()
    } else if let Some(url) = inferred.and_then(|mcp| mcp.hook_server_url.clone()) {
        url
    } else {
        return DEFAULT_SERVER_URL.to_string();
    };
    apply_base_path_to_hook_url(&normalise_hook_server_url(&raw), &config.base_path)
}

fn normalise_hook_server_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// Thread `Config::base_path` into the URL baked into hook commands so
/// the native hook subcommand (`sessionmunch hook`) and the POSIX
/// `.sh`/`.ps1` scripts POST to `<origin><base>/hook` instead of
/// 404'ing under a reverse proxy.
///
/// Skip when the resolved URL already carries a path component — that
/// means the operator put the prefix into `SESSIONMUNCH_SERVER_URL`
/// directly (`http://host:49374/wiki`) and we'd double it otherwise.
fn apply_base_path_to_hook_url(url: &str, base_path: &str) -> String {
    let (origin, existing_path) = crate::http_client::split_origin_and_path(url);
    if !existing_path.is_empty() {
        return url.to_string();
    }
    let prefix = sessionmunch_web::normalize_prefix(base_path);
    if prefix.is_empty() {
        origin
    } else {
        format!("{origin}{prefix}")
    }
}

fn infer_installed_mcp_config(agent: AgentChoice) -> Result<Option<InferredMcpConfig>> {
    if agent == AgentChoice::Grok {
        let cwd = std::env::current_dir()
            .context("could not resolve current dir for Grok project configuration")?;
        let repo_root = sessionmunch_consolidate::discover_repo_root(&cwd).ok();
        if let Some(project_config) = find_grok_project_overlay(&cwd, repo_root.as_deref()) {
            anyhow::bail!(
                "Grok project configuration {} may override GROK_HOME; pass explicit --server-url and --auth-token rather than inferring MCP settings",
                project_config.display()
            );
        }
    }
    let Some(client) = mcp_client_for_agent(agent) else {
        return Ok(None);
    };
    let path = install_mcp::mcp_config_path(client)?;
    let Ok(content) = fs::read_to_string(path) else {
        return Ok(None);
    };
    match client {
        McpClient::ClaudeCode => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "sessionmunch"],
            "url",
        )),
        // Codex uses `http_headers`; Grok uses `headers`. The shared TOML
        // inferencer accepts both.
        McpClient::Codex => Ok(infer_toml_mcp_config(&content)),
        McpClient::CommandCode => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "sessionmunch"],
            "url",
        )),
        McpClient::Grok => infer_grok_mcp_config(&content),
        McpClient::OpenCode => Ok(infer_json_mcp_config(
            &content,
            &["mcp", "sessionmunch"],
            "url",
        )),
        McpClient::OpenCode2 => Ok(infer_json_mcp_config(
            &content,
            &["mcp", "servers", "sessionmunch"],
            "url",
        )),
        McpClient::Cursor => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "sessionmunch"],
            "url",
        )),
        McpClient::GeminiCli => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "sessionmunch"],
            "httpUrl",
        )),
        McpClient::Openclaw => Ok(infer_json_mcp_config(
            &content,
            &["mcp", "servers", "sessionmunch"],
            "url",
        )),
        McpClient::Omp => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "sessionmunch"],
            "url",
        )),
        McpClient::Zero => Ok(infer_json_mcp_config(
            &content,
            &["mcp", "servers", "sessionmunch"],
            "url",
        )),
        McpClient::Zcode => Ok(infer_json_mcp_config(
            &content,
            &["mcp", "servers", "sessionmunch"],
            "url",
        )),
        McpClient::Pi => Ok(None),
        McpClient::AntigravityCli => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "sessionmunch"],
            "serverUrl",
        )),
        McpClient::Devin => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "sessionmunch"],
            "url",
        )),
        McpClient::KimiCode | McpClient::KiroCli => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "sessionmunch"],
            "url",
        )),
        McpClient::ClaudeDesktop => Ok(None),
        // MCP-only client: no AgentChoice counterpart routes here.
        McpClient::Swival => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "sessionmunch"],
            "url",
        )),
        // MCP-only client: no AgentChoice counterpart routes here.
        // Reachable only if a future install_hooks flow targets VS
        // Code Copilot directly.
        McpClient::VsCodeCopilot => Ok(infer_json_mcp_config(
            &content,
            &["servers", "sessionmunch"],
            "url",
        )),
        // MCP-only client: no AgentChoice counterpart routes here.
        McpClient::Zed => Ok(infer_json_mcp_config(
            &content,
            &["context_servers", "sessionmunch"],
            "url",
        )),
        // MCP-only client: no AgentChoice counterpart routes here. Muse
        // Code's hook surface is documented, but its SessionStart output
        // contract is not, so no lifecycle integration claims it yet.
        McpClient::Muse => Ok(infer_json_mcp_config(
            &content,
            &["mcp_servers", "sessionmunch"],
            "url",
        )),
    }
}

/// Grok loads project settings from every directory between the repository
/// root and CWD. Outside a repository, only the active directory is relevant.
fn grok_project_overlay_paths(cwd: &Path, repo_root: Option<&Path>) -> Vec<PathBuf> {
    let start = repo_root
        .filter(|root| cwd.starts_with(root))
        .unwrap_or(cwd);
    let mut directories = Vec::new();
    let mut current = cwd;
    loop {
        directories.push(current.to_path_buf());
        if current == start {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    directories.reverse();
    directories
        .into_iter()
        .map(|directory| directory.join(".grok").join("config.toml"))
        .collect()
}

fn find_grok_project_overlay(cwd: &Path, repo_root: Option<&Path>) -> Option<PathBuf> {
    grok_project_overlay_paths(cwd, repo_root)
        .into_iter()
        .find(|path| path.exists())
}

fn mcp_client_for_agent(agent: AgentChoice) -> Option<McpClient> {
    match agent {
        AgentChoice::ClaudeCode => Some(McpClient::ClaudeCode),
        AgentChoice::Codex => Some(McpClient::Codex),
        AgentChoice::CommandCode => Some(McpClient::CommandCode),
        AgentChoice::Cursor => Some(McpClient::Cursor),
        AgentChoice::GeminiCli => Some(McpClient::GeminiCli),
        AgentChoice::OpenCode => Some(McpClient::OpenCode),
        AgentChoice::OpenCode2 => Some(McpClient::OpenCode2),
        AgentChoice::Omp => Some(McpClient::Omp),
        AgentChoice::Openclaw => Some(McpClient::Openclaw),
        AgentChoice::AntigravityCli => Some(McpClient::AntigravityCli),
        AgentChoice::Zero => Some(McpClient::Zero),
        AgentChoice::Grok => Some(McpClient::Grok),
        AgentChoice::Devin => Some(McpClient::Devin),
        AgentChoice::KimiCode => Some(McpClient::KimiCode),
        // Pi bridges MCP through its generated extension, not a native
        // mcp.json the installer can scrape.
        AgentChoice::Pi => None,
        AgentChoice::KiroCli | AgentChoice::KiroCliV3 => Some(McpClient::KiroCli),
        // No first-party Pool MCP installer ships yet, so there is no
        // config file to infer a server URL or token from.
        AgentChoice::Pool => None,
        // The ZCode MCP client ships with #511; until that lands there is
        // no `McpClient::Zcode` whose config the installer could scrape a
        // server URL or token from.
        AgentChoice::Zcode => None,
    }
}

fn infer_json_mcp_config(
    content: &str,
    entry_path: &[&str],
    url_key: &str,
) -> Option<InferredMcpConfig> {
    let root: serde_json::Value = serde_json::from_str(content).ok()?;
    let mut entry = &root;
    for key in entry_path {
        entry = entry.get(*key)?;
    }
    let hook_server_url = entry
        .get(url_key)
        .and_then(|v| v.as_str())
        .and_then(hook_server_url_from_mcp_url);
    let auth_token = entry
        .get("headers")
        .and_then(|headers| headers.get("Authorization"))
        .and_then(|v| v.as_str())
        .and_then(bearer_token_from_header);
    Some(InferredMcpConfig {
        hook_server_url,
        auth_token,
    })
}

/// Infer hook server URL + bearer from a TOML `[mcp_servers.sessionmunch]`
/// entry (Codex `http_headers` or Grok `headers`).
fn infer_toml_mcp_config(content: &str) -> Option<InferredMcpConfig> {
    let doc: toml_edit::DocumentMut = content.parse().ok()?;
    // `toml_edit::Item`'s `Index` impl panics on missing keys, so this
    // walks the table chain with `.get()` instead. A user with
    // `[mcp_servers.context7]` but no `[mcp_servers.sessionmunch]` is a
    // perfectly valid hooks-only Codex/Grok setup (issue #53) — return None
    // rather than abort the whole install with a stack trace.
    let server = doc.get("mcp_servers")?.get("sessionmunch")?;

    let hook_server_url = server
        .get("url")
        .and_then(|v| v.as_str())
        .and_then(hook_server_url_from_mcp_url);
    let auth_token = server
        .get("http_headers")
        .and_then(|h| h.get("Authorization"))
        .or_else(|| server.get("headers").and_then(|h| h.get("Authorization")))
        .and_then(|v| v.as_str())
        .and_then(bearer_token_from_header);
    if hook_server_url.is_none() && auth_token.is_none() {
        return None;
    }
    Some(InferredMcpConfig {
        hook_server_url,
        auth_token,
    })
}

/// Infer Grok's TOML entry after resolving its supported `${VAR}` and
/// `${VAR:-default}` placeholders. Refuse missing variables so generated hook
/// commands never contain a literal placeholder.
fn infer_grok_mcp_config(content: &str) -> Result<Option<InferredMcpConfig>> {
    let doc: toml_edit::DocumentMut = match content.parse() {
        Ok(doc) => doc,
        Err(_) => return Ok(None),
    };
    let Some(server) = doc
        .get("mcp_servers")
        .and_then(|servers| servers.get("sessionmunch"))
    else {
        return Ok(None);
    };
    let hook_server_url = server
        .get("url")
        .and_then(|value| value.as_str())
        .map(expand_grok_placeholders)
        .transpose()?
        .as_deref()
        .and_then(hook_server_url_from_mcp_url);
    let auth_token = server
        .get("headers")
        .and_then(|headers| headers.get("Authorization"))
        .and_then(|value| value.as_str())
        .map(expand_grok_placeholders)
        .transpose()?
        .as_deref()
        .and_then(bearer_token_from_header);
    if hook_server_url.is_none() && auth_token.is_none() {
        return Ok(None);
    }
    Ok(Some(InferredMcpConfig {
        hook_server_url,
        auth_token,
    }))
}

fn expand_grok_placeholders(value: &str) -> Result<String> {
    expand_grok_placeholders_with(value, |name| std::env::var(name).ok())
}

fn expand_grok_placeholders_with(
    value: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<String> {
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some(start) = remaining.find("${") {
        output.push_str(&remaining[..start]);
        let after_start = &remaining[start + 2..];
        let end = after_start
            .find('}')
            .context("unterminated Grok ${...} placeholder")?;
        let expression = &after_start[..end];
        let (name, default) = expression
            .split_once(":-")
            .map_or((expression, None), |(name, default)| (name, Some(default)));
        if name.is_empty()
            || !name.chars().enumerate().all(|(index, ch)| {
                ch == '_' || ch.is_ascii_alphanumeric() && (index > 0 || ch.is_ascii_alphabetic())
            })
        {
            anyhow::bail!("invalid Grok environment placeholder `${{{expression}}}`");
        }
        let replacement = lookup(name)
            .filter(|value| !value.is_empty())
            .or_else(|| default.map(ToOwned::to_owned))
            .with_context(|| format!("Grok MCP placeholder `${{{name}}}` is unset; pass explicit --server-url and --auth-token"))?;
        output.push_str(&replacement);
        remaining = &after_start[end + 1..];
    }
    output.push_str(remaining);
    Ok(output)
}

fn hook_server_url_from_mcp_url(url: &str) -> Option<String> {
    // Drop query/fragment BEFORE the `/mcp` peel: Kimi Code's
    // `?flavor=moonshot` marker must never leak into hook URLs (and it
    // would stop the suffix from matching).
    let base = url
        .trim()
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    Some(base.strip_suffix("/mcp").unwrap_or(base).to_string())
}

fn bearer_token_from_header(header: &str) -> Option<String> {
    header
        .trim()
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// True if a hook-array entry belongs to sessionmunch — i.e. some command handler
/// inside it is one of our legacy command strings or one of our exec-form native
/// hook handlers. Used to replace our own entries on re-apply while preserving
/// hooks that other tools registered under the same event.
fn is_sessionmunch_hook_entry(entry: &serde_json::Value) -> bool {
    fn mentions_sessionmunch(value: &serde_json::Value) -> bool {
        let Some(command) = value.get("command").and_then(|c| c.as_str()) else {
            return false;
        };
        let lower = command.to_ascii_lowercase();
        let args = value.get("args").and_then(|a| a.as_array());
        let Some(args) = args else {
            return hook_command_is_ours(command);
        };
        let tokens: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
        // Exec form: require both an sessionmunch-ish executable and our hook argv
        // signature so unrelated helpers such as `sessionmunch-helper.exe` are not
        // removed just because their executable name contains sessionmunch.
        (lower.contains("sessionmunch") || lower.contains("sessionmunch"))
            && tokens.contains(&"hook")
            && tokens.contains(&"--event")
            && tokens.contains(&"--agent")
            && tokens.contains(&"--server-url")
    }
    // Flat shape (Cursor): `{ "type":"command", "command":"…" }`.
    // Nested shape (Claude Code / Codex / Gemini):
    // `{ "matcher":"", "hooks":[ {"command":"…"} ] }`.
    mentions_sessionmunch(entry)
        || entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .is_some_and(|inner| inner.iter().any(mentions_sessionmunch))
}

/// Overlay our hook entries for one event onto the user's existing array
/// for that event: drop any prior sessionmunch entries (so re-running
/// `install-hooks` never duplicates them) and append ours, while keeping
/// every third-party hook registered under the same event. Replaces a
/// blind `map.insert(event, value)`, which discarded co-located hooks
/// from other tools (e.g. a context-mode SessionStart hook).
fn overlay_event_hooks(
    map: &mut serde_json::Map<String, serde_json::Value>,
    event: &str,
    our_value: &serde_json::Value,
) {
    let mut entries: Vec<serde_json::Value> = map
        .get(event)
        .and_then(|v| v.as_array())
        .map(|existing| {
            existing
                .iter()
                .filter(|e| !is_sessionmunch_hook_entry(e))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if let Some(ours) = our_value.as_array() {
        entries.extend(ours.iter().cloned());
    }
    map.insert(event.to_string(), serde_json::Value::Array(entries));
}

/// Remove only sessionmunch's entries for one event. Delete the event key when
/// no third-party hooks remain so a rendered opt-out stays minimal.
fn remove_sessionmunch_event_hooks(
    map: &mut serde_json::Map<String, serde_json::Value>,
    event: &str,
) {
    overlay_event_hooks(map, event, &serde_json::Value::Array(Vec::new()));
    if map
        .get(event)
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        map.remove(event);
    }
}

fn overlay_kiro_cli_event_hooks(
    map: &mut serde_json::Map<String, serde_json::Value>,
    event: &str,
    our_value: &serde_json::Value,
) {
    let mut entries: Vec<serde_json::Value> = map
        .get(event)
        .and_then(|value| value.as_array())
        .map(|existing| {
            existing
                .iter()
                .filter(|entry| {
                    !entry
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(is_sessionmunch_kiro_hook_command)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if let Some(ours) = our_value.as_array() {
        entries.extend(ours.iter().cloned());
    }
    map.insert(event.to_string(), serde_json::Value::Array(entries));
}

/// Mutate `~/.claude/settings.json` in place: replace the hook entries
/// sessionmunch cares about (`CLAUDE_CODE_EVENTS`); preserve every other hook the
/// user has wired up to other tools.
/// Whether `--capture-assistant` may take effect for this agent + platform
/// (#196): Claude Code on a native hook platform only. Any other agent or a
/// script-fallback platform cannot honor the opt-in, so the installer bails
/// instead of enabling it silently.
fn capture_assistant_allowed(agent: AgentChoice) -> bool {
    matches!(agent, AgentChoice::ClaudeCode) && local_hook_policy_v1_supported()
}

fn prompt_capture_options_allowed(agent: AgentChoice) -> bool {
    agent == AgentChoice::ClaudeCode
}

fn configure_claude_prompt_capture(
    mut payload: serde_json::Value,
    capture_prompts: bool,
) -> serde_json::Value {
    if !capture_prompts
        && let Some(hooks) = payload
            .get_mut("hooks")
            .and_then(serde_json::Value::as_object_mut)
    {
        hooks.remove(CLAUDE_PROMPT_EVENT);
    }
    payload
}

fn apply_to_claude_code_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts(hooks_dir, "claude-code", data_dir)?;
    apply_to_claude_code_settings_with_staged(&staged, server_url, auth_token, data_dir, args)
}

#[cfg(test)]
fn apply_to_claude_code_settings_in(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    staging_data_local: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts_in(hooks_dir, "claude-code", staging_data_local)?;
    let command_dir = staged_command_dir(&staged, "claude-code");
    let capture_prompts = install_claude_prompt_capture(args);
    let payload = configure_claude_prompt_capture(
        crate::commands::render_shared::build_claude_code_script_payload_for_test(
            &command_dir,
            server_url,
            auth_token,
            Some(data_dir),
            args.project_strategy.and_then(ProjectStrategyArg::baked),
            args.capture_assistant,
        ),
        capture_prompts,
    );
    apply_to_claude_code_settings_with_payload(payload, args, capture_prompts)
}

fn apply_to_claude_code_settings_with_staged(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let command_dir = staged_command_dir(staged, "claude-code");
    let capture_prompts = install_claude_prompt_capture(args);
    let payload = configure_claude_prompt_capture(
        build_claude_code_payload_with_data_dir(
            &command_dir,
            server_url,
            auth_token,
            Some(data_dir),
            args.project_strategy.and_then(ProjectStrategyArg::baked),
            args.capture_assistant,
        ),
        capture_prompts,
    );
    apply_to_claude_code_settings_with_payload(payload, args, capture_prompts)
}

fn apply_to_claude_code_settings_with_payload(
    payload: serde_json::Value,
    args: &InstallHooksArgs,
    capture_prompts: bool,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => claude_settings_path()?,
    };
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_object())
        .context("internal: build_claude_code_payload didn't return a hooks object")?
        .clone();
    let outcome = apply_atomic(&path, |existing| {
        mutate_json(existing, |root| {
            // Get-or-create the top-level `hooks` table, then merge our
            // event keys in via `overlay_event_hooks`: our entries replace
            // any prior sessionmunch entries, while hooks the user (or another
            // tool) wired under the same event — or under a non-overlapping
            // event name (e.g. a hand-written "Notification" hook) — survive.
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`hooks` is present in settings.json but not an object")?;
            for (event, value) in &our_hooks {
                overlay_event_hooks(hooks, event, value);
            }
            if !capture_prompts {
                remove_sessionmunch_event_hooks(hooks, CLAUDE_PROMPT_EVENT);
            }
            Ok(())
        })
    })?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

/// Mutate `~/.grok/hooks/sessionmunch.json` so Grok Build CLI fires the sessionmunch
/// lifecycle hooks. Grok's hook config is structurally identical to Claude
/// Code's nested hook JSON and uses the same CamelCase event names, but
/// its script bundle carries `agent=grok` and skips destructive SessionStart
/// handoff fetches. We merge into a dedicated `sessionmunch.json` (Grok discovers
/// every `~/.grok/hooks/*.json`), so a pre-existing third-party hook file is
/// left untouched.
fn apply_to_grok_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => grok_hooks_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "grok", data_dir)?;
    let command_dir = staged_command_dir(&staged, "grok");
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let payload = build_grok_payload_with_data_dir(
        &command_dir,
        server_url,
        auth_token,
        Some(data_dir),
        strategy,
    );
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_object())
        .context("internal: build_grok_payload didn't return a hooks object")?
        .clone();
    let outcome = apply_atomic(&path, |existing| {
        mutate_json(existing, |root| {
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`hooks` is present in sessionmunch.json but not an object")?;
            for (event, value) in &our_hooks {
                overlay_event_hooks(hooks, event, value);
            }
            Ok(())
        })
    })?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

/// Mutate Devin's hook config (either `~/.devin/hooks.v1.json` or `~/.devin/config.json` hooks key).
/// Devin uses the same nested hook JSON shape as Claude Code/Grok, but with DEVIN_EVENTS
/// (PostCompaction instead of PreCompact, no subagent events). SessionStart injects
/// the handoff via hookSpecificOutput.additionalContext.
fn apply_to_devin_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts(hooks_dir, "devin", data_dir)?;
    apply_to_devin_settings_with_staged(&staged, server_url, auth_token, data_dir, args)
}

#[cfg(test)]
fn apply_to_devin_settings_in(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    staging_data_local: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts_in(hooks_dir, "devin", staging_data_local)?;
    apply_to_devin_settings_with_staged(&staged, server_url, auth_token, data_dir, args)
}

fn apply_to_devin_settings_with_staged(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => devin_hooks_path()?,
    };
    let command_dir = staged_command_dir(staged, "devin");
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let payload = build_devin_payload_with_data_dir(
        &command_dir,
        server_url,
        auth_token,
        Some(data_dir),
        strategy,
    );
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_object())
        .context("internal: build_devin_payload didn't return a hooks object")?
        .clone();
    let outcome = apply_atomic(&path, |existing| {
        // For hooks.v1.json, the entire file IS the hooks object
        // For config.json hooks key, we merge into the hooks object
        if path.file_name() == Some("hooks.v1.json".as_ref()) {
            mutate_json(existing, |root| {
                for (event, value) in &our_hooks {
                    overlay_event_hooks(root, event, value);
                }
                Ok(())
            })
        } else {
            mutate_json(existing, |root| {
                let hooks = root
                    .entry("hooks")
                    .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                    .as_object_mut()
                    .context("`hooks` is present in config.json but not an object")?;
                for (event, value) in &our_hooks {
                    overlay_event_hooks(hooks, event, value);
                }
                Ok(())
            })
        }
    })?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

/// Mutate `~/.commandcode/settings.json` so Command Code fires the stable
/// four-event lifecycle integration. Command Code's schema requires the outer
/// `matcher` to be omitted for SessionStart and Stop, so its profile must not
/// reuse the otherwise similar Claude/Codex payload byte-for-byte.
fn apply_to_command_code_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts(hooks_dir, "command-code", data_dir)?;
    apply_to_command_code_settings_with_staged(&staged, server_url, auth_token, data_dir, args)
}

#[cfg(test)]
fn apply_to_command_code_settings_in(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    staging_data_local: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts_in(hooks_dir, "command-code", staging_data_local)?;
    let command_dir = staged_command_dir(&staged, "command-code");
    let payload = crate::commands::render_shared::build_profile_script_payload_for_test(
        &COMMAND_CODE_PROFILE,
        &command_dir,
        server_url,
        auth_token,
        "command-code",
        Some(data_dir),
        args.project_strategy.and_then(ProjectStrategyArg::baked),
    );
    apply_to_command_code_settings_with_payload(payload, args)
}

fn apply_to_command_code_settings_with_staged(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let command_dir = staged_command_dir(staged, "command-code");
    let payload = build_profile_payload_for_agent(
        &COMMAND_CODE_PROFILE,
        &command_dir,
        server_url,
        auth_token,
        "command-code",
        Some(data_dir),
        args.project_strategy.and_then(ProjectStrategyArg::baked),
    );
    apply_to_command_code_settings_with_payload(payload, args)
}

fn apply_to_command_code_settings_with_payload(
    payload: serde_json::Value,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(path) => path.clone(),
        None => command_code_settings_path()?,
    };
    let our_hooks = payload
        .get("hooks")
        .and_then(serde_json::Value::as_object)
        .context("internal: Command Code payload did not return a hooks object")?
        .clone();
    let outcome = apply_atomic(&path, |existing| {
        mutate_json(existing, |root| {
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`hooks` is present in settings.json but not an object")?;
            for (event, value) in &our_hooks {
                overlay_event_hooks(hooks, event, value);
            }
            Ok(())
        })
    })?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

/// Mutate `~/.codex/hooks.json` (creating it if absent) so Codex's
/// lifecycle hook runner fires the sessionmunch scripts on every
/// session/prompt/tool event.
///
/// Codex's hook config is structurally identical to Claude Code's
/// (verified against `openai/codex/codex-rs/config/src/hooks_tests.rs`):
///
///   { "hooks": {
///       "SessionStart": [
///         { "matcher": "",
///           "hooks": [ {"type":"command", "command":"..."} ]
///         }
///       ], ...
///   } }
///
/// Codex looks for hooks in `~/.codex/hooks.json` by default (or
/// wherever `hooks = "./relative-path.json"` in config.toml points).
/// We write the standalone file and don't touch config.toml — Codex
/// picks it up automatically.
///
/// Trust note: Codex refuses to RUN new hooks until the user accepts
/// them in the TUI ("Trust all and continue") or sets
/// `--dangerously-bypass-hook-trust`. We print a reminder.
fn apply_to_codex_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts(hooks_dir, "codex", data_dir)?;
    apply_to_codex_settings_with_staged(&staged, server_url, auth_token, data_dir, args)
}

#[cfg(test)]
fn apply_to_codex_settings_in(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    staging_data_local: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts_in(hooks_dir, "codex", staging_data_local)?;
    let command_dir = staged_command_dir(&staged, "codex");
    let payload = crate::commands::render_shared::build_profile_script_payload_for_test(
        &super::render_shared::CODEX_PROFILE,
        &command_dir,
        server_url,
        auth_token,
        "codex",
        Some(data_dir),
        args.project_strategy.and_then(ProjectStrategyArg::baked),
    );
    apply_to_codex_settings_with_payload(payload, args)
}

fn apply_to_codex_settings_with_staged(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let command_dir = staged_command_dir(staged, "codex");
    let payload = build_profile_payload_for_agent(
        &super::render_shared::CODEX_PROFILE,
        &command_dir,
        server_url,
        auth_token,
        "codex",
        Some(data_dir),
        args.project_strategy.and_then(ProjectStrategyArg::baked),
    );
    apply_to_codex_settings_with_payload(payload, args)
}

fn apply_to_codex_settings_with_payload(
    payload: serde_json::Value,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => codex_hooks_path()?,
    };
    let outcome = merge_codex_payload(payload, &path)?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    // First-time trust reminder. Codex's TUI flags new/changed
    // hooks on startup; users must explicitly trust them before
    // they fire.
    if !matches!(outcome, ApplyOutcome::NoOp) {
        println!();
        println!("Codex requires explicit trust for new hooks. Next time you start `codex`:");
        println!("  → the TUI will surface 'Hooks need review' for each new event");
        println!("  → choose 'Trust all and continue' (or trust individually)");
        println!("To bypass the prompt for automated installs, start with");
        println!("`codex --dangerously-bypass-hook-trust` (review hook scripts first).");
    }
    Ok(())
}

#[cfg(test)]
fn merge_codex_hooks(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<ApplyOutcome> {
    // Build the Codex-flavoured payload. The JSON shape is identical
    // to Claude Code's matcher + nested hooks form — the event list
    // differs only by the Claude-Code-only subagent events.
    let payload = build_profile_payload_for_agent(
        &super::render_shared::CODEX_PROFILE,
        staged,
        server_url,
        auth_token,
        "codex",
        Some(data_dir),
        project_strategy,
    );
    merge_codex_payload(payload, config_path)
}

fn merge_codex_payload(payload: serde_json::Value, config_path: &Path) -> Result<ApplyOutcome> {
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_object())
        .context("internal: payload builder didn't return a hooks object")?
        .clone();
    apply_atomic(config_path, |existing| {
        mutate_json(existing, |root| {
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`hooks` is present in hooks.json but not an object")?;
            for (event, value) in &our_hooks {
                overlay_event_hooks(hooks, event, value);
            }
            Ok(())
        })
    })
}

/// Mutate `~/.cursor/hooks.json` (creating it if absent) so Cursor's
/// agent fires the sessionmunch scripts on lifecycle events.
///
/// Cursor's hook schema (per <https://cursor.com/docs/agent/hooks>) is
/// *flatter* than Claude Code's / Codex's:
///
///   { "version": 1,
///     "hooks": {
///       "sessionStart": [
///         { "type": "command", "command": "...", "matcher": "" }
///       ]
///     }
///   }
///
/// — no inner `hooks: [...]` array, camelCase event names, plus a
/// required top-level `version: 1` key. We use `CURSOR_PROFILE`
/// (HookShape::Flat) to produce the right payload, then merge into
/// the existing file (preserving any non-overlapping events the
/// user has wired up to other tools).
fn apply_to_cursor_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => cursor_hooks_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "cursor", data_dir)?;
    let command_dir = staged_command_dir(&staged, "cursor");
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let outcome = merge_cursor_hooks(
        &command_dir,
        server_url,
        auth_token,
        data_dir,
        strategy,
        &path,
    )?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

fn merge_cursor_hooks(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<ApplyOutcome> {
    let payload = build_profile_payload_for_agent(
        &CURSOR_PROFILE,
        staged,
        server_url,
        auth_token,
        "cursor",
        Some(data_dir),
        project_strategy,
    );
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_object())
        .context("internal: payload builder didn't return a hooks object")?
        .clone();
    apply_atomic(config_path, |existing| {
        mutate_json(existing, |root| {
            // Cursor requires "version": 1 at the top level.
            // Overwrite unconditionally — the schema is versioned
            // so future Cursor releases can bump this; we'll bump
            // here too when that happens.
            root.insert("version".into(), serde_json::json!(1));
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`hooks` is present in hooks.json but not an object")?;
            for (event, value) in &our_hooks {
                overlay_event_hooks(hooks, event, value);
            }
            Ok(())
        })
    })
}

/// Mutate `~/.gemini/settings.json` so Gemini CLI fires the sessionmunch
/// scripts on its (Gemini-specific) lifecycle events.
///
/// Gemini's schema (per <https://geminicli.com/docs/hooks/reference>)
/// is the same nested shape as Claude Code's (`matcher` +
/// `hooks: [{type, command}]`), but the event vocabulary differs:
///
///   - `BeforeTool` / `AfterTool`  (sessionmunch: `pre-tool-use` / `post-tool-use`)
///   - `PreCompress`               (sessionmunch: `pre-compact`)
///   - `SessionStart` / `SessionEnd` line up with Claude Code's
///   - No `UserPromptSubmit` / `Stop` equivalents — skipped
///
/// Like Claude Code, Gemini doesn't honour an `env` field at the
/// inner-hook level, so the env vars get inlined into the command
/// string by the shared payload builder.
fn apply_to_gemini_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => gemini_settings_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "gemini-cli", data_dir)?;
    let command_dir = staged_command_dir(&staged, "gemini-cli");
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let outcome = merge_gemini_hooks(
        &command_dir,
        server_url,
        auth_token,
        data_dir,
        strategy,
        &path,
    )?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

fn merge_gemini_hooks(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<ApplyOutcome> {
    let payload = build_profile_payload_for_agent(
        &GEMINI_PROFILE,
        staged,
        server_url,
        auth_token,
        "gemini-cli",
        Some(data_dir),
        project_strategy,
    );
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_object())
        .context("internal: payload builder didn't return a hooks object")?
        .clone();
    apply_atomic(config_path, |existing| {
        mutate_json(existing, |root| {
            // Gemini's settings.json mixes MCP servers, hooks, and
            // other config under one document. Get-or-create the
            // `hooks` table; overlay our events; preserve siblings.
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`hooks` is present in settings.json but not an object")?;
            for (event, value) in &our_hooks {
                overlay_event_hooks(hooks, event, value);
            }
            Ok(())
        })
    })
}

/// Mutate `~/.gemini/config/hooks.json` so Antigravity CLI (`agy`)
/// fires the sessionmunch scripts on its lifecycle events.
///
/// Antigravity CLI uses a named-groups format where hook groups are
/// top-level keys (e.g. `"sessionmunch"`) containing event arrays. Tool
/// events (`PreToolUse`, `PostToolUse`) use nested shape with matcher;
/// lifecycle events (`PreInvocation`, `Stop`) use flat shape.
///
/// Config file: `~/.gemini/config/hooks.json`
fn apply_to_antigravity_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => antigravity_hooks_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "antigravity-cli", data_dir)?;
    let command_dir = staged_command_dir(&staged, "antigravity-cli");
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let outcome = merge_antigravity_hooks(
        &command_dir,
        server_url,
        auth_token,
        data_dir,
        strategy,
        &path,
    )?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    println!();
    print!("{ANTIGRAVITY_FINALIZATION_GUIDANCE}");
    Ok(())
}

const ANTIGRAVITY_FINALIZATION_GUIDANCE: &str = "\
Antigravity `Stop` ends one execution loop, not the conversation. After the\n\
final turn, run `sessionmunch finalize-session --agent antigravity-cli` to create\n\
the final summary and handoff and to queue opt-in SessionEnd consolidation.\n";

fn merge_antigravity_hooks(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<ApplyOutcome> {
    let payload = build_antigravity_payload_with_data_dir(
        staged,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let our_group = payload
        .get("sessionmunch")
        .and_then(|v| v.as_object())
        .context("internal: build_antigravity_payload didn't return an sessionmunch group")?
        .clone();
    apply_atomic(config_path, |existing| {
        mutate_json(existing, |root| {
            // Get-or-create the "sessionmunch" named group; overlay
            // our events. Other named groups survive untouched.
            let group = root
                .entry("sessionmunch")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`sessionmunch` is present in hooks.json but not an object")?;
            for (event, value) in &our_group {
                overlay_event_hooks(group, event, value);
            }
            Ok(())
        })
    })
}

/// Mutate Kimi Code's `config.toml` (`$KIMI_CODE_HOME/config.toml` when
/// the var is set, else `~/.kimi-code/config.toml`) so Kimi Code fires
/// the sessionmunch scripts on its lifecycle events.
///
/// Kimi Code stores hooks as `[[hooks]]` array-of-tables entries inside
/// the SAME config.toml that carries the user's providers and model,
/// and any unknown key on a hook entry makes the whole config fail to
/// load. The merge is therefore TOML-aware via `toml_edit` (the rest of
/// the document is preserved as parsed) and each entry carries exactly
/// `event` + `command` — `matcher` omitted (Kimi Code matches
/// everything) and `timeout` omitted (30s default).
fn apply_to_kimi_code_config(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => kimi_code_config_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "kimi-code", data_dir)?;
    let command_dir = staged_command_dir(&staged, "kimi-code");
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let outcome = merge_kimi_code_hooks(
        &command_dir,
        server_url,
        auth_token,
        data_dir,
        strategy,
        &path,
    )?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

fn merge_kimi_code_hooks(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<ApplyOutcome> {
    let commands = kimi_code_hook_commands(
        staged,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    apply_atomic(config_path, |existing| {
        mutate_toml(existing, |doc| upsert_kimi_code_hooks(doc, &commands))
    })
}

/// Insert or refresh sessionmunch's `[[hooks]]` entries: drop any prior
/// entry whose command references our staged scripts (so re-runs update
/// in place instead of duplicating), append the fresh set, and leave
/// every third-party `[[hooks]]` entry — and every other table in
/// config.toml — untouched.
fn upsert_kimi_code_hooks(
    doc: &mut toml_edit::DocumentMut,
    commands: &[(&str, String)],
) -> Result<()> {
    use toml_edit::{ArrayOfTables, Item, Table, value};

    if doc.get("hooks").is_none() {
        doc.insert("hooks", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let hooks = doc
        .get_mut("hooks")
        .and_then(Item::as_array_of_tables_mut)
        .context("`hooks` is present in config.toml but not an array of tables")?;

    let stale: Vec<usize> = hooks
        .iter()
        .enumerate()
        .filter_map(|(i, entry)| is_sessionmunch_toml_hook_entry(entry).then_some(i))
        .collect();
    for i in stale.into_iter().rev() {
        hooks.remove(i);
    }
    for (event, command) in commands {
        let mut entry = Table::new();
        entry["event"] = value(*event);
        entry["command"] = value(command);
        hooks.push(entry);
    }
    Ok(())
}

/// True if a `[[hooks]]` table belongs to sessionmunch. Keep the ownership
/// signature shared with JSON hook overlays and uninstall so string commands
/// from other tools are not removed during re-apply.
fn is_sessionmunch_toml_hook_entry(entry: &toml_edit::Table) -> bool {
    let Some(command) = entry.get("command").and_then(|c| c.as_str()) else {
        return false;
    };
    hook_command_is_ours(command)
}

/// Merge sessionmunch's camelCase hook entries into every existing Kiro CLI
/// v2 agent config (`~/.kiro/agents/*.json`, or the single file passed
/// via `--config-file`).
///
/// The v2 engine has no global hook surface: hooks fire only for the
/// agent config that is active, so the installer updates each config the
/// user already has. It never invents one — a fabricated agent config
/// would not be the active agent (the built-in default has no file on
/// disk), so hooks in it would never fire and the install would look
/// successful while capturing nothing.
fn apply_to_kiro_cli_agent_configs(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    // Resolve targets before staging anything so a missing-agent-config
    // bail leaves no half-done install behind.
    let targets: Vec<PathBuf> = match &args.config_file {
        Some(p) => {
            anyhow::ensure!(
                p.is_file(),
                "{} does not exist. Kiro CLI v2 hooks live inside an existing agent config; \
                 create one first (`kiro-cli agent create`) and re-run. For Kiro v3, select \
                 the explicit `--agent kiro-cli-v3` standalone-hook target instead.",
                p.display()
            );
            vec![p.clone()]
        }
        None => list_kiro_cli_agent_configs(&kiro_cli_agents_dir()?)?,
    };
    anyhow::ensure!(
        !targets.is_empty(),
        "no Kiro CLI agent configs found in {}. The v2 engine only fires hooks defined in an \
         agent config and the built-in default agent has no file on disk, so create an agent \
         first (`kiro-cli agent create`, then `kiro-cli agent set-default <name>`) and re-run. \
         For Kiro v3, select the explicit `--agent kiro-cli-v3` standalone-hook target instead.",
        kiro_cli_agents_dir()?.display()
    );
    let prepared = preflight_kiro_cli_agent_configs(
        targets,
        hooks_dir,
        server_url,
        auth_token,
        data_dir,
        args.project_strategy,
    )?;

    let staged = stage_hook_scripts(hooks_dir, "kiro-cli", data_dir)?;
    let command_dir = staged_command_dir(&staged, "kiro-cli");
    for (path, strategy) in prepared {
        let outcome = merge_kiro_cli_agent_hooks(
            &command_dir,
            server_url,
            auth_token,
            data_dir,
            strategy.and_then(ProjectStrategyArg::baked),
            &path,
        )?;
        println!(
            "✓ {} {} ({})",
            outcome.verb(),
            path.display(),
            match outcome {
                ApplyOutcome::Created => "new file",
                ApplyOutcome::Updated => "backup written next to it",
                ApplyOutcome::NoOp => "already up to date",
            }
        );
    }
    Ok(())
}

/// Parse and validate every Kiro v2 target before staging scripts or writing
/// any config. The strategy is recovered per file because Kiro can update
/// several independently-configured agents in one invocation.
fn preflight_kiro_cli_agent_configs(
    targets: Vec<PathBuf>,
    command_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    explicit_strategy: Option<ProjectStrategyArg>,
) -> Result<Vec<(PathBuf, Option<ProjectStrategyArg>)>> {
    let mut prepared = Vec::with_capacity(targets.len());
    for path in targets {
        let existing =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let strategy =
            explicit_strategy.or_else(|| baked_project_strategy(AgentChoice::KiroCli, &existing));
        render_kiro_cli_agent_hooks(
            &existing,
            command_dir,
            server_url,
            auth_token,
            data_dir,
            strategy.and_then(ProjectStrategyArg::baked),
            &path,
        )?;
        prepared.push((path, strategy));
    }
    Ok(prepared)
}

/// Merge sessionmunch's v2 hook entries into one Kiro agent config,
/// preserving every third-party entry under the same triggers and every
/// non-hook field of the agent definition.
fn merge_kiro_cli_agent_hooks(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<ApplyOutcome> {
    apply_atomic(config_path, |existing| {
        render_kiro_cli_agent_hooks(
            existing,
            staged,
            server_url,
            auth_token,
            data_dir,
            project_strategy,
            config_path,
        )
    })
}

fn render_kiro_cli_agent_hooks(
    existing: &str,
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<String> {
    let our_hooks = build_kiro_cli_v2_hooks_value(
        staged,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    mutate_json(existing, |root| {
        let hooks = root
            .entry("hooks")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .with_context(|| {
                format!(
                    "`hooks` in {} is present but not an object",
                    config_path.display()
                )
            })?;
        for (event, value) in &our_hooks {
            overlay_kiro_cli_event_hooks(hooks, event, value);
        }
        Ok(())
    })
}

fn apply_to_kiro_cli_v3_hooks(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(path) => path.clone(),
        None => kiro_cli_v3_hooks_path()?,
    };
    let existing = match fs::read_to_string(&path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);

    // Validate before staging scripts so an incompatible registration file
    // cannot leave a partial installation behind.
    render_kiro_cli_v3_hooks(
        &existing, hooks_dir, server_url, auth_token, data_dir, strategy, &path,
    )?;

    let staged = stage_hook_scripts(hooks_dir, "kiro-cli", data_dir)?;
    let command_dir = staged_command_dir(&staged, "kiro-cli");
    let outcome = merge_kiro_cli_v3_hooks(
        &command_dir,
        server_url,
        auth_token,
        data_dir,
        strategy,
        &path,
    )?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

fn merge_kiro_cli_v3_hooks(
    command_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<ApplyOutcome> {
    apply_atomic(config_path, |existing| {
        render_kiro_cli_v3_hooks(
            existing,
            command_dir,
            server_url,
            auth_token,
            data_dir,
            project_strategy,
            config_path,
        )
    })
}

fn render_kiro_cli_v3_hooks(
    existing: &str,
    command_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<String> {
    let desired = build_kiro_cli_v3_hooks_value(
        command_dir,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let desired_hooks = desired
        .get("hooks")
        .and_then(serde_json::Value::as_array)
        .expect("static Kiro v3 hook payload has a hooks array")
        .clone();

    mutate_json(existing, |root| {
        match root.get("version") {
            Some(serde_json::Value::String(version)) if version == "v1" => {}
            Some(serde_json::Value::String(version)) => anyhow::bail!(
                "unsupported Kiro hook schema version `{version}` in {}; expected `v1`",
                config_path.display()
            ),
            Some(_) => anyhow::bail!(
                "`version` in {} is present but not a string",
                config_path.display()
            ),
            None => {
                root.insert(
                    "version".to_string(),
                    serde_json::Value::String("v1".to_string()),
                );
            }
        }

        let hooks = root
            .entry("hooks")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| {
                format!(
                    "`hooks` in {} is present but not an array",
                    config_path.display()
                )
            })?;

        for entry in hooks.iter() {
            if let Some(name) = entry.get("name").and_then(serde_json::Value::as_str)
                && kiro_cli_v3_hook_name_is_reserved(name)
                && !is_sessionmunch_kiro_v3_hook_entry(entry)
            {
                anyhow::bail!(
                    "Kiro v3 hook `{name}` in {} uses sessionmunch's reserved name but is not an \
                     sessionmunch hook; rename it before installing",
                    config_path.display()
                );
            }
        }
        hooks.retain(|entry| !is_sessionmunch_kiro_v3_hook_entry(entry));
        hooks.extend(desired_hooks);
        Ok(())
    })
}

fn kiro_cli_v3_hook_name_is_reserved(name: &str) -> bool {
    KIRO_CLI_V3_EVENTS.iter().any(|(_, script)| {
        script
            .strip_suffix(".sh")
            .is_some_and(|stem| name == format!("sessionmunch-{stem}"))
    })
}

pub(crate) fn is_sessionmunch_kiro_v3_hook_entry(entry: &serde_json::Value) -> bool {
    let Some(name) = entry.get("name").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let Some(trigger) = entry.get("trigger").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let known_pair = KIRO_CLI_V3_EVENTS.iter().any(|(expected_trigger, script)| {
        *expected_trigger == trigger
            && script
                .strip_suffix(".sh")
                .is_some_and(|stem| name == format!("sessionmunch-{stem}"))
    });
    known_pair
        && entry
            .pointer("/action/type")
            .and_then(serde_json::Value::as_str)
            == Some("command")
        && entry
            .pointer("/action/command")
            .and_then(serde_json::Value::as_str)
            .is_some_and(is_sessionmunch_kiro_hook_command)
}

/// Every `*.json` agent config in the Kiro CLI global agents directory,
/// sorted for deterministic apply order. A missing directory is an empty
/// list, not an error — the caller decides how to report it.
pub(crate) fn list_kiro_cli_agent_configs(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", dir.display()));
        }
    };
    let mut configs = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading entry in {}", dir.display()))?;
        let path = entry.path();
        let metadata = fs::metadata(&path)
            .with_context(|| format!("reading metadata for {}", path.display()))?;
        if metadata.is_file() && path.extension().and_then(|e| e.to_str()) == Some("json") {
            configs.push(path);
        }
    }
    configs.sort();
    Ok(configs)
}

pub(crate) fn is_sessionmunch_kiro_hook_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let generated_script =
        lower.contains("/hooks/kiro-cli/") || lower.contains("\\hooks\\kiro-cli\\");
    let sessionmunch_executable = lower.contains("sessionmunch") || lower.contains("sessionmunch");
    let native_hook = sessionmunch_executable
        && lower.contains(" hook ")
        && lower.contains("--agent kiro-cli")
        && lower.contains("--server-url");
    (generated_script || native_hook)
        && KIRO_CLI_V2_EVENTS.iter().any(|(_, script)| {
            let stem = script.trim_end_matches(".sh");
            lower.contains(script)
                || lower.contains(&format!("{stem}.ps1"))
                || lower.contains(&format!("--event {stem}"))
        })
}

/// Generate an OpenCode plugin at `~/.config/opencode/plugins/sessionmunch.ts`.
///
/// OpenCode's integration surface is a TypeScript plugin, not a JSON
/// hook table. The plugin posts normalized lifecycle payloads directly
/// to `/hook` and injects pending handoffs through
/// `experimental.chat.system.transform`, because plugin shell stdout is
/// not prepended to the model context the way Claude Code hook stdout is.
fn apply_to_opencode_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    args: &InstallHooksArgs,
    capture_mode: &str,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => opencode_plugin_path()?,
    };
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let body = build_opencode_plugin(server_url, auth_token, strategy, capture_mode);

    let outcome = apply_atomic(&path, move |_existing| Ok(body.clone()))?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new plugin file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    if !matches!(outcome, ApplyOutcome::NoOp) {
        println!();
        println!("OpenCode auto-loads plugins from ~/.config/opencode/plugins/ on next start.");
        println!("If you're already inside an `opencode` session, restart it for the");
        println!("new plugin to take effect.");
    }
    Ok(())
}

fn render_opencode_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    capture_mode: &str,
) -> Result<()> {
    println!("// OpenCode plugin — write to ~/.config/opencode/plugins/sessionmunch.ts");
    println!("// Or re-run with `--apply` to install it automatically.");
    println!("// Restart OpenCode after changing plugins; config is loaded at startup.");
    println!();
    println!(
        "{}",
        build_opencode_plugin(server_url, auth_token, project_strategy, capture_mode)
    );
    Ok(())
}

/// `~/.config/opencode/plugins/sessionmunch-opencode2.ts` — the OpenCode 2.0
/// beta plugin file. The beta shares v1's config dir and session store but
/// not its plugin API, so this is a distinct file from `sessionmunch.ts`
/// (uninstall keys each file to its own banner + agent constant).
pub(crate) fn opencode2_plugin_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.config/opencode")?
        .join(".config")
        .join("opencode")
        .join("plugins")
        .join("sessionmunch-opencode2.ts"))
}

/// Generate an OpenCode 2.0 beta plugin at
/// `~/.config/opencode/plugins/sessionmunch-opencode2.ts`.
///
/// The beta's plugin API (`Plugin.define({ id, setup })` with
/// `ctx.session.hook` / `ctx.tool.hook` / `ctx.event.subscribe`) is
/// incompatible with v1's function plugin, so the beta gets its own file.
/// Both files share the one auto-loaded dir while the beta is side-by-side;
/// a host may warn about its sibling's file (API mismatch) — that warning
/// is benign, and `uninstall` removes each file only on its own ownership
/// markers.
fn apply_to_opencode2_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    args: &InstallHooksArgs,
    capture_mode: &str,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => opencode2_plugin_path()?,
    };
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let body = build_opencode2_plugin(server_url, auth_token, strategy, capture_mode)?;

    let outcome = apply_atomic(&path, move |_existing| Ok(body.clone()))?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new plugin file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    if !matches!(outcome, ApplyOutcome::NoOp) {
        println!();
        println!("OpenCode 2 auto-loads plugins from ~/.config/opencode/plugins/ on next start.");
        println!("If you're already inside an `opencode2` session, restart it for the");
        println!("new plugin to take effect.");
    }
    Ok(())
}

fn render_opencode2_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    capture_mode: &str,
) -> Result<()> {
    println!(
        "// OpenCode 2.0 beta plugin — write to ~/.config/opencode/plugins/sessionmunch-opencode2.ts"
    );
    println!("// Or re-run with `--apply` to install it automatically.");
    println!("// Restart OpenCode 2 after changing plugins; config is loaded at startup.");
    println!();
    println!(
        "{}",
        build_opencode2_plugin(server_url, auth_token, project_strategy, capture_mode)?
    );
    Ok(())
}

/// Build the beta plugin from the v1 template. The capture prelude (hook
/// queue + spooling, marker resolution, capture policy, session maps,
/// `/hook` + `/handoff` helpers) is host-independent and reused verbatim;
/// only the host binding is rewritten for the V2 API. Anchors are plain
/// rendered-TS constants like the spool patch's: a template edit that moves
/// them fails loudly here instead of shipping a half-v1 plugin.
fn build_opencode2_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    capture_mode: &str,
) -> Result<String> {
    let v1 = build_opencode_plugin(server_url, auth_token, project_strategy, capture_mode);
    const BANNER_V1: &str =
        "// Auto-generated by `sessionmunch install-hooks --agent opencode --apply`.";
    const BANNER_V2: &str =
        "// Auto-generated by `sessionmunch install-hooks --agent opencode2 --apply`.";
    const AGENT_V1: &str = "const AGENT = \"open-code\";";
    const AGENT_V2: &str = "const AGENT = \"opencode2\";";
    // Start of the v1 host binding through end of file. Everything before
    // this line (including the spooled delivery path) is shared.
    const V1_BINDING_ANCHOR: &str =
        "\nexport const SessionMunchHooks: Plugin = async ({ directory }) => {";
    // `replacen` silently keeps the v1 text when its anchor moves, which
    // would ship a half-v1 plugin — fail loudly instead, like the spool
    // patch's anchors do.
    let out = replace_opencode_anchor(&v1, BANNER_V1, BANNER_V2, "banner")?;
    let out = replace_opencode_anchor(&out, AGENT_V1, AGENT_V2, "agent constant")?;
    let Some(binding_at) = out.find(V1_BINDING_ANCHOR) else {
        anyhow::bail!("opencode2 template drifted: v1 host binding anchor not found");
    };
    let mut rebuilt = out[..binding_at].to_string();
    rebuilt.push_str(OPENCODE2_BINDING);
    Ok(rebuilt)
}

fn replace_opencode_anchor(haystack: &str, from: &str, to: &str, what: &str) -> Result<String> {
    if !haystack.contains(from) {
        anyhow::bail!("opencode2 template drifted: v1 {what} anchor not found");
    }
    Ok(haystack.replacen(from, to, 1))
}

/// V2 host binding for the shared capture prelude: the beta's `{ id, setup }`
/// plugin shape with its session/tool/event hooks (verified against
/// `@opencode-ai/plugin@beta`, including a `tsc --noEmit` pass over the
/// rendered file). Lifecycle arrives on `ctx.event.subscribe` whose
/// envelopes carry `{ type, data, location }` (v1's `event.properties`
/// is kept as a fallback because the beta schema is still changing).
/// Handoff injection moved from v1's removed
/// `experimental.chat.system.transform` to the `context` hook, which edits
/// the outgoing model call without persisting into history — guarded to
/// inject once per session because it fires on every continuation.
const OPENCODE2_BINDING: &str = include_str!("../../templates/plugins/opencode2_binding.ts");
/// `findSettingsMarker` mirrors the native `find_settings_marker` (`marker.rs`)
/// and the shell `sessionmunch_find_settings_marker` (`hooks/_lib.sh`), #668:
/// the same ancestor walk and HOME/`.git` boundary as `findMarker`, but a
/// marker that declares nothing beyond `[capture]` is transparent — the walk
/// skips it and continues to the next ancestor. That keeps a nested
/// capture-only marker (e.g. one that only sets `ignore_paths`) from
/// shadowing an outer marker's `workspace`/`project`/etc, without changing
/// `[capture]`/`ignore_paths` resolution itself (that stays on `findMarker`,
/// the nearest marker, in the separate capture-policy template). The
/// boundary walk is duplicated from `findMarker` rather than shared, on
/// purpose: `findMarker` stays untouched, well-exercised, nearest-marker
/// behavior for every other caller.
pub(crate) const TS_FIND_SETTINGS_MARKER: &str = r#"function declaresSettings(text: string): boolean {
  for (const key of ["workspace", "project", "project_strategy", "drop_subagent_captures"]) {
    if (tomlKey(text, key) !== undefined) return true;
  }
  for (const key of ["default_global", "inject_on_session_start", "max_chars"]) {
    if (tomlFlag(text, key) !== undefined) return true;
  }
  return false;
}

function findSettingsMarker(cwd: string | undefined): string | undefined {
  if (!cwd) return undefined;
  let dir = resolve(cwd);
  const home = homedir();
  let boundary: string | undefined;
  if (home && (dir === home || dir.startsWith(home.endsWith(sep) ? home : home + sep))) {
    boundary = home;
  } else if (home) {
    let probe = dir;
    while (probe && probe !== dirname(probe)) {
      if (existsSync(join(probe, ".git"))) {
        boundary = probe;
        break;
      }
      probe = dirname(probe);
    }
    boundary ??= dir;
  }
  while (dir && dir !== dirname(dir)) {
    const marker = join(dir, ".sessionmunch.toml");
    if (existsSync(marker)) {
      try {
        if (declaresSettings(readFileSync(marker, "utf8"))) return marker;
      } catch (_e) {
      }
    }
    if (boundary && dir === boundary) return undefined;
    dir = dirname(dir);
  }
  return undefined;
}"#;

/// Emit the `applyMarkerParams` TypeScript function shared verbatim by the
/// OpenCode plugin and the OMP extension.
///
/// `None` reproduces the historical marker-only function byte-for-byte, so
/// existing generated files and golden tests are unchanged. `Some(default)`
/// prepends a `DEFAULT_PROJECT_STRATEGY` const and emits a variant that applies
/// that install-time default when no marker pins a `project_strategy` (#128).
/// A marker's own `project` / `project_strategy` still take precedence (§3.3),
/// and repo-root is resolved host-side via `repoRootProject`.
///
/// Scope/settings resolution walks past a capture-only marker to the nearest
/// ancestor marker that declares a setting (#668) via `findSettingsMarker`,
/// emitted alongside this function.
fn ts_apply_marker_params(default_strategy: Option<&str>) -> String {
    let Some(default) = default_strategy else {
        return format!(
            "{TS_TOML_FLAG}\n{TS_FIND_SETTINGS_MARKER}\n{}",
            r#"function applyMarkerParams(url: URL, cwd: string | undefined): void {
  const managedRun = process.env.SESSIONMUNCH_RUN_ID;
  if (managedRun) url.searchParams.set("managed_run", managedRun);
  const marker = findSettingsMarker(cwd);
  if (!marker || !cwd) return;
  url.searchParams.set("cwd", cwd);
  try {
    const body = readFileSync(marker, "utf8");
    const workspace = tomlKey(body, "workspace");
    const project = tomlKey(body, "project");
    const projectStrategy = tomlKey(body, "project_strategy");
    const dropSubagent = tomlKey(body, "drop_subagent_captures");
    const defaultGlobal = tomlFlag(body, "default_global");
    const briefing = tomlFlag(body, "inject_on_session_start");
    const briefingBudget = tomlFlag(body, "max_chars");
    if (workspace) url.searchParams.set("workspace", workspace);
    if (project) url.searchParams.set("project", project);
    if (projectStrategy) url.searchParams.set("project_strategy", projectStrategy);
    if (dropSubagent) url.searchParams.set("drop_subagent", dropSubagent);
    if (defaultGlobal) url.searchParams.set("default_global", defaultGlobal);
    if (briefing) url.searchParams.set("briefing", briefing);
    if (briefingBudget) url.searchParams.set("briefing_budget", briefingBudget);
    if (!project && (projectStrategy === "repo-root" || projectStrategy === "repo_root")) {
      const repoProject = repoRootProject(cwd);
      if (repoProject) url.searchParams.set("project", repoProject);
    }
  } catch (_e) {
  }
}"#
        );
    };
    let body = r#"function applyMarkerParams(url: URL, cwd: string | undefined): void {
  const managedRun = process.env.SESSIONMUNCH_RUN_ID;
  if (managedRun) url.searchParams.set("managed_run", managedRun);
  if (!cwd) return;
  url.searchParams.set("cwd", cwd);
  let workspace: string | undefined;
  let project: string | undefined;
  let projectStrategy: string | undefined;
  let dropSubagent: string | undefined;
  let defaultGlobal: string | undefined;
  let briefing: string | undefined;
  let briefingBudget: string | undefined;
  const marker = findSettingsMarker(cwd);
  if (marker) {
    try {
      const body = readFileSync(marker, "utf8");
      workspace = tomlKey(body, "workspace");
      project = tomlKey(body, "project");
      projectStrategy = tomlKey(body, "project_strategy");
      dropSubagent = tomlKey(body, "drop_subagent_captures");
      defaultGlobal = tomlFlag(body, "default_global");
      briefing = tomlFlag(body, "inject_on_session_start");
      briefingBudget = tomlFlag(body, "max_chars");
    } catch (_e) {
    }
  }
  if (!projectStrategy) projectStrategy = DEFAULT_PROJECT_STRATEGY;
  if (!project && (projectStrategy === "repo-root" || projectStrategy === "repo_root")) {
    const repoProject = repoRootProject(cwd);
    if (repoProject) project = repoProject;
  }
  if (workspace) url.searchParams.set("workspace", workspace);
  if (project) url.searchParams.set("project", project);
  if (projectStrategy) url.searchParams.set("project_strategy", projectStrategy);
  if (dropSubagent) url.searchParams.set("drop_subagent", dropSubagent);
  if (defaultGlobal) url.searchParams.set("default_global", defaultGlobal);
  if (briefing) url.searchParams.set("briefing", briefing);
  if (briefingBudget) url.searchParams.set("briefing_budget", briefingBudget);
}"#;
    format!(
        "const DEFAULT_PROJECT_STRATEGY = {};\n{TS_TOML_FLAG}\n{TS_FIND_SETTINGS_MARKER}\n{body}",
        ts_string_literal(default)
    )
}

/// `tomlFlag` mirrors the native hook's `parse_toml_flag`: unlike `tomlKey`
/// (quoted strings only) it also accepts a bare token (`default_global =
/// true`, `max_chars = 4000`), so section-style marker keys work whether or
/// not the operator quotes the value. Emitted next to `applyMarkerParams`
/// in every generated TypeScript integration.
pub(crate) const TS_TOML_FLAG: &str = r#"function tomlFlag(text: string, key: string): string | undefined {
  const re = new RegExp(`^\\s*${key}\\s*=\\s*(?:"([^"]*)"|([^#\\s]+))`);
  for (const line of text.split(/\r?\n/)) {
    const match = re.exec(line);
    if (match) return match[1] ?? match[2];
  }
  return undefined;
}"#;

/// Post-process a generated TypeScript integration so failed hook
/// deliveries SPOOL instead of vanishing (#580). The shell hooks have
/// spooled since day one; the TS plugins fire-and-forgot, so a laptop
/// off the server's network lost whole sessions of capture. Entries are
/// written in the CLI spool's exact on-disk contract
/// (`{ms:013}-{pid}-{seq:016x}.json`, `SpoolEntry` fields, 0700 dir /
/// 0600 files, tmp+rename) so `sessionmunch hook-drain` and the shell
/// hooks' piggyback drain deliver them too; the plugin also drains its
/// own spool on first use and after any queue flush. An `ingest_key`
/// is minted at spool time, so a double drain (plugin + CLI) dedupes
/// server-side instead of double-ingesting.
///
/// Implemented as a transformation over the rendered source (rather
/// than another copy of the template) so all TS integrations share one
/// audited implementation; it fails loudly if the delivery block it
/// patches ever drifts.
fn add_hook_spooling(source: String) -> Result<String> {
    const FETCH_BLOCK: &str = r#"      try {
        await fetch(item.url, {
          method: "POST",
          headers: { "Content-Type": "application/json", ...authHeaders() },
          body: JSON.stringify(item.payload),
          signal: timeoutSignal(HOOK_REQUEST_TIMEOUT_MS),
        }).catch(() => undefined);
      } catch (_e) {
        // Best-effort capture. Hooks must never block the agent.
      }"#;
    const FETCH_REPLACEMENT: &str = r#"      try {
        const resp = await fetch(item.url, {
          method: "POST",
          headers: { "Content-Type": "application/json", ...authHeaders() },
          body: JSON.stringify(item.payload),
          signal: timeoutSignal(HOOK_REQUEST_TIMEOUT_MS),
        }).catch(() => undefined);
        // Unreachable server or 5xx: keep the event on disk for a later
        // drain (#580). 4xx is permanent - spooling it would retry a
        // rejection forever.
        if (!resp || resp.status >= 500) spoolFailedHook(item.url, item.payload);
      } catch (_e) {
        try { spoolFailedHook(item.url, item.payload); } catch (_e2) {}
      }"#;
    const ENQUEUE_ANCHOR: &str = "function enqueueHook(";
    let runtime = crate::commands::render_shared::ts_spool_runtime();
    let resolve_fn = crate::commands::render_shared::ts_resolve_token_fn();
    if !source.contains(FETCH_BLOCK) {
        anyhow::bail!(
            "TS integration template drifted: the hook delivery block the \
             spool patch targets was not found"
        );
    }
    if source.matches(ENQUEUE_ANCHOR).count() != 1 {
        anyhow::bail!("TS integration template drifted: enqueueHook anchor not unique");
    }
    let mut out = source.replacen(FETCH_BLOCK, FETCH_REPLACEMENT, 1);
    out = out.replacen(
        ENQUEUE_ANCHOR,
        &format!("{resolve_fn}{runtime}{ENQUEUE_ANCHOR}"),
        1,
    );
    // Drain the offline spool alongside every queue flush, and extend the
    // imports the runtime needs.
    let drain_anchor =
        "async function drainHookQueue(): Promise<void> {\n  if (hookDraining) return;";
    if out.matches(drain_anchor).count() != 1 {
        anyhow::bail!("TS integration template drifted: drainHookQueue anchor not unique");
    }
    out = out.replacen(
        drain_anchor,
        "async function drainHookQueue(): Promise<void> {\n  if (hookDraining) return;\n  requestSpoolDrain();",
        1,
    );
    let import_anchor = "import { closeSync, existsSync, openSync, readFileSync as readMarkerText, readSync } from \"node:fs\";";
    if out.matches(import_anchor).count() != 1 {
        anyhow::bail!("TS integration template drifted: node:fs import anchor not unique");
    }
    out = out.replacen(
        import_anchor,
        "import { closeSync, existsSync, mkdirSync, openSync, readFileSync as readMarkerText, readSync, readdirSync, renameSync, unlinkSync, writeFileSync } from \"node:fs\";",
        1,
    );
    Ok(out)
}

fn build_opencode_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    capture_mode: &str,
) -> String {
    let token_line = auth_token
        .map(|t| format!("const TOKEN: string | null = {};\n", ts_string_literal(t)))
        .unwrap_or_else(|| "const TOKEN: string | null = null;\n".to_string());
    let apply_marker_params = ts_apply_marker_params(project_strategy);
    let capture_policy = ts_capture_policy_v1(capture_mode);
    let body = include_str!("../../templates/plugins/opencode_plugin.ts")
        .replace("__SESSIONMUNCH_CAPTURE_POLICY__", &capture_policy)
        .replace("__SESSIONMUNCH_APPLY_MARKER_PARAMS__", &apply_marker_params)
        .replace(
            "__SESSIONMUNCH_SERVER_LITERAL__",
            &ts_string_literal(server_url),
        )
        .replace("__SESSIONMUNCH_TOKEN_LINE__", &token_line);
    // Anchors are compile-time constants over the bundled template;
    // a mismatch is a template edit that forgot the spool patch.
    add_hook_spooling(body).expect("generated TS integration lost its hook-spool anchors")
}

/// Warn when Pi and OMP resolve to the same extensions directory.
///
/// Agent-distinct filenames stop the two installs from overwriting each
/// other, but they do not make sharing a directory safe. OMP discovers
/// **every** direct `*.ts` under its extensions directory, so with both
/// files present each agent loads both extensions — and since the Pi
/// extension is the OMP one with `const AGENT` swapped, and neither guards
/// on its host, every lifecycle event is captured twice under two different
/// agent identities.
///
/// That is not a regression this installer can fix by itself: both agents
/// honour the same `PI_CODING_AGENT_DIR`, so the collision is upstream. What
/// it can do is refuse to be silent about it, and name the two ways out.
fn warn_if_agents_share_extensions_dir(this_agent: &str, this_path: &Path) {
    let other = match this_agent {
        "pi" => omp_extension_path(None),
        "omp" => pi_extension_path(),
        _ => return,
    };
    let Ok(other_path) = other else { return };
    let (Some(a), Some(b)) = (this_path.parent(), other_path.parent()) else {
        return;
    };
    if a != b {
        return;
    }
    eprintln!(
        "warning: Pi and OMP both resolve to {} — they share PI_CODING_AGENT_DIR.\n\
         \x20 Each agent loads every *.ts in that directory, so installing both\n\
         \x20 captures each event twice, once as `pi` and once as `omp`.\n\
         \x20 Give them separate homes, or scope OMP to a profile:\n\
         \x20     sessionmunch install-hooks --agent omp --profile <name> --apply",
        a.display()
    );
}

/// Remove a pre-rename `sessionmunch.ts` sitting beside the new agent-distinct
/// file, so the agent does not load both.
///
/// **Only removes a file this tool generated.** `uninstall` gates every
/// delete behind `generated_file_is_ours`, and `removal.rs` asserts that a
/// user-owned file which merely happens to be named `sessionmunch.ts` survives
/// (`uninstall_omp_extension_deletes_only_generated_file`). An unconditional
/// delete here would let `install-hooks --apply` destroy exactly what
/// `uninstall` refuses to touch — a worse outcome than the stale file,
/// because it is silent and hits a path the operator did not ask to modify.
///
/// The marker is the generated banner plus the agent constant, matching the
/// pair `generated_file_is_ours` checks.
///
/// `agent` scopes the match to the caller's own agent. Pi and OMP can share
/// one extensions directory (both honour `PI_CODING_AGENT_DIR`), and a
/// legacy `sessionmunch.ts` there belongs to whichever installed last. Removing
/// the *other* agent's file would silently stop its capture until the
/// operator happened to reinstall it.
fn remove_legacy_extension(new_path: &Path, agent: &str) {
    let legacy = new_path.with_file_name("sessionmunch.ts");
    if legacy == new_path || !legacy.exists() {
        return;
    }
    let Ok(content) = std::fs::read_to_string(&legacy) else {
        return;
    };
    let banner = format!("Auto-generated by `sessionmunch install-hooks --agent {agent} --apply`");
    let agent_const = format!("const AGENT = \"{agent}\";");
    let ours = content.contains(&banner) && content.contains(&agent_const);
    if !ours {
        println!(
            "note: left {} in place — it was not generated by this agent's \
             installer. Remove it yourself if the agent should stop loading it.",
            legacy.display()
        );
        return;
    }
    match std::fs::remove_file(&legacy) {
        Ok(()) => println!("✓ Removed superseded extension {}", legacy.display()),
        Err(e) => eprintln!(
            "[sessionmunch] warning: could not remove superseded extension {}: {e}",
            legacy.display()
        ),
    }
}

/// Generate an Oh My Pi extension at `~/.omp/agent/extensions/sessionmunch-omp.ts`.
///
/// OMP discovers direct `*.ts` / `*.js` files under `~/.omp/agent/extensions/`
/// at startup, so no separate settings merge is needed. The extension uses OMP's
/// lifecycle API for capture and `before_agent_start` for handoff injection.
fn apply_to_omp_extension(
    server_url: &str,
    auth_token: Option<&str>,
    args: &InstallHooksArgs,
    capture_mode: &str,
) -> Result<()> {
    let path = resolve_omp_extension_path(args)?;
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let body = build_omp_extension(server_url, auth_token, strategy, capture_mode);

    let outcome = apply_atomic(&path, move |_existing| Ok(body.clone()))?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new extension file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );

    warn_if_agents_share_extensions_dir("omp", &path);
    remove_legacy_extension(&path, "omp");

    if !matches!(outcome, ApplyOutcome::NoOp) {
        println!();
        println!(
            "OMP auto-loads direct TypeScript extensions from ~/.omp/agent/extensions/ on next start."
        );
        println!("If you're already inside an `omp` session, restart it for the");
        println!("new extension to take effect.");
    }
    Ok(())
}

fn omp_extension_hint_in(
    env_override: Option<std::ffi::OsString>,
    profile: Option<&str>,
) -> String {
    omp_extension_path_in(env_override, profile).map_or_else(
        |_| {
            if let Some(name) = profile {
                format!(
                    "~/.omp/profiles/{}/agent/extensions/sessionmunch-omp.ts",
                    name
                )
            } else {
                "~/.omp/agent/extensions/sessionmunch-omp.ts".to_string()
            }
        },
        |p| p.display().to_string(),
    )
}

/// See [`omp_extension_hint_in`]; same reasoning for Pi.
fn pi_extension_hint_in(env_override: Option<std::ffi::OsString>) -> String {
    pi_extension_path_in(env_override).map_or_else(
        |_| "~/.pi/agent/extensions/sessionmunch-pi.ts".to_string(),
        |p| p.display().to_string(),
    )
}

fn render_omp_extension(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    profile: Option<&str>,
    capture_mode: &str,
) -> Result<()> {
    println!(
        "// Oh My Pi / OMP extension — write to {}",
        omp_extension_hint_in(std::env::var_os("PI_CODING_AGENT_DIR"), profile)
    );
    println!("// Or re-run with `--apply` to install it automatically.");
    println!("// Restart OMP after changing extensions; config is loaded at startup.");
    println!();
    println!(
        "{}",
        build_omp_extension(server_url, auth_token, project_strategy, capture_mode)
    );
    Ok(())
}

fn resolve_omp_extension_path(args: &InstallHooksArgs) -> Result<PathBuf> {
    if let Some(p) = &args.config_file {
        return Ok(p.clone());
    }
    // See `omp_extension_path`: bind before borrowing.
    let env_profile = std::env::var_os("OMP_PROFILE");
    let profile = args
        .profile
        .as_deref()
        .or_else(|| env_profile.as_ref().and_then(|s| s.to_str()));
    omp_extension_path(profile)
}

fn apply_to_pi_extension(
    server_url: &str,
    auth_token: Option<&str>,
    args: &InstallHooksArgs,
    capture_mode: &str,
) -> Result<()> {
    let path = resolve_pi_extension_path(args)?;
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let body = build_pi_extension(server_url, auth_token, strategy, capture_mode);

    let outcome = apply_atomic(&path, move |_existing| Ok(body.clone()))?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new Pi extension file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );

    warn_if_agents_share_extensions_dir("pi", &path);
    remove_legacy_extension(&path, "pi");

    if !matches!(outcome, ApplyOutcome::NoOp) {
        println!();
        println!("Pi loads TypeScript extensions from ~/.pi/agent/extensions/ on next start.");
        println!("Restart Pi for lifecycle capture and MCP tools to take effect.");
    }
    Ok(())
}

fn render_pi_extension(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    capture_mode: &str,
) -> Result<()> {
    println!(
        "// Pi extension — write to {}",
        pi_extension_hint_in(std::env::var_os("PI_CODING_AGENT_DIR"))
    );
    println!("// Or re-run with `--apply` to install it automatically.");
    println!("// Restart Pi after changing extensions; MCP tools are bridged by this file.");
    println!();
    println!(
        "{}",
        build_pi_extension(server_url, auth_token, project_strategy, capture_mode)
    );
    Ok(())
}

fn resolve_pi_extension_path(args: &InstallHooksArgs) -> Result<PathBuf> {
    if let Some(p) = &args.config_file {
        return Ok(p.clone());
    }
    pi_extension_path()
}

fn build_pi_extension(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    capture_mode: &str,
) -> String {
    let lifecycle = build_omp_extension(server_url, auth_token, project_strategy, capture_mode)
        .replace("install-hooks --agent omp --apply", "install-hooks --agent pi --apply")
        .replace("const AGENT = \"omp\";", "const AGENT = \"pi\";")
        .replace(
            r#"
  api.on("session.compacting", (_event: any, ctx: any) => {
    postPreCompact(ctx);
  });
"#,
            "\n",
        )
        .replace(
            "export default function SessionMunchExtension(api: any): void {",
            &format!(
                "{}\nexport default function SessionMunchExtension(pi: any): void {{\n  try {{ void bootstrapMcpBridge(pi); }} catch (_e) {{}}",
                pi_mcp_bridge_source()
            ),
        )
        .replace("api.on(\"", "pi.on(\"");
    debug_assert!(!lifecycle.contains(".omp"));
    lifecycle
}

fn pi_mcp_bridge_source() -> &'static str {
    include_str!("../../templates/plugins/pi_mcp_bridge.ts")
}

fn build_omp_extension(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    capture_mode: &str,
) -> String {
    let token_line = auth_token
        .map(|t| format!("const TOKEN: string | null = {};\n", ts_string_literal(t)))
        .unwrap_or_else(|| "const TOKEN: string | null = null;\n".to_string());
    let apply_marker_params = ts_apply_marker_params(project_strategy);
    let capture_policy = ts_capture_policy_v1(capture_mode);
    let body = include_str!("../../templates/plugins/omp_extension.ts")
        .replace("__SESSIONMUNCH_CAPTURE_POLICY__", &capture_policy)
        .replace("__SESSIONMUNCH_APPLY_MARKER_PARAMS__", &apply_marker_params)
        .replace(
            "__SESSIONMUNCH_SERVER_LITERAL__",
            &ts_string_literal(server_url),
        )
        .replace("__SESSIONMUNCH_TOKEN_LINE__", &token_line);
    // Anchors are compile-time constants over the bundled template;
    // a mismatch is a template edit that forgot the spool patch.
    add_hook_spooling(body).expect("generated TS integration lost its hook-spool anchors")
}

fn render_agent(
    label: &str,
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    event_lists: &[&[(&str, &str)]],
) -> Result<()> {
    print!(
        "{}",
        render_agent_output(
            label,
            hooks_dir,
            server_url,
            auth_token,
            project_strategy,
            event_lists,
        )
    );
    Ok(())
}

fn render_agent_output(
    label: &str,
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    event_lists: &[&[(&str, &str)]],
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# {label} hook scripts (manual install — wire each to the matching event)\n"
    ));
    out.push_str(&format!("# Hook scripts: {}\n", hooks_dir.display()));
    out.push_str(&format!("# AI-memory server URL: {server_url}\n"));
    if auth_token.is_some() {
        out.push_str("# Auth: set SESSIONMUNCH_AUTH_TOKEN in each hook's environment to the\n");
        out.push_str("#       value passed via --auth-token (omitted from this printout).\n");
    } else {
        out.push_str("# Auth: server requires no bearer token. To require one, generate a\n");
        out.push_str("#       token with `sessionmunch generate-auth-token` and pass it via\n");
        out.push_str("#       --auth-token here AND set SESSIONMUNCH_AUTH_TOKEN on the server.\n");
    }
    out.push('\n');
    for events in event_lists {
        for (_, script) in *events {
            let script = hook_script_for_current_platform(script);
            out.push_str(&format!(
                "- {}\n",
                hooks_dir.join(script.as_ref()).display()
            ));
        }
    }
    out.push('\n');
    out.push_str("Set SESSIONMUNCH_HOOK_URL in each hook's environment to override the default.\n");
    if let Some(instruction) = manual_agent_project_strategy_instruction(project_strategy) {
        out.push_str(&instruction);
        out.push('\n');
    }
    if label == "antigravity-cli" {
        out.push('\n');
        out.push_str(ANTIGRAVITY_FINALIZATION_GUIDANCE);
    }
    out
}

fn manual_agent_project_strategy_instruction(project_strategy: Option<&str>) -> Option<String> {
    project_strategy.map(|strategy| {
        format!(
            "Set SESSIONMUNCH_PROJECT_STRATEGY={strategy} in each hook's environment to use the requested project strategy."
        )
    })
}

/// Copy the bundled hook scripts to a stable user-global location
/// and return that location. The path the agent's config file
/// references is THIS path, not the source bundle's path.
///
/// Why this matters:
///
/// - **Project-portability.** The previous behaviour wrote the
///   repo-relative path (e.g. `/mnt/data/Projects/sessionmunch/hooks/
///   claude-code/session-start.sh`) into the agent's settings.
///   Any agent CLI started from a different project — or in a
///   filesystem sandbox that didn't whitelist that path — failed
///   the SessionStart hook with "No such file or directory".
///
/// - **Docker-image upgrades.** Users who installed via the docker
///   image had paths under `/usr/local/share/sessionmunch/hooks/`
///   baked into their settings — paths only valid INSIDE the
///   container. Staging copies the scripts OUT to the host's
///   `~/.local/share/sessionmunch/hooks/` so the host-side agent can
///   actually reach them.
///
/// - **Updates.** When a new docker image ships with updated hook
///   scripts, the user re-runs `install-hooks --apply` and the
///   stage step overwrites the previous copies. No special
///   `update-hooks` command, no version-tracking dance.
///
/// Errors propagate when source is missing, the staging dir
/// can't be created, or any file copy fails.
fn stage_hook_scripts(source_dir: &Path, agent_label: &str, data_dir: &Path) -> Result<PathBuf> {
    stage_hook_scripts_in(
        source_dir,
        agent_label,
        &hook_staging_root(data_dir, sessionmunch_wiki::backup::running_in_container()),
    )
}

/// Where hook scripts are staged — which is also the path written into
/// the agent's settings, so it must be reachable by the process that
/// EXECUTES the hooks: the host-side agent CLI.
///
/// - **Native installs**: the resolved data dir (`--data-dir` /
///   `SESSIONMUNCH_DATA_DIR` / platform default), per #554.
/// - **Inside a container** (the docker wrapper): the server data dir is
///   a volume (`/data`) the host cannot execute from — staging there
///   wrote container-only paths into the host's settings and every hook
///   failed silently (#581). The wrapper binds `$HOME:$HOME` and reads
///   staged hooks from `~/.local/share/sessionmunch/hooks` (its
///   `HOOKS_STAGE_DIR` contract), so the home-based default is the
///   host-reachable location there.
pub(crate) fn hook_staging_root(data_dir: &Path, in_container: bool) -> PathBuf {
    if in_container {
        dirs::data_local_dir()
            .map(|d| d.join("sessionmunch"))
            .unwrap_or_else(|| data_dir.to_path_buf())
    } else {
        data_dir.to_path_buf()
    }
}

fn stage_hook_scripts_in(source_dir: &Path, agent_label: &str, data_dir: &Path) -> Result<PathBuf> {
    // `<data_dir>/hooks/<agent>`, not `<platform default>/sessionmunch/hooks/…`.
    // Byte-identical for a default install, since `default_data_dir()` *is*
    // `data_local_dir()/sessionmunch` — but it now follows `--data-dir` and
    // `SESSIONMUNCH_DATA_DIR` instead of populating a directory the operator is
    // not using (#554).
    let dest_root = data_dir.join("hooks").join(agent_label);

    fs::create_dir_all(&dest_root)
        .with_context(|| format!("creating staging dir {}", dest_root.display()))?;

    // When `resolve_hooks_dir` falls through to the data-local
    // candidate (e.g. docker `setup-agent` already extracted the
    // bundle into ~/.local/share/sessionmunch/hooks/<agent>/, or a prior
    // install left scripts in place), the source dir IS the
    // destination dir. The wipe-then-copy flow below would delete the
    // very scripts we mean to install before reading them, leaving 0
    // copied and a settings.json pointing at an empty directory
    // (issue #52). Detect that case via canonical paths and verify
    // the existing layout in place instead of touching it.
    let same_path = same_canonical_dir(source_dir, &dest_root);

    if !same_path {
        // Wipe any previously-staged scripts that the current bundle
        // no longer ships. Idempotent re-runs against an old install
        // shouldn't leave stale entries pointed at by nothing.
        if let Ok(entries) = fs::read_dir(&dest_root) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() && is_hook_script_file(&p) {
                    fs::remove_file(&p).ok();
                }
            }
        }
    }

    let mut count = 0_usize;
    for entry in fs::read_dir(source_dir)
        .with_context(|| format!("reading source bundle {}", source_dir.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        if !from.is_file() || !is_hook_script_file(&from) {
            continue;
        }
        if !same_path {
            copy_hook_file(&from, &dest_root)?;
        }
        count += 1;
    }

    if !same_path {
        copy_support_hook_scripts(source_dir, &dest_root)?;

        // Stage the shared `_lib.sh` helper alongside the event scripts so
        // they can `. "$(dirname "$0")/_lib.sh"` without depending on the
        // user's PATH or repo layout. The helper lives ONCE in
        // `hooks/_lib.sh` (one parent up from the agent-specific dir) —
        // staging it here is what keeps every agent's runtime view
        // consistent with the source of truth.
        if let Some(shared) = source_dir.parent().map(|p| p.join("_lib.sh"))
            && shared.is_file()
        {
            copy_hook_file(&shared, &dest_root)?;
        }
    }

    if count == 0 {
        anyhow::bail!(
            "no hook scripts found at {}.\n\
             Refusing to install — pointing the agent's settings at an empty \
             directory would silently disable all capture. Either pass \
             `--hooks-dir <path>` to point at a populated source tree, or run \
             `sessionmunch setup-agent --agent <name>` first to extract the \
             bundled scripts.",
            source_dir.display()
        );
    }

    let verb = if same_path { "verified" } else { "staged" };
    eprintln!("✓ {verb} {count} hook script(s) → {}", dest_root.display());
    Ok(dest_root)
}

/// `true` when `a` and `b` resolve to the same directory after symlink
/// canonicalization. Falls back to literal `==` if either canonicalize
/// call fails (e.g. dest hasn't been created yet on Windows, network
/// FS quirks). The caller has already `create_dir_all`'d both ends
/// in the staging flow, so the fast path almost always wins.
fn same_canonical_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// Copy a single hook file (event script or shared `_lib.sh`) into the
/// staging dir, preserving the executable bit on Unix. Centralised so
/// the script bulk-copy and the `_lib.sh` companion follow the same
/// rules without duplicating permission-handling.
fn copy_hook_file(from: &Path, dest_root: &Path) -> Result<()> {
    let to = dest_root.join(from.file_name().context("bad source file name")?);
    fs::copy(from, &to)
        .with_context(|| format!("copying {} → {}", from.display(), to.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&to)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&to, perms)?;
    }
    Ok(())
}

/// Copy the optional `lib/` support directory (currently PowerShell
/// helpers for Windows hook parity) alongside the event scripts.
/// No-op when the source bundle doesn't ship it.
fn copy_support_hook_scripts(source_dir: &Path, dest_root: &Path) -> Result<()> {
    let Some(source_hooks_root) = source_dir.parent() else {
        return Ok(());
    };
    let source_lib = source_hooks_root.join("lib");
    if !source_lib.is_dir() {
        return Ok(());
    }
    let Some(dest_hooks_root) = dest_root.parent() else {
        return Ok(());
    };
    let dest_lib = dest_hooks_root.join("lib");
    fs::create_dir_all(&dest_lib)
        .with_context(|| format!("creating hook support dir {}", dest_lib.display()))?;
    for entry in fs::read_dir(&source_lib)
        .with_context(|| format!("reading hook support dir {}", source_lib.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        if !from.is_file() || from.extension().and_then(|s| s.to_str()) != Some("ps1") {
            continue;
        }
        let to = dest_lib.join(from.file_name().context("bad support file name")?);
        fs::copy(&from, &to)
            .with_context(|| format!("copying {} → {}", from.display(), to.display()))?;
    }
    Ok(())
}

fn staged_command_dir(staged: &Path, agent_label: &str) -> PathBuf {
    match std::env::var("SESSIONMUNCH_HOOKS_HOST_ROOT") {
        Ok(root) if !root.trim().is_empty() => PathBuf::from(root).join(agent_label),
        _ => staged.to_path_buf(),
    }
}

fn is_hook_script_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("sh" | "ps1")
    )
}

fn resolve_hooks_dir(
    explicit: Option<&Path>,
    agent: AgentChoice,
    data_dir: &Path,
) -> Result<PathBuf> {
    let Some(sub) = agent.script_hook_subdir() else {
        anyhow::bail!("{agent:?} uses a generated integration, not a hook script directory")
    };
    if let Some(p) = explicit {
        let path = p.join(sub);
        if path.is_dir() {
            return Ok(path);
        }
        anyhow::bail!("hooks directory {} does not exist", path.display());
    }

    // Probe candidates in order. The first dir that exists wins.
    let candidates = hook_source_candidates(
        sub,
        repo_root_guess(),
        exe_dir_guess(),
        Some(data_dir.to_path_buf()),
    );
    for path in &candidates {
        if !path.as_os_str().is_empty() && path.is_dir() {
            return Ok(path.clone());
        }
    }
    anyhow::bail!(
        "could not locate hooks directory. Tried: {candidates:?}. \
         Pass --hooks-dir <directory containing {sub}/> to point at the bundle explicitly.",
    );
}

fn hook_source_candidates(
    sub: &str,
    repo_root: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(5);
    // Cargo-run from the repo.
    if let Some(root) = repo_root {
        candidates.push(root.join("hooks").join(sub));
    }
    // Release tarball (macOS/Windows/Linux archive): the `hooks/` bundle
    // ships in the same directory as the binary, so it's reachable without
    // `--source` (issue #107).
    if let Some(dir) = exe_dir {
        candidates.push(dir.join("hooks").join(sub));
    }
    // Docker image lays them out under /usr/local/share/sessionmunch/.
    candidates.push(PathBuf::from(format!(
        "/usr/local/share/sessionmunch/hooks/{sub}"
    )));
    // Native Linux packages install hook sources under /usr/share.
    candidates.push(PathBuf::from(format!(
        "/usr/share/sessionmunch/hooks/{sub}"
    )));
    // Local install honourable mention: the bundle a previous `--apply`, or
    // docker `setup-agent`, staged under the data dir actually in use.
    if let Some(dir) = data_dir {
        candidates.push(dir.join("hooks").join(sub));
    }
    candidates
}

fn repo_root_guess() -> Option<PathBuf> {
    repo_root_from_exe(&current_exe_resolved()?)
}

/// When the binary lives under target/{debug,release}/<name>, the
/// workspace root is two parents up.
fn repo_root_from_exe(exe: &Path) -> Option<PathBuf> {
    Some(exe.parent()?.parent()?.parent()?.to_path_buf())
}

/// Directory the running binary lives in. The release tarball ships the
/// `hooks/` bundle right next to the binary, so a no-`--source`
/// `install-hooks` finds it there (issue #107).
fn exe_dir_guess() -> Option<PathBuf> {
    Some(current_exe_resolved()?.parent()?.to_path_buf())
}

/// Path of the running binary with symlinks resolved.
///
/// Both guesses above are relative to where the binary *lives*, not to how
/// it was invoked. `~/.local/bin/sessionmunch -> <repo>/target/release/sessionmunch`
/// is the layout the quick start suggests, and without this the repo-root
/// guess walks up from the link's own directory to `$HOME` while the tarball
/// guess stops at `~/.local/bin` — leaving the bundled `hooks/` unreachable
/// from every candidate, whatever the cwd (issue #546).
fn current_exe_resolved() -> Option<PathBuf> {
    std::env::current_exe().ok().map(resolve_exe_path)
}

/// Canonicalise an executable path, keeping the original when the target
/// cannot be resolved (deleted binary, or a filesystem that refuses).
fn resolve_exe_path(exe: PathBuf) -> PathBuf {
    fs::canonicalize(&exe).unwrap_or(exe)
}

// CLAUDE_CODE_EVENTS + build_claude_code_payload now live in
// `super::render_shared`, shared with `setup-agent`.

/// Which optional capture surfaces the generated Claude Code settings should
/// include.
///
/// Grouped rather than passed as two positional bools: at the call site
/// `true, false` said nothing about which surface was which, and the pair is
/// exactly the privacy-relevant part of this install — worth naming.
#[derive(Debug, Clone, Copy)]
struct ClaudeCaptureScope {
    /// Include the assistant-message capture hook (double opt-in, off by
    /// default).
    assistant: bool,
    /// Include the `UserPromptSubmit` hook. When false the entry is omitted
    /// entirely, so the agent never emits prompt text at all.
    prompts: bool,
}

fn render_claude_code(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    settings_path: &Path,
    capture: ClaudeCaptureScope,
) -> Result<()> {
    let ClaudeCaptureScope {
        assistant: capture_assistant,
        prompts: capture_prompts,
    } = capture;
    // Soft check: warn (don't bail) if a script is missing. The user
    // may be running this command inside docker against a host path
    // that exists only on the host's filesystem — bailing would
    // sabotage the docker-only flow `setup-agent` enables.
    for (event, script) in super::render_shared::CLAUDE_CODE_EVENTS {
        if !capture_prompts && event == CLAUDE_PROMPT_EVENT {
            continue;
        }
        let script = hook_script_for_claude_code(script);
        let abs = hooks_dir.join(script.as_ref());
        if !abs.exists() {
            eprintln!(
                "# warning: {} not present on this filesystem. \
                 If this command is running inside docker against a \
                 host path, you can ignore this; otherwise extract \
                 the scripts first with `sessionmunch setup-agent`.",
                abs.display()
            );
        }
    }
    let payload = configure_claude_prompt_capture(
        build_claude_code_payload_with_data_dir(
            hooks_dir,
            server_url,
            auth_token,
            Some(data_dir),
            project_strategy,
            capture_assistant,
        ),
        capture_prompts,
    );
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing claude code hook config")?;
    println!(
        "# Claude Code hook config — merge into {}",
        settings_path.display()
    );
    println!("# Hook scripts: {}", hooks_dir.display());
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: SESSIONMUNCH_AUTH_TOKEN embedded in each hook command below.");
        println!(
            "#       Treat {} as sensitive (chmod 600).",
            settings_path.display()
        );
    }
    println!();
    println!("{serialized}");
    Ok(())
}

fn render_grok(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    // Soft check (same rationale as render_claude_code): warn, don't bail,
    // so the docker host-path flow still works.
    for (_, script) in super::render_shared::CLAUDE_CODE_EVENTS {
        let script = hook_script_for_claude_code(script);
        let abs = hooks_dir.join(script.as_ref());
        if !abs.exists() {
            eprintln!(
                "# warning: {} not present on this filesystem. \
                 If this command is running inside docker against a \
                 host path, you can ignore this; otherwise extract \
                 the scripts first with `sessionmunch setup-agent`.",
                abs.display()
            );
        }
    }
    let payload = build_grok_payload_with_data_dir(
        hooks_dir,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing grok hook config")?;
    let config_path = grok_hooks_path()?;
    println!(
        "# Grok Build CLI hook config — write to {}",
        config_path.display()
    );
    println!("# Hook scripts: {}", hooks_dir.display());
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: SESSIONMUNCH_AUTH_TOKEN embedded in each hook command below.");
        println!(
            "#       Treat {} as sensitive (chmod 600).",
            config_path.display()
        );
    }
    println!("# NOTE: Grok ignores hook stdout on SessionStart — capture works,");
    println!("#       but handoff injection does not. Recover a prior session's");
    println!("#       handoff via the MCP `memory_handoff_accept` tool.");
    println!();
    println!("{serialized}");
    Ok(())
}

/// Merge sessionmunch's lifecycle hooks into Zero's user-level
/// `~/.config/zero/hooks.json` (issue #156). Zero executes hook entries
/// directly (`command` + `args`, JSON payload on stdin) — no scripts to
/// stage, no shell. Only entries whose `id` carries the `sessionmunch-`
/// prefix are replaced, so third-party hooks in the same file survive
/// re-installs and version upgrades. The file-level `enabled` flag is
/// preserved when the file already exists (a user who disabled hooks
/// globally keeps that choice — we warn instead of overriding).
fn apply_to_zero_hooks(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => zero_hooks_path()?,
    };
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let payload = super::render_shared::build_zero_hooks_config(
        server_url,
        auth_token,
        Some(data_dir),
        strategy,
    );
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_array())
        .context("internal: build_zero_hooks_config didn't return a hooks array")?
        .clone();
    let mut hooks_disabled = false;
    let outcome = apply_atomic(&path, |existing| {
        mutate_json(existing, |root| {
            hooks_disabled = root.get("enabled").and_then(|v| v.as_bool()) == Some(false);
            if !root.contains_key("enabled") {
                root.insert("enabled".into(), serde_json::Value::Bool(true));
            }
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Array(Vec::new()))
                .as_array_mut()
                .context("`hooks` is present in hooks.json but not an array")?;
            hooks.retain(|hook| {
                !hook
                    .get("id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|id| id.starts_with("sessionmunch-"))
            });
            hooks.extend(our_hooks.iter().cloned());
            Ok(())
        })
    })?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    if hooks_disabled {
        eprintln!(
            "# warning: this hooks.json sets \"enabled\": false at the top level, \
             so Zero will not run ANY hooks (including sessionmunch's) until you \
             re-enable them."
        );
    }
    println!("# NOTE: Zero discards sessionStart hook stdout — capture works, but");
    println!("#       handoff injection does not. Recover a prior session's handoff");
    println!("#       via the MCP `memory_handoff_accept` tool.");
    Ok(())
}

/// Print Zero's hooks.json to stdout (dry-run counterpart of
/// [`apply_to_zero_hooks`]).
fn render_zero(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    let payload = super::render_shared::build_zero_hooks_config(
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing zero hook config")?;
    println!("# Zero hook config — merge into ~/.config/zero/hooks.json");
    println!("# ($XDG_CONFIG_HOME/zero/hooks.json on non-default XDG setups), or");
    println!("# re-run with --apply to merge it in place, preserving other hooks.");
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: token embedded in each hook's args below.");
        println!("#       Treat hooks.json as sensitive (chmod 600).");
    }
    println!("# NOTE: Zero discards sessionStart hook stdout — capture works, but");
    println!("#       handoff injection does not. Recover a prior session's handoff");
    println!("#       via the MCP `memory_handoff_accept` tool.");
    println!();
    println!("{serialized}");
    Ok(())
}

/// Whether a hook entry in ZCode's `hooks.events` map is one sessionmunch
/// wrote. Our entries carry a `statusMessage` starting with `sessionmunch`
/// — the only schema-documented free-form key, so ownership is marked
/// without emitting keys ZCode would drop.
fn is_our_zcode_hook(hook: &serde_json::Value) -> bool {
    hook.get("statusMessage")
        .and_then(|v| v.as_str())
        .is_some_and(|msg| msg.starts_with("sessionmunch"))
}

/// Withdraw sessionmunch's own hook entries from every matcher group in
/// `groups`, leaving each group, and anyone else's hooks inside it, in
/// place.
fn strip_our_zcode_hook_entries(groups: &mut [serde_json::Value]) {
    for group in groups.iter_mut() {
        if let Some(hooks) = group
            .as_object_mut()
            .and_then(|group| group.get_mut("hooks"))
            .and_then(|v| v.as_array_mut())
        {
            hooks.retain(|hook| !is_our_zcode_hook(hook));
        }
    }
}

/// Same, for a `hooks.events` slot of unknown shape. Returns how many
/// entries were withdrawn so the caller can report them; a slot that is
/// not an array is left untouched and counts zero.
fn strip_our_zcode_hooks(slot: &mut serde_json::Value) -> usize {
    let Some(groups) = slot.as_array_mut() else {
        return 0;
    };
    let before = zcode_hook_count(groups);
    strip_our_zcode_hook_entries(groups);
    before.saturating_sub(zcode_hook_count(groups))
}

/// Total hook entries across every matcher group in `groups`, ignoring
/// groups whose `hooks` is absent or not an array.
fn zcode_hook_count(groups: &[serde_json::Value]) -> usize {
    groups
        .iter()
        .filter_map(|group| group.get("hooks").and_then(|v| v.as_array()))
        .map(|hooks| hooks.len())
        .sum()
}

/// True when a `hooks.events` entry cannot run anything: no matcher groups
/// at all, no group carrying a non-empty `hooks` array, or a value that is
/// not a matcher-group array in the first place. Such a key is pure
/// downside under ZCode's strict validation, since it runs nothing and can
/// cost the user every other hook in the block.
///
/// Deliberately narrower than "a key sessionmunch does not write": several of
/// those are real ZCode events this tool skips on purpose (see
/// `PermissionRequest` in `render_shared`), and a key still running
/// someone's hook is their working config, not a leftover. Reporting on
/// that set would be a false-positive generator.
fn zcode_event_runs_nothing(slot: &serde_json::Value) -> bool {
    match slot.as_array() {
        Some(groups) => zcode_hook_count(groups) == 0,
        None => true,
    }
}

/// Merge sessionmunch hooks into the root `hooks` block of ZCode's
/// user-scope `~/.zcode/cli/config.json` (#512). Sibling config keys
/// (`model`, `provider`, `mcp.servers`, …) always survive: the merge
/// touches only `hooks.enabled` (defaulted, never flipped),
/// `hooks.maxOutputBytes` (set only when absent), and `hooks.events`,
/// replaces entries sessionmunch owns (idempotent re-apply), and never
/// removes a hook it did not write. Entries it owns are withdrawn
/// wherever they sit, including under event keys this version no longer
/// writes; event keys themselves are never deleted, only reported when
/// sessionmunch withdrew entries from them, or when they are left unable to
/// run anything (#600). The one thing it does
/// discard is a matcher group left holding an empty `hooks` array under a
/// key it writes, which drops a caller's own empty group along with the
/// ones its withdrawal emptied.
fn apply_to_zcode_hooks(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => zcode_config_path()?,
    };
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let payload = super::render_shared::build_zcode_hooks_config(
        server_url,
        auth_token,
        Some(data_dir),
        strategy,
    );
    let our_events = payload
        .get("events")
        .and_then(|v| v.as_object())
        .context("internal: build_zcode_hooks_config didn't return an events map")?
        .clone();
    let mut hooks_disabled = false;
    let mut stale: Vec<(String, usize, bool)> = Vec::new();
    let outcome = apply_atomic(&path, |existing| {
        mutate_json(existing, |root| {
            // `hooks` sits beside sibling config keys that must survive.
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .context("`hooks` is present in the ZCode config but not an object")?;
            hooks_disabled = hooks.get("enabled").and_then(|v| v.as_bool()) == Some(false);
            if !hooks.contains_key("enabled") {
                hooks.insert("enabled".into(), serde_json::Value::Bool(true));
            }
            // Raise the stdout ceiling above ZCode's 32 KiB default so a
            // fetched handoff is not truncated mid-JSON before injection.
            // Only when the user has not chosen their own value: the key
            // is block-level and would also bound third-party hooks.
            if !hooks.contains_key("maxOutputBytes")
                && let Some(ceiling) = payload.get("maxOutputBytes")
            {
                hooks.insert("maxOutputBytes".into(), ceiling.clone());
            }
            let events = hooks
                .entry("events")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .context("`hooks.events` is present in the ZCode config but not an object")?;
            for (event, ours) in &our_events {
                let ours = ours
                    .as_array()
                    .context("internal: zcode event payload is not a matcher-group array")?;
                let slot = events
                    .entry(event.clone())
                    .or_insert_with(|| serde_json::Value::Array(Vec::new()));
                let slot = slot.as_array_mut().context(
                    "`hooks.events.{event}` is present in the ZCode config but not an array",
                )?;
                strip_our_zcode_hook_entries(slot);
                slot.retain(|group| match group.get("hooks") {
                    Some(serde_json::Value::Array(hooks)) => !hooks.is_empty(),
                    _ => true,
                });
                slot.extend(ours.iter().cloned());
            }
            // sessionmunch writes only the six keys in `ZCODE_EVENTS`, but its
            // own entries can sit under other keys too (a config carried
            // over from another agent, or hand-edited). Those are ours to
            // withdraw; the keys themselves are not, so every one is left
            // in place. What makes a leftover dangerous is ZCode strict-
            // validating this map: one key it does not recognize and it
            // rejects the ENTIRE hooks block, sessionmunch's included, so
            // capture dies while the file still matches byte for byte and
            // apply reports it as up to date (#600). Collect the keys that
            // can no longer run anything and say so after the write.
            for (event, slot) in events.iter_mut() {
                if our_events.contains_key(event) {
                    continue;
                }
                let withdrawn = strip_our_zcode_hooks(slot);
                let runs_nothing = zcode_event_runs_nothing(slot);
                if withdrawn > 0 || runs_nothing {
                    stale.push((event.clone(), withdrawn, runs_nothing));
                }
            }
            Ok(())
        })
    })?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            // Only claim the install is current when nothing turned up
            // that can stop ZCode loading it. Saying "already up to date"
            // over a block ZCode rejects is the whole of #600, and the
            // notes explaining it go to stderr, so a redirected stdout
            // would otherwise carry the reassurance and none of the cause.
            ApplyOutcome::NoOp if stale.is_empty() => "already up to date",
            ApplyOutcome::NoOp => "unchanged; see the notes below",
        }
    );
    if hooks_disabled {
        eprintln!(
            "# warning: this config sets \"enabled\": false inside `hooks`, \
             so ZCode will not run ANY hooks (including sessionmunch's) until \
             you re-enable them."
        );
    }
    if !stale.is_empty() {
        // The report above goes to stdout and these go to stderr; stdout
        // block-buffers when redirected, so without this the two arrive out
        // of order in a log or in CI.
        let _ = std::io::stdout().flush();
    }
    for (event, withdrawn, runs_nothing) in &stale {
        if *withdrawn > 0 {
            // A withdrawal is positive evidence sessionmunch once lived under
            // this key, so the risk here is not hypothetical.
            eprintln!(
                "# warning: withdrew {withdrawn} sessionmunch hook(s) from \
                 `hooks.events.{event}`, a key sessionmunch does not write. The \
                 key itself was left as found, but ZCode validates this map \
                 strictly and rejects the ENTIRE hooks block, sessionmunch's \
                 included, over one key it does not recognize, which leaves \
                 capture silently dead. If ZCode reports `hookCount: 0`, \
                 remove that key from {}.",
                path.display()
            );
        } else if *runs_nothing {
            eprintln!(
                // Only the provenance claim degrades between the two
                // tiers; the consequence is identical, so state it at full
                // strength. "If ZCode does not recognize it" keeps this
                // honest about keys ZCode does accept, such as an empty
                // `Notification`.
                "# note: `hooks.events.{event}` runs nothing, and sessionmunch \
                 does not write that key. If ZCode does not recognize it, \
                 ZCode rejects the ENTIRE hooks block, sessionmunch's included, \
                 and capture is dead while this file looks correct. If ZCode \
                 reports `hookCount: 0`, remove that key from {}.",
                path.display()
            );
        }
    }
    println!("# NOTE: ZCode injects SessionStart stdout as model context, so the");
    println!("#       prior session's handoff is delivered automatically.");
    println!("# NOTE: ZCode fires `Stop` at the end of every turn and has no");
    println!("#       SessionEnd — close sessions with");
    println!("#       `sessionmunch finalize-session --agent zcode`.");
    Ok(())
}

/// Print ZCode's `hooks` block to stdout (dry-run counterpart of
/// [`apply_to_zcode_hooks`]).
fn render_zcode(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    let payload = super::render_shared::build_zcode_hooks_config(
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing zcode hook config")?;
    println!("# ZCode (z.ai) hooks config — merge the `hooks` block into");
    println!("# ~/.zcode/cli/config.json (the same file `install-mcp --client");
    println!("# zcode` registers MCP servers in), or re-run with --apply to");
    println!("# merge it in place, preserving the rest of the config.");
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: this printed snippet embeds the token in each hook's args");
        println!("#       so a hand-placed config works as-is — treat it as");
        println!("#       sensitive (chmod 600). NOTE: `--apply` does NOT embed the");
        println!("#       token; it persists it to <data-dir>/auth-token (0600) and");
        println!("#       writes token-less args, so the applied config differs from");
        println!("#       this snippet by design (#552, #600).");
    }
    println!("# NOTE: ZCode injects SessionStart stdout as model context, so the");
    println!("#       prior session's handoff is delivered automatically.");
    println!("# NOTE: ZCode fires `Stop` at the end of every turn and has no");
    println!("#       SessionEnd — close sessions with");
    println!("#       `sessionmunch finalize-session --agent zcode`.");
    println!();
    println!("{serialized}");
    Ok(())
}

fn render_devin(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    // Soft check (same rationale as render_claude_code): warn, don't bail,
    // so the docker host-path flow still works.
    for (_, script) in super::render_shared::DEVIN_EVENTS {
        let script = hook_script_for_claude_code(script);
        let abs = hooks_dir.join(script.as_ref());
        if !abs.exists() {
            eprintln!(
                "# warning: {} not present on this filesystem. \
                 If this command is running inside docker against a \
                 host path, you can ignore this; otherwise extract \
                 the scripts first with `sessionmunch setup-agent`.",
                abs.display()
            );
        }
    }
    let payload = build_devin_payload_with_data_dir(
        hooks_dir,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing devin hook config")?;
    println!(
        "# Devin CLI hook config — write to ~/.devin/hooks.v1.json or ~/.devin/config.json hooks key"
    );
    println!("# Hook scripts: {}", hooks_dir.display());
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: SESSIONMUNCH_AUTH_TOKEN embedded in each hook command below.");
        println!(
            "#       Treat ~/.devin/hooks.v1.json or ~/.devin/config.json as sensitive (chmod 600)."
        );
    }
    println!(
        "# NOTE: Devin consumes the handoff via hookSpecificOutput.additionalContext on SessionStart."
    );
    println!();
    println!("{serialized}");
    Ok(())
}

/// Print Kimi Code's `[[hooks]]` TOML fragment to stdout (dry-run
/// counterpart of [`apply_to_kimi_code_config`]). Rendered through the
/// same upsert path the apply mode uses, so the printout is byte-for-
/// byte what `--apply` would append to config.toml.
fn render_kimi_code(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    // Soft check (same rationale as render_claude_code): warn, don't bail,
    // so the docker host-path flow still works.
    for (_, script) in KIMI_CODE_EVENTS {
        let script = hook_script_for_current_platform(script);
        let abs = hooks_dir.join(script.as_ref());
        if !abs.exists() {
            eprintln!(
                "# warning: {} not present on this filesystem. \
                 If this command is running inside docker against a \
                 host path, you can ignore this; otherwise extract \
                 the scripts first with `sessionmunch setup-agent`.",
                abs.display()
            );
        }
    }
    let commands = kimi_code_hook_commands(
        hooks_dir,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let mut doc = toml_edit::DocumentMut::new();
    upsert_kimi_code_hooks(&mut doc, &commands)?;
    println!("# Kimi Code hook config — merge into $KIMI_CODE_HOME/config.toml");
    println!("# (~/.kimi-code/config.toml when KIMI_CODE_HOME is unset), or re-run");
    println!("# with --apply to merge it in place, preserving providers/model and");
    println!("# any third-party [[hooks]] entries already in the file.");
    println!("# Hook scripts: {}", hooks_dir.display());
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: SESSIONMUNCH_AUTH_TOKEN embedded in each hook command below.");
        println!("#       Treat config.toml as sensitive (chmod 600).");
    }
    println!();
    print!("{doc}");
    Ok(())
}

fn render_kiro_cli(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    // Soft check (same rationale as render_claude_code): warn, don't bail,
    // so the docker host-path flow still works.
    for (_, script) in KIRO_CLI_V2_EVENTS {
        let script = hook_script_for_current_platform(script);
        let abs = hooks_dir.join(script.as_ref());
        if !abs.exists() {
            eprintln!(
                "# warning: {} not present on this filesystem. \
                 If this command is running inside docker against a \
                 host path, you can ignore this; otherwise extract \
                 the scripts first with `sessionmunch setup-agent`.",
                abs.display()
            );
        }
    }
    let hooks = build_kiro_cli_v2_hooks_value(
        hooks_dir,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    println!("// Kiro CLI v2-engine hooks — merge the \"hooks\" object into each");
    println!("// agent config under $KIRO_HOME/agents (~/.kiro/agents when unset),");
    println!("// or re-run with --apply to merge into every existing agent config,");
    println!("// preserving third-party hook entries. The v2 engine fires hooks");
    println!("// only for the active agent config; the built-in default agent has");
    println!("// no file, so create one first (`kiro-cli agent create`). Do not reuse");
    println!("// this v2 registration for v3; use `--agent kiro-cli-v3` instead.");
    println!("// Hook scripts: {}", hooks_dir.display());
    println!("// AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("// Auth: SESSIONMUNCH_AUTH_TOKEN embedded in each hook command below.");
        println!("//       Treat agent configs as sensitive (chmod 600).");
    }
    println!();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({ "hooks": hooks }))
            .expect("static hook payload serializes")
    );
    Ok(())
}

fn render_kiro_cli_v3(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    for (_, script) in KIRO_CLI_V3_EVENTS {
        let abs = hooks_dir.join(hook_script_for_current_platform(script).as_ref());
        if !abs.exists() {
            eprintln!(
                "# warning: {} not present on this filesystem. If this command is running \
                 inside docker against a host path, you can ignore this; otherwise extract \
                 the scripts first with `sessionmunch setup-agent`.",
                abs.display()
            );
        }
    }
    let payload = build_kiro_cli_v3_hooks_value(
        hooks_dir,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    println!("// Kiro CLI v3 hooks — write this document to");
    println!("// $KIRO_HOME/hooks/sessionmunch.json (~/.kiro/hooks/sessionmunch.json when unset),");
    println!("// or re-run with --apply for an atomic, idempotent merge.");
    println!("// Hook scripts: {}", hooks_dir.display());
    println!("// AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("// Auth: SESSIONMUNCH_AUTH_TOKEN embedded in each hook command below.");
        println!("//       Treat the hook file as sensitive (chmod 600).");
    }
    println!();
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).expect("static hook payload serializes")
    );
    Ok(())
}

/// Print Pool's `.poolside/settings.yaml` `hooks:` block to stdout. Pool's
/// hook config lives at the root of each repository the agent runs in, so
/// there is no user-global file for an atomic merge — the snippet is pasted
/// per project. `--apply` still stages the scripts to the stable user-global
/// location first so the printed commands reference paths that outlive the
/// source checkout / docker image (see [`apply_to_pool`]).
fn render_pool(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    // Soft check (same rationale as render_claude_code): warn, don't bail,
    // so the docker host-path flow still works.
    for (_, script) in POOL_EVENTS {
        let script = hook_script_for_current_platform(script);
        let abs = hooks_dir.join(script.as_ref());
        if !abs.exists() {
            eprintln!(
                "# warning: {} not present on this filesystem. \
                 If this command is running inside docker against a \
                 host path, you can ignore this; otherwise extract \
                 the scripts first with `sessionmunch setup-agent`.",
                abs.display()
            );
        }
    }
    print!(
        "{}",
        render_pool_output(
            hooks_dir,
            server_url,
            auth_token,
            data_dir,
            project_strategy
        )
    );
    Ok(())
}

fn render_pool_output(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> String {
    let snippet = build_pool_settings_yaml_with_data_dir(
        hooks_dir,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let mut out = String::new();
    out.push_str("# Pool (Poolside Agent CLI) hook config — merge into the repo-root\n");
    out.push_str("# .poolside/settings.yaml of each project Pool runs in. sessionmunch\n");
    out.push_str("# does not write project-local files, so paste this snippet manually\n");
    out.push_str("# (re-run with --apply first to stage the scripts to a stable path).\n");
    out.push_str(&format!("# Hook scripts: {}\n", hooks_dir.display()));
    out.push_str(&format!("# AI-memory server URL: {server_url}\n"));
    if auth_token.is_some() {
        out.push_str("# Auth: SESSIONMUNCH_AUTH_TOKEN embedded in each hook command below.\n");
        out.push_str("#       Treat .poolside/settings.yaml as sensitive (chmod 600).\n");
    }
    out.push_str("# NOTE: Pool's SessionStart stdout injection is not demonstrated —\n");
    out.push_str("#       capture works, but handoff injection does not. Recover a prior\n");
    out.push_str("#       session's handoff via the MCP `memory_handoff_accept` tool.\n");
    out.push_str("# NOTE: Pool has no true session-end event; `Stop` is a turn boundary.\n");
    out.push_str(
        "#       Close a finished session with `sessionmunch finalize-session --agent pool`.\n",
    );
    out.push('\n');
    out.push_str(&snippet);
    out
}

/// `--apply` for Pool: stage the hook scripts to the stable user-global
/// location (the same step every other script agent's apply performs), then
/// print the ready-to-paste snippet pointing at the staged copies. The
/// project-local `.poolside/settings.yaml` itself is deliberately NOT
/// written: install-hooks is a user-scoped operation and must not mutate
/// files inside the user's repositories.
fn apply_to_pool(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts(hooks_dir, "pool", data_dir)?;
    let command_dir = staged_command_dir(&staged, "pool");
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    print!(
        "{}",
        render_pool_output(&command_dir, server_url, auth_token, data_dir, strategy)
    );
    Ok(())
}

#[cfg(test)]
mod tests;
