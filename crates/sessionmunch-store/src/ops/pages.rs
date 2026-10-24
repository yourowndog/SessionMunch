//! Page writes: upserts, OKF migration, entity/section backfills, links and evidence.

use super::*;

/// Upsert a page by path, superseding any existing latest version when the
/// content (sha256 of body) has changed.
///
/// Returns the id of the page row that should now be considered current.
pub fn upsert_page(conn: &mut Connection, page: &NewPage) -> StoreResult<PageId> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let (result_id, _passages) = upsert_page_in_tx(&tx, page, now)?;
    tx.commit()?;
    Ok(result_id)
}

/// Upsert a batch of pages inside one transaction. Either *all* pages
/// land (each becoming the new `is_latest=true` version) or none do.
///
/// This is the M7b atomic-fan-out path: the consolidator can hand a
/// list of {sessions, concepts, decisions} pages and trust that
/// either the whole batch supersedes or the wiki is unchanged.
pub fn upsert_pages_batch(conn: &mut Connection, pages: &[NewPage]) -> StoreResult<Vec<PageId>> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let mut out = Vec::with_capacity(pages.len());
    for page in pages {
        let (id, _passages) = upsert_page_in_tx(&tx, page, now)?;
        out.push(id);
    }
    tx.commit()?;
    Ok(out)
}

pub(super) struct ExistingPageVersion {
    id: Vec<u8>,
    body_sha256: Vec<u8>,
    frontmatter_json: String,
    title: String,
    tier: String,
    pinned: i64,
}

/// Normalise a page path into FTS-friendly search text, indexing BOTH forms
/// so either a whole-slug or a single-word query hits:
/// - segments: `/` and `.` → space, KEEPING `-`/`_` (FTS token chars) so the
///   full hyphenated slug stays one token (`foo-bar` matches a `"foo-bar"`
///   query);
/// - words: also split `-`/`_` so each word is its own token (`bar` matches).
///
/// `notes/foo-bar.md` → `notes foo-bar md notes foo bar md`.
///
/// MUST stay byte-identical to the backfill expression in migration V17 so
/// the `rebuild` and live-write paths index the same text (matching bm25
/// term frequencies, not just the same match set).
pub(crate) fn path_search_text(path: &str) -> String {
    let segments = path.replace(['/', '.'], " ");
    let words = segments.replace(['-', '_'], " ");
    format!("{segments} {words}")
}

/// Count latest page rows whose frontmatter is not yet OKF-conformant
/// (missing `type` or `generated.at`). Read-only; the migration uses it
/// to decide whether the backup gate must engage at all.
pub fn okf_nonconformant_latest_pages(conn: &Connection) -> StoreResult<u64> {
    let mut stmt = conn.prepare("SELECT path, frontmatter_json FROM pages WHERE is_latest = 1")?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut pending = 0u64;
    for (path, fm_str) in rows {
        let fm: serde_json::Value = serde_json::from_str(&fm_str).unwrap_or_default();
        let mut conformed = fm.clone();
        sessionmunch_core::okf::conform_frontmatter(&path, &mut conformed);
        if conformed != fm || sessionmunch_core::okf::generated_at(&fm).is_none() {
            pending += 1;
        }
    }
    Ok(pending)
}

/// One latest-page row rewritten in place by [`okf_migrate_latest_pages`].
#[derive(Debug, Clone)]
pub struct OkfMigratedPage {
    /// Owning workspace.
    pub workspace_id: sessionmunch_core::WorkspaceId,
    /// Owning project.
    pub project_id: sessionmunch_core::ProjectId,
    /// Wiki-relative page path.
    pub path: String,
    /// The `generated.at` stamped from the row's own `updated_at`.
    pub generated_at: String,
}

/// One-shot OKF migration of every latest page row (docs/okf.md): conform
/// the frontmatter IN PLACE — same id, same version row, `updated_at`
/// untouched, body untouched — stamping `generated.at` from the row's own
/// `updated_at` so nothing looks freshly written. Historical versions
/// (`is_latest = 0`) stay as they were. Idempotent: conformed rows are
/// left alone, so a re-run rewrites zero pages.
pub fn okf_migrate_latest_pages(conn: &mut Connection) -> StoreResult<Vec<OkfMigratedPage>> {
    type LatestPageRow = (Vec<u8>, Vec<u8>, Vec<u8>, String, String, i64);
    let tx = conn.transaction()?;
    let mut migrated = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT id, workspace_id, project_id, path, frontmatter_json, updated_at              FROM pages WHERE is_latest = 1",
        )?;
        let rows: Vec<LatestPageRow> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        for (id, ws, proj, path, fm_str, updated_at) in rows {
            let fm: serde_json::Value = serde_json::from_str(&fm_str).unwrap_or_default();
            let mut conformed = fm.clone();
            sessionmunch_core::okf::conform_frontmatter(&path, &mut conformed);
            let at = jiff::Timestamp::from_microsecond(updated_at)
                .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
                .strftime("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
            if sessionmunch_core::okf::generated_at(&conformed).is_none() {
                sessionmunch_core::okf::stamp_generated_at(&mut conformed, &at);
            }
            if conformed == fm {
                continue;
            }
            tx.execute(
                "UPDATE pages SET frontmatter_json = ?1 WHERE id = ?2",
                params![serde_json::to_string(&conformed)?, id],
            )?;
            migrated.push(OkfMigratedPage {
                workspace_id: sessionmunch_core::WorkspaceId::from_slice(&ws)?,
                project_id: sessionmunch_core::ProjectId::from_slice(&proj)?,
                path,
                generated_at: sessionmunch_core::okf::generated_at(&conformed)
                    .unwrap_or(&at)
                    .to_string(),
            });
        }
    }
    tx.commit()?;
    Ok(migrated)
}

/// Stamp the per-version `generated.at` (write time, UTC) onto conformed
/// frontmatter and serialize it. Runs only on the paths that create a new
/// version row — the idempotent short-circuit above never reaches it, so
/// an unchanged page keeps its original timestamp.
pub(super) fn stamped_frontmatter(
    mut conformed: serde_json::Value,
    now_us: i64,
) -> StoreResult<String> {
    // The wiki layer stamps (or inherits) `generated.at` before the file
    // is emitted; keep that value so file and row stay byte-aligned. The
    // stamp here covers writers that reach the store without a wiki file.
    if sessionmunch_core::okf::generated_at(&conformed).is_none() {
        let ts = jiff::Timestamp::from_microsecond(now_us)
            .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
            .strftime("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        sessionmunch_core::okf::stamp_generated_at(&mut conformed, &ts);
    }
    Ok(serde_json::to_string(&conformed)?)
}

/// What a [`backfill_entity_index`] pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EntityBackfillSummary {
    /// Latest pages that gained at least one entity link.
    pub pages_backfilled: u64,
    /// Entity→page links inserted.
    pub links_added: u64,
}

/// One-shot, idempotent backfill of the entity index from existing
/// frontmatter (docs/temporal.md).
///
/// The entity retrieval stream and every `as_of` timeline read from
/// `entities` / `entity_page_links`, which are populated only when a page
/// is written with entities. Historically those came from an LLM
/// consolidator's `entities:` field, absent on the bulk of a real store —
/// bootstrapped pages predate it and stable pages are never
/// re-consolidated — so both features sat empty on mature deployments.
///
/// This derives each latest page's entities from its `tags`/`entities`
/// frontmatter (the exact rule the write path now uses,
/// [`sessionmunch_core::frontmatter_entity_names`]) and attaches the links
/// the page version never got, with `valid_from` = the version's
/// `created_at` — the same window semantics the V56 migration used, and
/// honest because those tags have lived in the frontmatter since that
/// version was written.
///
/// Only latest pages that currently have NO entity links are considered,
/// so a store whose pages were written through the fixed path is a cheap
/// no-op and a re-run inserts nothing (the link insert is also
/// `ON CONFLICT DO NOTHING`). Superseded versions are left untouched.
pub fn backfill_entity_index(conn: &mut Connection) -> StoreResult<EntityBackfillSummary> {
    type CandidateRow = (Vec<u8>, Vec<u8>, Vec<u8>, String, i64);
    let tx = conn.transaction()?;
    let mut summary = EntityBackfillSummary::default();
    {
        let rows: Vec<CandidateRow> = {
            let mut stmt = tx.prepare(
                "SELECT p.id, p.workspace_id, p.project_id, p.frontmatter_json, p.created_at \
                 FROM pages p \
                 WHERE p.is_latest = 1 \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM entity_page_links l WHERE l.page_id = p.id \
                   )",
            )?;
            stmt.query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect::<Result<_, _>>()?
        };
        for (page_id, ws, proj, fm_str, created_at) in rows {
            let fm: serde_json::Value = serde_json::from_str(&fm_str).unwrap_or_default();
            let names = sessionmunch_core::frontmatter_entity_names(&fm);
            if names.is_empty() {
                continue;
            }
            let mut page_gained_a_link = false;
            for name in names {
                let entity_id: Vec<u8> = tx.query_row(
                    "INSERT INTO entities (id, workspace_id, project_id, name, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT (workspace_id, project_id, name) DO UPDATE SET name = excluded.name \
                     RETURNING id",
                    params![EntityId::new().as_bytes(), &ws, &proj, name, created_at],
                    |row| row.get(0),
                )?;
                let inserted = tx.execute(
                    "INSERT INTO entity_page_links (entity_id, page_id, valid_from) \
                     VALUES (?1, ?2, ?3) \
                     ON CONFLICT (entity_id, page_id) DO NOTHING",
                    params![entity_id, &page_id, created_at],
                )?;
                if inserted > 0 {
                    summary.links_added += 1;
                    page_gained_a_link = true;
                }
            }
            if page_gained_a_link {
                summary.pages_backfilled += 1;
            }
        }
    }
    tx.commit()?;
    Ok(summary)
}

/// Rebuild the section/passage index for all latest pages in a scope,
/// processing in bounded batches. Returns the count of pages processed.
pub fn backfill_sections(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    batch_size: usize,
) -> StoreResult<u64> {
    let tx = conn.transaction()?;
    let rows: Vec<(Vec<u8>,)> = tx
        .prepare(
            "SELECT p.id FROM pages p
             WHERE p.workspace_id = ?1 AND p.project_id = ?2 AND p.is_latest = 1",
        )?
        .query_map([workspace_id.as_bytes(), project_id.as_bytes()], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?,))
        })?
        .collect::<Result<_, _>>()?;
    drop(tx);

    let batch_size = batch_size.max(1);
    let mut count: u64 = 0;
    for chunk in rows.chunks(batch_size) {
        let tx = conn.transaction()?;
        for (page_id_bytes,) in chunk {
            let page_id = PageId::from_slice(page_id_bytes)?;
            let body: String = tx.query_row(
                "SELECT body FROM pages WHERE id = ?1",
                params![page_id_bytes],
                |row| row.get(0),
            )?;
            crate::passage_index::replace_page_sections_and_passages(
                &tx,
                page_id,
                *workspace_id,
                *project_id,
                &body,
            )?;
            count += 1;
        }
        tx.commit()?;
    }
    Ok(count)
}

pub(crate) fn upsert_page_in_tx(
    tx: &rusqlite::Transaction<'_>,
    page: &NewPage,
    now: i64,
) -> StoreResult<(PageId, Vec<crate::passage_index::CreatedPassage>)> {
    let path_search = path_search_text(page.path.as_str());
    let body_sha256: [u8; 32] = {
        let mut hasher = Sha256::new();
        hasher.update(page.body.as_bytes());
        hasher.finalize().into()
    };
    // OKF conformance at the single write choke point (docs/okf.md):
    // fill the deterministic keys, compare idempotency modulo the
    // per-version `generated.at`, and stamp `at` only when content
    // actually changed (below).
    let mut conformed = page.frontmatter_json.clone();
    sessionmunch_core::okf::conform_frontmatter(page.path.as_str(), &mut conformed);
    let tier_str = page.tier.as_str();

    let existing: Option<ExistingPageVersion> = tx
        .query_row(
            "SELECT id, body_sha256, frontmatter_json, title, tier, pinned FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![
                page.workspace_id.as_bytes(),
                page.project_id.as_bytes(),
                page.path.as_str(),
            ],
            |row| {
                Ok(ExistingPageVersion {
                    id: row.get(0)?,
                    body_sha256: row.get(1)?,
                    frontmatter_json: row.get(2)?,
                    title: row.get(3)?,
                    tier: row.get(4)?,
                    pinned: row.get(5)?,
                })
            },
        )
        .optional()?;

    if let Some(existing) = existing {
        let existing_fm: serde_json::Value =
            serde_json::from_str(&existing.frontmatter_json).unwrap_or_default();
        if existing.body_sha256 == body_sha256
            && sessionmunch_core::okf::strip_generated_at(&existing_fm)
                == sessionmunch_core::okf::strip_generated_at(&conformed)
            && existing.title == page.title
            && existing.tier == tier_str
            && existing.pinned == i64::from(page.pinned)
        {
            let unchanged_id = PageId::from_slice(&existing.id).map_err(StoreError::from)?;
            // The content short-circuit skips a new version row, but a
            // reconsolidation from a different session still cites the
            // page it reaffirmed (P2, docs/design-hindsight-borrowings.md
            // §3) — record the evidence against the still-current id.
            insert_evidence_in_tx(tx, &unchanged_id, &page.evidence, now)?;
            return Ok((unchanged_id, Vec::new()));
        }
        let frontmatter_str = stamped_frontmatter(conformed, now)?;
        let new_id = PageId::new();
        // The page-grain ingestion window closes in the same statement,
        // same instant (issue #656, docs/design-page-ingestion-windows.md).
        tx.execute(
            "UPDATE pages SET is_latest = 0, valid_to = ?2 WHERE id = ?1",
            params![&existing.id, now],
        )?;
        // Close the superseded version's entity-link windows at the new
        // version's birth instant (docs/temporal.md).
        tx.execute(
            "UPDATE entity_page_links SET superseded_at = ?2 \
             WHERE page_id = ?1 AND superseded_at IS NULL",
            params![&existing.id, now],
        )?;
        tx.execute(
            "INSERT INTO pages \
             (id, workspace_id, project_id, path, path_search, title, tier, body, body_sha256, \
              frontmatter_json, is_latest, supersedes, pinned, author_id, \
              created_at, updated_at, expires_at, valid_from) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?12, ?13, ?14, ?14, ?15, ?14)",
            params![
                new_id.as_bytes(),
                page.workspace_id.as_bytes(),
                page.project_id.as_bytes(),
                page.path.as_str(),
                path_search,
                page.title,
                tier_str,
                page.body,
                body_sha256.as_slice(),
                frontmatter_str,
                &existing.id,
                i64::from(page.pinned),
                page.author_id.map(|id| id.as_bytes().to_vec()),
                now,
                page.expires_at.map(|ts| ts.as_microsecond()),
            ],
        )?;
        replace_links_in_tx(tx, &new_id, page)?;
        attach_entities_in_tx(tx, &new_id, page, now)?;
        refresh_incoming_links_for_path(tx, page, &new_id)?;
        insert_evidence_in_tx(tx, &new_id, &page.evidence, now)?;
        // Rebuild the section/passage index for this page version in the same
        // transaction (AGENTS.md invariant #3: indexes commit with the data).
        let passages = crate::passage_index::replace_page_sections_and_passages(
            tx,
            new_id,
            page.workspace_id,
            page.project_id,
            &page.body,
        )?;
        audit(
            tx,
            "supersede_page",
            Some(page.workspace_id.as_bytes()),
            Some(page.project_id.as_bytes()),
            Some(new_id.as_bytes()),
            page.author_id
                .as_ref()
                .map(sessionmunch_core::UserId::as_bytes),
            now,
        )?;
        return Ok((new_id, passages));
    }
    let frontmatter_str = stamped_frontmatter(conformed, now)?;
    let new_id = PageId::new();
    tx.execute(
        "INSERT INTO pages \
         (id, workspace_id, project_id, path, path_search, title, tier, body, body_sha256, \
          frontmatter_json, is_latest, pinned, author_id, created_at, updated_at, expires_at, valid_from) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?12, ?13, ?13, ?14, ?13)",
        params![
            new_id.as_bytes(),
            page.workspace_id.as_bytes(),
            page.project_id.as_bytes(),
            page.path.as_str(),
            path_search,
            page.title,
            tier_str,
            page.body,
            body_sha256.as_slice(),
            frontmatter_str,
            i64::from(page.pinned),
            page.author_id.map(|id| id.as_bytes().to_vec()),
            now,
            page.expires_at.map(|ts| ts.as_microsecond()),
        ],
    )?;
    replace_links_in_tx(tx, &new_id, page)?;
    attach_entities_in_tx(tx, &new_id, page, now)?;
    refresh_incoming_links_for_path(tx, page, &new_id)?;
    insert_evidence_in_tx(tx, &new_id, &page.evidence, now)?;
    // Rebuild the section/passage index for this page version in the same
    // transaction (AGENTS.md invariant #3: indexes commit with the data).
    let passages = crate::passage_index::replace_page_sections_and_passages(
        tx,
        new_id,
        page.workspace_id,
        page.project_id,
        &page.body,
    )?;
    audit(
        tx,
        "create_page",
        Some(page.workspace_id.as_bytes()),
        Some(page.project_id.as_bytes()),
        Some(new_id.as_bytes()),
        page.author_id
            .as_ref()
            .map(sessionmunch_core::UserId::as_bytes),
        now,
    )?;
    Ok((new_id, passages))
}

/// Attach the normalized entity set to a new page version (V38). Entity
/// rows are shared within one project; links stay attached to immutable
/// page versions and latest-page filtering controls retrieval.
pub(super) fn attach_entities_in_tx(
    tx: &rusqlite::Transaction<'_>,
    page_id: &PageId,
    page: &NewPage,
    version_created_at: i64,
) -> StoreResult<()> {
    if page.entities.is_empty() {
        return Ok(());
    }
    let now = Timestamp::now().as_microsecond();
    // Defence in depth: the wiki layer normalises on the way in, but the
    // store is the last gate before the UNIQUE(name) constraint.
    for name in sessionmunch_core::normalize_entities(&page.entities) {
        let entity_id: Vec<u8> = tx.query_row(
            "INSERT INTO entities (id, workspace_id, project_id, name, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (workspace_id, project_id, name) DO UPDATE SET name = excluded.name \
             RETURNING id",
            params![
                EntityId::new().as_bytes(),
                page.workspace_id.as_bytes(),
                page.project_id.as_bytes(),
                name,
                now,
            ],
            |row| row.get(0),
        )?;
        // Bi-temporal-lite (docs/temporal.md): the link's window opens
        // when its page version was created and stays open until the
        // version is superseded (closed in `upsert_page_in_tx`).
        tx.execute(
            "INSERT INTO entity_page_links (entity_id, page_id, valid_from) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT (entity_id, page_id) DO NOTHING",
            params![entity_id, page_id.as_bytes(), version_created_at],
        )?;
    }
    Ok(())
}

pub(super) fn replace_links_in_tx(
    tx: &rusqlite::Transaction<'_>,
    from_page_id: &PageId,
    page: &NewPage,
) -> StoreResult<()> {
    tx.execute(
        "DELETE FROM links WHERE from_page_id = ?1",
        params![from_page_id.as_bytes()],
    )?;

    let mut seen = BTreeSet::new();
    for link in &page.links {
        // The relation joins the dedupe key (and the table's PK): a
        // typed edge and a plain reference to the same target are two
        // different rows, mirroring `link_type` in the schema.
        let key = (
            link.workspace.clone(),
            link.project.clone(),
            link.path.as_str().to_string(),
            link.relation,
        );
        if !seen.insert(key) {
            continue;
        }
        let to_page_id = latest_page_id_for_link(tx, page, link)?;
        let to_page_blob = to_page_id.as_ref().map(|id| &id.as_bytes()[..]);
        tx.execute(
            "INSERT INTO links \
                 (from_page_id, to_page_id, to_workspace, to_project, to_path, link_type) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                from_page_id.as_bytes(),
                to_page_blob,
                link.workspace,
                link.project,
                link.path.as_str(),
                link.relation
                    .map_or("references", sessionmunch_core::Relation::as_str),
            ],
        )?;
    }
    Ok(())
}

/// Record a page write's evidence sources (P2,
/// docs/design-hindsight-borrowings.md §3), in the same transaction as the
/// page upsert. `INSERT OR IGNORE` on the `(page_id, source_kind,
/// source_id)` PK makes re-citing the same source a no-op — a session that
/// reconsolidates the same path twice does not inflate the count. Empty
/// `evidence` (every pre-2.2 caller) inserts nothing.
pub(super) fn insert_evidence_in_tx(
    tx: &rusqlite::Transaction<'_>,
    page_id: &PageId,
    evidence: &[PageEvidence],
    now: i64,
) -> StoreResult<()> {
    for e in evidence {
        tx.execute(
            "INSERT OR IGNORE INTO page_evidence (page_id, source_kind, source_id, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![page_id.as_bytes(), e.kind.as_str(), e.source_id, now],
        )?;
    }
    Ok(())
}

/// Resolve a link target to the latest page id it points at, or `None` if the
/// target workspace / project / page does not exist yet (an unresolved forward
/// link). A bare link resolves within the source page's own project; a
/// `[[project:path]]` / `[[workspace/project:path]]` link resolves against the
/// named project (same workspace when only the project is given).
pub(super) fn latest_page_id_for_link(
    tx: &rusqlite::Transaction<'_>,
    page: &NewPage,
    link: &LinkTarget,
) -> StoreResult<Option<PageId>> {
    let (workspace_blob, project_blob): (Vec<u8>, Vec<u8>) = match &link.project {
        None => (
            page.workspace_id.as_bytes().to_vec(),
            page.project_id.as_bytes().to_vec(),
        ),
        Some(project_name) => {
            let workspace_blob: Vec<u8> = match &link.workspace {
                None => page.workspace_id.as_bytes().to_vec(),
                Some(workspace_name) => {
                    let found: Option<Vec<u8>> = tx
                        .query_row(
                            "SELECT id FROM workspaces WHERE name = ?1",
                            params![workspace_name],
                            |row| row.get(0),
                        )
                        .optional()?;
                    match found {
                        Some(id) => id,
                        None => return Ok(None),
                    }
                }
            };
            let project_blob: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT id FROM projects WHERE workspace_id = ?1 AND name = ?2",
                    params![workspace_blob, project_name],
                    |row| row.get(0),
                )
                .optional()?;
            match project_blob {
                Some(id) => (workspace_blob, id),
                None => return Ok(None),
            }
        }
    };

    let bytes: Option<Vec<u8>> = tx
        .query_row(
            "SELECT id FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![workspace_blob, project_blob, link.path.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    bytes
        .map(|bytes| PageId::from_slice(&bytes).map_err(StoreError::from))
        .transpose()
}

pub(super) fn refresh_incoming_links_for_path(
    tx: &rusqlite::Transaction<'_>,
    page: &NewPage,
    latest_page_id: &PageId,
) -> StoreResult<()> {
    // (1) Bare (same-project) links: from_page lives in this page's project and
    // the target carries no scope. Repoints all matches (not only unresolved):
    // a new page version changes the latest id, so resolved links must follow.
    tx.execute(
        "UPDATE links \
         SET to_page_id = ?1 \
         WHERE to_project IS NULL AND to_path = ?2 \
           AND EXISTS ( \
               SELECT 1 FROM pages from_page \
               WHERE from_page.id = links.from_page_id \
                 AND from_page.workspace_id = ?3 \
                 AND from_page.project_id = ?4 \
           )",
        params![
            latest_page_id.as_bytes(),
            page.path.as_str(),
            page.workspace_id.as_bytes(),
            page.project_id.as_bytes(),
        ],
    )?;

    // (2) Cross-project links naming this page's project by name. `to_workspace`
    // may be explicit (cross-workspace) or NULL (same workspace as the source).
    let project_name: Option<String> = tx
        .query_row(
            "SELECT name FROM projects WHERE id = ?1",
            params![page.project_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    let workspace_name: Option<String> = tx
        .query_row(
            "SELECT name FROM workspaces WHERE id = ?1",
            params![page.workspace_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    if let (Some(project_name), Some(workspace_name)) = (project_name, workspace_name) {
        tx.execute(
            "UPDATE links \
             SET to_page_id = ?1 \
             WHERE to_project = ?2 AND to_path = ?3 \
               AND ( \
                   to_workspace = ?4 \
                   OR ( \
                       to_workspace IS NULL \
                       AND EXISTS ( \
                           SELECT 1 FROM pages from_page \
                           WHERE from_page.id = links.from_page_id \
                             AND from_page.workspace_id = ?5 \
                       ) \
                   ) \
               )",
            params![
                latest_page_id.as_bytes(),
                project_name,
                page.path.as_str(),
                workspace_name,
                page.workspace_id.as_bytes(),
            ],
        )?;
    }
    Ok(())
}
