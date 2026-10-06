//! HTTP caller that ships a content-addressed artifact to the canonical Hub's
//! `POST /artifacts` ingestion surface (North Star P1.4).
//!
//! The caller embodies the satellite half of artifact transport: it sends the
//! raw bytes, a content **receipt** (the lower SHA-256 hex digest), and
//! provenance, and the Hub verifies the receipt before persisting anything. A
//! lying receipt is refused (409) and nothing is written; a corrupt
//! pre-existing blob is repaired or rejected, never silently accepted. The
//! client never modifies the satellite's store — it only ships evidence.
//!
//! Error taxonomy mirrors the event sink's (see `drain.rs`): per-artifact
//! rejections (4xx that are the artifact's fault) are distinguished from the
//! destination-wide auth wall (401/403, the credential is at fault) and from
//! transient/transport failures (safe to retry).

use crate::artifact::{ARTIFACTS_PATH, ArtifactReceipt, ArtifactUpload};
use reqwest::Client;

/// Upper bound on raw artifact bytes the client will even attempt to ship.
/// Matches the Hub's server-side cap so an oversized payload is refused
/// locally before any bytes leave the node.
pub const ARTIFACT_UPLOAD_BYTE_CAP: usize = 16 * 1024 * 1024;

/// Errors from [`ArtifactClient::upload`], naming which side of the dialog is
/// at fault so a caller can decide whether to repair, reject, or retry.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactClientError {
    /// The Hub refused the digest as a lying receipt (HTTP 409): the caller
    /// supplied a content hash that does not match the bytes. Nothing was
    /// stored; the caller must recompute the receipt and resend.
    #[error("hub refused the content receipt (409): {message}")]
    HashMismatch {
        /// The Hub's explanation (its expected vs. computed digests).
        message: String,
    },

    /// The Hub rejected the artifact's shape/content as its own fault (4xx
    /// other than 409/401/403). Not retryable as-is.
    #[error("hub rejected the artifact (status {status}): {message}")]
    Rejected {
        /// HTTP status returned by the Hub.
        status: u16,
        /// The Hub's explanation.
        message: String,
    },

    /// The Hub could not store the artifact it accepted (5xx). Retry is
    /// reasonable later.
    #[error("hub reported an artifact integrity failure (status {status}): {message}")]
    Integrity {
        /// HTTP status returned by the Hub.
        status: u16,
        /// The Hub's explanation.
        message: String,
    },

    /// Authorization wall (401/403): the credential, not the artifact, is at
    /// fault. Retrying without fixing the credential will not succeed.
    #[error("hub refused authorization: {message}")]
    Unauthorized {
        /// The Hub's explanation.
        message: String,
    },

    /// Destination unreachable or a transient failure; the upload can be
    /// retried later without any change to the artifact.
    #[error("transport failure reaching {url}: {message}")]
    Transport {
        /// The Hub URL that could not be reached.
        url: String,
        /// The underlying failure.
        message: String,
    },

    /// The caller refused to ship an artifact that is locally malformed.
    #[error("refusing to upload: {message}")]
    Malformed {
        /// Why the artifact cannot be shipped as-is.
        message: String,
    },
}

/// HTTP caller that POSTs one [`ArtifactUpload`] to the Hub's
/// `POST /artifacts` surface and parses the resulting [`ArtifactReceipt`].
pub struct ArtifactClient {
    client: Client,
    /// `https://host:port` prefix; the ingest path is appended by
    /// [`ARTIFACTS_PATH`].
    base_url: String,
    /// Optional bearer token the Hub requires (`SESSIONMUNCH_AUTH_TOKEN`).
    token: Option<String>,
}

impl ArtifactClient {
    /// Create a caller for a Hub base URL with an optional bearer token. The
    /// ingest path is appended automatically.
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token,
        }
    }

    /// The fully-qualified artifact-ingest URL.
    #[must_use]
    pub fn upload_url(&self) -> String {
        format!("{}{}", self.base_url, &ARTIFACTS_PATH)
    }

    /// Ship one artifact. Returns the Hub's verified receipt, or an
    /// [`ArtifactClientError`] naming which side of the dialog is at fault.
    ///
    /// # Errors
    /// [`ArtifactClientError::Malformed`] for a locally-empty content; the
    /// hub-side statuses are surfaced as [`ArtifactClientError::HashMismatch`]
    /// (409), [`ArtifactClientError::Rejected`] (other 4xx),
    /// [`ArtifactClientError::Unauthorized`] (401/403), and
    /// [`ArtifactClientError::Integrity`] (5xx); an unreachable destination is
    /// [`ArtifactClientError::Transport`].
    pub async fn upload(
        &self,
        upload: &ArtifactUpload,
    ) -> Result<ArtifactReceipt, ArtifactClientError> {
        if upload.content.is_empty() {
            return Err(ArtifactClientError::Malformed {
                message: "artifact content cannot be empty".into(),
            });
        }
        if upload.content.len() > ARTIFACT_UPLOAD_BYTE_CAP {
            return Err(ArtifactClientError::Malformed {
                message: format!("content exceeds the {ARTIFACT_UPLOAD_BYTE_CAP} byte cap"),
            });
        }
        let mut builder = self.client.post(self.upload_url()).json(upload);
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| ArtifactClientError::Transport {
                url: self.upload_url(),
                message: format!("connect to {}: {e}", self.upload_url()),
            })?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        let trimmed = body.chars().take(512).collect::<String>();
        match status {
            201 => serde_json::from_str::<ArtifactReceipt>(&trimmed).map_err(|e| {
                ArtifactClientError::Integrity {
                    status,
                    message: format!("malformed 201 receipt: {e}"),
                }
            }),
            401 | 403 => Err(ArtifactClientError::Unauthorized { message: trimmed }),
            409 => Err(ArtifactClientError::HashMismatch { message: trimmed }),
            400 | 413 | 422 => Err(ArtifactClientError::Rejected {
                status,
                message: trimmed,
            }),
            // Everything else (transient 408/425/429/5xx, wrong-endpoint
            // 404/405/501, …): destination-wide or ambiguous, retry later.
            _ => Err(ArtifactClientError::Integrity {
                status,
                message: trimmed,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::ArtifactProvenance;
    use sessionmunch_core::SessionId;

    fn provenance() -> ArtifactProvenance {
        ArtifactProvenance {
            session_id: SessionId::new(),
            node_id: "satellite-1".into(),
            harness: "hermes".into(),
            event_id: Some("evt-upload".into()),
            event_source: Some("managed".into()),
            created_at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn upload_url_appends_artifacts_path_and_normalizes_slash() {
        let client = ArtifactClient::new("http://sleeper:49374/", None);
        assert_eq!(client.upload_url(), "http://sleeper:49374/artifacts");
        let root = ArtifactClient::new("http://sleeper", None);
        assert_eq!(root.upload_url(), "http://sleeper/artifacts");
    }

    #[tokio::test]
    async fn local_malformation_is_refused_before_any_io() {
        let client = ArtifactClient::new("http://127.0.0.1:1", None);
        let empty = ArtifactUpload {
            content: Vec::new(),
            digest: format!("{:064x}", 0usize),
            provenance: provenance(),
        };
        let err = client.upload(&empty).await.unwrap_err();
        assert!(matches!(err, ArtifactClientError::Malformed { .. }));
    }
}
