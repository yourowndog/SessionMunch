//! Tool argument and response types, with their limits and defaults.

use super::*;

pub(super) const MAX_QUERY_SCOPES: usize = 25;

pub(super) const DEFAULT_CONTENT_BUDGET: usize = 8_192;

pub(super) fn default_content_budget() -> Option<usize> {
    Some(DEFAULT_CONTENT_BUDGET)
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct MemoryScopeArg {
    /// Project to read inside the workspace.
    pub(super) project: String,
    /// Workspace to read.
    pub(super) workspace: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct QueryArgs {
    /// FTS5 query expression (e.g. `"karpathy wiki"` or `quick OR slow`).
    #[serde(alias = "q", alias = "search")]
    pub(super) query: String,
    /// Maximum number of hits to return (default 10, max 100).
    #[serde(default, alias = "n", alias = "top_k")]
    pub(super) limit: Option<usize>,
    /// Project to search. Session-aware clients may omit it for the current
    /// project. Static MCP clients must pass it together with `workspace` for
    /// every project-scoped call. Omit it for `global=true`.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace to search together with `project`. Session-aware clients may
    /// omit both for the current project; static MCP clients must pass both.
    /// Omit both for `global=true`.
    #[serde(default)]
    pub(super) workspace: Option<String>,
    /// Explicit multi-project scopes to search. Use this when a task
    /// needs context from a client project plus shared practice/project
    /// knowledge. Cannot be combined with `workspace`/`project`.
    #[serde(default)]
    pub(super) scopes: Vec<MemoryScopeArg>,
    /// Search EVERY project in every workspace in one call (cross-project
    /// global search). Use when you don't know which project holds the
    /// knowledge — e.g. shared infra/ops notes. When true, omit
    /// `project`/`workspace`/`scopes`; each hit is annotated with its
    /// workspace + project so you can tell where it came from.
    #[serde(default)]
    pub(super) global: Option<bool>,
    /// Also return pages whose `expires_at` TTL has passed (hidden by
    /// default; they are deleted by the next forget sweep). Default false.
    #[serde(default)]
    pub(super) include_expired: Option<bool>,
    /// Attach `score_details` to project/scopes hits: per-stream ranks
    /// (FTS5, entity, vector, graph), raw scores, and RRF contributions, plus a
    /// top-level `streams_active` list. A `global=true` query uses a
    /// different FTS-only ranker, so it reports only `streams_active`.
    /// Default false.
    #[serde(default)]
    pub(super) explain: Option<bool>,
    /// Time-travel: an ISO-8601 instant (e.g. `2026-06-01T00:00:00Z`).
    /// When set, the query becomes a time-travel lookup: the entity
    /// timeline fused (default-path RRF) with version-filtered full-text
    /// search over the page versions whose ingestion windows contained
    /// that instant — what the store knew then, including versions
    /// superseded since (docs/temporal.md). Vector / graph streams and
    /// the raw-observation fallback are skipped in this mode; cannot be
    /// combined with `global` or `scopes`. Omit for a normal search.
    #[serde(default)]
    pub(super) as_of: Option<String>,
    /// Retrieval unit: `page` (default, backward-compatible) or `passage`.
    /// When `passage`, each result is a text passage extracted at ingest
    /// time (D1a/D1b/E1), not a whole wiki page. Passage search requires an
    /// embedder for the dense vector stream; when no embedder is configured
    /// it degrades gracefully to FTS5-only passage search. Cannot be
    /// combined with `as_of` (time-travel operates on whole-page versions).
    #[serde(default, rename = "unit")]
    pub(super) unit: Option<QueryUnit>,
    /// How much parent context to attach per passage hit, so a caller
    /// knows which page and section a passage came from without a second
    /// lookup. `none` (default) keeps the response body self-contained
    /// with just the passage data. `section` adds the parent section's
    /// heading and body offset. `document` also expands to the page-level
    /// metadata (title, path). Ignored when `unit = "page"`.
    #[serde(default)]
    pub(super) parent_expansion: Option<ParentExpansion>,
    /// Per-hit content budget: the approximate character cap applied to
    /// the `snippet` (page mode) or `text` (passage mode) field of each
    /// result so a burst of hits cannot blow the caller's context
    /// window. Defaults to 8 192 chars. Ignored when 0 (no cap).
    #[serde(default = "default_content_budget")]
    pub(super) content_budget: Option<usize>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum QueryUnit {
    #[default]
    Page,
    Passage,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum ParentExpansion {
    #[default]
    None,
    Section,
    Document,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct RecentArgs {
    /// Maximum number of recent pages to return (default 10, max 100).
    #[serde(default, alias = "n")]
    pub(super) limit: Option<usize>,
    /// Project to read. Session-aware clients may omit it for the current
    /// project. Static MCP clients must pass it together with `workspace` for
    /// every project-scoped call.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace to read together with `project`. Session-aware clients may omit
    /// both for the current project; static MCP clients must pass both.
    #[serde(default)]
    pub(super) workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct StatusArgs {
    /// Project to report counts for. Session-aware clients may omit it for the
    /// current project. Static MCP clients must pass it together with
    /// `workspace` for every project-scoped call.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace to report together with `project`. Session-aware clients may
    /// omit both for the current project; static MCP clients must pass both.
    #[serde(default)]
    pub(super) workspace: Option<String>,
}

/// One `memory_query` hit; serializes exactly like a bare
/// [`sessionmunch_store::PageHit`] unless `explain=true` attached
/// `score_details`.
#[derive(Debug, Serialize)]
pub(super) struct QueryHit {
    #[serde(flatten)]
    pub(super) hit: sessionmunch_store::PageHit,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) score_details: Option<sessionmunch_store::SearchExplain>,
}

impl From<sessionmunch_store::PageHit> for QueryHit {
    fn from(hit: sessionmunch_store::PageHit) -> Self {
        Self {
            hit,
            score_details: None,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct ProjectSearchOptions<'a> {
    pub(super) query: &'a str,
    pub(super) query_vec: Option<&'a [f32]>,
    pub(super) limit: usize,
    pub(super) include_expired: bool,
    pub(super) explain: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct MemoryQueryResponse {
    pub(super) hits: Vec<QueryHit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) raw_hits: Vec<sessionmunch_store::ObservationHit>,
    /// Populated only by a `global=true` query: cross-project hits, each
    /// carrying its workspace + project name.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) global_hits: Vec<sessionmunch_store::PageHitWithMeta>,
    /// Standing user/team context from the reserved `_global` preferences
    /// scope, unioned into default-scoped queries alongside the current
    /// project's `hits` (issue #154). Empty when the scope doesn't exist or
    /// the query was explicitly scoped (`workspace`/`project`/`scopes`/
    /// `global=true`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) global_scope_hits: Vec<QueryHit>,
    /// Present only when `explain=true`: which retrieval streams ran for
    /// the primary search. Project/scopes retrieval always runs `fts` and
    /// `entity`, and `graph`; `vector` is present only when an embedder
    /// produced a query vector. Cross-project `global=true` retrieval is FTS-only.
    /// An `as_of` query runs `entity` plus version-filtered `fts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) streams_active: Option<Vec<&'static str>>,
}

/// One `memory_query` passage hit when `unit="passage"`. Carries the
/// passage's text and byte offsets, plus optional parent page/section
/// context based on `parent_expansion`.
#[derive(Debug, Serialize)]
pub(super) struct PassageQueryHit {
    /// Stable passage identifier.
    pub(super) passage_id: uuid::Uuid,
    /// Stable section identifier.
    pub(super) section_id: uuid::Uuid,
    /// Stable page identifier.
    pub(super) page_id: sessionmunch_core::PageId,
    /// Relative wiki path.
    pub(super) page_path: sessionmunch_core::PagePath,
    /// Page title.
    pub(super) page_title: String,
    /// Heading path (denormalized from parent section).
    pub(super) heading_path: String,
    /// Passage text, truncated if `content_budget` is set.
    pub(super) text: String,
    /// UTF-8 byte offset where this passage starts in the page body.
    pub(super) start_byte: i64,
    /// UTF-8 byte offset where this passage ends in the page body.
    pub(super) end_byte: i64,
    /// Relevance rank (0-based position in the result set).
    pub(super) rank: usize,
    /// 1-based rank in the lexical FTS5 stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) lexical_rank: Option<usize>,
    /// 1-based rank in the dense embedding stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) dense_rank: Option<usize>,
    /// Reciprocal Rank Fusion score (higher is better).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) rrf_score: Option<f64>,
    /// Parent context when `parent_expansion` is `section` or `document`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) parent: Option<PassageParent>,
}

/// Parent context attached to a passage hit.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(super) enum PassageParent {
    /// Section-level context: parent section's heading path and id.
    Section {
        /// Stable section identifier.
        section_id: uuid::Uuid,
        /// Heading path denormalized from the parent section.
        heading_path: String,
    },
    /// Document-level context: the page the passage belongs to.
    Document(PassageParentDocument),
}

/// Page-level parent context for `parent_expansion = "document"`.
#[derive(Debug, Serialize)]
pub(super) struct PassageParentDocument {
    /// Workspace identifier.
    pub(super) workspace_id: sessionmunch_core::WorkspaceId,
    /// Project identifier.
    pub(super) project_id: sessionmunch_core::ProjectId,
    /// Relative wiki path.
    pub(super) path: sessionmunch_core::PagePath,
    /// Page title.
    pub(super) title: String,
}

/// Response for passage-mode `memory_query`. Similar structure to
/// page-mode response but with `passage_hits` instead of `hits`.
#[derive(Debug, Serialize)]
pub(super) struct MemoryQueryPassageResponse {
    pub(super) hits: Vec<PassageQueryHit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) global_hits: Vec<sessionmunch_store::PageHitWithMeta>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) global_scope_hits: Vec<PassageQueryHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) streams_active: Option<Vec<&'static str>>,
}

/// Response for `memory_recent`. `hits` carries the current project's recent
/// pages; `global_hits` is populated instead when the read broadened to global
/// (repo opted into `[recall] default_global`), each hit annotated with its
/// workspace + project. `global_hits` is omitted when empty, so a plain
/// project-scoped response is unchanged.
#[derive(Debug, Serialize)]
pub(super) struct MemoryRecentResponse {
    pub(super) hits: Vec<sessionmunch_store::PageHit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) global_hits: Vec<sessionmunch_store::PageHitWithMeta>,
}

#[derive(Debug, Serialize)]
pub(super) struct StatusResponse {
    pub(super) counts: sessionmunch_store::StatusCounts,
}

/// Cap on the free-text `reason` stored with a feedback signal.
pub(super) const MAX_FEEDBACK_REASON_CHARS: usize = 500;

pub(super) fn sanitize_feedback_reason(sanitizer: &Sanitizer, raw: Option<&str>) -> Option<String> {
    let bounded: String = raw?
        .trim()
        .chars()
        .take(MAX_FEEDBACK_REASON_CHARS)
        .collect();
    if bounded.is_empty() {
        return None;
    }
    let scrubbed = sanitizer.scrub(&bounded);
    let single_line = scrubbed.split_whitespace().collect::<Vec<_>>().join(" ");
    let final_reason: String = single_line
        .chars()
        .take(MAX_FEEDBACK_REASON_CHARS)
        .collect();
    (!final_reason.is_empty()).then_some(final_reason)
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct FeedbackArgs {
    /// Exact wiki path of the page you are rating — copy it from the
    /// `memory_query` / `memory_read_page` hit.
    pub(super) path: String,
    /// Quality signal to record.
    pub(super) signal: FeedbackKind,
    /// Optional short note on *why*, especially for `stale` / `wrong`
    /// (e.g. "we moved off Postgres in March"). Shows up in the lint
    /// report. Sanitized and stored as a single line capped at 500 characters.
    #[serde(default)]
    pub(super) reason: Option<String>,
    /// Project the page lives in. Session-aware clients may omit it for the
    /// current project. Static MCP clients must pass it together with
    /// `workspace` for every project-scoped call.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace to use together with `project`. Session-aware clients may omit
    /// both for the current project; static MCP clients must pass both.
    #[serde(default)]
    pub(super) workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct SweepArgs {
    /// If true, preview only. Default false.
    #[serde(default)]
    pub(super) dry_run: Option<bool>,
    /// Project to sweep. Session-aware clients may omit it for the current
    /// project. Static MCP clients must pass it together with `workspace` for
    /// every project-scoped call.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace the project lives in. Session-aware clients may omit both scope
    /// fields for the current project; static MCP clients must pass both.
    #[serde(default)]
    pub(super) workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct LintArgs {
    /// If true, don't write wiki/_lint/report.md. Default false.
    #[serde(default)]
    pub(super) dry_run: Option<bool>,
    /// If true, skip the LLM contradiction pass (rule-based only).
    /// Useful when a provider is configured but you only want the
    /// fast rule-based checks. Default false.
    #[serde(default)]
    pub(super) no_llm: Option<bool>,
    /// Project to audit. Session-aware clients may omit it for the current
    /// project. Static MCP clients must pass it together with `workspace` for
    /// every project-scoped call.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace the project lives in. Session-aware clients may omit both scope
    /// fields for the current project; static MCP clients must pass both.
    #[serde(default)]
    pub(super) workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct ConsolidateArgs {
    /// UUID of the session to consolidate.
    pub(super) session_id: String,
    /// If true, preview without writing. Default false.
    #[serde(default)]
    pub(super) dry_run: Option<bool>,
    /// If true, M7b multi-page atomic fan-out. Default false (single page).
    #[serde(default)]
    pub(super) multi_page: Option<bool>,
    /// One-off advisory project preferences appended to the consolidation
    /// prompt as untrusted JSON data (sanitized, 2,000-character cap).
    /// Overrides the project's standing
    /// `_prompts/consolidation.md` page for this call only.
    #[serde(default)]
    pub(super) instructions: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct AutoImproveArgs {
    /// Completed session UUID to review. Omit to review the latest completed
    /// session without a persisted auto-improvement run in the resolved
    /// current project.
    #[serde(default)]
    pub(super) session_id: Option<String>,
    /// Removed compatibility field. Hidden from the tool schema; if an old
    /// caller still sends it, fail closed instead of turning an old preview
    /// request into an applying request.
    #[serde(default)]
    #[schemars(skip)]
    pub(super) dry_run: Option<bool>,
    /// Removed compatibility field; hidden from schema and rejected if present.
    #[serde(default)]
    #[schemars(skip)]
    pub(super) stage: Option<bool>,
    /// Removed compatibility field; hidden from schema and rejected if present.
    #[serde(default)]
    #[schemars(skip)]
    pub(super) mode: Option<String>,
    /// Project to review. Session-aware clients may omit it for the current
    /// project. Static MCP clients must pass it together with `workspace` for
    /// every project-scoped call.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace to review together with `project`. Session-aware clients may
    /// omit both for the current project; static MCP clients must pass both.
    #[serde(default)]
    pub(super) workspace: Option<String>,
    /// Override the minimum observation count for this run.
    #[serde(default)]
    pub(super) min_observations: Option<usize>,
    /// Override the minimum session span for this run.
    #[serde(default)]
    pub(super) min_session_duration_secs: Option<u64>,
    /// Override the proposal confidence floor for this run.
    #[serde(default)]
    pub(super) min_confidence: Option<f32>,
    /// Override the approximate chars/4 input token budget for this run.
    #[serde(default)]
    pub(super) max_input_tokens: Option<usize>,
    /// Override the maximum validated proposal count for this run.
    #[serde(default)]
    pub(super) max_proposals: Option<usize>,
    /// Include raw fallback context when the reviewer supports it.
    #[serde(default)]
    pub(super) include_raw_fallback: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct HandoffBeginArgs {
    /// Short prose summary of where the session left off.
    pub(super) summary: String,
    /// Questions the next agent should resolve.
    #[serde(default)]
    pub(super) open_questions: Vec<String>,
    /// Suggested next steps.
    #[serde(default)]
    pub(super) next_steps: Vec<String>,
    /// Files touched during the session.
    #[serde(default)]
    pub(super) files_touched: Vec<String>,
    /// Working directory at the time of handoff. Recorded on the handoff and
    /// used to scope AUTOMATIC session-end handoffs by path boundary. A handoff
    /// created through this tool is project-wide, so `cwd` does not narrow who
    /// receives it — ownership does.
    #[serde(default)]
    pub(super) cwd: Option<String>,
    /// Publish the handoff to everyone in the project instead of keeping it for
    /// you. By default a handoff belongs to the operator that created it, so on
    /// a shared server a teammate's session cannot consume it by accident. Set
    /// this when you deliberately want to pass the baton to whoever picks the
    /// project up next.
    #[serde(default)]
    pub(super) shared: Option<bool>,
    /// Project to scope the handoff to. Session-aware clients may omit it for
    /// the current project. Static MCP clients must pass it together with
    /// `workspace` for every project-scoped call. When set to a name that
    /// doesn't exist yet, the project is **created**.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace to scope the handoff to, together with `project`; created if it
    /// doesn't exist. Session-aware clients may omit both for the current
    /// project; static MCP clients must pass both. Missing explicit scope can
    /// route a cross-workspace handoff to the wrong project.
    #[serde(default)]
    pub(super) workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct HandoffAcceptArgs {
    /// Set the receiving cwd so automatic handoffs from that directory or a
    /// path-boundary ancestor are eligible.
    /// **Omit unless the user explicitly asks about a handoff from a
    /// *different* directory** — by default this scopes to the current
    /// project (the SessionStart hook usually pre-fetches it into context).
    #[serde(default)]
    pub(super) cwd: Option<String>,
    /// Also consider handoffs that belong to OTHER operators. Off by default:
    /// on a shared server you only see your own plus the ones published to the
    /// whole project. Use this for recovery ("somebody left a baton here and
    /// they are away"), knowing it consumes their handoff.
    #[serde(default)]
    pub(super) any_owner: Option<bool>,
    /// Project to accept a handoff from. Session-aware clients may omit it for
    /// the current project. Static MCP clients must pass it together with
    /// `workspace` for every project-scoped call.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace to accept from, together with `project`. Session-aware clients
    /// may omit both for the current project; static MCP clients must pass both.
    #[serde(default)]
    pub(super) workspace: Option<String>,
    /// Exact open handoff id returned by `memory_handoff_list` or
    /// `memory_handoff_begin`. When set, this call claims that row rather than
    /// the latest eligible open handoff. Omit to keep the latest-open behavior.
    #[serde(default)]
    pub(super) handoff_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct HandoffListArgs {
    /// Also list handoffs that belong to OTHER operators. Off by default:
    /// on a shared server you only see your own plus the ones published to the
    /// whole project. Root-only recovery; requires an explicit user request.
    #[serde(default)]
    pub(super) any_owner: Option<bool>,
    /// Maximum open handoffs to return (clamped to 1..=200, default 50).
    #[serde(default)]
    pub(super) limit: Option<u32>,
    /// Project to list within. Session-aware clients may omit it for the
    /// current project. Static MCP clients must pass it together with
    /// `workspace` for every project-scoped call.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace to list within, together with `project`. Session-aware
    /// clients may omit both for the current project; static MCP clients must
    /// pass both.
    #[serde(default)]
    pub(super) workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct HandoffCancelArgs {
    /// Cancel even when the handoff belongs to another operator. Off by
    /// default; requires the same authority as other cross-operator actions.
    #[serde(default)]
    pub(super) any_owner: Option<bool>,
    /// Exact handoff id returned by `memory_handoff_begin`. Required so this
    /// tool only discards a handoff the agent can identify.
    pub(super) handoff_id: String,
    /// Project to cancel within. Session-aware clients may omit it for the
    /// current project. Static MCP clients must pass it together with
    /// `workspace` for every project-scoped call.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace to cancel within, together with `project`. Session-aware
    /// clients may omit both for the current project; static MCP clients must
    /// pass both.
    #[serde(default)]
    pub(super) workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct BriefingArgs {
    /// How many recently-updated pages to include (default 10, max 100).
    #[serde(default)]
    pub(super) recent_pages_limit: Option<usize>,
    /// Project to brief on. Session-aware clients may omit it for the current
    /// project. Static MCP clients must pass it together with `workspace` for
    /// every project-scoped call.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace to brief together with `project`. Session-aware clients may
    /// omit both for the current project; static MCP clients must pass both.
    #[serde(default)]
    pub(super) workspace: Option<String>,
    /// Lead the briefing with the project's settled rule/decision pages
    /// (highest-standing, ordered by evidence then recency). Default `false`
    /// leaves the briefing shape unchanged.
    #[serde(default)]
    pub(super) settled_first: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct ExploreArgs {
    /// Optional topic to bias the digest toward (e.g. "recent rules",
    /// "pending handoffs", or a free-form question). When absent the
    /// digest covers the project broadly.
    #[serde(default)]
    pub(super) focus: Option<String>,
    /// How many recently-updated pages the underlying briefing should
    /// consider (default 10).
    #[serde(default)]
    pub(super) recent_pages_limit: Option<usize>,
    /// Project to explore. Session-aware clients may omit it for the current
    /// project. Static MCP clients must pass it together with `workspace` for
    /// every project-scoped call.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace to explore together with `project`. Session-aware clients may
    /// omit both for the current project; static MCP clients must pass both.
    #[serde(default)]
    pub(super) workspace: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct InstallSelfRoutingArgs {
    /// Return the compact routing block instead of full operational guidance.
    /// Use when managed Agent Skills are installed or when refreshing a file that
    /// already uses the compact snippet.
    #[serde(default)]
    pub(super) compact: Option<bool>,
}

// The "you MUST pass exactly one of path/query" contract lives in the
// field descriptions and the runtime validation, NOT in a root-level
// `anyOf` (#577). The machine-readable encoding (#155) let
// schema-respecting clients refuse a null-filled call pre-flight — but
// the Anthropic Messages API rejects root-level `anyOf`/`oneOf`/`allOf`
// outright, so ANY provider-agnostic client routing tools through it
// (OpenCode with an Anthropic key, and every other Messages-API
// consumer) had its WHOLE session 400 before a single tool ran. One
// wasted round-trip for a null-filling client is a far smaller cost
// than dead sessions on the most common upstream; the per-flavor strip
// machinery (#412) stays for any future dialect that needs it.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct ReadPageArgs {
    /// FTS5 query to find the page (searches and returns the top hit's full
    /// body). You MUST pass exactly one of `query` or `path` — never neither,
    /// and never `null`. Ignored when `path` is provided.
    #[serde(default, alias = "q", alias = "search")]
    pub(super) query: Option<String>,
    /// Exact wiki path (e.g. `notes/foo.md`), typically taken verbatim from a
    /// `memory_recent` or `memory_query` hit. You MUST pass exactly one of
    /// `path` or `query` — never neither, and never `null`. Takes precedence
    /// over `query`.
    #[serde(default)]
    pub(super) path: Option<String>,
    /// Project to read from. Session-aware clients may omit it for the current
    /// project. Static MCP clients must pass it together with `workspace` for
    /// every project-scoped call.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace to read together with `project`. Session-aware clients may omit
    /// both for the current project; static MCP clients must pass both.
    #[serde(default)]
    pub(super) workspace: Option<String>,
}

/// Bounds for `memory_read_session_observations`. The defaults keep one call
/// well under a context window; the ceilings match what the store keeps per
/// body (16 KiB) so a caller can always read a whole observation in one go.
pub(super) const SESSION_OBSERVATIONS_DEFAULT_LIMIT: usize = 50;

pub(super) const SESSION_OBSERVATIONS_MAX_LIMIT: usize = 200;

pub(super) const SESSION_OBSERVATIONS_DEFAULT_BODY_CHARS: usize = 4_000;

pub(super) const SESSION_OBSERVATIONS_MIN_BODY_CHARS: usize = 200;

pub(super) const SESSION_OBSERVATIONS_MAX_BODY_CHARS: usize = 16_384;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct SearchEventsArgs {
    /// Full-text query over preserved session events: commands, prompts,
    /// tool output, error strings (e.g. `"cargo nextest"`, `ECONNREFUSED`).
    pub(super) query: String,
    /// Only sessions recorded on this machine (host name, e.g. `sleeper`).
    #[serde(default)]
    pub(super) machine: Option<String>,
    /// Only this harness (`claude-code`, `codex`, `open-code`, `hermes`, …).
    #[serde(default)]
    pub(super) harness: Option<String>,
    /// Only events from the last window: `30m`, `12h`, `7d`, `2w`.
    #[serde(default)]
    pub(super) since: Option<String>,
    /// `true` orders newest first ("when did we LAST run X"); default orders
    /// by relevance.
    #[serde(default)]
    pub(super) newest: Option<bool>,
    /// Maximum hits (default 20, max 100).
    #[serde(default)]
    pub(super) limit: Option<usize>,
    /// Restrict to one project. Static MCP clients must pass it together with
    /// `workspace`; omit both to search every project, machine, and harness.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace to search together with `project`; omit both for all history.
    #[serde(default)]
    pub(super) workspace: Option<String>,
}

/// Default and cap for `memory_search_events` hits.
pub(super) const SEARCH_EVENTS_DEFAULT_LIMIT: usize = 20;

pub(super) const SEARCH_EVENTS_MAX_LIMIT: usize = 100;

/// Per-hit content cap in `memory_search_events` responses.
pub(super) const SEARCH_EVENTS_CONTENT_CHARS: usize = 2_000;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct ReadSessionObservationsArgs {
    /// Session id (UUID) to read, typically taken from a `memory_query`
    /// raw hit, `memory_briefing`, or an admin session listing. Omit to
    /// read the most recent COMPLETED session visible to you in the
    /// resolved project.
    #[serde(default)]
    pub(super) session_id: Option<String>,
    /// Maximum observations per call (default 50, max 200).
    #[serde(default)]
    pub(super) limit: Option<usize>,
    /// Observations to skip before the first returned one (default 0).
    /// Combine with `total` from a previous response to page.
    #[serde(default)]
    pub(super) offset: Option<usize>,
    /// `asc` (capture order, default) or `desc` (newest first).
    #[serde(default)]
    pub(super) order: Option<String>,
    /// Keep only these observation kinds, e.g. `["user-prompt", "stop"]`.
    /// Known kinds: `session-start`, `user-prompt`, `pre-tool-use`,
    /// `post-tool-use`, `pre-compact`, `post-compaction`, `notification`,
    /// `stop`, `session-end`, `other`. Omit to keep every kind.
    #[serde(default)]
    pub(super) kinds: Option<Vec<String>>,
    /// Optional full-text query over observation titles and bodies,
    /// restricted to this session. Omit to list the session in order.
    #[serde(default)]
    pub(super) query: Option<String>,
    /// Cap each returned body at this many characters (default 4000, min
    /// 200, max 16384). Longer bodies end with a visible truncation marker.
    #[serde(default)]
    pub(super) body_max_chars: Option<usize>,
    /// Project the session belongs to. Session-aware clients may omit it for
    /// the current project. Static MCP clients must pass it together with
    /// `workspace` for every project-scoped call.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace to read together with `project`. Session-aware clients may omit
    /// both for the current project; static MCP clients must pass both.
    #[serde(default)]
    pub(super) workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct DeletePageArgs {
    /// Exact wiki path to delete (e.g. `notes/foo.md`).
    pub(super) path: String,
    /// Project to delete from. Session-aware clients may omit it for the current
    /// project. Static MCP clients must pass it together with `workspace` for
    /// every project-scoped call.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace to delete from together with `project`. Session-aware clients
    /// may omit both for the current project; static MCP clients must pass both.
    /// Missing explicit sibling scope fails closed instead of falling back.
    #[serde(default)]
    pub(super) workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(super) struct WritePageArgs {
    /// Relative wiki path to write, for example `notes/santander-2025.md`.
    pub(super) path: String,
    /// Markdown body. Pass the durable fact/note content, not a handoff
    /// summary. Start the body with `# Some Title` — sessionmunch derives the
    /// page title from that H1 automatically, so you do not need (and should
    /// not pass) the `title` argument.
    pub(super) body: String,
    /// **Prefer omitting this.** sessionmunch derives the title from the first
    /// `# H1` in `body` (or the path stem if there is no heading), so the
    /// safest call is to leave this out and put the title as a markdown H1
    /// on the first line of `body`. Passing a title here forces the agent
    /// to JSON-escape the string correctly — a known source of `JSON parsing`
    /// errors when the title contains quotes, colons, or other punctuation
    /// (issue #67). Only set this when there's no usable H1 in the body.
    #[serde(default)]
    pub(super) title: Option<String>,
    /// Tier (`working`, `episodic`, `semantic`, `procedural`). Default semantic.
    #[serde(default)]
    pub(super) tier: Option<String>,
    /// Tags to attach to the page.
    #[serde(default)]
    pub(super) tags: Vec<String>,
    /// Pin the page so the decay sweep skips it.
    #[serde(default)]
    pub(super) pinned: bool,
    /// Project to write into. Session-aware clients may omit it for the current
    /// project. Static MCP clients must pass it together with `workspace` for
    /// every project-scoped call. Omit it when `scope: "global"`. When set to a
    /// name that doesn't exist yet, the project is **created**.
    #[serde(default)]
    pub(super) project: Option<String>,
    /// Workspace to write into. Only honoured together with an explicit
    /// `project`; created if it doesn't exist. Session-aware clients may omit
    /// both for the current project; static MCP clients must pass both. Omit
    /// both when `scope: "global"`.
    #[serde(default)]
    pub(super) workspace: Option<String>,
    /// Set to `"global"` to write into the reserved `_global` preferences
    /// scope — standing user/team context (tech preferences, code style,
    /// durable decisions) that default `memory_query` reads union into
    /// every project. Cannot be combined with `workspace`/`project`.
    #[serde(default)]
    pub(super) scope: Option<String>,
    /// Optional TTL: RFC3339 instant (`2026-09-01T12:00:00Z`) or bare
    /// date (`2026-09-01` = end of that day, UTC). After this instant
    /// the page is hidden from search/recent/briefing and hard-deleted
    /// by the next forget sweep. Omit for pages that never expire.
    #[serde(default)]
    pub(super) expires_at: Option<String>,
}
