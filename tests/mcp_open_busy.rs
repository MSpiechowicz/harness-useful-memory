//! Opening a database that lacks the author columns while another process holds its write lock:
//! the MCP server does not wait for the lock, a user's own command does.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::mcp::repo_with_remote;
use common::stdio::{Client, command};
use common::{both, draft, repo};
use harness_memory::store::{Author, MemoryStore, NoteKind, Patience, Provenance, SqliteStore};
use pretty_assertions::assert_eq;
use rusqlite::Connection;
use tempfile::TempDir;

const REPO: &str = "github.com/acme/widget";

/// A database file written before the author columns existed, with one note.
fn old_database(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("memory.sqlite3");

    {
        let mut store = SqliteStore::open_with(&path, Patience::Wait).expect("create");
        store
            .remember(
                &repo(REPO),
                &draft("Build cache", "Caches live in target", NoteKind::Location),
            )
            .expect("save");
    }

    let conn = Connection::open(&path).expect("raw connection");

    for table in ["notes", "scripts"] {
        for column in ["added_by_name", "added_by_via", "updated_by_name", "updated_by_via"] {
            conn.execute_batch(&format!("ALTER TABLE {table} DROP COLUMN {column}"))
                .expect("drop");
        }
    }

    path
}

fn held_lock(path: &Path) -> Connection {
    let conn = Connection::open(path).expect("lock holder");
    conn.execute_batch("BEGIN IMMEDIATE").expect("take the write lock");
    conn
}

#[test]
fn the_mcp_server_reads_without_authors_while_another_process_holds_the_lock() {
    let data = TempDir::new().expect("temp");
    let path = old_database(data.path());
    let lock = held_lock(&path);
    let work = repo_with_remote(data.path(), "widget", "https://github.com/acme/widget.git");

    let started = Instant::now();
    let mut agent = Client::start(command(data.path(), &work));
    let found = agent.call(
        "note_search",
        serde_json::json!({ "query": "build cache", "project_path": work }),
    );
    let took = started.elapsed();

    assert!(found.contains("Build cache"), "{found}");
    assert!(took < Duration::from_secs(1), "the server waited {took:?} for the lock");
    drop(agent);

    // Opened the way the server does, the note reads with no authors.
    let started = Instant::now();
    let mut store = SqliteStore::open_with(&path, Patience::Skip).expect("open");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!store.has_authors());

    let note = store.peek(&both(REPO), &[1]).expect("peek").remove(0);
    assert_eq!(note.authors, harness_memory::store::Authors::default());
    drop(store);

    // Once the lock is free, a write adds the columns and records its author.
    drop(lock);
    let mut store = SqliteStore::open_with(&path, Patience::Wait).expect("open");
    assert!(store.has_authors());

    let mut second = draft("Release steps", "Tag then publish", NoteKind::Decision);
    second.provenance = Provenance {
        author: Author::new("Ada Lovelace", Some("omp")),
        ..Provenance::default()
    };
    let saved = store.remember(&repo(REPO), &second).expect("save");
    let note = store.peek(&both(REPO), &[saved.id]).expect("peek").remove(0);
    assert_eq!(note.authors.added_by, Author::new("Ada Lovelace", Some("omp")));
}

#[test]
fn a_skipping_open_adds_the_columns_when_the_lock_is_free() {
    let data = TempDir::new().expect("temp");
    let path = old_database(data.path());

    let store = SqliteStore::open_with(&path, Patience::Skip).expect("open");

    assert!(store.has_authors());
}

#[test]
fn a_waiting_open_waits_for_the_lock_and_then_adds_the_columns() {
    let data = TempDir::new().expect("temp");
    let path = old_database(data.path());
    let lock = held_lock(&path);

    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        drop(lock);
    });

    let started = Instant::now();
    let store = SqliteStore::open_with(&path, Patience::Wait).expect("open");
    release.join().expect("release");

    assert!(started.elapsed() >= Duration::from_millis(300), "it did not wait");
    assert!(store.has_authors());
}

/// A server that opened without the columns (the lock was held) learns that another process added
/// them, so a change it makes names its own writer and never leaves the previous one in place.
#[test]
fn a_skipping_store_that_missed_the_columns_names_its_writer_once_another_process_added_them() {
    use harness_memory::store::NotePatch;

    let data = TempDir::new().expect("temp");
    let path = old_database(data.path());
    let lock = held_lock(&path);

    let mut skipping = SqliteStore::open_with(&path, Patience::Skip).expect("open");
    assert!(!skipping.has_authors());

    // Another process gets the lock, adds the columns, and a person updates the note.
    drop(lock);
    drop(SqliteStore::open_with(&path, Patience::Wait).expect("open"));
    Connection::open(&path)
        .expect("raw connection")
        .execute_batch("UPDATE notes SET added_by_name = 'Alice', updated_by_name = 'Alice', updated_by_via = 'omp'")
        .expect("Alice's change");

    let patch = NotePatch {
        body: Some("Caches live in target/debug".to_owned()),
        provenance: Provenance {
            author: Author::new("Grace Hopper", Some("claude-code")),
            ..Provenance::default()
        },
        ..NotePatch::default()
    };
    skipping.update(&both(REPO), 1, &patch).expect("update");

    assert!(skipping.has_authors(), "the columns are known now");
    let note = skipping.peek(&both(REPO), &[1]).expect("peek").remove(0);
    assert_eq!(
        note.authors.updated_by,
        Author::new("Grace Hopper", Some("claude-code"))
    );
    assert_eq!(note.authors.added_by, Author::new("Alice", None));
}

/// The same for a script saved by a store that missed the columns.
#[test]
fn a_skipping_store_that_missed_the_columns_records_the_author_of_a_script_saved_after_they_came() {
    use common::scripts::script;
    use harness_memory::store::ScriptStore;

    let data = TempDir::new().expect("temp");
    let path = old_database(data.path());
    let lock = held_lock(&path);

    let mut skipping = SqliteStore::open_with(&path, Patience::Skip).expect("open");
    assert!(!skipping.has_authors());

    drop(lock);
    drop(SqliteStore::open_with(&path, Patience::Wait).expect("open"));

    let mut draft = script("build", "Build it", "cargo build\n");
    draft.provenance.author = Author::new("Grace Hopper", None);
    let saved = skipping.save_script(&repo(REPO), &draft).expect("save");

    assert_eq!(saved.script.summary.authors.added_by, Author::new("Grace Hopper", None));
    assert_eq!(
        saved.script.summary.authors.updated_by,
        Author::new("Grace Hopper", None)
    );
}
