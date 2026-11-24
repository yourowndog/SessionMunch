# Hermes Agent Integration: the SessionMunch MemoryProvider

SessionMunch ships a native `MemoryProvider` plugin for [Hermes Agent](https://github.com/NousResearch/hermes-agent). It lives in this repository at [`companions/hermes-sessionmunch`](../companions/hermes-sessionmunch).

Instead of treating Hermes as merely an external MCP client, SessionMunch plugs directly into Hermes's native `MemoryProvider` architecture, enabling cross-session memory consolidation, session checkpointing, and automatic scope management.

---

## Architectural Overview

```text
[Hermes Session] ──▶ [SessionMunchProvider] ──▶ [SessionMunch REST/MCP]
         │                      │                          │
         ▼                      ▼                          ▼
[Agent Turn/Tools]     [Context Injection]       [Git Markdown Wiki]
```

- **Protocol:** Communicates over SessionMunch's native REST/MCP HTTP endpoints (`http://127.0.0.1:49374`).
- **Scope Alignment:** Maps Hermes conversation threads and work contexts directly to SessionMunch workspaces and projects.
- **Attribution:** Records observations with explicit `agent = "hermes"` origin metadata, preserving provenance across multi-agent workflows.

---

## Installing the plugin

Hermes loads memory providers from its plugins directory. Copy the companion
directory there and select it:

```bash
mkdir -p "${HERMES_HOME:-$HOME/.hermes}/plugins"
cp -R companions/hermes-sessionmunch "${HERMES_HOME:-$HOME/.hermes}/plugins/sessionmunch"
hermes config set memory.provider sessionmunch
hermes memory status   # expect "Plugin: installed" and sessionmunch marked active
```

Only one external memory provider can be active in Hermes at a time.

## Configuration

The plugin reads `$HERMES_HOME/sessionmunch.json` first (or the legacy
`ai-memory.json` only when the canonical file is absent), then environment
variables:

| Setting | `sessionmunch.json` key | Environment variable | Default |
|---|---|---|---|
| MCP endpoint | `mcp_url` | `SESSIONMUNCH_MCP_URL` | `http://127.0.0.1:49374/mcp` |
| Bearer token file | `token_file` | `SESSIONMUNCH_TOKEN_FILE` | none (loopback servers need no token) |
| Default scope | `default_workspace`, `default_project` | `SESSIONMUNCH_DEFAULT_WORKSPACE`, `SESSIONMUNCH_DEFAULT_PROJECT` | empty |

Worker profiles may opt in to `scope_from_cwd: true` in their provider
sidecar. The provider freezes a complete marker pair from the owning session's
cwd, falling back to that profile's read-only native session metadata if the
host omits cwd. Linked worktree metadata must point back to its checkout.
Missing, invalid or symlinked markers do not fall through to a configured
profile default or the server's active-project pointer. Explicit complete
workspace/project pairs and global or multi-scope queries remain available.
`require_explicit_scope: true` also rejects unscoped calls when using static
profile defaults. These options are source capabilities, not a migration of
existing profile configurations.

Set the default workspace and project. With an empty scope, the server
resolves calls to the credential's active project, which on a shared server is
rarely the one you meant. Reads then come back empty instead of failing.

`hermes memory setup` writes the non-secret keys for you. Keep the token in a
file, never in `sessionmunch.json`. Setup writes only the canonical sidecar.
The corresponding `AI_MEMORY_*` environment aliases are read-only fallbacks
when the canonical variable is absent. To use a legacy token file, explicitly
configure its path; legacy token files are not automatically selected.

Hermes history that predates the plugin can be imported with
`sessionmunch import sessions --since 30d --harness hermes`, which reads every
profile's `state.db`.

---

## Provider Capabilities

- **Pre-Invocation Context Injection:** Hermes queries SessionMunch for project gotchas, active architecture rules, and handoffs before executing complex planner turns.
- **Checkpointing & Compaction:** When long Hermes trajectories compact context, SessionMunch receives a summary checkpoint, preserving long-horizon memory without context bloat.
- **Turn and session capture:** `sync_turn` intentionally performs no transport call; native hooks and transcript import own capture. `on_session_end` requests consolidation when hook capture is inactive. Required pre-compression checkpoints still archive evidence when hooks are active and must have explicit scope and a matching server write receipt.
- **Memory tools:** The provider exposes SessionMunch's memory tools to the Hermes model, scoped to the configured workspace and project.
