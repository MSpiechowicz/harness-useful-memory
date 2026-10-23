//! Hybrid `note_search` and the vectors kept current by the tools, against a stand-in embedder on
//! a real socket: notes only the vectors find, the fusion order, the floor, hidden team notes,
//! the ids of a split store, the notice when the embedder does not answer, silence when embedding
//! is off, and the writes that reach the embedder.
#![cfg(unix)]

mod common;

use std::path::Path;

use common::embedder::{FakeEmbedder, answering, not_ready};
use common::mcp::{call, env, repo_with_remote, run, server, text};
use common::stdio::{BINARY, without_team_settings};
use common::{draft, global_only};
use harness_memory::Server;
use harness_memory::backend::NotesPlace;
use harness_memory::embed::{set_enabled, store_key_local};
use harness_memory::mcp::Vectors;
use harness_memory::project::ProjectResolver;
use harness_memory::store::split::LOCAL_ID_OFFSET;
use harness_memory::store::{HiddenNotes, MemoryStore, NoteKind, Scope, SplitStore, SqliteStore, Target};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tempfile::TempDir;

const LOCAL_KEY: &str = "local:test";
const TEAM_KEY: &str = "team:test";

const UNAVAILABLE: &str = "(vector search unavailable: keyword results only)";

const LOGIN: (&str, &str) = (
    "Login token refresh after sleep",
    "After laptop sleep the OAuth login token expires, so refresh it on resume.",
);
const SESSION: (&str, &str) = (
    "Session lifetime rules",
    "Sessions end after thirty minutes without activity and users sign in again.",
);
const CACHE: (&str, &str) = (
    "Cache layer invalidation",
    "Invalidate the cached rows when the schema changes.",
);

fn store(dir: &Path) -> SqliteStore {
    SqliteStore::open(&dir.join("memory.sqlite3")).expect("store")
}

/// Saves global notes straight into the file, as an earlier session did: their ids are 1, 2, ...
fn seed(store: &mut dyn MemoryStore, notes: &[(&str, &str)]) {
    for (title, body) in notes {
        store
            .remember(&Target::Global, &draft(title, body, NoteKind::Note))
            .expect("save");
    }
}

fn vectors(dir: &Path) -> Vectors {
    Vectors::new(dir.to_path_buf(), Some(LOCAL_KEY.to_owned()), Some(TEAM_KEY.to_owned()))
}

/// A server over `dir/memory.sqlite3` that asks the embedder of `dir`.
fn hybrid(dir: &TempDir) -> Server {
    server(dir, dir.path(), dir.path()).with_vectors(vectors(dir.path()))
}

fn search(server: &mut Server, query: &str) -> String {
    let replies = run(
        server,
        &[call(1, "note_search", json!({ "query": query, "scope": "global" }))],
    );

    text(&replies[0]).to_owned()
}

/// The ids of the notes a search reply shows, in order.
fn ids_of(reply: &str) -> Vec<i64> {
    reply
        .lines()
        .filter_map(|line| line.strip_prefix('#'))
        .filter_map(|rest| rest.split_whitespace().next()?.parse().ok())
        .collect()
}

#[test]
fn notes_only_the_vectors_find_surface_and_a_note_in_both_lists_leads() {
    let dir = TempDir::new().expect("temp dir");
    seed(&mut store(dir.path()), &[LOGIN, SESSION, CACHE]);
    set_enabled(dir.path(), true).expect("enable");

    // The words find note 1 alone. The vectors rank note 2 first, then note 1, then note 3.
    let embedder = FakeEmbedder::start(
        dir.path(),
        answering(&[(LOCAL_KEY, 2, 0.80), (LOCAL_KEY, 1, 0.70), (LOCAL_KEY, 3, 0.55)]),
    );
    let mut server = hybrid(&dir);

    let reply = search(&mut server, "login token");

    assert_eq!(ids_of(&reply), [1, 2, 3], "{reply}");
    assert!(
        reply.contains("Sessions end after thirty minutes without activity"),
        "a note only the vectors found shows the start of its body: {reply}"
    );

    let asked = embedder.ops("search");
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["text"], "login token");
    assert_eq!(
        asked[0]["targets"],
        json!([{ "store": LOCAL_KEY, "projects": [null] }]),
        "global scope: the global notes of the one store"
    );
}

#[test]
fn a_note_below_the_floor_is_left_out_and_an_absent_topic_finds_nothing() {
    let dir = TempDir::new().expect("temp dir");
    seed(&mut store(dir.path()), &[LOGIN, SESSION, CACHE]);
    set_enabled(dir.path(), true).expect("enable");

    let _embedder = FakeEmbedder::start(
        dir.path(),
        answering(&[(LOCAL_KEY, 2, 0.62), (LOCAL_KEY, 3, 0.31), (LOCAL_KEY, 1, 0.22)]),
    );
    let mut server = hybrid(&dir);

    assert_eq!(ids_of(&search(&mut server, "authentication expiry")), [2]);

    // Every vector is unrelated: nothing answers.
    let other = TempDir::new().expect("temp dir");
    seed(&mut store(other.path()), &[LOGIN, SESSION, CACHE]);
    set_enabled(other.path(), true).expect("enable");
    let _unrelated = FakeEmbedder::start(
        other.path(),
        answering(&[(LOCAL_KEY, 3, 0.31), (LOCAL_KEY, 2, 0.27), (LOCAL_KEY, 1, 0.24)]),
    );
    let mut server = hybrid(&other);

    assert_eq!(
        search(&mut server, "what is the zebra migration plan"),
        "no matching notes"
    );
}

/// A split store over `dir/team.sqlite3` and `dir/memory.sqlite3`, hiding the team notes the
/// local file hides, that reads and saves both scopes as `place` says.
fn split(dir: &Path, place: NotesPlace) -> SplitStore {
    let team = SqliteStore::open(&dir.join("team.sqlite3")).expect("team file");
    let hidden = HiddenNotes::new(dir.join("memory.sqlite3"), "team");

    SplitStore::routed(Box::new(team), Box::new(store(dir)), place, place, Some(hidden))
}

fn server_over(store: Box<dyn MemoryStore>, dir: &Path, cwd: &Path) -> Server {
    let open = || SqliteStore::open(&dir.join("memory.sqlite3")).expect("store");

    Server::new(
        store,
        Box::new(open()),
        Box::new(open()),
        ProjectResolver::new(env(dir), Some(cwd.to_path_buf())),
    )
}

#[test]
fn a_hidden_team_note_is_never_returned_and_local_ids_keep_their_offset() {
    let dir = TempDir::new().expect("temp dir");
    let mut team = SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team file");
    seed(
        &mut team,
        &[SESSION, ("Secret rollout plan", "Hidden on this computer.")],
    );
    seed(&mut store(dir.path()), &[CACHE]);

    let secret = team.peek(&global_only(), &[2]).expect("peek").remove(0);
    assert!(
        HiddenNotes::new(dir.path().join("memory.sqlite3"), "team")
            .hide(&secret)
            .expect("hide")
    );
    set_enabled(dir.path(), true).expect("enable");

    let embedder = FakeEmbedder::start(
        dir.path(),
        answering(&[(TEAM_KEY, 2, 0.90), (TEAM_KEY, 1, 0.80), (LOCAL_KEY, 1, 0.70)]),
    );
    let mut server = server_over(
        Box::new(split(dir.path(), NotesPlace::RemoteLocal)),
        dir.path(),
        dir.path(),
    )
    .with_vectors(vectors(dir.path()));

    let reply = search(&mut server, "legacy authentication");

    assert_eq!(ids_of(&reply), [1, LOCAL_ID_OFFSET + 1], "{reply}");
    assert!(!reply.contains("Secret rollout"), "{reply}");

    let targets = embedder.ops("search")[0]["targets"].clone();
    let mut stores: Vec<&str> = targets
        .as_array()
        .expect("targets")
        .iter()
        .map(|target| target["store"].as_str().expect("store"))
        .collect();
    stores.sort_unstable();
    assert_eq!(stores, [LOCAL_KEY, TEAM_KEY]);
}

#[test]
fn a_vector_hit_whose_id_cannot_be_shifted_is_dropped_not_a_crash() {
    let dir = TempDir::new().expect("temp dir");
    let mut team = SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team file");
    seed(&mut team, &[SESSION]);
    seed(&mut store(dir.path()), &[CACHE]);
    set_enabled(dir.path(), true).expect("enable");

    // The local store's ids are shifted by LOCAL_ID_OFFSET: this one would overflow.
    let _embedder = FakeEmbedder::start(
        dir.path(),
        answering(&[(LOCAL_KEY, i64::MAX, 0.90), (TEAM_KEY, 1, 0.80), (LOCAL_KEY, 1, 0.70)]),
    );
    let mut server = server_over(
        Box::new(split(dir.path(), NotesPlace::RemoteLocal)),
        dir.path(),
        dir.path(),
    )
    .with_vectors(vectors(dir.path()));

    let reply = search(&mut server, "legacy authentication");

    assert_eq!(ids_of(&reply), [1, LOCAL_ID_OFFSET + 1], "{reply}");
}

#[test]
fn the_first_search_says_the_embedder_is_down_and_later_ones_stay_quiet() {
    let dir = TempDir::new().expect("temp dir");
    seed(&mut store(dir.path()), &[LOGIN, SESSION]);
    set_enabled(dir.path(), true).expect("enable");
    let mut server = hybrid(&dir);

    let first = search(&mut server, "login token");
    assert_eq!(first.matches(UNAVAILABLE).count(), 1, "{first}");
    assert_eq!(ids_of(&first), [1], "the keyword results are there: {first}");

    let second = search(&mut server, "login token");
    assert!(!second.contains(UNAVAILABLE), "{second}");
    assert_eq!(ids_of(&second), [1]);

    // The embedder comes up: the notice does not return, the vectors work.
    let _embedder = FakeEmbedder::start(dir.path(), answering(&[(LOCAL_KEY, 2, 0.8)]));
    let third = search(&mut server, "login token");
    assert!(!third.contains(UNAVAILABLE), "{third}");
    assert_eq!(ids_of(&third), [1, 2]);
}

#[test]
fn an_embedder_without_a_model_counts_as_down() {
    let dir = TempDir::new().expect("temp dir");
    seed(&mut store(dir.path()), &[LOGIN]);
    set_enabled(dir.path(), true).expect("enable");
    let _embedder = FakeEmbedder::start(dir.path(), not_ready);
    let mut server = hybrid(&dir);

    let reply = search(&mut server, "login token");

    assert_eq!(reply.matches(UNAVAILABLE).count(), 1, "{reply}");
    assert_eq!(ids_of(&reply), [1]);
}

#[test]
fn nothing_changes_while_embedding_is_off() {
    let dir = TempDir::new().expect("temp dir");
    seed(&mut store(dir.path()), &[LOGIN, SESSION]);
    let embedder = FakeEmbedder::start(dir.path(), answering(&[(LOCAL_KEY, 2, 0.9)]));

    let mut plain = server(&dir, dir.path(), dir.path());
    let mut off = hybrid(&dir);

    for query in ["login token", "authentication expiry", ""] {
        assert_eq!(search(&mut off, query), search(&mut plain, query), "{query:?}");
    }

    // Saving does not talk to it either.
    run(
        &mut off,
        &[call(
            2,
            "note_save",
            json!({ "title": "Another", "body": "Some other note.", "kind": "note", "scope": "global" }),
        )],
    );
    assert_eq!(embedder.requests(), Vec::<Value>::new());
}

fn initialize(server: &mut Server) -> Value {
    server
        .handle(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } }))
        .expect("an answer")
}

#[test]
fn a_session_asks_the_embedder_to_warm_up_when_it_starts_and_only_when_embedding_is_on() {
    let dir = TempDir::new().expect("temp dir");
    seed(&mut store(dir.path()), &[LOGIN]);
    let embedder = FakeEmbedder::start(dir.path(), answering(&[]));

    initialize(&mut hybrid(&dir));
    assert_eq!(embedder.requests(), Vec::<Value>::new(), "off");

    set_enabled(dir.path(), true).expect("enable");
    let reply = initialize(&mut hybrid(&dir));

    assert!(reply["result"].is_object(), "{reply}");
    assert_eq!(embedder.ops("warm").len(), 1);
    assert_eq!(embedder.requests().len(), 1);
}

#[test]
fn a_session_starts_without_an_embedder_to_warm() {
    let dir = TempDir::new().expect("temp dir");
    set_enabled(dir.path(), true).expect("enable");

    let started = std::time::Instant::now();
    let reply = initialize(&mut hybrid(&dir));

    assert!(reply["result"].is_object(), "{reply}");
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

#[test]
fn a_query_without_words_never_asks_the_embedder() {
    let dir = TempDir::new().expect("temp dir");
    seed(&mut store(dir.path()), &[LOGIN, SESSION]);
    set_enabled(dir.path(), true).expect("enable");
    let embedder = FakeEmbedder::start(dir.path(), answering(&[(LOCAL_KEY, 2, 0.9)]));
    let mut server = hybrid(&dir);

    assert_eq!(search(&mut server, "what is the"), "no matching notes");
    assert_eq!(
        ids_of(&search(&mut server, "")),
        [2, 1],
        "the recent notes, newest first"
    );
    assert_eq!(embedder.requests(), Vec::<Value>::new());
}

fn written(request: &Value) -> (&str, i64, &Value) {
    let note = &request["notes"][0];

    (
        request["store"].as_str().expect("store"),
        note["id"].as_i64().expect("id"),
        note,
    )
}

#[test]
fn saved_changed_and_deleted_notes_reach_the_embedder_under_their_own_store_and_id() {
    let dir = TempDir::new().expect("temp dir");
    let repo = repo_with_remote(dir.path(), "widget", "https://github.com/acme/widget.git");
    set_enabled(dir.path(), true).expect("enable");
    let embedder = FakeEmbedder::start(dir.path(), answering(&[]));

    // Global notes stay on this computer, repository notes go to the team database.
    let team = SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team file");
    let routed = SplitStore::new(Box::new(team), Box::new(store(dir.path())), Scope::Global);
    let mut server = server_over(Box::new(routed), dir.path(), &repo).with_vectors(vectors(dir.path()));

    let save = |scope: &str, title: &str| {
        call(
            1,
            "note_save",
            json!({ "title": title, "body": format!("Body of {title}."), "kind": "note", "scope": scope }),
        )
    };
    run(&mut server, &[save("global", "Local note"), save("repo", "Team note")]);

    let upserts = embedder.ops("upsert");
    assert_eq!(upserts.len(), 2, "{upserts:?}");

    let (store_key, id, note) = written(&upserts[0]);
    assert_eq!(
        (store_key, id),
        (LOCAL_KEY, 1),
        "the local store's own id, not the shown one"
    );
    assert_eq!(note["title"], "Local note");
    assert_eq!(note["body"], "Body of Local note.");
    assert_eq!(note["project"], Value::Null);

    let (store_key, id, note) = written(&upserts[1]);
    assert_eq!((store_key, id), (TEAM_KEY, 1));
    assert_eq!(note["project"], "git:github.com/acme/widget");

    run(
        &mut server,
        &[call(
            2,
            "note_update",
            json!({ "id": LOCAL_ID_OFFSET + 1, "append": "More detail.", "scope": "global" }),
        )],
    );

    let upserts = embedder.ops("upsert");
    let (store_key, id, note) = written(&upserts[2]);
    assert_eq!((store_key, id), (LOCAL_KEY, 1));
    assert_eq!(note["body"], "Body of Local note.\nMore detail.");

    run(&mut server, &[call(3, "note_delete", json!({ "id": 1 }))]);
    run(
        &mut server,
        &[call(
            4,
            "note_delete",
            json!({ "id": LOCAL_ID_OFFSET + 1, "scope": "global" }),
        )],
    );

    let forgotten: Vec<(String, Value)> = embedder
        .ops("forget")
        .iter()
        .map(|request| {
            (
                request["store"].as_str().expect("store").to_owned(),
                request["ids"].clone(),
            )
        })
        .collect();
    assert_eq!(
        forgotten,
        [(TEAM_KEY.to_owned(), json!([1])), (LOCAL_KEY.to_owned(), json!([1]))]
    );
}

#[test]
fn a_save_does_not_fail_when_the_embedder_is_down() {
    let dir = TempDir::new().expect("temp dir");
    set_enabled(dir.path(), true).expect("enable");
    let mut server = hybrid(&dir);

    let replies = run(
        &mut server,
        &[call(
            1,
            "note_save",
            json!({ "title": "Kept", "body": "Saved without an embedder.", "kind": "note", "scope": "global" }),
        )],
    );

    assert_eq!(replies[0]["result"]["isError"], false, "{}", replies[0]);
    assert_eq!(ids_of(&search(&mut server, "kept")), [1]);
}

#[test]
fn the_user_commands_keep_the_vectors_current_too() {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let dir = TempDir::new().expect("temp dir");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).expect("data folder");
    set_enabled(&data, true).expect("enable");
    let embedder = FakeEmbedder::start(&data, answering(&[]));

    let cli = |args: &[&str], input: &str| {
        let mut command = Command::new(BINARY);
        command
            .args(args)
            .current_dir(dir.path())
            .env("HARNESS_MEMORY_DIR", &data)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        without_team_settings(&mut command);

        let mut child = command.spawn().expect("the executable runs");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("input");
        let output = child.wait_with_output().expect("the executable ends");
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        serde_json::from_slice::<Value>(&output.stdout).unwrap_or(Value::Null)
    };

    let local = store_key_local(&data.join("memory.sqlite3"));
    let saved = cli(
        &["notes", "save", "--json"],
        &json!({ "title": "By hand", "body": "Written by the user.", "kind": "note", "scope": "global" }).to_string(),
    );
    let id = saved["id"].as_i64().expect("id");

    cli(
        &["notes", "update", &id.to_string(), "--json"],
        &json!({ "body": "Edited by the user." }).to_string(),
    );
    cli(&["notes", "delete", &id.to_string()], "");

    let writes: Vec<(String, String, i64)> = embedder
        .requests()
        .iter()
        .filter(|request| request["op"] != "ping")
        .map(|request| {
            let id = request["notes"][0]["id"]
                .as_i64()
                .unwrap_or_else(|| request["ids"][0].as_i64().unwrap_or(0));
            (
                request["op"].as_str().expect("op").to_owned(),
                request["store"].as_str().expect("store").to_owned(),
                id,
            )
        })
        .collect();

    assert_eq!(
        writes,
        [
            ("upsert".to_owned(), local.clone(), id),
            ("upsert".to_owned(), local.clone(), id),
            ("forget".to_owned(), local, id),
        ]
    );
    assert_eq!(embedder.ops("upsert")[1]["notes"][0]["body"], "Edited by the user.");
}
