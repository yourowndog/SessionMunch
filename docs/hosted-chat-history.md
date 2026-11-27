# Hosted chat history

Account-level ChatGPT and Claude conversations are distinct from local
coding-agent journals. An MCP connection alone does not capture hosted chat
history. Use the provider's account export, or an explicitly selected
conversation JSON file in the same format.

- [ChatGPT account data export](https://help.openai.com/en/articles/7260999-how-do-i-export-my-chatgpt-history-and-data)
- [Claude account data export](https://support.claude.com/en/articles/9450526-how-can-i-export-my-claude-data)

Extract the conversation JSON files locally, keep the original archive in
private storage, and do not pass unrelated account/profile/credential files.
Use a stable, non-secret account label on every machine. Select a destination
whose access controls are appropriate for personal history: a workspace name
by itself is not an access-control guarantee.

```bash
sessionmunch import chats --provider chatgpt --account personal \
  --workspace private-history --project hosted --dry-run --json \
  /path/to/conversations.json
```

Use `--provider claude` for Claude conversation JSON. Multiple file paths are
supported. Remove `--dry-run` only after checking counts, reported omissions
and the destination. Files must be extracted JSON (not ZIP), up to 256 MiB
each; larger exports must be split into valid conversation arrays locally.

The importer reads visible user/assistant text on the selected conversation
branch, including timestamps where present. It reports unsupported blocks,
unselected branches and malformed conversations; attachments, hidden reasoning,
system messages and tool internals are not imported. Keep the original exports
for later recovery of unsupported data. Replays deduplicate; edited visible
messages with the same source ID and changed sanitized content become additional
ledger revisions, rather than silently overwriting evidence. Importing the same
conversation into a different scope is rejected; use the explicit session
management path if relocation is intended.

Large visible text uses the existing verified artifact endpoint. The source
file must remain available to retry incomplete uploads. Hosted queues are
separated by destination, credential, account, provider and selected scope.
On credential rotation or destination changes, inspect any outstanding old
queues before retiring them. A successful fixture test does not establish
compatibility with every real export schema.

This is historical/periodic export ingestion, not automatic live capture.
Desktop-local Claude Code/Cowork requires separate native-store validation.
Codex desktop compressed and archived journals use the native history path,
not the hosted-chat import command. Preview with `sessionmunch import sessions
--harness codex --since 5w --dry-run --json`. This selects recently active sessions,
not a strict per-message 35-day window; do not treat it as a message-retention
boundary. For live hosted capture, prefer an opt-in
provider-supported interface or local browser collector of visible messages
that emits the same identity and event format. Do not collect authentication
cookies, use hidden account APIs or silently upload unrelated conversations.

## Live finalized visible turns (observed format)

Exports are historical. For live capture, a separately approved,
user-enrolled collector of the visible page of a selected thread (or another
provider-supported interface) writes one JSON object per line:

```json
{"schema":"sessionmunch.observed-visible-turn.v1","provider":"claude","surface":"desktop",
 "conversation_id":"<native conversation UUID>","message_id":"<native message UUID>",
 "role":"assistant","final":true,"text":"<visible text>",
 "occurred_at":"2026-11-26T12:00:02Z","observed_at":"2026-11-26T12:00:05Z"}
```

```bash
sessionmunch import chats --format observed --provider claude --account personal \
  --workspace private-history --project hosted --since 5w \
  --dry-run --json observed.ndjson
```

- Identity is identical to the export path: session UUIDv5 over
  (provider, account label, native conversation UUID); event
  `hosted:<native message UUID>:<sha256(role, sanitized text)>`. Replays,
  restarts with lost local state and later exports of the same final text
  deduplicate at the hub. An edit with the same message UUID is a new revision.
- Only `final: true` turns are imported; streaming/interrupted turns and turns
  without a native message UUID are excluded and counted, never given a
  guessed identity from title, URL, time or text. Equal text under distinct
  message UUIDs stays distinct.
- Unknown fields are rejected, so raw page markup, tool payloads, hidden
  reasoning or credentials cannot pass through this boundary. Errors report
  line numbers only.
- `--since` applies the same strict per-message cutoff as exports: a fixed
  UTC invocation instant, original source time only, unknown times withheld
  and counted in `cutoff_excluded`, cursors held, out-of-window spool refused.

A fixture-only collector prototype lives in
`companions/sessionmunch-visible-turn-collector` (no provider profiles; see its
README for the per-surface requirements). Whether a given provider surface
exposes native message UUIDs to a visible-page collector, and whether a
collector is acceptable for an account, must be proven per surface with an
owner-selected harmless thread before any real capture. This format is not
evidence of continuous coverage.

## Replaying finalized visible turns (consumers such as Wire)

`GET /session-events/{session_id}/visible-turns?workspace=W&project=P[&after_seq=N&after_event_id=ID][&limit=K]`

Uses the same bearer authentication as other ingress and requires normal read.
The session must belong to `W/P` and be visible to the caller's owner;
otherwise the response is `404`, whether or not the session exists. Each call scans at
most `K` (at most 1000) ledger rows in ascending insertion order after the
cursor and returns only `message` rows with role `user`/`assistant` that did
not come from hook mirrors, as `{sequence, event_id, role, content,
occurred_at, capture}`.

Checkpoint `(scanned_through, scanned_event_id)`, not the newest returned
turn: tool and other ineligible rows advance the scan without being returned.
Replaying the same cursor returns the same page. `status: reset` means the cursor's
anchor row is gone or now holds a different event (purge, restore or reused
sequence): stop and record an explicit gap instead of skipping. A
`gap_before_oldest` or non-zero `missing_in_range` flag reports rows deleted
before they were read. `occurred_at` is evidence only and is never a cursor.
This is a per-session cursor; there is no global cursor.
