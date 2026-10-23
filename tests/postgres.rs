//! What only the PostgreSQL store has to show (see `postgres_support`): clients migrating a new
//! schema at once, a newer schema, writers queueing, a dropped connection, the TLS rules against a
//! real server, the database role kept with each write, and `push` into it.

mod common;
mod postgres_support;
mod push_support;

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use common::{both, draft, query, repo};
use harness_memory::store::postgres::schema::SCHEMA_VERSION;
use harness_memory::store::{MemoryStore, NoteKind, PostgresStore, StoreError};
use postgres_support::database;
use pretty_assertions::assert_eq;

const REPO: &str = "github.com/acme/widget";

#[test]
fn clients_starting_together_migrate_a_new_schema_once() {
    let Some(db) = database() else { return };
    let clients = 4;
    let barrier = Arc::new(Barrier::new(clients));

    let starts: Vec<_> = (0..clients)
        .map(|_| {
            let url = db.url().to_owned();
            let barrier = Arc::clone(&barrier);

            thread::spawn(move || {
                barrier.wait();
                PostgresStore::connect(&url).map(|store| store.is_newer())
            })
        })
        .collect();

    for start in starts {
        let newer = start.join().expect("client thread").expect("every client connects");
        assert!(!newer);
    }

    let row = db
        .admin()
        .query_one("SELECT count(*), max(value) FROM harness_memory_meta", &[])
        .expect("meta");
    assert_eq!((row.get::<_, i64>(0), row.get::<_, i64>(1)), (1, SCHEMA_VERSION));
}

#[test]
fn a_newer_schema_is_refused_and_left_untouched() {
    let Some(db) = database() else { return };
    drop(db.open());

    let mut admin = db.admin();
    admin
        .execute(
            "UPDATE harness_memory_meta SET value = $1 WHERE key = 'schema_version'",
            &[&(SCHEMA_VERSION + 1)],
        )
        .expect("bump");

    let mut store = db.open();
    assert!(store.is_newer());

    let error = store
        .remember(&repo(REPO), &draft("Title", "Body", NoteKind::Note))
        .expect_err("refused");
    assert!(matches!(error, StoreError::NewerSchema));
    assert!(matches!(
        store.search(&query("", both(REPO))),
        Err(StoreError::NewerSchema)
    ));

    let version: i64 = admin
        .query_one("SELECT value FROM harness_memory_meta", &[])
        .expect("version")
        .get(0);
    assert_eq!(version, SCHEMA_VERSION + 1);
}

#[test]
fn writers_on_separate_connections_queue_without_failures() {
    let Some(db) = database() else { return };
    drop(db.open());
    let per_writer = 25;
    let barrier = Arc::new(Barrier::new(2));

    let writers: Vec<_> = (0..2)
        .map(|writer| {
            let url = db.url().to_owned();
            let barrier = Arc::clone(&barrier);

            thread::spawn(move || {
                let mut store = PostgresStore::connect(&url).expect("connect");
                barrier.wait();

                for index in 0..per_writer {
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

    let count: i64 = db
        .admin()
        .query_one("SELECT count(*) FROM notes", &[])
        .expect("count")
        .get(0);
    assert_eq!(count, 2 * per_writer);
}

#[test]
fn an_oversized_entity_list_fails_at_once_without_blocking_other_writers() {
    let Some(db) = database() else { return };
    drop(db.open());
    let barrier = Arc::new(Barrier::new(2));

    let flood = {
        let url = db.url().to_owned();
        let barrier = Arc::clone(&barrier);

        thread::spawn(move || {
            let mut store = PostgresStore::connect(&url).expect("connect");
            let mut note = draft("Flood", "body", NoteKind::Note);
            note.entities = (0..100_000).map(|index| format!("entity {index}")).collect();
            barrier.wait();

            let started = Instant::now();
            let result = store.remember(&repo(REPO), &note);
            (result, started.elapsed())
        })
    };

    let mut store = db.open();
    barrier.wait();
    let written = store.remember(&repo(REPO), &draft("Meanwhile", "still writes", NoteKind::Note));

    let (result, took) = flood.join().expect("flood thread");
    assert!(matches!(result, Err(StoreError::Invalid(_))), "{result:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert!(written.is_ok(), "{written:?}");
}

#[test]
fn a_dropped_connection_is_opened_again() {
    let Some(db) = database() else { return };
    let name = format!("hm_reconnect_{}", std::process::id());
    let mut store = PostgresStore::connect(&db.url_with(&format!("application_name={name}"))).expect("connect");
    store
        .remember(&repo(REPO), &draft("Before", "written before the drop", NoteKind::Note))
        .expect("before");

    let ended: i64 = db
        .admin()
        .query_one(
            "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity WHERE application_name = $1",
            &[&name],
        )
        .expect("terminate")
        .get(0);
    assert_eq!(ended, 1);

    let hits = store
        .search(&query("written", both(REPO)))
        .expect("search after the drop");
    assert_eq!(hits.len(), 1);
    store
        .remember(&repo(REPO), &draft("After", "written after the drop", NoteKind::Note))
        .expect("after");
}

#[test]
fn tls_rules_hold_against_a_server_without_tls() {
    let Some(db) = database() else { return };

    // The test server has no TLS: `require` fails instead of falling back to plaintext.
    let refused = PostgresStore::connect(&db.url_with("sslmode=require")).expect_err("TLS required");
    assert!(matches!(refused, StoreError::Backend(_)), "{refused:?}");

    // `disable` and the default `prefer` work for a server on this machine.
    PostgresStore::connect(&db.url_with("sslmode=disable")).expect("plaintext to localhost");
}

#[test]
fn each_write_records_the_database_role() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    store
        .remember(&repo(REPO), &draft("Owner", "who wrote this", NoteKind::Note))
        .expect("remember");

    let mut admin = db.admin();
    let role: String = admin.query_one("SELECT current_user", &[]).expect("role").get(0);
    let written_by: String = admin
        .query_one("SELECT written_by FROM notes", &[])
        .expect("note")
        .get(0);
    assert_eq!(written_by, role);
}

#[test]
fn pushing_into_postgres_is_idempotent() {
    let Some(db) = database() else { return };
    let mut store = db.open();

    push_support::scenario(&mut store);
}

#[test]
fn hidden_team_notes_are_never_written_over_in_postgres() {
    let Some(db) = database() else { return };
    let mut store = db.open();

    push_support::hidden_scenario(&mut store);
}
