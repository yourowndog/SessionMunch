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
