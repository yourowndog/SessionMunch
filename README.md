<p align="center">
  <img alt="SessionMunch — Stop pasting handoff prompts" src="docs/sessionmunch-hero.webp" width="1000">
</p>

<p align="center">
  <strong>Your model is allowed to forget. Your project isn't.</strong><br>
  One shared memory across sessions, agents, and machines • Local-first • MCP + HTTP • Plain Markdown source of truth
</p>

<p align="center">
  <a href="#what-is-sessionmunch">What it is</a> •
  <a href="#quick-start">Quick start</a> •
  <a href="#how-it-works">How it works</a> •
  <a href="#model-guide">Model guide</a> •
  <a href="#supported-agent-harnesses">Integrations</a> •
  <a href="#privacy-security-and-data-ownership">Privacy</a> •
  <a href="#project-lineage">Lineage</a> •
  <a href="#documentation-suite">Docs</a>
</p>

---

## What is SessionMunch?

Coding agents are excellent at working memory and strangely committed to waking up with amnesia.

A useful session accumulates things worth keeping: architectural decisions, failed approaches, project conventions, debugging discoveries, handoffs, and the one command that fixed the cursed thing at 2:13 AM. Then the context window fills, the session ends, or you switch tools — and the next agent politely asks you to explain the project again.

**SessionMunch is a local-first persistent memory runtime for AI coding agents.** It captures the work around your code, turns it into durable project knowledge, indexes it several different ways, and makes the same memory available to the next session or a completely different agent.

> **Stop pasting handoff prompts.** The last handoff you should need to copy is the command that installs the thing that makes handoff prompts unnecessary.

If jCodeMunch makes **code** searchable and jDocMunch makes **documentation** searchable, SessionMunch applies the same instinct to the work history surrounding both: decisions, attempts, gotchas, fixes, conventions, open threads, and what the last agent learned the hard way.

### What you actually get

| Piece | In plain English | What it gets you |
| :--- | :--- | :--- |
| **Shared project memory** | One SessionMunch service holds the durable state for all connected agents. | Claude Code can learn something and Codex, Hermes, OpenCode, or another MCP client can find it later. |
| **Markdown source of truth** | The important knowledge is ordinary human-readable files, not a mystery blob. | You can inspect it, grep it, edit it, version it, back it up, and leave whenever you want. |
| **Derived SQLite index** | SQLite is the fast card catalog over those files, not the owner of them. | Fast search without sacrificing recoverability; `sessionmunch reindex` can rebuild it. |
| **Full-text search (FTS5)** | Looks for the actual words and phrases you used. | Excellent for error strings, filenames, commands, symbols, and exact terminology. |
| **Embeddings** | Text is converted into coordinates where ideas with similar meaning sit near each other. | “Login token bug” can still find a note written as “authentication credential failure” even when the words do not match. |
| **Entities + graph links** | Projects, concepts, components, causes, fixes, and contradictions can be connected. | Memory can follow relationships instead of treating every note as an isolated paragraph. |
| **Passage indexing** | Long pages are searchable as smaller meaningful chunks. | The agent gets the useful paragraph, not twelve screens of surrounding archaeology. |
| **Reciprocal Rank Fusion (RRF)** | Multiple search methods vote on the answer. | Exact words, semantic similarity, and relationships can reinforce each other instead of one ranking method becoming pope. |
| **Briefs + handoffs** | Session startup can receive a small, relevant state packet automatically. | New session, less ceremony. The clipboard may finally know peace. |
| **MCP + HTTP + hooks** | Standard interfaces connect the memory service to agent harnesses. | The memory survives whichever model or coding tool you are using this week. |

**Chat history is evidence. Durable project knowledge is the product.**

The durable knowledge is yours. The database is an acceleration layer, not a hostage situation.

---

## Status

SessionMunch is currently a **v0.1.0 release candidate**.

The release candidate passed its independent clean-machine ship audit. Promoting it to a public repo then did exactly what public CI is supposed to do and exposed a few final release-gate issues: a dependency security update, one isolation test that needs root-cause verification, and some CI/lint housekeeping. Linux release builds, Docker smoke tests, Nix, packaging checks, and fresh source installation are passing.

So the source is here and usable; the `v0.1.0` tag waits until those last gates are green. We are resisting the ancient software tradition of declaring victory while the dashboard is visibly red.

---

## How It Works

Think of a context window as RAM. Useful, fast, temporary. SessionMunch is the part where somebody finally remembered to add a disk.

```text
agent work
   │
   ▼
capture hooks ──▶ sanitize ──▶ session evidence
                               │
                               ▼
                         Markdown knowledge
                               │
                               ▼
                    rebuildable SQLite index
                               │
                 ┌─────────────┼─────────────┐
                 ▼             ▼             ▼
               FTS5       embeddings     graph/entities
                 └─────────────┼─────────────┘
                               ▼
                         RRF fusion
                               │
                               ▼
                    relevant passages/brief
                               │
                    ┌──────────┴──────────┐
                    ▼                     ▼
                 MCP/HTTP            startup handoff
                    │                     │
                    └──────────┬──────────┘
                               ▼
                         next agent/session
```

### How does another agent know any of this?

There is no telepathy involved, disappointingly.

**Lifecycle hooks** tell SessionMunch which project/session is active and feed it the bounded events worth remembering. **MCP tools** let an agent explicitly search, read, write, explore, and hand off memory. **Startup hooks** can inject a small relevant brief automatically. **HTTP** exposes the same service to clients that prefer an API. **Hermes** can use the native MemoryProvider integration instead of pretending the memory layer is just another random tool.

The important part is that these are all doors into the **same memory service**. You do not need a Claude memory, a Codex memory, a Hermes memory, and a mysterious folder named `final-final-memory-2`.

### Find broadly, then judge narrowly

The core search path deliberately uses several independent signals and fuses them. Exact text is good at exact things. Embeddings are good at meaning. Graph links are good at relationships. RRF lets them vote.

**v0.1 also supports optional LLM reranking** for project/scoped searches: after the normal search has already found a bounded shortlist, an LLM can make one final relevance pass. If it fails, times out, returns nonsense, or is unavailable, SessionMunch keeps the normal ranking.

**JEV reranking is the next step, not a hidden v0.1 switch.** The reranker abstraction and fail-open benchmark scaffolding already exist, but the current runtime accepts `llm` as the live reranker option — not `jev`. The planned JEV path will sit in the same post-retrieval slot: storage and recall remain SessionMunch's job; JEV gets a small candidate set and helps decide what deserves to reach the model.

That separation matters. Memory should not stop working because the clever optional judge called in sick.

---

## Quick Start

The current release candidate installs from source. Once v0.1.0 is tagged, the release workflow is set up to produce the less-character-building options.

### 1. Installation

Until the first tagged release artifacts are published, the honest install path is a source build:

```bash
git clone https://github.com/yourowndog/SessionMunch.git
cd SessionMunch
cargo build --release --workspace

install -Dm755 target/release/sessionmunch ~/.local/bin/sessionmunch
```

Initialize and start it:

```bash
sessionmunch init
sessionmunch serve --transport http --bind 127.0.0.1:49374
```

The release machinery for native archives, Docker, systemd, and AUR packaging is already in the repo. Those become distribution channels when `v0.1.0` is actually tagged and published; they are not being advertised here as imaginary packages from the future.

*By default, SessionMunch binds strictly to `127.0.0.1:49374`, safe for local single-user operation.*

### 2. Connect Your Agent

Register SessionMunch with your preferred harness in two commands:

```bash
# Claude Code
sessionmunch install-mcp --client claude-code --apply
sessionmunch install-hooks --agent claude-code --apply

# OpenAI Codex
sessionmunch install-mcp --client codex --apply
sessionmunch install-hooks --agent codex --apply

# OpenCode
sessionmunch install-mcp --client opencode --apply
sessionmunch install-hooks --agent opencode --apply
```

For Hermes Agent, configure the native MemoryProvider plugin in your profile (see [`docs/hermes.md`](docs/hermes.md)).

### 3. Experience Cross-Session Continuity

1. Work in any project repository as normal.
2. Exit your session — SessionMunch automatically processes and indexes the session.
3. Start another agent in the same project directory — startup hooks immediately supply the active context brief.
4. Query memory explicitly during work: *"What gotchas did we hit with auth cookies?"* (invokes `memory_query`).

---

## Model Guide

### No LLM required. Really.

SessionMunch separates **remembering** from **generating**.

A generative LLM is **not required** for the memory system to work. With no summarizer configured, SessionMunch can still capture sessions, preserve Markdown, perform FTS5 search, use entity/graph signals, build passage indexes, generate handoffs, and serve the same memory over MCP/HTTP.

By default it can also run **Nomic Embed Text v1.5 locally in-process** for semantic search. An embedding model is not a chat model: it does not write your summaries or phone a frontier model for advice. It turns text into vectors so similar ideas can be found even when the wording changes. If you want **no model inference at all**, disable embeddings too and SessionMunch falls back to lexical/entity/graph retrieval.

Adding an LLM is an upgrade path, not an entrance fee. A summarizer can distill noisy session evidence into cleaner concepts, rules, gotchas, and architecture pages. That summarizer can be local through an OpenAI-compatible server, or remote through a supported provider. Swap models, turn them off, move from cloud to local — the memory format does not care.

In other words: **bring your own intelligence; keep your memory.**

The detailed model/hardware guide is available in [`docs/models.md`](docs/models.md), with provider mechanics in [`docs/models-summary.md`](docs/models-summary.md) and [`docs/configuration.md`](docs/configuration.md).

---

## Supported Agent Harnesses

The short version: if an agent speaks **MCP**, SessionMunch already has a door it can walk through. If the harness also exposes lifecycle hooks, SessionMunch can do the nicer automatic stuff — session capture, scope tracking, startup briefs, and handoffs — without you asking the model to remember to remember.

That is the whole point of a shared memory service: **change agents without changing brains.**

SessionMunch currently supports:

| Harness | MCP Tools | Session Hooks | Status |
| :--- | :---: | :---: | :--- |
| **Claude Code** | ✓ | ✓ | First-class supported |
| **OpenAI Codex** | ✓ | ✓ | First-class supported |
| **Hermes Agent** | ✓ | ✓ | First-class MemoryProvider plugin |
| **OpenCode** | ✓ | ✓ | First-class supported |
| **Cursor / Windsurf** | ✓ | Manual/MCP | Supported |
| **Gemini CLI / Antigravity** | ✓ | ✓ | Supported (Gemini-safe schemas) |
| **Aider** | ✓ | ✓ | Supported |
| **Pi / Crush / Devin** | ✓ | Managed | Supported |

For complete harness setup instructions, see [`docs/install.md`](docs/install.md) and [`docs/mcp-install.md`](docs/mcp-install.md).

---

## Migration, Backup, and Recovery

Existing `ai-memory` installations can be detected without modifying the source:

```bash
sessionmunch legacy-import --path /path/to/old/data
```

Apply the import explicitly:

```bash
sessionmunch legacy-import --path /path/to/old/data --apply
```

The legacy source is not moved or deleted. SessionMunch also includes backup, restore, reindex, Git checkpoints, and page recovery:

```bash
sessionmunch backup
sessionmunch restore <backup.tar.gz>
sessionmunch reindex
sessionmunch checkpoints
```

See [`docs/legacy-import.md`](docs/legacy-import.md) and [`docs/upgrade-backup.md`](docs/upgrade-backup.md).

---

## Privacy, Security, and Data Ownership

The default posture is local and conservative:

- loopback-only HTTP on `127.0.0.1:49374`;
- zero product analytics or telemetry;
- local embeddings available without an API key;
- no summarizer enabled until the operator chooses one;
- built-in sanitization for common API keys, tokens, private-key material, credential-bearing URLs, provider credentials, and sensitive filesystem paths;
- assistant-output capture is separately opt-in because an agent can quote sensitive material you never intended to archive.

Remote/non-loopback deployment is supported, but it is an explicit operational choice rather than the default.

No sanitizer is magic. A machine that can read your repository should still be treated like a machine that can read your repository.

See [`docs/privacy-security.md`](docs/privacy-security.md).

---

## Documentation Suite

- **[Landing Page Copy & Product Story](docs/landing-page.md):** Canonical public-facing website copy, claims, tone, and section structure.
- **[Architecture & Data Ownership](docs/architecture.md):** Markdown storage layout, Git synchronization, single-writer pattern, and SQLite derivation.
- **[Quickstart Guide](docs/quickstart.md):** Release-candidate setup, agent connection, and first useful retrieval.
- **[Configuration Reference](docs/configuration.md):** TOML configuration reference, environment variables, and directory markers.
- **[Opinionated Model & Hardware Guide](docs/models.md):** Copy-paste local/API profiles from CPU-only systems through 24 GB+ GPUs.
- **[Model Guide Summary](docs/models-summary.md):** Short explanation of embeddings vs. summarization and provider choices.
- **[MCP Interface Reference](docs/mcp-reference.md):** MCP tool descriptions, schemas, arguments, and Gemini/Bedrock compatibility.
- **[HTTP & Frontend API](docs/http-api.md):** Read-only `/api/v1` REST endpoints, dual authentication, and web UI hosting.
- **[Hermes Agent Integration](docs/hermes.md):** First-class MemoryProvider plugin configuration and lifecycle integration.
- **[Service & Supervision](docs/service-supervision.md):** Running via systemd user/system services, launchd, or Docker.
- **[Privacy, Security & Threat Model](docs/privacy-security.md):** Zero-telemetry guarantees, secret sanitization, and network boundaries.
- **[Troubleshooting & Diagnostics](docs/troubleshooting.md):** Resolving locking issues, hook execution, and DNS rebinding errors.
- **[Upgrade, Backup & Recovery](docs/upgrade-backup.md):** Automated backups, migration steps, and disaster recovery.
- **[Legacy Import & Compatibility](docs/legacy-import.md):** Importing from predecessor `ai-memory` and external agent logs.
- **[Acknowledgements & Lineage](docs/acknowledgements.md):** Project lineage, upstream credits, and foundational inspirations.

---

## Project Lineage

SessionMunch did **not** begin from a blank editor.

It directly descends from **[akitaonrails/ai-memory](https://github.com/akitaonrails/ai-memory)** by **Fabio Akita (AkitaOnRails)**. The original project supplied the foundation and a substantial body of architecture and implementation that made this downstream work possible.

The SessionMunch v0.1.0 release candidate is based on the `ai-memory` v2.2.1 lineage and retains the upstream MIT license and copyright attribution.

From that base, SessionMunch has been developed toward a broader product goal: a standalone local-first memory runtime that can serve multiple agent harnesses, expose stable MCP/HTTP interfaces, preserve human-readable durable knowledge, provide migration and operations tooling, and integrate directly with Hermes without depending on Hermes.

This is a downstream evolution, not an attempt to quietly put a new sticker on somebody else's work.

Additional architectural influences and prior art — including jCodeMunch, jDocMunch, Basic Memory, Cognee, A-MEM, and others — are credited in [`docs/acknowledgements.md`](docs/acknowledgements.md).

**Thank you to Fabio Akita and the `ai-memory` contributors for building the ground we got to stand on.**

---

## License

SessionMunch is open-source software licensed under the MIT License. See [LICENSE](LICENSE) for details.
