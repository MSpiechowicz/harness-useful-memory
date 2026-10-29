//! The `harness-memory` executable over stdio: the handshake, notes shared between processes, the
//! database location (including the takeover of Harness Desktop's database) and the command line.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::mcp::repo_with_remote;
use common::stdio::{BINARY, Client, command};
use common::{draft, global_only, query};
use harness_memory::store::{MemoryStore, NoteKind, SqliteStore, Target};
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

struct Machine {
    _root: TempDir,
    home: PathBuf,
    data: PathBuf,
    memory: PathBuf,
    repo: PathBuf,
    plain: PathBuf,
}

fn machine() -> Machine {
    let root = TempDir::new().expect("temp");
    let home = root.path().join("home");
    let data = root.path().join("data");
    let memory = root.path().join("memory");
    let plain = root.path().join("work/plain");

    for dir in [&home, &data, &plain] {
        std::fs::create_dir_all(dir).expect("mkdir");
    }

    let repo = repo_with_remote(
        &root.path().join("work"),
        "widget",
        "https://github.com/acme/widget.git",
    );

    Machine {
        _root: root,
        home,
        data,
        memory,
        repo,
        plain,
    }
}

/// `command` with the home and data folders of `machine` on every OS.
fn isolated(mut command: Command, machine: &Machine) -> Command {
    command
        .env("HOME", &machine.home)
        .env("USERPROFILE", &machine.home)
        .env("XDG_DATA_HOME", &machine.data)
        .env("LOCALAPPDATA", &machine.data);
    command
}

#[test]
fn notes_persist_across_processes_in_the_memory_folder() {
    let machine = machine();

    let mut first = Client::start(isolated(command(&machine.memory, &machine.repo), &machine));
    assert_eq!(first.initialized["serverInfo"]["name"], "harness-memory");

    let tools = first.request("tools/list", json!({}));
    let tools = tools["result"]["tools"].as_array().expect("tools");
    let names: Vec<&str> = tools.iter().filter_map(|tool| tool["name"].as_str()).collect();
    assert_eq!(
        names,
        [
            "note_save",
            "note_update",
            "note_search",
            "note_get",
            "note_delete",
            "entity_link",
            "entity_neighbors",
            "entity_delete",
            "script_save",
            "script_list",
            "script_get",
            "script_run",
            "script_delete",
            "note_feedback",
            "code_map",
            "code_symbol",
            "code_impact",
            "code_path",
        ]
    );

    let created = first.call(
        "note_save",
        json!({ "title": "Uploader location", "body": "src/upload.rs holds the uploader", "kind": "location" }),
    );
    assert_eq!(
        created,
        r#"created note 1 (repo "git:github.com/acme/widget", revision 1)"#
    );

    let found = first.call("note_search", json!({ "query": "uploader" }));
    assert!(found.contains("Uploader location"), "{found}");
    assert!(first.finish().success());

    assert!(machine.memory.join("memory.sqlite3").is_file());
    assert!(!machine.repo.join("memory.sqlite3").exists());
    assert!(!machine.data.join("harness-memory").exists());

    // A second server outside the repository finds the note when told which repository it is.
    let mut second = Client::start(isolated(command(&machine.memory, &machine.plain), &machine));
    let unrelated = second.call("note_search", json!({ "query": "uploader" }));
    assert_eq!(unrelated, "no matching notes");

    let repo_path = machine.repo.to_str().expect("utf-8");
    let shared = second.call("note_search", json!({ "query": "uploader", "project_path": repo_path }));
    assert!(shared.contains("#1 [location, repo,"), "{shared}");
    assert!(second.finish().success());
}

/// The data folder the executable uses on this OS for `machine` (see `isolated`).
fn data_dir(machine: &Machine) -> PathBuf {
    if cfg!(target_os = "macos") {
        machine.home.join("Library/Application Support")
    } else {
        machine.data.clone()
    }
}

fn save_global_note(path: &Path, title: &str) {
    let mut store = SqliteStore::open(path).expect("store");
    store
        .remember(
            &Target::Global,
            &draft(title, "kept across the move", NoteKind::Decision),
        )
        .expect("remember");
}

#[test]
fn the_default_database_takes_over_the_harness_desktop_one() {
    let machine = machine();
    let legacy = data_dir(&machine).join("com.mspiechowicz.harness-useful-desktop/memory/memory.sqlite3");
    save_global_note(&legacy, "Desktop decision");
    let legacy_bytes = std::fs::read(&legacy).expect("legacy");

    let mut start = isolated(Command::new(BINARY), &machine);
    start.current_dir(&machine.plain).env_remove("HARNESS_MEMORY_DIR");

    let mut client = Client::start(start);
    let found = client.call("note_search", json!({ "query": "desktop decision" }));
    assert!(found.contains("Desktop decision"), "{found}");
    assert!(client.finish().success());

    let database = data_dir(&machine).join("harness-memory/memory.sqlite3");
    let mut store = SqliteStore::open(&database).expect("new database");
    let hits = store.search(&query("desktop", global_only())).expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(std::fs::read(&legacy).expect("legacy"), legacy_bytes);
}

#[test]
fn the_command_line_accepts_only_mcp_and_version() {
    let version = Command::new(BINARY).arg("--version").output().expect("run");
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("harness-memory {}", env!("HARNESS_MEMORY_VERSION"))
    );

    let wrong = Command::new(BINARY).args(["--db", "/tmp/x"]).output().expect("run");
    assert_eq!(wrong.status.code(), Some(2));
    assert!(wrong.stdout.is_empty());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("usage: harness-memory"));
}
