//! `sessionmunch import sessions` — global historical import by time range
//! (North Star §5 "Historical import", P1).
//!
//! Runs on the machine that owns the native harness stores. Every session
//! changed since `--since` is read through the read-only transcript readers,
//! sanitized, queued in the crash-safe session-event spool, and drained to
//! the hub's `POST /session-events`. Oversized payloads keep a head and tail
//! inline and ship whole to `POST /artifacts`. Re-running is idempotent: the
//! hub dedups on each stable event id, and a local cursor per native session
//! skips tails already queued.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, anyhow};
use secrecy::ExposeSecret as _;
use serde::{Deserialize, Serialize};
use sessionmunch_core::{AgentKind, Sanitizer, SessionId, SessionProvenance, SessionSource};
use sessionmunch_workstream::{
    ArtifactClient, ArtifactProvenance, ArtifactUpload, ExportedTranscript, HarnessDiscovery,
    HttpEventSink, IMPORTABLE_HARNESSES, ManagedHarness, Spool, SpoolBatch, SpoolConfig,
    SpoolDrainer, StoreStatus, discover_harness_history, discover_hermes_history,
    export_discovered, export_hermes_session, to_session_events,
};

use crate::cli::ImportSessionsArgs;
use crate::commands::path_util;
use crate::config::Config;
use crate::http_client::ServerEndpoint;

/// Events per spooled batch; well under the hub's 4096-event cap so one
/// batch stays a bounded request.
const EVENTS_PER_BATCH: usize = 500;
/// Unacknowledged spool cap. Large enough for a first full backfill.
const MAX_SPOOL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Workspace for sessions whose transcript records no usable scope.
const DEFAULT_WORKSPACE: &str = "default";
/// Project for sessions whose transcript records no working directory.
const UNATTRIBUTED_PROJECT: &str = "unattributed-history";

/// Per-harness counts in the final report.
#[derive(Debug, Serialize)]
struct HarnessReport {
    harness: String,
    store: StoreStatus,
    sessions_found: usize,
    sessions_with_new_events: usize,
    events_queued: usize,
    artifacts_uploaded: usize,
    losses: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ImportReport {
    machine: String,
    since: String,
    dry_run: bool,
    hub: Option<String>,
    harnesses: Vec<HarnessReport>,
    events_delivered: usize,
    events_already_known: usize,
    batches_pending: u64,
    delivery_problem: Option<String>,
}

/// Local per-session cursors: harness → native session id → adapter cursor.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CursorFile {
    #[serde(default)]
    cursors: BTreeMap<String, BTreeMap<String, String>>,
}

pub async fn run(config: &Config, args: ImportSessionsArgs) -> Result<()> {
    let since_window = parse_since(&args.since)?;
    let since = SystemTime::now()
        .checked_sub(since_window)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let home = args
        .home
        .clone()
        .or_else(|| config.home_dir.as_deref().map(PathBuf::from))
        .or_else(path_util::home_dir)
        .ok_or_else(|| anyhow!("cannot resolve the home directory; pass --home"))?;
    let machine = machine_identity();
    let sanitizer = Sanitizer::new(&config.sanitize).context("building canonical sanitizer")?;
    let (hub_url, token) = hub_target(config).await;

    let (harnesses, include_hermes) = select_sources(args.harness.as_deref())?;
    let hermes_home = args
        .hermes_home
        .clone()
        .unwrap_or_else(|| home.join(".hermes"));

    let import_dir = config.data_dir.join("history-import");
    let cursor_path = import_dir.join("cursors.json");
    let mut cursors = read_cursors(&cursor_path);
    let spool = if args.dry_run {
        None
    } else {
        std::fs::create_dir_all(&import_dir)
            .with_context(|| format!("creating {}", import_dir.display()))?;
        Some(Spool::open(&SpoolConfig {
            dir: import_dir.join("spool"),
            max_spool_bytes: MAX_SPOOL_BYTES,
        })?)
    };
    let artifacts = ArtifactClient::new(&hub_url, token.clone());

    let mut reports = Vec::new();
    for harness in harnesses {
        let discovery = discover_harness_history(harness, &home, since);
        let mut report = HarnessReport {
            harness: harness_label(harness).to_owned(),
            store: discovery.status.clone(),
            sessions_found: discovery.sessions.len(),
            sessions_with_new_events: 0,
            events_queued: 0,
            artifacts_uploaded: 0,
            losses: Vec::new(),
        };
        report.losses.extend(
            discovery
                .unreadable
                .iter()
                .map(|path| format!("unreadable transcript header: {path}")),
        );
        import_harness(
            config,
            &discovery,
            &home,
            &machine,
            &sanitizer,
            spool.as_ref(),
            &artifacts,
            &mut cursors,
            &mut report,
            args.dry_run,
        )
        .await;
        reports.push(report);
    }
    if include_hermes {
        let discovery = discover_hermes_history(&hermes_home, since);
        let mut report = HarnessReport {
            harness: HERMES_LABEL.to_owned(),
            store: discovery.status.clone(),
            sessions_found: discovery.sessions.len(),
            sessions_with_new_events: 0,
            events_queued: 0,
            artifacts_uploaded: 0,
            losses: discovery
                .unreadable
                .iter()
                .map(|store| format!("unreadable Hermes store: {store}"))
                .collect(),
        };
        let sources = discovery.sessions.iter().map(|session| SessionSourceRef {
            native_session_id: &session.native_session_id,
            cwd: session.cwd.as_deref(),
            agent: AgentKind::Hermes,
            export: Box::new(move |cursor| export_hermes_session(session, cursor)),
        });
        import_sessions(
            config,
            HERMES_LABEL,
            sources,
            &machine,
            &sanitizer,
            spool.as_ref(),
            &artifacts,
            &mut cursors,
            &mut report,
            args.dry_run,
        )
        .await;
        reports.push(report);
    }

    let mut events_delivered = 0;
    let mut events_already_known = 0;
    let mut delivery_problem = None;
    let mut batches_pending = 0;
    if let Some(spool) = spool {
        // Cursors advance only once their events sit durably in the spool,
        // so a crash after this point loses nothing and repeats nothing.
        write_cursors(&cursor_path, &cursors)?;
        let queued: usize = reports.iter().map(|report| report.events_queued).sum();
        let drainer =
            SpoolDrainer::new(spool.clone(), Arc::new(HttpEventSink::new(&hub_url, token)))
                .with_batches_per_pass(64);
        loop {
            let pass = drainer.drain_once().await;
            events_delivered += pass.accepted_events;
            if pass.stopped_on_offline || pass.stopped_on_auth || pass.permanent_failure {
                delivery_problem = Some(if pass.stopped_on_auth {
                    format!("hub {hub_url} refused the credential; batches stay queued")
                } else {
                    format!("hub {hub_url} is unreachable; batches stay queued for the next run")
                });
                break;
            }
            if !pass.quarantined_seqs.is_empty() {
                delivery_problem = Some(format!(
                    "hub rejected {} batch(es); they are quarantined in {}",
                    pass.quarantined_seqs.len(),
                    import_dir.join("spool").display()
                ));
            }
            if pass.delivered_batches == 0 || spool.is_empty().unwrap_or(true) {
                break;
            }
        }
        batches_pending = spool.metrics().pending_batches;
        events_already_known = queued.saturating_sub(events_delivered);
    }

    let report = ImportReport {
        machine,
        since: args.since.clone(),
        dry_run: args.dry_run,
        hub: (!args.dry_run).then_some(hub_url),
        harnesses: reports,
        events_delivered,
        events_already_known,
        batches_pending,
        delivery_problem,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }
    if report.delivery_problem.is_some() && report.batches_pending > 0 {
        return Err(anyhow!(
            "{} batch(es) remain queued; re-run `sessionmunch import sessions` once the hub is reachable",
            report.batches_pending
        ));
    }
    Ok(())
}

/// Reads one session's transcript tail after an optional cursor.
type ExportFn<'a> = dyn Fn(Option<&str>) -> Result<ExportedTranscript> + 'a;

/// One session to import, whatever store it came from.
struct SessionSourceRef<'a> {
    native_session_id: &'a str,
    cwd: Option<&'a Path>,
    agent: AgentKind,
    export: Box<ExportFn<'a>>,
}

#[allow(clippy::too_many_arguments)]
async fn import_harness(
    config: &Config,
    discovery: &HarnessDiscovery,
    home: &Path,
    machine: &str,
    sanitizer: &Sanitizer,
    spool: Option<&Spool>,
    artifacts: &ArtifactClient,
    cursors: &mut CursorFile,
    report: &mut HarnessReport,
    dry_run: bool,
) {
    let sources = discovery.sessions.iter().map(|session| SessionSourceRef {
        native_session_id: &session.native_session_id,
        cwd: session.cwd.as_deref(),
        agent: discovery.harness.agent_kind(),
        export: Box::new(move |cursor| export_discovered(session, home, cursor)),
    });
    import_sessions(
        config,
        harness_label(discovery.harness),
        sources,
        machine,
        sanitizer,
        spool,
        artifacts,
        cursors,
        report,
        dry_run,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn import_sessions<'a>(
    config: &Config,
    label: &str,
    sources: impl Iterator<Item = SessionSourceRef<'a>>,
    machine: &str,
    sanitizer: &Sanitizer,
    spool: Option<&Spool>,
    artifacts: &ArtifactClient,
    cursors: &mut CursorFile,
    report: &mut HarnessReport,
    dry_run: bool,
) {
    for session in sources {
        let native = session.native_session_id;
        let cursor = cursors
            .cursors
            .get(label)
            .and_then(|by_id| by_id.get(native))
            .cloned();
        let export = match (session.export)(cursor.as_deref()) {
            Ok(export) => export,
            Err(error) => {
                report
                    .losses
                    .push(format!("{native}: unreadable transcript: {error:#}"));
                continue;
            }
        };
        report
            .losses
            .extend(export.losses.iter().map(|loss| format!("{native}: {loss}")));
        if export.events.is_empty() {
            continue;
        }
        let (events, oversized) = to_session_events(&export.events, sanitizer);
        report.sessions_with_new_events += 1;
        report.events_queued += events.len();
        if dry_run {
            continue;
        }
        let Some(spool) = spool else { continue };

        let session_id = hook_session_id(native);
        for payload in oversized {
            let upload = ArtifactUpload {
                content: payload.content,
                digest: payload.sha256.clone(),
                provenance: ArtifactProvenance {
                    session_id,
                    node_id: machine.to_owned(),
                    harness: session.agent.as_str().to_owned(),
                    event_id: Some(payload.event_id),
                    event_source: Some(SessionSource::Import.as_str().to_owned()),
                    created_at_ms: jiff::Timestamp::now().as_millisecond(),
                },
            };
            match artifacts.upload(&upload).await {
                Ok(_) => report.artifacts_uploaded += 1,
                // The inline head/tail and the digest still reach the hub, so
                // the loss is the middle of one payload, named here.
                Err(error) => report.losses.push(format!(
                    "{native}: artifact {} not uploaded: {error}",
                    payload.sha256
                )),
            }
        }

        let (workspace, project) = scope_for(config, session.cwd);
        let provenance = SessionProvenance {
            native_session_id: Some(native.to_owned()),
            machine_id: Some(machine.to_owned()),
            cwd: session.cwd.map(|cwd| cwd.display().to_string()),
            source: SessionSource::Import,
            import_cursor: export.source_cursor.clone(),
            ..SessionProvenance::default()
        };
        let mut queued_all = true;
        for chunk in events.chunks(EVENTS_PER_BATCH) {
            let mut batch = SpoolBatch::new(
                workspace.clone(),
                project.clone(),
                session_id.to_string(),
                session.agent.as_str(),
                chunk.to_vec(),
            );
            batch.cwd = provenance.cwd.clone();
            batch.provenance = provenance.clone();
            if let Err(error) = spool.enqueue(&batch) {
                report
                    .losses
                    .push(format!("{native}: could not queue events: {error}"));
                queued_all = false;
                break;
            }
        }
        if queued_all && let Some(cursor) = export.source_cursor {
            cursors
                .cursors
                .entry(label.to_owned())
                .or_default()
                .insert(native.to_owned(), cursor);
        }
    }
}

/// The session id the hook path derives for this native id, so imported
/// transcript events and live hook capture converge on one session.
fn hook_session_id(native: &str) -> SessionId {
    native.parse::<SessionId>().unwrap_or_else(|_| {
        SessionId(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            native.as_bytes(),
        ))
    })
}

/// Workspace/project the hooks would resolve for this session's cwd: the
/// nearest marker first, then the main repository root, then the directory.
fn scope_for(config: &Config, cwd: Option<&Path>) -> (String, String) {
    let Some(cwd) = cwd.and_then(Path::to_str) else {
        return (DEFAULT_WORKSPACE.into(), UNATTRIBUTED_PROJECT.into());
    };
    let marker = crate::marker::read_scope(cwd, &config.runtime_env);
    let workspace = marker
        .as_ref()
        .and_then(|scope| scope.workspace.clone())
        .unwrap_or_else(|| DEFAULT_WORKSPACE.into());
    let project = marker
        .and_then(|scope| scope.project)
        .or_else(|| crate::marker::repo_root_project(cwd))
        .or_else(|| {
            Path::new(cwd)
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| UNATTRIBUTED_PROJECT.into());
    (workspace, project)
}

/// This node's machine identity, shared with the hook path so "on Sleeper"
/// means the same thing for hook capture and imported history.
pub(crate) fn machine_identity() -> String {
    sysinfo::System::host_name()
        .and_then(|name| sessionmunch_core::NodeId::new(name).ok())
        .map_or_else(|| "localhost".to_owned(), |node| node.as_str().to_owned())
}

/// Where imported events go: a satellite's configured hub, else the server
/// this CLI already talks to.
async fn hub_target(config: &Config) -> (String, Option<String>) {
    if let Some(central) = config.cluster.central_url() {
        let token = config
            .cluster
            .auth_token
            .as_ref()
            .map(|secret| secret.expose_secret().to_owned());
        return (central.to_owned(), token);
    }
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    (endpoint.build_url(""), endpoint.auth_token)
}

/// Label of the Hermes source in reports and cursor files.
const HERMES_LABEL: &str = "hermes";

/// Which stores to scan: every transcript harness plus Hermes, or one.
fn select_sources(choice: Option<&str>) -> Result<(Vec<ManagedHarness>, bool)> {
    let Some(raw) = choice.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok((IMPORTABLE_HARNESSES.to_vec(), true));
    };
    if raw.eq_ignore_ascii_case(HERMES_LABEL) {
        return Ok((Vec::new(), true));
    }
    match ManagedHarness::from_name(raw) {
        Some(ManagedHarness::Kiro) => {
            Ok((vec![ManagedHarness::Kiro, ManagedHarness::KiroV3], false))
        }
        Some(harness) => Ok((vec![harness], false)),
        None => Err(anyhow!(
            "unknown --harness {raw:?}; use hermes, claude, codex, opencode, opencode2, pi, omp, kimi, command-code, kiro, grok, crush, or antigravity"
        )),
    }
}

fn harness_label(harness: ManagedHarness) -> &'static str {
    match harness {
        ManagedHarness::Claude => "claude",
        ManagedHarness::Codex => "codex",
        ManagedHarness::OpenCode => "opencode",
        ManagedHarness::OpenCode2 => "opencode2",
        ManagedHarness::Pi => "pi",
        ManagedHarness::Crush => "crush",
        ManagedHarness::Omp => "omp",
        ManagedHarness::Kimi => "kimi",
        ManagedHarness::CommandCode => "command-code",
        ManagedHarness::Kiro => "kiro",
        ManagedHarness::KiroV3 => "kiro-v3",
        ManagedHarness::Grok => "grok",
        ManagedHarness::Antigravity => "antigravity",
    }
}

/// `7d`, `12h`, `30m`, `2w`, or `all`.
pub(crate) fn parse_since(raw: &str) -> Result<Duration> {
    sessionmunch_core::parse_time_window(raw).map_err(|message| anyhow!("--since {message}"))
}

fn read_cursors(path: &Path) -> CursorFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_cursors(path: &Path, cursors: &CursorFile) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(cursors)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

fn print_report(report: &ImportReport) {
    println!(
        "History import on {} (changed in the last {}){}",
        report.machine,
        report.since,
        if report.dry_run { " — dry run" } else { "" }
    );
    for harness in &report.harnesses {
        let store = match &harness.store {
            StoreStatus::Found => "found".to_owned(),
            StoreStatus::Missing => "no store on this machine".to_owned(),
            StoreStatus::Unsupported(why) => format!("unsupported: {why}"),
            StoreStatus::Unreadable(why) => format!("unreadable: {why}"),
        };
        println!(
            "  {:<13} {store}; {} session(s), {} with new events, {} event(s) queued, {} artifact(s)",
            harness.harness,
            harness.sessions_found,
            harness.sessions_with_new_events,
            harness.events_queued,
            harness.artifacts_uploaded
        );
        for loss in harness.losses.iter().take(5) {
            println!("      lost: {loss}");
        }
        if harness.losses.len() > 5 {
            println!("      … {} more (see --json)", harness.losses.len() - 5);
        }
    }
    if let Some(hub) = &report.hub {
        println!(
            "Delivered {} new event(s) to {hub}; {} already known; {} batch(es) still queued.",
            report.events_delivered, report.events_already_known, report.batches_pending
        );
    }
    if let Some(problem) = &report.delivery_problem {
        println!("Delivery: {problem}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_sessions_reuse_the_hook_session_identity() {
        let uuid = "4f6e1a52-0000-4000-8000-000000000001";
        assert_eq!(hook_session_id(uuid).to_string(), uuid);
        let opaque = "ses_01JABCDEF";
        assert_eq!(
            hook_session_id(opaque),
            SessionId(uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_OID,
                opaque.as_bytes()
            ))
        );
    }

    #[test]
    fn harness_labels_are_distinct() {
        let mut labels: Vec<_> = IMPORTABLE_HARNESSES
            .iter()
            .map(|h| harness_label(*h))
            .collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), IMPORTABLE_HARNESSES.len());
    }

    #[test]
    fn agent_kind_wire_names_round_trip() {
        for harness in IMPORTABLE_HARNESSES {
            let wire = harness.agent_kind().as_str();
            assert_eq!(AgentKind::from_wire(wire), harness.agent_kind());
        }
    }
}
