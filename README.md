<p align="center">
  <img alt="SessionMunch — Stop pasting handoff prompts" src="docs/sessionmunch-hero.webp" width="1000">
</p>

<p align="center">
  <strong>Your model is allowed to forget. Your project isn't.</strong><br>
  One shared memory across sessions, agents, and machines • Local-first • MCP + HTTP • Inspectable knowledge and durable evidence
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
| **Evidence ledger** | Supported captured or imported events are kept as sanitized evidence, with session and source provenance. | "When did we run that on the build box?" gets answered from what actually happened, not from a summary that may have left it out. |
| **Shared project memory** | One SessionMunch service holds the durable state for all connected agents. | Claude Code can learn something and Codex, Hermes, OpenCode, or another MCP client can find it later. |
| **Markdown source of truth** | The important knowledge is ordinary human-readable files, not a mystery blob. | You can inspect it, grep it, edit it, version it, back it up, and leave whenever you want. |
| **SQLite ledger and indexes** | SQLite holds session history and operational state as well as the searchable index over Markdown. | Fast recall with durable event identities. Back up both the database and the knowledge files; reindexing is not disaster recovery. |
| **Full-text search (FTS5)** | Looks for the actual words and phrases you used. | Excellent for error strings, filenames, commands, symbols, and exact terminology. |
| **Embeddings** | Text is converted into coordinates where ideas with similar meaning sit near each other. | “Login token bug” can still find a note written as “authentication credential failure” even when the words do not match. |
| **Entities + graph links** | Projects, concepts, components, causes, fixes, and contradictions can be connected. | Memory can follow relationships instead of treating every note as an isolated paragraph. |
| **Passage indexing** | Long pages are searchable as smaller meaningful chunks. | The agent gets the useful paragraph, not twelve screens of surrounding archaeology. |
| **Reciprocal Rank Fusion (RRF)** | Multiple search methods vote on the answer. | Exact words, semantic similarity, and relationships can reinforce each other instead of one ranking method becoming pope. |
| **Briefs + handoffs** | Session startup can receive a small, relevant state packet automatically. | New session, less ceremony. The clipboard may finally know peace. |
| **MCP + HTTP + hooks** | Standard interfaces connect the memory service to agent harnesses. | The memory survives whichever model or coding tool you are using this week. |

**Chat history is evidence. Durable project knowledge is the product.**

Your knowledge and history belong to you. Markdown keeps the wiki inspectable; SQLite keeps structured history and indexes; content-addressed artifacts preserve oversized evidence. Back up all three.

---

## Status

SessionMunch is under active development toward **v0.1.0**. The source is available now; release acceptance is not implied by a green historical build or by the presence of packaging files.

The current development work includes stable project aliases, stronger OpenCode 2 continuity, account-export history import, compressed Codex journals, and more efficient global passage retrieval. The latest integration candidate is tracked in [PR #40](https://github.com/yourowndog/SessionMunch/pull/40). See its exact-commit checks and limitations rather than treating an older test count as a current release certificate.

Native archives, Docker and package publication are separate distribution checks. Until a channel has a verified release, use the source installation below. JEV is a planned optional reranker, not an enabled runtime provider yet.

---

## How It Works

Think of a context window as RAM. Useful, fast, temporary. SessionMunch is the part where somebody finally remembered to add a disk.

```text
agent work / explicit history import
              │
              ▼
      sanitize and identify
              │
       durable client queue
              │
              ▼
       SQLite event ledger ───► verified large artifacts
              │
              ├────► Markdown knowledge / deterministic cards
              │                     │
              ▼                     ▼
       event text search      passage / lexical / vector indexes
              │                     │
              └──────────┬──────────┘
                         ▼
              bounded evidence and recall
                         │
                   optional reranking
                         │
                  MCP / HTTP / handoffs
                         ▼
                   next agent/session
```

### How does another agent know any of this?

There is no telepathy involved, disappointingly.

**Lifecycle hooks** tell SessionMunch which project/session is active and feed it the bounded events worth remembering. **MCP tools** let an agent explicitly search, read, write, explore, and hand off memory. **Startup hooks** can inject a small relevant brief automatically. **HTTP** exposes the same service to clients that prefer an API. **Hermes** plugs in as a native MemoryProvider instead of pretending the memory layer is just another random tool.

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
cargo build --locked --release --bin sessionmunch

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

For Hermes Agent, configure the native MemoryProvider plugin in your profile (see [`docs/hermes.md`](docs/hermes.md)). For Muse Code, register the MCP server with `sessionmunch install-mcp --client muse --apply` and follow the [Muse setup and verification guide](docs/mcp-install.md#muse-code). Muse Code is currently MCP-only, not an automatic transcript-capture integration.

### 3. Experience Cross-Session Continuity

1. Work in any project repository as normal.
2. Exit your session — SessionMunch automatically processes and indexes the session.
3. Start another agent in the same project directory. The startup hook hands it the last session's handoff, so nobody has to paste anything.
4. Ask for curated knowledge while you work: *"What gotchas did we hit with auth cookies?"* (that's `memory_query`).
5. Ask for raw history when the summary isn't enough: *"When did we last run the migration on the build box?"* (that's `memory_search_events`, which searches every preserved event across machines and harnesses).

The MCP tool reference is in [`docs/mcp-reference.md`](docs/mcp-reference.md).

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

If an agent speaks **MCP**, SessionMunch already has a door it can walk through. If the harness also has lifecycle hooks, SessionMunch does the nicer automatic stuff too: session capture, scope tracking, end-of-session summaries and a handoff waiting for the next session, without you asking the model to remember to remember.

That is the whole point of a shared memory service: **change agents without changing brains.**

| Harness | How it connects | What you get |
| :--- | :--- | :--- |
| **Claude Code** | MCP + lifecycle hooks | Automatic capture, summaries, startup handoffs, search |
| **OpenAI Codex** | MCP + lifecycle hooks | Automatic capture, summaries, startup handoffs, search |
| **OpenCode / OpenCode 2** | MCP + version-specific generated plugin | Lifecycle capture, recall and handoffs; OpenCode 2 preserves recall through ephemeral contexts and has an awaited pre-compaction callback |
| **Hermes Agent** | Native `MemoryProvider` plugin ([`companions/hermes-sessionmunch`](companions/hermes-sessionmunch)) | Hermes memory backed by SessionMunch, plus history import |
| **Muse Code** | Native streamable-HTTP MCP ([setup guide](docs/mcp-install.md#muse-code)) | Explicit search, read, write and handoffs; no automatic lifecycle capture claimed |
| **Anything else that speaks MCP** | `sessionmunch install-mcp` or a hand-written MCP entry | Search, read, write and handoff tools. You bring the discipline; it brings the memory. |

Coverage differs by harness and version. The [support matrix](docs/support-matrix.md) distinguishes MCP-only clients from hook, native-import and managed-workstream adapters. An installed connector is not evidence that every conversation is captured.

---

## Bring your existing history

Native coding-agent history and hosted account conversations are different sources. Preview a bounded native import before transferring anything:

```bash
sessionmunch import sessions --harness codex --since 5w --dry-run --json
```

The native path includes supported compressed and archived Codex journals. The time window above selects recently active sessions; it does not yet guarantee a strict per-message cutoff. For account-level ChatGPT or Claude history, use an explicit provider export and the [hosted history guide](docs/hosted-chat-history.md). Hosted MCP access does not automatically collect chat transcripts. Preview counts and omissions, use a private authorized destination, and keep the original exports. Real export compatibility and continuous hosted capture still require acceptance testing.

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
sessionmunch backup --to ~/sessionmunch-backup.tar.gz
# Stop the target server before restoring into an empty data directory.
sessionmunch restore --from ~/sessionmunch-backup.tar.gz
sessionmunch reindex
sessionmunch checkpoints
```

The built-in archive currently contains the SQLite snapshot, wiki and configuration, not every artifact, native snapshot or client outbox. Preserve those separately for complete recovery. Never delete SQLite expecting reindex to reconstruct session history. See [`docs/legacy-import.md`](docs/legacy-import.md) and [`docs/upgrade-backup.md`](docs/upgrade-backup.md).

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
- **[Architecture & Data Ownership](docs/architecture-and-data-ownership.md):** Markdown storage layout, Git synchronization, single-writer pattern, and SQLite derivation.
- **[Quickstart Guide](docs/quickstart.md):** Release-candidate setup, agent connection, and first useful retrieval.
- **[Configuration Reference](docs/configuration.md):** TOML configuration reference, environment variables, and directory markers.
- **[Opinionated Model & Hardware Guide](docs/models.md):** Copy-paste local/API profiles from CPU-only systems through 24 GB+ GPUs.
- **[Model Guide Summary](docs/models-summary.md):** Short explanation of embeddings vs. summarization and provider choices.
- **[MCP Interface Reference](docs/mcp-reference.md):** MCP tool descriptions, schemas, arguments, and Gemini/Bedrock compatibility.
- **[HTTP & Frontend API](docs/http-api.md):** Read-only `/api/v1` REST endpoints, dual authentication, and web UI hosting.
- **[Hermes Agent Integration](docs/hermes.md):** The SessionMunch MemoryProvider plugin for Hermes: install, configuration and lifecycle.
- **[Muse Code Integration](docs/mcp-install.md#muse-code):** Native MCP configuration, safe connection and a read-only verification exercise.
- **[Hosted History Import](docs/hosted-chat-history.md):** Explicit account exports, identity, redaction, preview and limitations.
- **[Support Matrix](docs/support-matrix.md):** Every platform, harness and provider, including the community-grade hook bundles.
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
