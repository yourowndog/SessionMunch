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
    ArtifactClient, ArtifactProvenance, ArtifactUpload, CutoffExclusions, ExportedTranscript,
    HarnessDiscovery, HttpEventSink, IMPORTABLE_HARNESSES, ManagedHarness, Spool, SpoolBatch,
    SpoolConfig, SpoolDrainer, StoreStatus, cutoff_exclusion, discover_harness_history,
    discover_hermes_history, export_discovered, export_hermes_session, retain_within_cutoff,
    stage_snapshot_home, to_session_events,
};

use crate::cli::{ChatExportProvider, ImportChatsArgs, ImportSessionsArgs};
use crate::config::Config;
use crate::host_env::HostEnv;
use crate::http_client::ServerEndpoint;
use sessionmunch_workstream::{HostedChatProvider, read_hosted_chat_export};
use sha2::{Digest as _, Sha256};

/// Events per spooled batch; well under the hub's 4096-event cap so one
/// batch stays a bounded request.
const EVENTS_PER_BATCH: usize = 500;
/// Back-off retries (2s, 4s, … capped at 60s) on a hub that stops answering.
const DRAIN_RETRIES: u32 = 6;
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
    /// Events withheld by the strict per-event cutoff, by reason.
    cutoff_excluded: CutoffExclusions,
    /// Sessions whose cursor stayed put because the cutoff withheld some of
    /// their events, so a later wider window can still import them.
    cursors_held: usize,
    losses: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ImportReport {
    machine: String,
    since: String,
    /// Fixed UTC invocation cutoff applied to each event's original source
    /// occurrence; absent for `--since all`.
    #[serde(skip_serializing_if = "Option::is_none")]
    cutoff: Option<String>,
    dry_run: bool,
    hub: Option<String>,
    harnesses: Vec<HarnessReport>,
    /// Snapshot blobs that could not be staged (`--from-snapshots` only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    snapshot_losses: Vec<String>,
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
    /// Window (seconds; `null` = `all`) each still-queued spool batch was
    /// selected with, keyed by spool sequence. Written with the cursors before
    /// draining, so a batch is never recorded wider than it was selected.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    queued_windows: BTreeMap<u64, Option<u64>>,
}

pub async fn run(config: &Config, host: &HostEnv, args: ImportSessionsArgs) -> Result<()> {
    // One invocation instant for both session discovery and the strict
    // per-event cutoff, so they cannot disagree.
    let invoked = SystemTime::now();
    let since_window = parse_since(&args.since)?;
    let since = invoked
        .checked_sub(since_window)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let cutoff = strict_cutoff(&args.since, invoked)?;
    let window = cutoff.map(|_| since_window.as_secs());
    let import_dir = config.data_dir.join("history-import");
    let (harnesses, include_hermes) = select_sources(args.harness.as_deref())?;
    // Cursors and spool sequences belong to one on-disk import state.
    // The spool's mutex only coordinates clones inside one process. Hold
    // an OS lock across discovery, enqueue, cursor updates and draining so
    // a timer and a manual import cannot overwrite each other's batches.
    // A dry run never opens the spool or writes cursors, so it needs no lock.
    let _import_lock = if args.dry_run {
        None
    } else {
        Some(acquire_import_lock(&import_dir)?)
    };
    let mut staging_losses = Vec::new();
    let (home, harnesses, include_hermes, staging) = match &args.from_snapshots {
        Some(dir) => {
            let snapshots = dir
                .clone()
                .unwrap_or_else(|| config.data_dir.join("snapshots"));
            // One staging home per run, so concurrent imports (or a dry run
            // beside a real one) never read or delete each other's files.
            // Dropping the guard removes it, on error paths too.
            std::fs::create_dir_all(&import_dir)
                .with_context(|| format!("creating {}", import_dir.display()))?;
            let staging = tempfile::Builder::new()
                .prefix("snapshot-home-")
                .tempdir_in(&import_dir)
                .with_context(|| format!("creating a staging home in {}", import_dir.display()))?;
            let staged = stage_snapshot_home(&snapshots, staging.path())
                .with_context(|| format!("staging snapshots from {}", snapshots.display()))?;
            staging_losses = staged.losses;
            let harnesses = harnesses
                .into_iter()
                .filter(|harness| staged.harnesses.contains(harness))
                .collect();
            (
                staging.path().to_path_buf(),
                harnesses,
                false,
                Some(staging),
            )
        }
        None => {
            let home = args
                .home
                .clone()
                .or_else(|| config.home_dir.as_deref().map(PathBuf::from))
                .or_else(|| host.home_dir())
                .ok_or_else(|| anyhow!("cannot resolve the home directory; pass --home"))?;
            (home, harnesses, include_hermes, None)
        }
    };
    let machine = machine_identity();
    let sanitizer = Sanitizer::new(&config.sanitize).context("building canonical sanitizer")?;
    let (hub_url, token) = if args.dry_run {
        ("http://127.0.0.1:1".into(), None)
    } else {
        hub_target(config).await
    };
    let hermes_home = args
        .hermes_home
        .clone()
        .unwrap_or_else(|| home.join(".hermes"));

    let cursor_path = import_dir.join("cursors.json");
    let mut cursors = read_cursors(&cursor_path);
    let spool = if args.dry_run {
        None
    } else {
        std::fs::create_dir_all(&import_dir)
            .with_context(|| format!("creating {}", import_dir.display()))?;
        let spool = Spool::open(&SpoolConfig {
            dir: import_dir.join("spool"),
            max_spool_bytes: MAX_SPOOL_BYTES,
        })?;
        refuse_out_of_window_pending(&spool, cutoff, window, &cursors, &import_dir)?;
        Some(spool)
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
            cutoff_excluded: CutoffExclusions::default(),
            cursors_held: 0,
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
            cutoff,
            window,
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
            cutoff_excluded: CutoffExclusions::default(),
            cursors_held: 0,
            losses: discovery
                .unreadable
                .iter()
                .map(|store| format!("unreadable Hermes store: {store}"))
                .collect(),
        };
        let sources = discovery.sessions.iter().map(|session| SessionSourceRef {
            native_session_id: &session.native_session_id,
            hosted: None,
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
            cutoff,
            window,
            args.dry_run,
        )
        .await;
        reports.push(report);
    }
    // Every transcript has been read; the staged links are no longer needed.
    if let Some(staging) = staging {
        let path = staging.path().to_path_buf();
        staging
            .close()
            .with_context(|| format!("removing {}", path.display()))?;
    }

    finish_import(
        spool,
        &cursor_path,
        &cursors,
        &import_dir,
        &hub_url,
        token,
        machine,
        args.since,
        cutoff,
        args.dry_run,
        args.json,
        reports,
        staging_losses,
    )
    .await
}

/// Import explicit hosted-chat exports through the same event/artifact spool.
pub async fn run_chats(config: &Config, args: ImportChatsArgs) -> Result<()> {
    let provider = match args.provider {
        ChatExportProvider::ChatGpt => HostedChatProvider::ChatGpt,
        ChatExportProvider::Claude => HostedChatProvider::Claude,
    };
    for value in [&args.workspace, &args.project] {
        if value.trim() != value
            || value.is_empty()
            || value.len() > 128
            || value.contains('/')
            || value.chars().any(char::is_control)
            || value == "_global"
        {
            return Err(anyhow!(
                "hosted chat history requires an explicit non-global workspace/project"
            ));
        }
    }
    let cutoff = match &args.since {
        Some(since) => strict_cutoff(since, SystemTime::now())?,
        None => None,
    };
    let window = match &args.since {
        Some(since) if cutoff.is_some() => Some(parse_since(since)?.as_secs()),
        _ => None,
    };
    let sanitizer = Sanitizer::new(&config.sanitize).context("building canonical sanitizer")?;
    let machine = machine_identity();
    let label = provider.agent().as_str();
    // No auth lookup, directory creation, upload or network activity on preview.
    let (hub_url, token) = if args.dry_run {
        ("http://127.0.0.1:1".into(), None)
    } else {
        hub_target(config).await
    };
    // Unlike native history, this is an explicit privacy boundary. A change
    // of destination, account, scope or credential cannot drain a previous
    // account's queued messages to the new destination.
    let destination_key = serde_json::to_vec(&(
        &hub_url,
        &token,
        label,
        &args.account,
        &args.workspace,
        &args.project,
    ))?;
    let key = format!("{:x}", Sha256::digest(destination_key));
    let import_dir = config
        .data_dir
        .join("history-import")
        .join("hosted")
        .join(key);
    let _lock = if args.dry_run {
        None
    } else {
        std::fs::create_dir_all(&import_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&import_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Some(acquire_import_lock(&import_dir)?)
    };
    let cursor_path = import_dir.join("cursors.json");
    let mut cursors = read_cursors(&cursor_path);
    let spool = if args.dry_run {
        None
    } else {
        let spool = Spool::open(&SpoolConfig {
            dir: import_dir.join("spool"),
            max_spool_bytes: MAX_SPOOL_BYTES,
        })?;
        refuse_out_of_window_pending(&spool, cutoff, window, &cursors, &import_dir)?;
        Some(spool)
    };
    let artifacts = ArtifactClient::new(&hub_url, token.clone());
    let mut report = HarnessReport {
        harness: label.into(),
        store: StoreStatus::Found,
        sessions_found: 0,
        sessions_with_new_events: 0,
        events_queued: 0,
        artifacts_uploaded: 0,
        cutoff_excluded: CutoffExclusions::default(),
        cursors_held: 0,
        losses: Vec::new(),
    };
    let mut failed = false;
    for path in &args.files {
        let decoded = match read_hosted_chat_export(path, provider, &args.account, &sanitizer) {
            Ok(value) => value,
            Err(error) => {
                report.losses.push(format!("{}: {error}", path.display()));
                failed = true;
                continue;
            }
        };
        if !decoded.losses.is_empty() {
            failed = true;
        }
        report.losses.extend(decoded.losses);
        report.sessions_found += decoded.chats.len();
        let sources = decoded.chats.iter().map(|chat| SessionSourceRef {
            native_session_id: &chat.transcript.native_session_id,
            hosted: Some((chat.session_id, &args.workspace, &args.project)),
            cwd: None,
            agent: provider.agent(),
            export: Box::new(move |cursor| {
                let mut transcript = chat.transcript.clone();
                if cursor == transcript.source_cursor.as_deref() && !args.dry_run {
                    transcript.events.clear();
                }
                Ok(transcript)
            }),
        });
        import_sessions(
            config,
            label,
            sources,
            &machine,
            &sanitizer,
            spool.as_ref(),
            &artifacts,
            &mut cursors,
            &mut report,
            cutoff,
            window,
            args.dry_run,
        )
        .await;
    }
    // Explicitly surface incomplete large-payload uploads/queue failures.
    failed |= report.losses.iter().any(|s| {
        s.contains("not uploaded:")
            || s.contains("could not queue events:")
            || s.contains("unreadable transcript:")
    });
    finish_import(
        spool,
        &cursor_path,
        &cursors,
        &import_dir,
        &hub_url,
        token,
        machine,
        args.since
            .clone()
            .unwrap_or_else(|| "selected hosted exports".into()),
        cutoff,
        args.dry_run,
        args.json,
        vec![report],
        Vec::new(),
    )
    .await?;
    if failed {
        return Err(anyhow!(
            "hosted history import had incomplete or malformed inputs; inspect the report"
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn finish_import(
    spool: Option<Spool>,
    cursor_path: &Path,
    cursors: &CursorFile,
    import_dir: &Path,
    hub_url: &str,
    token: Option<String>,
    machine: String,
    since: String,
    cutoff: Option<jiff::Timestamp>,
    dry_run: bool,
    json: bool,
    reports: Vec<HarnessReport>,
    staging_losses: Vec<String>,
) -> Result<()> {
    let mut events_delivered = 0;
    let mut events_already_known = 0;
    let mut delivery_problem = None;
    let mut batches_pending = 0;
    if let Some(spool) = spool {
        // Cursors advance only once their events sit durably in the spool,
        // so a crash after this point loses nothing and repeats nothing.
        write_cursors(cursor_path, cursors)?;
        let queued: usize = reports.iter().map(|report| report.events_queued).sum();
        let drainer =
            SpoolDrainer::new(spool.clone(), Arc::new(HttpEventSink::new(hub_url, token)))
                .with_batches_per_pass(64);
        let mut transient_retries = 0u32;
        loop {
            let pass = drainer.drain_once().await;
            events_delivered += pass.accepted_events;
            // A busy hub (a fleet-wide backfill) times requests out; that is
            // not an outage. Back off and retry before leaving it queued.
            if pass.stopped_on_offline && !pass.stopped_on_auth && transient_retries < DRAIN_RETRIES
            {
                transient_retries += 1;
                tokio::time::sleep(Duration::from_secs(2u64.pow(transient_retries).min(60))).await;
                continue;
            }
            if pass.delivered_batches > 0 {
                transient_retries = 0;
            }
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
        if let Ok(pending) = spool.pending() {
            let queued: std::collections::BTreeSet<u64> =
                pending.iter().map(|record| record.seq).collect();
            let mut pruned = CursorFile {
                cursors: cursors.cursors.clone(),
                queued_windows: cursors.queued_windows.clone(),
            };
            pruned.queued_windows.retain(|seq, _| queued.contains(seq));
            write_cursors(cursor_path, &pruned)?;
        }
        // Only meaningful once everything queued has been answered.
        if batches_pending == 0 {
            events_already_known = queued.saturating_sub(events_delivered);
        }
    }

    let report = ImportReport {
        machine,
        since,
        cutoff: cutoff.map(|cutoff| cutoff.to_string()),
        dry_run,
        hub: (!dry_run).then(|| hub_url.to_owned()),
        harnesses: reports,
        snapshot_losses: staging_losses,
        events_delivered,
        events_already_known,
        batches_pending,
        delivery_problem,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }
    if report.delivery_problem.is_some() {
        return Err(anyhow!(
            "history import did not fully deliver; {} batch(es) remain queued; inspect the report and retry",
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
    hosted: Option<(SessionId, &'a str, &'a str)>,
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
    cutoff: Option<jiff::Timestamp>,
    window: Option<u64>,
    dry_run: bool,
) {
    let sources = discovery.sessions.iter().map(|session| SessionSourceRef {
        native_session_id: &session.native_session_id,
        hosted: None,
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
        cutoff,
        window,
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
    cutoff: Option<jiff::Timestamp>,
    window: Option<u64>,
    dry_run: bool,
) {
    for session in sources {
        let native = session.native_session_id;
        let session_id = session
            .hosted
            .map_or_else(|| hook_session_id(native), |(id, _, _)| id);
        let cursor_key = session
            .hosted
            .map_or_else(|| native.to_owned(), |_| session_id.to_string());
        let cursor = cursors
            .cursors
            .get(label)
            .and_then(|by_id| by_id.get(&cursor_key))
            .cloned();
        let mut export = match (session.export)(cursor.as_deref()) {
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
        // Strict window on each event's original source occurrence. A session
        // that straddles the cutoff keeps its prior cursor: the withheld events
        // stay importable by a later, wider window, and the hub dedups the
        // in-window events it already holds by their stable ids.
        let excluded = cutoff.map_or_else(CutoffExclusions::default, |cutoff| {
            retain_within_cutoff(&mut export.events, cutoff)
        });
        report.cutoff_excluded.add(excluded);
        let hold_cursor = excluded.total() > 0;
        if hold_cursor {
            report.cursors_held += 1;
        }
        if export.events.is_empty() {
            continue;
        }
        let (mut events, oversized) = to_session_events(&export.events, sanitizer);
        // Per-event origin: a session can be imported from more than one
        // machine when a transcript store was copied between them.
        for event in &mut events {
            if let serde_json::Value::Object(metadata) = &mut event.metadata {
                metadata.insert(
                    if session.hosted.is_some() {
                        "imported_on"
                    } else {
                        "machine"
                    }
                    .into(),
                    machine.into(),
                );
            }
        }
        report.sessions_with_new_events += 1;
        if dry_run {
            report.events_queued += events.len();
            continue;
        }
        let Some(spool) = spool else { continue };

        let mut artifact_failure = false;
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
                Err(error) => {
                    artifact_failure = true;
                    report.losses.push(format!(
                        "{native}: artifact {} not uploaded: {error}",
                        payload.sha256
                    ));
                }
            }
        }

        // If a large payload failed to arrive, do not commit an incomplete
        // first-write ledger or advance the cursor. Retain the export for retry.
        if artifact_failure {
            continue;
        }
        let (workspace, project) = session.hosted.map_or_else(
            || scope_for(config, session.cwd),
            |(_, workspace, project)| (workspace.to_owned(), project.to_owned()),
        );
        let provenance = SessionProvenance {
            native_session_id: Some(native.to_owned()),
            machine_id: session.hosted.is_none().then(|| machine.to_owned()),
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
            match spool.enqueue(&batch) {
                Ok(seq) => {
                    cursors.queued_windows.insert(seq, window);
                }
                Err(error) => {
                    report
                        .losses
                        .push(format!("{native}: could not queue events: {error}"));
                    queued_all = false;
                    break;
                }
            }
            report.events_queued += chunk.len();
        }
        if queued_all
            && !hold_cursor
            && let Some(cursor) = export.source_cursor
        {
            cursors
                .cursors
                .entry(label.to_owned())
                .or_default()
                .insert(cursor_key, cursor);
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
pub(crate) async fn hub_target(config: &Config) -> (String, Option<String>) {
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

/// The fixed UTC instant a bounded `--since` window starts at, measured from
/// `invoked`; `None` for `all`. Applied to each event's original source
/// occurrence, never to file times, import time or ledger insertion.
fn strict_cutoff(raw: &str, invoked: SystemTime) -> Result<Option<jiff::Timestamp>> {
    if raw.trim().eq_ignore_ascii_case("all") {
        return Ok(None);
    }
    let window = parse_since(raw)?;
    let start = invoked
        .checked_sub(window)
        .ok_or_else(|| anyhow!("--since {raw:?} reaches before the Unix epoch; use `all`"))?;
    let cutoff = jiff::Timestamp::try_from(start)
        .map_err(|error| anyhow!("--since {raw:?} is out of range: {error}"))?;
    Ok(Some(cutoff))
}

/// Refuse to deliver batches an earlier import queued with a wider window.
/// A bounded run drains the whole spool, so without this an old, unrelated
/// backlog would ride along. Nothing is enqueued, drained or deleted here.
fn refuse_out_of_window_pending(
    spool: &Spool,
    cutoff: Option<jiff::Timestamp>,
    window: Option<u64>,
    cursors: &CursorFile,
    import_dir: &Path,
) -> Result<()> {
    let Some(cutoff) = cutoff else {
        return Ok(());
    };
    let pending = spool
        .pending()
        .with_context(|| format!("reading queued batches in {}", import_dir.display()))?;
    let outside = pending
        .iter()
        .filter(|record| match cursors.queued_windows.get(&record.seq) {
            // Selected by a window no wider than this one: it was in scope
            // when queued, even if a hub outage has since aged it past the
            // cutoff. Retrying it is this import's own unfinished work.
            Some(Some(queued)) => window.is_none_or(|current| *queued > current),
            // Queued by `--since all`.
            Some(None) => true,
            // No record (older binary, or a crash before the cursor write):
            // judge the events themselves, failing closed.
            None => record
                .batch
                .events
                .iter()
                .any(|event| cutoff_exclusion(event.occurred_at.as_deref(), cutoff).is_some()),
        })
        .count();
    if outside > 0 {
        return Err(anyhow!(
            "{outside} of {} queued batch(es) in {} are outside the --since window: queued by a wider window, or holding events before {cutoff} or with unknown time; refusing to deliver them with this import. Deliver them with the window that queued them, or inspect the spool first",
            pending.len(),
            import_dir.join("spool").display()
        ));
    }
    Ok(())
}

/// Exclusive access to a single history-import state directory.
struct ImportLock {
    _file: std::fs::File,
}

fn acquire_import_lock(import_dir: &Path) -> Result<ImportLock> {
    std::fs::create_dir_all(import_dir)
        .with_context(|| format!("creating {}", import_dir.display()))?;
    let path = import_dir.join(".import.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening history import lock {}", path.display()))?;
    // Never unlink a lock file: another process could hold that inode while
    // a new importer creates a different one. The OS unlocks it on exit.
    fs2::FileExt::try_lock_exclusive(&file).with_context(|| {
        format!(
            "cannot acquire history import lock {}; another import may be active; retry after it exits",
            path.display()
        )
    })?;
    Ok(ImportLock { _file: file })
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
        "History import on {} ({}{}){}",
        report.machine,
        report.since,
        report
            .cutoff
            .as_deref()
            .map(|cutoff| format!("; events occurring at or after {cutoff}"))
            .unwrap_or_default(),
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
        let excluded = harness.cutoff_excluded;
        if excluded.total() > 0 {
            println!(
                "      outside window: {} before cutoff, {} with unknown time; {} session cursor(s) held",
                excluded.before_cutoff, excluded.unknown_time, harness.cursors_held
            );
        }
        for loss in harness.losses.iter().take(5) {
            println!("      lost: {loss}");
        }
        if harness.losses.len() > 5 {
            println!("      … {} more (see --json)", harness.losses.len() - 5);
        }
    }
    for loss in &report.snapshot_losses {
        println!("  snapshot lost: {loss}");
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
    fn concurrent_imports_cannot_share_cursor_and_spool_state() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire_import_lock(dir.path()).unwrap();
        let error = acquire_import_lock(dir.path())
            .err()
            .expect("second writer refused");
        assert!(error.to_string().contains("history import lock"));
        // The persistent inode is harmless after the first importer exits.
        drop(first);
        assert!(acquire_import_lock(dir.path()).is_ok());
    }

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
