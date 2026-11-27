# Upgrade, Backup & Recovery

SessionMunch ensures your knowledge base is resilient, durable, and easily recoverable.

---

## 1. Upgrades

Back up and verify the exact candidate before upgrading. Schema migrations may make a database unreadable to an older binary; rollback requires a compatible database snapshot.

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
SessionMunch includes an online SQLite snapshot command that packages the database, wiki and configuration. It does not currently archive every artifact blob, native snapshot or client outbox, so preserve those separately. Backups may contain private data or credentials; restrict access.

```bash
sessionmunch backup --to ~/backups/sessionmunch-$(date +%Y%m%d).tar.gz
```

### B. Filesystem / Git Backup
The wiki is ordinary Markdown under `<data_dir>/wiki/`. Standard backup tooling can preserve it and the other authoritative data:
- **Git:** Commit and push the `<data_dir>/wiki` directory to a private Git remote.
- **rsync / restic / Borg:** Preserve the complete data directory and any externally configured artifact/snapshot roots. Stop the service for a consistent filesystem copy, or use a coordinated snapshot strategy that includes SQLite WAL state. Preserve client outboxes separately.

---

## 3. Disaster Recovery & Reconstruction

In the event of database corruption or file loss:

1. **Restore from Backup:**
   ```bash
   sessionmunch restore --from ~/backups/sessionmunch-20261001.tar.gz
   ```

2. **Reindex the wiki only when appropriate:** Restore the database from a
   verified snapshot before rebuilding derived indexes. Do not delete SQLite
   expecting Markdown to reconstruct the event ledger, session identities,
   access state, handoffs or artifact provenance. Stop the target service and
   test restoration into a separate empty data directory first.

The native restore command expects the built-in archive layout; a complete
filesystem recovery also requires the separately backed-up artifact and snapshot
roots. Test the entire recovery procedure before treating it as a release gate.
