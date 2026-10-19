-- V66: backfill hook observations into the session-owned event ledger.
--
-- From V66 on, every observation insert also appends its ledger event in the
-- same transaction (`session_events::mirror_observation`). This copies the
-- history captured before that, so "when did we run X" reaches every hook
-- event already on disk, not only the ones arriving after the upgrade.
--
-- The mapping matches `observation_event_kind`: the event id is
-- `obs:<observation uuid>`, so the live mirror and this backfill can never
-- duplicate one another, and sequences continue after any events a session
-- already has (P1 ingest or managed runs). `observations` stays as it is.

INSERT OR IGNORE INTO session_events (
    session_id, sequence, event_id, kind, role, content,
    occurred_at, metadata_json, source, created_at
)
SELECT
    o.session_id,
    COALESCE(
        (SELECT MAX(e.sequence) FROM session_events e WHERE e.session_id = o.session_id),
        0
    ) + ROW_NUMBER() OVER (PARTITION BY o.session_id ORDER BY o.created_at, o.id),
    'obs:' || lower(
        substr(hex(o.id), 1, 8) || '-' || substr(hex(o.id), 9, 4) || '-' ||
        substr(hex(o.id), 13, 4) || '-' || substr(hex(o.id), 17, 4) || '-' ||
        substr(hex(o.id), 21, 12)
    ),
    CASE o.kind
        WHEN 'user-prompt' THEN 'message'
        WHEN 'stop' THEN 'message'
        WHEN 'pre-tool-use' THEN 'tool_call'
        WHEN 'post-tool-use' THEN 'tool_result'
        WHEN 'pre-compact' THEN 'compaction'
        WHEN 'post-compaction' THEN 'compaction'
        ELSE 'annotation'
    END,
    CASE o.kind
        WHEN 'user-prompt' THEN 'user'
        WHEN 'stop' THEN 'assistant'
        ELSE NULL
    END,
    CASE
        WHEN trim(o.body) = '' THEN trim(o.title)
        WHEN trim(o.title) = '' THEN trim(o.body)
        ELSE trim(o.title) || char(10) || trim(o.body)
    END,
    strftime('%Y-%m-%dT%H:%M:%fZ', o.created_at / 1000000.0, 'unixepoch'),
    json_object(
        'observation_kind', o.kind,
        'extension', o.extension,
        'source_event', o.source_event
    ),
    'hook',
    o.created_at
FROM observations o
JOIN sessions s ON s.id = o.session_id;
