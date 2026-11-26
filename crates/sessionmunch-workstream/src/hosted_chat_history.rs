//! Read explicit user-supplied hosted-chat JSON exports. No credentials,
//! private endpoints, hidden reasoning, or browser cache access.
use crate::ExportedTranscript;
use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use sessionmunch_core::{AgentKind, NewWorkstreamEvent, Sanitizer, SessionId, WorkstreamEventKind};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::BufReader,
    path::Path,
};
use uuid::Uuid;

const MAX_EXPORT_BYTES: u64 = 256 * 1024 * 1024;

/// Account-level hosted chat service (web and desktop share this identity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostedChatProvider {
    /// OpenAI ChatGPT.
    ChatGpt,
    /// Anthropic Claude chat.
    Claude,
}

impl HostedChatProvider {
    /// Wire-level harness used by the ledger.
    #[must_use]
    pub const fn agent(self) -> AgentKind {
        match self {
            Self::ChatGpt => AgentKind::ChatGpt,
            Self::Claude => AgentKind::ClaudeChat,
        }
    }
}

/// A conversation and its portable ledger identity.
#[derive(Debug)]
pub struct HostedChat {
    /// Provider/account/native-conversation namespaced id.
    pub session_id: SessionId,
    /// Visible messages on the selected branch.
    pub transcript: ExportedTranscript,
}

/// Decoded conversations and explicit partial coverage.
#[derive(Debug, Default)]
pub struct HostedChatExport {
    /// Successfully decoded conversations.
    pub chats: Vec<HostedChat>,
    /// Conversation-level failures, without raw private contents.
    pub losses: Vec<String>,
    /// Expected, counted exclusions (non-final or identity-less turns)
    /// that are coverage facts rather than input failures.
    pub notes: Vec<String>,
}

/// Read one conversation object or array from an explicitly selected file.
/// Only visible messages on the selected branch become linear ledger events;
/// preserve the original export separately for unselected branches/attachments.
///
/// # Errors
/// Returns an error for unreadable, oversized or invalid JSON files.
pub fn read_hosted_chat_export(
    path: &Path,
    provider: HostedChatProvider,
    account: &str,
    sanitizer: &Sanitizer,
) -> Result<HostedChatExport> {
    validate_account(account)?;
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > MAX_EXPORT_BYTES {
        bail!(
            "expected regular conversation JSON file at most 256 MiB; extract/split large archives first"
        );
    }
    let raw: Value = serde_json::from_reader(BufReader::new(std::io::Read::take(
        file,
        MAX_EXPORT_BYTES + 1,
    )))
    .map_err(|e| {
        anyhow::anyhow!(
            "invalid conversation JSON at line {}, column {}",
            e.line(),
            e.column()
        )
    })?;
    let conversations = match raw {
        Value::Array(v) => v,
        Value::Object(_) => vec![raw],
        _ => bail!("expected conversation object or array"),
    };
    let mut out = HostedChatExport::default();
    for (i, chat) in conversations.iter().enumerate() {
        match parse_chat(chat, provider, account, sanitizer) {
            Ok(chat) => out.chats.push(chat),
            Err(error) => out.losses.push(format!("conversation {}: {error}", i + 1)),
        }
    }
    Ok(out)
}

fn valid_id(value: Option<&Value>) -> Option<String> {
    Uuid::parse_str(value?.as_str()?)
        .ok()
        .map(|id| id.to_string())
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn time(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return text.parse::<jiff::Timestamp>().ok().map(|t| t.to_string());
    }
    let sec = value.as_f64()?;
    if !(0.0..=253_402_300_799.0).contains(&sec) {
        return None;
    }
    jiff::Timestamp::from_millisecond((sec * 1000.0) as i64)
        .ok()
        .map(|t| t.to_string())
}

fn parse_chat(
    chat: &Value,
    provider: HostedChatProvider,
    account: &str,
    sanitizer: &Sanitizer,
) -> Result<HostedChat> {
    let native_id = valid_id(match provider {
        HostedChatProvider::ChatGpt => chat.get("conversation_id").or_else(|| chat.get("id")),
        HostedChatProvider::Claude => chat.get("uuid"),
    })
    .context("missing valid conversation UUID")?;
    let agent = provider.agent();
    let session_id = hosted_session_id(provider, account, &native_id)?;
    let mut transcript = ExportedTranscript {
        native_session_id: native_id.clone(),
        ..Default::default()
    };
    let mut omitted = 0usize;
    let mut unsupported = 0usize;
    let title = chat
        .get("title")
        .or_else(|| chat.get("name"))
        .and_then(Value::as_str)
        .map(|t| sanitizer.scrub(t));
    let title = title
        .as_deref()
        .map(|s| sessionmunch_core::truncate_utf8_bytes(s, 1024));
    let records = match provider {
        HostedChatProvider::ChatGpt => chatgpt_branch(chat, &mut transcript.losses)?,
        HostedChatProvider::Claude => claude_branch(chat, &mut transcript.losses)?,
    };
    let mut seen = HashSet::new();
    for record in records {
        let Some(id) = valid_id(record.get("id").or_else(|| record.get("uuid"))) else {
            unsupported += 1;
            continue;
        };
        if !seen.insert(id.clone()) {
            unsupported += 1;
            continue;
        }
        let raw_role = match provider {
            HostedChatProvider::ChatGpt => record.pointer("/author/role"),
            HostedChatProvider::Claude => record.get("sender"),
        }
        .and_then(Value::as_str);
        let role = match raw_role {
            Some("user" | "human") => "user",
            Some("assistant") => "assistant",
            _ => {
                omitted += 1;
                continue;
            }
        };
        if record.get("channel").and_then(Value::as_str) == Some("analysis")
            || record.pointer("/metadata/channel").and_then(Value::as_str) == Some("analysis")
            || record
                .pointer("/metadata/is_visually_hidden_from_conversation")
                .and_then(Value::as_bool)
                == Some(true)
            || (role == "assistant"
                && record
                    .get("recipient")
                    .and_then(Value::as_str)
                    .is_some_and(|r| r != "all"))
        {
            omitted += 1;
            continue;
        }
        for field in ["attachments", "files"] {
            if let Some(items) = record
                .get(field)
                .or_else(|| record.get("metadata").and_then(|m| m.get(field)))
                .and_then(Value::as_array)
            {
                unsupported += items.len();
            }
        }
        let text = match provider {
            HostedChatProvider::ChatGpt => {
                let content = &record["content"];
                if !matches!(
                    content.get("content_type").and_then(Value::as_str),
                    Some("text" | "multimodal_text")
                ) {
                    unsupported += 1;
                    continue;
                }
                content
                    .get("parts")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| {
                                if let Some(s) = p.as_str() {
                                    Some(s)
                                } else {
                                    unsupported += 1;
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default()
            }
            HostedChatProvider::Claude => {
                if let Some(blocks) = record.get("content").and_then(Value::as_array) {
                    blocks
                        .iter()
                        .filter_map(|b| {
                            if b.get("type").and_then(Value::as_str) == Some("text") {
                                b.get("text").and_then(Value::as_str)
                            } else {
                                unsupported += 1;
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    record
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                }
            }
        };
        let content = sanitizer.scrub(&text);
        if content.trim().is_empty() {
            continue;
        }
        // Stable retries, including revisions of a visible message that kept
        // its native id. Never hash the unsanitized content.
        transcript.events.push(visible_event(
            agent,
            &native_id,
            id,
            role,
            content,
            time(record.get("create_time").or_else(|| record.get("created_at"))),
            json!({"provider":agent.as_str(),"conversation_title":title,"source_account":account,"capture":"export"}),
        )?);
    }
    if omitted > 0 {
        transcript.losses.push(format!(
            "{omitted} non-visible/system/tool records excluded"
        ));
    }
    if unsupported > 0 {
        transcript.losses.push(format!(
            "{unsupported} non-text or malformed records/blocks not imported"
        ));
    }
    set_cursor(&mut transcript)?;
    Ok(HostedChat {
        session_id,
        transcript,
    })
}

fn validate_account(account: &str) -> Result<()> {
    if account.is_empty()
        || account.len() > 64
        || !account
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!(
            "account must be a stable non-secret label of 1-64 ASCII letters, digits, hyphens or underscores"
        );
    }
    Ok(())
}

/// Portable conversation identity shared by account exports and live
/// observations: UUIDv5 over (provider, stable account label, native UUID).
fn hosted_session_id(
    provider: HostedChatProvider,
    account: &str,
    native_id: &str,
) -> Result<SessionId> {
    let namespace = serde_json::to_vec(&(provider.agent().as_str(), account, native_id))?;
    Ok(SessionId(Uuid::new_v5(&Uuid::NAMESPACE_URL, &namespace)))
}

/// One visible message revision. The id is the native message UUID plus a
/// digest of the sanitized role/content, so an export and a live observation
/// of the same final text converge and an edit becomes a new revision.
/// Never hash the unsanitized content.
fn visible_event(
    agent: sessionmunch_core::AgentKind,
    native_id: &str,
    message_id: String,
    role: &str,
    content: String,
    occurred_at: Option<String>,
    metadata: Value,
) -> Result<NewWorkstreamEvent> {
    let revision = digest(serde_json::to_vec(&(role, &content))?.as_slice());
    Ok(NewWorkstreamEvent {
        event_id: format!("hosted:{message_id}:{revision}"),
        agent,
        native_session_id: native_id.to_owned(),
        source_record_id: Some(message_id),
        kind: WorkstreamEventKind::Message,
        role: Some(role.into()),
        content,
        occurred_at,
        metadata,
    })
}

fn set_cursor(transcript: &mut ExportedTranscript) -> Result<()> {
    let ids: Vec<_> = transcript.events.iter().map(|e| &e.event_id).collect();
    transcript.source_cursor = Some(digest(&serde_json::to_vec(&ids)?));
    Ok(())
}

/// Schema tag every live observation record must carry.
pub const OBSERVED_TURN_SCHEMA: &str = "sessionmunch.observed-visible-turn.v1";

/// One finalized visible turn reported by a cooperative, user-enrolled
/// collector (for example a local collector reading the visible page of a
/// selected thread). Unknown fields are rejected so collectors cannot smuggle
/// raw page markup, tool payloads or credentials through this boundary.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservedTurn {
    schema: String,
    provider: String,
    #[serde(default)]
    surface: Option<String>,
    conversation_id: String,
    #[serde(default)]
    message_id: Option<String>,
    role: String,
    #[serde(rename = "final")]
    is_final: bool,
    text: String,
    #[serde(default)]
    occurred_at: Option<String>,
    #[serde(default)]
    observed_at: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

/// Read live finalized visible turns from an explicitly selected NDJSON file
/// (one [`OBSERVED_TURN_SCHEMA`] record per line). This is live capture input,
/// distinct from account export: it never reads provider stores, cookies or
/// private endpoints, and records without a native message UUID or that are
/// not final are excluded and counted rather than given a guessed identity.
/// Identity matches [`read_hosted_chat_export`] exactly, so a later export of
/// the same conversation deduplicates against what was observed live.
///
/// # Errors
/// Returns an error for an invalid account label or unreadable/oversized file.
pub fn read_observed_visible_turns(
    path: &Path,
    provider: HostedChatProvider,
    account: &str,
    sanitizer: &Sanitizer,
) -> Result<HostedChatExport> {
    validate_account(account)?;
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > MAX_EXPORT_BYTES {
        bail!("expected regular observed-turn NDJSON file at most 256 MiB");
    }
    let mut raw = String::new();
    std::io::Read::read_to_string(
        &mut std::io::Read::take(BufReader::new(file), MAX_EXPORT_BYTES + 1),
        &mut raw,
    )
    .context("observed-turn file is not UTF-8")?;
    let agent = provider.agent();
    let mut out = HostedChatExport::default();
    let mut order: Vec<String> = Vec::new();
    let mut by_conversation: HashMap<String, HostedChat> = HashMap::new();
    let mut seen_events = HashSet::new();
    let (mut non_final, mut no_message_id, mut duplicate) = (0usize, 0usize, 0usize);
    for (index, line) in raw.lines().enumerate() {
        let line_no = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        // Error text is position/shape only; never echo record contents.
        let turn: ObservedTurn = match serde_json::from_str(line) {
            Ok(turn) => turn,
            Err(e) => {
                out.losses.push(format!(
                    "line {line_no}: invalid observed turn ({:?})",
                    e.classify()
                ));
                continue;
            }
        };
        if turn.schema != OBSERVED_TURN_SCHEMA {
            out.losses
                .push(format!("line {line_no}: unsupported observed-turn schema"));
            continue;
        }
        let provider_matches = match provider {
            HostedChatProvider::ChatGpt => turn.provider == "chatgpt",
            HostedChatProvider::Claude => {
                matches!(turn.provider.as_str(), "claude" | "claude-chat")
            }
        };
        if !provider_matches {
            out.losses.push(format!(
                "line {line_no}: provider does not match --provider"
            ));
            continue;
        }
        let Some(native_id) = Uuid::parse_str(&turn.conversation_id)
            .ok()
            .map(|id| id.to_string())
        else {
            out.losses
                .push(format!("line {line_no}: missing valid conversation UUID"));
            continue;
        };
        let role = match turn.role.as_str() {
            "user" | "human" => "user",
            "assistant" => "assistant",
            _ => {
                out.losses.push(format!("line {line_no}: unsupported role"));
                continue;
            }
        };
        if !turn.is_final {
            non_final += 1;
            continue;
        }
        let Some(message_id) = turn
            .message_id
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok())
            .map(|id| id.to_string())
        else {
            no_message_id += 1;
            continue;
        };
        let content = sanitizer.scrub(&turn.text);
        if content.trim().is_empty() {
            continue;
        }
        let occurred_at = turn
            .occurred_at
            .as_deref()
            .and_then(|t| t.parse::<jiff::Timestamp>().ok())
            .map(|t| t.to_string());
        let observed_at = turn
            .observed_at
            .as_deref()
            .and_then(|t| t.parse::<jiff::Timestamp>().ok())
            .map(|t| t.to_string());
        let surface = turn
            .surface
            .as_deref()
            .filter(|s| matches!(*s, "web" | "desktop" | "mobile"));
        let title = turn
            .title
            .as_deref()
            .map(|t| sanitizer.scrub(t))
            .map(|t| sessionmunch_core::truncate_utf8_bytes(&t, 1024).to_owned());
        let event = visible_event(
            agent,
            &native_id,
            message_id,
            role,
            content,
            occurred_at,
            json!({"provider":agent.as_str(),"conversation_title":title,"source_account":account,
                   "capture":"observed","surface":surface,"observed_at":observed_at}),
        )?;
        if !seen_events.insert(event.event_id.clone()) {
            duplicate += 1;
            continue;
        }
        let chat = match by_conversation.entry(native_id.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                order.push(native_id.clone());
                e.insert(HostedChat {
                    session_id: hosted_session_id(provider, account, &native_id)?,
                    transcript: ExportedTranscript {
                        native_session_id: native_id.clone(),
                        ..Default::default()
                    },
                })
            }
        };
        chat.transcript.events.push(event);
    }
    for id in order {
        if let Some(mut chat) = by_conversation.remove(&id) {
            set_cursor(&mut chat.transcript)?;
            out.chats.push(chat);
        }
    }
    if non_final > 0 {
        out.notes.push(format!(
            "{non_final} non-final (streaming/interrupted) turns excluded"
        ));
    }
    if no_message_id > 0 {
        out.notes.push(format!(
            "{no_message_id} turns without a native message UUID excluded (identity gap, not imported)"
        ));
    }
    if duplicate > 0 {
        out.notes.push(format!(
            "{duplicate} repeated observations of an identical revision collapsed"
        ));
    }
    Ok(out)
}

// A parent chain is intentionally selected instead of concatenating abandoned
// edits into a fictitious chat. Fail on malformed/cyclic or ambiguous graphs.
fn walk_branch<'a>(
    nodes: &HashMap<&str, &'a Value>,
    leaf: &str,
    parent_key: &str,
) -> Result<Vec<&'a Value>> {
    let mut path = Vec::new();
    let mut seen = HashSet::new();
    let mut current = Some(leaf);
    while let Some(id) = current {
        if !seen.insert(id) {
            bail!("cyclic conversation branch")
        }
        let node = *nodes
            .get(id)
            .context("missing node in conversation branch")?;
        path.push(node);
        current = node
            .get(parent_key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        // Claude uses the nil UUID as a root sentinel.
        if current == Some("00000000-0000-0000-0000-000000000000") {
            current = None;
        }
    }
    path.reverse();
    Ok(path)
}

fn chatgpt_branch<'a>(chat: &'a Value, losses: &mut Vec<String>) -> Result<Vec<&'a Value>> {
    let mapping = chat
        .get("mapping")
        .and_then(Value::as_object)
        .context("missing ChatGPT mapping")?;
    if mapping.is_empty() {
        return Ok(Vec::new());
    }
    let nodes: HashMap<_, _> = mapping.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let leaf = chat
        .get("current_node")
        .and_then(Value::as_str)
        .context("ChatGPT export has no selected branch")?;
    let path = walk_branch(&nodes, leaf, "parent")?;
    if path.len() < nodes.len() {
        losses.push(format!(
            "{} nodes outside the selected branch retained only in the original export",
            nodes.len() - path.len()
        ));
    }
    Ok(path
        .into_iter()
        .filter_map(|n| n.get("message").filter(|m| m.is_object()))
        .collect())
}

fn claude_branch<'a>(chat: &'a Value, losses: &mut Vec<String>) -> Result<Vec<&'a Value>> {
    let messages = chat
        .get("chat_messages")
        .and_then(Value::as_array)
        .context("missing Claude chat_messages")?;
    if messages.is_empty() {
        return Ok(Vec::new());
    }
    if messages
        .iter()
        .any(|m| m.get("parent_message_uuid").is_some())
    {
        let nodes: HashMap<_, _> = messages
            .iter()
            .filter_map(|m| Some((m.get("uuid")?.as_str()?, m)))
            .collect();
        if nodes.len() != messages.len() {
            bail!("duplicate or missing Claude message id")
        }
        let parents: HashSet<_> = messages
            .iter()
            .filter_map(|m| m.get("parent_message_uuid")?.as_str())
            .collect();
        let leaf = if let Some(leaf) = chat
            .get("current_leaf_message_uuid")
            .and_then(Value::as_str)
        {
            leaf
        } else {
            let leaves: Vec<_> = nodes
                .keys()
                .copied()
                .filter(|id| !parents.contains(id))
                .collect();
            if leaves.len() != 1 {
                bail!("Claude branch selection ambiguous")
            }
            leaves[0]
        };
        let path = walk_branch(&nodes, leaf, "parent_message_uuid")?;
        if path.len() < messages.len() {
            losses.push(format!(
                "{} messages outside the selected branch retained only in the original export",
                messages.len() - path.len()
            ));
        }
        Ok(path)
    } else {
        Ok(messages.iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const CID: &str = "11111111-1111-4111-8111-111111111111";
    const U: &str = "22222222-2222-4222-8222-222222222222";
    const A: &str = "33333333-3333-4333-8333-333333333333";
    const B: &str = "44444444-4444-4444-8444-444444444444";
    #[test]
    fn chatgpt_selected_branch_stable_id_and_private_records() {
        let mut chat = json!({"id":CID,"current_node":A,"title":"fixture","mapping":{
            (U):{"parent":null,"message":{"id":U,"author":{"role":"user"},"content":{"content_type":"text","parts":["hello"]}}},
            (A):{"parent":U,"message":{"id":A,"author":{"role":"assistant"},"content":{"content_type":"text","parts":["world"]}}},
            (B):{"parent":U,"message":{"id":B,"author":{"role":"assistant"},"content":{"content_type":"text","parts":["abandoned"]}}}
        }});
        let sanitizer = Sanitizer::default();
        let parse =
            |c: &Value, a: &str| parse_chat(c, HostedChatProvider::ChatGpt, a, &sanitizer).unwrap();
        let first = parse(&chat, "account-a");
        assert_eq!(
            first
                .transcript
                .events
                .iter()
                .map(|e| e.content.as_str())
                .collect::<Vec<_>>(),
            vec!["hello", "world"]
        );
        assert_eq!(first.transcript.losses.len(), 1);
        assert_eq!(first.session_id, parse(&chat, "account-a").session_id);
        assert_ne!(first.session_id, parse(&chat, "account-b").session_id);
        chat["mapping"][A]["message"]["content"]["parts"][0] = json!("changed");
        let edited = parse(&chat, "account-a");
        assert_eq!(
            first.transcript.events[0].event_id,
            edited.transcript.events[0].event_id
        );
        assert_ne!(
            first.transcript.events[1].event_id,
            edited.transcript.events[1].event_id
        );
        chat["mapping"][A]["parent"] = json!(A);
        assert!(parse_chat(&chat, HostedChatProvider::ChatGpt, "a", &sanitizer).is_err());
    }
    #[test]
    fn claude_visible_text_only_and_replay() {
        let chat = json!({"uuid":CID,"name":"fixture","chat_messages":[
            {"uuid":U,"sender":"human","text":"hello","created_at":"2026-10-19T00:00:00Z"},
            {"uuid":A,"sender":"assistant","content":[{"type":"thinking","thinking":"hidden"},{"type":"text","text":"answer"},{"type":"tool_use","input":{"secret":"not captured"}}]}
        ]});
        let s = Sanitizer::default();
        let a = parse_chat(&chat, HostedChatProvider::Claude, "a", &s).unwrap();
        let b = parse_chat(&chat, HostedChatProvider::Claude, "a", &s).unwrap();
        assert_eq!(a.transcript.events.len(), 2);
        assert_eq!(a.transcript.events[1].content, "answer");
        assert_eq!(
            a.transcript.events[0].occurred_at.as_deref(),
            Some("2026-10-19T00:00:00Z")
        );
        assert_eq!(a.transcript.source_cursor, b.transcript.source_cursor);
        assert_ne!(a.session_id, SessionId(Uuid::parse_str(CID).unwrap()));
        assert!(!a.transcript.losses.is_empty());
    }

    const M2: &str = "55555555-5555-4555-8555-555555555555";

    fn turn(provider: &str, msg: Option<&str>, role: &str, text: &str, fin: bool) -> Value {
        let mut v = json!({"schema":OBSERVED_TURN_SCHEMA,"provider":provider,"surface":"desktop",
            "conversation_id":CID,"role":role,"final":fin,"text":text,
            "occurred_at":"2026-11-20T10:00:00Z","observed_at":"2026-11-20T10:00:05Z"});
        if let Some(m) = msg {
            v["message_id"] = json!(m);
        }
        v
    }

    fn ndjson(lines: &[Value]) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(f.path(), body).unwrap();
        f
    }

    /// A live observation and a later account export of the same final
    /// visible text converge on one session and one event per revision.
    #[test]
    fn observed_turns_converge_with_export_identity() {
        let s = Sanitizer::default();
        let export = json!({"uuid":CID,"name":"fixture","chat_messages":[
            {"uuid":U,"sender":"human","text":"hello"},
            {"uuid":A,"sender":"assistant","content":[{"type":"text","text":"answer"}]}
        ]});
        let exported = parse_chat(&export, HostedChatProvider::Claude, "acct", &s).unwrap();
        let file = ndjson(&[
            turn("claude", Some(U), "human", "hello", true),
            turn("claude", Some(A), "assistant", "answer", true),
        ]);
        let live = read_observed_visible_turns(file.path(), HostedChatProvider::Claude, "acct", &s)
            .unwrap();
        assert!(live.losses.is_empty(), "{:?}", live.losses);
        assert_eq!(live.chats.len(), 1);
        assert_eq!(live.chats[0].session_id, exported.session_id);
        let ids = |t: &ExportedTranscript| {
            t.events
                .iter()
                .map(|e| e.event_id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&live.chats[0].transcript), ids(&exported.transcript));
        assert_eq!(
            live.chats[0].transcript.events[1].metadata["capture"],
            "observed"
        );
        assert_eq!(exported.transcript.events[1].metadata["capture"], "export");
        // ChatGPT uses its own provider namespace for the same native UUIDs.
        let gpt = ndjson(&[turn("chatgpt", Some(A), "assistant", "answer", true)]);
        let gpt = read_observed_visible_turns(gpt.path(), HostedChatProvider::ChatGpt, "acct", &s)
            .unwrap();
        assert_ne!(gpt.chats[0].session_id, exported.session_id);
    }

    /// Restart replay collapses identical revisions, an edit with the same
    /// native id is a new revision, and equal text under a distinct native id
    /// stays a distinct message.
    #[test]
    fn observed_replay_edit_and_equal_text_identity() {
        let s = Sanitizer::default();
        let file = ndjson(&[
            turn("chatgpt", Some(A), "assistant", "ready", true),
            turn("chatgpt", Some(A), "assistant", "ready", true),
            turn("chatgpt", Some(M2), "assistant", "ready", true),
            turn("chatgpt", Some(A), "assistant", "ready (edited)", true),
        ]);
        let read = || {
            read_observed_visible_turns(file.path(), HostedChatProvider::ChatGpt, "acct", &s)
                .unwrap()
        };
        let first = read();
        let events = &first.chats[0].transcript.events;
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].source_record_id, events[2].source_record_id);
        assert_ne!(events[0].event_id, events[2].event_id);
        assert_ne!(events[0].event_id, events[1].event_id);
        assert!(first.notes.iter().any(|n| n.contains("1 repeated")));
        let again = read();
        assert_eq!(
            again.chats[0].transcript.source_cursor,
            first.chats[0].transcript.source_cursor
        );
    }

    /// Partial, identity-less, foreign-provider, unknown-field and
    /// wrong-schema records never become events, and errors never echo text.
    #[test]
    fn observed_turns_exclude_partial_unidentified_and_smuggled_records() {
        let s = Sanitizer::default();
        let mut smuggled = turn("chatgpt", Some(U), "user", "secretcanary", true);
        smuggled["html"] = json!("<div>secretcanary</div>");
        let mut schema = turn("chatgpt", Some(U), "user", "secretcanary", true);
        schema["schema"] = json!("other.v0");
        let file = ndjson(&[
            turn("chatgpt", Some(A), "assistant", "streaming partial", false),
            turn("chatgpt", None, "assistant", "no identity", true),
            turn("claude", Some(A), "assistant", "wrong provider", true),
            turn("chatgpt", Some(A), "tool", "tool output", true),
            smuggled,
            schema,
            json!("not an object"),
        ]);
        let out = read_observed_visible_turns(file.path(), HostedChatProvider::ChatGpt, "acct", &s)
            .unwrap();
        assert!(out.chats.is_empty());
        assert_eq!(out.losses.len(), 5, "{:?}", out.losses);
        assert!(out.notes.iter().any(|n| n.contains("1 non-final")));
        assert!(
            out.notes
                .iter()
                .any(|n| n.contains("1 turns without a native message UUID"))
        );
        let all = format!("{:?}{:?}", out.losses, out.notes);
        assert!(!all.contains("secretcanary") && !all.contains("partial"));
        assert!(
            read_observed_visible_turns(file.path(), HostedChatProvider::ChatGpt, "bad label", &s)
                .is_err()
        );
    }
}
