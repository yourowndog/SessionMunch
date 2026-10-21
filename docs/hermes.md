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

The plugin reads `$HERMES_HOME/sessionmunch.json` first, then environment
variables:

| Setting | `sessionmunch.json` key | Environment variable | Default |
|---|---|---|---|
| MCP endpoint | `mcp_url` | `SESSIONMUNCH_MCP_URL` | `http://127.0.0.1:49374/mcp` |
| Bearer token file | `token_file` | `SESSIONMUNCH_TOKEN_FILE` | none (loopback servers need no token) |
| Default scope | `default_workspace`, `default_project` | | empty |

Set the default workspace and project. With an empty scope, the server
resolves calls to the credential's active project, which on a shared server is
rarely the one you meant. Reads then come back empty instead of failing.

`hermes memory setup` writes the non-secret keys for you. Keep the token in a
file, never in `sessionmunch.json`.

Hermes history that predates the plugin can be imported with
`sessionmunch import sessions --since 30d --harness hermes`, which reads every
profile's `state.db`.

---

## Provider Capabilities

- **Pre-Invocation Context Injection:** Hermes queries SessionMunch for project gotchas, active architecture rules, and handoffs before executing complex planner turns.
- **Checkpointing & Compaction:** When long Hermes trajectories compact context, SessionMunch receives a summary checkpoint, preserving long-horizon memory without context bloat.
- **Turn and session capture:** Each completed turn is recorded through `sync_turn`, and `on_session_end` closes the session so SessionMunch writes its summary and handoff.
- **Memory tools:** The provider exposes SessionMunch's memory tools to the Hermes model, scoped to the configured workspace and project.
