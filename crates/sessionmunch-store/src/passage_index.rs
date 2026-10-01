//! Struct mappings for page_sections and page_passages tables.

use crate::error::StoreResult;
use sessionmunch_core::sections;
use sessionmunch_core::sections::count_tokens;
use sessionmunch_core::{PageId, ProjectId, WorkspaceId};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// A row from `page_sections`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageSection {
    /// Deterministic UUID derived from `page_id` + `ordinal` + `heading_path`.
    pub id: Uuid,
    /// The page this section belongs to.
    pub page_id: PageId,
    /// Denormalized scope for fast (workspace, project) filtering.
    pub workspace_id: WorkspaceId,
    /// Denormalized scope for fast (workspace, project) filtering.
    pub project_id: ProjectId,
    /// Zero-based position of this section within its page.
    pub ordinal: i64,
    /// Markdown heading level (1 for `#`, 2 for `##`, ...).
    pub level: i64,
    /// The section's own heading text.
    pub heading: String,
    /// Canonical encoding of the full ancestor heading chain.
    pub heading_path: String,
    /// Section source text, excluding frontmatter.
    pub body: String,
    /// UTF-8 byte offset where this section starts in the page body.
    pub start_byte: i64,
    /// UTF-8 byte offset where this section ends in the page body.
    pub end_byte: i64,
    /// SHA-256 of `body`, used for change detection.
    pub content_sha256: [u8; 32],
}

impl PageSection {
    /// Compute the deterministic UUID id from page_id + ordinal + heading_path.
    pub fn compute_id(page_id: PageId, ordinal: u32, heading_path: &str) -> Uuid {
        let mut hasher = Sha256::new();
        Digest::update(&mut hasher, page_id.as_bytes());
        Digest::update(&mut hasher, ordinal.to_be_bytes());
        Digest::update(&mut hasher, heading_path.as_bytes());
        let bytes: [u8; 32] = hasher.finalize().into();
        Uuid::from_slice(&bytes).unwrap_or_else(|_| {
            // Fallback: use first 16 bytes as UUIDv5 would
            Uuid::from_bytes(bytes[..16].try_into().unwrap())
        })
    }

    /// Build a [`PageSection`] row from a parsed section's fields, computing
    /// its deterministic id and content hash.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        page_id: PageId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        ordinal: u32,
        level: u32,
        heading: String,
        heading_path: String,
        body: String,
        start_byte: usize,
        end_byte: usize,
    ) -> Self {
        let id = Self::compute_id(page_id, ordinal, &heading_path);
        let mut hasher = Sha256::new();
        Digest::update(&mut hasher, body.as_bytes());
        let content_sha256 = hasher.finalize().into();
        Self {
            id,
            page_id,
            workspace_id,
            project_id,
            ordinal: ordinal as i64,
            level: level as i64,
            heading,
            heading_path,
            body,
            start_byte: start_byte as i64,
            end_byte: end_byte as i64,
            content_sha256,
        }
    }
}

/// A row from `page_passages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PagePassage {
    /// Deterministic UUID derived from `section_id` + `ordinal` + content hash.
    pub id: Uuid,
    /// The section this passage was split from.
    pub section_id: Uuid,
    /// The page this passage ultimately belongs to.
    pub page_id: PageId,
    /// Denormalized scope for fast (workspace, project) filtering.
    pub workspace_id: WorkspaceId,
    /// Denormalized scope for fast (workspace, project) filtering.
    pub project_id: ProjectId,
    /// Zero-based position of this passage within its section.
    pub ordinal: i64,
    /// The passage's own text.
    pub text: String,
    /// UTF-8 byte offset where this passage starts in the section body.
    pub start_byte: i64,
    /// UTF-8 byte offset where this passage ends in the section body.
    pub end_byte: i64,
    /// Approximate token count, used for retrieval budgeting.
    pub token_count: i64,
    /// SHA-256 of `text`, used for change detection.
    pub content_sha256: [u8; 32],
}

impl PagePassage {
    /// Compute the deterministic UUID id from section id + ordinal + content hash.
    pub fn compute_id(section_id: Uuid, ordinal: u32, content_hash: &[u8; 32]) -> Uuid {
        let mut hasher = Sha256::new();
        Digest::update(&mut hasher, section_id.as_bytes());
        Digest::update(&mut hasher, ordinal.to_be_bytes());
        Digest::update(&mut hasher, content_hash);
        let bytes: [u8; 32] = hasher.finalize().into();
        Uuid::from_slice(&bytes)
            .unwrap_or_else(|_| Uuid::from_bytes(bytes[..16].try_into().unwrap()))
    }

    /// Build a [`PagePassage`] row from a parsed passage's fields, computing
    /// its deterministic id and content hash.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        section_id: Uuid,
        page_id: PageId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        ordinal: u32,
        text: String,
        start_byte: usize,
        end_byte: usize,
        token_count: u32,
    ) -> Self {
        let mut hasher = Sha256::new();
        Digest::update(&mut hasher, text.as_bytes());
        let content_sha256 = hasher.finalize().into();
        let id = Self::compute_id(section_id, ordinal, &content_sha256);
        Self {
            id,
            section_id,
            page_id,
            workspace_id,
            project_id,
            ordinal: ordinal as i64,
            text,
            start_byte: start_byte as i64,
            end_byte: end_byte as i64,
            token_count: token_count as i64,
            content_sha256,
        }
    }
}

/// A passage created during section/passage indexing, with enough data
/// for the caller to compute an embedding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedPassage {
    /// Deterministic id of the passage row.
    pub id: Uuid,
    /// Full text to embed.
    pub text: String,
}

/// Atomically replace all sections and passages for a page version inside a transaction.
///
/// Deletes existing `page_sections` rows for the given `page_id`; the FK
/// `page_passages.section_id -> page_sections(id) ON DELETE CASCADE` then
/// cascade-deletes their `page_passages`, and the `page_passages_fts` AFTER
/// DELETE trigger cleans the FTS index. Then re-inserts via `parse_sections`
/// and `split_passages`. The `page_embeddings` and `page_abstract_embeddings`
/// rows are NOT touched.
///
/// Returns the list of passages created, so the caller can compute embeddings
/// and persist them via [`crate::ops::store_passage_embeddings`].
///
/// # Errors
/// Returns [`StoreError::DatabaseError`] if any SQL operation fails.
pub fn replace_page_sections_and_passages(
    tx: &rusqlite::Transaction<'_>,
    page_id: PageId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    body: &str,
) -> StoreResult<Vec<CreatedPassage>> {
    // 1. Delete old sections — FK cascade from page_sections → page_passages
    //    will delete page_passages rows. The page_passages_fts AFTER DELETE trigger
    //    will clean the FTS index rows.
    tx.execute(
        "DELETE FROM page_sections WHERE page_id = ?1",
        params![page_id.as_bytes()],
    )?;

    // 2. Parse new sections from body
    let parsed_sections = sections::parse_sections(body, page_id);

    // 3. Insert new sections and passages
    let mut created_passages = Vec::new();
    for section in &parsed_sections {
        let heading_path_str = heading_path_json(section);
        let section_row = PageSection::new(
            page_id,
            workspace_id,
            project_id,
            section.ordinal.try_into().unwrap(),
            section.level,
            section.heading.clone(),
            heading_path_str.clone(),
            section.body.clone(),
            section.start_byte,
            section.end_byte,
        );

        tx.execute(
            "INSERT INTO page_sections (id, page_id, workspace_id, project_id, ordinal, level, heading, heading_path, body, start_byte, end_byte, content_sha256) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                section_row.id.as_bytes(),
                section_row.page_id.as_bytes(),
                section_row.workspace_id.as_bytes(),
                section_row.project_id.as_bytes(),
                section_row.ordinal,
                section_row.level,
                section_row.heading,
                section_row.heading_path,
                section_row.body,
                section_row.start_byte,
                section_row.end_byte,
                section_row.content_sha256,
            ],
        )?;

        // Split section into passages and insert each
        let passages = sections::split_passages(section);
        for passage in &passages {
            let token_count = count_tokens(&passage.text);
            let passage_row = PagePassage::new(
                section_row.id,
                page_id,
                workspace_id,
                project_id,
                passage.ordinal.try_into().unwrap(),
                passage.text.clone(),
                passage.start_byte,
                passage.end_byte,
                token_count.try_into().unwrap(),
            );

            tx.execute(
                "INSERT INTO page_passages (id, section_id, page_id, workspace_id, project_id, ordinal, heading_path, text, start_byte, end_byte, token_count, content_sha256) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    passage_row.id.as_bytes(),
                    passage_row.section_id.as_bytes(),
                    passage_row.page_id.as_bytes(),
                    passage_row.workspace_id.as_bytes(),
                    passage_row.project_id.as_bytes(),
                    passage_row.ordinal,
                    heading_path_str,
                    passage_row.text,
                    passage_row.start_byte,
                    passage_row.end_byte,
                    passage_row.token_count,
                    passage_row.content_sha256,
                ],
            )?;

            created_passages.push(CreatedPassage {
                id: passage_row.id,
                text: passage_row.text.clone(),
            });
        }
    }

    Ok(created_passages)
}

/// Convert a section's heading_path Vec<String> to a JSON string for storage.
fn heading_path_json(section: &sections::Section) -> String {
    serde_json::to_string(&section.heading_path).unwrap_or_else(|_| "[]".to_string())
}
