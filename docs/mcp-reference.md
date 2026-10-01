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

## Core MCP Tools

### `memory_query`
Search the knowledge base using hybrid Reciprocal Rank Fusion across FTS5, entities, graph links, and optional vectors.

- **Arguments:**
  - `query` (string, required): Natural language search query or keywords.
  - `workspace` (string, optional): Target workspace name.
  - `project` (string, optional): Target project name.
  - `scopes` (array of strings, optional): Search multiple projects concurrently.
  - `global` (boolean, optional): Search across all workspaces/projects.
  - `limit` (integer, optional): Maximum results (default: 10).
  - `explain` (boolean, optional): Include ranking factor breakdown in response.

### `memory_explore`
Returns a synthesized prose digest of recent activity, active architectural decisions, and current gotchas for a project.

- **Arguments:**
  - `workspace` (string, optional)
  - `project` (string, optional)
  - `depth` (string, optional): Digest detail level (`shallow`, `normal`, `deep`).

### `memory_recent`
Retrieves recently updated wiki pages or raw session records.

- **Arguments:**
  - `workspace` (string, optional)
  - `project` (string, optional)
  - `limit` (integer, optional, default: 5)

### `memory_write_page`
Explicitly creates or updates a durable markdown page in the wiki.

- **Arguments:**
  - `path` (string, required): Relative page path (e.g. `concepts/auth-tokens.md`).
  - `body` (string, required): Markdown content with optional YAML frontmatter.
  - `expires_at` (string, optional): ISO-8601 timestamp for time-limited notes.

### `memory_handoff_begin` & `memory_handoff_accept`
Manages explicit, atomic baton passes across agent sessions.

- `memory_handoff_begin`: Stages an active handoff containing current state, blockers, and next steps.
- `memory_handoff_accept`: Claims an open handoff for the current session. Atomic: once claimed, it cannot be consumed by another agent.
- `memory_handoff_cancel`: Discards an open handoff created by mistake.

### `memory_consolidate`
Triggers on-demand LLM consolidation of recent session observations into durable pages.

---

## Schema Compatibility Normalization

Different frontier models enforce strict restrictions on MCP tool parameter schemas:
- **Google Vertex / Gemini CLI:** Disallows nested `anyOf` / nullable unions. Enable `gemini_safe_schemas = true` in config or append `?flavor=gemini` to the MCP URL.
- **Bedrock / Moonshot:** Reject root schema combinators. Enable `strip_root_combinators = true` or append `?flavor=bedrock`.
