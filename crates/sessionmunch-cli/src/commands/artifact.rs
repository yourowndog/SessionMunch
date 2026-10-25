//! `sessionmunch artifact get` — read one artifact's full bytes.
//!
//! Thin HTTP client over `GET /artifacts/{sha256}` on the server that
//! `import sessions` delivers to, so a digest from an imported event resolves
//! wherever that event's payload was uploaded.

use std::io::Write as _;

use anyhow::{Context as _, Result, anyhow};
use sessionmunch_workstream::ArtifactClient;

use crate::cli::ArtifactGetArgs;
use crate::config::Config;

/// Run `artifact get`.
///
/// # Errors
/// Returns an error for an unknown digest, an unreachable or refusing
/// server, a body that fails its hash check, or an unwritable `--output`.
pub async fn get(config: &Config, args: ArtifactGetArgs) -> Result<()> {
    let digest = args.sha256.trim().to_ascii_lowercase();
    let (url, token) = super::import_sessions::hub_target(config).await;
    let content = ArtifactClient::new(&url, token)
        .download(&digest)
        .await?
        .ok_or_else(|| anyhow!("{url} holds no artifact {digest}"))?;
    match &args.output {
        Some(path) => {
            std::fs::write(path, &content).with_context(|| format!("writing {}", path.display()))?
        }
        None => std::io::stdout()
            .lock()
            .write_all(&content)
            .context("writing to stdout")?,
    }
    Ok(())
}
