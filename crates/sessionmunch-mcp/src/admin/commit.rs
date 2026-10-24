//! Admin routes: commit.

use super::*;

/// JSON request body for `POST /admin/commit`.
#[derive(Deserialize)]
pub(super) struct CommitRequest {
    /// Commit message.
    message: String,
}

#[derive(Debug, Serialize)]
pub(super) struct CheckpointResponse {
    /// Full git commit OID.
    oid: String,
    /// First 12 hex characters, useful for CLI display.
    short_oid: String,
    /// Commit author timestamp in seconds since Unix epoch.
    time: i64,
    /// First line of the commit message.
    summary: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct CheckpointListQuery {
    /// Maximum number of checkpoints to return.
    #[serde(default = "default_checkpoint_limit")]
    limit: usize,
}

pub(super) fn default_checkpoint_limit() -> usize {
    20
}

pub(super) fn short_oid(oid: &str) -> String {
    oid.chars().take(12).collect()
}

pub(super) fn checkpoint_or_500(
    wiki: &Wiki,
    message: impl AsRef<str>,
) -> Result<Option<String>, (StatusCode, Json<serde_json::Value>)> {
    wiki.commit_all(message.as_ref())
        .map(|oid| oid.map(|oid| oid.to_string()))
        .map_err(|e| internal_err(e.to_string()))
}

pub(super) fn checkpoint_or_warn(wiki: &Wiki, message: impl AsRef<str>) -> Option<String> {
    match wiki.commit_all(message.as_ref()) {
        Ok(Some(oid)) => Some(oid.to_string()),
        Ok(None) => None,
        Err(e) => {
            warn!(error = %e, "wiki checkpoint failed after mutation");
            None
        }
    }
}

pub(super) async fn handle_commit(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<CommitRequest>,
) -> impl IntoResponse {
    match state.wiki.commit_all(&req.message) {
        Ok(Some(oid)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "committed": true,
                "oid": oid.to_string(),
            })),
        ),
        Ok(None) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "committed": false,
                "reason": "nothing to commit",
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

pub(super) async fn handle_checkpoints(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<CheckpointListQuery>,
) -> impl IntoResponse {
    let limit = query.limit.clamp(1, 100);
    match state.wiki.recent_checkpoints(limit) {
        Ok(checkpoints) => {
            let checkpoints: Vec<CheckpointResponse> = checkpoints
                .into_iter()
                .map(|cp| CheckpointResponse {
                    short_oid: short_oid(&cp.oid),
                    oid: cp.oid,
                    time: cp.time,
                    summary: cp.summary,
                })
                .collect();
            (
                StatusCode::OK,
                Json(serde_json::to_value(checkpoints).unwrap_or_else(|_| serde_json::json!([]))),
            )
        }
        Err(e) => internal_err(e.to_string()),
    }
}
