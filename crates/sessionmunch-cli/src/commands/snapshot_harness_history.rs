//! `snapshot-harness-history` — read-only, content-addressed preservation of
//! native harness transcript stores (North Star §5.9, P0 "stop losing history").

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde_json::{Value, json};
use sessionmunch_core::Sanitizer;
use sessionmunch_workstream::{
    HarnessSnapshot, ManagedHarness, SnapshotOutcome, SnapshotReport, snapshot_all_harnesses,
    snapshot_store,
};

use crate::cli::{RunHarnessChoice, SnapshotHarnessHistoryArgs};
use crate::commands::path_util;
use crate::config::Config;

const SNAPSHOT_VERSION: &str = "harness-store-v1";

pub async fn run(config: &Config, args: SnapshotHarnessHistoryArgs) -> Result<()> {
    let artifact_root = args
        .to
        .clone()
        .unwrap_or_else(|| config.data_dir.join("snapshots"));
    std::fs::create_dir_all(&artifact_root)
        .with_context(|| format!("creating snapshot root {}", artifact_root.display()))?;

    let sanitizer = Sanitizer::new(&config.sanitize).context("building canonical sanitizer")?;
    let machine_id = sysinfo::System::host_name().unwrap_or_else(|| "localhost".to_string());

    let home = args
        .home
        .clone()
        .or_else(|| config.home_dir.as_deref().map(PathBuf::from))
        .or_else(path_util::home_dir)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .ok()
                .unwrap_or(Path::new(".").to_path_buf())
        });
    let cwd = std::env::current_dir()
        .ok()
        .unwrap_or(Path::new(".").to_path_buf());

    let report = if let Some(harness) = args.harness {
        let snap = snapshot_store(
            to_managed(harness),
            &artifact_root,
            &home,
            &cwd,
            args.session_dir.as_deref(),
            &machine_id,
            &sanitizer,
        )
        .await?;
        SnapshotReport {
            harnesses: vec![snap],
        }
    } else {
        snapshot_all_harnesses(&artifact_root, &home, &cwd, &machine_id, &sanitizer).await?
    };

    if args.json {
        println!(
            "{}",
            serde_json::to_string(&report_json(&report, &artifact_root))?
        );
    } else {
        print_report(&report, &artifact_root);
    }

    println!(
        "\n{} preserved ({}, {} already-known, {} unreadable) → {}",
        if report.harnesses.len() == 1 {
            "snapshot complete"
        } else {
            "snapshots complete"
        },
        report.snapshotted_total(),
        report.already_known_total(),
        report.unreadable_total(),
        artifact_root.display()
    );
    Ok(())
}

fn to_managed(choice: RunHarnessChoice) -> ManagedHarness {
    match choice {
        RunHarnessChoice::Claude => ManagedHarness::Claude,
        RunHarnessChoice::Codex => ManagedHarness::Codex,
        RunHarnessChoice::OpenCode => ManagedHarness::OpenCode,
        RunHarnessChoice::OpenCode2 => ManagedHarness::OpenCode2,
        RunHarnessChoice::Pi => ManagedHarness::Pi,
        RunHarnessChoice::Crush => ManagedHarness::Crush,
        RunHarnessChoice::Omp => ManagedHarness::Omp,
        RunHarnessChoice::Kimi => ManagedHarness::Kimi,
        RunHarnessChoice::CommandCode => ManagedHarness::CommandCode,
        RunHarnessChoice::Kiro => ManagedHarness::Kiro,
        RunHarnessChoice::Grok => ManagedHarness::Grok,
        RunHarnessChoice::Antigravity => ManagedHarness::Antigravity,
    }
}

fn print_report(report: &SnapshotReport, root: &Path) {
    println!("Snapshot of native harness history");
    println!("  destination: {}", root.display());
    println!("  snapshot version: {}", SNAPSHOT_VERSION);
    println!("  harnesses: {}", report.harnesses.len());
    println!();
    for snap in &report.harnesses {
        print_harness(snap);
    }
}

fn print_harness(snap: &HarnessSnapshot) {
    let name = snap.harness.as_str();
    if let Some(reason) = snap.unsupported.as_deref() {
        println!("{name}: UNSUPPORTED — {reason}");
        return;
    }
    if snap.is_clean() {
        println!("{name}: OK");
    } else {
        println!("{name}: PARTIAL — see losses below");
    }
    if !snap.store_roots.is_empty() {
        println!("  store(s): {}", snap.store_roots.join(", "));
    }
    for root in &snap.missing_roots {
        println!("  MISSING store: {root}");
    }
    for source in &snap.sources {
        let tag = match source.outcome {
            SnapshotOutcome::Snapshotted => "snapshotted",
            SnapshotOutcome::AlreadyKnown => "already-known",
            SnapshotOutcome::Unreadable => "UNREADABLE",
        };
        let extra = source
            .detail
            .as_deref()
            .map(|detail| format!(" ({detail})"))
            .unwrap_or_default();
        println!("  {tag}: {}({} bytes){extra}", source.rel, source.size);
    }
    println!(
        "  summary: {} snapshotted, {} already-known, {} unreadable",
        snap.snapshotted(),
        snap.already_known(),
        snap.unreadable()
    );
}

fn report_json(report: &SnapshotReport, root: &Path) -> Value {
    let harnesses = report
        .harnesses
        .iter()
        .map(harness_json)
        .collect::<Vec<Value>>();
    let value: Value = json!({
        "snapshot_version": SNAPSHOT_VERSION.to_string(),
        "dest": root.to_string_lossy().to_string(),
        "harnesses": harnesses,
    });
    value
}

fn harness_json(snap: &HarnessSnapshot) -> Value {
    let sources = snap
        .sources
        .iter()
        .map(|source| {
            let outcome = match source.outcome {
                SnapshotOutcome::Snapshotted => "snapshotted",
                SnapshotOutcome::AlreadyKnown => "already-known",
                SnapshotOutcome::Unreadable => "unreadable",
            };
            let entry: Value = json!({
                "rel": source.rel,
                "sha256": source.sha256,
                "size": source.size,
                "outcome": outcome,
                "detail": source.detail,
            });
            entry
        })
        .collect::<Vec<Value>>();
    let value: Value = json!({
        "harness": snap.harness.as_str().to_string(),
        "store_roots": snap.store_roots,
        "missing_roots": snap.missing_roots,
        "unsupported": snap.unsupported,
        "snapshotted": snap.snapshotted(),
        "already_known": snap.already_known(),
        "unreadable": snap.unreadable(),
        "sources": sources,
    });
    value
}
