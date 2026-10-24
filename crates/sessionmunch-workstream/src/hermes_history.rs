//! Hermes Agent history reader (North Star §3 "Hermes is a Phase 1 history
//! source").
//!
//! Hermes keeps every conversation in SQLite: `~/.hermes/state.db` for the
//! default profile and `~/.hermes/profiles/<name>/state.db` for each named
//! profile, with `sessions` and `messages` tables. The databases are opened
//! read-only and never written. Columns are probed before use so an older or
//! newer Hermes schema degrades to a named loss, not a crash.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::Value;
use sessionmunch_core::{AgentKind, NewWorkstreamEvent, WorkstreamEventKind};

use crate::history_import::StoreStatus;
use crate::transcript::ExportedTranscript;

const MAX_EVENT_CHARS: usize = 128 * 1024;

/// One Hermes session changed since the cutoff.
#[derive(Clone, Debug)]
pub struct HermesSession {
    /// Hermes session id (`20261018_234205_74de12`).
    pub native_session_id: String,
    /// Profile that owns the session (`default` for the root store).
    pub profile: String,
    /// Working directory recorded for the session, when any.
    pub cwd: Option<PathBuf>,
    /// Last activity (or start) time.
    pub updated_at: SystemTime,
    /// Model recorded on the session row.
    pub model: Option<String>,
    db: PathBuf,
}

/// Discovery over every Hermes profile store under one Hermes home.
#[derive(Clone, Debug)]
pub struct HermesDiscovery {
    /// Store-level outcome across all profiles.
    pub status: StoreStatus,
    /// Sessions changed since the cutoff, oldest first.
    pub sessions: Vec<HermesSession>,
    /// Profile stores that could not be read, by path.
    pub unreadable: Vec<String>,
}

/// Every Hermes `state.db` under `hermes_home`: the root store and one per
/// profile.
fn profile_stores(hermes_home: &Path) -> Vec<(String, PathBuf)> {
    let mut stores = Vec::new();
    let root = hermes_home.join("state.db");
    if root.is_file() {
        stores.push(("default".to_owned(), root));
    }
    if let Ok(entries) = std::fs::read_dir(hermes_home.join("profiles")) {
        let mut profiles: Vec<_> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let db = entry.path().join("state.db");
                let name = entry.file_name().to_str()?.to_owned();
                db.is_file().then_some((name, db))
            })
            .collect();
        profiles.sort();
        stores.extend(profiles);
    }
    stores
}

fn open_read_only(db: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening Hermes store {} read-only", db.display()))
}

fn columns(conn: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut statement = conn.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<HashSet<_>, _>>()?;
    Ok(names)
}

fn seconds_to_time(seconds: f64) -> SystemTime {
    if seconds.is_finite() && seconds > 0.0 {
        UNIX_EPOCH + Duration::from_secs_f64(seconds)
    } else {
        UNIX_EPOCH
    }
}

/// Enumerate Hermes sessions active at or after `since` across every profile
/// under `hermes_home` (normally `~/.hermes`).
#[must_use]
pub fn discover_hermes_history(hermes_home: &Path, since: SystemTime) -> HermesDiscovery {
    let stores = profile_stores(hermes_home);
    let mut discovery = HermesDiscovery {
        status: if stores.is_empty() {
            StoreStatus::Missing
        } else {
            StoreStatus::Found
        },
        sessions: Vec::new(),
        unreadable: Vec::new(),
    };
    let since_seconds = since
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64());
    for (profile, db) in stores {
        match list_sessions(&db, &profile, since_seconds) {
            Ok(sessions) => discovery.sessions.extend(sessions),
            Err(error) => discovery
                .unreadable
                .push(format!("{}: {error:#}", db.display())),
        }
    }
    discovery.sessions.sort_by_key(|session| session.updated_at);
    discovery
}

fn list_sessions(db: &Path, profile: &str, since_seconds: f64) -> Result<Vec<HermesSession>> {
    let conn = open_read_only(db)?;
    let have = columns(&conn, "sessions")?;
    for required in ["id", "started_at"] {
        if !have.contains(required) {
            anyhow::bail!("sessions table has no `{required}` column");
        }
    }
    let activity_columns: Vec<&str> = ["last_activity_at", "ended_at", "started_at"]
        .into_iter()
        .filter(|name| have.contains(*name))
        .collect();
    let activity = format!("COALESCE({})", activity_columns.join(", "));
    let cwd = if have.contains("cwd") { "cwd" } else { "NULL" };
    let model = if have.contains("model") {
        "model"
    } else {
        "NULL"
    };
    let mut statement = conn.prepare(&format!(
        "SELECT id, {cwd}, {activity}, {model} FROM sessions WHERE {activity} >= ?1"
    ))?;
    let rows = statement.query_map(params![since_seconds], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut sessions = Vec::new();
    for row in rows {
        let (id, cwd, activity, model) = row?;
        if id.trim().is_empty() || id.len() > 256 {
            continue;
        }
        sessions.push(HermesSession {
            native_session_id: id,
            profile: profile.to_owned(),
            cwd: cwd
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from),
            updated_at: seconds_to_time(activity),
            model,
            db: db.to_path_buf(),
        });
    }
    Ok(sessions)
}

/// Read one Hermes session's messages after `cursor` (the last message row
/// id already exported). System prompts and hidden reasoning are excluded
/// and reported as losses.
///
/// # Errors
/// Propagates a read failure of the Hermes store.
pub fn export_hermes_session(
    session: &HermesSession,
    cursor: Option<&str>,
) -> Result<ExportedTranscript> {
    let conn = open_read_only(&session.db)?;
    let have = columns(&conn, "messages")?;
    for required in ["id", "session_id", "role"] {
        if !have.contains(required) {
            anyhow::bail!("messages table has no `{required}` column");
        }
    }
    let col = |name: &str| {
        if have.contains(name) {
            name.to_owned()
        } else {
            format!("NULL AS {name}")
        }
    };
    let after: i64 = cursor.and_then(|raw| raw.parse().ok()).unwrap_or(0);
    let mut statement = conn.prepare(&format!(
        "SELECT id, role, {}, {}, {}, {}, {}, {} FROM messages \
         WHERE session_id = ?1 AND id > ?2 ORDER BY id",
        col("content"),
        col("tool_calls"),
        col("tool_name"),
        col("timestamp"),
        col("reasoning"),
        col("_compressed_summary"),
    ))?;
    let rows = statement.query_map(params![session.native_session_id, after], |row| {
        Ok(HermesRow {
            id: row.get(0)?,
            role: row.get(1)?,
            content: row.get(2)?,
            tool_calls: row.get(3)?,
            tool_name: row.get(4)?,
            timestamp: row.get(5)?,
            reasoning: row.get(6)?,
            compressed_summary: row.get::<_, Option<i64>>(7)?.unwrap_or(0) != 0,
        })
    })?;

    let mut export = ExportedTranscript {
        native_session_id: session.native_session_id.clone(),
        source_cursor: cursor.map(str::to_owned),
        events: Vec::new(),
        losses: Vec::new(),
    };
    let mut skipped_system = 0usize;
    let mut skipped_reasoning = 0usize;
    for row in rows {
        let row = row?;
        export.source_cursor = Some(row.id.to_string());
        if row
            .reasoning
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
        {
            skipped_reasoning += 1;
        }
        let occurred_at = row.timestamp.and_then(|seconds| {
            jiff::Timestamp::from_microsecond((seconds * 1_000_000.0) as i64)
                .ok()
                .map(|at| at.to_string())
        });
        let base_metadata = serde_json::json!({
            "hermes_profile": session.profile,
            "model": session.model,
        });
        let mut push = |suffix: &str, kind, role: Option<&str>, content: String, extra: Value| {
            if content.trim().is_empty() {
                return;
            }
            let mut metadata = base_metadata.clone();
            if let (Value::Object(target), Value::Object(source)) = (&mut metadata, extra) {
                target.extend(source);
            }
            export.events.push(NewWorkstreamEvent {
                event_id: format!("hermes:{}{suffix}", row.id),
                agent: AgentKind::Hermes,
                native_session_id: session.native_session_id.clone(),
                source_record_id: Some(row.id.to_string()),
                kind,
                role: role.map(str::to_owned),
                content: truncate_chars(&content, MAX_EVENT_CHARS),
                occurred_at: occurred_at.clone(),
                metadata,
            });
        };
        let content = row.content.clone().unwrap_or_default();
        match row.role.as_str() {
            "system" => skipped_system += 1,
            _ if row.compressed_summary => push(
                "",
                WorkstreamEventKind::Compaction,
                Some(row.role.as_str()),
                content,
                serde_json::json!({ "compressed_summary": true }),
            ),
            "user" => push(
                "",
                WorkstreamEventKind::Message,
                Some("user"),
                content,
                Value::Null,
            ),
            "assistant" => {
                push(
                    "",
                    WorkstreamEventKind::Message,
                    Some("assistant"),
                    content,
                    Value::Null,
                );
                for (index, call) in tool_calls(row.tool_calls.as_deref())
                    .into_iter()
                    .enumerate()
                {
                    push(
                        &format!(":call:{index}"),
                        WorkstreamEventKind::ToolCall,
                        None,
                        call.text,
                        serde_json::json!({ "tool": call.name, "call_id": call.id }),
                    );
                }
            }
            "tool" => push(
                "",
                WorkstreamEventKind::ToolResult,
                Some("tool"),
                content,
                serde_json::json!({ "tool": row.tool_name }),
            ),
            other => export
                .losses
                .push(format!("message {} has unsupported role {other:?}", row.id)),
        }
    }
    if skipped_system > 0 {
        export.losses.push(format!(
            "{skipped_system} Hermes system prompt message(s) were intentionally excluded"
        ));
    }
    if skipped_reasoning > 0 {
        export.losses.push(format!(
            "Hermes hidden reasoning on {skipped_reasoning} message(s) was intentionally excluded"
        ));
    }
    Ok(export)
}

struct HermesRow {
    id: i64,
    role: String,
    content: Option<String>,
    tool_calls: Option<String>,
    tool_name: Option<String>,
    timestamp: Option<f64>,
    reasoning: Option<String>,
    compressed_summary: bool,
}

struct ToolCallText {
    id: Option<String>,
    name: Option<String>,
    text: String,
}

/// OpenAI-style `tool_calls` JSON → one readable `name arguments` line each.
fn tool_calls(raw: Option<&str>) -> Vec<ToolCallText> {
    let Some(raw) = raw.filter(|raw| !raw.trim().is_empty()) else {
        return Vec::new();
    };
    let Ok(Value::Array(calls)) = serde_json::from_str::<Value>(raw) else {
        return vec![ToolCallText {
            id: None,
            name: None,
            text: raw.to_owned(),
        }];
    };
    calls
        .iter()
        .map(|call| {
            let function = call.get("function").unwrap_or(call);
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let arguments = match function.get("arguments") {
                Some(Value::String(text)) => text.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            ToolCallText {
                id: call
                    .get("call_id")
                    .or_else(|| call.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                text: format!("{} {arguments}", name.as_deref().unwrap_or("tool")),
                name,
            }
        })
        .collect()
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => text[..cut].to_owned(),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// role, content, tool_calls, tool_name, reasoning
    type SeedRow<'a> = (
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
    );

    fn seed(db: &Path) {
        let conn = Connection::open(db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, model TEXT, \
                 started_at REAL NOT NULL, last_activity_at REAL, cwd TEXT); \
             CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 session_id TEXT NOT NULL, role TEXT NOT NULL, content TEXT, \
                 tool_calls TEXT, tool_name TEXT, timestamp REAL NOT NULL, \
                 reasoning TEXT, _compressed_summary INTEGER NOT NULL DEFAULT 0);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions VALUES ('20261018_234205_74de12', 'cli', 'm1', 1792381328.0, \
             1792381513.0, '/work/repo')",
            [],
        )
        .unwrap();
        let rows: &[SeedRow] = &[
            ("system", Some("you are hermes"), None, None, None),
            (
                "user",
                Some("check the falconridge deploy"),
                None,
                None,
                None,
            ),
            (
                "assistant",
                Some(""),
                Some(
                    r#"[{"id":"call_1","type":"function","function":{"name":"terminal","arguments":"{\"command\":\"systemctl status falconridge\"}"}}]"#,
                ),
                None,
                Some("thinking hard"),
            ),
            (
                "tool",
                Some("falconridge.service: active"),
                None,
                Some("terminal"),
                None,
            ),
        ];
        for (role, content, calls, tool, reasoning) in rows {
            conn.execute(
                "INSERT INTO messages (session_id, role, content, tool_calls, tool_name, \
                 timestamp, reasoning) VALUES ('20261018_234205_74de12', ?1, ?2, ?3, ?4, \
                 1792381400.0, ?5)",
                params![role, content, calls, tool, reasoning],
            )
            .unwrap();
        }
    }

    #[test]
    fn reads_every_profile_store_and_exports_incrementally() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            discover_hermes_history(home.path(), UNIX_EPOCH).status,
            StoreStatus::Missing
        );
        let profile = home.path().join("profiles/reviewer");
        std::fs::create_dir_all(&profile).unwrap();
        seed(&profile.join("state.db"));

        let discovery = discover_hermes_history(home.path(), UNIX_EPOCH);
        assert_eq!(discovery.status, StoreStatus::Found);
        assert_eq!(discovery.sessions.len(), 1, "{:?}", discovery.unreadable);
        let session = &discovery.sessions[0];
        assert_eq!(session.profile, "reviewer");
        assert_eq!(session.cwd.as_deref(), Some(Path::new("/work/repo")));

        let export = export_hermes_session(session, None).unwrap();
        let kinds: Vec<_> = export.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                WorkstreamEventKind::Message,
                WorkstreamEventKind::ToolCall,
                WorkstreamEventKind::ToolResult
            ]
        );
        assert!(
            export.events[1]
                .content
                .contains("systemctl status falconridge")
        );
        assert_eq!(export.events[1].metadata["tool"], "terminal");
        assert_eq!(export.events[0].metadata["hermes_profile"], "reviewer");
        assert!(
            export
                .losses
                .iter()
                .any(|loss| loss.contains("system prompt"))
        );
        assert!(export.losses.iter().any(|loss| loss.contains("reasoning")));
        assert!(
            !export
                .events
                .iter()
                .any(|e| e.content.contains("thinking hard"))
        );
        assert_eq!(export.source_cursor.as_deref(), Some("4"));

        let again = export_hermes_session(session, export.source_cursor.as_deref()).unwrap();
        assert!(again.events.is_empty());

        let future = SystemTime::now() + Duration::from_secs(3600);
        assert!(
            discover_hermes_history(home.path(), future)
                .sessions
                .is_empty()
        );
    }

    #[test]
    fn an_unrecognised_schema_is_named_not_fatal() {
        let home = tempfile::tempdir().unwrap();
        let conn = Connection::open(home.path().join("state.db")).unwrap();
        conn.execute_batch("CREATE TABLE sessions (name TEXT);")
            .unwrap();
        drop(conn);
        let discovery = discover_hermes_history(home.path(), UNIX_EPOCH);
        assert!(discovery.sessions.is_empty());
        assert_eq!(discovery.unreadable.len(), 1);
        assert!(discovery.unreadable[0].contains("sessions table has no"));
    }
}
