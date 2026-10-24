//! Admin routes: export-okf.

use super::*;

/// Query for `POST /admin/export-okf`: the one project bundle to export.
#[derive(Deserialize)]
pub(super) struct ExportOkfQuery {
    workspace: String,
    project: String,
}

/// `POST /admin/export-okf?workspace=…&project=…` — stream the project
/// directory as an OKF v0.2 bundle tarball (docs/okf.md). The wiki files
/// ARE the bundle (native conformance), so this is a validated copy with
/// a freshly generated `index.md` (full listing + `okf_version`). Any
/// non-conformant page file fails the export rather than shipping a
/// bundle a strict reader would reject.
pub(super) async fn handle_export_okf(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<ExportOkfQuery>,
) -> Response {
    let ids = match lookup_ws_proj_no_create(&state, q.workspace.trim(), q.project.trim()).await {
        Ok(ids) => ids,
        Err((status, body)) => return (status, body).into_response(),
    };
    match build_okf_bundle_file(&state, ids.0, ids.1).await {
        Ok(file) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/gzip")
            .header(
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"okf-bundle.tar.gz\"",
            )
            .body(Body::from_stream(ReaderStream::new(file)))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err(e) => {
            warn!(error = %e, "okf export failed");
            let body = serde_json::to_vec(&serde_json::json!({ "error": e.to_string() }))
                .unwrap_or_default();
            Response::builder()
                .status(StatusCode::UNPROCESSABLE_ENTITY)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

pub(super) async fn build_okf_bundle_file(
    state: &AdminState,
    ws: sessionmunch_core::WorkspaceId,
    proj: sessionmunch_core::ProjectId,
) -> anyhow::Result<tokio::fs::File> {
    let bundle_dir = state
        .data_dir
        .join("wiki")
        .join(ws.to_string())
        .join(proj.to_string());
    if !bundle_dir.is_dir() {
        anyhow::bail!("project has no wiki directory yet");
    }

    // Validate + collect: every non-reserved .md must be conformant.
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    let mut families: Vec<String> = Vec::new();
    let mut stack = vec![bundle_dir.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                // `_pending/` is the auto-improve staging area — proposal
                // sidecars, not concept files; a strict OKF reader has no
                // business seeing them (post-audit finding: an unresolved
                // pending proposal blocked every export).
                if entry.file_name() == "_pending" {
                    continue;
                }
                if dir == bundle_dir {
                    families.push(entry.file_name().to_string_lossy().into_owned());
                }
                stack.push(path);
            } else if ft.is_file() && path.extension().is_some_and(|e| e == "md") {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == "index.md" || name == "log.md" {
                    continue; // regenerated / not adopted
                }
                let raw = std::fs::read_to_string(&path)?;
                let fm = sessionmunch_wiki::parse(&raw)
                    .map(|m| m.frontmatter)
                    .unwrap_or_default();
                if !sessionmunch_core::okf::is_conformant(&fm) {
                    anyhow::bail!(
                        "page {} is not OKF-conformant; run the server once to migrate \
                         before exporting",
                        path.strip_prefix(&bundle_dir).unwrap_or(&path).display()
                    );
                }
                files.push(path);
            }
        }
    }

    let mut tar_file = tempfile::tempfile()?;
    {
        let encoder = GzEncoder::new(&mut tar_file, Compression::default());
        let mut tar = tar::Builder::new(encoder);
        tar.mode(tar::HeaderMode::Deterministic);
        tar.follow_symlinks(false);
        // Fresh bundle-root index.md: okf_version + full listing.
        families.sort();
        let listing: String = families
            .iter()
            .map(|f| format!("- [{f}/]({f}/)\n"))
            .collect();
        let index = format!(
            "---\nokf_version: \"0.2\"\n---\n\n# Bundle index\n\nConcept files live in these directories:\n\n{listing}"
        );
        let mut header = tar::Header::new_gnu();
        header.set_size(index.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "index.md", index.as_bytes())?;
        for path in files {
            let rel = path.strip_prefix(&bundle_dir).unwrap_or(&path);
            tar.append_path_with_name(&path, rel)?;
        }
        let encoder = tar.into_inner()?;
        encoder.finish()?;
    }
    tar_file.sync_data()?;
    tar_file.rewind()?;
    Ok(tokio::fs::File::from_std(tar_file))
}
