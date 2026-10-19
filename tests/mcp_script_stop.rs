//! Stopping a running script through the real `harness-memory mcp`: the client cancels the
//! request, closes the server's input, or sends it SIGTERM. Each time the script and what it
//! started in the background are killed and its script file is removed. A stop while the server
//! waits for the user's approval ends the wait at once, and the script never runs.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::approval::Fixture as ApprovalFixture;
use common::stdio::{Client, command};
use serde_json::{Value, json};
use tempfile::TempDir;

/// How long a stopped script may take to be gone.
const LIMIT: Duration = Duration::from_secs(5);

/// A server with a global script `hang` that starts `sleep 30` in the background, writes its pid
/// and its own path to the two files it gets, and waits.
struct Fixture {
    dir: TempDir,
    client: Client,
}

impl Fixture {
    fn start() -> Self {
        let dir = TempDir::new().expect("temp");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("work");

        let mut client = Client::start(command(&dir.path().join("memory"), &work));
        client.call(
            "script_save",
            json!({ "scope": "global", "name": "hang", "description": "", "language": "sh", "timeout_secs": 120,
                    "body": "sleep 30 &\necho $! > \"$1\"\nprintf '%s' \"$0\" > \"$2\"\nwait\n" }),
        );

        Self { dir, client }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Sends `script_run` of `hang` as request `id` and waits until the script runs.
    fn run_script(&mut self, id: i64) {
        let args = [self.file("pid"), self.file("path")].map(|path| path.display().to_string());
        self.client
            .send(&json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": "script_run", "arguments": { "name": "hang", "scope": "global", "args": args } } }));

        let started = Instant::now();
        while read(&self.file("path")).is_empty() {
            assert!(started.elapsed() < LIMIT, "the script did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The script's background child and its file are gone soon.
    fn assert_script_gone(&self) {
        let pid = read(&self.file("pid"));
        let started = Instant::now();

        while alive(&pid) && started.elapsed() < LIMIT {
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(!alive(&pid), "the script's background child still runs");
        assert!(
            !Path::new(&read(&self.file("path"))).exists(),
            "the script file is still there"
        );
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default().trim().to_owned()
}

fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn a_cancelled_request_stops_its_script_and_gets_no_answer() {
    let mut fixture = Fixture::start();
    fixture.run_script(41);

    fixture
        .client
        .send(&json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 41 } }));
    fixture.assert_script_gone();

    // The server goes on, and the next answer is that of the next request.
    let listed = fixture.client.request("tools/list", json!({}));
    assert!(listed["result"]["tools"].is_array(), "{listed}");
}

#[test]
fn closing_the_input_stops_the_running_script() {
    let mut fixture = Fixture::start();
    fixture.run_script(41);

    fixture.client.close_input();
    fixture.assert_script_gone();

    assert!(fixture.client.exit_within(LIMIT).is_some(), "the server did not exit");
}

#[test]
fn sigterm_stops_the_running_script_and_the_server() {
    let mut fixture = Fixture::start();
    fixture.run_script(41);

    let pid = fixture.client.pid().to_string();
    let sent = Command::new("kill").args(["-TERM", &pid]).status().expect("kill");
    assert!(sent.success());

    fixture.assert_script_gone();
    assert!(fixture.client.exit_within(LIMIT).is_some(), "the server did not exit");
}

#[test]
fn a_script_runs_to_its_end_while_the_input_stays_open() {
    let mut fixture = Fixture::start();
    fixture.client.call(
        "script_save",
        json!({ "scope": "global", "name": "short", "description": "", "language": "sh", "body": "sleep 0.3\necho done\n" }),
    );

    let ran = fixture.client.request(
        "tools/call",
        json!({ "name": "script_run", "arguments": { "name": "short", "scope": "global" } }),
    );
    assert_eq!(
        ran["result"]["structuredContent"]["stdout"],
        Value::from("done\n"),
        "{ran}"
    );
}

/// A server asked to run `foreign`, a teammate's script that touches `ran`, waiting for the user's
/// answer in Harness Desktop (it waits up to 45 s).
fn waiting_for_approval() -> (ApprovalFixture, Client) {
    let fixture = ApprovalFixture::new();
    let marker = fixture.dir.path().join("ran");
    fixture.save_elsewhere("foreign", &format!("touch '{}'", marker.display()));
    fixture.desktop_alive(Duration::ZERO);

    let mut client = Client::start(command(&fixture.data(), &fixture.work()));
    client.send(&json!({ "jsonrpc": "2.0", "id": 41, "method": "tools/call",
        "params": { "name": "script_run", "arguments": { "name": "foreign", "scope": "global" } } }));

    let started = Instant::now();
    while fixture.leftovers().is_empty() {
        assert!(started.elapsed() < LIMIT, "the server did not ask");
        std::thread::sleep(Duration::from_millis(20));
    }

    (fixture, client)
}

/// The approval wait ended: its request is gone and the script never ran.
fn assert_not_run(fixture: &ApprovalFixture) {
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(fixture.leftovers(), Vec::<String>::new());
    assert!(!fixture.dir.path().join("ran").exists(), "the script ran");
}

#[test]
fn sigterm_during_an_approval_wait_ends_it_and_the_server_promptly() {
    let (fixture, mut client) = waiting_for_approval();

    let sent = Command::new("kill")
        .args(["-TERM", &client.pid().to_string()])
        .status()
        .expect("kill");
    assert!(sent.success());

    assert!(client.exit_within(LIMIT).is_some(), "the server still waits");
    assert_not_run(&fixture);
}

#[test]
fn a_request_cancelled_during_its_approval_wait_gets_no_answer_and_the_server_goes_on() {
    let (fixture, mut client) = waiting_for_approval();

    client.send(&json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 41 } }));

    let started = Instant::now();
    let listed = client.request("tools/list", json!({}));
    assert!(started.elapsed() < LIMIT, "the server still waited");
    assert!(listed["result"]["tools"].is_array(), "{listed}");
    assert_not_run(&fixture);
}
