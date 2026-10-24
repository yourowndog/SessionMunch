//! Promoted session-owned event ledger and session provenance.
//!
//! The portable workstream-event concept (`NewWorkstreamEvent` /
//! [`crate::workstream`]) is promoted here into the general *session-owned*
//! source ledger: an event now belongs to a real [`crate::SessionId`] (not a
//! workstream) so hook capture, transcript import, and managed runs converge
//! on one session identity. This is the same event concept, not a third event
//! system — the event families are reused verbatim from
//! [`WorkstreamEventKind`], and event identity stays idempotent via a stable
//! source-derived `event_id`.
//!
//! `observations` remains the compatibility capture path; the promoted ledger
//! is the authoritative history going forward.

use serde::{Deserialize, Serialize};

use crate::{ProjectId, SessionId, WorkspaceId, WorkstreamEventKind};

/// Where a session's evidence originated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionSource {
    /// Captured through an agent lifecycle hook.
    #[default]
    Hook,
    /// Imported from a native harness transcript store.
    Import,
    /// Launched/observed through `sessionmunch run`.
    Managed,
}

impl SessionSource {
    /// Canonical SQLite/wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hook => "hook",
            Self::Import => "import",
            Self::Managed => "managed",
        }
    }

    /// Parse a wire value; unknown values degrade to [`Self::Hook`] so import
    /// of older or foreign data stays tolerant.
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "import" => Self::Import,
            "managed" => Self::Managed,
            _ => Self::Hook,
        }
    }
}

/// Provenance captured when a session is promoted into the owned source
/// ledger. All fields are best-effort and nullable — a global/host-wide
/// session may have no project, and a non-git or partially-observed run may
/// lack repository detail.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionProvenance {
    /// Harness-native session identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    /// Machine identifier where the session ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    /// Working directory at session start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Git root at session start, when inside a repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_root: Option<String>,
    /// Branch checked out at session start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// Commit at session start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_at_start: Option<String>,
    /// Source of this session's evidence.
    #[serde(default)]
    pub source: SessionSource,
    /// Adapter cursor for importing the transcript tail incrementally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_cursor: Option<String>,
}

impl SessionProvenance {
    /// Replace the source-derived fields (non-repo/identity detail) with
    /// fresh values; repo/identity fields are preserved when the caller does
    /// not supply a replacement.
    #[must_use]
    pub fn merge_identity(&self, incoming: &SessionProvenance) -> SessionProvenance {
        SessionProvenance {
            native_session_id: incoming
                .native_session_id
                .clone()
                .or_else(|| self.native_session_id.clone()),
            machine_id: incoming
                .machine_id
                .clone()
                .or_else(|| self.machine_id.clone()),
            cwd: incoming.cwd.clone().or_else(|| self.cwd.clone()),
            git_root: incoming.git_root.clone().or_else(|| self.git_root.clone()),
            git_branch: incoming
                .git_branch
                .clone()
                .or_else(|| self.git_branch.clone()),
            commit_at_start: incoming
                .commit_at_start
                .clone()
                .or_else(|| self.commit_at_start.clone()),
            source: incoming.source,
            import_cursor: incoming
                .import_cursor
                .clone()
                .or_else(|| self.import_cursor.clone()),
        }
    }
}

/// A single event to append to a session-owned source ledger.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewSessionEvent {
    /// Stable, source-derived identifier used to make retries idempotent.
    pub event_id: String,
    /// Semantic event family (reused verbatim from the portable concept).
    pub kind: WorkstreamEventKind,
    /// Message role (`user`, `assistant`, `tool`, ...) when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Sanitized, human-readable event content.
    pub content: String,
    /// Source timestamp as RFC 3339 when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
    /// Where this event's evidence originated. Defaults to `hook`.
    #[serde(default)]
    pub source: SessionSource,
    /// Adapter-specific, allow-listed metadata — including best-effort
    /// `model`/`provider` because models can change mid-session.
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// A materialized event row returned by ledger reads.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionEvent {
    /// Monotonic sequence within the session.
    pub sequence: i64,
    /// Stable source-derived event identifier.
    pub event_id: String,
    /// Semantic event family.
    pub kind: WorkstreamEventKind,
    /// Optional message role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Sanitized event content.
    pub content: String,
    /// Source timestamp when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
    /// Adapter-specific metadata, including model/provider when known.
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// A hit returned by lexical search over the session-owned event ledger.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionEventHit {
    /// Owning session identifier.
    pub session_id: SessionId,
    /// Monotonic sequence within the session.
    pub sequence: i64,
    /// Stable source-derived event identifier.
    pub event_id: String,
    /// Semantic event family.
    pub kind: WorkstreamEventKind,
    /// Optional message role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Sanitized event content.
    pub content: String,
    /// FTS5 snippet of the content around matched terms.
    pub snippet: String,
    /// FTS5 rank score (lower is better).
    pub rank: f64,
    /// Source timestamp when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
    /// Adapter-specific metadata, including model/provider when known.
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// Where this event's evidence originated.
    pub source: SessionSource,
    /// Workspace ID if scoped to a project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// Project ID if scoped to a project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// Harness / agent kind.
    pub harness: String,
    /// Machine ID where the session ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    /// Workspace name, for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Project name, for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Session working directory when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// When the server persisted the event (microseconds since the epoch).
    #[serde(default)]
    pub created_at: i64,
}

/// Filters for a lexical search over the session event ledger. Every filter
/// is optional; an empty filter set searches all history.
#[derive(Clone, Debug, Default)]
pub struct SessionEventSearch {
    /// FTS5 query text (sanitised into a safe MATCH expression).
    pub query: String,
    /// Maximum hits (clamped to 1..=1000).
    pub limit: usize,
    /// Restrict to one workspace and project.
    pub scope: Option<(WorkspaceId, ProjectId)>,
    /// Restrict to sessions recorded on this machine (exact, case-insensitive).
    pub machine: Option<String>,
    /// Restrict to one harness (exact `agent_kind`, case-insensitive).
    pub harness: Option<String>,
    /// Only events persisted at or after this time (microseconds).
    pub since_micros: Option<i64>,
    /// Only events persisted before this time (microseconds).
    pub until_micros: Option<i64>,
    /// Order newest first instead of by relevance.
    pub newest_first: bool,
    /// Whose sessions are visible. `None` is cross-owner (root or
    /// single-user); otherwise the owner's sessions plus unowned ones, the
    /// same rule raw observation reads apply.
    pub owner: Option<crate::OwnerFilter>,
}

/// Parse a look-back window: `30m`, `12h`, `7d`, `2w`, or `all`.
///
/// # Errors
/// A human-readable message for a missing number, a missing or unknown unit.
pub fn parse_time_window(raw: &str) -> Result<std::time::Duration, String> {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("all") {
        return Ok(std::time::Duration::from_secs(u64::MAX / 4));
    }
    let split = raw
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("{raw:?} needs a unit, e.g. 7d, 12h, 30m, 2w, or `all`"))?;
    let (count, unit) = raw.split_at(split);
    let count: u64 = count
        .parse()
        .map_err(|_| format!("{raw:?} must start with a number"))?;
    let seconds = match unit {
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(format!("time unit must be m, h, d, or w (got {unit:?})")),
    };
    Ok(std::time::Duration::from_secs(
        count.saturating_mul(seconds),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_windows_accept_units_and_all() {
        use std::time::Duration;
        assert_eq!(parse_time_window("7d"), Ok(Duration::from_secs(7 * 86_400)));
        assert_eq!(
            parse_time_window("12h"),
            Ok(Duration::from_secs(12 * 3_600))
        );
        assert_eq!(
            parse_time_window("2w"),
            Ok(Duration::from_secs(14 * 86_400))
        );
        assert!(parse_time_window("all").unwrap() > Duration::from_secs(100 * 365 * 86_400));
        assert!(parse_time_window("7").is_err());
        assert!(parse_time_window("7y").is_err());
        assert!(parse_time_window("d").is_err());
    }

    #[test]
    fn session_source_round_trips() {
        for wire in ["hook", "import", "managed"] {
            assert_eq!(SessionSource::from_wire(wire).as_str(), wire);
        }
        assert_eq!(SessionSource::from_wire("bogus"), SessionSource::Hook);
    }

    #[test]
    fn provenance_merge_keeps_existing_identity_when_incoming_is_sparse() {
        let base = SessionProvenance {
            native_session_id: Some("native-1".into()),
            machine_id: Some("box-a".into()),
            cwd: Some("/repo".into()),
            git_root: Some("/repo".into()),
            git_branch: Some("main".into()),
            commit_at_start: Some("abc".into()),
            source: SessionSource::Managed,
            import_cursor: Some("cursor-1".into()),
        };
        let incoming = SessionProvenance {
            source: SessionSource::Import,
            import_cursor: Some("cursor-2".into()),
            ..Default::default()
        };
        let merged = base.merge_identity(&incoming);
        assert_eq!(merged.native_session_id.as_deref(), Some("native-1"));
        assert_eq!(merged.machine_id.as_deref(), Some("box-a"));
        assert_eq!(merged.git_branch.as_deref(), Some("main"));
        assert_eq!(merged.source, SessionSource::Import);
        assert_eq!(merged.import_cursor.as_deref(), Some("cursor-2"));
    }
}
