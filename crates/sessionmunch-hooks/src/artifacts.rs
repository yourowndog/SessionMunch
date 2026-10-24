//! Authoritative artifact ingest on Sleeper.
//!
//! `POST /artifacts` is the North Star P1.4 surface through which a remote
//! node ships a large sanitized evidence blob to the canonical Hub. It is
//! mounted beside `/session-events` so it receives the same bearer-auth
//! middleware as every other machine ingress, and each handler re-checks the
//! capability on the resolved [`AuthLevel`].
//!
//! Contract:
//! - The caller supplies the raw bytes, a **content receipt** (the lower
//!   SHA-256 hex digest of those raw bytes), and provenance (session id, node
//!   id, harness, event id/source). The Hub stores the blob content-addressed
//!   and gzip-compressed via [`ArtifactStore::store_verified`], which refuses
//!   a lying receipt hash (nothing is written) before persisting anything.
//! - On success the response carries the content address; the caller can then
//!   link the artifact to its session ledger explicitly. Retries of identical
//!   bytes are idempotent (the store returns the same digest). A corrupt or
//!   tampered pre-existing blob is repaired or rejected — never silently
//!   accepted (see [`ArtifactStore::write_blob`]).
//! - The store root derives from the configured data dir (a hub owns the
//!   authoritative artifact store); the caller passes no path.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Extension, Json, Router};
use serde::Serialize;
use sessionmunch_core::{AuthLevel, Capability};
use sessionmunch_workstream::{
    ARTIFACTS_PATH, ArtifactError, ArtifactReceipt, ArtifactStore, ArtifactUpload,
};

/// Hard cap on artifact raw byte length (16 MiB), matching the class of large
/// sanitized evidence (full tool outputs, imported transcripts) the store is
/// designed for while still rejecting unbounded bodies before decompression.
pub const MAX_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;

/// State shared by the authoritative artifact-ingest endpoint.
#[derive(Clone)]
pub struct ArtifactState {
    /// The authoritative content-addressed artifact store (hub's data dir).
    pub store: std::sync::Arc<ArtifactStore>,
}

/// Build the authoritative artifact-ingest router. Mounted beside
/// `/session-events`, so it receives the same bearer-auth middleware as all
/// machine ingress.
pub fn artifacts_router(state: ArtifactState) -> Router {
    Router::new()
        .route(ARTIFACTS_PATH, post(upload_artifact))
        .with_state(state)
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
}

fn api_failure(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (
        status,
        Json(ApiError {
            error: message.into(),
        }),
    )
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    api_failure(status, message).into_response()
}

fn authorize(
    level: Option<Extension<AuthLevel>>,
    capability: Capability,
) -> Result<(), (StatusCode, Json<ApiError>)> {
    let level = level.map_or(AuthLevel::Anonymous, |Extension(level)| level);
    level.authorize(capability, true).map_err(|failure| {
        let status = if failure.is_authentication_required() {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::FORBIDDEN
        };
        api_failure(status, failure.message())
    })
}

/// `POST {ARTIFACTS_PATH}`
async fn upload_artifact(
    State(state): State<ArtifactState>,
    level: Option<Extension<AuthLevel>>,
    Json(request): Json<ArtifactUpload>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }
    if request.content.is_empty() {
        return error(StatusCode::BAD_REQUEST, "artifact content cannot be empty");
    }
    if request.content.len() > MAX_ARTIFACT_BYTES {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("artifact exceeds {MAX_ARTIFACT_BYTES} byte cap"),
        );
    }

    // Authoritative write: verify the receipt hash before persisting anything,
    // and repair-or-reject a corrupt pre-existing blob (never silently keep
    // corruption and report success).
    match state
        .store
        .store_verified(&request.content, &request.digest)
    {
        Ok(digest) => {
            // Link provenance; a link failure is preserved on the response
            // rather than silently dropped (no phantom/durable-loss hiding).
            match state.store.link(&digest, &request.provenance) {
                Ok(_) => {}
                Err(failure) => {
                    return error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("stored {digest} but failed to link provenance: {failure}"),
                    );
                }
            }
            (
                StatusCode::CREATED,
                Json(ArtifactReceipt {
                    sha256: digest,
                    session_id: request.provenance.session_id,
                }),
            )
                .into_response()
        }
        Err(ArtifactError::HashMismatch { expected, computed }) => error(
            StatusCode::CONFLICT,
            format!("content hash mismatch: expected {expected}, computed {computed}"),
        ),
        Err(ArtifactError::InvalidDigest { digest, .. }) => {
            error(StatusCode::BAD_REQUEST, format!("invalid digest {digest}"))
        }
        Err(corrupt) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("artifact integrity failure: {corrupt}"),
        ),
    }
}
