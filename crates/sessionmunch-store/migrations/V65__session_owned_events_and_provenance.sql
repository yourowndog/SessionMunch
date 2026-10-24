-- Promote the portable workstream-event concept into the general
-- session-owned source ledger and add session provenance.
--
-- Two changes here trace directly to North Star decisions:
--
-- 1. Harness becomes free text. Historically every new harness shipped a full
--    `sessions` table rebuild just to extend an `agent_kind` CHECK enum
--    (V09, V11, V20, V25, V26, V28, V30, V32, V44, V45, V47, V50, V51...).
--    This migration is intentionally the LAST such rebuild: it drops the CHECK
--    constraint entirely, so a future harness is written as an ordinary string
--    with no schema change. The column keeps its historical `agent_kind` name
--    for read compatibility; its value IS the free-text harness.
--
-- 2. The portable event concept (`workstream_events`) is promoted to a
--    session-owned ledger. This is NOT a third event system — it reuses the
--    same event families and idempotent event identity as the portable
--    concept, but an event now belongs to a real `sessions` row so global
--    history (hook + import + managed) converges on one identity.
--
-- `observations` is preserved untouched for compatibility; the promoted event
-- ledger is the authoritative history going forward.

PRAGMA foreign_keys = OFF;

DROP TRIGGER IF EXISTS auto_improve_scheduler_claims_session_pairing_ai;
DROP TRIGGER IF EXISTS session_consolidation_jobs_session_pairing_ai;

CREATE TABLE sessions_new (
    id                       BLOB PRIMARY KEY NOT NULL,
    workspace_id             BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- `project_id` is now nullable: a session captured or imported outside
    -- any project scope (e.g. global/host-wide history) may carry no project.
    project_id               BLOB REFERENCES projects(id) ON DELETE CASCADE,
    -- Free-text harness (was `agent_kind`, previously a closed CHECK enum).
    agent_kind               TEXT NOT NULL,
    native_session_id        TEXT,
    machine_id               TEXT,
    cwd                      TEXT,
    git_root                 TEXT,
    git_branch               TEXT,
    commit_at_start          TEXT,
    source                   TEXT NOT NULL DEFAULT 'hook' CHECK (source IN ('hook','import','managed')),
    import_cursor            TEXT,
    started_at               INTEGER NOT NULL,
    ended_at                 INTEGER,
    summary_page_id          BLOB REFERENCES pages(id) ON DELETE SET NULL,
    ended_observation_count  INTEGER NOT NULL DEFAULT 0 CHECK (ended_observation_count >= 0),
    actor_user               TEXT
);

INSERT INTO sessions_new (
    id, workspace_id, project_id, agent_kind, cwd, started_at, ended_at,
    summary_page_id, ended_observation_count, actor_user
)
SELECT
    id, workspace_id, project_id, agent_kind, cwd, started_at, ended_at,
    summary_page_id, ended_observation_count, actor_user
FROM sessions;

DROP TABLE sessions;

ALTER TABLE sessions_new RENAME TO sessions;

CREATE INDEX idx_sessions_recent ON sessions(workspace_id, project_id, started_at DESC);
CREATE INDEX idx_sessions_project ON sessions(project_id);
CREATE INDEX idx_sessions_started_at ON sessions(started_at DESC);
CREATE INDEX idx_sessions_scope_ended
    ON sessions(workspace_id, project_id, ended_at ASC)
    WHERE ended_at IS NOT NULL;
CREATE INDEX idx_sessions_open_owner
    ON sessions(workspace_id, project_id, agent_kind, actor_user)
    WHERE ended_at IS NULL;
CREATE INDEX idx_sessions_native
    ON sessions(machine_id, agent_kind, native_session_id);

-- Scope pairing guard, relaxed so a NULL project_id (global session) is
-- permitted while a non-NULL project must still match its workspace.
CREATE TRIGGER sessions_ws_proj_pairing_ai
BEFORE INSERT ON sessions
FOR EACH ROW
WHEN NEW.project_id IS NOT NULL
 AND NEW.workspace_id IS NOT (SELECT workspace_id FROM projects WHERE id = NEW.project_id)
BEGIN
    SELECT RAISE(ABORT, 'sessions.workspace_id does not match the project''s workspace');
END;

CREATE TRIGGER auto_improve_scheduler_claims_session_pairing_ai
BEFORE INSERT ON auto_improve_scheduler_claims
FOR EACH ROW
WHEN NEW.project_id IS NOT NULL
 AND (NEW.workspace_id IS NOT (SELECT workspace_id FROM sessions WHERE id = NEW.session_id)
   OR NEW.project_id IS NOT (SELECT project_id FROM sessions WHERE id = NEW.session_id))
BEGIN
    SELECT RAISE(ABORT, 'auto_improve_scheduler_claims scope does not match the session scope');
END;

CREATE TRIGGER session_consolidation_jobs_session_pairing_ai
BEFORE INSERT ON session_consolidation_jobs
FOR EACH ROW
WHEN NEW.project_id IS NOT NULL
 AND (NEW.workspace_id IS NOT (SELECT workspace_id FROM sessions WHERE id = NEW.session_id)
   OR NEW.project_id IS NOT (SELECT project_id FROM sessions WHERE id = NEW.session_id))
BEGIN
    SELECT RAISE(ABORT, 'session_consolidation_jobs scope does not match the session scope');
END;

-- Promoted session-owned event ledger. Reuses the portable workstream-event
-- shape (event_id identity, kind families, role/content/occurred_at, metadata)
-- so global history converges on one identity; model/provider metadata is
-- best-effort per event inside metadata_json because models change mid-session.
CREATE TABLE session_events (
    session_id    BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    sequence      INTEGER NOT NULL,
    event_id      TEXT NOT NULL,
    kind          TEXT NOT NULL CHECK (kind IN (
                      'message', 'tool_call', 'tool_result',
                      'compaction', 'checkpoint', 'annotation'
                  )),
    role          TEXT,
    content       TEXT NOT NULL,
    occurred_at   TEXT,
    metadata_json TEXT NOT NULL DEFAULT '{}',
    source        TEXT NOT NULL DEFAULT 'hook' CHECK (source IN ('hook','import','managed')),
    created_at    INTEGER NOT NULL,
    PRIMARY KEY (session_id, sequence),
    UNIQUE (session_id, event_id)
);

CREATE INDEX idx_session_events_recent
    ON session_events(session_id, sequence DESC);

CREATE VIRTUAL TABLE session_events_fts USING fts5(
    content,
    role UNINDEXED,
    kind UNINDEXED,
    content='session_events',
    content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER session_events_fts_ai AFTER INSERT ON session_events BEGIN
    INSERT INTO session_events_fts(rowid, content, role, kind)
    VALUES (new.rowid, new.content, new.role, new.kind);
END;

CREATE TRIGGER session_events_fts_ad AFTER DELETE ON session_events BEGIN
    INSERT INTO session_events_fts(session_events_fts, rowid, content, role, kind)
    VALUES ('delete', old.rowid, old.content, old.role, old.kind);
END;

CREATE TRIGGER session_events_fts_au AFTER UPDATE ON session_events BEGIN
    INSERT INTO session_events_fts(session_events_fts, rowid, content, role, kind)
    VALUES ('delete', old.rowid, old.content, old.role, old.kind);
    INSERT INTO session_events_fts(rowid, content, role, kind)
    VALUES (new.rowid, new.content, new.role, new.kind);
END;

PRAGMA foreign_keys = ON;