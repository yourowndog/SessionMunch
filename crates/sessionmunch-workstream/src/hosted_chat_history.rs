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
    let namespace = serde_json::to_vec(&(agent.as_str(), account, &native_id))?;
    let session_id = SessionId(Uuid::new_v5(&Uuid::NAMESPACE_URL, &namespace));
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
        let revision = digest(serde_json::to_vec(&(role, &content))?.as_slice());
        transcript.events.push(NewWorkstreamEvent{
            event_id:format!("hosted:{id}:{revision}"),agent,native_session_id:native_id.clone(),
            source_record_id:Some(id),kind:WorkstreamEventKind::Message,role:Some(role.into()),content,
            occurred_at:time(record.get("create_time").or_else(||record.get("created_at"))),
            metadata:json!({"provider":agent.as_str(),"conversation_title":title,"source_account":account}),
        });
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
    let ids: Vec<_> = transcript.events.iter().map(|e| &e.event_id).collect();
    transcript.source_cursor = Some(digest(&serde_json::to_vec(&ids)?));
    Ok(HostedChat {
        session_id,
        transcript,
    })
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
}
