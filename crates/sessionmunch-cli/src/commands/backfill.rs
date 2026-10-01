//! `sessionmunch backfill` — operator-reachable section/passage backfill (G1).
//!
//! Sends a `POST /admin/backfill-sections` request to the server, which
//! invokes [`sessionmunch_store::ops::backfill_sections`] on the target
//! workspace/project. This is the thin-HTTP path that makes the existing
//! writer path reachable from the CLI without a server restart or
//! manual code path.
//!
//! Without this command, pages written before the G1 migration have no
//! sections/passages — only pages written after cutover get indexed.

use anyhow::Result;
use serde::Serialize;

use crate::cli::BackfillArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/backfill-sections`.
#[derive(Serialize)]
struct BackfillRequest {
    workspace: String,
    project: String,
    batch_size: usize,
    dry_run: bool,
    force: bool,
}

/// Summary returned by `POST /admin/backfill-sections`.
#[derive(Debug, serde::Deserialize)]
pub struct BackfillReport {
    /// Pages whose sections/passages were (or would be) rebuilt.
    pub pages_backfilled: u64,
    /// Pages skipped because sections already exist and `force` was false.
    pub pages_skipped: u64,
}

/// Run the `backfill` subcommand.
///
/// Sends the request to the server over HTTP and prints a human-readable
/// summary. In dry-run mode the server counts pages that would be
/// backfilled without mutating anything.
///
/// # Errors
/// Returns an error if the server is unreachable or returns a non-2xx
/// response.
pub async fn run(config: &Config, args: BackfillArgs) -> Result<()> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let (workspace, project) = if args.force && args.project.is_none() {
        (
            super::resolve_workspace(config, args.workspace.as_deref()),
            String::new(),
        )
    } else {
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?
    };
    let report: BackfillReport = post_json(
        &endpoint,
        "/admin/backfill-sections",
        &BackfillRequest {
            workspace,
            project,
            batch_size: 64,
            dry_run: args.dry_run,
            force: args.force,
        },
    )
    .await?;

    if args.dry_run {
        println!(
            "dry-run: would backfill {} page(s), {} already have sections",
            report.pages_backfilled, report.pages_skipped
        );
    } else {
        println!(
            "backfilled {} page(s) to sections/passages, {} already up-to-date",
            report.pages_backfilled, report.pages_skipped
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::cli::{Cli, Command};
    use clap::Parser;

    #[test]
    fn force_without_project_fans_out_to_all_projects() {
        let cli = Cli::try_parse_from(["sessionmunch", "backfill", "--force"]).unwrap();
        let Command::Backfill(args) = cli.command else {
            panic!("expected backfill command");
        };
        assert!(args.force);
        assert!(args.project.is_none());
        assert!(
            args.force && args.project.is_none(),
            "--force without --project must fan out to all projects"
        );
    }

    #[test]
    fn force_with_explicit_project_stays_scoped() {
        let cli = Cli::try_parse_from(["sessionmunch", "backfill", "--force", "--project", "myproj"])
            .unwrap();
        let Command::Backfill(args) = cli.command else {
            panic!("expected backfill command");
        };
        let all_projects = args.force && args.project.is_none();
        assert!(
            !all_projects,
            "--force with --project must stay scoped to that project"
        );
    }
}
