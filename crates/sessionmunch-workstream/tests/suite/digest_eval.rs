//! Dev-only runner: renders digest cards for a private evaluation set of real
//! native sessions. Ignored by default and reads nothing from the repository,
//! so no real session content ever lands here. Run it with
//!
//! ```text
//! SESSIONMUNCH_DIGEST_EVAL=<dir> cargo t -p sessionmunch-workstream \
//!     --run-ignored only -E 'test(/digest_eval/)'
//! ```
//!
//! `<dir>/sessions.tsv` lists `NN<TAB>harness<TAB>source`, where source is a
//! JSONL path, `<hermes state.db>#<session id>` or `<opencode.db>#<session
//! id>` (`~/` expands to the home directory). Cards are written to
//! `<dir>/cards/NN.md` with the facts beside them as `NN.facts.json`, and the
//! exported events as `NN.events.jsonl` (`e` is the card's `eN` number) for
//! summary experiments that must cite the same events.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context as _, Result, bail};

use crate::digest::{
    NativeSignals, extract_transcript_facts, hermes_signals, opencode_signals, read_native_signals,
    render_card,
};
use crate::harness::ManagedHarness;
use crate::hermes_history::{discover_hermes_history, export_hermes_session};
use crate::transcript::{ExportedTranscript, export_jsonl, export_opencode, session_header};

#[test]
#[ignore = "reads a private evaluation set named by SESSIONMUNCH_DIGEST_EVAL"]
#[allow(clippy::disallowed_methods)] // dev-only runner, not a library path
fn digest_eval_renders_cards_for_the_private_set() {
    let dir =
        PathBuf::from(std::env::var("SESSIONMUNCH_DIGEST_EVAL").expect("SESSIONMUNCH_DIGEST_EVAL"));
    let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
    let list = std::fs::read_to_string(dir.join("sessions.tsv")).expect("sessions.tsv");
    let out = dir.join("cards");
    std::fs::create_dir_all(&out).expect("cards dir");
    for line in list.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<&str> = line.split('\t').collect();
        let [number, harness, source] = fields.as_slice() else {
            panic!("bad sessions.tsv line: {line}");
        };
        let source = source.replacen("~/", &format!("{}/", home.display()), 1);
        let card = match render_one(harness, &source, &home) {
            Ok((card, facts, events)) => {
                std::fs::write(out.join(format!("{number}.facts.json")), facts).expect("facts");
                std::fs::write(out.join(format!("{number}.events.jsonl")), events).expect("events");
                card
            }
            Err(error) => format!("# Session card: {harness}\n\nNo card: {error:#}\n"),
        };
        std::fs::write(out.join(format!("{number}.md")), card).expect("card");
    }
}

fn render_one(harness: &str, source: &str, home: &Path) -> Result<(String, String, String)> {
    let (transcript, signals) = load(harness, source, home)?;
    let digest = extract_transcript_facts(harness, &transcript, &signals);
    let mut events = String::new();
    for (index, event) in transcript.events.iter().enumerate() {
        let line = serde_json::json!({
            "e": index + 1,
            "kind": event.kind.as_str(),
            "role": event.role,
            "content": event.content,
            "metadata": event.metadata,
        });
        events.push_str(&line.to_string());
        events.push('\n');
    }
    Ok((
        render_card(&digest),
        serde_json::to_string_pretty(&digest)?,
        events,
    ))
}

fn load(harness: &str, source: &str, home: &Path) -> Result<(ExportedTranscript, NativeSignals)> {
    match harness {
        "claude" | "codex" => {
            let managed = if harness == "claude" {
                ManagedHarness::Claude
            } else {
                ManagedHarness::Codex
            };
            let path = Path::new(source);
            let (id, _) = session_header(managed, path)?.context("no session header")?;
            let transcript = export_jsonl(managed, path, &id, None)?;
            let signals = read_native_signals(managed, path)?;
            Ok((transcript, signals))
        }
        "hermes" => {
            let (db, id) = source.split_once('#').context("hermes source needs #id")?;
            let hermes_home = Path::new(db)
                .ancestors()
                .find(|dir| dir.file_name().is_some_and(|name| name == ".hermes"))
                .context("hermes db outside a .hermes home")?;
            let discovery = discover_hermes_history(hermes_home, UNIX_EPOCH);
            let session = discovery
                .sessions
                .into_iter()
                .find(|session| {
                    session.native_session_id == id && session.db_path() == Path::new(db)
                })
                .context("hermes session not discovered")?;
            let transcript = export_hermes_session(&session, None)?;
            let signals = hermes_signals(&session)?;
            Ok((transcript, signals))
        }
        "opencode" => {
            let (db, id) = source
                .split_once('#')
                .context("opencode source needs #id")?;
            let transcript = export_opencode(home, None, id, None)?;
            let signals = opencode_signals(Path::new(db), id)?;
            Ok((transcript, signals))
        }
        other => bail!("no native transcript reader for {other} in the P1 importer"),
    }
}
