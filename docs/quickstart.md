# SessionMunch Quickstart

Get SessionMunch up and running and perform your first useful retrieval in under two minutes.

SessionMunch gives AI coding agents persistent, shared memory across sessions, tools, and machines. Whether you switch between Claude Code, OpenAI Codex, or Hermes Agent, SessionMunch preserves decisions, failed attempts, and project context without manual note-taking.

---

## 1. Prerequisites

- **OS:** Linux, macOS, or Windows (WSL2 recommended).
- **Runtime:** Native binary (recommended for Linux/macOS) or Docker/Podman.
- **Hardware:** Works locally with zero API keys using fast in-process FTS5 search (and optional local embeddings via ONNX/nomic).

---

## 2. Installation & Server Start

### Option A: Arch Linux (AUR)

```bash
yay -S sessionmunch-bin   # Prebuilt binary + systemd units

mkdir -p ~/.config/sessionmunch ~/.local/share/sessionmunch
sessionmunch --data-dir ~/.local/share/sessionmunch \
  --config ~/.config/sessionmunch/config.toml init

# Start the background service (optional process supervision)
systemctl --user enable --now sessionmunch.service
```

### Option B: Prebuilt Binary or Shell Wrapper (Linux / macOS)

Download the latest release binary for your platform and place it on your `PATH` (e.g. `~/.local/bin/sessionmunch`). Then start the server on loopback:

```bash
sessionmunch --data-dir ~/.local/share/sessionmunch init
sessionmunch serve --data-dir ~/.local/share/sessionmunch
```

### Option C: Docker Container

```bash
docker run -d --name sessionmunch \
  --restart unless-stopped \
  -p 127.0.0.1:49374:49374 \
  -v sessionmunch-data:/data \
  docker.io/yourowndog/sessionmunch:latest
```

By default, SessionMunch binds strictly to `127.0.0.1:49374` without authentication, ensuring loopback safety on single-user workstations.

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
