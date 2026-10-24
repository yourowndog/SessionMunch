//! Global historical import of native harness transcripts (North Star §5,
//! P1 "implement global historical import by time range").
//!
//! Unlike `sessionmunch run`'s checkout-local adapters, discovery here is
//! independent of the current working directory: every session a harness
//! store holds that changed since a cutoff is enumerated, read through the
//! same read-only transcript readers, and turned into session-ledger events.
//! Stores that cannot be enumerated globally are reported, never skipped
//! silently (North Star invariant 4).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use sessionmunch_core::{NewSessionEvent, NewWorkstreamEvent, Sanitizer, SessionSource};
use sha2::{Digest as _, Sha256};

use crate::harness::ManagedHarness;
use crate::transcript::{
    ExportedTranscript, collect_session_files, export_jsonl, export_opencode, export_opencode2,
    opencode_db, session_header, valid_native_session_id,
};

/// Every harness with a native transcript adapter, in report order.
pub const IMPORTABLE_HARNESSES: &[ManagedHarness] = &[
    ManagedHarness::Claude,
    ManagedHarness::Codex,
    ManagedHarness::OpenCode,
    ManagedHarness::OpenCode2,
    ManagedHarness::Pi,
    ManagedHarness::Omp,
    ManagedHarness::Kimi,
    ManagedHarness::CommandCode,
    ManagedHarness::Kiro,
    ManagedHarness::KiroV3,
    ManagedHarness::Grok,
    ManagedHarness::Crush,
    ManagedHarness::Antigravity,
];

/// Inline content cap for one ledger event; larger payloads keep a head and
/// tail inline and ship whole as a content-addressed artifact. Matches the
/// store-boundary cap so the inline excerpt is never truncated again there.
pub const INLINE_EVENT_BYTES: usize = 16 * 1024;
const EXCERPT_HEAD_BYTES: usize = 8 * 1024;
const EXCERPT_TAIL_BYTES: usize = 6 * 1024;

/// What discovery found for one harness store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum StoreStatus {
    /// The store exists and was enumerated.
    Found,
    /// No store exists on this machine.
    Missing,
    /// The store exists but cannot be enumerated or decoded globally.
    Unsupported(String),
    /// The store exists but could not be read.
    Unreadable(String),
}

/// Where one discovered session's transcript lives.
#[derive(Clone, Debug)]
enum SessionLocation {
    File(PathBuf),
    OpenCodeDb,
}

/// One native session changed since the cutoff.
#[derive(Clone, Debug)]
pub struct DiscoveredSession {
    /// Harness that owns the store.
    pub harness: ManagedHarness,
    /// Harness-native session id.
    pub native_session_id: String,
    /// Recorded working directory, when the store carries one.
    pub cwd: Option<PathBuf>,
    /// Last native-store update.
    pub updated_at: SystemTime,
    location: SessionLocation,
}

/// Discovery result for one harness.
#[derive(Clone, Debug)]
pub struct HarnessDiscovery {
    /// Harness that was scanned.
    pub harness: ManagedHarness,
    /// Store-level outcome.
    pub status: StoreStatus,
    /// Sessions changed since the cutoff.
    pub sessions: Vec<DiscoveredSession>,
    /// Transcript files whose header could not be read (named, not dropped).
    pub unreadable: Vec<String>,
}

/// Enumerate every session `harness` stores under `home` that changed at or
/// after `since`, independent of any working directory.
#[must_use]
pub fn discover_harness_history(
    harness: ManagedHarness,
    home: &Path,
    since: SystemTime,
) -> HarnessDiscovery {
    let mut discovery = HarnessDiscovery {
        harness,
        status: StoreStatus::Found,
        sessions: Vec::new(),
        unreadable: Vec::new(),
    };
    match harness {
        ManagedHarness::Antigravity => {
            discovery.status = StoreStatus::Unsupported(
                "conversation steps are undocumented protobuf; events come from hook capture"
                    .into(),
            );
        }
        ManagedHarness::Crush => {
            discovery.status = StoreStatus::Unsupported(
                "crush keeps one database per project directory; import it from that checkout with `sessionmunch run`"
                    .into(),
            );
        }
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => {
            let db = opencode_db(home, None);
            if !db.is_file() {
                discovery.status = StoreStatus::Missing;
                return discovery;
            }
            let table = if harness == ManagedHarness::OpenCode {
                "session"
            } else {
                "session_v2"
            };
            match list_opencode_since(&db, table, since) {
                Ok(rows) => {
                    discovery.sessions = rows
                        .into_iter()
                        .map(|(id, directory, updated_at)| DiscoveredSession {
                            harness,
                            native_session_id: id,
                            cwd: directory.map(PathBuf::from),
                            updated_at,
                            location: SessionLocation::OpenCodeDb,
                        })
                        .collect();
                }
                // OpenCode 1.x stores have no `session_v2` table; that is an
                // absent store for the 2.0 adapter, not a failure.
                Err(error)
                    if harness == ManagedHarness::OpenCode2
                        && error.to_string().contains("no such table") =>
                {
                    discovery.status = StoreStatus::Missing;
                }
                Err(error) => discovery.status = StoreStatus::Unreadable(format!("{error:#}")),
            }
        }
        _ => {
            let files = match collect_session_files(harness, home, None) {
                Ok(files) => files,
                Err(error) => {
                    discovery.status = StoreStatus::Unreadable(format!("{error:#}"));
                    return discovery;
                }
            };
            if files.is_empty() {
                discovery.status = StoreStatus::Missing;
                return discovery;
            }
            for path in files {
                let Some(updated_at) = std::fs::metadata(&path).and_then(|m| m.modified()).ok()
                else {
                    discovery.unreadable.push(path.display().to_string());
                    continue;
                };
                if updated_at < since {
                    continue;
                }
                match session_header(harness, &path) {
                    Ok(Some((id, cwd))) if valid_native_session_id(&id) => {
                        discovery.sessions.push(DiscoveredSession {
                            harness,
                            native_session_id: id,
                            cwd: (!cwd.as_os_str().is_empty()).then_some(cwd),
                            updated_at,
                            location: SessionLocation::File(path),
                        });
                    }
                    // A file with no session header is a sidecar or an
                    // in-progress journal, not a lost session.
                    Ok(_) => {}
                    Err(_) => discovery.unreadable.push(path.display().to_string()),
                }
            }
        }
    }
    discovery.sessions.sort_by_key(|session| session.updated_at);
    discovery
}

fn list_opencode_since(
    db: &Path,
    table: &str,
    since: SystemTime,
) -> Result<Vec<(String, Option<String>, SystemTime)>> {
    let connection = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {} read-only", db.display()))?;
    let since_millis = since.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
        i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
    });
    let mut statement = connection.prepare(&format!(
        "SELECT id, directory, time_updated FROM {table} \
         WHERE time_updated >= ?1 ORDER BY time_updated"
    ))?;
    let rows = statement.query_map(params![since_millis], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut sessions = Vec::new();
    for row in rows {
        let (id, directory, updated_millis) = row?;
        let Ok(updated_millis) = u64::try_from(updated_millis) else {
            continue;
        };
        if !valid_native_session_id(&id) {
            continue;
        }
        let updated_at = UNIX_EPOCH + Duration::from_millis(updated_millis);
        sessions.push((id, directory, updated_at));
    }
    Ok(sessions)
}

/// Read the visible events of one discovered session after `cursor`.
///
/// # Errors
/// Propagates a read or decode failure of the native store.
pub fn export_discovered(
    session: &DiscoveredSession,
    home: &Path,
    cursor: Option<&str>,
) -> Result<ExportedTranscript> {
    match (&session.location, session.harness) {
        (SessionLocation::File(path), harness) => {
            export_jsonl(harness, path, &session.native_session_id, cursor)
        }
        (SessionLocation::OpenCodeDb, ManagedHarness::OpenCode2) => {
            export_opencode2(home, None, &session.native_session_id, cursor)
        }
        (SessionLocation::OpenCodeDb, _) => {
            export_opencode(home, None, &session.native_session_id, cursor)
        }
    }
}

/// A payload too large to keep inline: the whole sanitized bytes and the
/// event they belong to.
#[derive(Clone, Debug)]
pub struct OversizedPayload {
    /// Event the payload belongs to.
    pub event_id: String,
    /// Lower-hex SHA-256 of `content`.
    pub sha256: String,
    /// Complete sanitized payload.
    pub content: Vec<u8>,
}

/// Convert exported transcript records into ledger events. Content is
/// sanitized here, before anything is hashed or leaves the machine (North
/// Star invariant 7); the hub sanitizes inline content again on ingest.
/// Oversized payloads keep a head and tail inline with their length and
/// digest, and are returned for artifact upload.
#[must_use]
pub fn to_session_events(
    events: &[NewWorkstreamEvent],
    sanitizer: &Sanitizer,
) -> (Vec<NewSessionEvent>, Vec<OversizedPayload>) {
    let mut ledger = Vec::with_capacity(events.len());
    let mut oversized = Vec::new();
    for event in events {
        let content = sanitizer.scrub(&event.content);
        let mut metadata = match &event.metadata {
            serde_json::Value::Object(map) => map.clone(),
            serde_json::Value::Null => serde_json::Map::new(),
            other => {
                let mut map = serde_json::Map::new();
                map.insert("native".into(), other.clone());
                map
            }
        };
        if let Some(record) = &event.source_record_id {
            metadata.insert("source_record_id".into(), record.clone().into());
        }
        let inline = if content.len() > INLINE_EVENT_BYTES {
            let sha256 = hex_sha256(content.as_bytes());
            metadata.insert("artifact_sha256".into(), sha256.clone().into());
            metadata.insert("byte_len".into(), content.len().into());
            let excerpt = head_tail_excerpt(&content, EXCERPT_HEAD_BYTES, EXCERPT_TAIL_BYTES);
            oversized.push(OversizedPayload {
                event_id: event.event_id.clone(),
                sha256,
                content: content.into_bytes(),
            });
            excerpt
        } else {
            content
        };
        ledger.push(NewSessionEvent {
            event_id: event.event_id.clone(),
            kind: event.kind,
            role: event.role.clone(),
            content: inline,
            occurred_at: event.occurred_at.clone(),
            source: SessionSource::Import,
            metadata: serde_json::Value::Object(metadata),
        });
    }
    (ledger, oversized)
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `head … [N bytes omitted; full payload in artifact] … tail`, cut on char
/// boundaries.
fn head_tail_excerpt(content: &str, head: usize, tail: usize) -> String {
    let mut head_end = head.min(content.len());
    while !content.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = content.len().saturating_sub(tail).max(head_end);
    while !content.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start - head_end;
    format!(
        "{}\n… [{omitted} bytes omitted; full payload in artifact] …\n{}",
        &content[..head_end],
        &content[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sessionmunch_core::{AgentKind, WorkstreamEventKind};

    fn event(id: &str, content: &str) -> NewWorkstreamEvent {
        NewWorkstreamEvent {
            event_id: id.into(),
            agent: AgentKind::ClaudeCode,
            native_session_id: "native".into(),
            source_record_id: Some("rec-1".into()),
            kind: WorkstreamEventKind::ToolCall,
            role: None,
            content: content.into(),
            occurred_at: Some("2026-10-19T00:00:00Z".into()),
            metadata: serde_json::json!({ "tool": "bash" }),
        }
    }

    #[test]
    fn small_events_stay_inline_and_are_scrubbed() {
        let sanitizer = Sanitizer::default();
        let (ledger, oversized) = to_session_events(
            &[event("e1", "export TOKEN=sk-abcdef1234567890abcdef1234")],
            &sanitizer,
        );
        assert!(oversized.is_empty());
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].source, SessionSource::Import);
        assert!(!ledger[0].content.contains("sk-abcdef1234567890abcdef1234"));
        assert_eq!(ledger[0].metadata["tool"], "bash");
        assert_eq!(ledger[0].metadata["source_record_id"], "rec-1");
    }

    #[test]
    fn oversized_events_keep_head_tail_and_ship_whole() {
        let sanitizer = Sanitizer::default();
        let body = format!("HEAD{}TAIL", "x".repeat(40 * 1024));
        let (ledger, oversized) = to_session_events(&[event("big", &body)], &sanitizer);
        assert_eq!(oversized.len(), 1);
        assert_eq!(oversized[0].content, body.as_bytes());
        assert_eq!(oversized[0].sha256, hex_sha256(body.as_bytes()));
        let inline = &ledger[0].content;
        assert!(inline.len() <= INLINE_EVENT_BYTES);
        assert!(inline.starts_with("HEAD") && inline.ends_with("TAIL"));
        assert_eq!(ledger[0].metadata["artifact_sha256"], oversized[0].sha256);
        assert_eq!(ledger[0].metadata["byte_len"], body.len());
    }

    #[test]
    fn excerpt_respects_char_boundaries() {
        let text = "é".repeat(10_000);
        let excerpt = head_tail_excerpt(&text, 101, 101);
        assert!(excerpt.contains("bytes omitted"));
    }

    #[test]
    fn discovery_is_cwd_independent_and_reports_missing_stores() {
        let home = tempfile::tempdir().unwrap();
        let claude = discover_harness_history(ManagedHarness::Claude, home.path(), UNIX_EPOCH);
        assert_eq!(claude.status, StoreStatus::Missing);

        let project_dir = home.path().join(".claude/projects/-elsewhere-repo");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("4f6e1a52-0000-4000-8000-000000000001.jsonl"),
            concat!(
                r#"{"type":"user","sessionId":"4f6e1a52-0000-4000-8000-000000000001","cwd":"/elsewhere/repo","message":{"role":"user","content":"run the probe"},"uuid":"u1","timestamp":"2026-10-19T00:00:00Z"}"#,
                "\n"
            ),
        )
        .unwrap();
        let claude = discover_harness_history(ManagedHarness::Claude, home.path(), UNIX_EPOCH);
        assert_eq!(claude.status, StoreStatus::Found);
        assert_eq!(claude.sessions.len(), 1, "{:?}", claude.unreadable);
        let session = &claude.sessions[0];
        assert_eq!(
            session.native_session_id,
            "4f6e1a52-0000-4000-8000-000000000001"
        );
        assert_eq!(session.cwd.as_deref(), Some(Path::new("/elsewhere/repo")));
        let export = export_discovered(session, home.path(), None).unwrap();
        assert!(
            export
                .events
                .iter()
                .any(|e| e.content.contains("run the probe")),
            "{:?}",
            export.events
        );

        let future = SystemTime::now() + Duration::from_secs(3600);
        let later = discover_harness_history(ManagedHarness::Claude, home.path(), future);
        assert!(later.sessions.is_empty());

        let agy = discover_harness_history(ManagedHarness::Antigravity, home.path(), UNIX_EPOCH);
        assert!(matches!(agy.status, StoreStatus::Unsupported(_)));
    }
}
