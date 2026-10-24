//! `sessionmunch search-events` — search preserved session events.
//!
//! Thin HTTP client over `GET /admin/search-events`. Answers "when did we run
//! X on Sleeper?" from the event ledger, not from a summary.

use anyhow::Result;
use sessionmunch_core::SessionEventHit;

use crate::cli::SearchEventsArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

/// Run the `search-events` subcommand.
///
/// # Errors
/// Returns an error for a malformed `--since`, an unreachable server, or a
/// non-2xx response.
pub async fn run(config: &Config, args: SearchEventsArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    let since_micros = match args.since.as_deref() {
        Some(raw) => {
            let window = super::import_sessions::parse_since(raw)?;
            let window = i64::try_from(window.as_micros()).unwrap_or(i64::MAX);
            Some(
                jiff::Timestamp::now()
                    .as_microsecond()
                    .saturating_sub(window),
            )
        }
        None => None,
    };
    let workspace = args
        .project
        .as_ref()
        .map(|_| args.workspace.clone().unwrap_or_else(|| "default".into()));
    let limit = args.limit.to_string();
    let since = since_micros.map(|micros| micros.to_string());
    let mut query: Vec<(&str, &str)> = vec![("q", args.query.as_str()), ("limit", &limit)];
    if let Some(machine) = args.machine.as_deref() {
        query.push(("machine", machine));
    }
    if let Some(harness) = args.harness.as_deref() {
        query.push(("harness", harness));
    }
    if let Some(since) = since.as_deref() {
        query.push(("since_micros", since));
    }
    if let (Some(workspace), Some(project)) = (workspace.as_deref(), args.project.as_deref()) {
        query.push(("workspace", workspace));
        query.push(("project", project));
    }
    if args.newest {
        query.push(("newest", "true"));
    }
    let hits: Vec<SessionEventHit> = get_json(&ep, "/admin/search-events", &query).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
        return Ok(());
    }
    if hits.is_empty() {
        println!("no events match {:?}", args.query);
        return Ok(());
    }
    println!("{} event(s) for {:?}:", hits.len(), args.query);
    for hit in &hits {
        let when = jiff::Timestamp::from_microsecond(hit.created_at)
            .map(|at| at.strftime("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|_| "unknown time".into());
        println!(
            "  {when}  {}  {}  {}/{}  {}",
            hit.machine_id.as_deref().unwrap_or("unknown-machine"),
            hit.harness,
            hit.workspace.as_deref().unwrap_or("?"),
            hit.project.as_deref().unwrap_or("?"),
            hit.kind.as_str(),
        );
        println!("    session {}", hit.session_id);
        println!("    {}", hit.snippet.replace('\n', " "));
    }
    Ok(())
}
