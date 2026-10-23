//! Hybrid `note_search` against the real embedder and the real model, end to end through the
//! executable. Ignored by default: set `HARNESS_MEMORY_TEST_MODEL_DIR` to a folder that holds
//! `model_quantized.onnx` (or `onnx/model_quantized.onnx`) and `tokenizer.json`, then run
//! `cargo test --test mcp_hybrid_real -- --ignored`.
//!
//! The embedder downloads the model from a local HTTP server that serves those two files, into a
//! temporary data folder, as it would from the release assets.
#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use common::stdio::{BINARY, Client, command, without_team_settings};
use serde_json::{Value, json};
use tempfile::TempDir;

const NOTES: [(&str, &str); 23] = [
    (
        "Login token refresh after sleep",
        "After laptop sleep the OAuth login token expires; refresh it on resume before the first request.",
    ),
    (
        "Cargo clean before commit",
        "Run cargo clean before committing because target/debug grew to 276 GB.",
    ),
    (
        "PostgreSQL connection pool timeout under load",
        "Pool of 4 runs out; a connection waits 30 s then times out. Raise max_size to 16 for PostgreSQL.",
    ),
    (
        "Publishing a desktop release",
        "Run the release workflow; semantic-release tags vX.Y.Z and the publish job writes latest.json once.",
    ),
    (
        "WebKitGTK blink on DMA-BUF renderer",
        "WebKitGTK 2.54 blinks with the DMA-BUF renderer. Dev scripts set WEBKIT_DISABLE_DMABUF_RENDERER=1.",
    ),
    (
        "Flaky tests on Windows runners",
        "Path separators make the snapshot tests flaky on Windows runners; normalise with to_slash.",
    ),
    (
        "Routines scheduler cron parsing",
        "Cron expressions for routines are parsed in store/routines/cron.rs; 5 fields, local time.",
    ),
    (
        "Database schema migration order",
        "Schema migrations run in version order on open; a newer database makes every tool fail.",
    ),
    (
        "Theme tokens for widths",
        "Widths and breakpoints live in tokens.css, never raw px in components.",
    ),
    (
        "Sidebar animation jank",
        "The sidebar slide animation drops frames on WebKitGTK when the transcript is long.",
    ),
    (
        "Updater signature check",
        "The updater verifies minisign signatures from latest.json before installing.",
    ),
    (
        "Keyboard shortcut for panels",
        "Ctrl [ and Ctrl ] toggle the side panels in the workspace layout.",
    ),
    (
        "Hooks run order",
        "Hooks run in the order of their priority; a failing hook blocks the tool call.",
    ),
    (
        "Tray icon on Linux",
        "The tray icon needs libayatana-appindicator; without it the tray is skipped.",
    ),
    (
        "Installer silent flags",
        "The Windows installer accepts /S for a silent install and /D for the folder.",
    ),
    (
        "MCP server startup",
        "The MCP server reads newline-delimited JSON-RPC from stdin and never writes client data to stderr.",
    ),
    (
        "Note quality gate",
        "note_save refuses a note that is too similar to an existing one and names the similar note id.",
    ),
    (
        "Session list pagination",
        "The session list is paginated server side with a filter and a sort, to scale to a thousand rows.",
    ),
    (
        "Team scripts approval",
        "Scripts read from the team database need approval on this computer before they run.",
    ),
    (
        "Embedding model memory",
        "The embedder unloads the model after ten idle minutes to give the memory back.",
    ),
    (
        "Git worktree layout",
        "Feature branches live in sibling worktrees named after the milestone they carry.",
    ),
    (
        "Fish shell quoting",
        "In fish, use set -x for environment variables and avoid bash heredocs.",
    ),
    (
        "Typography scale",
        "Body text is 14 px with a 1.5 line height, headings step up by a fixed ratio.",
    ),
];

/// The folder of the model files, from the environment; `None` (with a note) when it is not set.
fn model_folder() -> Option<PathBuf> {
    let Ok(folder) = std::env::var("HARNESS_MEMORY_TEST_MODEL_DIR") else {
        eprintln!("HARNESS_MEMORY_TEST_MODEL_DIR is not set; skipping");
        return None;
    };

    Some(PathBuf::from(folder))
}

/// A local HTTP server for the two model files; returns the base URL.
fn serve_model(folder: &Path) -> String {
    let onnx = [
        folder.join("model_quantized.onnx"),
        folder.join("onnx/model_quantized.onnx"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .expect("model_quantized.onnx under HARNESS_MEMORY_TEST_MODEL_DIR");
    let files = vec![
        ("/model_quantized.onnx".to_owned(), onnx),
        ("/tokenizer.json".to_owned(), folder.join("tokenizer.json")),
    ];

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("address").port();

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let files = files.clone();
            std::thread::spawn(move || answer(stream, &files));
        }
    });

    format!("http://127.0.0.1:{port}")
}

fn answer(stream: TcpStream, files: &[(String, PathBuf)]) {
    let mut reader = BufReader::new(stream);
    let mut request = String::new();

    if reader.read_line(&mut request).is_err() {
        return;
    }

    let mut header = String::new();

    while reader.read_line(&mut header).is_ok_and(|read| read > 2) {
        header.clear();
    }

    let path = request.split_whitespace().nth(1).unwrap_or_default();
    let mut stream = reader.into_inner();

    let Some((_, file)) = files.iter().find(|(name, _)| name == path) else {
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };

    let Ok(mut file) = std::fs::File::open(file) else {
        return;
    };
    let length = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);

    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = std::io::copy(&mut file, &mut stream);
}

/// `harness-memory embedder <args>` on the data folder `data`.
fn embedder(data: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(BINARY);
    command.arg("embedder").args(args).env("HARNESS_MEMORY_DIR", data);
    without_team_settings(&mut command);
    command
}

/// A running embedder and its events; stopped when dropped.
struct Running {
    child: Child,
    events: Receiver<Value>,
}

impl Running {
    fn start(data: &Path, base_url: &str) -> Self {
        let mut command = embedder(data, &["run"]);
        command
            .env("HARNESS_MEMORY_MODEL_BASE_URL", base_url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = command.spawn().expect("start the embedder");
        let stdout = child.stdout.take().expect("stdout");
        let (sender, events) = mpsc::channel();

        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(event) = serde_json::from_str::<Value>(&line) {
                    let _ = sender.send(event);
                }
            }
        });

        Self { child, events }
    }

    /// Waits until the backfill of every note is done (the model is downloaded and loaded by then).
    fn backfilled(&self, notes: usize) {
        let until = Instant::now() + Duration::from_secs(600);

        while Instant::now() < until {
            let Ok(event) = self.events.recv_timeout(Duration::from_secs(1)) else {
                continue;
            };

            assert_ne!(event["event"], "error", "the embedder failed: {event}");

            let done = event["done"].as_u64().unwrap_or(0);
            let total = event["total"].as_u64().unwrap_or(0);

            if event["event"] == "backfill" && total as usize >= notes && done == total {
                return;
            }
        }

        panic!("the backfill did not finish");
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

fn vectors(data: &Path) -> u64 {
    let output = embedder(data, &["status", "--json"]).output().expect("status");
    let status: Value = serde_json::from_slice(&output.stdout).expect("status JSON");

    status["vectors"].as_u64().expect("vectors")
}

fn search(client: &mut Client, query: &str) -> String {
    client.call("note_search", json!({ "query": query, "scope": "global" }))
}

#[test]
#[ignore = "needs the model files: set HARNESS_MEMORY_TEST_MODEL_DIR"]
fn the_real_model_finds_synonyms_and_other_languages_and_leaves_absent_topics_empty() {
    let Some(folder) = model_folder() else {
        return;
    };

    let dir = TempDir::new().expect("temp dir");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).expect("data folder");

    let mut writer = Client::start(command(&data, dir.path()));

    for (title, body) in NOTES {
        writer.call(
            "note_save",
            json!({ "title": title, "body": body, "kind": "note", "scope": "global" }),
        );
    }

    let enabled = embedder(&data, &["enable"]).output().expect("enable");
    assert!(enabled.status.success());

    let base_url = serve_model(&folder);
    let running = Running::start(&data, &base_url);
    running.backfilled(NOTES.len());

    // A fresh session: its first search would carry the notice if the embedder did not answer.
    let mut client = Client::start(command(&data, dir.path()));
    let unavailable = "vector search unavailable";

    let synonym = search(&mut client, "authentication expiry");
    assert!(!synonym.contains(unavailable), "{synonym}");
    assert!(synonym.contains("Login token refresh after sleep"), "{synonym}");

    let polish = search(&mut client, "token logowania wygasa po uśpieniu laptopa");
    assert!(polish.contains("Login token refresh after sleep"), "{polish}");

    for absent in ["what is the zebra migration plan", "kubernetes helm chart deployment"] {
        assert_eq!(search(&mut client, absent), "no matching notes", "{absent}");
    }

    // A note saved now is sent to the embedder at once.
    let before = vectors(&data);
    client.call(
        "note_save",
        json!({
            "title": "Marmalade labels",
            "body": "Jars of orange preserve get a dated label with the batch number.",
            "kind": "note",
            "scope": "global",
        }),
    );

    let until = Instant::now() + Duration::from_secs(15);

    while vectors(&data) <= before && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(200));
    }

    assert_eq!(vectors(&data), before + 1, "the new note was embedded");

    let jam = search(&mut client, "dated stickers on jars of orange jam");
    assert!(jam.contains("Marmalade labels"), "{jam}");

    drop(running);
}
