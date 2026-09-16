//! End-to-end cover for `ai-memory backfill` (G3).
//!
//! The unit tests next to the command (`commands::backfill::tests`) only
//! assert how clap parses `--force` / `--project`. What they cannot see is
//! the thing the subcommand exists for: a store whose pages predate the
//! section/passage index (G1) has no `page_sections` / `page_passages` rows
//! at all, and before this command there was no operator-reachable way to
//! build them — `ops::backfill_sections` was live code nothing called.
//!
//! So this test drives the real binary against a real server over HTTP:
//! fixture pages whose section index has been wiped (the pre-G1 shape),
//! `backfill --dry-run` to prove it reports without mutating, then a live
//! `backfill` to prove the index comes back.
//!
//! Passage *embeddings* are deliberately not asserted here: the writer path
//! `backfill_sections` drives is index-only, and `page_passage_embeddings`
//! is filled by the separate embedding path (`ai-memory embed`), which
//! needs a configured provider this hermetic test does not have.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use ai_memory_core::{NewPage, PagePath, Tier};
use ai_memory_store::Store;
use ai_memory_store::create_explicit_scope;
use rusqlite::Connection;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
const WORKSPACE: &str = "backfill-ws";
const PROJECT: &str = "backfill-proj";

/// A page body with two headings, so a correct backfill produces more
/// than one section and the passage splitter has real text to chunk.
const RUNBOOK: &str = "# Deploy runbook\n\n\
    The release job redeploys the staging cluster every night.\n\n\
    ## Rollback\n\n\
    Roll back by redeploying the previously published artifact tag.\n";

const NOTES: &str = "# Retrieval notes\n\n\
    Section-level retrieval scores passages, not whole pages.\n\n\
    ## Ranking\n\n\
    Reciprocal rank fusion merges the lexical and dense streams.\n";

/// Reserve a loopback port, then hand it back. Binding `127.0.0.1:0` in
/// the server would be tidier, but the test has to know the port before
/// the child exists in order to point the client at it, and parsing it
/// back out of an ANSI-coloured log line is the more brittle half of
/// that trade.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserving a loopback port")
        .local_addr()
        .expect("reading the reserved port")
        .port()
}

/// A spawned `ai-memory serve --transport http`, killed on drop.
struct Server {
    child: Child,
    port: u16,
    /// Held open only so the child never sees stdin EOF.
    _stdin: ChildStdin,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    /// Start a server on `data_dir` and block until it logs that its
    /// HTTP listener is up.
    fn start(data_dir: &Path) -> Self {
        let port = free_port();
        let mut child = Command::new(BIN)
            .args(["serve", "--transport", "http", "--bind"])
            .arg(format!("127.0.0.1:{port}"))
            // The fixture writes pages straight into SQLite and never
            // touches `wiki/`, so there is nothing for the watcher to
            // observe and no reason to pay for it.
            .arg("--no-watcher")
            .arg("--data-dir")
            .arg(data_dir)
            .env("AI_MEMORY_DATA_DIR", data_dir)
            // Hermetic: the default provider would start a model
            // download the moment the server builds its embedder.
            .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
            .env("RUST_LOG", "info")
            .env("HOME", data_dir)
            .env_remove("AI_MEMORY_HOME")
            .env_remove("AI_MEMORY_AUTH_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawning ai-memory serve");

        let stdin = child.stdin.take().expect("serve stdin");
        let stderr = child.stderr.take().expect("serve stderr");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        let server = Self {
            child,
            port,
            _stdin: stdin,
        };
        server.await_ready(&rx);
        server
    }

    /// Wait for the readiness line. Migrations run before the listener
    /// binds, so a connect-poll would report "up" on a port the OS had
    /// accepted but the router had not yet been mounted behind.
    fn await_ready(&self, lines: &Receiver<String>) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            match lines.recv_timeout(Duration::from_secs(1)) {
                Ok(line) if line.contains("MCP HTTP server ready") => return,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("serve exited before it reported an HTTP listener")
                }
            }
        }
        panic!("serve never reported an HTTP listener within 60s");
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// Write the fixture pages through the real write path, then drop the
/// store so the server owns the database alone.
fn seed_pages(data_dir: &Path) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async {
        let store = Store::open(data_dir).expect("opening the fixture store");
        let scope = create_explicit_scope(&store.writer, WORKSPACE, PROJECT)
            .await
            .expect("creating the fixture scope");
        for (path, title, body) in [
            ("runbooks/deploy.md", "Deploy runbook", RUNBOOK),
            ("notes/retrieval.md", "Retrieval notes", NOTES),
        ] {
            store
                .writer
                .upsert_page(NewPage {
                    workspace_id: scope.workspace_id,
                    project_id: scope.project_id,
                    path: PagePath::new(path).expect("fixture page path"),
                    title: title.to_string(),
                    body: body.to_string(),
                    tier: Tier::Working,
                    frontmatter_json: serde_json::json!({}),
                    pinned: false,
                    links: Vec::new(),
                    author_id: None,
                    expires_at: None,
                    entities: Vec::new(),
                    evidence: Vec::new(),
                })
                .await
                .expect("writing a fixture page");
        }
    });
}

fn db_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("db").join("memory.sqlite")
}

fn open_db(data_dir: &Path) -> Connection {
    Connection::open(db_path(data_dir)).expect("opening the fixture database")
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap_or_else(|e| panic!("counting {table}: {e}"))
}

/// Turn post-G1 pages back into the pre-G1 shape: rows in `pages`, no
/// section index. This is what a store written before the G1 migration
/// actually looks like, and the only way to produce it — every write
/// path builds sections in the same transaction as the page (invariant
/// §3), so there is no "write a page without sections" API to call.
///
/// Passages go first: `PRAGMA foreign_keys` is off by default on a
/// fresh connection, so the `ON DELETE CASCADE` from `page_sections`
/// cannot be relied on here. Deleting them through SQL also fires the
/// FTS delete trigger, so `page_passages_fts` is emptied too.
fn wipe_section_index(data_dir: &Path) {
    let conn = open_db(data_dir);
    conn.execute("DELETE FROM page_passages", [])
        .expect("deleting fixture passages");
    conn.execute("DELETE FROM page_sections", [])
        .expect("deleting fixture sections");
}

/// Run the `backfill` subcommand against `server`.
fn run_backfill(data_dir: &Path, server: &Server, extra: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("backfill")
        .args(["--workspace", WORKSPACE, "--project", PROJECT])
        .args(extra)
        .env("AI_MEMORY_DATA_DIR", data_dir)
        .env("AI_MEMORY_SERVER_URL", server.url())
        .env("HOME", data_dir)
        .env_remove("AI_MEMORY_HOME")
        .env_remove("AI_MEMORY_AUTH_TOKEN")
        .output()
        .expect("running ai-memory backfill")
}

fn stdout_of(output: &std::process::Output) -> String {
    assert!(
        output.status.success(),
        "backfill failed: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The whole point of the subcommand: pages that predate the section
/// index get one, and `--dry-run` reports what it would do without
/// writing a row.
///
/// Both halves share one server because the dry-run assertion is only
/// meaningful against the same untouched fixture the live run then
/// repairs — and because a second server start would double the
/// wall-clock for no extra coverage.
#[test]
fn backfill_rebuilds_the_section_index_for_pre_g1_pages() {
    let data_dir = TempDir::new().expect("tempdir");
    seed_pages(data_dir.path());

    // Sanity: the write path indexed the fixture, so a later non-zero
    // count cannot be mistaken for rows that were there all along.
    let conn = open_db(data_dir.path());
    assert!(
        count(&conn, "page_sections") > 0 && count(&conn, "page_passages") > 0,
        "the fixture write path should have built a section index to wipe"
    );
    drop(conn);

    wipe_section_index(data_dir.path());
    let conn = open_db(data_dir.path());
    assert_eq!(count(&conn, "page_sections"), 0);
    assert_eq!(count(&conn, "page_passages"), 0);
    assert_eq!(count(&conn, "pages"), 2, "the pages themselves must stay");
    drop(conn);

    let server = Server::start(data_dir.path());

    // --- dry run: report, mutate nothing --------------------------
    let dry = stdout_of(&run_backfill(data_dir.path(), &server, &["--dry-run"]));
    assert!(
        dry.contains("dry-run: would backfill 2 page(s)"),
        "dry-run should report both latest pages, got: {dry}"
    );
    let conn = open_db(data_dir.path());
    assert_eq!(
        count(&conn, "page_sections"),
        0,
        "--dry-run must not write sections"
    );
    assert_eq!(
        count(&conn, "page_passages"),
        0,
        "--dry-run must not write passages"
    );
    drop(conn);

    // --- live run: the index comes back ---------------------------
    let live = stdout_of(&run_backfill(data_dir.path(), &server, &[]));
    assert!(
        live.contains("backfilled 2 page(s)"),
        "live run should report both pages, got: {live}"
    );

    let conn = open_db(data_dir.path());
    // Two headings per fixture page.
    assert_eq!(
        count(&conn, "page_sections"),
        4,
        "each fixture page has two headings"
    );
    assert!(
        count(&conn, "page_passages") >= 4,
        "every section must yield at least one passage"
    );
    // Sections are attributed to both pages, not just whichever the
    // batch loop happened to reach first.
    let pages_with_sections: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT page_id) FROM page_sections",
            [],
            |row| row.get(0),
        )
        .expect("counting pages with sections");
    assert_eq!(pages_with_sections, 2);

    // The lexical index the backfill exists to feed is queryable, not
    // merely populated — the FTS triggers ran on the writer's inserts.
    let hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM page_passages_fts WHERE page_passages_fts MATCH 'rollback'",
            [],
            |row| row.get(0),
        )
        .expect("querying page_passages_fts");
    assert!(
        hits > 0,
        "a term from a backfilled passage should be findable in page_passages_fts"
    );
}
