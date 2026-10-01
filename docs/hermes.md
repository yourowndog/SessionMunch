# Hermes Agent Integration: First-Class MemoryProvider

SessionMunch features a native, flagship integration with [Hermes Agent](https://github.com/NousResearch/hermes-agent) via the companion `sessionmunch` MemoryProvider plugin.

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

## Configuration in Hermes Agent

To enable SessionMunch as your primary memory provider in Hermes Agent:

1. **Locate or create your Hermes profile config** (`~/.hermes/config.yaml` or active profile):

```yaml
memory:
  provider: sessionmunch
  endpoint: http://127.0.0.1:49374
  workspace: default
  project: hermes
  token: "${SESSIONMUNCH_AUTH_TOKEN}"
```

2. **Verify Provider Health:**

Hermes executes health and schema validations against the SessionMunch endpoint at startup. You can test connectivity and state directly using Hermes CLI:

```bash
hermes memory status
```

---

## Provider Capabilities

- **Pre-Invocation Context Injection:** Hermes queries SessionMunch for project gotchas, active architecture rules, and handoffs before executing complex planner turns.
- **Checkpointing & Compaction:** When long Hermes trajectories compact context, SessionMunch receives a summary checkpoint, preserving long-horizon memory without context bloat.
- **Curator & Self-Improvement Loops:** Hermes auto-improvement runs and curator decisions integrate cleanly with SessionMunch's pending write review pipelines.
