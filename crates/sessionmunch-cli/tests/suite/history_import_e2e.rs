//! End-to-end P1 exit condition: "when did we run X on <machine>?" answered
//! from preserved evidence. A real `serve` hub, a real `import sessions` run
//! against a native Claude Code transcript store, and `search-events` over
//! the result — no LLM, no summary in between.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const NATIVE_ID: &str = "4f6e1a52-0000-4000-8000-0000000000e2";
const PROBE: &str = "zebraquokka7731";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sessionmunch")
}

/// A child process with no inherited sessionmunch environment.
fn child(data_dir: &Path, home: &Path, server_url: &str) -> Command {
    let mut cmd = Command::new(bin());
    for (key, _) in std::env::vars() {
        if key.starts_with("SESSIONMUNCH_") || key.starts_with("AI_MEMORY_") {
            cmd.env_remove(&key);
        }
    }
    cmd.env("HOME", home)
        .env("USERPROFILE", home)
        .env("SESSIONMUNCH_DATA_DIR", data_dir)
        .env("SESSIONMUNCH_SERVER_URL", server_url)
        .env("SESSIONMUNCH_IGNORE_MARKER", "1");
    cmd
}

struct Hub(Child);

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start_hub(data_dir: &Path, home: &Path) -> (Hub, String) {
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let hub = child(data_dir, home, &url)
        .args(["serve", "--transport", "http", "--bind"])
        .arg(format!("127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn serve");
    let hub = Hub(hub);
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return (hub, url);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("hub never started listening on {url}");
}

fn write_claude_transcript(home: &Path) {
    let dir = home.join(".claude/projects/-work-probe-repo");
    std::fs::create_dir_all(&dir).unwrap();
    let lines = [
        serde_json::json!({
            "type": "user", "sessionId": NATIVE_ID, "cwd": "/work/probe-repo",
            "uuid": "u1", "timestamp": "2026-10-19T00:00:00Z",
            "message": { "role": "user", "content": format!("please run the {PROBE} check") }
        }),
        serde_json::json!({
            "type": "assistant", "sessionId": NATIVE_ID, "cwd": "/work/probe-repo",
            "uuid": "a1", "timestamp": "2026-10-19T00:00:01Z",
            "message": { "role": "assistant", "content": [
                { "type": "tool_use", "id": "toolu_1", "name": "Bash",
                  "input": { "command": format!("cargo nextest run {PROBE}") } }
            ]}
        }),
    ];
    let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(dir.join(format!("{NATIVE_ID}.jsonl")), body).unwrap();
}

fn run_json(cmd: &mut Command) -> serde_json::Value {
    let output = cmd.output().expect("run sessionmunch");
    assert!(
        output.status.success(),
        "command failed: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "non-JSON stdout ({e}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn imported_transcript_answers_when_did_we_run_x() {
    let tmp = tempfile::tempdir().unwrap();
    let hub_data = tmp.path().join("hub");
    let node_data = tmp.path().join("node");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    write_claude_transcript(&home);
    let (_hub, url) = start_hub(&hub_data, &home);

    let dry = run_json(
        child(&node_data, &home, &url)
            .args([
                "import",
                "sessions",
                "--since",
                "all",
                "--harness",
                "claude",
            ])
            .args(["--dry-run", "--json"]),
    );
    assert_eq!(dry["harnesses"][0]["sessions_found"], 1, "{dry}");
    assert_eq!(dry["events_delivered"], 0, "a dry run sends nothing: {dry}");

    let first = run_json(child(&node_data, &home, &url).args([
        "import",
        "sessions",
        "--since",
        "all",
        "--harness",
        "claude",
        "--json",
    ]));
    let queued = first["harnesses"][0]["events_queued"].as_u64().unwrap();
    assert!(queued >= 2, "{first}");
    assert_eq!(first["events_delivered"].as_u64(), Some(queued), "{first}");
    assert_eq!(first["batches_pending"], 0, "{first}");
    let machine = first["machine"].as_str().unwrap().to_owned();

    // The local cursor skips the tail already delivered.
    let second = run_json(child(&node_data, &home, &url).args([
        "import",
        "sessions",
        "--since",
        "all",
        "--harness",
        "claude",
        "--json",
    ]));
    assert_eq!(second["harnesses"][0]["events_queued"], 0, "{second}");

    // Losing the cursor re-sends everything; the hub keeps one copy.
    std::fs::remove_file(node_data.join("history-import/cursors.json")).unwrap();
    let replay = run_json(child(&node_data, &home, &url).args([
        "import",
        "sessions",
        "--since",
        "all",
        "--harness",
        "claude",
        "--json",
    ]));
    assert_eq!(replay["events_delivered"], 0, "{replay}");
    assert_eq!(
        replay["events_already_known"].as_u64(),
        Some(queued),
        "{replay}"
    );

    let hits = run_json(child(&node_data, &home, &url).args([
        "search-events",
        PROBE,
        "--machine",
        &machine,
        "--json",
    ]));
    let hits = hits.as_array().unwrap();
    assert_eq!(hits.len(), queued as usize, "{hits:?}");
    for hit in hits {
        assert_eq!(hit["machine_id"], machine.as_str());
        assert_eq!(hit["harness"], "claude-code");
        assert_eq!(hit["project"], "probe-repo");
        assert_eq!(
            hit["session_id"], NATIVE_ID,
            "imports share the hook session id"
        );
        assert_eq!(hit["source"], "import");
    }
    assert!(
        hits.iter().any(|hit| hit["content"]
            .as_str()
            .unwrap()
            .contains("cargo nextest run")),
        "the command itself is searchable: {hits:?}"
    );

    let elsewhere = run_json(child(&node_data, &home, &url).args([
        "search-events",
        PROBE,
        "--machine",
        "some-other-node",
        "--json",
    ]));
    assert_eq!(elsewhere.as_array().unwrap().len(), 0);
}

/// A snapshot outlives its live store: once the native transcript is gone,
/// `import sessions --from-snapshots` still delivers it, and the payload the
/// import cut to a head and tail comes back whole from `artifact get`.
#[test]
fn snapshot_imports_after_the_live_store_is_gone_and_artifacts_read_back() {
    let tmp = tempfile::tempdir().unwrap();
    let hub_data = tmp.path().join("hub");
    let node_data = tmp.path().join("node");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    write_claude_transcript(&home);
    let big_output = format!("{PROBE} {}", "x".repeat(40 * 1024));
    let transcript = home.join(format!(
        ".claude/projects/-work-probe-repo/{NATIVE_ID}.jsonl"
    ));
    let mut body = std::fs::read_to_string(&transcript).unwrap();
    body.push_str(&format!(
        "{}\n",
        serde_json::json!({
            "type": "user", "sessionId": NATIVE_ID, "cwd": "/work/probe-repo",
            "uuid": "u2", "timestamp": "2026-10-19T00:00:02Z",
            "message": { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "toolu_1", "content": big_output }
            ]}
        })
    ));
    std::fs::write(&transcript, body).unwrap();
    let (_hub, url) = start_hub(&hub_data, &home);

    run_json(child(&node_data, &home, &url).args([
        "snapshot-harness-history",
        "--harness",
        "claude",
        "--json",
    ]));
    std::fs::remove_dir_all(home.join(".claude")).unwrap();

    let imported = run_json(child(&node_data, &home, &url).args([
        "import",
        "sessions",
        "--from-snapshots",
        "--since",
        "all",
        "--harness",
        "claude",
        "--json",
    ]));
    assert_eq!(imported["harnesses"][0]["sessions_found"], 1, "{imported}");
    assert_eq!(
        imported["harnesses"][0]["artifacts_uploaded"], 1,
        "{imported}"
    );
    let leftovers: Vec<_> = std::fs::read_dir(node_data.join("history-import"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("snapshot-home")
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "the staged home is removed: {leftovers:?}"
    );

    let hits = run_json(child(&node_data, &home, &url).args(["search-events", PROBE, "--json"]));
    let digest = hits
        .as_array()
        .unwrap()
        .iter()
        .find_map(|hit| hit["metadata"]["artifact_sha256"].as_str())
        .unwrap_or_else(|| panic!("the cut event names its artifact: {hits}"))
        .to_owned();
    let output = child(&node_data, &home, &url)
        .args(["artifact", "get", &digest])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), big_output);
}

/// A scheduled and a manual import on one machine share the same on-disk
/// queue. A second writer must refuse before opening the spool, but a
/// read-only preview remains available while the first holds the lock.
#[test]
fn history_import_lock_refuses_concurrent_writer_but_allows_dry_run() {
    let tmp = tempfile::tempdir().unwrap();
    let node_data = tmp.path().join("node");
    let import_dir = node_data.join("history-import");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&import_dir).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    write_claude_transcript(&home);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(import_dir.join(".import.lock"))
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&file).unwrap();
    let url = "http://127.0.0.1:1";
    let args = [
        "import",
        "sessions",
        "--since",
        "all",
        "--harness",
        "claude",
        "--json",
    ];
    let refused = child(&node_data, &home, url).args(args).output().unwrap();
    assert!(!refused.status.success(), "second writer must be refused");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("history import lock"),
        "{refused:?}"
    );
    assert!(!import_dir.join("spool").exists());
    assert!(!import_dir.join("cursors.json").exists());
    let dry = run_json(child(&node_data, &home, url).args(args).arg("--dry-run"));
    assert_eq!(dry["harnesses"][0]["sessions_found"], 1, "{dry}");
    assert_eq!(dry["events_delivered"], 0, "{dry}");
    assert!(!import_dir.join("spool").exists());
    assert!(!import_dir.join("cursors.json").exists());
}

/// Hosted chat files enter the same real ledger and artifact pipeline as native
/// transcripts. Replay from another machine's local state remains idempotent.
#[test]
fn hosted_chat_exports_are_searchable_and_account_scoped() {
    let tmp = tempfile::tempdir().unwrap();
    let hub_data = tmp.path().join("hub");
    let node_data = tmp.path().join("node");
    let second_data = tmp.path().join("second-node");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (hub, url) = start_hub(&hub_data, &home);
    let file = tmp.path().join("conversations.json");
    let user = "22222222-2222-4222-8222-222222222222";
    let assistant = "33333333-3333-4333-8333-333333333333";
    let cid = "11111111-1111-4111-8111-111111111111";
    let visible = format!("hostedchatcanary {} tail", "z".repeat(40 * 1024));
    std::fs::write(&file, serde_json::to_vec(&serde_json::json!([{
        "id":cid, "title":"synthetic chat", "current_node":assistant,
        "mapping":{
            (user):{"parent":null,"message":{"id":user,"author":{"role":"user"},"content":{"content_type":"text","parts":["private fixture question"]}}},
            (assistant):{"parent":user,"message":{"id":assistant,"author":{"role":"assistant"},"recipient":"all","content":{"content_type":"text","parts":[visible]}}}
        }
    }])).unwrap()).unwrap();
    let import = |data: &Path, account: &str, dry: bool| {
        let mut cmd = child(data, &home, &url);
        cmd.args([
            "import",
            "chats",
            "--provider",
            "chatgpt",
            "--account",
            account,
            "--workspace",
            "private-history",
            "--project",
            "hosted",
            "--json",
        ])
        .arg(&file);
        if dry {
            cmd.arg("--dry-run");
        }
        run_json(&mut cmd)
    };
    let dry = import(&node_data, "fixture", true);
    assert_eq!(dry["harnesses"][0]["events_queued"], 2, "{dry}");
    assert_eq!(dry["events_delivered"], 0, "{dry}");
    assert!(!node_data.join("history-import").exists());
    let first = import(&node_data, "fixture", false);
    assert_eq!(first["events_delivered"], 2, "{first}");
    assert_eq!(first["harnesses"][0]["artifacts_uploaded"], 1, "{first}");
    let replay = import(&second_data, "fixture", false);
    assert_eq!(replay["events_delivered"], 0, "{replay}");
    assert_eq!(replay["events_already_known"], 2, "{replay}");
    let same = import(&node_data, "fixture", false);
    assert_eq!(same["harnesses"][0]["events_queued"], 0, "{same}");
    let separate = import(&node_data, "other-account", false);
    assert_eq!(separate["events_delivered"], 2, "{separate}");
    let hits = run_json(child(&node_data, &home, &url).args([
        "search-events",
        "hostedchatcanary",
        "--harness",
        "chatgpt",
        "--json",
    ]));
    let hits = hits.as_array().unwrap();
    assert_eq!(hits.len(), 2, "{hits:?}");
    assert_ne!(hits[0]["session_id"], hits[1]["session_id"]);
    let digest = hits[0]["metadata"]["artifact_sha256"].as_str().unwrap();
    let output = child(&node_data, &home, &url)
        .args(["artifact", "get", digest])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), visible);

    let claude = tmp.path().join("claude-conversations.json");
    std::fs::write(&claude,serde_json::to_vec(&serde_json::json!([{
        "uuid":cid,"name":"synthetic Claude","chat_messages":[
            {"uuid":user,"sender":"human","text":"claudehostedcanary"},
            {"uuid":assistant,"sender":"assistant","content":[{"type":"text","text":"claudehostedcanary response"},{"type":"thinking","thinking":"hiddenreasoningcanary"}]}
        ]
    }])).unwrap()).unwrap();
    let claude_result = run_json(
        child(&node_data, &home, &url)
            .args([
                "import",
                "chats",
                "--provider",
                "claude",
                "--account",
                "fixture",
                "--workspace",
                "private-history",
                "--project",
                "hosted",
                "--json",
            ])
            .arg(claude),
    );
    assert_eq!(claude_result["events_delivered"], 2, "{claude_result}");
    let hits = run_json(child(&node_data, &home, &url).args([
        "search-events",
        "claudehostedcanary",
        "--harness",
        "claude-chat",
        "--json",
    ]));
    assert_eq!(hits.as_array().unwrap().len(), 2, "{hits}");
    let hidden = run_json(child(&node_data, &home, &url).args([
        "search-events",
        "hiddenreasoningcanary",
        "--json",
    ]));
    assert_eq!(hidden.as_array().unwrap().len(), 0, "{hidden}");
    drop(hub);
}

const WINDOW_ID: &str = "4f6e1a52-0000-4000-8000-0000000000e3";

fn days_ago(days: i64) -> jiff::Timestamp {
    jiff::Timestamp::now() - jiff::SignedDuration::from_hours(24 * days)
}

/// One recent, one 40-day-old and one untimestamped visible user message in
/// a transcript whose file was modified now (so `--since` selects it).
fn write_mixed_age_transcript(home: &Path) {
    let dir = home.join(".claude/projects/-work-window-repo");
    std::fs::create_dir_all(&dir).unwrap();
    let line = |uuid: &str, timestamp: Option<String>, text: &str| {
        let mut value = serde_json::json!({
            "type": "user", "sessionId": WINDOW_ID, "cwd": "/work/window-repo",
            "uuid": uuid, "message": { "role": "user", "content": text }
        });
        if let Some(timestamp) = timestamp {
            value["timestamp"] = timestamp.into();
        }
        format!("{value}\n")
    };
    let body = [
        line("old", Some(days_ago(40).to_string()), "windowold canary"),
        line("unknown", None, "windowunknown canary"),
        line(
            "recent",
            Some(days_ago(1).to_string()),
            "windowrecent canary",
        ),
    ]
    .concat();
    std::fs::write(dir.join(format!("{WINDOW_ID}.jsonl")), body).unwrap();
}

fn search_count(data: &Path, home: &Path, url: &str, query: &str) -> usize {
    run_json(child(data, home, url).args(["search-events", query, "--json"]))
        .as_array()
        .unwrap()
        .len()
}

/// Gate 4: a bounded `--since` filters each event by its original source
/// time against one fixed invocation cutoff, never by file activity. Old and
/// untimestamped events are counted and withheld, the session cursor is held
/// so a later wider window can still import them, and replays add no
/// duplicate ledger rows.
#[test]
fn bounded_since_is_a_strict_per_event_cutoff() {
    let tmp = tempfile::tempdir().unwrap();
    let hub_data = tmp.path().join("hub");
    let node_data = tmp.path().join("node");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    write_mixed_age_transcript(&home);
    let (_hub, url) = start_hub(&hub_data, &home);
    let import = |since: &str, dry: bool| {
        let mut cmd = child(&node_data, &home, &url);
        cmd.args([
            "import",
            "sessions",
            "--since",
            since,
            "--harness",
            "claude",
            "--json",
        ]);
        if dry {
            cmd.arg("--dry-run");
        }
        run_json(&mut cmd)
    };

    let dry = import("5w", true);
    let harness = &dry["harnesses"][0];
    assert_eq!(harness["sessions_found"], 1, "{dry}");
    assert_eq!(harness["events_queued"], 1, "{dry}");
    assert_eq!(harness["cutoff_excluded"]["before_cutoff"], 1, "{dry}");
    assert_eq!(harness["cutoff_excluded"]["unknown_time"], 1, "{dry}");
    let cutoff: jiff::Timestamp = dry["cutoff"].as_str().unwrap().parse().unwrap();
    let expected = days_ago(35);
    assert!(
        cutoff.duration_since(expected).abs() < jiff::SignedDuration::from_mins(5),
        "cutoff {cutoff} is the fixed invocation instant minus 35 days"
    );
    assert!(!node_data.join("history-import/spool").exists());

    let first = import("5w", false);
    assert_eq!(first["events_delivered"], 1, "{first}");
    assert_eq!(first["harnesses"][0]["cursors_held"], 1, "{first}");
    assert_eq!(search_count(&node_data, &home, &url, "windowrecent"), 1);
    assert_eq!(search_count(&node_data, &home, &url, "windowold"), 0);
    assert_eq!(search_count(&node_data, &home, &url, "windowunknown"), 0);
    let cursors = std::fs::read_to_string(node_data.join("history-import/cursors.json")).unwrap();
    assert!(
        !cursors.contains(WINDOW_ID),
        "a straddling session keeps its prior cursor: {cursors}"
    );

    // Same window again: same selection, the hub already holds it.
    let replay = import("5w", false);
    assert_eq!(replay["harnesses"][0]["events_queued"], 1, "{replay}");
    assert_eq!(replay["events_delivered"], 0, "{replay}");
    assert_eq!(replay["events_already_known"], 1, "{replay}");
    assert_eq!(search_count(&node_data, &home, &url, "windowrecent"), 1);

    // The withheld originals stay importable by an explicit wider window.
    let all = import("all", false);
    assert_eq!(all["events_delivered"], 2, "{all}");
    assert!(all.get("cutoff").is_none(), "{all}");
    assert_eq!(search_count(&node_data, &home, &url, "windowold"), 1);
    assert_eq!(search_count(&node_data, &home, &url, "windowunknown"), 1);
    assert_eq!(search_count(&node_data, &home, &url, "windowrecent"), 1);
}

fn queue_batch(node_data: &Path, occurred_at: Option<String>, text: &str) {
    let spool = sessionmunch_workstream::Spool::open(&sessionmunch_workstream::SpoolConfig {
        dir: node_data.join("history-import/spool"),
        max_spool_bytes: 0,
    })
    .unwrap();
    let event = sessionmunch_core::NewSessionEvent {
        event_id: format!("queued:{text}"),
        kind: sessionmunch_core::WorkstreamEventKind::Message,
        role: Some("user".into()),
        content: text.into(),
        occurred_at,
        source: sessionmunch_core::SessionSource::Import,
        metadata: serde_json::json!({}),
    };
    let batch = sessionmunch_workstream::SpoolBatch::new(
        "default",
        "queued-repo",
        "4f6e1a52-0000-4000-8000-0000000000e4",
        "claude-code",
        vec![event],
    );
    spool.enqueue(&batch).unwrap();
}

/// Gate 4: a bounded import drains the whole spool, so it must refuse,
/// before queueing or sending anything, when an earlier wider import left
/// out-of-window batches behind. In-window leftovers still drain.
#[test]
fn bounded_since_refuses_to_drain_older_queued_batches() {
    let tmp = tempfile::tempdir().unwrap();
    let hub_data = tmp.path().join("hub");
    let node_data = tmp.path().join("node");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    write_mixed_age_transcript(&home);
    let (_hub, url) = start_hub(&hub_data, &home);
    queue_batch(
        &node_data,
        Some(days_ago(60).to_string()),
        "queuedold canary",
    );
    let args = [
        "import",
        "sessions",
        "--since",
        "5w",
        "--harness",
        "claude",
        "--json",
    ];

    let refused = child(&node_data, &home, &url).args(args).output().unwrap();
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("outside the --since window"),
        "{refused:?}"
    );
    assert!(!node_data.join("history-import/cursors.json").exists());
    assert_eq!(search_count(&node_data, &home, &url, "queuedold"), 0);
    assert_eq!(search_count(&node_data, &home, &url, "windowrecent"), 0);
    let pending = sessionmunch_workstream::Spool::open(&sessionmunch_workstream::SpoolConfig {
        dir: node_data.join("history-import/spool"),
        max_spool_bytes: 0,
    })
    .unwrap()
    .pending()
    .unwrap();
    assert_eq!(pending.len(), 1, "the older batch stays queued untouched");

    // An untimestamped leftover is refused the same way.
    let other = tmp.path().join("other");
    queue_batch(&other, None, "queuedunknown canary");
    let refused = child(&other, &home, &url).args(args).output().unwrap();
    assert!(!refused.status.success(), "{refused:?}");

    // In-window leftovers are this window's own work and drain normally.
    let fresh = tmp.path().join("fresh");
    queue_batch(&fresh, Some(days_ago(2).to_string()), "queuedfresh canary");
    let ok = run_json(child(&fresh, &home, &url).args(args));
    assert_eq!(ok["events_delivered"], 2, "{ok}");
    assert_eq!(search_count(&fresh, &home, &url, "queuedfresh"), 1);
}

/// Gate 4 for hosted exports: `--since` is the same strict per-message cutoff
/// on the provider's own message time; untimestamped messages are withheld.
#[test]
fn hosted_chat_since_is_a_strict_per_message_cutoff() {
    let tmp = tempfile::tempdir().unwrap();
    let hub_data = tmp.path().join("hub");
    let node_data = tmp.path().join("node");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (_hub, url) = start_hub(&hub_data, &home);
    let file = tmp.path().join("conversations.json");
    let old = "22222222-2222-4222-8222-222222222222";
    let new = "33333333-3333-4333-8333-333333333333";
    let unknown = "44444444-4444-4444-8444-444444444444";
    let seconds = |at: jiff::Timestamp| at.as_millisecond() as f64 / 1000.0;
    std::fs::write(&file, serde_json::to_vec(&serde_json::json!([{
        "id":"11111111-1111-4111-8111-111111111111", "title":"synthetic", "current_node":unknown,
        "mapping":{
            (old):{"parent":null,"message":{"id":old,"create_time":seconds(days_ago(40)),"author":{"role":"user"},"content":{"content_type":"text","parts":["hostedold canary"]}}},
            (new):{"parent":old,"message":{"id":new,"create_time":seconds(days_ago(1)),"author":{"role":"assistant"},"recipient":"all","content":{"content_type":"text","parts":["hostednew canary"]}}},
            (unknown):{"parent":new,"message":{"id":unknown,"author":{"role":"user"},"content":{"content_type":"text","parts":["hostedunknown canary"]}}}
        }
    }])).unwrap()).unwrap();
    let import = |dry: bool| {
        let mut cmd = child(&node_data, &home, &url);
        cmd.args([
            "import",
            "chats",
            "--provider",
            "chatgpt",
            "--account",
            "fixture",
            "--workspace",
            "private-history",
            "--project",
            "hosted",
            "--since",
            "5w",
            "--json",
        ])
        .arg(&file);
        if dry {
            cmd.arg("--dry-run");
        }
        run_json(&mut cmd)
    };
    let dry = import(true);
    let harness = &dry["harnesses"][0];
    assert_eq!(harness["events_queued"], 1, "{dry}");
    assert_eq!(harness["cutoff_excluded"]["before_cutoff"], 1, "{dry}");
    assert_eq!(harness["cutoff_excluded"]["unknown_time"], 1, "{dry}");
    assert!(dry["cutoff"].is_string(), "{dry}");
    let first = import(false);
    assert_eq!(first["events_delivered"], 1, "{first}");
    assert_eq!(search_count(&node_data, &home, &url, "hostednew"), 1);
    assert_eq!(search_count(&node_data, &home, &url, "hostedold"), 0);
    assert_eq!(search_count(&node_data, &home, &url, "hostedunknown"), 0);
    let replay = import(false);
    assert_eq!(replay["events_delivered"], 0, "{replay}");
    assert_eq!(search_count(&node_data, &home, &url, "hostednew"), 1);
}
