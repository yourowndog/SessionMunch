//! Read-only connection pool and query helpers.
//!
//! WAL mode lets us have unlimited concurrent readers alongside the single
//! writer, so the pool is mostly about bounding file-descriptor usage and
//! avoiding `Connection::open` overhead on hot paths. Pool eviction is a
//! soft cap: a connection that comes back when the pool is already full
//! is simply dropped.

use std::cmp::Ordering;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jiff::Timestamp;
use parking_lot::Mutex;
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use sessionmunch_core::{
    AgentKind, AutoImproveProposalId, AutoImproveRunId, Handoff, HandoffContent, HandoffId,
    HandoffLifecycle, HandoffOrigin, HandoffScope, HandoffState, IdentityKey, ManagedRunId,
    Observation, ObservationId, ObservationKind, OwnerFilter, PageId, PagePath, ProjectId,
    SessionEvent, SessionEventHit, SessionEventSearch, SessionId, User, UserId, WorkspaceId,
    WorkstreamEvent, WorkstreamId,
};
use uuid::Uuid;
// `sessionmunch_core::Tier` is referenced via fully-qualified path inside the
// DecayCandidate struct definition above to avoid a top-level import
// for a single use-site.

use crate::auto_improve::{
    AutoImproveProposalDetail, AutoImproveProposalEvent, AutoImproveProposalStatus,
    AutoImproveProposalSummary, AutoImproveRejectionSummary, AutoImproveTelemetryAggregate,
    AutoImproveTelemetryCount, OwnedAutoImproveProposalDetail, bytes32, opt_bytes32,
    summary_from_row, to_sql_err,
};
use crate::error::{StoreError, StoreResult};
use crate::fts_query::prepare_fts5_query;
use crate::maintenance::MaintenanceJob;
use crate::retrieval_tuning::{RetrievalTuning, is_session_recall_query};
use crate::users::TOKEN_HASH_LEN;
use crate::workstream::{ManagedRunContext, StoredManagedRunStatus, StoredWorkstreamSummary};

/// TTL guard for retrieval surfaces (search / recent / embedding hits /
/// graph neighbours / briefing lists): appended to a WHERE clause that
/// already constrains `is_latest = 1`. `table` is the pages alias in
/// that query; `now_param` is the positional placeholder bound to the
/// current wall-clock in microseconds. One definition so the
/// NULL-handling never drifts between queries. Exact-path/id reads and
/// the decay-candidate walk deliberately do NOT use this — reads
/// annotate expiry instead of hiding it, and the sweep must see
/// expired rows to delete them.
fn not_expired(table: &str, now_param: &str) -> String {
    format!(" AND ({table}.expires_at IS NULL OR {table}.expires_at > {now_param})")
}

/// Current wall-clock in microseconds, for binding against
/// [`not_expired`] fragments.
fn now_us() -> i64 {
    Timestamp::now().as_microsecond()
}

fn page_kind_expr(path_column: &str, frontmatter_column: &str) -> String {
    format!(
        "COALESCE( \
            json_extract({frontmatter_column}, '$.kind'), \
            CASE \
                WHEN {path_column} LIKE '\\_rules/%' ESCAPE '\\' THEN 'rule' \
                WHEN {path_column} LIKE '\\_slots/%' ESCAPE '\\' THEN 'slot' \
                WHEN {path_column} LIKE 'sessions/%' THEN 'session' \
                WHEN {path_column} LIKE 'decisions/%' THEN 'decision' \
                WHEN {path_column} LIKE 'gotchas/%' THEN 'gotcha' \
                WHEN {path_column} LIKE 'concepts/%' THEN 'concept' \
                WHEN {path_column} LIKE 'procedures/%' THEN 'procedure' \
                WHEN {path_column} LIKE 'notes/%' THEN 'note' \
                ELSE 'fact' \
            END \
        )"
    )
}

/// Leading body characters scanned when a page carries no frontmatter
/// `summary`. Wide enough to step past the `# Title` line and a structural
/// heading or two before the first real prose.
const DESCRIPTOR_SCAN_CHARS: usize = 600;

/// Maximum length of a synthesised page descriptor, in characters.
const DESCRIPTOR_MAX_CHARS: usize = 240;

/// SQL expression yielding the raw material for [`page_descriptor`]: the
/// page's own frontmatter `summary` when it has a non-blank one, otherwise a
/// prefix of the body. `NULLIF(TRIM(...), '')` keeps an empty summary from
/// winning the `COALESCE` over real body text.
fn page_descriptor_expr(body_column: &str, frontmatter_column: &str) -> String {
    format!(
        "COALESCE( \
            NULLIF(TRIM(json_extract({frontmatter_column}, '$.summary')), ''), \
            substr({body_column}, 1, {DESCRIPTOR_SCAN_CHARS}) \
        )"
    )
}

/// Strip a leading markdown list marker (`- `, `* `, `+ `, `1. `) so a kept
/// line reads as prose in the descriptor.
fn strip_list_marker(line: &str) -> &str {
    if let Some(rest) = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))
    {
        return rest.trim_start();
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && line[digits..].starts_with(". ") {
        return line[digits + 2..].trim_start();
    }
    line
}

/// `true` for the `- **key:** value` bullets that open a compiled session
/// page's metadata block — session id, timestamps, observation count. They
/// are addressing, not content.
fn is_metadata_bullet(line: &str) -> bool {
    line.starts_with("- **") && line.contains(":**")
}

/// Page-intrinsic descriptor for a retrieval hit.
///
/// The FTS path centres its excerpt on the matched terms through
/// `snippet(pages_fts, ...)`. The vector, entity-match, graph-neighbour and
/// recency paths have no matched term to centre on. They previously returned
/// `substr(body, 1, 240)`, which on a compiled page is the `# Title` line plus
/// the `## Session metadata` block under it — the title the hit already
/// carries, followed by a session id and three timestamps.
///
/// Fills the [`DESCRIPTOR_MAX_CHARS`] budget from the page's own text,
/// skipping four things that carry no signal for the reader deciding whether
/// to open the page: structural lines, metadata bullets, and any line that
/// merely repeats `title`. On a session page that lands on the prompts after
/// the first, which is the part `title` does not already show.
fn page_descriptor(raw: &str, title: &str) -> String {
    // `truncate_for_title` suffixes an ellipsis only when it shortened the
    // title, so an ellipsis-free title is complete: a longer line that merely
    // opens with it is a different sentence and must be kept.
    let title_trimmed = title.trim();
    let title_was_truncated = title_trimmed.ends_with('\u{2026}');
    let title_key = title_trimmed.trim_end_matches('\u{2026}').trim();
    let mut out = String::new();
    for line in raw.lines().map(str::trim) {
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with("---")
            || line.starts_with("___")
            || line.starts_with("***")
            || is_metadata_bullet(line)
        {
            continue;
        }
        let content = strip_list_marker(line).trim();
        if content.is_empty() {
            continue;
        }
        let repeats_title = !title_key.is_empty()
            && if title_was_truncated {
                content.starts_with(title_key)
            } else {
                content == title_key
            };
        if repeats_title {
            continue;
        }
        if !out.is_empty() {
            if out.chars().count() + 1 + content.chars().count() > DESCRIPTOR_MAX_CHARS {
                break;
            }
            out.push(' ');
        }
        out.push_str(content);
        if out.chars().count() >= DESCRIPTOR_MAX_CHARS {
            break;
        }
    }
    if out.is_empty() {
        return truncate_chars(raw.trim(), DESCRIPTOR_MAX_CHARS);
    }
    truncate_chars(&out, DESCRIPTOR_MAX_CHARS)
}

/// Truncate on a character boundary, marking elision with an ellipsis.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

const AUTHORITY_CANDIDATE_MULTIPLIER: usize = 4;
const AUTHORITY_MIN_CANDIDATES: usize = 20;
const AUTHORITY_MAX_EXTRA_CANDIDATES: usize = 300;

#[derive(Debug, Clone, Copy)]
struct PageAuthority {
    factor: f64,
    /// Kind/tier penalty this page carries *because* it is a session page
    /// (`0.0` for every other kind). A `session_recall` query gives it back.
    session_penalty: f64,
}

impl PageAuthority {
    fn from_stored(
        path: &str,
        kind: &str,
        tier: &str,
        pinned: bool,
        frontmatter_json: &str,
    ) -> Self {
        // Relevance remains primary: even the strongest maintained page gets
        // at most a 1.5x boost, while explicit low-authority metadata bottoms
        // out at 0.55x rather than excluding the page.
        let mut factor = 1.0_f64;

        let kind_adjust = match kind {
            "rule" | "decision" => 0.15,
            "procedure" | "gotcha" => 0.12,
            "concept" | "slot" => 0.07,
            "session" => -0.15,
            _ => 0.0,
        };
        let tier_adjust = match tier {
            "semantic" | "procedural" => 0.10,
            "episodic" => -0.08,
            "working" => -0.03,
            _ => 0.0,
        };
        factor += kind_adjust + tier_adjust;
        let session_penalty = if kind == "session" {
            -(kind_adjust + tier_adjust.min(0.0))
        } else {
            0.0
        };
        if pinned {
            factor += 0.08;
        }

        if path.starts_with("_lint/") {
            factor -= 0.20;
        } else if path.starts_with("investigations/") {
            factor -= 0.05;
        }

        let tags = authority_tags(frontmatter_json);
        if tags
            .iter()
            .any(|tag| tag == "canonical" || tag == "source-of-truth")
        {
            factor += 0.20;
        }
        if tags.iter().any(|tag| tag == "active") {
            factor += 0.05;
        }

        // Negative tags are explicit curator instructions. They cap the
        // result's authority instead of merely cancelling a canonical path
        // boost, but they never remove the page from search.
        if tags
            .iter()
            .any(|tag| tag == "do-not-answer-from" || tag == "test-fixture")
        {
            factor = factor.min(0.55);
        } else if tags.iter().any(|tag| tag == "superseded") {
            factor = factor.min(0.65);
        } else if tags.iter().any(|tag| tag == "historical") {
            factor = factor.min(0.80);
        }

        Self {
            factor: factor.clamp(0.55, 1.50),
            session_penalty,
        }
    }

    fn adjust_rank(&self, rank: f64) -> f64 {
        apply_rank_multiplier(rank, self.factor)
    }

    /// Effective multiplier when the query routed to session-recall
    /// retrieval: session pages get the kind/tier penalty they carry by
    /// default handed back plus `bonus`, still inside the bounds every
    /// other page lives in. Any other query leaves the factor alone.
    fn factor_for(&self, session_recall: bool, bonus: f64) -> f64 {
        if session_recall && self.session_penalty > 0.0 {
            (self.factor + self.session_penalty + bonus).clamp(0.55, 1.50)
        } else {
            self.factor
        }
    }
}

/// Scale a rank by a `> 0` multiplier so a larger multiplier always means a
/// better (lower) rank, whichever sign convention the rank arrived in: fused
/// RRF ranks are negated scores (`<= 0`), raw FTS5 ranks are positive.
fn apply_rank_multiplier(rank: f64, multiplier: f64) -> f64 {
    if rank <= 0.0 {
        rank * multiplier
    } else {
        rank / multiplier
    }
}

/// The two embedding tables share one schema; scans differ only by name.
#[derive(Debug, Clone, Copy)]
enum EmbeddingTable {
    /// `page_embeddings`: one vector per page over the full body.
    Body,
    /// `page_abstract_embeddings`: one vector per page over its L0
    /// frontmatter `abstract:` line (opt-in stream).
    Abstract,
}

impl EmbeddingTable {
    fn name(self) -> &'static str {
        match self {
            Self::Body => "page_embeddings",
            Self::Abstract => "page_abstract_embeddings",
        }
    }
}

fn authority_tags(frontmatter_json: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(frontmatter_json)
        .ok()
        .and_then(|frontmatter| frontmatter.get("tags").cloned())
        .and_then(|tags| tags.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|tag| tag.as_str().map(normalize_authority_tag))
        .collect()
}

fn normalize_authority_tag(tag: &str) -> String {
    tag.trim().to_ascii_lowercase().replace('_', "-")
}

fn authority_candidate_limit(limit: usize) -> usize {
    if limit == 0 {
        return 0;
    }
    limit
        .saturating_mul(AUTHORITY_CANDIDATE_MULTIPLIER)
        .max(AUTHORITY_MIN_CANDIDATES)
        .min(limit.saturating_add(AUTHORITY_MAX_EXTRA_CANDIDATES))
}

fn rerank_page_hits(candidates: Vec<(PageHit, PageAuthority)>, limit: usize) -> Vec<PageHit> {
    let mut hits: Vec<PageHit> = candidates
        .into_iter()
        .map(|(mut hit, authority)| {
            hit.rank = authority.adjust_rank(hit.rank);
            hit
        })
        .collect();
    hits.sort_by(|a, b| {
        a.rank
            .partial_cmp(&b.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit);
    hits
}

fn rerank_page_hits_with_meta(
    candidates: Vec<(PageHitWithMeta, PageAuthority)>,
    limit: usize,
) -> Vec<PageHitWithMeta> {
    let mut hits: Vec<PageHitWithMeta> = candidates
        .into_iter()
        .map(|(mut hit, authority)| {
            hit.rank = authority.adjust_rank(hit.rank);
            hit
        })
        .collect();
    hits.sort_by(|a, b| {
        a.rank
            .partial_cmp(&b.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit);
    hits
}

/// Rows keyed to one session (see [`ReaderPool::session_dependent_rows`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionDependentRows {
    /// `observations` rows of the session, in any scope.
    pub observations: u64,
    /// `handoffs` rows the session produced, in any scope.
    pub handoffs: u64,
    /// `session_consolidation_jobs` rows, in any scope.
    pub consolidation_jobs: u64,
    /// `auto_improve_runs` rows, in any scope.
    pub auto_improve_runs: u64,
    /// `auto_improve_scheduler_claims` rows, in any scope.
    pub auto_improve_claims: u64,
    /// Versions of `sessions/<id>.md` in the queried page scope.
    pub page_versions: u64,
    /// Latest versions of `sessions/<id>.md` in the queried page scope
    /// (0 or 1).
    pub page_latest: u64,
}

/// One page flagged by `stale`/`wrong` feedback on its current version,
/// aggregated per (page, kind) for the lint pass.
#[derive(Debug, Clone, Serialize)]
pub struct FeedbackFinding {
    /// Wiki path of the flagged page.
    pub path: String,
    /// `stale` or `wrong`.
    pub kind: String,
    /// How many signals of this kind the current version has.
    pub signal_count: u32,
    /// ISO-8601 timestamp of the most recent signal.
    pub latest_at: String,
    /// Most recent caller-supplied reason for this kind, if any.
    pub reason: Option<String>,
}

/// A page matched by the entity stream, with the inverse-frequency
/// weight that ranked it and the entity names that matched.
#[derive(Debug, Clone)]
pub struct EntityHit {
    /// The matched page.
    pub hit: PageHit,
    /// Sum of `1 / pages_carrying_entity` over the matched entities.
    pub weight: f64,
    /// Entity names that matched the query.
    pub matched: Vec<String>,
}

/// Escape a literal for use inside a SQL `LIKE` pattern with
/// `ESCAPE '\'`. Entity tokens legitimately contain `_`, which `LIKE`
/// would otherwise treat as "any single character" — so `foo_bar` would
/// match `fooxbar`.
fn like_escape(literal: &str) -> String {
    literal
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Split a query into entity-match tokens: lowercase, alphanumeric-ish
/// runs, 3+ characters, de-duplicated, bounded. Short tokens are
/// dropped because a 1-2 character prefix match is noise, not a signal.
///
/// A hyphenated/underscored run yields BOTH the whole token and its
/// parts — the same both-forms trick [`crate::ops::path_search_text`]
/// uses on paths, and for the same reason: an entity may be written
/// `sessionmunch`, `sessionmunch`, or `ai memory`, and the query may pick
/// any of them.
fn entity_query_tokens(query: &str) -> Vec<String> {
    /// Beyond this the query is prose, not a set of nouns; the FTS
    /// stream already handles prose better than prefix matching does.
    const MAX_TOKENS: usize = 12;
    const MIN_TOKEN_CHARS: usize = 3;
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let mut push = |raw: &str| -> bool {
        let char_count = raw.chars().count();
        if !(MIN_TOKEN_CHARS..=sessionmunch_core::MAX_ENTITY_LEN).contains(&char_count) {
            return true;
        }
        let token = raw.to_lowercase();
        if seen.insert(token.clone()) {
            out.push(token);
        }
        out.len() < MAX_TOKENS
    };
    'outer: for raw in query.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_')) {
        let token = raw.trim_matches(['-', '_']);
        if token.is_empty() {
            continue;
        }
        let compound = token.contains(['-', '_']);
        if !push(token) {
            break;
        }
        if compound {
            for part in token.split(['-', '_']) {
                if !push(part) {
                    break 'outer;
                }
            }
        }
    }
    out
}

/// A graph-expansion neighbour with provenance (which seed and which
/// link direction produced it). Internal to hybrid search + explain.
struct GraphNeighbor {
    hit: PageHit,
    /// Index into the seed list handed to the expansion.
    seed_ord: usize,
    /// `true` when the neighbour links TO the seed (backlink).
    incoming: bool,
    /// The typed edge the neighbour was reached by (`causes` / `fixes` /
    /// `contradicts`), or `None` for a plain `references` link. Informational
    /// only — it is surfaced in `explain` and does not affect ranking.
    edge: Option<String>,
}

/// Which seed page pulled a hit in via graph expansion, and the link
/// direction. Part of [`SearchExplain`].
#[derive(Debug, Clone, Serialize)]
pub struct GraphVia {
    /// Path of the FTS/entity/vector seed page the link was followed from.
    pub seed_path: String,
    /// `outgoing` = seed links to the hit; `incoming` = hit links to
    /// the seed (backlink).
    pub direction: &'static str,
    /// The typed edge kind followed (`causes` / `fixes` / `contradicts`),
    /// omitted for a plain `references` link. Informational: it explains *why*
    /// a neighbour surfaced; it does not weight ranking (typed-edge weighting
    /// and `contradicts` capping are deferred behind the eval harness).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge: Option<String>,
}

/// Per-stream RRF contributions (`1/(k+rank)`, k=60) for one hit.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RrfContributions {
    /// FTS5 stream contribution (0.0 when the hit missed that stream).
    pub fts: f64,
    /// Vector stream contribution.
    pub vector: f64,
    /// Graph-neighbour stream contribution.
    pub graph: f64,
    /// Entity-match stream contribution.
    pub entity: f64,
    /// L0 abstract-embedding stream contribution (opt-in
    /// `[retrieval] abstract_vectors`; 0.0 when off or missed).
    #[serde(rename = "abstract", skip_serializing_if = "is_zero")]
    pub abstract_vector: f64,
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// Score transparency for one `memory_query` hit: where the hit ranked
/// in each retrieval stream, the raw per-stream scores, and how the
/// RRF fusion combined them. All ranks are 1-based; a `None` rank
/// means the hit did not surface in that stream.
///
/// [`Default`] is "surfaced in no stream": every rank `None`, zero
/// contributions. Streams fill in their own fields as they contribute.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SearchExplain {
    /// 1-based rank in the FTS5 stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fts_rank: Option<usize>,
    /// Raw FTS5 (bm25-based) score — lower is better.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fts_score: Option<f64>,
    /// 1-based rank in the vector stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_rank: Option<usize>,
    /// Cosine similarity against the query embedding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cosine: Option<f32>,
    /// 1-based rank in the L0 abstract-embedding stream (opt-in).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abstract_rank: Option<usize>,
    /// Cosine similarity of the page's abstract embedding against the query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abstract_cosine: Option<f32>,
    /// 1-based rank in the graph-neighbour stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_rank: Option<usize>,
    /// Which seed page and link direction pulled the hit in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_via: Option<GraphVia>,
    /// 1-based rank in the entity-match stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_rank: Option<usize>,
    /// Raw inverse-frequency weight used to order the entity stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_weight: Option<f64>,
    /// Entity names on this page that matched the query.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_entities: Vec<String>,
    /// Per-stream RRF contributions.
    pub rrf: RrfContributions,
    /// Total fused score (sum of the RRF contributions) — higher is
    /// better. The returned `rank` is its negation *after* the
    /// authority multiplier below, so `fused` alone does not reproduce
    /// the ordering.
    pub fused: f64,
    /// Bounded page-authority multiplier applied to the fused rank
    /// after RRF (canonical kind/tier/`pinned`/frontmatter tags).
    /// `None` when the page had no authority row to adjust with; `1.0`
    /// means it was considered and left alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authority: Option<f64>,
    /// The routing decision for this query when `[retrieval] query_intent`
    /// is on: `"session_recall"` for queries the lexical router recognised.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<&'static str>,
    /// Multiplier the routing applied to this hit: the session-page
    /// authority lift expressed as a ratio over the un-routed factor.
    /// `1.0` = considered and left alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent_boost: Option<f64>,
    /// Relevance in `[0, 1]` assigned by the optional post-RRF
    /// reranker. `None` when no reranker is configured, when it
    /// degraded, or when the hit fell outside the bounded judged prefix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank_score: Option<f32>,
    /// Number of `page_evidence` rows citing this page version (P2,
    /// docs/design-hindsight-borrowings.md §3) — how many
    /// sessions/observations/feedback/reconsolidation passes produced or
    /// reaffirmed it. Populated only on the explained path
    /// ([`Reader::hybrid_search_explained`]), batch-fetched after fusion;
    /// `None` on the default (non-explained) path. Inert this release: it
    /// never feeds `fused`/`authority` or changes ranking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_count: Option<u32>,
}

/// One hit returned by [`ReaderPool::search_pages`].
#[derive(Debug, Clone, Serialize)]
pub struct PageHit {
    /// Stable identifier for this page version.
    pub id: PageId,
    /// Relative path within the wiki tree.
    pub path: PagePath,
    /// Page title.
    pub title: String,
    /// FTS5 snippet of the body around the matched terms (HTML-marked).
    pub snippet: String,
    /// Relevance rank after the bounded authority adjustment (lower is better).
    pub rank: f64,
}

/// One hit returned by a passage-level retrieval query.
///
/// Each hit carries the passage's own text and byte offsets so a caller can
/// fetch the surrounding context without a second round-trip, plus the
/// per-stream ranks that produced it (`lexical_rank`, `dense_rank`) and the
/// fused RRF score. The explain fields are `Option` and only populated when
/// the hit actually surfaced in that stream — a hit that came from the
/// dense stream alone has `lexical_rank = None`.
#[derive(Debug, Clone, Serialize)]
pub struct PassageHit {
    /// Stable passage identifier (deterministic UUID derived from section id,
    /// ordinal, and content hash).
    pub passage_id: Uuid,
    /// Stable section identifier (deterministic UUID derived from page id,
    /// ordinal, and heading path).
    pub section_id: Uuid,
    /// Stable page identifier (UUID of the latest page row).
    pub page_id: PageId,
    /// Stable workspace identifier.
    pub workspace_id: WorkspaceId,
    /// Stable project identifier.
    pub project_id: ProjectId,
    /// Relative wiki path.
    pub page_path: PagePath,
    /// Page title.
    pub page_title: String,
    /// Heading path (denormalized from page_sections).
    pub heading_path: String,
    /// Passage text.
    pub text: String,
    /// UTF-8 byte offset where this passage starts in the page body.
    pub start_byte: i64,
    /// UTF-8 byte offset where this passage ends in the page body.
    pub end_byte: i64,
    /// Final fused rank (0-based position in the result set).
    pub rank: usize,
    /// 1-based rank in the lexical FTS5 stream. `None` when the hit came from
    /// the dense stream only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lexical_rank: Option<usize>,
    /// 1-based rank in the dense embedding stream. `None` when the hit came
    /// from the lexical stream only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dense_rank: Option<usize>,
    /// Reciprocal Rank Fusion score. Higher is better.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rrf_score: Option<f64>,
}

/// One raw passage candidate with its stream-specific scores, used during
/// RRF fusion before the final `PassageHit` is assembled.
#[derive(Debug, Clone)]
struct PassageCandidate {
    hit: PassageHit,
    lexical_score: Option<f64>,
    dense_score: Option<f64>,
}

/// Completed session selected for scheduled auto-improvement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoImproveCandidateSession {
    /// Session id.
    pub session_id: SessionId,
    /// Session `ended_at` timestamp in Unix microseconds.
    pub ended_at: i64,
}

/// Open session selected for manual finalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSession {
    /// Session id.
    pub session_id: SessionId,
    /// Captured session cwd, if available.
    pub cwd: Option<String>,
}

/// One session as listed from a scope by [`ReaderPool::sessions_for_scope`]
/// and [`ReaderPool::session_summary_scoped`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionSummary {
    /// Session id.
    pub session_id: SessionId,
    /// Captured session cwd, if available.
    pub cwd: Option<String>,
    /// `AgentKind` as stored (`claude-code`, `cursor`, ...).
    pub agent_kind: String,
    /// ISO-8601 session start timestamp.
    pub started_at: String,
    /// ISO-8601 session end timestamp; `None` while the session is open.
    pub ended_at: Option<String>,
    /// Observations of this session that landed in the queried scope. A
    /// session that crossed repositories mid-flight has more rows elsewhere.
    pub observation_count: u64,
    /// Operator the session belongs to, as stored on the `sessions` row.
    pub actor_user: Option<String>,
}

/// Aggregate MCP tool-call counts for one client, from
/// `client_activity` — the MCP-only complement to
/// [`AgentSessionCount`]: hook-less clients (VS Code Copilot, Claude
/// Desktop, scripts) never open sessions but still read and write
/// memory through tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientActivity {
    /// Sanitized client name (`clientInfo.name`, actor overlay, or
    /// `unknown`).
    pub client: String,
    /// Read-shaped tool calls (query/read/recent/briefing/…).
    pub reads: u64,
    /// Write-shaped tool calls (write_page/feedback/consolidate/…).
    pub writes: u64,
}

/// How many sessions one agent CLI opened in a scope — the shape behind
/// "where is this project's memory actually coming from".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentSessionCount {
    /// `AgentKind` as stored (`claude-code`, `cursor`, …).
    pub agent: String,
    /// Sessions this agent opened in the window, ended or still open.
    pub sessions: u64,
}

/// How a `SessionEnd` event should treat its target session — see
/// [`ReaderPool::session_end_disposition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEndDisposition {
    /// The session is open in the resolved scope/agent: run the normal end
    /// path.
    Open,
    /// The session is missing or does not match the resolved scope/agent. Drop
    /// the event without attempting recovery against an unrelated session.
    DropInvalid,
    /// The matching session already ended with no observations beyond the
    /// persisted end generation. Converge any interrupted downstream effects,
    /// then drop the duplicate end.
    AlreadyEnded,
    /// Already ended, but the observation count exceeds the persisted end
    /// generation: the agent resumed the session under the same id — run the
    /// full end path again so the resumed work reaches the compiled page
    /// (issue #152).
    ReEndWithNewWork,
}

/// The latest version of a page's stored content, used as a DB-backed
/// fallback when the on-disk markdown read fails (index/disk skew). The
/// markdown file is the source of truth, but the store keeps a faithful copy
/// written in the same transaction, so serving it is safe.
#[derive(Debug, Clone)]
pub struct StoredPageBody {
    /// Page title as stored in the DB.
    pub title: String,
    /// Page body (markdown without frontmatter).
    pub body: String,
    /// Raw `frontmatter_json` TEXT column (parse at the call site).
    pub frontmatter_json: String,
    /// Memory tier stored on the latest page row.
    pub tier: String,
    /// Whether the latest page row is pinned.
    pub pinned: bool,
}

/// Search hit with workspace/project names, used by the web UI to avoid
/// per-hit metadata lookups after a global search.
#[derive(Debug, Clone, Serialize)]
pub struct PageHitWithMeta {
    /// Name of the workspace containing the page.
    pub workspace_name: String,
    /// Name of the project containing the page.
    pub project_name: String,
    /// Relative path within the wiki tree.
    pub path: PagePath,
    /// Page title.
    pub title: String,
    /// FTS5 snippet of the body around the matched terms (HTML-marked).
    pub snippet: String,
    /// Relevance rank after the bounded authority adjustment (lower is better).
    pub rank: f64,
}

/// One raw observation fallback hit returned when compiled wiki pages miss.
#[derive(Debug, Clone, Serialize)]
pub struct ObservationHit {
    /// Stable observation identifier.
    pub id: ObservationId,
    /// Owning session identifier.
    pub session_id: SessionId,
    /// Observation kind as stored on the lifecycle row.
    pub kind: String,
    /// Observation title.
    pub title: String,
    /// FTS5 snippet of the raw observation body around the matched terms.
    pub snippet: String,
    /// FTS5 rank score (lower is better).
    pub rank: f64,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
}

/// One raw observation with its full stored body, as returned by
/// [`ReaderPool::session_observations_scoped`]. Bodies were sanitized and
/// bounded on the way in, so this is a read of what the store holds; callers
/// that need a smaller payload cap the body themselves.
#[derive(Debug, Clone, Serialize)]
pub struct ObservationRecord {
    /// Stable observation identifier.
    pub id: ObservationId,
    /// Owning session identifier.
    pub session_id: SessionId,
    /// Observation kind as stored on the lifecycle row.
    pub kind: String,
    /// Observation title.
    pub title: String,
    /// Full sanitized observation body.
    pub body: String,
    /// Importance in `1..=10`.
    pub importance: u8,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
    /// Third-party extension namespace that supplied `source_event`, if any.
    pub extension: Option<String>,
    /// Source event name from an opt-in extension vocabulary, if any.
    pub source_event: Option<String>,
}

/// Sort direction for [`ObservationPage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObservationOrder {
    /// Oldest first (capture order).
    #[default]
    Asc,
    /// Newest first.
    Desc,
}

/// Paging and filtering for [`ReaderPool::session_observations_scoped`].
/// The store applies `limit` and `offset` as given; callers clamp.
#[derive(Debug, Clone)]
pub struct ObservationPage {
    /// Maximum rows to return. Zero returns no rows but still counts.
    pub limit: usize,
    /// Rows to skip before the first returned one.
    pub offset: usize,
    /// Sort direction over `created_at` (with the id as tiebreak).
    pub order: ObservationOrder,
    /// Keep only these kinds. `None` or an empty list keeps every kind.
    pub kinds: Option<Vec<ObservationKind>>,
    /// Optional full-text query over title and body. Ignored when it
    /// normalizes to nothing searchable.
    pub query: Option<String>,
}

/// One page of a session's observations plus the counts a caller needs to
/// paginate and to notice that the session has rows outside the scope.
#[derive(Debug, Clone, Serialize)]
pub struct ObservationPageResult {
    /// The requested page, in the requested order.
    pub records: Vec<ObservationRecord>,
    /// Rows in scope matching the kind and query filters, across all pages.
    pub total: u64,
    /// Rows of the same session that landed in a different
    /// `(workspace, project)`, unfiltered. Non-zero means the session crossed
    /// repositories and this scope holds only part of it.
    pub elided_other_scope: u64,
}

/// Aggregate counts surfaced by [`ReaderPool::status_counts`] and consumed
/// by `sessionmunch status`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StatusCounts {
    /// Pages with `is_latest = 1`.
    pub pages_latest: u64,
    /// All page versions including superseded ones.
    pub pages_all: u64,
    /// Total sessions ever recorded.
    pub sessions: u64,
    /// Total observations across all sessions.
    pub observations: u64,
}

/// One likely cross-project contamination finding from
/// [`ReaderPool::audit_contamination`]. Advisory only — it flags STRUCTURAL
/// mislandings (an entity whose identity disagrees with the bucket it landed
/// in). Purely semantic contamination (a page whose topic belongs elsewhere
/// with no cwd/session anomaly) is not detectable structurally.
#[derive(Debug, Clone, Serialize)]
pub struct ContaminationFinding {
    /// Heuristic that fired: `session_wrong_bucket` (CHECK A).
    pub check: &'static str,
    /// Confidence — `high` for the structural check.
    pub confidence: &'static str,
    /// Entity kind: `session`.
    pub entity_kind: &'static str,
    /// Entity id (lowercase hex of the 16-byte UUID).
    pub entity_id: String,
    /// Workspace name the entity actually landed in.
    pub landed_workspace: String,
    /// Project name the entity actually landed in.
    pub landed_project: String,
    /// Project the evidence says it belongs to (the session's cwd
    /// prefix-resolution result for CHECK A).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_project: Option<String>,
    /// Originating session cwd — the prefix-resolution evidence (CHECK A).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Per-check counts for an [`ReaderPool::audit_contamination`] run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ContaminationSummary {
    /// Sessions whose cwd prefix-resolves to a different project (CHECK A).
    pub sessions_misbucketed: usize,
}

/// Result of [`ReaderPool::audit_contamination`] — advisory, never mutates.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ContaminationReport {
    /// Per-check counts.
    pub summary: ContaminationSummary,
    /// Individual findings.
    pub findings: Vec<ContaminationFinding>,
}

/// One `audit_log` row with names resolved through LEFT JOINs.
///
/// Workspace, project, page, and author are `Option` because the log has
/// no foreign keys (V05: append-only; orphan rows are expected).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditEvent {
    /// Auto-increment primary key; also the keyset pagination cursor.
    pub id: i64,
    /// Event time in microseconds since Unix epoch (V01 convention).
    pub at: i64,
    /// Writer-assigned op (`create_page`, `supersede_page`, `purge_project`, …).
    pub op: String,
    /// Workspace name, or `None` when the id no longer resolves.
    pub workspace: Option<String>,
    /// Project name, or `None` when the id no longer resolves.
    pub project: Option<String>,
    /// Page path, or `None` when the id no longer resolves.
    pub page_path: Option<String>,
    /// Username, or `None` for anonymous writes and deleted users.
    pub author_username: Option<String>,
    /// Schema column. The only writer stores the literal `{}`; not a payload.
    pub detail: String,
}

/// Filters for [`ReaderPool::list_audit_events`].
#[derive(Debug, Clone)]
pub struct AuditLogFilter {
    /// Restrict to this workspace **name**.
    pub workspace: Option<String>,
    /// Restrict to this project **name**.
    pub project: Option<String>,
    /// Restrict to this op string.
    pub op: Option<String>,
    /// Keyset cursor: return rows with `id` strictly less than this value.
    pub before_id: Option<i64>,
    /// Page size. Clamped to `1..=200`; [`Default`] is 50.
    pub limit: usize,
}

impl Default for AuditLogFilter {
    fn default() -> Self {
        Self {
            workspace: None,
            project: None,
            op: None,
            before_id: None,
            limit: 50,
        }
    }
}

/// Counts that must all be zero before `sessionmunch reindex` rebuilds the
/// derived SQLite store from wiki files.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReindexTargetStatus {
    /// Workspace rows already present in SQLite.
    pub workspaces: u64,
    /// Project rows already present in SQLite.
    pub projects: u64,
    /// Page rows, including superseded versions.
    pub pages: u64,
    /// Link rows derived from latest page bodies.
    pub links: u64,
    /// Stored embedding rows derived from latest pages.
    pub page_embeddings: u64,
    /// Session rows. These are DB-only episodic state and are not rebuilt.
    pub sessions: u64,
    /// Observation rows. These are DB-only episodic state and are not rebuilt.
    pub observations: u64,
    /// Handoff rows. These are DB-only episodic state and are not rebuilt.
    pub handoffs: u64,
    /// User rows and token hashes. These are DB-only state and are not rebuilt.
    pub users: u64,
    /// Audit rows. These are DB-only state and are not rebuilt.
    pub audit_log: u64,
}

impl ReindexTargetStatus {
    /// True when the store has no user data or derived rows.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.workspaces == 0
            && self.projects == 0
            && self.pages == 0
            && self.links == 0
            && self.page_embeddings == 0
            && self.sessions == 0
            && self.observations == 0
            && self.handoffs == 0
            && self.users == 0
            && self.audit_log == 0
    }

    /// Render a compact list of non-zero counters for operator errors.
    #[must_use]
    pub fn nonzero_summary(&self) -> String {
        let mut parts = Vec::new();
        macro_rules! push_nonzero {
            ($field:ident) => {
                if self.$field != 0 {
                    parts.push(format!("{}={}", stringify!($field), self.$field));
                }
            };
        }
        push_nonzero!(workspaces);
        push_nonzero!(projects);
        push_nonzero!(pages);
        push_nonzero!(links);
        push_nonzero!(page_embeddings);
        push_nonzero!(sessions);
        push_nonzero!(observations);
        push_nonzero!(handoffs);
        push_nonzero!(users);
        push_nonzero!(audit_log);
        parts.join(", ")
    }
}

/// Derived-index health counters surfaced by admin status.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DerivedIndexStatus {
    /// All page rows in the source table. `pages_fts_rows` should match this.
    pub pages_rows: u64,
    /// Rows currently present in the page FTS5 index.
    pub pages_fts_rows: u64,
    /// All observation rows. `observations_fts_rows` should match this.
    pub observations_rows: u64,
    /// Rows currently present in the observation FTS5 index.
    pub observations_fts_rows: u64,
    /// Latest pages without any embedding row whose body is non-empty —
    /// i.e. the pages a backfill can actually act on.
    pub latest_pages_missing_embeddings: u64,
    /// Latest pages without an embedding whose body is empty; no
    /// embedder can ever cover these (the backfill skips them by rule).
    pub latest_pages_unembeddable: u64,
    /// Latest pages whose last embed attempt failed or was skipped and which
    /// still have no embedding. These are the pages an operator can act on.
    pub embed_failures_unresolved: u64,
    /// Latest pages that recorded a failed or skipped embed at some point but
    /// have an embedding now. Kept because a global `embed --force` used to
    /// erase exactly this history, leaving a recurrence unattributable (#528).
    pub embed_failures_recovered: u64,
    /// Stored embedding rows, regardless of provider/model/dim.
    pub embedding_rows: u64,
    /// Stored embedding triples and row counts.
    pub embedding_triples: Vec<EmbeddingTripleCount>,
    /// Typed relation edges (`link_type != 'references'`) from latest
    /// pages, as `(relation, count)` — the 2.0 typed-edge surface.
    pub typed_links_from_latest_pages: Vec<(String, u64)>,
    /// Outgoing links whose source page is latest.
    pub links_from_latest_pages: u64,
    /// Latest-page outgoing links whose target path has not resolved yet.
    pub unresolved_links_from_latest_pages: u64,
    /// Latest-page outgoing links pointing at a non-latest target row.
    pub stale_links_from_latest_pages: u64,
}

/// Physical storage figures, so an operator can decide whether a `VACUUM` is
/// worth its exclusive lock instead of scheduling one blindly.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StorageStatus {
    /// SQLite page size in bytes.
    pub page_size: u64,
    /// Total pages in the database.
    pub page_count: u64,
    /// Pages on the freelist — allocated, holding no live data, reusable by
    /// SQLite without growing the file.
    pub freelist_count: u64,
    /// `page_count * page_size`. The database's own view of its size, which is
    /// what `VACUUM` acts on.
    pub database_bytes: u64,
    /// `freelist_count * page_size` — an *estimate* of what a `VACUUM` would
    /// return to the filesystem.
    ///
    /// An estimate in both directions: `VACUUM` also defragments, so it can
    /// release more than the freelist, and it rewrites page headers, so it can
    /// release slightly less. Treat it as the signal for "is this worth an
    /// exclusive lock", not as an exact figure.
    pub reclaimable_bytes: u64,
    /// Free space on the filesystem holding the database file, in bytes.
    /// `None` when it could not be read (e.g. an unsupported filesystem).
    ///
    /// The database's own size says nothing about how much headroom is left
    /// for it to keep growing — a store that is small can still be minutes
    /// away from a WAL that cannot extend because the *disk*, not the
    /// database, is full. This is that signal, reported alongside the
    /// database's own figures rather than gated on them.
    pub data_dir_free_bytes: Option<u64>,
}

impl StorageStatus {
    /// Reclaimable share of the file, 0.0–100.0. Zero when the database is
    /// empty rather than a division by zero.
    #[must_use]
    pub fn reclaimable_pct(&self) -> f64 {
        if self.database_bytes == 0 {
            return 0.0;
        }
        (self.reclaimable_bytes as f64 / self.database_bytes as f64) * 100.0
    }
}

/// Count of embedding rows sharing one `(provider, model, dim)` triple.
#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingTripleCount {
    /// Embedding provider name.
    pub provider: String,
    /// Embedding model name.
    pub model: String,
    /// Vector dimension.
    pub dim: u32,
    /// Rows using this triple.
    pub count: u64,
}

/// Rolling activity counters over a fixed time window. Surfaced by
/// [`ReaderPool::briefing`] so the caller (or an LLM-driven `memory_explore`)
/// can calibrate verbosity against how busy the project's been.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ActivityWindow {
    /// Window size in days (e.g. 7 or 30).
    pub days: u32,
    /// Sessions whose `created_at` falls in the window.
    pub sessions: u64,
    /// Observations whose `created_at` falls in the window.
    pub observations: u64,
    /// Pages whose `updated_at` falls in the window — counts only
    /// `is_latest = 1`. Supersession of an old version into a new one
    /// counts as one update (the new row).
    pub pages_updated: u64,
}

/// Snapshot used by `memory_briefing` and the LLM-driven
/// `memory_explore`. Pure SQL aggregation; no LLM, no schema reads
/// outside the existing `pages` / `sessions` / `observations` /
/// `handoffs` tables.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BriefingSnapshot {
    /// Lifetime totals — same shape `memory_status` returns today.
    pub counts: StatusCounts,
    /// Activity over the last 7 days.
    pub activity_7d: ActivityWindow,
    /// Activity over the last 30 days.
    pub activity_30d: ActivityWindow,
    /// Timestamp of the most recent observation (ISO-8601), or `null`
    /// if no observations exist. The `now - last_observation_at` gap
    /// is the signal `memory_explore` uses to scale its verbosity.
    pub last_observation_at: Option<String>,
    /// Number of open (un-accepted) handoffs.
    pub pending_handoff_count: u64,
    /// All pages currently under `_rules/` — small, surfaced verbatim
    /// because they're the highest-signal type of memory.
    pub rules: Vec<BriefingPage>,
    /// Small pinned pages under `_slots/` for active project context,
    /// preferences, current focus, and pending items.
    pub slots: Vec<BriefingPage>,
    /// Top-N most-recently-updated `is_latest = 1` pages.
    pub recent_pages: Vec<BriefingPage>,
    /// Distinct other projects whose pages link INTO this project (who
    /// depends on us). Project-scoped briefings only; `0` for
    /// workspace/global snapshots.
    pub cross_project_dependents: u64,
    /// Distinct other projects this project's pages link OUT to (what we
    /// depend on). Project-scoped briefings only; `0` otherwise.
    pub cross_project_dependencies: u64,
    /// The project's highest-standing `rule`/`decision` pages, ordered by
    /// evidence count then recency. Populated only when the caller opts
    /// in via `settled_first: true` on `memory_briefing` /
    /// `briefing_for_project(_with_slot_visibility)` (P4,
    /// docs/design-hindsight-borrowings.md §5); empty and omitted from
    /// JSON otherwise so the default briefing shape is unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settled: Vec<SettledPage>,
}

/// Trimmed page view for the briefing — path, title, kind, updated_at
/// timestamp. Body and snippets are intentionally omitted (the caller
/// can follow up with `memory_query` if they need detail).
#[derive(Debug, Clone, Serialize)]
pub struct BriefingPage {
    /// Relative wiki path.
    pub path: String,
    /// Page title (first H1 / frontmatter title).
    pub title: String,
    /// Semantic classification, derived from explicit frontmatter or the
    /// page's canonical path family.
    pub kind: String,
    /// ISO-8601 timestamp of the last update.
    pub updated_at: String,
}

/// One of a project's highest-standing `rule`/`decision` pages, surfaced
/// by [`ReaderPool::briefing_for_project_with_slot_visibility`] only when
/// the caller opts in via `settled_first: true` (P4,
/// docs/design-hindsight-borrowings.md §5) — a bounded list an agent can
/// boot from instead of re-deriving settled answers from scratch.
#[derive(Debug, Clone, Serialize)]
pub struct SettledPage {
    /// Relative wiki path.
    pub path: String,
    /// Page title (first H1 / frontmatter title).
    pub title: String,
    /// Semantic classification — always `rule` or `decision` here.
    pub kind: String,
    /// Number of `page_evidence` rows citing this page version (0 when
    /// none have accrued yet).
    pub evidence_count: u32,
}

/// One core page of the session-start project brief — body included,
/// because the brief is injected as agent context and the whole point is
/// sparing the agent a re-exploration round-trip. Returned by
/// [`ReaderPool::session_brief_pages`].
#[derive(Debug, Clone, Serialize)]
pub struct BriefPageBody {
    /// Relative wiki path.
    pub path: String,
    /// Page title (first H1 / frontmatter title).
    pub title: String,
    /// Full markdown body of the latest version. The hooks-side renderer
    /// applies the char budget; the store returns it whole.
    pub body: String,
    /// Whether the page is pinned (decay-immune, operator-curated).
    pub pinned: bool,
    /// ISO-8601 timestamp of the last update.
    pub updated_at: String,
}

/// One row per (workspace, project) with aggregate stats.
/// Returned by [`ReaderPool::list_projects_with_stats`].
#[derive(Debug, Clone, Serialize)]
pub struct ProjectSummary {
    /// Name of the workspace.
    pub workspace_name: String,
    /// Name of the project within the workspace.
    pub project_name: String,
    /// Number of `is_latest = 1` pages.
    pub page_count: u64,
    /// ISO-8601 timestamp of the newest `updated_at`, or `None` when
    /// the project has no pages yet.
    pub last_updated: Option<String>,
}

/// One workspace scope with the id + name needed to write its
/// self-describing `_meta.md` manifest. Returned by
/// [`ReaderPool::list_all_workspace_scopes`].
#[derive(Debug, Clone)]
pub struct WorkspaceScopeRow {
    /// Workspace id — matches the level-1 wiki directory name.
    pub workspace_id: WorkspaceId,
    /// Human-readable workspace name.
    pub workspace_name: String,
}

/// One `(workspace, project)` scope with the ids + repo_path needed to write
/// its self-describing `_meta.md` manifest. Returned by
/// [`ReaderPool::list_all_scopes`]; consumed by `Wiki::backfill_scope_manifests`.
#[derive(Debug, Clone)]
pub struct ScopeRow {
    /// Workspace id — matches the level-1 wiki directory name.
    pub workspace_id: WorkspaceId,
    /// Human-readable workspace name.
    pub workspace_name: String,
    /// Project id — matches the level-2 wiki directory name.
    pub project_id: ProjectId,
    /// Human-readable project name.
    pub project_name: String,
    /// Filesystem path the project's cwd-based routing resolves to, if any.
    pub repo_path: Option<String>,
}

/// One row per workspace with aggregate stats.
/// Returned by [`ReaderPool::list_workspaces_with_stats`].
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceSummary {
    /// Name of the workspace.
    pub workspace_name: String,
    /// Number of projects in this workspace.
    pub project_count: u64,
    /// Number of `is_latest = 1` pages across the workspace.
    pub page_count: u64,
    /// ISO-8601 timestamp of the newest `updated_at`, or `None` when
    /// the workspace has no pages yet.
    pub last_updated: Option<String>,
}

/// Page summary for tree-view rendering (no body).
/// Returned by [`ReaderPool::list_pages`].
#[derive(Debug, Clone, Serialize)]
pub struct PageSummary {
    /// Relative path within the wiki tree.
    pub path: String,
    /// Page title.
    pub title: String,
    /// Semantic kind: `fact` | `rule` | `decision` | `gotcha` | …
    pub kind: String,
    /// Memory tier: `working` | `episodic` | `semantic` | `procedural`.
    pub tier: String,
    /// ISO-8601 timestamp of last update.
    pub updated_at: String,
}

/// Page author surfaced alongside read responses (P1.7).
/// JOINed from the `users` table when `pages.author_id IS NOT NULL`;
/// `None` for anonymous + root writes (where attribution lives only in
/// the on-disk frontmatter `last_modified_by` block from P1.6).
///
/// Repeated here rather than reused from `sessionmunch_core::User` because
/// the response shape intentionally omits internal fields (id,
/// created_at, last_seen_at, role) — only the human-facing
/// identity is part of the API contract.
#[derive(Debug, Clone, Serialize)]
pub struct PageAuthor {
    /// Stable username (the attribution key recorded on writes).
    pub username: String,
    /// Optional display name (`Alice Smith`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional email surfaced alongside the username in UIs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// Full page metadata for the page-view template.
/// Returned by [`ReaderPool::page_meta`].
#[derive(Debug, Clone, Serialize)]
pub struct PageMeta {
    /// Name of the workspace.
    pub workspace_name: String,
    /// Name of the project.
    pub project_name: String,
    /// UUID of the workspace — used to construct the per-project wiki path.
    pub workspace_id: WorkspaceId,
    /// UUID of the project — used to construct the per-project wiki path.
    pub project_id: ProjectId,
    /// Relative wiki path.
    pub path: String,
    /// Page title.
    pub title: String,
    /// Semantic kind.
    pub kind: String,
    /// Memory tier.
    pub tier: String,
    /// Whether the page is pinned (decay-immune).
    pub pinned: bool,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
    /// ISO-8601 last-update timestamp.
    pub updated_at: String,
    /// Path of the page this one supersedes, if any.
    pub supersedes: Option<String>,
    /// Multi-user attribution (P1.7). `None` for pre-multi-user pages
    /// and for root / anonymous writes (where `pages.author_id IS
    /// NULL`); `Some` when JOIN resolves a `users` row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<PageAuthor>,
    /// ISO-8601 TTL instant from the frontmatter `expires_at:` key.
    /// Exact-path/id reads return expired pages (an explicit read is
    /// not search); callers annotate expiry via [`PageMeta::expired`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// `true` when [`PageMeta::expires_at`] is in the past — the page
    /// is hidden from retrieval and awaiting the next forget sweep.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub expired: bool,
}

/// One resolved cross-project edge (a link whose endpoints live in
/// different projects). The `/api/v1/graph` endpoint returns these; the UI
/// builds nodes from the endpoints and can aggregate to a project graph.
#[derive(Debug, Clone, Serialize)]
pub struct CrossProjectEdge {
    /// Source page workspace.
    pub from_workspace: String,
    /// Source page project.
    pub from_project: String,
    /// Source page path.
    pub from_path: String,
    /// Target page workspace.
    pub to_workspace: String,
    /// Target page project.
    pub to_project: String,
    /// Target page path.
    pub to_path: String,
}

/// One typed `contradicts` edge between two latest pages, surfaced as a
/// lint finding (2.0 item 3): the declaration IS the signal — no LLM
/// needed to notice that two pages disagree once an author or the
/// consolidator said so.
#[derive(Debug, Clone, Serialize)]
pub struct ContradictionEdge {
    /// Wiki path of the page declaring the contradiction.
    pub from_path: String,
    /// Wiki path of the contradicted page (as declared; the target may
    /// be unresolved, in which case `resolved` is false).
    pub to_path: String,
    /// Whether the target currently resolves to a latest page.
    pub resolved: bool,
}

/// An unresolved cross-project link — a declared dependency on another
/// project's page that does not resolve. Surfaced by `memory_lint`.
#[derive(Debug, Clone, Serialize)]
pub struct DanglingCrossLink {
    /// Path of the page that authored the link (in the queried project).
    pub from_path: String,
    /// Target workspace name (`None` = the source page's own workspace).
    pub workspace: Option<String>,
    /// Target project name.
    pub project: String,
    /// Target page path within that project.
    pub path: String,
    /// Whether the named target project exists at all. `false` →
    /// likely a typo / wrong name; `true` → the page is missing or was
    /// renamed/deleted in an existing project (a broken dependency).
    pub project_exists: bool,
}

/// A page related to another through the link graph — used by the
/// page-view "references / referenced by" panel. Body is omitted; just
/// enough to render a clickable row.
#[derive(Debug, Clone, Serialize)]
pub struct RelatedPage {
    /// Relative wiki path of the related page.
    pub path: String,
    /// Title of the related page.
    pub title: String,
    /// Semantic kind of the related page.
    pub kind: String,
    /// Workspace the related page lives in. Lets a backlink from another
    /// project be labelled / navigated (the cross-project dependency signal).
    pub workspace: String,
    /// Project the related page lives in.
    pub project: String,
}

/// Resolved outgoing links and incoming back-links for one page.
/// Returned by [`ReaderPool::page_links`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct PageLinks {
    /// Latest pages this page references (resolved outgoing links).
    pub links: Vec<RelatedPage>,
    /// Latest pages that reference this page (incoming back-links).
    pub backlinks: Vec<RelatedPage>,
}

/// One page flagged by a workspace health check, with enough identity to
/// render a clickable drill-down row across projects.
#[derive(Debug, Clone, Serialize)]
pub struct HealthPage {
    /// Workspace name.
    pub workspace: String,
    /// Project name within the workspace.
    pub project: String,
    /// Relative wiki path.
    pub path: String,
    /// Page title.
    pub title: String,
    /// Semantic kind.
    pub kind: String,
}

/// Drill-down lists backing the workspace "memory health" counters.
/// Each list is capped; the headline counts stay authoritative.
/// Returned by [`ReaderPool::health_detail_for_workspace`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct HealthDetail {
    /// Episodic latest pages untouched for over 30 days.
    pub stale: Vec<HealthPage>,
    /// Latest pages sharing a title with at least one other page.
    pub duplicates: Vec<HealthPage>,
    /// Latest pages with no incoming or outgoing links.
    pub orphans: Vec<HealthPage>,
}

/// Cheap, cloneable read-only connection pool handle.
#[derive(Clone)]
pub struct ReaderPool {
    inner: Arc<Inner>,
    /// Opt-in post-fusion ranking signals (see [`RetrievalTuning`]). Lives
    /// on the handle, so clones taken after [`Self::set_retrieval_tuning`]
    /// share the operator's choice while the pool itself stays untouched.
    tuning: RetrievalTuning,
}

struct Inner {
    db_path: PathBuf,
    pool: Mutex<Vec<Connection>>,
    soft_cap: usize,
}

/// Parse a stored 16-byte blob into a `Uuid`, mapping a malformed value into
/// the store's error type (mirrors the `MalformedRecord` pattern used
/// elsewhere for corrupt stored data).
fn uuid_from_stored_bytes(bytes: &[u8], field: &str) -> StoreResult<Uuid> {
    Uuid::from_slice(bytes).map_err(|e| {
        StoreError::Memory(sessionmunch_core::MemoryError::MalformedRecord(format!(
            "invalid {field}: {e}"
        )))
    })
}

fn rrf_fuse(
    lexical: Vec<PassageHit>,
    dense: Option<Vec<(PassageHit, f32)>>,
    limit: usize,
) -> Vec<PassageHit> {
    const K: f64 = 60.0;
    let mut candidates: std::collections::BTreeMap<Uuid, PassageCandidate> =
        std::collections::BTreeMap::new();

    for hit in lexical {
        let lexical_rank = hit.lexical_rank.unwrap_or(usize::MAX);
        candidates.insert(
            hit.passage_id,
            PassageCandidate {
                hit,
                lexical_score: Some(1.0 / (K + lexical_rank as f64)),
                dense_score: None,
            },
        );
    }

    if let Some(dense_hits) = dense {
        for (dense_rank, (mut hit, _cosine)) in dense_hits.into_iter().enumerate() {
            hit.dense_rank = Some(dense_rank + 1);
            let dense_score = 1.0 / (K + (dense_rank + 1) as f64);
            candidates
                .entry(hit.passage_id)
                .and_modify(|c| {
                    c.hit.dense_rank = hit.dense_rank;
                    c.dense_score = Some(dense_score);
                })
                .or_insert(PassageCandidate {
                    hit,
                    lexical_score: None,
                    dense_score: Some(dense_score),
                });
        }
    }

    // Deterministic tie-break: sort by fused RRF score descending, then by
    // passage id ascending so re-running the same query never reorders ties
    // (`BTreeMap` iteration is already id-ordered, `sort_by` is stable, so
    // this two-key sort is enough without an explicit tertiary key).
    let mut fused: Vec<PassageCandidate> = candidates.into_values().collect();
    fused.sort_by(|a, b| {
        let score_a = a.lexical_score.unwrap_or(0.0) + a.dense_score.unwrap_or(0.0);
        let score_b = b.lexical_score.unwrap_or(0.0) + b.dense_score.unwrap_or(0.0);
        score_b
            .partial_cmp(&score_a)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.hit.passage_id.as_bytes().cmp(b.hit.passage_id.as_bytes()))
    });

    fused.truncate(limit);
    fused
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let mut hit = c.hit;
            hit.rank = i;
            hit.rrf_score = Some(c.lexical_score.unwrap_or(0.0) + c.dense_score.unwrap_or(0.0));
            hit
        })
        .collect()
}

fn l2_normalize(v: &[f32]) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / norm).collect()
}

fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

impl ReaderPool {
    /// Initialise the pool. Connections are opened lazily on first use.
    ///
    /// # Errors
    /// Currently infallible, but reserved so we can pre-open connections
    /// in a later milestone.
    pub fn new(db_path: &Path, soft_cap: usize) -> StoreResult<Self> {
        Ok(Self {
            inner: Arc::new(Inner {
                db_path: db_path.to_path_buf(),
                pool: Mutex::new(Vec::with_capacity(soft_cap.max(1))),
                soft_cap: soft_cap.max(1),
            }),
            tuning: RetrievalTuning::default(),
        })
    }

    /// Configure the opt-in ranking signals used by [`Self::hybrid_search`].
    /// Only handles cloned from this one afterwards observe the change.
    pub fn set_retrieval_tuning(&mut self, tuning: RetrievalTuning) {
        self.tuning = tuning;
    }

    /// The ranking signals this handle applies (default: none).
    #[must_use]
    pub fn retrieval_tuning(&self) -> RetrievalTuning {
        self.tuning
    }

    /// Run a synchronous closure against a pooled read-only connection.
    ///
    /// The closure runs on the tokio blocking pool so it never starves the
    /// async runtime. If the pool is empty we open a fresh connection;
    /// on return we keep it only when the pool is below its soft cap.
    ///
    /// # Errors
    /// Returns [`StoreError::PoolPanic`] if the blocking task panics; any
    /// error returned by the closure is propagated unchanged.
    pub async fn with_conn<F, T>(&self, f: F) -> StoreResult<T>
    where
        F: FnOnce(&Connection) -> StoreResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let conn = checkout(&inner)?;
            let result = f(&conn);
            checkin(&inner, conn);
            result
        })
        .await
        .map_err(|e| StoreError::PoolPanic(e.to_string()))?
    }
}

mod auth;
mod auto_improve;
mod briefing;
mod embeddings;
mod handoffs;
mod hybrid;
mod maintenance;
mod pages;
mod scopes;
mod search;
mod sessions;
mod status;
mod workstream;

/// Build the bounded history query and its optional owner binding.
///
/// A named operator reads two disjoint ranges: shared (`NULL`) and their own
/// owner key. Expressing those ranges as `OR` makes SQLite prefer the
/// project-wide timestamp index to satisfy `ORDER BY`, scanning other users'
/// history until it finds enough visible rows. `UNION ALL` lets SQLite merge
/// two already ordered owner-index ranges and stop at the requested limit.
fn handoff_listing_sql(
    has_state: bool,
    owner_filter: &OwnerFilter,
    limit: usize,
) -> (String, Option<String>) {
    const SELECT: &str = "SELECT id, workspace_id, project_id, from_session_id, from_agent, to_agent, \
                cwd, summary, open_questions, next_steps, files_touched, state, \
                created_at, accepted_by, accepted_at, accepted_by_session, \
                owner_user, accepted_by_user \
         FROM handoffs";
    let state_clause = if has_state { " AND state = ?3" } else { "" };
    let owner_index = if has_state { 4 } else { 3 };
    let sql_limit = limit.clamp(1, 500);
    match owner_filter {
        OwnerFilter::User(owner) => (
            format!(
                "{SELECT} \
                 WHERE workspace_id = ?1 AND project_id = ?2{state_clause} \
                   AND owner_user IS NULL \
                 UNION ALL \
                 {SELECT} \
                 WHERE workspace_id = ?1 AND project_id = ?2{state_clause} \
                   AND owner_user = ?{owner_index} \
                 ORDER BY created_at DESC \
                 LIMIT {sql_limit}"
            ),
            Some(owner.clone()),
        ),
        _ => {
            let (owner_clause, owner_param) = handoff_owner_sql(owner_filter, owner_index);
            (
                format!(
                    "{SELECT} \
                     WHERE workspace_id = ?1 AND project_id = ?2{state_clause}{owner_clause} \
                     ORDER BY created_at DESC \
                     LIMIT {sql_limit}"
                ),
                owner_param,
            )
        }
    }
}

/// Map a `(workspace, project, path, title, kind)` row to a [`HealthPage`].
fn health_page_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HealthPage> {
    Ok(HealthPage {
        workspace: row.get(0)?,
        project: row.get(1)?,
        path: row.get(2)?,
        title: row.get(3)?,
        kind: row.get(4)?,
    })
}

fn page_meta_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoreResult<PageMeta>> {
    let workspace_name: String = row.get(0)?;
    let project_name: String = row.get(1)?;
    let ws_id_bytes: Vec<u8> = row.get(2)?;
    let proj_id_bytes: Vec<u8> = row.get(3)?;
    let path: String = row.get(4)?;
    let title: String = row.get(5)?;
    let kind: String = row.get(6)?;
    let tier: String = row.get(7)?;
    let pinned: i64 = row.get(8)?;
    let created_us: i64 = row.get(9)?;
    let updated_us: i64 = row.get(10)?;
    let supersedes: Option<String> = row.get(11)?;
    // P1.7: author columns LEFT JOIN'd from `users`. NULL when the
    // page was written anonymously / by root, or when the user row
    // has been hard-deleted (FK `ON DELETE SET NULL`).
    let author_username: Option<String> = row.get(12)?;
    let author_name: Option<String> = row.get(13)?;
    let author_email: Option<String> = row.get(14)?;
    let expires_us: Option<i64> = row.get(15)?;
    let author = author_username.map(|username| PageAuthor {
        username,
        name: author_name,
        email: author_email,
    });

    let workspace_id = WorkspaceId::from_slice(&ws_id_bytes)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(2, 0))?;
    let project_id = ProjectId::from_slice(&proj_id_bytes)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(3, 0))?;

    let created_at = jiff::Timestamp::from_microsecond(created_us)
        .map(|ts| ts.to_string())
        .unwrap_or_default();
    let updated_at = jiff::Timestamp::from_microsecond(updated_us)
        .map(|ts| ts.to_string())
        .unwrap_or_default();
    let expired = expires_us.is_some_and(|us| us <= now_us());
    let expires_at = expires_us
        .and_then(|us| jiff::Timestamp::from_microsecond(us).ok())
        .map(|ts| ts.to_string());

    Ok(Ok(PageMeta {
        workspace_name,
        project_name,
        workspace_id,
        project_id,
        path,
        title,
        kind,
        tier,
        pinned: pinned != 0,
        created_at,
        updated_at,
        supersedes,
        author,
        expires_at,
        expired,
    }))
}

fn row_to_observation(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoreResult<Observation>> {
    let id_bytes: Vec<u8> = row.get(0)?;
    let session_bytes: Vec<u8> = row.get(1)?;
    let workspace_bytes: Vec<u8> = row.get(2)?;
    let project_bytes: Vec<u8> = row.get(3)?;
    let kind_str: String = row.get(4)?;
    let extension: Option<String> = row.get(5)?;
    let source_event: Option<String> = row.get(6)?;
    let title: String = row.get(7)?;
    let body: String = row.get(8)?;
    let importance: i64 = row.get(9)?;
    let created_us: i64 = row.get(10)?;
    Ok(materialise_observation(
        id_bytes,
        session_bytes,
        workspace_bytes,
        project_bytes,
        kind_str,
        extension,
        source_event,
        title,
        body,
        importance,
        created_us,
    ))
}

fn row_to_observation_record(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StoreResult<ObservationRecord>> {
    let id_bytes: Vec<u8> = row.get(0)?;
    let session_bytes: Vec<u8> = row.get(1)?;
    let kind: String = row.get(2)?;
    let title: String = row.get(3)?;
    let body: String = row.get(4)?;
    let importance: i64 = row.get(5)?;
    let created_us: i64 = row.get(6)?;
    let extension: Option<String> = row.get(7)?;
    let source_event: Option<String> = row.get(8)?;
    let created_at = jiff::Timestamp::from_microsecond(created_us)
        .map(|ts| ts.to_string())
        .unwrap_or_default();
    let record = ObservationId::from_slice(&id_bytes).and_then(|id| {
        SessionId::from_slice(&session_bytes).map(|session_id| ObservationRecord {
            id,
            session_id,
            kind,
            title,
            body,
            importance: u8::try_from(importance.clamp(1, 10)).unwrap_or(1),
            created_at,
            extension,
            source_event,
        })
    });
    Ok(record.map_err(StoreError::from))
}

#[allow(clippy::too_many_arguments)]
fn materialise_observation(
    id_bytes: Vec<u8>,
    session_bytes: Vec<u8>,
    workspace_bytes: Vec<u8>,
    project_bytes: Vec<u8>,
    kind_str: String,
    extension: Option<String>,
    source_event: Option<String>,
    title: String,
    body: String,
    importance: i64,
    created_us: i64,
) -> StoreResult<Observation> {
    Ok(Observation {
        id: ObservationId::from_slice(&id_bytes)?,
        session_id: SessionId::from_slice(&session_bytes)?,
        workspace_id: WorkspaceId::from_slice(&workspace_bytes)?,
        project_id: ProjectId::from_slice(&project_bytes)?,
        kind: kind_str
            .parse::<ObservationKind>()
            .map_err(StoreError::from)?,
        extension,
        source_event,
        title,
        body,
        importance: u8::try_from(importance.clamp(1, 10)).unwrap_or(1),
        created_at: jiff::Timestamp::from_microsecond(created_us).map_err(|e| {
            StoreError::Memory(sessionmunch_core::MemoryError::MalformedRecord(format!(
                "bad timestamp: {e}"
            )))
        })?,
    })
}

/// One stored embedding row, materialised for the vector path.
#[derive(Debug, Clone)]
pub struct StoredEmbedding {
    /// Page identifier (always the `is_latest=1` row's id).
    pub id: PageId,
    /// Relative wiki path.
    pub path: PagePath,
    /// Unit-normalised vector.
    pub vector: Vec<f32>,
}

fn score_desc(a: &(PageId, PagePath, f32), b: &(PageId, PagePath, f32)) -> std::cmp::Ordering {
    b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal)
}

fn dot_embedding_bytes(query: &[f32], bytes: &[u8], dim: u32) -> StoreResult<f32> {
    let dim = dim as usize;
    if query.len() != dim {
        return Err(StoreError::Memory(
            sessionmunch_core::MemoryError::MalformedRecord(format!(
                "query vector dim {} != expected {}",
                query.len(),
                dim
            )),
        ));
    }
    let expected = dim * 4;
    if bytes.len() != expected {
        return Err(StoreError::Memory(
            sessionmunch_core::MemoryError::MalformedRecord(format!(
                "embedding bytes {} != expected {}",
                bytes.len(),
                expected
            )),
        ));
    }
    Ok(query
        .iter()
        .zip(bytes.as_chunks::<4>().0)
        .map(|(q, chunk)| f32::from_le_bytes(*chunk) * q)
        .sum())
}

fn bytes_to_f32_vec(bytes: &[u8], dim: u32) -> StoreResult<Vec<f32>> {
    let expected = (dim as usize) * 4;
    if bytes.len() != expected {
        return Err(StoreError::Memory(
            sessionmunch_core::MemoryError::MalformedRecord(format!(
                "embedding bytes {} != expected {}",
                bytes.len(),
                expected
            )),
        ));
    }
    let mut out = Vec::with_capacity(dim as usize);
    for chunk in bytes.as_chunks::<4>().0 {
        out.push(f32::from_le_bytes(*chunk));
    }
    Ok(out)
}

/// Pack a `&[f32]` into little-endian bytes for storage. Inverse of
/// [`bytes_to_f32_vec`].
#[must_use]
pub fn f32_vec_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// One row's worth of input for the M8 retention formula.
/// One passage needing embedding for backfill.
#[derive(Debug, Clone)]
pub struct PassageEmbedCandidate {
    /// Stable passage identifier (UUID).
    pub id: uuid::Uuid,
    /// Passage text to embed.
    pub text: String,
}

/// One decay candidate for the page decay (forget) sweep.
#[derive(Debug, Clone, Serialize)]
pub struct DecayCandidate {
    /// Stable identifier.
    pub id: PageId,
    /// Relative wiki path.
    pub path: PagePath,
    /// Tier (the sweep only considers `episodic`).
    pub tier: sessionmunch_core::Tier,
    /// Pinned flag — true means "never decay".
    pub pinned: bool,
    /// `updated_at` in microseconds since epoch.
    pub updated_at_us: i64,
    /// Total query/access hits.
    pub access_count: u32,
    /// `last_accessed_at` in microseconds since epoch, or `None` if never accessed.
    pub last_accessed_at_us: Option<i64>,
    /// Frontmatter JSON; the sweep peeks at it for an explicit
    /// `pinned: true` (which overrides the schema flag).
    pub frontmatter_json: String,
    /// TTL instant in microseconds since epoch. The sweep hard-deletes
    /// pages past this regardless of tier or pin.
    pub expires_at_us: Option<i64>,
    /// Per-page salience once explicit feedback has moved it (V37);
    /// `None` means "use `DecayParams::salience_default`".
    pub salience: Option<f64>,
}

/// One forget-sweep tombstone eligible for permanent cleanup.
#[derive(Debug, Clone, Serialize)]
pub struct DecayTombstone {
    /// Evicted head whose ancestry chain is awaiting deletion.
    pub id: PageId,
    /// Relative wiki path associated with the evicted chain.
    pub path: PagePath,
}

fn row_to_decay_candidate(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StoreResult<DecayCandidate>> {
    let id_bytes: Vec<u8> = row.get(0)?;
    let path: String = row.get(1)?;
    let tier_str: String = row.get(2)?;
    let pinned: i64 = row.get(3)?;
    let updated_at_us: i64 = row.get(4)?;
    let access_count: i64 = row.get(5)?;
    let last_accessed_at_us: Option<i64> = row.get(6)?;
    let frontmatter_json: String = row.get(7)?;
    let expires_at_us: Option<i64> = row.get(8)?;
    let salience: Option<f64> = row.get(9)?;
    Ok(materialise_decay_candidate(
        id_bytes,
        path,
        tier_str,
        pinned,
        updated_at_us,
        access_count,
        last_accessed_at_us,
        frontmatter_json,
        expires_at_us,
        salience,
    ))
}

#[allow(clippy::too_many_arguments)]
fn materialise_decay_candidate(
    id_bytes: Vec<u8>,
    path: String,
    tier_str: String,
    pinned: i64,
    updated_at_us: i64,
    access_count: i64,
    last_accessed_at_us: Option<i64>,
    frontmatter_json: String,
    expires_at_us: Option<i64>,
    salience: Option<f64>,
) -> StoreResult<DecayCandidate> {
    Ok(DecayCandidate {
        id: PageId::from_slice(&id_bytes)?,
        path: PagePath::new(path)?,
        tier: tier_str
            .parse::<sessionmunch_core::Tier>()
            .map_err(StoreError::from)?,
        pinned: pinned != 0,
        updated_at_us,
        access_count: u32::try_from(access_count.max(0)).unwrap_or(u32::MAX),
        last_accessed_at_us,
        frontmatter_json,
        expires_at_us,
        salience,
    })
}

fn auto_improve_group_counts(
    conn: &Connection,
    column: &str,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    since_created_at: i64,
    extra_where: Option<&str>,
) -> rusqlite::Result<Vec<AutoImproveTelemetryCount>> {
    let learning_filter = "kind NOT IN ('curator_report', 'auto_improve_report')";
    let filter = extra_where.unwrap_or(learning_filter);
    let sql = format!(
        "SELECT COALESCE({column}, '(none)') AS key, COUNT(*) AS count
         FROM auto_improve_proposals
         WHERE workspace_id = ?1 AND project_id = ?2 AND staged_at >= ?3 AND {filter}
         GROUP BY key ORDER BY count DESC, key ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            since_created_at
        ],
        telemetry_count_from_row,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn auto_improve_rejection_group_counts(
    conn: &Connection,
    expr: &str,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    since_created_at: i64,
    limit: i64,
) -> rusqlite::Result<Vec<AutoImproveTelemetryCount>> {
    let sql = format!(
        "SELECT {expr} AS key, COUNT(*) AS count
         FROM auto_improve_rejections r
         LEFT JOIN auto_improve_proposals p ON p.id = r.source_proposal_id
         WHERE r.workspace_id = ?1 AND r.project_id = ?2 AND r.created_at >= ?3
           AND (r.source_proposal_id IS NULL OR COALESCE(p.kind, '') NOT IN ('curator_report', 'auto_improve_report'))
         GROUP BY key ORDER BY count DESC, key ASC LIMIT ?4"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            since_created_at,
            limit
        ],
        telemetry_count_from_row,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn auto_improve_repeated_rejection_fingerprints(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    since_created_at: i64,
    limit: i64,
) -> rusqlite::Result<Vec<AutoImproveTelemetryCount>> {
    let mut stmt = conn.prepare(
        "SELECT r.normalized_fingerprint AS key, COUNT(*) AS count
         FROM auto_improve_rejections r
         LEFT JOIN auto_improve_proposals p ON p.id = r.source_proposal_id
         WHERE r.workspace_id = ?1 AND r.project_id = ?2 AND r.created_at >= ?3
           AND (r.source_proposal_id IS NULL OR COALESCE(p.kind, '') NOT IN ('curator_report', 'auto_improve_report'))
         GROUP BY normalized_fingerprint HAVING count > 1
         ORDER BY count DESC, key ASC LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            since_created_at,
            limit
        ],
        telemetry_count_from_row,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn telemetry_count_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<AutoImproveTelemetryCount> {
    let count: i64 = row.get(1)?;
    Ok(AutoImproveTelemetryCount {
        key: row.get(0)?,
        count: usize::try_from(count).unwrap_or(usize::MAX),
    })
}

/// Normalize a cwd for comparison: treat Windows backslashes as path
/// separators and trim trailing separators while keeping a bare root as `/`.
/// Pure and string-only; it does not touch the filesystem.
pub(crate) fn normalize_cwd(p: &str) -> std::borrow::Cow<'_, str> {
    let replaced = if p.contains('\\') {
        std::borrow::Cow::Owned(p.replace('\\', "/"))
    } else {
        std::borrow::Cow::Borrowed(p)
    };
    let trimmed = replaced.trim_end_matches('/');
    if trimmed.is_empty() {
        std::borrow::Cow::Borrowed("/")
    } else if trimmed.len() == replaced.len() {
        replaced
    } else {
        std::borrow::Cow::Owned(trimmed.to_string())
    }
}

/// True when `descendant` is the same directory as `ancestor` or nested
/// below it, with component-boundary awareness: `/repo` contains
/// `/repo/api` but neither `/repo-other` nor `/repository`. Inputs are
/// normalized first. Pure; no SQL `LIKE`, so `%`/`_` in a path cannot act
/// as wildcards.
pub(crate) fn cwd_within(ancestor: &str, descendant: &str) -> bool {
    let a = normalize_cwd(ancestor);
    let d = normalize_cwd(descendant);
    if a == d {
        return true;
    }
    if a == "/" {
        // Root is an ancestor of every other absolute path.
        return d.starts_with('/') && d.len() > 1;
    }
    // Byte-prefix plus a `/` boundary right after it. `starts_with` is
    // byte-exact and `/` is ASCII, so this is UTF-8 safe.
    d.starts_with(a.as_ref()) && d.as_bytes().get(a.len()) == Some(&b'/')
}

fn filter_briefing_slots(
    snapshot: &mut BriefingSnapshot,
    visibility: &sessionmunch_core::SlotVisibility,
) {
    snapshot.slots.retain(|page| visibility.allows(&page.path));
    snapshot
        .recent_pages
        .retain(|page| visibility.allows(&page.path));
}

fn slot_visibility_sql(
    visibility: &sessionmunch_core::SlotVisibility,
    param_index: usize,
) -> (String, Option<String>) {
    if !visibility.hides_other_namespaces() {
        return ("path GLOB '_slots/*'".to_string(), None);
    }
    match visibility.own_namespace() {
        Some(namespace) => (
            format!(
                "path GLOB '_slots/*' AND (path NOT GLOB '_slots/*/*' OR path GLOB ?{param_index})"
            ),
            Some(format!("{}{namespace}/*", sessionmunch_core::SLOT_PREFIX)),
        ),
        None => (
            "path GLOB '_slots/*' AND path NOT GLOB '_slots/*/*'".to_string(),
            None,
        ),
    }
}

fn slot_exclusion_sql(
    visibility: &sessionmunch_core::SlotVisibility,
    param_index: usize,
) -> (String, Option<String>) {
    if !visibility.hides_other_namespaces() {
        return ("1".to_string(), None);
    }
    match visibility.own_namespace() {
        Some(namespace) => (
            format!("(path NOT GLOB '_slots/*/*' OR path GLOB ?{param_index})"),
            Some(format!("{}{namespace}/*", sessionmunch_core::SLOT_PREFIX)),
        ),
        None => ("path NOT GLOB '_slots/*/*'".to_string(), None),
    }
}

/// SQL fragment restricting a handoff query to what `filter` can actually see,
/// plus the owner key it expects bound at `?{param_index}` — the first free
/// positional parameter of the statement it is spliced into.
///
/// The count and the fetch must agree: a briefing that advertises a pending
/// baton the same caller cannot retrieve is worse than no count at all, because
/// the agent keeps asking for something that will always come back empty.
///
/// The owner key is BOUND, never interpolated. It is not a validated
/// identifier: it is [`sessionmunch_core::IdentityKey::storage_key`] TEXT, and on
/// a server behind a trusted proxy what follows the prefix is whatever
/// `X-Memory-Actor-Sub` carried — an OIDC subject the engine never parses — so
/// nothing upstream constrains its characters.
/// One open handoff, as an operator needs to see it to decide what to cancel.
///
/// Content-free by construction: identity, provenance and age only. The
/// summary body is deliberately absent — this exists so
/// `memory_handoff_cancel` has an id to be given, not to render the handoff.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OpenHandoff {
    pub id: String,
    pub from_agent: String,
    pub to_agent: Option<String>,
    pub cwd: Option<String>,
    pub created_at_ms: i64,
}

fn handoff_owner_sql(filter: &OwnerFilter, param_index: usize) -> (String, Option<String>) {
    match filter {
        OwnerFilter::Any => (String::new(), None),
        OwnerFilter::Unattributed => (" AND owner_user IS NULL".to_string(), None),
        OwnerFilter::User(user) => (
            format!(" AND (owner_user IS NULL OR owner_user = ?{param_index})"),
            Some(user.clone()),
        ),
    }
}

fn is_handoff_candidate(h: &Handoff, cwd_filter: Option<&str>, owner_filter: &OwnerFilter) -> bool {
    // Ownership is checked FIRST, before the manual short-circuit below.
    // Order matters: a manual handoff is project-wide by cwd, so checking the
    // owner afterwards would let one operator's `memory_handoff_begin` be
    // claimed by the next session to start, whoever it belongs to — the exact
    // cross-operator mixing this filter exists to stop. Handoffs with no owner
    // (every pre-V39 row, and anything written without an actor) stay visible
    // to everyone, which preserves single-operator behaviour untouched.
    if !owner_filter.admits(h.origin.owner_user.as_deref()) {
        return false;
    }
    // Manual handoffs (memory_handoff_begin always sets from_session_id = None)
    // are project-wide and always candidates, whatever cwd they carry. Only auto
    // SessionEnd handoffs are cwd-path-boundary scoped. This makes "a manual
    // handoff always beats the auto one" deterministic on from_session_id,
    // instead of relying on a manual handoff happening to have a NULL cwd.
    if h.origin.from_session_id.is_none() {
        return true;
    }
    auto_handoff_matches_cwd(h.origin.cwd.as_deref(), cwd_filter)
}

/// Whether an automatic handoff is eligible for a session starting in
/// `cwd_filter`. Shared with the acceptance path so selection and supersession
/// apply exactly the same path-boundary rule.
pub(crate) fn auto_handoff_matches_cwd(
    handoff_cwd: Option<&str>,
    cwd_filter: Option<&str>,
) -> bool {
    match (cwd_filter, handoff_cwd) {
        // Project-wide read: every open handoff is a candidate.
        (None, _) => true,
        // No stored cwd: treat as project-wide.
        (_, None) => true,
        // cwd-bearing auto handoffs match by path-boundary.
        (Some(session_cwd), Some(handoff_cwd)) => cwd_within(handoff_cwd, session_cwd),
    }
}

pub(crate) fn handoff_selection_key(
    manual: bool,
    created_at_micros: i64,
    cwd: Option<&str>,
    id: HandoffId,
) -> (bool, i64, usize, [u8; 16]) {
    // Cwd specificity discriminates only equal-time AUTO handoffs. For manual
    // handoffs it must be neutral so the newest manual wins, not whichever
    // happens to carry the longest cwd. Normalize before counting so legacy
    // `/repo/` rows do not outrank equivalent `/repo` rows.
    let auto_specificity = if manual {
        0
    } else {
        cwd.map_or(0, |cwd| normalize_cwd(cwd).len())
    };
    (
        manual,            // manual beats auto
        created_at_micros, // newest
        auto_specificity,  // most specific equal-time auto cwd
        *id.as_bytes(),    // deterministic final tie-break
    )
}

fn prefer_handoff(a: &Handoff, b: &Handoff) -> std::cmp::Ordering {
    handoff_selection_key(
        a.origin.from_session_id.is_none(),
        a.lifecycle.created_at.as_microsecond(),
        a.origin.cwd.as_deref(),
        a.scope.id,
    )
    .cmp(&handoff_selection_key(
        b.origin.from_session_id.is_none(),
        b.lifecycle.created_at.as_microsecond(),
        b.origin.cwd.as_deref(),
        b.scope.id,
    ))
}

/// Pick the handoff to deliver from a project's open handoffs.
///
/// See [`ReaderPool::latest_open_handoff`] for the full contract: manual handoffs
/// are project-wide, auto handoffs are filtered by cwd path-boundary, and a
/// manual handoff always beats an auto one, then newest, with cwd specificity
/// only breaking timestamp ties.
#[cfg(test)]
fn select_open_handoff(candidates: Vec<Handoff>, cwd_filter: Option<&str>) -> Option<Handoff> {
    candidates
        .into_iter()
        .filter(|h| is_handoff_candidate(h, cwd_filter, &OwnerFilter::Any))
        .max_by(prefer_handoff)
}

/// Build a [`Handoff`] from a row selected with the 18-column handoff SELECT
/// every handoff query in this file shares.
///
/// Reading the row here (instead of destructuring it into a long positional
/// argument list) keeps the column order defined in exactly one place next to
/// the `SELECT`s that produce it.
fn row_to_handoff(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoreResult<Handoff>> {
    let id_bytes: Vec<u8> = row.get(0)?;
    let ws_bytes: Vec<u8> = row.get(1)?;
    let pj_bytes: Vec<u8> = row.get(2)?;
    let from_session_bytes: Option<Vec<u8>> = row.get(3)?;
    let from_agent: String = row.get(4)?;
    let to_agent: Option<String> = row.get(5)?;
    let cwd: Option<String> = row.get(6)?;
    let summary: String = row.get(7)?;
    let open_q_json: String = row.get(8)?;
    let next_s_json: String = row.get(9)?;
    let files_json: String = row.get(10)?;
    let state: String = row.get(11)?;
    let created_us: i64 = row.get(12)?;
    let accepted_by: Option<String> = row.get(13)?;
    let accepted_at_us: Option<i64> = row.get(14)?;
    let accepted_by_session_bytes: Option<Vec<u8>> = row.get(15)?;
    let owner_user: Option<String> = row.get(16)?;
    let accepted_by_user: Option<String> = row.get(17)?;

    Ok((|| {
        for (label, value) in [
            ("handoff owner", owner_user.as_deref()),
            ("accepting handoff operator", accepted_by_user.as_deref()),
        ] {
            if value.is_some_and(|value| IdentityKey::from_storage_key(value).is_none()) {
                return Err(StoreError::MalformedRecord(format!(
                    "{label} is not a qualified identity storage key"
                )));
            }
        }
        let open_questions: Vec<String> = serde_json::from_str(&open_q_json)?;
        let next_steps: Vec<String> = serde_json::from_str(&next_s_json)?;
        let files_touched: Vec<String> = serde_json::from_str(&files_json)?;
        let from_session = from_session_bytes
            .as_deref()
            .map(SessionId::from_slice)
            .transpose()?;
        let accepted_session = accepted_by_session_bytes
            .as_deref()
            .map(SessionId::from_slice)
            .transpose()?;
        Ok(Handoff {
            scope: HandoffScope {
                id: HandoffId::from_slice(&id_bytes)?,
                workspace_id: WorkspaceId::from_slice(&ws_bytes)?,
                project_id: ProjectId::from_slice(&pj_bytes)?,
            },
            origin: HandoffOrigin {
                from_session_id: from_session,
                from_agent: parse_agent(&from_agent),
                to_agent: to_agent.as_deref().map(parse_agent),
                cwd,
                owner_user,
            },
            content: HandoffContent {
                summary,
                open_questions,
                next_steps,
                files_touched,
            },
            lifecycle: HandoffLifecycle {
                state: state.parse::<HandoffState>().map_err(StoreError::from)?,
                created_at: jiff::Timestamp::from_microsecond(created_us).map_err(|e| {
                    StoreError::Memory(sessionmunch_core::MemoryError::MalformedRecord(format!(
                        "bad created_at: {e}"
                    )))
                })?,
                accepted_by: accepted_by.as_deref().map(parse_agent),
                accepted_at: accepted_at_us
                    .map(jiff::Timestamp::from_microsecond)
                    .transpose()
                    .map_err(|e| {
                        StoreError::Memory(sessionmunch_core::MemoryError::MalformedRecord(
                            format!("bad accepted_at: {e}"),
                        ))
                    })?,
                accepted_by_session: accepted_session,
                accepted_by_user,
            },
        })
    })())
}

fn parse_agent(s: &str) -> AgentKind {
    AgentKind::from_wire(s)
}

fn count(conn: &Connection, sql: &str) -> StoreResult<u64> {
    count_bound(conn, sql, &[])
}

/// `COUNT(*)` with an explicit parameter list.
///
/// The scope-less [`count`] and the scoped `count_project` / `count_workspace`
/// helpers each bind a fixed shape; a predicate spliced in by the caller (the
/// handoff owner filter) adds one more parameter to whichever shape it lands in,
/// so it needs a helper that takes the whole list.
fn count_bound(conn: &Connection, sql: &str, params: &[&dyn rusqlite::ToSql]) -> StoreResult<u64> {
    let n: Option<i64> = conn.query_row(sql, params, |row| row.get(0)).optional()?;
    Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
}

fn count_project(
    conn: &Connection,
    sql: &str,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<u64> {
    let n: Option<i64> = conn
        .query_row(
            sql,
            params![workspace_id.as_bytes(), project_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
}

/// Cross-project link degree for a project: `(dependents, dependencies)`.
/// `dependents` = distinct other projects whose pages link into this one;
/// `dependencies` = distinct other projects this one links out to. Counts
/// resolved links only (`to_page_id` is set); project ids are globally
/// unique, so a bare `!= project_id` excludes self across all workspaces.
fn cross_project_degree(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<(u64, u64)> {
    let dependents: Option<i64> = conn
        .query_row(
            "SELECT COUNT(DISTINCT fp.project_id) \
             FROM links l \
             JOIN pages tp ON tp.id = l.to_page_id \
                 AND tp.workspace_id = ?1 AND tp.project_id = ?2 AND tp.is_latest = 1 \
             JOIN pages fp ON fp.id = l.from_page_id AND fp.is_latest = 1 \
             WHERE fp.project_id != ?2",
            params![workspace_id.as_bytes(), project_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    let dependencies: Option<i64> = conn
        .query_row(
            "SELECT COUNT(DISTINCT tp.project_id) \
             FROM links l \
             JOIN pages fp ON fp.id = l.from_page_id \
                 AND fp.workspace_id = ?1 AND fp.project_id = ?2 AND fp.is_latest = 1 \
             JOIN pages tp ON tp.id = l.to_page_id AND tp.is_latest = 1 \
             WHERE tp.project_id != ?2",
            params![workspace_id.as_bytes(), project_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    Ok((
        u64::try_from(dependents.unwrap_or(0)).unwrap_or(0),
        u64::try_from(dependencies.unwrap_or(0)).unwrap_or(0),
    ))
}

fn count_workspace(conn: &Connection, sql: &str, workspace_id: WorkspaceId) -> StoreResult<u64> {
    let n: Option<i64> = conn
        .query_row(sql, params![workspace_id.as_bytes()], |row| row.get(0))
        .optional()?;
    Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
}

fn normalize_fts_query(query: &str) -> String {
    // Delegates to prepare_fts5_query: neutralises `word:` column syntax and
    // quotes tokens so `-` / `*` are not FTS5 operators.
    prepare_fts5_query(query)
}

/// Count rows in a time-bounded window. Used by [`ReaderPool::briefing`]
/// to compute "last 7 days" / "last 30 days" activity slices.
fn window_activity(conn: &Connection, days: u32, cutoff_us: i64) -> StoreResult<ActivityWindow> {
    let count_since = |sql: &str| -> StoreResult<u64> {
        let n: Option<i64> = conn
            .query_row(sql, params![cutoff_us], |row| row.get(0))
            .optional()?;
        Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
    };
    Ok(ActivityWindow {
        days,
        // `sessions` schema uses `started_at`, not `created_at` — easy
        // to forget because the other tables all use `created_at`.
        sessions: count_since("SELECT COUNT(*) FROM sessions WHERE started_at > ?1")?,
        observations: count_since("SELECT COUNT(*) FROM observations WHERE created_at > ?1")?,
        pages_updated: count_since(
            "SELECT COUNT(*) FROM pages WHERE is_latest = 1 AND updated_at > ?1",
        )?,
    })
}

fn window_activity_project(
    conn: &Connection,
    days: u32,
    cutoff_us: i64,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<ActivityWindow> {
    let count_since = |sql: &str| -> StoreResult<u64> {
        let n: Option<i64> = conn
            .query_row(
                sql,
                params![workspace_id.as_bytes(), project_id.as_bytes(), cutoff_us],
                |row| row.get(0),
            )
            .optional()?;
        Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
    };
    Ok(ActivityWindow {
        days,
        sessions: count_since(
            "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1 AND project_id = ?2 AND started_at > ?3",
        )?,
        observations: count_since(
            "SELECT COUNT(*) FROM observations WHERE workspace_id = ?1 AND project_id = ?2 AND created_at > ?3",
        )?,
        pages_updated: count_since(
            "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1 AND updated_at > ?3",
        )?,
    })
}

fn window_activity_workspace(
    conn: &Connection,
    days: u32,
    cutoff_us: i64,
    workspace_id: WorkspaceId,
) -> StoreResult<ActivityWindow> {
    let count_since = |sql: &str| -> StoreResult<u64> {
        let n: Option<i64> = conn
            .query_row(sql, params![workspace_id.as_bytes(), cutoff_us], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
    };
    Ok(ActivityWindow {
        days,
        sessions: count_since(
            "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1 AND started_at > ?2",
        )?,
        observations: count_since(
            "SELECT COUNT(*) FROM observations WHERE workspace_id = ?1 AND created_at > ?2",
        )?,
        pages_updated: count_since(
            "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1 AND is_latest = 1 AND updated_at > ?2",
        )?,
    })
}

/// Materialise one row from the briefing's recent-pages / rules queries
/// into a [`BriefingPage`]. The row shape is `(path, title, kind,
/// updated_at_us)` — all queries above MUST select those columns in
/// that order.
fn briefing_page_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoreResult<BriefingPage>> {
    let path: String = row.get(0)?;
    let title: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let updated_us: i64 = row.get(3)?;
    Ok(jiff::Timestamp::from_microsecond(updated_us)
        .map(|ts| BriefingPage {
            path,
            title,
            kind,
            updated_at: ts.to_string(),
        })
        .map_err(|e| {
            StoreError::Memory(sessionmunch_core::MemoryError::MalformedRecord(format!(
                "bad updated_at: {e}"
            )))
        }))
}

/// Materialise one row from the briefing's `settled_first` query into a
/// [`SettledPage`]. The row shape is `(path, title, kind,
/// evidence_count)`.
fn settled_page_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SettledPage> {
    let path: String = row.get(0)?;
    let title: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let evidence_count: i64 = row.get(3)?;
    Ok(SettledPage {
        path,
        title,
        kind,
        evidence_count: u32::try_from(evidence_count).unwrap_or(u32::MAX),
    })
}

/// Reject cwds that can't safely participate in a `repo_path` prefix
/// match. Trailing slash is already trimmed by the caller; this catches:
/// empty / single-slash / dot-segments (a `/foo/../bar` resolved-by-LIKE
/// could match a stored `/foo` parent the cwd doesn't logically belong
/// to, since LIKE doesn't normalise paths). Treats any failure as "no
/// match" so the caller falls through to the safe create-by-basename path.
fn is_safe_cwd_for_prefix_match(cwd: &str) -> bool {
    if cwd.is_empty() || cwd == "/" {
        return false;
    }
    for segment in cwd.split('/') {
        if segment == "." || segment == ".." {
            return false;
        }
    }
    true
}

fn has_trailing_path_separator(path: &str) -> bool {
    path.len() > 1 && path.ends_with(['/', '\\'])
}

fn checkout(inner: &Inner) -> StoreResult<Connection> {
    if let Some(conn) = inner.pool.lock().pop() {
        return Ok(conn);
    }
    open_read_only(&inner.db_path)
}

fn checkin(inner: &Inner, conn: Connection) {
    let mut pool = inner.pool.lock();
    if pool.len() < inner.soft_cap {
        pool.push(conn);
    }
}

fn open_read_only(path: &Path) -> StoreResult<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)?;
    conn.pragma_update(None, "busy_timeout", 5_000)?;
    Ok(conn)
}

#[cfg(test)]
mod tests;
