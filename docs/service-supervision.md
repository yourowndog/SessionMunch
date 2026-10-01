# Service & Supervisor Setup

Running SessionMunch as a managed background service ensures reliable startup on machine boot, automatic recovery after restarts, and structured journal logging.

> **Important:** System services (systemd, launchd, docker) provide **process supervision and lifecycle management only**. They are **not** capture or recording hooks. Capturing prompt and tool events is handled exclusively by agent lifecycle hooks (`sessionmunch install-hooks`).

---

## 1. Systemd (Linux: User Service vs. System Service)

### A. Systemd User Service (Recommended for Workstations)

A systemd user service runs in your personal user session and starts automatically upon user login (or boot if lingering is enabled via `loginctl enable-linger`).

Packaged unit file: `packaging/systemd/sessionmunch-user.service`

```bash
# 1. Initialize user directories
mkdir -p ~/.config/sessionmunch ~/.local/share/sessionmunch

# 2. Initialize configuration if not already present
sessionmunch --data-dir ~/.local/share/sessionmunch init

# 3. Enable and start the user service
systemctl --user enable --now sessionmunch.service

# 4. Check service status and logs
systemctl --user status sessionmunch.service
journalctl --user -u sessionmunch.service -f
```

### B. Systemd System Service (Recommended for Shared Servers / Homelabs)

Runs as a dedicated unprivileged system user (`sessionmunch`), storing data under `/var/lib/sessionmunch` and config under `/etc/sessionmunch/config.toml`.

Packaged unit file: `packaging/systemd/sessionmunch.service`

```bash
# Enable and start the system service
sudo systemctl enable --now sessionmunch.service

# View logs
sudo journalctl -u sessionmunch.service -f
```

---

## 2. macOS: launchd Agent

On macOS, SessionMunch can be managed via `launchd` using the bundled LaunchAgent plist (`packaging/launchd/com.sessionmunch.server.plist`):

```bash
cp packaging/launchd/com.sessionmunch.server.plist ~/Library/LaunchAgents/
launchctl load -w ~/Library/LaunchAgents/com.sessionmunch.server.plist
```

---

## 3. Docker / Podman Container Supervision

For containerized environments, pass `--restart unless-stopped` to ensure process restart across Docker daemon restarts or host reboot:

```bash
docker run -d --name sessionmunch \
  --restart unless-stopped \
  -p 127.0.0.1:49374:49374 \
  -v sessionmunch-data:/data \
  docker.io/yourowndog/sessionmunch:latest
```
