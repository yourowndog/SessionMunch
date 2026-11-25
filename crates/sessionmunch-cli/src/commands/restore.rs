//! `sessionmunch restore --from <tarball>` — restore a backup tarball.
//!
//! Refuses to overwrite a non-empty data dir unless `--force` is given.
//! Refuses while another `sessionmunch` process is alive. After extraction,
//! re-opens the store so any pending migrations run (and a corrupt
//! snapshot fails loudly).
//!
//! # Exception to invariant §16
//!
//! `restore` is one of the documented exceptions to the rule that the CLI
//! is always a thin HTTP client. Restoration is a lifecycle operation that
//! fundamentally requires the server to be stopped: extracting a tarball
//! over a live SQLite WAL writer would corrupt the database. The sysinfo
//! guard at the top of `run` enforces this precondition by refusing to
//! proceed when any sibling `sessionmunch` process is detected.

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use sessionmunch_store::Store;
use std::path::{Component, Path};
use tracing::info;

use crate::cli::RestoreArgs;
use crate::config::Config;
use crate::process_guard::{busy_message, sibling_processes};

/// Run the `restore` subcommand.
///
/// # Errors
/// Returns an error if another `sessionmunch` process is running, the
/// data dir is non-empty without `--force`, the tarball cannot be
/// extracted, or the restored store fails to open.
pub fn run(config: &Config, args: RestoreArgs) -> Result<()> {
    let siblings = sibling_processes();
    if !siblings.is_empty() {
        bail!(busy_message("restore", &siblings));
    }

    if !args.from.is_file() {
        bail!("source tarball {} not found", args.from.display());
    }

    restore_archive(&config.data_dir, &args.from, args.force)?;
    info!("restore complete");
    println!(
        "restored {} -> {}",
        args.from.display(),
        config.data_dir.display()
    );
    Ok(())
}

const RESTORED_PATHS: [&str; 4] = ["wiki", "db", "artifacts", "config.toml"];

fn restore_archive(data_dir: &Path, archive_path: &Path, force: bool) -> Result<()> {
    if has_existing_restore_data(data_dir)? && !force {
        bail!(
            "refusing to restore: data dir at {} is non-empty (pass --force to overwrite)",
            data_dir.display(),
        );
    }
    let parent = data_dir.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".sessionmunch-restore-")
        .tempdir_in(parent)?;
    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("opening {}", archive_path.display()))?;
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    unpack_checked_archive(&mut archive, staging.path()).with_context(|| {
        format!(
            "extracting {} into isolated staging",
            archive_path.display()
        )
    })?;
    if !staging.path().join("db/memory.sqlite").is_file() {
        bail!("backup is missing required db/memory.sqlite");
    }
    {
        let _store = Store::open(staging.path()).context("opening staged restored store")?;
    }
    install_staged_restore(staging.path(), data_dir)?;
    info!(from = %archive_path.display(), into = %data_dir.display(), "validated staged restore installed");
    Ok(())
}

fn has_existing_restore_data(data_dir: &Path) -> Result<bool> {
    for name in ["wiki", "db", "artifacts"] {
        let path = data_dir.join(name);
        if path.is_file() || (path.is_dir() && std::fs::read_dir(path)?.next().is_some()) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn install_staged_restore(staged: &Path, data_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let parent = data_dir.parent().unwrap_or_else(|| Path::new("."));
    let rollback = tempfile::Builder::new()
        .prefix(".sessionmunch-rollback-")
        .tempdir_in(parent)?;
    let mut originals = Vec::<String>::new();
    let mut installed = Vec::<String>::new();

    let result = (|| -> Result<()> {
        for name in RESTORED_PATHS {
            let live = data_dir.join(name);
            if live.exists() {
                std::fs::rename(&live, rollback.path().join(name))
                    .with_context(|| format!("moving existing {name} aside"))?;
                originals.push(name.to_owned());
            }
        }
        for name in RESTORED_PATHS {
            let source = staged.join(name);
            if source.exists() {
                std::fs::rename(&source, data_dir.join(name))
                    .with_context(|| format!("installing restored {name}"))?;
                installed.push(name.to_owned());
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        let rollback_error = rollback_install(data_dir, rollback.path(), &installed, &originals);
        return match rollback_error {
            Ok(()) => Err(error),
            Err(rollback_error) => {
                Err(error.context(format!("restore rollback also failed: {rollback_error}")))
            }
        };
    }
    Ok(())
}

fn rollback_install(
    data_dir: &Path,
    rollback: &Path,
    installed: &[String],
    originals: &[String],
) -> Result<()> {
    for name in installed.iter().rev() {
        remove_path(&data_dir.join(name))?;
    }
    for name in originals.iter().rev() {
        std::fs::rename(rollback.join(name), data_dir.join(name))?;
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn unpack_checked_archive<R: std::io::Read>(
    archive: &mut tar::Archive<R>,
    data_dir: &Path,
) -> Result<()> {
    archive.set_preserve_permissions(false);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let entry_type = entry.header().entry_type();
        validate_restore_entry(&path, entry_type)?;
        entry
            .unpack_in(data_dir)
            .with_context(|| format!("extracting {}", path.display()))?;
    }
    Ok(())
}

/// The backup writes `db/memory.sqlite` via `tar::Builder`'s default sparse
/// detection: on a sparse SQLite snapshot the entry type is GNU-sparse
/// (header byte `S`), not plain `Regular` — `tar::EntryType::is_file()` is
/// `false` for it even though it is a hole-encoded regular file. `tar`
/// already expands the sparse blocks to their full logical content while
/// iterating entries (see `Archive::parse_sparse_header`), so `unpack_in`
/// on such an entry writes byte-identical content to a non-sparse one.
fn is_regular_file(entry_type: tar::EntryType) -> bool {
    entry_type.is_file() || entry_type == tar::EntryType::GNUSparse
}

fn validate_restore_entry(path: &Path, entry_type: tar::EntryType) -> Result<()> {
    if !path.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("backup contains unsafe path: {}", path.display());
    }
    if entry_type.is_symlink() || entry_type.is_hard_link() {
        bail!("backup contains unsupported link entry: {}", path.display());
    }
    if !(is_regular_file(entry_type) || entry_type.is_dir()) {
        bail!("backup contains unsupported entry type: {}", path.display());
    }
    let path_str = path.to_string_lossy();
    let allowed = if entry_type.is_dir() {
        path_str == "wiki"
            || path_str.starts_with("wiki/")
            || path_str == "db"
            || path_str == "artifacts"
            || path_str.starts_with("artifacts/")
    } else {
        path_str == "config.toml"
            || path_str == "db/memory.sqlite"
            || path_str.starts_with("wiki/")
            || path_str.starts_with("artifacts/")
    };
    if !allowed {
        bail!("backup contains unexpected path: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive_with_entry(path: &str, entry_type: tar::EntryType) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            let mut header = tar::Header::new_gnu();
            header.set_path(path).unwrap();
            header.set_entry_type(entry_type);
            if entry_type.is_symlink() || entry_type.is_hard_link() {
                header.set_link_name("/etc/passwd").unwrap();
            }
            let body: &[u8] = if entry_type.is_file() { b"body" } else { b"" };
            header.set_size(body.len() as u64);
            header.set_cksum();
            builder.append(&header, body).unwrap();
            builder.finish().unwrap();
        }
        bytes
    }

    fn write_backup(source: &Path, destination: &Path) {
        use flate2::Compression;
        use flate2::write::GzEncoder;

        let file = std::fs::File::create(destination).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for name in ["wiki", "artifacts"] {
            let path = source.join(name);
            if path.is_dir() {
                builder.append_dir_all(name, path).unwrap();
            }
        }
        builder
            .append_path_with_name(source.join("db/memory.sqlite"), "db/memory.sqlite")
            .unwrap();
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
    }

    #[test]
    fn restore_installs_populated_artifacts_from_validated_staging() {
        let source = tempfile::tempdir().unwrap();
        drop(Store::open(source.path()).unwrap());
        let source_artifact = source.path().join("artifacts/sha256/aa/blob.bin");
        std::fs::create_dir_all(source_artifact.parent().unwrap()).unwrap();
        std::fs::write(&source_artifact, b"artifact-body").unwrap();
        let archive_dir = tempfile::tempdir().unwrap();
        let archive = archive_dir.path().join("backup.tar.gz");
        write_backup(source.path(), &archive);

        let destination_parent = tempfile::tempdir().unwrap();
        let destination = destination_parent.path().join("data");
        restore_archive(&destination, &archive, false).unwrap();

        assert_eq!(
            std::fs::read(destination.join("artifacts/sha256/aa/blob.bin")).unwrap(),
            b"artifact-body"
        );
        assert!(destination.join("db/memory.sqlite").is_file());
    }

    #[test]
    fn failed_staged_validation_leaves_existing_restore_data_untouched() {
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join("db")).unwrap();
        std::fs::write(source.path().join("db/memory.sqlite"), b"not sqlite").unwrap();
        let source_artifact = source.path().join("artifacts/new.bin");
        std::fs::create_dir_all(source_artifact.parent().unwrap()).unwrap();
        std::fs::write(&source_artifact, b"new-artifact").unwrap();
        let archive_dir = tempfile::tempdir().unwrap();
        let archive = archive_dir.path().join("bad-backup.tar.gz");
        write_backup(source.path(), &archive);

        let destination_parent = tempfile::tempdir().unwrap();
        let destination = destination_parent.path().join("data");
        std::fs::create_dir_all(destination.join("wiki")).unwrap();
        std::fs::create_dir_all(destination.join("artifacts")).unwrap();
        std::fs::create_dir_all(destination.join("db")).unwrap();
        std::fs::write(destination.join("wiki/old.md"), b"old-wiki").unwrap();
        std::fs::write(destination.join("artifacts/old.bin"), b"old-artifact").unwrap();
        std::fs::write(destination.join("db/memory.sqlite"), b"old-db").unwrap();

        let error = restore_archive(&destination, &archive, true).unwrap_err();
        assert!(error.to_string().contains("opening staged restored store"));
        assert_eq!(
            std::fs::read(destination.join("wiki/old.md")).unwrap(),
            b"old-wiki"
        );
        assert_eq!(
            std::fs::read(destination.join("artifacts/old.bin")).unwrap(),
            b"old-artifact"
        );
        assert_eq!(
            std::fs::read(destination.join("db/memory.sqlite")).unwrap(),
            b"old-db"
        );
    }

    #[test]
    fn restore_accepts_expected_backup_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            builder.append_dir("wiki", tmp.path()).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_path("wiki/default/project/notes/x.md").unwrap();
            header.set_size(4);
            header.set_cksum();
            builder.append(&header, &b"body"[..]).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_path("db/memory.sqlite").unwrap();
            header.set_size(0);
            header.set_cksum();
            builder.append(&header, &b""[..]).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_path("config.toml").unwrap();
            header.set_size(0);
            header.set_cksum();
            builder.append(&header, &b""[..]).unwrap();
            builder.finish().unwrap();
        }

        let restore_dir = tempfile::TempDir::new().unwrap();
        let mut archive = tar::Archive::new(bytes.as_slice());
        unpack_checked_archive(&mut archive, restore_dir.path()).unwrap();
        assert!(
            restore_dir
                .path()
                .join("wiki/default/project/notes/x.md")
                .is_file()
        );
        assert!(restore_dir.path().join("db/memory.sqlite").is_file());
        assert!(restore_dir.path().join("config.toml").is_file());
    }

    #[test]
    fn restore_rejects_link_entries() {
        for entry_type in [tar::EntryType::symlink(), tar::EntryType::hard_link()] {
            let bytes = archive_with_entry("wiki/link.md", entry_type);
            let restore_dir = tempfile::TempDir::new().unwrap();
            let mut archive = tar::Archive::new(bytes.as_slice());
            let err = unpack_checked_archive(&mut archive, restore_dir.path()).unwrap_err();
            assert!(err.to_string().contains("unsupported link entry"));
        }
    }

    #[test]
    fn validate_restore_entry_accepts_gnu_sparse_for_the_sqlite_path() {
        validate_restore_entry(Path::new("db/memory.sqlite"), tar::EntryType::GNUSparse)
            .expect("a GNU-sparse db/memory.sqlite entry is a regular file, just hole-encoded");
    }

    #[test]
    fn validate_restore_entry_still_rejects_gnu_sparse_at_an_unexpected_path() {
        let err =
            validate_restore_entry(Path::new("secret.txt"), tar::EntryType::GNUSparse).unwrap_err();
        assert!(
            err.to_string().contains("unexpected path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn restore_rejects_unexpected_or_unsafe_paths() {
        for path in ["../config.toml", "/tmp/x"] {
            let err = validate_restore_entry(Path::new(path), tar::EntryType::file()).unwrap_err();
            assert!(
                err.to_string().contains("unsafe path"),
                "unexpected error for {path}: {err}"
            );
        }

        let path = "db/extra.sqlite";
        let bytes = archive_with_entry(path, tar::EntryType::file());
        let restore_dir = tempfile::TempDir::new().unwrap();
        let mut archive = tar::Archive::new(bytes.as_slice());
        let err = unpack_checked_archive(&mut archive, restore_dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("unexpected path"),
            "unexpected error for {path}: {err}"
        );
    }

    #[cfg(target_os = "linux")]
    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    /// Whether the `tar` on PATH is GNU tar with sparse support, so the
    /// real-fixture test below can be skipped everywhere else (musl/BSD
    /// `tar`, or no `tar` at all) without breaking those environments.
    #[cfg(target_os = "linux")]
    fn gnu_tar_with_sparse_available() -> bool {
        std::process::Command::new("tar")
            .arg("--version")
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("GNU tar"))
    }

    /// Reproduces #718: the backup command's `tar::Builder` (default
    /// `sparse: true`) writes `db/memory.sqlite` as a GNU-sparse entry
    /// whenever the SQLite snapshot has real holes on disk. This builds an
    /// actual sparse file, archives it with real GNU tar (never a hand-built
    /// header — that would prove nothing about whether unpacking works), and
    /// asserts the restored file is byte-identical to the original logical
    /// content.
    #[cfg(target_os = "linux")]
    #[test]
    fn restore_round_trips_a_real_gnu_sparse_sqlite_snapshot() {
        if !gnu_tar_with_sparse_available() {
            eprintln!(
                "skipping restore_round_trips_a_real_gnu_sparse_sqlite_snapshot: \
                 GNU tar with --sparse not available on PATH"
            );
            return;
        }

        use std::io::{Seek, SeekFrom, Write};

        let src = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(src.path().join("wiki/default/project/notes")).unwrap();
        std::fs::write(src.path().join("wiki/default/project/notes/a.md"), b"body").unwrap();
        std::fs::write(src.path().join("config.toml"), b"# cfg\n").unwrap();
        std::fs::create_dir_all(src.path().join("db")).unwrap();
        let sqlite_path = src.path().join("db/memory.sqlite");
        {
            let mut f = std::fs::File::create(&sqlite_path).unwrap();
            f.write_all(&[0xAB; 8192]).unwrap();
            // Seek far past the last write without writing the gap: on a
            // filesystem that supports holes (tmpfs, ext4, xfs, btrfs) this
            // leaves a real hole rather than allocated zero pages.
            f.seek(SeekFrom::Start(8 * 1024 * 1024)).unwrap();
            f.write_all(&[0xCD; 8192]).unwrap();
        }
        let expected_hash = sha256_hex(&std::fs::read(&sqlite_path).unwrap());

        let archive_dir = tempfile::TempDir::new().unwrap();
        let archive_path = archive_dir.path().join("backup.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("--sparse")
            .arg("-czf")
            .arg(&archive_path)
            .arg("-C")
            .arg(src.path())
            .arg("config.toml")
            .arg("wiki")
            .arg("db/memory.sqlite")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "GNU tar failed to build the fixture archive"
        );

        // Confirm the fixture actually reproduces the bug before trusting the
        // restore result: db/memory.sqlite must be a GNU-sparse entry, not a
        // plain regular file (which would exercise nothing new).
        {
            let file = std::fs::File::open(&archive_path).unwrap();
            let dec = GzDecoder::new(file);
            let mut ar = tar::Archive::new(dec);
            let mut found_sparse = false;
            for entry in ar.entries().unwrap() {
                let entry = entry.unwrap();
                if entry.path().unwrap().as_ref() == Path::new("db/memory.sqlite") {
                    found_sparse = entry.header().entry_type() == tar::EntryType::GNUSparse;
                }
            }
            assert!(
                found_sparse,
                "fixture archive did not encode db/memory.sqlite as GNU-sparse; \
                 test does not exercise #718"
            );
        }

        let restore_dir = tempfile::TempDir::new().unwrap();
        let file = std::fs::File::open(&archive_path).unwrap();
        let dec = GzDecoder::new(file);
        let mut ar = tar::Archive::new(dec);
        unpack_checked_archive(&mut ar, restore_dir.path()).unwrap();

        let restored = std::fs::read(restore_dir.path().join("db/memory.sqlite")).unwrap();
        assert_eq!(
            sha256_hex(&restored),
            expected_hash,
            "restored SQLite content must byte-match the original logical content"
        );
        assert!(restore_dir.path().join("config.toml").is_file());
        assert!(
            restore_dir
                .path()
                .join("wiki/default/project/notes/a.md")
                .is_file()
        );
    }
}
