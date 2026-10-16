//! Several servers on one database file, and a database written by a newer server.

mod common;

use std::sync::{Arc, Barrier};
use std::thread;

use common::{both, draft, query, repo};
use harness_memory::store::sqlite::schema::SCHEMA_VERSION;
use harness_memory::store::{MemoryStore, NoteKind, SqliteStore, StoreError};
use pretty_assertions::assert_eq;
use tempfile::TempDir;

const REPO: &str = "github.com/acme/widget";
const PER_WRITER: usize = 40;

#[test]
fn two_stores_write_to_one_file_without_busy_failures() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("memory").join("memory.sqlite3");
    let barrier = Arc::new(Barrier::new(2));

    let writers: Vec<_> = (0..2)
        .map(|writer| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);

            thread::spawn(move || {
                let mut store = SqliteStore::open(&path).expect("open");
                barrier.wait();

                for index in 0..PER_WRITER {
                    let title = format!("writer {writer} note {index}");
                    store
                        .remember(&repo(REPO), &draft(&title, "shared body text", NoteKind::Note))
                        .expect("remember");
                    store.search(&query("shared", both(REPO))).expect("search");
                }
            })
        })
        .collect();

    for writer in writers {
        writer.join().expect("writer thread");
    }

    let conn = rusqlite::Connection::open(&path).expect("inspect");
    let count: i64 = conn
        .query_row("SELECT count(*) FROM notes", [], |row| row.get(0))
        .expect("count");
    assert_eq!(count, i64::try_from(2 * PER_WRITER).expect("small"));
}

#[test]
fn a_newer_database_is_refused_and_left_untouched() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("memory.sqlite3");
    drop(SqliteStore::open(&path).expect("create"));

    let conn = rusqlite::Connection::open(&path).expect("raw");
    conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .expect("bump");
    drop(conn);

    let mut store = SqliteStore::open(&path).expect("opens");
    assert!(store.is_newer());

    let error = store
        .remember(&repo(REPO), &draft("Title", "Body", NoteKind::Note))
        .expect_err("refused");
    assert!(matches!(error, StoreError::NewerSchema));
    assert_eq!(error.to_string(), "memory database is newer than this server");
    assert!(matches!(
        store.search(&query("", both(REPO))),
        Err(StoreError::NewerSchema)
    ));
    drop(store);

    let conn = rusqlite::Connection::open(&path).expect("raw");
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("version");
    assert_eq!(version, SCHEMA_VERSION + 1);
}

#[cfg(unix)]
#[test]
fn a_new_database_is_private() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("temp dir");
    let folder = dir.path().join("memory");
    let path = folder.join("memory.sqlite3");
    drop(SqliteStore::open(&path).expect("create"));

    let mode = |path: &std::path::Path| std::fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode(&folder), 0o700);
    assert_eq!(mode(&path), 0o600);
}
