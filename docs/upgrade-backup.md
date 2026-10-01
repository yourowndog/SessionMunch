# Upgrade, Backup & Recovery

SessionMunch ensures your knowledge base is resilient, durable, and easily recoverable.

---

## 1. Upgrades

Upgrades are non-destructive and backward compatible.

- **Arch Linux (AUR):**
  ```bash
  yay -Syu sessionmunch-bin
  systemctl --user restart sessionmunch.service
  ```
- **Docker Container:**
  ```bash
  docker pull docker.io/yourowndog/sessionmunch:latest
  docker stop sessionmunch && docker rm sessionmunch
  # Re-run your docker run command with the existing volume
  ```

Database migrations execute automatically at startup. If an upgrade includes schema changes, SessionMunch applies them via atomic migration steps before serving requests.

---

## 2. Backup Strategy

### A. Automated Native Backup
SessionMunch includes a built-in atomic backup command that produces a self-contained tarball of the wiki and indexes:

```bash
sessionmunch backup --out ~/backups/sessionmunch-$(date +%Y%m%d).tar.gz
```

### B. Filesystem / Git Backup
Because the primary source of truth is a standard directory of Markdown files under `<data_dir>/wiki/`, standard backup tooling works natively:
- **Git:** Commit and push the `<data_dir>/wiki` directory to a private Git remote.
- **rsync / restic / Borg:** Snapshot the `<data_dir>` path directly.

---

## 3. Disaster Recovery & Reconstruction

In the event of database corruption or file loss:

1. **Restore from Backup:**
   ```bash
   sessionmunch restore --archive ~/backups/sessionmunch-20261001.tar.gz
   ```

2. **Rebuild Database from Markdown:**
   If only the SQLite database file (`<data_dir>/db/`) was damaged, delete the SQLite file and re-run:
   ```bash
   sessionmunch reindex
   ```
   SessionMunch will parse all `.md` files in the wiki directory, re-extract entities and links, re-compute vectors, and reconstruct the database cleanly.
