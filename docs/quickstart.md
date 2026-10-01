# SessionMunch Quickstart

<p align="center">
  <img alt="SessionMunch — Stop pasting handoff prompts" src="sessionmunch-hero.webp" width="1000">
</p>

SessionMunch gives AI coding agents **one persistent, shared project memory** across sessions, tools, and machines. Claude Code can learn something today and Codex, Hermes, OpenCode, or another MCP client can retrieve it tomorrow.

You do not need to understand vector databases to use it. You also do not need a generative LLM. SessionMunch runs locally with ordinary full-text/entity/graph retrieval and best-effort local embeddings for search-by-meaning; a generative summarizer is optional.

The goal is pleasantly boring: **stop pasting handoff prompts and let the next agent ask the project memory instead.**

---

## 1. Prerequisites

- **OS:** Linux, macOS, or Windows (WSL2 recommended).
- **Runtime:** Rust toolchain for the current source-build release candidate. Native archives and containers arrive with the first tagged release.
- **Hardware:** Works locally with zero API keys using FTS5/entity/graph retrieval; Nomic Embed Text v1.5 can add local semantic search in-process.

---

## 2. Installation & Server Start

SessionMunch is currently a **v0.1.0 release candidate**. Until the first public tag is cut, install from source rather than pretending an AUR package, Docker image, or prebuilt archive has already materialized from the future.

```bash
git clone https://github.com/yourowndog/SessionMunch.git
cd SessionMunch
cargo build --release --workspace
install -Dm755 target/release/sessionmunch ~/.local/bin/sessionmunch

sessionmunch init
sessionmunch serve --transport http --bind 127.0.0.1:49374
```

The repository already contains release machinery for native archives, Docker, systemd, and AUR packaging. Those become the easier install paths once `v0.1.0` is actually published.

By default, SessionMunch binds strictly to `127.0.0.1:49374`, keeping the normal single-user setup on loopback.

---

## 3. Connect Your Agent

Integrate your agent CLI in two commands using built-in discovery and registration:

```bash
# Example: Claude Code
sessionmunch install-mcp --client claude-code --apply
sessionmunch install-hooks --agent claude-code --apply

# Example: OpenAI Codex
sessionmunch install-mcp --client codex --apply
sessionmunch install-hooks --agent codex --apply

# Example: OpenCode
sessionmunch install-mcp --client opencode --apply
sessionmunch install-hooks --agent opencode --apply
```

For other harnesses (Cursor, Gemini CLI, Devin, Grok, Kiro, etc.), see [`docs/install.md`](install.md).

---

## 4. First Useful Retrieval

Now experience cross-session continuity:

1. **Run a session in your project:**
   ```bash
   cd ~/projects/my-app
   claude
   ```
   Ask Claude to investigate a bug or outline an architecture decision. Exit when done.

2. **Inspect what was captured:**
   SessionMunch's lifecycle hooks capture prompt/tool events silently, sanitize sensitive data, and compile structured wiki notes upon session exit.
   ```bash
   sessionmunch status
   sessionmunch search "investigation"
   ```

3. **Resume with another agent:**
   Switch tools seamlessly in the same directory:
   ```bash
   codex
   ```
   The startup hook automatically presents the handoff context: where you left off, what was resolved, and what remains pending.

4. **Query memory proactively:**
   Inside any connected agent, ask:
   - *"Have we discussed auth session tokens?"* (invokes `memory_query`)
   - *"Where did we leave off?"* (invokes `memory_handoff_accept`)
   - *"Catch me up on recent changes."* (invokes `memory_explore`)

---

## Next Steps

- Read the full [Architecture & Data Ownership Guide](architecture.md).
- Configure embeddings and LLM providers in the [Model Guide](models.md) and [Configuration Guide](configuration.md).
- Set up the [Hermes MemoryProvider Integration](hermes.md).
