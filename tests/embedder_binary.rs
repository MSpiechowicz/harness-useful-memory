//! `harness-memory embedder` as a program: its settings, its status, and one embedder per computer
//! (the model itself is not needed: the lock is taken, and the socket answers, before any download).

mod common;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use common::stdio::{BINARY, without_team_settings};
use serde_json::Value;
use tempfile::TempDir;

fn embedder(memory: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(BINARY);
    command
        .arg("embedder")
        .args(args)
        .env("HARNESS_MEMORY_DIR", memory)
        .env_remove("HARNESS_MEMORY_MODEL_BASE_URL");
    without_team_settings(&mut command);
    command
}

fn json_of(output: &Output) -> Value {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).expect("a JSON line")
}

fn status(memory: &Path) -> Value {
    json_of(&embedder(memory, &["status", "--json"]).output().expect("status"))
}

#[test]
fn embedding_is_off_until_enabled_and_status_says_where_things_stand() {
    let memory = TempDir::new().expect("temp dir");

    let first = status(memory.path());
    assert_eq!(first["enabled"], false, "missing means disabled");
    assert_eq!(first["running"], false);
    assert_eq!(first["active_model"], Value::Null);
    assert_eq!(first["pending_model"], Value::Null);
    assert_eq!(first["model_present"], false);
    assert_eq!(first["vectors"], 0);

    let on = json_of(&embedder(memory.path(), &["enable"]).output().expect("enable"));
    assert_eq!(on["enabled"], true);
    assert_eq!(status(memory.path())["enabled"], true);
    assert!(
        std::fs::read_to_string(memory.path().join("embedder.toml"))
            .expect("settings file")
            .contains("enabled = true")
    );

    json_of(&embedder(memory.path(), &["disable"]).output().expect("disable"));
    assert_eq!(status(memory.path())["enabled"], false);
}

#[test]
fn removing_the_model_clears_the_files_and_vectors() {
    let memory = TempDir::new().expect("temp dir");
    let folder = memory.path().join("models").join("some-model@1");
    std::fs::create_dir_all(&folder).expect("model folder");
    std::fs::write(folder.join("model_quantized.onnx"), b"x").expect("a model file");

    let removed = json_of(&embedder(memory.path(), &["remove-model"]).output().expect("remove"));

    assert_eq!(removed["removed"], true);
    assert!(!memory.path().join("models").exists());
}

#[test]
fn an_unknown_embedder_command_is_a_usage_error() {
    let memory = TempDir::new().expect("temp dir");
    let output = embedder(memory.path(), &["frobnicate"]).output().expect("run");

    assert_eq!(output.status.code(), Some(2));
}

#[cfg(unix)]
#[test]
fn a_second_embedder_says_busy_and_the_first_stops_when_its_input_closes() {
    use harness_memory::embed::EmbedderClient;

    let memory = TempDir::new().expect("temp dir");

    // No model to download from: the download fails at once, which is all this test needs.
    let mut first = embedder(memory.path(), &["run"])
        .env("HARNESS_MEMORY_MODEL_BASE_URL", "http://127.0.0.1:9")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the embedder");

    let mut lines = BufReader::new(first.stdout.take().expect("stdout")).lines();
    let mut seen = Vec::new();

    loop {
        let line = lines.next().expect("the embedder stays up").expect("a line");
        let event: Value = serde_json::from_str(&line).expect("a JSON event");
        let name = event["event"].as_str().unwrap_or_default().to_owned();
        seen.push(name.clone());

        if name == "error" {
            break;
        }
    }

    assert_eq!(seen.first().map(String::as_str), Some("download"));

    let second = embedder(memory.path(), &["run"])
        .env("HARNESS_MEMORY_MODEL_BASE_URL", "http://127.0.0.1:9")
        .output()
        .expect("second embedder");
    assert!(second.status.success());
    assert_eq!(String::from_utf8_lossy(&second.stdout).trim(), "{\"event\":\"busy\"}");

    assert_eq!(status(memory.path())["running"], true);

    let pong = EmbedderClient::new(memory.path(), &std::env::temp_dir())
        .ping()
        .expect("ping");
    assert!(!pong.ready, "no model yet");

    let refused = embedder(memory.path(), &["remove-model"])
        .output()
        .expect("remove while running");
    assert!(!refused.status.success(), "refused while running");

    drop(first.stdin.take());
    assert!(first.wait().expect("exit").success());

    assert_eq!(status(memory.path())["running"], false);
    assert!(!memory.path().join("embedder.sock").exists(), "the socket is removed");
}

#[cfg(unix)]
#[test]
fn a_termination_signal_stops_the_embedder_cleanly() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let memory = TempDir::new().expect("temp dir");

    let mut child = embedder(memory.path(), &["run"])
        .env("HARNESS_MEMORY_MODEL_BASE_URL", "http://127.0.0.1:9")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the embedder");

    // Its first event comes after the socket is up and the signal handler is set.
    let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
    lines.next().expect("the embedder stays up").expect("an event");
    assert!(memory.path().join("embedder.sock").exists());

    kill(Pid::from_raw(i32::try_from(child.id()).expect("pid")), Signal::SIGTERM).expect("signal");

    assert!(child.wait().expect("exit").success());
    assert!(!memory.path().join("embedder.sock").exists(), "the socket is removed");
    assert_eq!(status(memory.path())["running"], false, "the lock is released");
}
