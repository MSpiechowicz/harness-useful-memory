//! Schema versions and concurrent clients on both SQLite-dialect backends (see
//! `libsql_support`): a version 1 database is migrated, a newer one is refused and left as it is,
//! and clients that start together on a new database migrate it once and then write side by side.
//! On libSQL, a client already connected when a newer server migrates stops writing.

mod common;
mod libsql_support;

use std::path::PathBuf;
use std::sync::Barrier;
use std::thread;

use common::{both, draft, query, repo};
use harness_memory::store::sqlite::schema::{SCHEMA_VERSION, V1};
use harness_memory::store::{NoteKind, Provenance, StoreError};
use libsql_support::Database;
use pretty_assertions::assert_eq;

const REPO: &str = "github.com/acme/widget";
const CLIENTS: usize = 4;
const PER_WRITER: usize = 15;

store_cases!(
    version_1_is_migrated_and_keeps_its_notes,
    a_newer_database_is_refused_and_left_untouched,
    clients_starting_together_migrate_once_and_write_side_by_side,
);

fn version_1_is_migrated_and_keeps_its_notes(db: &Database) {
    db.batch(V1);
    db.batch(
        "INSERT INTO projects (key, root_path, remote, created_at, last_seen_at)
             VALUES ('git:github.com/acme/widget', '/work/github.com/acme/widget', 'github.com/acme/widget', 1, 1);
         INSERT INTO notes (project_id, kind, title, title_norm, body, source, created_at, updated_at)
             VALUES (1, 'location', 'Build cache', 'build cache', 'Lives in target/', 'omp', 1, 1);
         INSERT INTO notes (project_id, kind, title, title_norm, body, created_at, updated_at)
             VALUES (NULL, 'note', 'Editor tabs', 'editor tabs', 'Four wide', 1, 1);
         PRAGMA user_version = 1;",
    );

    let mut store = db.open();
    let hits = store.search(&query("cache", both(REPO))).expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "Build cache");

    let notes = store.get(&both(REPO), &[1, 2]).expect("get");
    assert_eq!(notes.len(), 2);
    assert_eq!(
        notes[0].provenance,
        Provenance {
            root: Some(PathBuf::from("/work/github.com/acme/widget")),
            source: Some("omp".to_owned()),
            via_override: false,
        }
    );
    assert_eq!(notes[1].provenance, Provenance::default());
    assert_eq!(db.integer("PRAGMA user_version"), SCHEMA_VERSION);
}

fn a_newer_database_is_refused_and_left_untouched(db: &Database) {
    drop(db.open());
    db.batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1));

    let mut store = db.open();
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

    assert_eq!(db.integer("PRAGMA user_version"), SCHEMA_VERSION + 1);
    assert_eq!(db.integer("SELECT count(*) FROM notes"), 0);
}

/// A newer server migrating the shared database while an older client is connected stops that
/// client's next write (the version is re-read under the write lock) and every later call.
#[test]
fn an_open_libsql_client_stops_writing_after_a_newer_migration() {
    let Some(db) = Database::libsql() else {
        return;
    };

    let mut store = db.open();
    store
        .remember(&repo(REPO), &draft("Before", "Body", NoteKind::Note))
        .expect("remember");
    db.batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1));

    let error = store
        .remember(&repo(REPO), &draft("After", "Body", NoteKind::Note))
        .expect_err("refused");
    assert!(matches!(error, StoreError::NewerSchema), "{error:?}");
    assert!(matches!(
        store.search(&query("", both(REPO))),
        Err(StoreError::NewerSchema)
    ));
    drop(store);

    assert_eq!(db.integer("SELECT count(*) FROM notes"), 1);
    assert_eq!(db.integer("PRAGMA user_version"), SCHEMA_VERSION + 1);
}

fn clients_starting_together_migrate_once_and_write_side_by_side(db: &Database) {
    let barrier = Barrier::new(CLIENTS);

    thread::scope(|scope| {
        for writer in 0..CLIENTS {
            let barrier = &barrier;

            scope.spawn(move || {
                barrier.wait();
                let mut store = db.open();

                for index in 0..PER_WRITER {
                    let title = format!("writer {writer} note {index}");
                    store
                        .remember(&repo(REPO), &draft(&title, "shared body text", NoteKind::Note))
                        .expect("remember");
                    store.search(&query("shared", both(REPO))).expect("search");
                }
            });
        }
    });

    assert_eq!(db.integer("PRAGMA user_version"), SCHEMA_VERSION);
    assert_eq!(
        db.integer("SELECT count(*) FROM notes"),
        i64::try_from(CLIENTS * PER_WRITER).expect("small")
    );
}
