//! Deterministic session digest: facts extracted from a session's events and
//! a bounded Markdown card rendered from them.
//!
//! Nothing here calls a model. The same events and native signals always
//! give the same facts and the same card. Facts are navigation metadata that
//! point back at the events they came from (`e<N>`, see
//! [`DigestEvent::number`]); they never replace those events, and the card
//! says what it could not observe instead of guessing.
//!
//! The hub renders the card into `sessions/<id>.md` from the session event
//! ledger, where `eN` is the ledger sequence number.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
#[cfg(test)]
use std::io::{BufRead as _, BufReader};
#[cfg(test)]
use std::path::Path;

#[cfg(test)]
use anyhow::{Context as _, Result};
#[cfg(test)]
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};
use serde::Serialize;
use serde_json::{Value, json};
use sessionmunch_core::{NewWorkstreamEvent, SessionEvent, WorkstreamEventKind};

#[cfg(test)]
use crate::harness::ManagedHarness;
#[cfg(test)]
use crate::hermes_history::HermesSession;
#[cfg(test)]
use crate::transcript::ExportedTranscript;

/// Version of the card builder. A card whose frontmatter records an older
/// version is regenerated.
pub const CARD_BUILDER_VERSION: u32 = 1;

/// The fields of an event the digest reads.
pub trait DigestEvent {
    /// Semantic event family.
    fn kind(&self) -> WorkstreamEventKind;
    /// Message role, when any.
    fn role(&self) -> Option<&str>;
    /// Sanitized content.
    fn content(&self) -> &str;
    /// Adapter metadata.
    fn metadata(&self) -> &Value;
    /// Source timestamp, when known.
    fn occurred_at(&self) -> Option<&str>;
    /// The number a card cites this event by (`eN`), given its position.
    fn number(&self, index: usize) -> usize {
        index + 1
    }
}

impl DigestEvent for NewWorkstreamEvent {
    fn kind(&self) -> WorkstreamEventKind {
        self.kind
    }
    fn role(&self) -> Option<&str> {
        self.role.as_deref()
    }
    fn content(&self) -> &str {
        &self.content
    }
    fn metadata(&self) -> &Value {
        &self.metadata
    }
    fn occurred_at(&self) -> Option<&str> {
        self.occurred_at.as_deref()
    }
}

/// Ledger rows are cited by their sequence, which is stable as the session
/// grows, so a card's references keep pointing at the same events.
impl DigestEvent for SessionEvent {
    fn kind(&self) -> WorkstreamEventKind {
        self.kind
    }
    fn role(&self) -> Option<&str> {
        self.role.as_deref()
    }
    fn content(&self) -> &str {
        &self.content
    }
    fn metadata(&self) -> &Value {
        &self.metadata
    }
    fn occurred_at(&self) -> Option<&str> {
        self.occurred_at.as_deref()
    }
    fn number(&self, _index: usize) -> usize {
        usize::try_from(self.sequence).unwrap_or(0)
    }
}

/// Longest final reply quoted verbatim in a card before it is cut with a
/// visible marker pointing at the event.
const HEADLINE_CHARS: usize = 2_000;
/// Longest single request line in a card.
const REQUEST_CHARS: usize = 240;
const MAX_CARD_REQUESTS: usize = 12;
const MAX_CARD_FILES: usize = 30;
const MAX_CARD_CHECKS: usize = 10;
const MAX_CARD_SUBAGENTS: usize = 8;
/// Bytes of a tool result searched for a reported exit code.
const EXIT_SCAN_BYTES: usize = 400;

/// Position of an event in [`ExportedTranscript::events`], 1-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct EventRef(pub usize);

impl std::fmt::Display for EventRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "e{}", self.0)
    }
}

/// Kind of a [`Fact`]. The payload shape is fixed per kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    /// A user turn that asks for something. Payload: `text`.
    Request,
    /// A short nudge such as "continue" or "check for updates". Payload: `text`.
    Continuation,
    /// Terminal output or a tool banner pasted into a user turn. It may still
    /// be an implicit request (a pasted compiler error). Payload: `first_line`.
    PastedOutput,
    /// One tool invocation. Payload: `tool`, `class`.
    ToolCall,
    /// A file written, edited or deleted. Payload: `path`, `via`, `verb`.
    FileTouch,
    /// A shell command with its observed exit. Payload: `command`, `exit`,
    /// `exit_basis`.
    Command,
    /// A command that looks like a test, build, lint or CI check. Payload:
    /// `command`, `outcome`.
    Check,
    /// A git commit seen in tool output or a successful commit command.
    /// Payload: `sha` (or null), `branch`, `message`, `basis`.
    Commit,
    /// A git push range seen in tool output. Payload: `range`, `dest`.
    Push,
    /// A delegated subagent/task invocation. Payload: `tool`, `description`.
    Subagent,
    /// The last substantive assistant message. Payload: `text`, `chars`.
    FinalReply,
    /// A title the harness itself wrote. Payload: `text`.
    NativeTitle,
    /// A recap/summary the harness itself wrote. Payload: `text`.
    NativeRecap,
    /// How the session ended, judged from the last event or the last native
    /// terminal marker only. Payload: `state`, `basis`.
    Ended,
}

/// One extracted fact with the events it came from.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Fact {
    /// What the fact is.
    pub kind: FactKind,
    /// Kind-specific data; see [`FactKind`].
    pub payload: Value,
    /// Events that support it.
    pub refs: Vec<EventRef>,
}

/// How a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndState {
    /// Ended on an assistant reply or a native completion marker.
    Clean,
    /// Interrupted, aborted or reaped.
    Interrupted,
    /// Stopped by a provider usage or rate limit.
    UsageLimit,
    /// Nothing observed says how it ended.
    Unknown,
}

impl EndState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Interrupted => "interrupted",
            Self::UsageLimit => "usage_limit",
            Self::Unknown => "unknown",
        }
    }
}

/// Last terminal marker a native store recorded outside the portable stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalSignal {
    /// End state the marker implies.
    pub state: EndState,
    /// The marker as recorded, e.g. `codex turn_aborted: interrupted`.
    pub detail: String,
}

/// Native-store records the portable event stream deliberately leaves out:
/// harness-written titles and recaps, and turn/session end markers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NativeSignals {
    /// Harness-written session title.
    pub title: Option<String>,
    /// Harness-written recap or away summary (the latest one).
    pub recap: Option<String>,
    /// The last terminal marker in the store.
    pub terminal: Option<TerminalSignal>,
}

/// Facts for one session plus the context a card needs.
#[derive(Clone, Debug, Serialize)]
pub struct SessionDigest {
    /// Harness that produced the session.
    pub harness: String,
    /// Native session id.
    pub native_session_id: String,
    /// Earliest event timestamp seen.
    pub first_at: Option<String>,
    /// Latest event timestamp seen.
    pub last_at: Option<String>,
    /// Number of exported events.
    pub event_count: usize,
    /// User turns dropped as harness scaffolding (reminders, command echoes).
    pub scaffolding_dropped: usize,
    /// Tool results that could not be paired with a call.
    pub unpaired_results: usize,
    /// Extracted facts in event order.
    pub facts: Vec<Fact>,
    /// Distinct reader loss notes carried from the export.
    pub losses: Vec<String>,
}

impl SessionDigest {
    /// Facts of one kind, in order.
    pub fn of(&self, kind: FactKind) -> impl Iterator<Item = &Fact> {
        self.facts.iter().filter(move |fact| fact.kind == kind)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolClass {
    Shell,
    CodeExec,
    Read,
    Edit,
    Search,
    Web,
    Subagent,
    Mcp,
    Other,
}

impl ToolClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::CodeExec => "code_exec",
            Self::Read => "read",
            Self::Edit => "edit",
            Self::Search => "search",
            Self::Web => "web",
            Self::Subagent => "subagent",
            Self::Mcp => "mcp",
            Self::Other => "other",
        }
    }

    fn of(name: &str) -> Self {
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("mcp__") || lower == "call_mcp_tool" {
            return Self::Mcp;
        }
        match lower.as_str() {
            "bash" | "shell" | "shell_command" | "exec_command" | "local_shell_call"
            | "terminal" | "run_command" | "run_shell_command" | "powershell" => Self::Shell,
            "exec" => Self::CodeExec,
            "read" | "read_file" | "view_file" | "notebookread" | "read_many_files" => Self::Read,
            "edit"
            | "write"
            | "multiedit"
            | "notebookedit"
            | "apply_patch"
            | "write_file"
            | "patch"
            | "replace_file_content"
            | "multi_replace_file_content"
            | "write_to_file"
            | "str_replace_based_edit_tool"
            | "create_file" => Self::Edit,
            "grep" | "glob" | "search_files" | "grep_search" | "find_by_name" | "list_dir"
            | "ls" | "codebase_search" => Self::Search,
            "webfetch" | "websearch" | "web_search" | "web_extract" | "read_url_content"
            | "fetch" => Self::Web,
            "task" | "agent" | "spawn_agent" | "delegate_task" | "subagent" => Self::Subagent,
            _ => Self::Other,
        }
    }
}

struct Call {
    at: EventRef,
    id: Option<String>,
    name: String,
    class: ToolClass,
    command: Option<String>,
    input: Value,
    raw_input: String,
}

/// Extract facts from one exported transcript. `harness` is only a label.
#[cfg(test)]
#[must_use]
pub fn extract_transcript_facts(
    harness: &str,
    transcript: &ExportedTranscript,
    signals: &NativeSignals,
) -> SessionDigest {
    extract_facts(
        harness,
        &transcript.native_session_id,
        &transcript.events,
        &transcript.losses,
        signals,
    )
}

/// Extract facts from a session's events, in order. `harness` is only a
/// label; `losses` are the reader's notes about data it excluded.
#[must_use]
pub fn extract_facts<E: DigestEvent>(
    harness: &str,
    native_session_id: &str,
    events: &[E],
    losses: &[String],
    signals: &NativeSignals,
) -> SessionDigest {
    let mut facts = Vec::new();
    let mut scaffolding_dropped = 0;
    let mut unpaired_results = 0;
    let mut pending: VecDeque<Call> = VecDeque::new();
    let mut files: BTreeMap<String, usize> = BTreeMap::new();
    let mut commits: BTreeMap<String, usize> = BTreeMap::new();
    let mut final_reply: Option<(EventRef, &str)> = None;
    let mut last_request: Option<EventRef> = None;

    for (index, event) in events.iter().enumerate() {
        let at = EventRef(event.number(index));
        match (event.kind(), event.role()) {
            (WorkstreamEventKind::Message, Some("user")) => {
                match classify_user_text(event.content()) {
                    UserText::Request(text) => {
                        last_request = Some(at);
                        facts.push(fact(FactKind::Request, json!({"text": text}), at));
                    }
                    UserText::Continuation(text) => {
                        facts.push(fact(FactKind::Continuation, json!({"text": text}), at));
                    }
                    // Pasted output usually answers or extends the request before it.
                    UserText::Pasted(first_line) => facts.push(fact(
                        FactKind::PastedOutput,
                        json!({"first_line": first_line, "folds_into": last_request.map(|r| r.0)}),
                        at,
                    )),
                    UserText::Scaffolding | UserText::Interrupt => scaffolding_dropped += 1,
                }
            }
            (WorkstreamEventKind::Message, Some("assistant")) => {
                let text = event.content().trim();
                if !text.is_empty() && !is_usage_limit(text) {
                    final_reply = Some((at, text));
                }
            }
            (WorkstreamEventKind::ToolCall, _) => {
                let call = parse_call(at, event);
                facts.push(fact(
                    FactKind::ToolCall,
                    json!({"tool": call.name, "class": call.class.as_str()}),
                    at,
                ));
                if call.class == ToolClass::Subagent {
                    let description = first_str(
                        &call.input,
                        &["description", "prompt", "task", "message", "goal"],
                    )
                    .map_or_else(String::new, |text| one_line(text, REQUEST_CHARS));
                    facts.push(fact(
                        FactKind::Subagent,
                        json!({"tool": call.name, "description": description}),
                        at,
                    ));
                }
                for path in edit_paths(&call) {
                    push_file(&mut facts, &mut files, &path, "edit_tool", "edit", at);
                }
                pending.push_back(call);
            }
            (WorkstreamEventKind::ToolResult, _) => {
                // Match by native id when both sides kept one (some readers
                // keep it only on the call); otherwise results arrive in call order.
                let position = pairing_id(event)
                    .and_then(|id| pending.iter().position(|c| c.id.as_deref() == Some(id)))
                    .unwrap_or(0);
                let call = pending.remove(position);
                let Some(call) = call else {
                    unpaired_results += 1;
                    scan_git_output(&mut facts, &mut commits, event.content(), at);
                    continue;
                };
                let call_ref = call.at;
                if let Some(command) = &call.command {
                    let (exit, basis) = exit_of(event, call.class);
                    facts.push(Fact {
                        kind: FactKind::Command,
                        payload: json!({
                            "command": one_line(command, REQUEST_CHARS),
                            "exit": exit,
                            "exit_basis": basis,
                        }),
                        refs: vec![call_ref, at],
                    });
                    if let Some(check) = check_kind(command) {
                        let outcome = match exit {
                            Some(0) => "passed",
                            Some(_) => "failed",
                            None if basis == "is_error" => "failed",
                            None => result_outcome(event.content()),
                        };
                        facts.push(Fact {
                            kind: FactKind::Check,
                            payload: json!({
                                "command": one_line(command, REQUEST_CHARS),
                                "check": check,
                                "outcome": outcome,
                            }),
                            refs: vec![call_ref, at],
                        });
                    }
                    if exit.is_none_or(|code| code == 0) && basis != "is_error" {
                        for (path, verb) in shell_writes(command) {
                            push_file(&mut facts, &mut files, &path, "shell", verb, call_ref);
                        }
                        let found = scan_git_output(&mut facts, &mut commits, event.content(), at);
                        // A quiet commit is only evidence once the command is
                        // known to have exited cleanly; a still-running or
                        // unreported process proves nothing.
                        if found == 0 && exit == Some(0) && is_git_commit(command) {
                            let message = commit_message(command);
                            let key = format!("cmd:{}", call.at.0);
                            if commits.insert(key, call.at.0).is_none() {
                                facts.push(Fact {
                                    kind: FactKind::Commit,
                                    payload: json!({
                                        "sha": Value::Null,
                                        "branch": Value::Null,
                                        "message": message,
                                        "basis": "command_succeeded_without_output",
                                    }),
                                    refs: vec![call_ref, at],
                                });
                            }
                        }
                    }
                } else {
                    scan_git_output(&mut facts, &mut commits, event.content(), at);
                }
            }
            _ => {}
        }
    }

    // Calls whose result never reached the export still ran a command.
    for call in pending {
        if let Some(command) = &call.command {
            facts.push(fact(
                FactKind::Command,
                json!({
                    "command": one_line(command, REQUEST_CHARS),
                    "exit": Value::Null,
                    "exit_basis": "result_not_exported",
                }),
                call.at,
            ));
        }
    }

    if let Some((at, text)) = final_reply {
        facts.push(fact(
            FactKind::FinalReply,
            json!({"text": text, "chars": text.chars().count()}),
            at,
        ));
    }
    if let Some(title) = &signals.title {
        facts.push(Fact {
            kind: FactKind::NativeTitle,
            payload: json!({"text": one_line(title, REQUEST_CHARS)}),
            refs: Vec::new(),
        });
    }
    if let Some(recap) = &signals.recap {
        facts.push(Fact {
            kind: FactKind::NativeRecap,
            payload: json!({"text": recap}),
            refs: Vec::new(),
        });
    }
    let (state, basis, refs) = ended(events, signals);
    facts.push(Fact {
        kind: FactKind::Ended,
        payload: json!({"state": state.as_str(), "basis": basis}),
        refs,
    });

    let first_at = events
        .iter()
        .filter_map(|e| e.occurred_at())
        .min()
        .map(str::to_string);
    let last_at = events
        .iter()
        .filter_map(|e| e.occurred_at())
        .max()
        .map(str::to_string);
    let mut losses = losses.to_vec();
    losses.sort();
    losses.dedup();
    SessionDigest {
        harness: harness.to_string(),
        native_session_id: native_session_id.to_string(),
        first_at,
        last_at,
        event_count: events.len(),
        scaffolding_dropped,
        unpaired_results,
        facts,
        losses,
    }
}

/// Native id that links a tool result to its call, when the reader kept one.
fn pairing_id(event: &impl DigestEvent) -> Option<&str> {
    ["tool_use_id", "call_id", "tool_call_id"]
        .iter()
        .find_map(|key| event.metadata().get(key).and_then(Value::as_str))
}

fn fact(kind: FactKind, payload: Value, at: EventRef) -> Fact {
    Fact {
        kind,
        payload,
        refs: vec![at],
    }
}

/// Temporary files, unexpanded shell variables and loop variables (`x`, `j`)
/// are not outcomes worth a card line.
fn is_scratch(path: &str) -> bool {
    path.starts_with("/tmp/")
        || path.starts_with("/var/tmp/")
        || path.contains('$')
        || !(path.contains('/') || path.contains('.'))
}

fn push_file(
    facts: &mut Vec<Fact>,
    seen: &mut BTreeMap<String, usize>,
    path: &str,
    via: &str,
    verb: &str,
    at: EventRef,
) {
    if is_scratch(path) {
        return;
    }
    if let Some(&position) = seen.get(path) {
        if let Some(existing) = facts.get_mut(position)
            && !existing.refs.contains(&at)
        {
            existing.refs.push(at);
        }
        return;
    }
    seen.insert(path.to_string(), facts.len());
    facts.push(fact(
        FactKind::FileTouch,
        json!({"path": path, "via": via, "verb": verb}),
        at,
    ));
}

// ---------------------------------------------------------------- user turns

enum UserText {
    Request(String),
    Continuation(String),
    Pasted(String),
    Scaffolding,
    Interrupt,
}

const SCAFFOLDING_PREFIXES: &[&str] = &[
    "<local-command",
    "<command-name>",
    "<command-message>",
    "<command-args>",
    "Caveat:",
    "<task-notification>",
    "<user_shell_command>",
    "<turn_aborted>",
    "<system-reminder>",
    "<environment_context>",
    "<user_instructions>",
    "Base directory for this skill",
    "This session is being continued from a previous conversation",
];

const CONTINUATIONS: &[&str] = &[
    "continue",
    "contine",
    "contiue",
    "keep going",
    "keep trying",
    "go",
    "go ahead",
    "go on",
    "yes",
    "y",
    "ok",
    "okay",
    "proceed",
    "try again",
    "do it",
    "check for updates",
    "%check for updates",
    "failed",
    "still failing",
];

/// Glyphs that start lines of TUI frames, logs and tool banners.
const LOG_GLYPHS: &[char] = &[
    '│', '┃', '○', '●', '▕', '▐', '╭', '╰', '─', '▣', '⣿', '█', '┌', '└', '├',
];
/// Substrings that mark a turn as pasted tool output rather than prose.
const LOG_MARKERS: &[&str] = &[
    "[ℹ]",
    "[✔]",
    "[✘]",
    "bridge$",
    "e: file://",
    "Traceback (most recent call last)",
    "error[E",
    "BUILD FAILED",
    "▣ DCP",
    "Exit code",
    "command not found",
    "Status code:",
    "Error body",
    "\"code\":",
];

fn classify_user_text(raw: &str) -> UserText {
    let text = unwrap_user_request(raw);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return UserText::Scaffolding;
    }
    if trimmed.starts_with("[Request interrupted") {
        return UserText::Interrupt;
    }
    if SCAFFOLDING_PREFIXES.iter().any(|p| trimmed.starts_with(p)) {
        return UserText::Scaffolding;
    }
    let normalized = trimmed.trim_end_matches(['.', '!']).to_lowercase();
    let slash_command = trimmed.starts_with('/') && !trimmed.contains(char::is_whitespace);
    if slash_command || CONTINUATIONS.contains(&normalized.as_str()) {
        return UserText::Continuation(trimmed.to_string());
    }
    if looks_pasted(trimmed) {
        let first = trimmed
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default();
        return UserText::Pasted(one_line(first, REQUEST_CHARS));
    }
    UserText::Request(strip_frame(trimmed))
}

/// Drop TUI border glyphs that come along when a prompt is copied from a
/// terminal frame, keeping the prose.
fn strip_frame(text: &str) -> String {
    text.lines()
        .map(|line| {
            line.chars()
                .filter(|c| !LOG_GLYPHS.contains(c) && !matches!(c, '▕' | '▐' | '›' | '❯'))
                .collect::<String>()
                .trim()
                .to_string()
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Antigravity wraps real input in `<USER_REQUEST>` and appends metadata.
fn unwrap_user_request(raw: &str) -> &str {
    let Some(start) = raw.find("<USER_REQUEST>") else {
        return raw;
    };
    let body = &raw[start + "<USER_REQUEST>".len()..];
    body.find("</USER_REQUEST>")
        .map_or(body, |end| &body[..end])
}

fn looks_pasted(text: &str) -> bool {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return false;
    }
    let glyph_lines = lines
        .iter()
        .filter(|line| line.starts_with(LOG_GLYPHS) || line.ends_with(['▕', '▐', '│']))
        .count();
    let marked = LOG_MARKERS.iter().any(|marker| text.contains(marker));
    let glyph_heavy = glyph_lines * 2 >= lines.len();
    // Prose framed by a TUI border is still a request: only call it pasted
    // output when it also carries a log marker, or has almost no words.
    let words = word_count(text);
    (marked && (glyph_heavy || words < 40)) || (glyph_heavy && words < 6)
}

fn word_count(text: &str) -> usize {
    text.split_whitespace()
        .filter(|word| word.chars().filter(|c| c.is_alphabetic()).count() >= 2)
        .count()
}

fn is_usage_limit(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("hit your session limit")
        || lower.contains("you've hit your limit")
        || lower.contains("usage limit reached")
        || lower.contains("usage_limit_reached")
        || lower.contains("you've reached your usage limit")
}

// ---------------------------------------------------------------- tool calls

fn parse_call(at: EventRef, event: &impl DigestEvent) -> Call {
    let content = event.content();
    let name = event
        .metadata()
        .get("tool")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| content.split_once(": ").map(|(name, _)| name.to_string()))
        .unwrap_or_else(|| "tool".to_string());
    let raw_input = content
        .strip_prefix(name.as_str())
        .and_then(|rest| rest.strip_prefix(": ").or_else(|| rest.strip_prefix(' ')))
        .unwrap_or(content)
        .to_string();
    let input: Value = serde_json::from_str(&raw_input).unwrap_or(Value::Null);
    let class = ToolClass::of(&name);
    let command = match class {
        ToolClass::Shell => shell_command(&input).or_else(|| {
            // Some adapters keep the raw command string, not JSON.
            (!raw_input.trim_start().starts_with('{')).then(|| raw_input.clone())
        }),
        ToolClass::CodeExec => {
            let inner = js_cmd_literals(&raw_input);
            (!inner.is_empty()).then(|| inner.join(" && "))
        }
        _ => None,
    };
    Call {
        at,
        id: pairing_id(event).map(str::to_string),
        name,
        class,
        command,
        input,
        raw_input,
    }
}

fn shell_command(input: &Value) -> Option<String> {
    let value = ["command", "cmd", "CommandLine", "commandLine", "script"]
        .iter()
        .find_map(|key| input.get(key))?;
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let parts: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            let parts = match parts.as_slice() {
                [shell, flag, rest @ ..] if shell.ends_with("sh") && flag.starts_with('-') => rest,
                all => all,
            };
            Some(parts.join(" "))
        }
        _ => None,
    }
}

/// `cmd: "..."` string literals inside Codex code-mode JavaScript.
fn js_cmd_literals(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = source;
    while let Some(position) = rest.find("cmd") {
        rest = &rest[position + 3..];
        let after = rest.trim_start_matches(['"', '\'']).trim_start();
        let Some(after) = after.strip_prefix(':') else {
            continue;
        };
        let after = after.trim_start();
        let Some(quote) = after
            .chars()
            .next()
            .filter(|c| matches!(c, '"' | '\'' | '`'))
        else {
            continue;
        };
        let mut literal = String::new();
        let mut escaped = false;
        for c in after[quote.len_utf8()..].chars() {
            if escaped {
                literal.push(match c {
                    'n' => '\n',
                    't' => '\t',
                    other => other,
                });
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == quote {
                break;
            } else {
                literal.push(c);
            }
        }
        if !literal.is_empty() {
            found.push(literal);
        }
    }
    found
}

fn first_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
}

fn edit_paths(call: &Call) -> Vec<String> {
    let mut paths = Vec::new();
    let patch_source = if call.class == ToolClass::Edit {
        if let Some(path) = first_str(
            &call.input,
            &[
                "file_path",
                "filePath",
                "path",
                "notebook_path",
                "TargetFile",
                "target_file",
            ],
        ) {
            paths.push(path.to_string());
        }
        Some(first_str(&call.input, &["input", "patch"]).map_or(call.raw_input.as_str(), |s| s))
    } else if call.class == ToolClass::CodeExec {
        // Code-mode JavaScript passes patches as string literals.
        Some(call.raw_input.as_str())
    } else {
        call.command.as_deref()
    };
    if let Some(source) = patch_source {
        for marker in ["*** Update File: ", "*** Add File: ", "*** Delete File: "] {
            for (position, _) in source.match_indices(marker) {
                let rest = &source[position + marker.len()..];
                // Ends at a newline, or at an escaped one inside a literal.
                let end = rest
                    .find(|c: char| c.is_whitespace() || matches!(c, '\\' | '"' | '\'' | '`'))
                    .unwrap_or(rest.len());
                let path = rest[..end].to_string();
                if !path.is_empty() && !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }
    paths
}

/// Exit code reported by a tool result, and how it was learned.
fn exit_of(event: &impl DigestEvent, class: ToolClass) -> (Option<i64>, &'static str) {
    let head = truncate_chars(event.content(), EXIT_SCAN_BYTES);
    for marker in [
        "Exit code:",
        "Exit code",
        "Process exited with code",
        "exit code",
    ] {
        if let Some(code) = number_after(head, marker) {
            return (Some(code), "reported");
        }
    }
    for marker in [
        "\"exit_code\":",
        "\\\"exit_code\\\":",
        "exit_code:",
        "\"exitCode\":",
    ] {
        if let Some(code) = number_after(event.content(), marker) {
            return (Some(code), "reported");
        }
    }
    if event.metadata().get("is_error").and_then(Value::as_bool) == Some(true) {
        return (None, "is_error");
    }
    if class == ToolClass::Shell
        && event.metadata().get("is_error").and_then(Value::as_bool) == Some(false)
    {
        // Claude marks failing Bash results is_error and prints no code on
        // success, so a clean flag is the success signal.
        return (Some(0), "inferred_from_is_error_false");
    }
    (None, "not_reported")
}

fn number_after(text: &str, marker: &str) -> Option<i64> {
    let position = text.find(marker)?;
    let rest = text[position + marker.len()..].trim_start();
    let digits: String = rest
        .chars()
        .enumerate()
        .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && *c == '-'))
        .map(|(_, c)| c)
        .collect();
    digits.parse().ok()
}

fn result_outcome(text: &str) -> &'static str {
    let head = truncate_chars(text, 4_000);
    if head.contains("BUILD FAILED")
        || head.contains("test result: FAILED")
        || head.contains("FAILED")
    {
        "failed"
    } else if head.contains("BUILD SUCCESSFUL") || head.contains("test result: ok") {
        "passed"
    } else {
        "unknown"
    }
}

/// `needle` occurs as whole words, not inside a longer name or path segment.
fn has_word(haystack: &str, needle: &str) -> bool {
    let edge =
        |c: Option<char>| c.is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.')));
    haystack.match_indices(needle).any(|(i, _)| {
        edge(haystack[..i].chars().next_back()) && edge(haystack[i + needle.len()..].chars().next())
    })
}

fn check_kind(command: &str) -> Option<&'static str> {
    let lower = command.to_lowercase();
    const TESTS: &[&str] = &[
        "cargo test",
        "cargo nextest",
        "cargo t",
        "cargo tf",
        "pytest",
        "unittest",
        "npm test",
        "npm run test",
        "pnpm test",
        "yarn test",
        "go test",
        "jest",
        "vitest",
        "ctest",
        "bun test",
        "deno test",
        "make test",
        "testdebugunittest",
        "connectedandroidtest",
    ];
    const LINTS: &[&str] = &[
        "cargo clippy",
        "cargo fmt",
        "eslint",
        "ruff",
        "mypy",
        "ktlint",
        "shellcheck",
        "bash -n",
        "npm run lint",
        "go vet",
        "detekt",
        "lintdebug",
    ];
    const BUILDS: &[&str] = &[
        "cargo build",
        "cargo check",
        "npm run build",
        "go build",
        "tsc",
        "assembledebug",
        "assemblerelease",
        "make check",
    ];
    const CI: &[&str] = &["gh pr checks", "gh run view", "gh run watch"];
    if TESTS.iter().any(|p| has_word(&lower, p)) {
        Some("test")
    } else if LINTS.iter().any(|p| has_word(&lower, p)) {
        Some("lint")
    } else if BUILDS.iter().any(|p| has_word(&lower, p)) {
        Some("build")
    } else if CI.iter().any(|p| has_word(&lower, p)) {
        Some("ci")
    } else {
        None
    }
}

// ----------------------------------------------------------------- shell IO

/// Files a shell command writes: redirections, `tee`, in-place `sed`/`perl`,
/// copy/move/install destinations, `touch`, and `rm` (as deletes).
fn shell_writes(command: &str) -> Vec<(String, &'static str)> {
    let mut writes: Vec<(String, &'static str)> = Vec::new();
    let mut add = |path: &str, verb: &'static str| {
        let path = path.trim_matches(['"', '\'']);
        if path.is_empty()
            || path.starts_with('&')
            || matches!(path, "/dev/null" | "/dev/stderr" | "/dev/stdout" | "-")
        {
            return;
        }
        if !writes.iter().any(|(p, _)| p == path) {
            writes.push((path.to_string(), verb));
        }
    };
    for segment in split_segments(command) {
        let tokens = tokenize(&segment);
        let mut index = 0;
        while index < tokens.len() {
            let token = tokens[index].as_str();
            let (op, attached) = if let Some(rest) = token.strip_prefix(">>") {
                (Some("append"), rest)
            } else if let Some(rest) = token.strip_prefix('>') {
                (Some("write"), rest)
            } else {
                (None, "")
            };
            if let Some(verb) = op {
                if attached.is_empty() {
                    if let Some(next) = tokens.get(index + 1) {
                        add(next, verb);
                        index += 1;
                    }
                } else if !attached.starts_with('&') {
                    add(attached, verb);
                }
            }
            index += 1;
        }
        let words: Vec<&str> = tokens
            .iter()
            .map(String::as_str)
            .take_while(|t| !t.starts_with('>') && *t != "<<" && !t.starts_with("<<"))
            .collect();
        let words: Vec<&str> = words
            .iter()
            .copied()
            .skip_while(|word| *word == "sudo" || word.contains('=') && !word.starts_with('-'))
            .collect();
        let Some((&program, args)) = words.split_first() else {
            continue;
        };
        let program = program.rsplit('/').next().unwrap_or(program);
        let operands: Vec<&str> = args
            .iter()
            .copied()
            .filter(|a| !a.starts_with('-'))
            .collect();
        match program {
            "tee" => operands.iter().for_each(|p| add(p, "write")),
            "sed" | "perl"
                if args.iter().any(|a| {
                    a.starts_with("-i") || a.starts_with("--in-place") || a.starts_with("-pi")
                }) =>
            {
                if let Some(path) = operands.last() {
                    add(path, "edit");
                }
            }
            "cp" | "mv" | "install" | "rsync" if operands.len() >= 2 => {
                if let Some(path) = operands.last() {
                    add(path, "write");
                }
            }
            "touch" => operands.iter().for_each(|p| add(p, "write")),
            "rm" => operands.iter().for_each(|p| add(p, "delete")),
            _ => {}
        }
    }
    writes
}

/// Split a command line on `&&`, `||`, `;`, `|` and newlines outside quotes.
/// Heredoc bodies are dropped so their lines are not read as commands.
fn split_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut heredoc_end: Option<String> = None;
    for line in command.lines() {
        if let Some(end) = &heredoc_end {
            if line.trim() == end {
                heredoc_end = None;
            }
            continue;
        }
        if let Some(position) = line.find("<<") {
            let tag = line[position + 2..]
                .trim_start_matches('-')
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches(['\'', '"'])
                .to_string();
            if !tag.is_empty() && !tag.starts_with('<') {
                heredoc_end = Some(tag);
            }
        }
        let mut current = String::new();
        let mut quote: Option<char> = None;
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None if c == '\'' || c == '"' => quote = Some(c),
                None if c == ';' || c == '|' || (c == '&' && chars.get(i + 1) == Some(&'&')) => {
                    segments.push(std::mem::take(&mut current));
                    if chars.get(i + 1) == Some(&c) {
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
                None => {}
            }
            current.push(c);
            i += 1;
        }
        segments.push(current);
    }
    segments.retain(|s| !s.trim().is_empty());
    segments
}

fn tokenize(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in segment.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    // `2>`, `1>` and `&>` redirect streams, not files of interest here.
    tokens
        .into_iter()
        .filter(|t| !(t.starts_with("2>") || t.starts_with("1>") || t.starts_with("&>")))
        .collect()
}

// -------------------------------------------------------------------- git

fn is_git_commit(command: &str) -> bool {
    split_segments(command).iter().any(|segment| {
        let tokens = tokenize(segment);
        let mut words = tokens.iter().map(String::as_str);
        words.next() == Some("git")
            && words.find(|w| !(*w == "-C" || w.starts_with('-') || w.contains('/')))
                == Some("commit")
    })
}

/// Message of a `git commit` command: `-m`/`-am`/`-qam`, `--message`, or the
/// first line of a `-F -` heredoc.
fn commit_message(command: &str) -> Value {
    let tokens = tokenize(command.lines().next().unwrap_or_default());
    let text = |m: &str| Value::String(one_line(m, 120));
    for (i, token) in tokens.iter().enumerate() {
        let short_m = token.starts_with('-') && !token.starts_with("--") && token.ends_with('m');
        if short_m || token == "--message" {
            return tokens.get(i + 1).map_or(Value::Null, |m| text(m));
        }
        if let Some(message) = token.strip_prefix("--message=") {
            return text(message);
        }
        if (token == "-F" || token == "--file") && tokens.get(i + 1).is_some_and(|f| f == "-") {
            return command
                .lines()
                .skip(1)
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map_or(Value::Null, text);
        }
    }
    Value::Null
}

/// Commits (`[branch sha] message`) and push ranges (`a..b  src -> dest`) in
/// tool output, including JSON-escaped output. Returns how many it found.
fn scan_git_output(
    facts: &mut Vec<Fact>,
    commits: &mut BTreeMap<String, usize>,
    text: &str,
    at: EventRef,
) -> usize {
    let unescaped;
    let text = if text.contains("\\n") {
        unescaped = text.replace("\\n", "\n");
        unescaped.as_str()
    } else {
        text
    };
    let mut found = 0;
    for line in text.lines() {
        let line = line.trim();
        // A commit line may sit inside JSON (`"output":"[main abc1234] ..."`).
        let commit = line
            .match_indices('[')
            .filter(|(i, _)| {
                line[..*i]
                    .chars()
                    .next_back()
                    .is_none_or(|c| c.is_whitespace() || matches!(c, '"' | '\''))
            })
            .find_map(|(i, _)| commit_line(&line[i..]));
        if let Some((branch, sha, message)) = commit {
            if commits.insert(sha.clone(), at.0).is_none() {
                facts.push(fact(
                    FactKind::Commit,
                    json!({"sha": sha, "branch": branch, "message": message, "basis": "git_output"}),
                    at,
                ));
            }
            found += 1;
        } else if let Some((range, dest)) = push_line(line) {
            let key = format!("push:{range}:{dest}");
            if commits.insert(key, at.0).is_none() {
                facts.push(fact(
                    FactKind::Push,
                    json!({"range": range, "dest": dest}),
                    at,
                ));
            }
            found += 1;
        }
    }
    found
}

fn is_hex(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_hexdigit())
}

fn commit_line(line: &str) -> Option<(String, String, String)> {
    let rest = line.strip_prefix('[')?;
    let (inside, message) = rest.split_once("] ")?;
    let mut parts: Vec<&str> = inside.split_whitespace().collect();
    let sha = parts.pop()?;
    if !(7..=40).contains(&sha.len()) || !is_hex(sha) {
        return None;
    }
    let branch = parts.first()?;
    if branch.is_empty() || parts.len() > 3 {
        return None;
    }
    let message = message.split(['"', '\\']).next().unwrap_or(message);
    Some((branch.to_string(), sha.to_string(), one_line(message, 120)))
}

fn push_line(line: &str) -> Option<(String, String)> {
    let line = line.trim_start_matches(['+', ' ', '*']).trim();
    let (left, dest) = line.split_once(" -> ")?;
    let mut words = left.split_whitespace();
    let range = words.next()?;
    let range = if range == "[new" {
        "new-branch".to_string()
    } else {
        let (a, b) = range.split_once("...").or_else(|| range.split_once(".."))?;
        if !(is_hex(a) && is_hex(b) && a.len() >= 7) {
            return None;
        }
        range.to_string()
    };
    let dest = dest.split_whitespace().next()?.to_string();
    Some((range, dest))
}

// ------------------------------------------------------------------- ended

fn ended<E: DigestEvent>(
    events: &[E],
    signals: &NativeSignals,
) -> (EndState, String, Vec<EventRef>) {
    let last = events.iter().enumerate().rev().find(|(_, e)| {
        !matches!(
            e.kind(),
            WorkstreamEventKind::Annotation | WorkstreamEventKind::Checkpoint
        )
    });
    let last_ref = last
        .map(|(i, e)| vec![EventRef(e.number(i))])
        .unwrap_or_default();
    let last_is_open_request = last.is_some_and(|(_, e)| {
        e.kind() == WorkstreamEventKind::Message
            && e.role() == Some("user")
            && matches!(classify_user_text(e.content()), UserText::Request(_))
    });
    if let Some(signal) = &signals.terminal {
        if signal.state == EndState::Clean && last_is_open_request {
            return (
                EndState::Unknown,
                format!("{} but the last user request has no reply", signal.detail),
                last_ref,
            );
        }
        return (signal.state, signal.detail.clone(), last_ref);
    }
    let Some((_, event)) = last else {
        return (EndState::Unknown, "no events".into(), last_ref);
    };
    let text = event.content().trim();
    let (state, basis) = match (event.kind(), event.role()) {
        (WorkstreamEventKind::Message, Some("user"))
            if text.starts_with("[Request interrupted") =>
        {
            (
                EndState::Interrupted,
                "last event is a user interrupt marker",
            )
        }
        (WorkstreamEventKind::Message, Some("assistant")) if is_usage_limit(text) => {
            (EndState::UsageLimit, "last event is a usage-limit notice")
        }
        (WorkstreamEventKind::Message, Some("assistant")) => (
            EndState::Clean,
            "last event is an assistant reply; the harness records no completion marker",
        ),
        (WorkstreamEventKind::Message, Some("user")) => {
            (EndState::Unknown, "last event is a user turn with no reply")
        }
        _ => (
            EndState::Unknown,
            "last event is tool or compaction activity with no reply",
        ),
    };
    (state, basis.to_string(), last_ref)
}

// ---------------------------------------------------------- native signals

#[cfg(test)]
/// Read titles, recaps and terminal markers from a JSONL native store.
/// Only Claude Code and Codex record them; other harnesses return nothing.
///
/// # Errors
/// Returns an error when the file cannot be opened.
pub fn read_native_signals(harness: ManagedHarness, path: &Path) -> Result<NativeSignals> {
    let mut signals = NativeSignals::default();
    if !matches!(harness, ManagedHarness::Claude | ManagedHarness::Codex) {
        return Ok(signals);
    }
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        // Cheap prefilter: these records are rare in multi-megabyte files.
        if !(line.contains("ai-title")
            || line.contains("custom-title")
            || line.contains("away_summary")
            || line.contains("turn_aborted")
            || line.contains("task_complete")
            || line.contains("\"error\""))
        {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match harness {
            ManagedHarness::Claude => claude_signal(&value, &mut signals),
            _ => codex_signal(&value, &mut signals),
        }
    }
    Ok(signals)
}

#[cfg(test)]
fn claude_signal(value: &Value, signals: &mut NativeSignals) {
    match value.get("type").and_then(Value::as_str) {
        Some("ai-title") => {
            if let Some(title) = value.get("aiTitle").and_then(Value::as_str) {
                signals.title.get_or_insert_with(|| title.to_string());
            }
        }
        Some("custom-title") => {
            if let Some(title) = value.get("customTitle").and_then(Value::as_str) {
                signals.title = Some(title.to_string());
            }
        }
        Some("system") if value.get("subtype").and_then(Value::as_str) == Some("away_summary") => {
            if let Some(text) = value.get("content").and_then(Value::as_str) {
                signals.recap = Some(text.to_string());
            }
        }
        _ => {}
    }
}

#[cfg(test)]
fn codex_signal(value: &Value, signals: &mut NativeSignals) {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return;
    }
    let payload = value.get("payload").unwrap_or(&Value::Null);
    match payload.get("type").and_then(Value::as_str) {
        Some("turn_aborted") => {
            let reason = payload
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("aborted");
            signals.terminal = Some(TerminalSignal {
                state: EndState::Interrupted,
                detail: format!("codex turn_aborted: {reason}"),
            });
        }
        Some("task_complete") => {
            signals.terminal = Some(TerminalSignal {
                state: EndState::Clean,
                detail: "codex task_complete".into(),
            });
        }
        Some("error") => {
            let message = payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let usage = is_usage_limit(message)
                || payload.get("codex_error_info").and_then(Value::as_str)
                    == Some("usage_limit_exceeded");
            signals.terminal = Some(TerminalSignal {
                state: if usage {
                    EndState::UsageLimit
                } else {
                    EndState::Unknown
                },
                detail: format!("codex error: {}", one_line(message, 120)),
            });
        }
        _ => {}
    }
}

#[cfg(test)]
/// Title and end reason from a Hermes session row.
///
/// # Errors
/// Returns an error when the profile store cannot be read.
pub fn hermes_signals(session: &HermesSession) -> Result<NativeSignals> {
    let conn = Connection::open_with_flags(
        session.db_path(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {} read-only", session.db_path().display()))?;
    let row: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT title, end_reason FROM sessions WHERE id = ?1",
            params![session.native_session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((title, end_reason)) = row else {
        return Ok(NativeSignals::default());
    };
    let terminal = end_reason
        .filter(|r| !r.is_empty())
        .map(|reason| TerminalSignal {
            state: if reason.contains("orphan")
                || reason.contains("abort")
                || reason.contains("interrupt")
            {
                EndState::Interrupted
            } else if reason.contains("limit") {
                EndState::UsageLimit
            } else {
                EndState::Clean
            },
            detail: format!("hermes end_reason: {reason}"),
        });
    Ok(NativeSignals {
        title: title.filter(|t| !t.is_empty()),
        recap: None,
        terminal,
    })
}

#[cfg(test)]
/// Title from an OpenCode session row (`session` or `session_v2`).
///
/// # Errors
/// Returns an error when the database cannot be read.
pub fn opencode_signals(db: &Path, native_session_id: &str) -> Result<NativeSignals> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {} read-only", db.display()))?;
    let mut title = None;
    for table in ["session", "session_v2"] {
        let found: Option<Option<String>> = conn
            .query_row(
                &format!("SELECT title FROM {table} WHERE id = ?1"),
                params![native_session_id],
                |row| row.get(0),
            )
            .optional()
            .unwrap_or(None);
        if let Some(found) = found {
            title = found;
            break;
        }
    }
    Ok(NativeSignals {
        title: title.filter(|t| !t.is_empty()),
        ..NativeSignals::default()
    })
}

// -------------------------------------------------------------------- card

/// Render a bounded Markdown card. Every claim cites the events behind it;
/// what the facts cannot show is listed under "Unknowns".
#[must_use]
pub fn render_card(digest: &SessionDigest) -> String {
    let mut out = String::new();
    let mut unknowns: Vec<String> = Vec::new();
    let _ = writeln!(
        out,
        "# Session card: {} `{}`\n",
        digest.harness, digest.native_session_id
    );

    match digest.of(FactKind::FinalReply).next() {
        Some(reply) => {
            let text = reply.payload["text"].as_str().unwrap_or_default();
            let total = text.chars().count();
            let _ = writeln!(
                out,
                "**Final assistant message (verbatim, {}):**\n",
                cite(&reply.refs)
            );
            for line in truncate_chars(text, HEADLINE_CHARS).lines() {
                let _ = writeln!(out, "> {line}");
            }
            if total > HEADLINE_CHARS {
                let _ = writeln!(
                    out,
                    "> [... {} more chars; read {}]",
                    total - HEADLINE_CHARS,
                    cite(&reply.refs)
                );
            }
            out.push('\n');
        }
        None => {
            let _ = writeln!(out, "**Final assistant message:** none observed\n");
            unknowns.push("no assistant reply was exported".into());
        }
    }

    let _ = writeln!(out, "## Meta\n");
    let _ = writeln!(
        out,
        "- time: {} .. {}",
        digest.first_at.as_deref().unwrap_or("?"),
        digest.last_at.as_deref().unwrap_or("?")
    );
    let _ = writeln!(
        out,
        "- events: {} (scaffolding turns dropped: {})",
        digest.event_count, digest.scaffolding_dropped
    );
    if let Some(ended) = digest.of(FactKind::Ended).next() {
        let state = ended.payload["state"].as_str().unwrap_or("unknown");
        let _ = writeln!(
            out,
            "- ended: **{state}** ({}; {})",
            ended.payload["basis"].as_str().unwrap_or_default(),
            cite(&ended.refs)
        );
        if state == "unknown" {
            unknowns.push("how the session ended".into());
        }
    }
    for (kind, label) in [
        (
            FactKind::NativeTitle,
            "native title (harness-written, unverified)",
        ),
        (
            FactKind::NativeRecap,
            "native recap (harness-written, unverified)",
        ),
    ] {
        if let Some(f) = digest.of(kind).next() {
            let _ = writeln!(
                out,
                "- {label}: {}",
                one_line(f.payload["text"].as_str().unwrap_or_default(), 400)
            );
        }
    }

    let requests: Vec<&Fact> = digest.of(FactKind::Request).collect();
    let continuations = digest.of(FactKind::Continuation).count();
    let pasted: Vec<&Fact> = digest.of(FactKind::PastedOutput).collect();
    let _ = writeln!(out, "\n## Requests ({})\n", requests.len());
    if requests.is_empty() {
        let _ = writeln!(out, "- none observed");
        unknowns.push("no user request was exported".into());
    }
    let pasted_line = |paste: &Fact| {
        format!(
            "`{}` ({})",
            one_line(
                paste.payload["first_line"].as_str().unwrap_or_default(),
                100
            )
            .replace('`', "'"),
            cite(&paste.refs)
        )
    };
    let folded = |request: &Fact| -> Vec<&Fact> {
        let at = request.refs.first().map(|r| r.0);
        pasted
            .iter()
            .copied()
            .filter(|p| p.payload["folds_into"].as_u64().map(|r| r as usize) == at)
            .collect()
    };
    for request in requests.iter().take(MAX_CARD_REQUESTS) {
        let _ = writeln!(
            out,
            "- {} ({})",
            one_line(
                request.payload["text"].as_str().unwrap_or_default(),
                REQUEST_CHARS
            ),
            cite(&request.refs)
        );
        let pastes = folded(request);
        for paste in pastes.iter().take(2) {
            let _ = writeln!(out, "  - with pasted output {}", pasted_line(paste));
        }
        if pastes.len() > 2 {
            let _ = writeln!(
                out,
                "  - with {} more pasted output turn(s)",
                pastes.len() - 2
            );
        }
    }
    if requests.len() > MAX_CARD_REQUESTS {
        let _ = writeln!(out, "- [... {} more]", requests.len() - MAX_CARD_REQUESTS);
    }
    if continuations > 0 {
        let _ = writeln!(
            out,
            "- plus {continuations} short continuation turn(s) (\"continue\", \"yes\", ...)"
        );
    }
    let loose: Vec<&&Fact> = pasted
        .iter()
        .filter(|p| p.payload["folds_into"].is_null())
        .collect();
    for paste in loose.iter().take(3) {
        let _ = writeln!(
            out,
            "- pasted output before any request: {}",
            pasted_line(paste)
        );
    }

    let _ = writeln!(out, "\n## Observed work\n");
    let mut classes: BTreeMap<&str, usize> = BTreeMap::new();
    for call in digest.of(FactKind::ToolCall) {
        *classes
            .entry(call.payload["class"].as_str().unwrap_or("other"))
            .or_default() += 1;
    }
    if classes.is_empty() {
        let _ = writeln!(out, "- no tool calls");
    } else {
        let summary: Vec<String> = classes
            .iter()
            .map(|(class, n)| format!("{class} {n}"))
            .collect();
        let _ = writeln!(out, "- tool calls: {}", summary.join(", "));
    }
    let commands: Vec<&Fact> = digest.of(FactKind::Command).collect();
    if !commands.is_empty() {
        let failed: Vec<&&Fact> = commands
            .iter()
            .filter(|c| {
                c.payload["exit"].as_i64().is_some_and(|code| code != 0)
                    || c.payload["exit_basis"] == "is_error"
            })
            .collect();
        let no_exit = commands
            .iter()
            .filter(|c| c.payload["exit"].is_null() && c.payload["exit_basis"] != "is_error")
            .count();
        let _ = writeln!(
            out,
            "- commands: {} ({} failed, {} without an exit code)",
            commands.len(),
            failed.len(),
            no_exit
        );
        if no_exit > 0 {
            unknowns.push(format!("exit status of {no_exit} command(s)"));
        }
    }
    let subagents: Vec<&Fact> = digest.of(FactKind::Subagent).collect();
    if !subagents.is_empty() {
        let _ = writeln!(out, "- subagents: {}", subagents.len());
        for agent in subagents.iter().take(MAX_CARD_SUBAGENTS) {
            let _ = writeln!(
                out,
                "  - {} ({})",
                one_line(
                    agent.payload["description"]
                        .as_str()
                        .unwrap_or("(no description)"),
                    120
                ),
                cite(&agent.refs)
            );
        }
    }
    if digest.unpaired_results > 0 {
        unknowns.push(format!(
            "{} tool result(s) could not be paired with a call",
            digest.unpaired_results
        ));
    }

    let _ = writeln!(out, "\n## Checks observed\n");
    let checks: Vec<&Fact> = digest.of(FactKind::Check).collect();
    if checks.is_empty() {
        let _ = writeln!(out, "- none observed");
    }
    let mut last_command = "";
    let mut shown = 0;
    for check in &checks {
        let command = check.payload["command"].as_str().unwrap_or_default();
        if command == last_command {
            continue;
        }
        last_command = command;
        shown += 1;
        if shown > MAX_CARD_CHECKS {
            let _ = writeln!(out, "- [... more]");
            break;
        }
        let _ = writeln!(
            out,
            "- {} {}: `{}` ({})",
            check.payload["check"].as_str().unwrap_or_default(),
            check.payload["outcome"].as_str().unwrap_or_default(),
            one_line(command, 140).replace('`', "'"),
            cite(&check.refs)
        );
    }
    if checks.iter().any(|c| c.payload["outcome"] == "unknown") {
        unknowns.push("outcome of some checks".into());
    }

    let _ = writeln!(out, "\n## Outcome\n");
    let commits: Vec<&Fact> = digest.of(FactKind::Commit).collect();
    let pushes: Vec<&Fact> = digest.of(FactKind::Push).collect();
    if commits.is_empty() && pushes.is_empty() {
        let _ = writeln!(out, "- commits: none observed");
    }
    for commit in &commits {
        let sha = commit.payload["sha"]
            .as_str()
            .unwrap_or("(sha not printed)");
        let branch = commit.payload["branch"]
            .as_str()
            .map_or_else(String::new, |b| format!(" on {b}"));
        let _ = writeln!(
            out,
            "- commit {sha}{branch}: {} ({})",
            commit.payload["message"]
                .as_str()
                .unwrap_or("(message not seen)"),
            cite(&commit.refs)
        );
    }
    for push in &pushes {
        let _ = writeln!(
            out,
            "- push {} -> {} ({})",
            push.payload["range"].as_str().unwrap_or_default(),
            push.payload["dest"].as_str().unwrap_or_default(),
            cite(&push.refs)
        );
    }
    let touches: Vec<&Fact> = digest.of(FactKind::FileTouch).collect();
    if touches.is_empty() {
        let _ = writeln!(out, "- files touched: none observed");
    } else {
        let _ = writeln!(out, "- files touched ({}):", touches.len());
        for touch in touches.iter().take(MAX_CARD_FILES) {
            let _ = writeln!(
                out,
                "  - `{}` {} via {} ({})",
                touch.payload["path"].as_str().unwrap_or_default(),
                touch.payload["verb"].as_str().unwrap_or_default(),
                touch.payload["via"].as_str().unwrap_or_default(),
                cite(&touch.refs)
            );
        }
        if touches.len() > MAX_CARD_FILES {
            let _ = writeln!(out, "  - [... {} more]", touches.len() - MAX_CARD_FILES);
        }
    }

    unknowns.push("whether the requests were satisfied, and what to do next (read the final message and the cited events)".into());
    if !digest.losses.is_empty() {
        unknowns.push(format!(
            "{} kind(s) of native data the reader excluded",
            digest.losses.len()
        ));
    }
    let _ = writeln!(out, "\n## Unknowns\n");
    for unknown in &unknowns {
        let _ = writeln!(out, "- {unknown}");
    }
    let _ = writeln!(
        out,
        "\n_Deterministic card: built from exported events without a model. `eN` is event N of this session's event record; the events, not this card, are the evidence._"
    );
    out
}

fn cite(refs: &[EventRef]) -> String {
    if refs.is_empty() {
        return "native store".into();
    }
    let shown: Vec<String> = refs.iter().take(4).map(ToString::to_string).collect();
    let more = if refs.len() > 4 {
        format!(" +{}", refs.len() - 4)
    } else {
        String::new()
    };
    format!("{}{more}", shown.join(", "))
}

fn one_line(text: &str, max: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= max {
        joined
    } else {
        format!("{}...", truncate_chars(&joined, max))
    }
}

fn truncate_chars(text: &str, max: usize) -> &str {
    text.char_indices()
        .nth(max)
        .map_or(text, |(i, _)| &text[..i])
}
