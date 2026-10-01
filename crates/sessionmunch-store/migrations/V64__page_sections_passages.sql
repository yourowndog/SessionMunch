-- Passage-level retrieval schema (D1b - schema only)
-- Additive schema for section and passage storage with FTS and embeddings
-- Based on ARCHITECTURE_RECON_AND_PLAN.md lines 56-90

-- page_sections table
CREATE TABLE page_sections (
    id BLOB PRIMARY KEY, -- deterministic UUID derived from page_id + ordinal + heading_path
    page_id BLOB NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    workspace_id BLOB NOT NULL,
    project_id BLOB NOT NULL,
    ordinal INTEGER NOT NULL,
    level INTEGER NOT NULL,
    heading TEXT NOT NULL,
    heading_path TEXT NOT NULL, -- JSON array or canonical separator encoding, never reconstructed from titles
    body TEXT NOT NULL, -- section source text excluding frontmatter
    start_byte INTEGER NOT NULL, -- UTF-8 source offsets into the stored page body
    end_byte INTEGER NOT NULL,
    content_sha256 BLOB NOT NULL,
    UNIQUE (page_id, ordinal)
);

-- page_passages table
CREATE TABLE page_passages (
    id BLOB PRIMARY KEY, -- deterministic UUID derived from section id, ordinal, and content hash
    section_id BLOB NOT NULL REFERENCES page_sections(id) ON DELETE CASCADE,
    page_id BLOB NOT NULL,
    workspace_id BLOB NOT NULL,
    project_id BLOB NOT NULL,
    ordinal INTEGER NOT NULL,
    heading_path TEXT NOT NULL, -- denormalized from page_sections for FTS external content
    text TEXT NOT NULL,
    start_byte INTEGER NOT NULL,
    end_byte INTEGER NOT NULL,
    token_count INTEGER NOT NULL,
    content_sha256 BLOB NOT NULL,
    UNIQUE (section_id, ordinal)
);

-- Lexical and dense indexes
-- contentless/external-content FTS5 table page_passages_fts over heading_path and text
CREATE VIRTUAL TABLE page_passages_fts USING fts5(
    heading_path,
    text,
    content='page_passages',
    content_rowid='rowid'
);

-- Triggers to keep FTS in sync with page_passages
CREATE TRIGGER page_passages_fts_ai AFTER INSERT ON page_passages BEGIN
    INSERT INTO page_passages_fts(rowid, heading_path, text)
    VALUES (new.rowid, new.heading_path, new.text);
END;

CREATE TRIGGER page_passages_fts_ad AFTER DELETE ON page_passages BEGIN
    INSERT INTO page_passages_fts(page_passages_fts, rowid, heading_path, text)
    VALUES ('delete', old.rowid, old.heading_path, old.text);
END;

CREATE TRIGGER page_passages_fts_au AFTER UPDATE ON page_passages BEGIN
    INSERT INTO page_passages_fts(page_passages_fts, rowid, heading_path, text)
    VALUES ('delete', old.rowid, old.heading_path, old.text);
    INSERT INTO page_passages_fts(rowid, heading_path, text)
    VALUES (new.rowid, new.heading_path, new.text);
END;

-- page_passage_embeddings table
CREATE TABLE page_passage_embeddings (
    passage_id BLOB PRIMARY KEY REFERENCES page_passages(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    dim INTEGER NOT NULL,
    embedding BLOB NOT NULL -- stored as raw bytes
);

-- Indexes for scoped queries
CREATE INDEX idx_page_sections_ws_proj ON page_sections(workspace_id, project_id);
CREATE INDEX idx_page_passages_ws_proj ON page_passages(workspace_id, project_id);