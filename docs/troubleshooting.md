# Troubleshooting & Diagnostics

When agents fail to recall prior decisions or commands fail to connect, this guide will help you isolate the cause quickly.

---

## 1. Quick Diagnostic Checklist

```bash
# 1. Check if the SessionMunch server is active and listening
sessionmunch status

# 2. Check health endpoint via curl
curl -i http://127.0.0.1:49374/health

# 3. Verify SQLite and Markdown integrity
sessionmunch --data-dir ~/.local/share/sessionmunch lint
```

---

## 2. Common Issues & Solutions

### A. Agent does not receive handoff context at session start
- **Cause 1:** Startup hooks are not installed for this harness.
  - **Fix:** Re-run `sessionmunch install-hooks --agent <name> --apply`.
- **Cause 2:** The harness does not consume hook stdout (e.g. Grok or Zero).
  - **Fix:** Ask the agent directly to run `memory_handoff_accept`.
- **Cause 3:** Working directory mismatch.
  - **Fix:** Handoffs are scoped to directory boundaries. Ensure you launched the new session in the same directory or within the same `.sessionmunch.toml` project marker.

### B. `database is locked` error
- **Cause:** Multiple `sessionmunch serve` processes pointing to the same data directory.
  - **Fix:** SessionMunch uses a strict single-writer model. Run `pkill -f sessionmunch` and restart exactly one server instance or service.

### C. Connection refused (`curl: (7) Failed to connect to 127.0.0.1:49374`)
- **Cause:** The server is not running or crashed due to misconfiguration.
  - **Fix:** Inspect the service log (`journalctl --user -u sessionmunch.service -e` or container logs). If binding failed due to an existing process on port 49374, verify with `ss -tulpn | grep 49374`.

### D. DNS rebinding rejection (`403 Forbidden` on LAN/remote access)
- **Cause:** Accessing the server via a hostname or LAN IP not listed in `allowed_hosts`.
  - **Fix:** Add your host/IP to `allowed_hosts` in `config.toml` or set `SESSIONMUNCH_ALLOWED_HOSTS="localhost,127.0.0.1,homelab,192.168.1.50"`.

### E. Index desynchronization or corrupted SQLite DB
- **Cause:** External filesystem changes, abrupt power loss, or manual edits to wiki files.
  - **Fix:** Run the safe non-destructive reindex command to rebuild the SQLite database from markdown source files:
    ```bash
    sessionmunch reindex
    ```
