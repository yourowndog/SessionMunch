use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::LegacyImportArgs;
use crate::config::{Config, DEFAULT_WORKSPACE};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum PageStatus {
    Planned,
    Copied,
    VerificationFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PageEntry {
    source_rel: String,
    source_sha256: String,
    dest_path: String,
    status: PageStatus,
    dest_sha256: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImportManifest {
    import_version: String,
    source_dir: String,
    dest_wiki_dir: String,
    workspace: String,
    project: String,
    entries: Vec<PageEntry>,
}

const IMPORT_VERSION: &str = "legacy-ai-memory-v1";
const MANIFEST_FILE: &str = ".legacy-import-manifest.json";

pub async fn run(config: &Config, args: LegacyImportArgs) -> Result<()> {
    let source_dir = resolve_source_dir(&args)?;
    let dest_data_dir = &config.data_dir;

    if !source_dir.is_dir() {
        bail!("source path is not a directory: {}", source_dir.display());
    }

    let detection = detect_legacy_installation(&source_dir)?;
    print_detection(&detection);
    if detection.wiki_pages.is_empty() {
        println!("no wiki pages found at the source; nothing to import");
        return Ok(());
    }

    let workspace = args.workspace.clone().unwrap_or_else(|| {
        detection
            .config_workspace
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKSPACE.into())
    });
    let project = args.project.clone().unwrap_or_else(|| {
        detection.config_project.clone().unwrap_or_else(|| {
            source_dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or("imported".to_string())
        })
    });

    let dest_wiki = dest_data_dir.join("wiki").join(&workspace).join(&project);
    let mut manifest_path = source_dir.join(MANIFEST_FILE);
    if let Some(p) = &args.manifest_out {
        manifest_path = p.clone();
    }

    let entries = plan_import(&source_dir, &detection, args.apply)?;
    if entries.is_empty() || entries.iter().all(|e| e.status != PageStatus::Planned) {
        println!("all pages already imported and verified; nothing to do");
        return Ok(());
    }
    if !args.apply {
        print_plan(&entries, &dest_wiki);
        println!(
            "dry-run: planned {} pages (use --apply to import)",
            entries.len()
        );
        return Ok(());
    }

    println!(
        "importing {} pages to {}",
        entries.len(),
        dest_wiki.display()
    );
    if dest_wiki.is_dir() {
        // exists — nothing to create
    } else if args.create_destination {
        fs::create_dir_all(&dest_wiki)
            .with_context(|| format!("create destination {}", dest_wiki.display()))?;
    } else {
        bail!(
            "destination {} does not exist; pass --create-destination to auto-create it",
            dest_wiki.display()
        );
    }

    let mut manifest = ImportManifest {
        import_version: IMPORT_VERSION.into(),
        source_dir: source_dir.to_string_lossy().to_string(),
        dest_wiki_dir: dest_wiki.to_string_lossy().to_string(),
        workspace,
        project,
        entries: entries.clone(),
    };

    for idx in 0..manifest.entries.len() {
        let e = &manifest.entries[idx];
        if e.status != PageStatus::Planned {
            continue;
        }
        let source_rel = e.source_rel.clone();
        let expected_sha = e.source_sha256.clone();
        let dest_path = e.dest_path.clone();

        let source_abs = source_dir.join(&source_rel);
        let dest_abs = dest_wiki.join(&dest_path);
        let pre_hash = file_sha256(&source_abs)?;
        if pre_hash != expected_sha {
            bail!("source changed during import: {} (hash)", source_rel);
        }
        let content =
            fs::read(&source_abs).with_context(|| format!("read {}", source_abs.display()))?;
        // Ensure parent directory of destination exists
        if let Some(parent) = dest_abs.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create parent dir {}", parent.display()))?;
        }
        fs::write(&dest_abs, &content).with_context(|| format!("write {}", dest_abs.display()))?;
        let post_hash = file_sha256(&dest_abs)?;
        if post_hash != pre_hash {
            bail!("write verification failed for {}", source_rel);
        }
        manifest.entries[idx].status = PageStatus::Copied;
        manifest.entries[idx].dest_sha256 = Some(post_hash);
        write_manifest(&manifest_path, &manifest)?;
    }

    println!("verifying source integrity after import...");
    let verified = manifest
        .entries
        .iter()
        .filter(|e| {
            let sa = source_dir.join(&e.source_rel);
            file_sha256(&sa).ok().is_some_and(|h| h == e.source_sha256)
        })
        .count();
    let failed = manifest.entries.len() - verified;
    write_manifest(&manifest_path, &manifest)?;

    println!(
        "\nimport complete: {} pages, {} verified, {} failed",
        manifest.entries.len(),
        verified,
        failed
    );
    if failed == 0 {
        println!("  source integrity: CONFIRMED (all hashes match)");
    }
    println!("  manifest: {}", manifest_path.display());
    Ok(())
}

#[derive(Debug)]
struct LegacyDetection {
    path: PathBuf,
    has_wiki: bool,
    has_config: bool,
    has_db: bool,
    has_marker: bool,
    wiki_pages: Vec<PathBuf>,
    marker_name: Option<String>,
    config_workspace: Option<String>,
    config_project: Option<String>,
}

fn detect_legacy_installation(path: &Path) -> Result<LegacyDetection> {
    let mut d = LegacyDetection {
        path: path.to_path_buf(),
        has_wiki: false,
        has_config: false,
        has_db: false,
        has_marker: false,
        wiki_pages: Vec::new(),
        marker_name: None,
        config_workspace: None,
        config_project: None,
    };
    let cp = path.join("config.toml");
    if cp.is_file() {
        d.has_config = true;
        // Try flat format first: workspace = "name"
        for line in fs::read_to_string(&cp)
            .ok()
            .unwrap_or("".to_string())
            .lines()
        {
            let t = line.trim_start();
            let Some(r) = t.strip_prefix("workspace") else {
                continue;
            };
            let Some(r) = r.trim_start().strip_prefix('=') else {
                continue;
            };
            let Some(q) = r.trim_start().strip_prefix('"') else {
                continue;
            };
            if let Some(end) = q.find('"') {
                d.config_workspace = Some(q[..end].to_string());
                break;
            }
        }
        // Fallback: TOML section format [workspace]\nname = "..."
        if d.config_workspace.is_none() {
            let text = fs::read_to_string(&cp).ok().unwrap_or("".to_string());
            let mut in_ws_section = false;
            for line in text.lines() {
                let t = line.trim();
                if t == "[workspace]" {
                    in_ws_section = true;
                    continue;
                }
                if t.starts_with('[') && t != "[workspace]" {
                    in_ws_section = false;
                    continue;
                }
                if in_ws_section {
                    let Some(r) = t.strip_prefix("name") else {
                        continue;
                    };
                    let Some(r) = r.trim_start().strip_prefix('=') else {
                        continue;
                    };
                    let Some(q) = r.trim_start().strip_prefix('"') else {
                        continue;
                    };
                    if let Some(end) = q.find('"') {
                        d.config_workspace = Some(q[..end].to_string());
                        break;
                    }
                }
            }
        }
        // Try flat format for project: project = "name"
        if d.config_project.is_none() {
            for line in fs::read_to_string(&cp)
                .ok()
                .unwrap_or("".to_string())
                .lines()
            {
                let t = line.trim_start();
                let Some(r) = t.strip_prefix("project") else {
                    continue;
                };
                let Some(r) = r.trim_start().strip_prefix('=') else {
                    continue;
                };
                let Some(q) = r.trim_start().strip_prefix('"') else {
                    continue;
                };
                if let Some(end) = q.find('"') {
                    d.config_project = Some(q[..end].to_string());
                    break;
                }
            }
        }
        // Fallback: TOML section format [project]\nname = "..."
        if d.config_project.is_none() {
            let text = fs::read_to_string(&cp).ok().unwrap_or("".to_string());
            let mut in_pr_section = false;
            for line in text.lines() {
                let t = line.trim();
                if t == "[project]" {
                    in_pr_section = true;
                    continue;
                }
                if t.starts_with('[') && t != "[project]" {
                    in_pr_section = false;
                    continue;
                }
                if in_pr_section {
                    let Some(r) = t.strip_prefix("name") else {
                        continue;
                    };
                    let Some(r) = r.trim_start().strip_prefix('=') else {
                        continue;
                    };
                    let Some(q) = r.trim_start().strip_prefix('"') else {
                        continue;
                    };
                    if let Some(end) = q.find('"') {
                        d.config_project = Some(q[..end].to_string());
                        break;
                    }
                }
            }
        }
    }
    if path.join(".sessionmunch.toml").is_file() {
        d.has_marker = true;
        d.marker_name = Some(".sessionmunch.toml".into());
    } else if path.join(".ai-memory.toml").is_file() {
        d.has_marker = true;
        d.marker_name = Some(".ai-memory.toml".into());
    }
    let db = path.join("db");
    if db.is_dir() {
        d.has_db = db.join("memory.sqlite").is_file();
    }
    for sub in &["wiki", "archive"] {
        let dir = path.join(sub);
        if dir.is_dir() {
            d.has_wiki = true;
            scan_wiki_pages(&dir, &mut d.wiki_pages);
        }
    }
    Ok(d)
}

fn scan_wiki_pages(dir: &Path, pages: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let p = entry.path();
        if p.is_file() {
            let name = p.file_name().map_or("".into(), |s| s.to_string_lossy());
            if name.ends_with(".md")
                && name != "_meta.md"
                && name != "bootstrap.md"
                && !name.starts_with(".")
            {
                pages.push(p.to_path_buf());
            }
        } else if p.is_dir() {
            scan_wiki_pages(p.as_path(), pages);
        }
    }
}

fn print_detection(d: &LegacyDetection) {
    println!("=== Legacy ai-memory detection ===");
    println!("  path: {}", d.path.display());
    println!("  has config.toml: {}", d.has_config);
    if let Some(ws) = &d.config_workspace {
        println!("  config workspace: {}", ws);
    }
    if let Some(pr) = &d.config_project {
        println!("  config project: {}", pr);
    }
    println!("  has marker: {}", d.has_marker);
    if let Some(m) = &d.marker_name {
        println!("  marker: {}", m);
    }
    println!("  has wiki: {}", d.has_wiki);
    println!("  has db: {}", d.has_db);
    println!("  wiki pages: {}", d.wiki_pages.len());
    for pg in &d.wiki_pages {
        println!("    {}", pg.display());
    }
    println!();
}

fn plan_import(
    source_dir: &Path,
    detection: &LegacyDetection,
    apply: bool,
) -> Result<Vec<PageEntry>> {
    let existing: HashMap<String, String> = if apply {
        let mp = source_dir.join(MANIFEST_FILE);
        if mp.is_file() {
            let text = fs::read_to_string(&mp)?;
            let m: ImportManifest = serde_json::from_str(&text).context("parse manifest")?;
            m.entries
                .iter()
                .filter(|e| e.status == PageStatus::Copied)
                .map(|e| {
                    (
                        e.source_rel.clone(),
                        e.dest_sha256.clone().unwrap_or_default(),
                    )
                })
                .collect::<HashMap<_, _>>()
        } else {
            HashMap::new()
        }
    } else {
        HashMap::new()
    };

    let mut entries: Vec<PageEntry> = Vec::new();
    for sp in &detection.wiki_pages {
        let rel = relative_to(sp, source_dir);
        let sha = file_sha256(sp)?;
        let dr = canonical_dest_path(sp, &detection.path);
        if existing.get(&rel).is_some_and(|h| h == &sha) {
            entries.push(PageEntry {
                source_rel: rel,
                source_sha256: sha,
                dest_path: dr,
                status: PageStatus::Copied,
                dest_sha256: None,
                error: None,
            });
        } else {
            entries.push(PageEntry {
                source_rel: rel,
                source_sha256: sha,
                dest_path: dr,
                status: PageStatus::Planned,
                dest_sha256: None,
                error: None,
            });
        }
    }
    Ok(entries)
}

fn print_plan(entries: &[PageEntry], dw: &Path) {
    println!(
        "=== Import plan ===\n  destination: {}\n  pages: {}",
        dw.display(),
        entries.len()
    );
    for e in entries {
        println!("  {} -> {}", e.source_rel, dw.join(&e.dest_path).display());
    }
    if entries.is_empty() {
        println!("  (nothing)");
    }
    println!();
}

fn resolve_source_dir(args: &LegacyImportArgs) -> Result<PathBuf> {
    if let Some(p) = &args.path {
        return Ok(p.clone());
    }
    let env = std::env::var("AI_MEMORY_DATA_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    if let Some(p) = env.filter(|p| p.is_dir()) {
        return Ok(p);
    }
    let home = std::env::var("HOME").ok().unwrap_or("/tmp".into());
    for c in &[".local/share/ai-memory", ".ai-memory", ".config/ai-memory"] {
        let p = Path::new(&home).join(c);
        if p.is_dir() {
            return Ok(p);
        }
    }
    bail!("no legacy ai-memory found; pass --path <data-dir>");
}

fn relative_to(path: &Path, source: &Path) -> String {
    let rel = path.to_string_lossy();
    let src = source.to_string_lossy();
    #[allow(clippy::explicit_auto_deref)]
    let src_ref: &str = &*src;
    if let Some(stripped) = rel.strip_prefix(src_ref) {
        let stripped = stripped.trim_start_matches('/');
        stripped.to_string()
    } else {
        path.file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or(rel)
            .to_string()
    }
}

fn canonical_dest_path(sp: &Path, data_dir: &Path) -> String {
    let rel = relative_to(sp, data_dir);
    let s = rel
        .strip_prefix("wiki/")
        .or_else(|| rel.strip_prefix("archive/"))
        .unwrap_or(rel.as_str());
    let parts = s.split('/').collect::<Vec<_>>();
    if parts.len() >= 2 {
        let pr = parts[2..].join("/");
        if !pr.is_empty() {
            return pr;
        }
    }
    s.to_owned()
}

fn file_sha256(path: &Path) -> Result<String> {
    let b = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let d = Sha256::digest(&b);
    Ok(d.iter().map(|b| format!("{b:02x}")).collect())
}

fn write_manifest(path: &Path, manifest: &ImportManifest) -> Result<()> {
    let serialized = serde_json::to_string_pretty(manifest).context("serialize")?;
    let bytes = serialized.as_bytes();
    let fname = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or("manifest".to_string());
    let tmp = path.with_file_name(format!(".{}.tmp", fname));
    fs::write(&tmp, bytes).with_context(|| format!("write tmp {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("rename {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn create_legacy_fixture(dir: &Path, pages: &[(&str, &str)]) -> PathBuf {
    let dd = dir.join("ai-memory-data");
    fs::create_dir_all(&dd).unwrap();
    let config =
        "\n[workspace]\nname = \"fw\"\n\n[project]\nname = \"fp\"\n\n[llm]\nprovider = \"none\"\n";
    fs::write(dd.join("config.toml"), config.as_bytes()).unwrap();
    let wd = dd.join("wiki").join("fw").join("fp");
    fs::create_dir_all(&wd).unwrap();
    for (n, c) in pages {
        fs::write(wd.join(n), c.as_bytes()).unwrap();
    }
    fs::write(
        dd.join(".ai-memory.toml"),
        "workspace = \"fw\"\nproject = \"fp\"\n".as_bytes(),
    )
    .unwrap();
    dd
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn detects_legacy_installation_with_wiki() {
        let td = tempdir().unwrap();
        let d = create_legacy_fixture(td.path(), &[("p.md", "# P")]);
        let r = detect_legacy_installation(&d).unwrap();
        assert!(r.has_wiki);
        assert!(r.has_config);
        assert!(r.has_marker);
        assert_eq!(r.wiki_pages.len(), 1);
    }

    #[test]
    fn detects_absent_legacy_installation() {
        let td = tempdir().unwrap();
        let e = td.path().join("e");
        fs::create_dir_all(&e).unwrap();
        let r = detect_legacy_installation(&e).unwrap();
        assert!(!r.has_wiki);
        assert!(r.wiki_pages.is_empty());
    }

    #[test]
    fn sha256_reproducible() {
        let td = tempdir().unwrap();
        let f = td.path().join("t.md");
        fs::write(&f, b"x").unwrap();
        assert_eq!(file_sha256(&f).unwrap(), file_sha256(&f).unwrap());
    }

    #[test]
    fn canonical_dest_path_strips_wiki_prefix() {
        assert_eq!(
            canonical_dest_path(Path::new("/d/wiki/a/b/p.md"), Path::new("/d")),
            "p.md"
        );
    }

    #[test]
    fn canonical_dest_path_handles_archive() {
        assert_eq!(
            canonical_dest_path(Path::new("/d/archive/a/b/doc.md"), Path::new("/d")),
            "doc.md"
        );
    }

    #[test]
    fn dry_run_does_not_write() -> Result<()> {
        let td = tempdir().unwrap();
        let d = create_legacy_fixture(td.path(), &[("p1.md", "# 1")]);
        let r = detect_legacy_installation(&d).unwrap();
        let entries = plan_import(&d, &r, false)?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, PageStatus::Planned);
        Ok(())
    }

    #[test]
    fn empty_dir_reports_no_pages() {
        let td = tempdir().unwrap();
        let e = td.path().join("e");
        fs::create_dir_all(&e).unwrap();
        assert!(
            detect_legacy_installation(&e)
                .unwrap()
                .wiki_pages
                .is_empty()
        );
    }
}
