//! Recognise Claude Code runs that are a model call, not a session.
//!
//! Agent harnesses that reach Claude through Claude Code (Hermes' subscription
//! plugin, Agent SDK callers) start one `claude -p --no-session-persistence`
//! process per model call. Lifecycle hooks fire in each of them, so without this
//! check every call registers a one-message "session", and the SessionStart
//! handoff injection changes the prompt prefix on every call, which defeats the
//! caller's prompt cache. The harness keeps its own history (Hermes is
//! imported separately), so skipping these loses nothing.
//!
//! Two signals, either one sufficient:
//! - the Claude Code process (`CLAUDE_PID`, else an ancestor of this hook) was
//!   started with `--no-session-persistence`;
//! - on `Stop` / `SessionEnd`, the payload names a transcript that was never
//!   written. A persisted session has written it by then.

use std::ffi::OsString;
use std::path::Path;

use sessionmunch_core::AgentKind;
use sessionmunch_hooks::HookEvent;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::host_env::HostEnv;

/// The Claude Code flag that disables transcript persistence.
const NO_PERSISTENCE_FLAG: &str = "--no-session-persistence";

/// How many ancestors to inspect when `CLAUDE_PID` is absent. The hook is
/// usually run through a shell (`test -x … && sessionmunch hook …`), so the
/// Claude process is the parent or grandparent.
const MAX_ANCESTORS: usize = 4;

/// True when this hook fired inside a Claude Code run that is a single model
/// call rather than a session worth recording.
pub(crate) fn is_ephemeral_claude_run(
    host: &HostEnv,
    agent: AgentKind,
    event: HookEvent,
    raw: &serde_json::Value,
) -> bool {
    if agent != AgentKind::ClaudeCode {
        return false;
    }
    if matches!(event, HookEvent::Stop | HookEvent::SessionEnd) && transcript_never_written(raw) {
        return true;
    }
    claude_process_args(host)
        .iter()
        .any(|args| args_disable_persistence(args))
}

/// True when `args` contain the no-persistence flag.
pub(crate) fn args_disable_persistence(args: &[OsString]) -> bool {
    args.iter().any(|arg| arg == NO_PERSISTENCE_FLAG)
}

/// True when the payload names an absolute transcript path that does not
/// exist. Payloads without a transcript path (other harness shapes, older
/// Claude builds) are not treated as ephemeral.
pub(crate) fn transcript_never_written(raw: &serde_json::Value) -> bool {
    raw.get("transcript_path")
        .and_then(serde_json::Value::as_str)
        .map(Path::new)
        .is_some_and(|path| path.is_absolute() && !path.exists())
}

/// Command lines of the Claude Code process: `CLAUDE_PID` when set, else this
/// process's nearest ancestors.
fn claude_process_args(host: &HostEnv) -> Vec<Vec<OsString>> {
    let refresh = ProcessRefreshKind::new().with_cmd(UpdateKind::Always);
    let mut sys = System::new();
    let cmd_of = |sys: &mut System, pid: Pid| -> Option<(Vec<OsString>, Option<Pid>)> {
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, refresh);
        sys.process(pid)
            .map(|process| (process.cmd().to_vec(), process.parent()))
    };
    if let Some(pid) = host
        .var("CLAUDE_PID")
        .and_then(|raw| raw.trim().parse::<u32>().ok())
    {
        return cmd_of(&mut sys, Pid::from_u32(pid))
            .map(|(args, _)| vec![args])
            .unwrap_or_default();
    }
    let mut found = Vec::new();
    let mut next =
        cmd_of(&mut sys, Pid::from_u32(std::process::id())).and_then(|(_, parent)| parent);
    for _ in 0..MAX_ANCESTORS {
        let Some(pid) = next else { break };
        let Some((args, parent)) = cmd_of(&mut sys, pid) else {
            break;
        };
        found.push(args);
        next = parent;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_persistence_flag_marks_a_model_call() {
        assert!(args_disable_persistence(&args(&[
            "claude",
            "-p",
            "--max-turns",
            "1",
            "--no-session-persistence",
        ])));
        assert!(!args_disable_persistence(&args(&["claude", "-p", "hello"])));
        assert!(!args_disable_persistence(&args(&["claude"])));
    }

    #[test]
    fn missing_transcript_marks_a_model_call() {
        let tmp = tempfile::TempDir::new().unwrap();
        let written = tmp.path().join("written.jsonl");
        std::fs::write(&written, "{}\n").unwrap();
        let never = tmp.path().join("never.jsonl");

        let payload = |path: &Path| serde_json::json!({ "transcript_path": path });
        assert!(!transcript_never_written(&payload(&written)));
        assert!(transcript_never_written(&payload(&never)));
        // No path, or a relative one, is not evidence either way.
        assert!(!transcript_never_written(&serde_json::json!({})));
        assert!(!transcript_never_written(
            &serde_json::json!({ "transcript_path": "never.jsonl" })
        ));
    }

    #[test]
    fn only_claude_code_runs_are_classified() {
        let host = HostEnv::default();
        let tmp = tempfile::TempDir::new().unwrap();
        let raw = serde_json::json!({ "transcript_path": tmp.path().join("never.jsonl") });
        assert!(!is_ephemeral_claude_run(
            &host,
            AgentKind::Codex,
            HookEvent::SessionEnd,
            &raw
        ));
        assert!(is_ephemeral_claude_run(
            &host,
            AgentKind::ClaudeCode,
            HookEvent::SessionEnd,
            &raw
        ));
    }
}
