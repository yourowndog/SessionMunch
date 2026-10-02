//! End-to-end tests for `sessionmunch legacy-import`.
//!
//! Runs the real compiled binary against temp directories to verify
//! detection, import, integrity, resume, and fresh-install safety.
//! These tests create a legacy ai-memory fixture, run the import, and
//! assert on file state — they never modify real data or network.

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Path to the built `sessionmunch` binary.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sessionmunch")
}

/// Build a pristine child process with no inherited sessionmunch env.
fn child(data_dir: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(bin());
    cmd.env("HOME", home).env("SESSIONMUNCH_DATA_DIR", data_dir);
    for (key, _) in std::env::vars() {
        if key.starts_with("SESSIONMUNCH_") || key.starts_with("AI_MEMORY_") {
            cmd.env_remove(&key);
        }
    }
    cmd.env("SESSIONMUNCH_DATA_DIR", data_dir);
    cmd
}

/// Create a representative legacy ai-memory fixture at `root/ai-memory-data/`
/// with a config, marker file, and some wiki pages.
fn create_legacy_fixture(root: &Path) -> PathBuf {
    let dd = root.join("ai-memory-data");
    std::fs::create_dir_all(&dd).unwrap();

    // Config file
    let config =
        "\n[workspace]\nname = \"fw\"\n\n[project]\nname = \"fp\"\n\n[llm]\nprovider = \"none\"\n";
    std::fs::write(dd.join("config.toml"), config.as_bytes()).unwrap();

    // Marker
    std::fs::write(
        dd.join(".ai-memory.toml"),
        "workspace = \"fw\"\nproject = \"fp\"\n".as_bytes(),
    )
    .unwrap();

    // Wiki pages under wiki/fw/fp/
    let wd = dd.join("wiki").join("fw").join("fp");
    std::fs::create_dir_all(&wd).unwrap();
    std::fs::write(
        wd.join("architecture.md"),
        "# Architecture\n\nLegacy design notes.\n".as_bytes(),
    )
    .unwrap();
    std::fs::write(
        wd.join("decisions.md"),
        "# Decisions\n\nADR-001: Use SQLite.\n".as_bytes(),
    )
    .unwrap();
    let notes_dir = wd.join("notes");
    std::fs::create_dir_all(&notes_dir).unwrap();
    std::fs::write(
        notes_dir.join("team.md"),
        "# Team\n\nAlice, Bob.\n".as_bytes(),
    )
    .unwrap();

    // Nested subdirectory
    let nested = wd.join("deep").join("sub");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join("detail.md"),
        "# Detail\n\nDeep page.\n".as_bytes(),
    )
    .unwrap();

    dd
}

/// Walk a directory and return all file paths relative to `root`.
fn walk_rel(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
            .flatten()
        {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p.strip_prefix(root).unwrap_or(&p);
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

/// Compute SHA-256 hex of a file.
fn file_sha256(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(&bytes);
    d.iter().map(|b| format!("{b:02x}")).collect::<String>()
}

// ── Tests ────────────────────────────────────────────────────────────

#[test]
fn detection_is_read_only() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&home).unwrap();

    let fixture = create_legacy_fixture(tmp.path());
    // capture pre-hashes of source files
    let md_files = walk_rel(&fixture)
        .iter()
        .filter(|r| r.ends_with(".md"))
        .map(|r| r.to_owned())
        .collect::<Vec<_>>();
    let mut pre_hashes: Vec<(String, String)> = Vec::new();
    for f in md_files {
        pre_hashes.push((file_sha256(&fixture.join(&f)), f));
    }

    // Run detection (no --apply = dry-run)
    let output = child(&data, &home)
        .arg("legacy-import")
        .arg("--path")
        .arg(&fixture)
        .output()
        .expect("legacy-import detection should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "detection must exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        stdout.contains("ai-memory detection"),
        "detection header missing"
    );
    assert!(
        stdout.contains("wiki pages: 4"),
        "should find 4 wiki pages, got: {stdout:?}"
    );

    // Prove no files written by detection
    for (ph, r) in pre_hashes {
        let actual = file_sha256(&fixture.join(&r));
        assert_eq!(actual, ph, "source file {} changed after detection", r);
    }
}

#[test]
fn full_import_cycle_with_integrity_check() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&home).unwrap();

    let fixture = create_legacy_fixture(tmp.path());

    // Init the fresh data dir so sessionmunch has a valid config
    let init = child(&data, &home)
        .arg("init")
        .output()
        .expect("init should succeed");
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    // Pre-hashes of source files
    let src_files = walk_rel(&fixture)
        .iter()
        .filter(|r| r.ends_with(".md"))
        .map(|r| r.to_owned())
        .collect::<Vec<_>>();
    let mut pre_hashes: Vec<(String, String)> = Vec::new();
    for f in src_files {
        pre_hashes.push((file_sha256(&fixture.join(&f)), f));
    }

    // Run dry-run first (should report plan, write nothing)
    let dry = child(&data, &home)
        .arg("legacy-import")
        .arg("--path")
        .arg(&fixture)
        .output()
        .expect("dry-run should succeed");
    let dry_stdout = String::from_utf8_lossy(&dry.stdout);
    assert!(
        dry_stdout.contains("dry-run"),
        "dry-run should say 'dry-run': {dry_stdout:?}"
    );
    assert!(
        !data.join("wiki").join("fw").join("fp").exists(),
        "dry-run must not create import destination: {}",
        data.display(),
    );

    // Now run with --apply --create-destination
    let import_output = child(&data, &home)
        .arg("legacy-import")
        .arg("--path")
        .arg(&fixture)
        .arg("--apply")
        .arg("--create-destination")
        .output()
        .expect("import with --apply should succeed");
    let import_stdout = String::from_utf8_lossy(&import_output.stdout);
    assert!(
        import_stdout.contains("import complete"),
        "import should complete: {import_stdout:?}",
    );
    assert!(
        import_stdout.contains("source integrity: CONFIRMED"),
        "integrity should be confirmed: {import_stdout:?}",
    );

    // Verify destination files exist with correct content
    let wiki_root = data.join("wiki").join("fw").join("fp");
    assert!(wiki_root.is_dir(), "destination wiki dir must exist");
    let dest_files = walk_rel(&wiki_root);
    assert!(
        dest_files.contains(&"architecture.md".to_string()),
        "architecture.md should be in dest, got: {:?}",
        dest_files,
    );
    assert!(
        dest_files.contains(&"decisions.md".to_string()),
        "decisions.md should be in dest, got: {:?}",
        dest_files,
    );
    assert!(
        dest_files.contains(&"notes/team.md".to_string()),
        "notes/team.md should be in dest, got: {:?}",
        dest_files,
    );
    // Should have at least 4 pages (the 3 above + the nested page)
    assert!(
        dest_files.len() >= 4,
        "should be at least 4 pages in dest, got: {:?}",
        dest_files,
    );

    // Source files MUST be unchanged
    for (ph, r) in pre_hashes {
        let actual = file_sha256(&fixture.join(&r));
        assert_eq!(actual, ph, "source file {} changed after import", r);
    }

    // Verify manifest was written
    let manifest = fixture.join(".legacy-import-manifest.json");
    assert!(manifest.is_file(), "manifest must exist after import");

    // Re-running --apply should be idempotent (all pages already copied)
    let reimport = child(&data, &home)
        .arg("legacy-import")
        .arg("--path")
        .arg(&fixture)
        .arg("--apply")
        .output()
        .expect("re-import should succeed");
    let reimport_stdout = String::from_utf8_lossy(&reimport.stdout);
    assert!(
        reimport_stdout.contains("nothing to do") || reimport_stdout.contains("already"),
        "re-import should be a no-op: {reimport_stdout:?}",
    );
}

#[test]
fn fresh_install_creates_no_legacy_compat_dirs() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&home).unwrap();

    // Init into a fresh data dir
    let init = child(&data, &home)
        .arg("init")
        .output()
        .expect("init should succeed");
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr),
    );

    // Check NO legacy ai-memory directories are created inside the data dir
    let all_files = walk_rel(&data);
    for f in all_files {
        let lower = f.to_lowercase();
        assert!(
            !lower.contains("ai-memory"),
            "fresh install must not create ai-memory paths, found: {f}",
        );
    }

    // Also check that no .ai-memory.toml marker was created
    let old_marker = data.join(".ai-memory.toml");
    assert!(
        !old_marker.exists(),
        "fresh install must not create .ai-memory.toml"
    );
}

#[test]
fn create_destination_flag_required_when_dest_missing() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&home).unwrap();

    let fixture = create_legacy_fixture(tmp.path());
    // Must init data dir first (needs a valid sessionmunch config)
    let init = child(&data, &home)
        .arg("init")
        .output()
        .expect("init should succeed");
    assert!(init.status.success());

    // Attempt import without --create-destination: should fail
    let result = child(&data, &home)
        .arg("legacy-import")
        .arg("--path")
        .arg(&fixture)
        .arg("--apply")
        .output()
        .expect("command should run (exit non-zero is expected)");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        !result.status.success(),
        "import without --create-destination should fail: {stderr}",
    );
    assert!(
        stderr.contains("--create-destination") || stderr.contains("create-destination"),
        "error should mention --create-destination: {stderr}",
    );

    // Same command with --create-destination should pass
    let ok = child(&data, &home)
        .arg("legacy-import")
        .arg("--path")
        .arg(&fixture)
        .arg("--apply")
        .arg("--create-destination")
        .output()
        .expect("import with --create-destination should succeed");
    let ok_stdout = String::from_utf8_lossy(&ok.stdout);
    assert!(
        ok_stdout.contains("import complete"),
        "should complete: {ok_stdout:?}",
    );
}

#[test]
fn interrupted_import_is_resumable() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&home).unwrap();

    let fixture = create_legacy_fixture(tmp.path());
    let init = child(&data, &home)
        .arg("init")
        .output()
        .expect("init should succeed");
    assert!(init.status.success());

    // First run: do the full import so manifest is written
    let first = child(&data, &home)
        .arg("legacy-import")
        .arg("--path")
        .arg(&fixture)
        .arg("--apply")
        .arg("--create-destination")
        .output()
        .expect("first import should succeed");
    let first_stdout = String::from_utf8_lossy(&first.stdout);
    assert!(
        first_stdout.contains("import complete"),
        "first import: {first_stdout:?}"
    );

    // Verify all files are in place
    let wiki_root = data.join("wiki").join("fw").join("fp");
    let dest_files = walk_rel(&wiki_root);
    assert_eq!(
        dest_files.len(),
        4,
        "all 4 pages should be present: {:?}",
        dest_files
    );

    // Add a new page to source to test that only NEW pages are imported
    let wd = fixture.join("wiki").join("fw").join("fp");
    std::fs::write(wd.join("new.md"), "# New page\n".as_bytes()).unwrap();

    // Re-run: should only import the new page
    let resume_output = child(&data, &home)
        .arg("legacy-import")
        .arg("--path")
        .arg(&fixture)
        .arg("--apply")
        .output()
        .expect("resume import should succeed");
    let resume_stdout = String::from_utf8_lossy(&resume_output.stdout);
    assert!(
        resume_stdout.contains("import complete"),
        "resume should complete: {resume_stdout:?}",
    );

    // Now all 5 pages should be present
    let dest_files2 = walk_rel(&wiki_root);
    assert_eq!(
        dest_files2.len(),
        5,
        "after resume all 5 pages should be present: {:?}",
        dest_files2
    );
}

#[test]
fn absent_legacy_dir_reports_clear_error() {
    let tmp = TempDir::new().expect("tempdir");
    let data = tmp.path().join("data");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let result = child(&data, &home)
        .arg("legacy-import")
        .arg("--path")
        .arg(tmp.path().join("nonexistent"))
        .output()
        .expect("command should run (exit non-zero is expected)");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        !result.status.success(),
        "should fail on missing path: {stderr}",
    );
}
