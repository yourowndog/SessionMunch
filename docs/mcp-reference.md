# Model Context Protocol (MCP) Interface Reference

SessionMunch runs as a high-performance Model Context Protocol (MCP) server supporting both **stdio** and **HTTP/SSE** transports.

Coding agents and client harnesses discover SessionMunch's capabilities through standard MCP tools. Rather than exposing dozens of redundant endpoints, SessionMunch provides a focused, carefully bounded tool suite.

---

## Transports

1. **HTTP/SSE Transport (Default):**
   - Listens on `http://127.0.0.1:49374/mcp`.
   - Stateless by default: plain JSON request/response with no mandatory session tokens required for standard tool invocations.
   - Stateful mode (`--http-stateful`) is available for clients requiring persistent SSE streams or server-initiated notifications.

2. **stdio Transport:**
   - Used when launching SessionMunch as a subprocess directly from agent runners (e.g. `sessionmunch serve --transport stdio`).

---

## Tool Summary

The server registers 20 tools.

| Tool | Purpose | Access |
|---|---|---|
| **Retrieval** | | |
| `memory_query` | Search the wiki (and raw-observation fallback) | read |
| `memory_read_page` | Fetch the full body of one page | read |
| `memory_recent` | List the most recently updated pages | read |
| `memory_read_session_observations` | Read the raw captured observations of one session | read |
| `memory_search_events` | Full-text search over preserved session events | read |
| `memory_explore` | Prose digest of project state | read |
| `memory_briefing` | Structured project snapshot, no LLM | read |
| `memory_status` | Aggregate counts for the project | read |
| **Writing pages** | | |
| `memory_write_page` | Create or update a durable wiki page | write |
| `memory_delete_page` | Delete one wiki page | write |
| `memory_feedback` | Record how useful a recalled page was | write |
| **Handoffs** | | |
| `memory_handoff_begin` | Record a handoff for the next agent | write |
| `memory_handoff_list` | List open handoffs without claiming them | read |
| `memory_handoff_accept` | Claim an open handoff (single use) | write |
| `memory_handoff_cancel` | Discard an open handoff | write |
| **Maintenance** | | |
| `memory_consolidate` | Compile a session's observations into pages | write |
| `memory_auto_improve` | Review a completed session and propose wiki edits | write |
| `memory_lint` | Audit the wiki and write a report | write (read-only with `dry_run`) |
| `memory_forget_sweep` | Run the retention sweep | write (preview with `dry_run`) |
| **Setup** | | |
| `memory_install_self_routing` | Return the routing snippet and skills to install | read |

### Project scope arguments

Most tools take `workspace` and `project`. Session-aware clients may omit both to use the current project. Static MCP clients must pass both together on every project-scoped call. Passing only one fails closed. Examples below show the pair where a static client would need it. Tools that can create a project (`memory_write_page`, `memory_handoff_begin`) create it if the named project does not exist yet.

---

## Retrieval

### `memory_query`
Search the knowledge base using hybrid Reciprocal Rank Fusion across FTS5, entities, graph links, and optional vectors. Falls back to raw observations when no page matches.

- **Arguments:**
  - `query` (string, required): FTS5 query expression. Aliases: `q`, `search`.
  - `limit` (integer, optional, default: 10, max 100): Maximum hits. Aliases: `n`, `top_k`.
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.
  - `scopes` (array of `{workspace, project}`, optional): Search several explicit projects. Cannot be combined with `workspace`/`project`.
  - `global` (boolean, optional): Search every project in every workspace. Omit `workspace`, `project`, and `scopes` when true.
  - `include_expired` (boolean, optional, default: false): Also return pages past their `expires_at`.
  - `explain` (boolean, optional, default: false): Attach per-stream ranks, raw scores, and RRF contributions. A `global` query reports only `streams_active`.
  - `as_of` (string, optional): ISO-8601 instant for a time-travel lookup of what the store knew then. Cannot be combined with `global`, `scopes`, or `unit: "passage"`.
  - `unit` (string, optional, default: `page`): `page` or `passage`. Passage mode returns matching spans instead of whole pages and falls back to FTS5-only when no embedder is configured.
  - `parent_expansion` (string, optional, default: `none`): `none`, `section`, or `document`. Parent context attached to passage hits. Ignored in page mode.
  - `content_budget` (integer, optional, default: 8192): Approximate character cap per hit. `0` means no cap.

```json
{"name": "memory_query", "arguments": {"query": "auth token rotation", "workspace": "acme", "project": "api", "limit": 5}}
```

### `memory_read_page`
Fetch the full body of one wiki page. Pass exactly one of `path` or `query`; a call with neither is an error.

- **Arguments:**
  - `path` (string, optional): Exact wiki path, e.g. `notes/foo.md`. Takes precedence over `query`.
  - `query` (string, optional): FTS5 query; returns the top hit's full body. Aliases: `q`, `search`.
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.

```json
{"name": "memory_read_page", "arguments": {"path": "concepts/auth-tokens.md", "workspace": "acme", "project": "api"}}
```

### `memory_recent`
Return the most recently updated wiki pages, newest first. Useful at the start of a session even when no handoff exists.

- **Arguments:**
  - `limit` (integer, optional, default: 10, max 100): Maximum pages. Alias: `n`.
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.

```json
{"name": "memory_recent", "arguments": {"limit": 5, "workspace": "acme", "project": "api"}}
```

### `memory_read_session_observations`
Read the raw lifecycle observations of one session (prompts, tool calls, stops) as captured by hooks, before consolidation. Use it to audit what happened in a session.

- **Arguments:**
  - `session_id` (string, optional): Session UUID. Omit to read the most recent completed session in the project.
  - `limit` (integer, optional, default: 50, max 200): Observations per call.
  - `offset` (integer, optional, default: 0): Observations to skip, for paging.
  - `order` (string, optional, default: `asc`): `asc` (capture order) or `desc`.
  - `kinds` (array of strings, optional): Keep only these kinds: `session-start`, `user-prompt`, `pre-tool-use`, `post-tool-use`, `pre-compact`, `post-compaction`, `notification`, `stop`, `session-end`, `other`.
  - `query` (string, optional): Full-text filter over observation titles and bodies within the session.
  - `body_max_chars` (integer, optional, default: 4000, min 200, max 16384): Cap per body; longer bodies end with a truncation marker.
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.

```json
{"name": "memory_read_session_observations", "arguments": {"session_id": "3f6c1d0e-0000-0000-0000-000000000000", "kinds": ["user-prompt", "stop"], "limit": 20, "workspace": "acme", "project": "api"}}
```

### `memory_search_events`
Full-text search over preserved session events (commands run, prompts, tool output, error text) from hook capture and imported native transcripts. Use it for evidence questions the wiki may not answer, such as "when did we last run X".

- **Arguments:**
  - `query` (string, required): Full-text query, e.g. `cargo nextest` or `ECONNREFUSED`.
  - `machine` (string, optional): Only sessions recorded on this host.
  - `harness` (string, optional): Only this harness, e.g. `claude-code`, `codex`.
  - `since` (string, optional): Only events within this window: `30m`, `12h`, `7d`, `2w`.
  - `newest` (boolean, optional): Order newest first instead of by relevance.
  - `limit` (integer, optional, default: 20, max 100): Maximum hits.
  - `workspace` (string, optional): Restrict to one project; pass together with `project`.
  - `project` (string, optional): Restrict to one project; pass together with `workspace`. Omit both to search all history.

```json
{"name": "memory_search_events", "arguments": {"query": "ECONNREFUSED", "since": "7d", "newest": true, "limit": 10}}
```

### `memory_explore`
Return a prose digest of project state. Builds on `memory_briefing` and asks the configured LLM to scale the length to how long it has been since the last observation.

- **Arguments:**
  - `focus` (string, optional): Topic to bias the digest toward, e.g. `recent rules` or a free-form question.
  - `recent_pages_limit` (integer, optional, default: 10): Recent pages the underlying briefing considers.
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.

```json
{"name": "memory_explore", "arguments": {"focus": "pending handoffs", "workspace": "acme", "project": "api"}}
```

### `memory_briefing`
Structured snapshot of project activity with no LLM call: lifetime counts, 7-day and 30-day activity, last observation time, pending handoff count, current `_rules/` pages, and recent pages.

- **Arguments:**
  - `recent_pages_limit` (integer, optional, default: 10, max 100): Recently updated pages to include.
  - `settled_first` (boolean, optional, default: false): Lead with the project's settled rule and decision pages.
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.

```json
{"name": "memory_briefing", "arguments": {"recent_pages_limit": 10, "settled_first": true, "workspace": "acme", "project": "api"}}
```

### `memory_status`
Report aggregate memory counts and runtime status: latest pages, all page versions, sessions, observations.

- **Arguments:**
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.

```json
{"name": "memory_status", "arguments": {"workspace": "acme", "project": "api"}}
```

---

## Writing Pages

### `memory_write_page`
Create or update a durable markdown page. Use it when the user explicitly asks to remember, save, pin, or annotate something permanently. Start `body` with a `# Title` line; the title is derived from that heading.

- **Arguments:**
  - `path` (string, required): Relative page path, e.g. `concepts/auth-tokens.md`.
  - `body` (string, required): Markdown content.
  - `title` (string, optional): Prefer omitting; the title comes from the first H1 in `body`, or the path stem.
  - `tier` (string, optional, default: `semantic`): `working`, `episodic`, `semantic`, or `procedural`.
  - `tags` (array of strings, optional): Tags to attach.
  - `pinned` (boolean, optional, default: false): Pin the page so the decay sweep skips it. A past `expires_at` still deletes it.
  - `scope` (string, optional): `"global"` writes to the reserved global preferences scope, which default reads include in every project. Cannot be combined with `workspace`/`project`.
  - `expires_at` (string, optional): RFC3339 instant or bare date (end of that day, UTC). After it the page is hidden and deleted by the next forget sweep.
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.

```json
{"name": "memory_write_page", "arguments": {"path": "concepts/auth-tokens.md", "body": "# Auth tokens\n\nTokens rotate every 24 hours.", "tier": "semantic", "tags": ["auth"], "workspace": "acme", "project": "api"}}
```

### `memory_delete_page`
Delete one wiki page by exact path. Runs the admission chain (`op=delete`) before the file is removed. Use only when the user asks for deletion.

- **Arguments:**
  - `path` (string, required): Exact wiki path, e.g. `notes/foo.md`.
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.

```json
{"name": "memory_delete_page", "arguments": {"path": "notes/foo.md", "workspace": "acme", "project": "api"}}
```

### `memory_feedback`
Record how useful a recalled page was. `helpful` and `not_helpful` adjust the page's salience, which affects how long it survives decay; `stale` and `wrong` flag it for the lint report.

- **Arguments:**
  - `path` (string, required): Exact wiki path, copied from a `memory_query` or `memory_read_page` hit.
  - `signal` (string, required): `helpful`, `not_helpful`, `stale`, or `wrong`.
  - `reason` (string, optional): Short note on why, stored as one line capped at 500 characters.
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.

```json
{"name": "memory_feedback", "arguments": {"path": "concepts/db.md", "signal": "stale", "reason": "moved off Postgres in March", "workspace": "acme", "project": "api"}}
```

---

## Handoffs

Handoffs are explicit, atomic baton passes between agent sessions. A handoff is single use: once claimed, no other agent can consume it. By default a handoff belongs to the operator who created it.

### `memory_handoff_begin`
Record a snapshot of where the session left off for the next agent. Use it when ending a session or when the user asks to save context for the next agent.

- **Arguments:**
  - `summary` (string, required): Short prose summary of where the session left off.
  - `open_questions` (array of strings, optional): Questions the next agent should resolve.
  - `next_steps` (array of strings, optional): Suggested next steps.
  - `files_touched` (array of strings, optional): Files touched during the session.
  - `cwd` (string, optional): Working directory at handoff time. Recorded on the handoff; it does not narrow who receives a handoff created through this tool.
  - `shared` (boolean, optional): Publish to everyone in the project instead of keeping it for you.
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.

```json
{"name": "memory_handoff_begin", "arguments": {"summary": "Auth refactor half done; tests for token refresh still failing.", "next_steps": ["Fix refresh test", "Update docs"], "files_touched": ["src/auth.rs"], "workspace": "acme", "project": "api"}}
```

### `memory_handoff_list`
List open handoffs without claiming or expiring them. Every returned row stays open.

- **Arguments:**
  - `any_owner` (boolean, optional): Also list other operators' handoffs. Root-only recovery; use only on explicit user request.
  - `limit` (integer, optional, default: 50, range 1 to 200): Maximum handoffs.
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.

```json
{"name": "memory_handoff_list", "arguments": {"limit": 10, "workspace": "acme", "project": "api"}}
```

### `memory_handoff_accept`
Claim an open handoff and return its contents. The SessionStart hook usually fetches it already, so call this only when no handoff appeared in context.

- **Arguments:**
  - `handoff_id` (string, optional): Exact id from `memory_handoff_list` or `memory_handoff_begin`. Omit to claim the latest eligible open handoff.
  - `cwd` (string, optional): Receiving working directory. Omit unless the user asks about a handoff from a different directory.
  - `any_owner` (boolean, optional): Also consider other operators' handoffs. Consumes their handoff.
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.

```json
{"name": "memory_handoff_accept", "arguments": {"handoff_id": "0198a1b2-0000-0000-0000-000000000000", "workspace": "acme", "project": "api"}}
```

### `memory_handoff_cancel`
Discard an open handoff created by mistake.

- **Arguments:**
  - `handoff_id` (string, required): Exact id from `memory_handoff_begin` or `memory_handoff_list`.
  - `any_owner` (boolean, optional): Cancel even if it belongs to another operator. Needs the same authority as other cross-operator actions.
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.

```json
{"name": "memory_handoff_cancel", "arguments": {"handoff_id": "0198a1b2-0000-0000-0000-000000000000", "workspace": "acme", "project": "api"}}
```

---

## Maintenance

### `memory_consolidate`
Run LLM consolidation of one session's observations into durable pages. By default it rewrites `sessions/<id>.md`; with `multi_page` it also writes concept, decision, and gotcha pages in one atomic batch.

- **Arguments:**
  - `session_id` (string, required): UUID of the session to consolidate.
  - `dry_run` (boolean, optional, default: false): Preview without writing.
  - `multi_page` (boolean, optional, default: false): Write multiple pages atomically.
  - `instructions` (string, optional): One-off advisory preferences for the consolidation prompt, sanitized and capped at 2,000 characters. Overrides the standing `_prompts/consolidation.md` page for this call.

```json
{"name": "memory_consolidate", "arguments": {"session_id": "3f6c1d0e-0000-0000-0000-000000000000", "multi_page": true, "dry_run": true}}
```

### `memory_auto_improve`
Run manual auto-improvement for one completed session. Validated wiki edit proposals are applied, or staged for approval when `[auto_improve] require_approval = true`. Omit `session_id` to review the latest completed session that has no auto-improvement run yet in the project.

- **Arguments:**
  - `session_id` (string, optional): Completed session UUID to review or rerun.
  - `min_observations` (integer, optional): Override the minimum observation count for this run.
  - `min_session_duration_secs` (integer, optional): Override the minimum session span.
  - `min_confidence` (number, optional): Override the proposal confidence floor.
  - `max_input_tokens` (integer, optional): Override the approximate input token budget (chars/4).
  - `max_proposals` (integer, optional): Override the maximum validated proposal count.
  - `include_raw_fallback` (boolean, optional): Include raw fallback context when the reviewer supports it.
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.

The older `dry_run`, `stage`, and `mode` fields are no longer in the schema and are rejected if sent.

```json
{"name": "memory_auto_improve", "arguments": {"max_proposals": 3, "workspace": "acme", "project": "api"}}
```

### `memory_lint`
Audit the wiki for stale episodic pages, duplicate titles, broken cross-references, and (when an LLM provider is configured) contradictions across semantic pages. Findings go to `wiki/_lint/report.md` unless `dry_run` is set.

- **Arguments:**
  - `dry_run` (boolean, optional, default: false): Do not write the report.
  - `no_llm` (boolean, optional, default: false): Skip the LLM contradiction pass and run rule-based checks only.
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.

```json
{"name": "memory_lint", "arguments": {"dry_run": true, "no_llm": true, "workspace": "acme", "project": "api"}}
```

### `memory_forget_sweep`
Run the retention sweep. Pages whose `expires_at` has passed are hard-deleted regardless of tier or pin; the other passes apply decay rules to unpinned pages. Use `dry_run` first to see what would go.

- **Arguments:**
  - `dry_run` (boolean, optional, default: false): Preview only.
  - `workspace` (string, optional): Workspace name. Pass together with `project`.
  - `project` (string, optional): Project name. Pass together with `workspace`.

```json
{"name": "memory_forget_sweep", "arguments": {"dry_run": true, "workspace": "acme", "project": "api"}}
```

---

## Setup

### `memory_install_self_routing`
Return the canonical routing install payload: the marker-delimited block for `CLAUDE.md` / `AGENTS.md`, target filenames, and the managed Agent Skills to write. The agent applies it with its own file tools; the server writes nothing.

- **Arguments:**
  - `compact` (boolean, optional): Return the compact routing block instead of the full operational guidance. Use when managed skills are installed or when refreshing a file that already uses the compact snippet.

```json
{"name": "memory_install_self_routing", "arguments": {"compact": true}}
```

---

## Schema Compatibility Normalization

Different frontier models enforce strict restrictions on MCP tool parameter schemas:
- **Google Vertex / Gemini CLI:** Disallows nested `anyOf` / nullable unions. Enable `gemini_safe_schemas = true` in config or append `?flavor=gemini` to the MCP URL.
- **Bedrock / Moonshot:** Reject root schema combinators. Enable `strip_root_combinators = true` or append `?flavor=bedrock`.
