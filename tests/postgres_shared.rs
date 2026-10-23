//! What a shared PostgreSQL database can hand back that this server never wrote: a schema migrated
//! by a newer server after connecting, rows changed by hand (NULLs, huge text) and errors raised
//! with the server's own text. None of it may panic, flood a reply or reach the agent verbatim.
//! See `postgres_support` for the test server.

mod common;
mod postgres_support;

use common::{both, draft, query, repo};
use harness_memory::limits::{BODY_MAX, ENTITY_MAX, TITLE_MAX};
use harness_memory::store::postgres::schema::SCHEMA_VERSION;
use harness_memory::store::{MemoryStore, NoteKind, StoreError};
use postgres_support::database;
use pretty_assertions::assert_eq;

const REPO: &str = "github.com/acme/widget";

#[test]
fn a_client_stops_writing_once_a_newer_server_migrated_after_it_connected() {
    let Some(db) = database() else { return };
    let mut older = db.open();
    older
        .remember(&repo(REPO), &draft("Before", "written before the bump", NoteKind::Note))
        .expect("before");

    let mut admin = db.admin();
    admin
        .execute(
            "UPDATE harness_memory_meta SET value = $1 WHERE key = 'schema_version'",
            &[&(SCHEMA_VERSION + 1)],
        )
        .expect("bump");

    let refused = older.remember(&repo(REPO), &draft("After", "written after the bump", NoteKind::Note));
    assert!(matches!(refused, Err(StoreError::NewerSchema)), "{refused:?}");
    assert!(older.is_newer());
    assert!(matches!(
        older.search(&query("", both(REPO))),
        Err(StoreError::NewerSchema)
    ));

    let count: i64 = admin
        .query_one("SELECT count(*) FROM notes", &[])
        .expect("count")
        .get(0);
    assert_eq!(count, 1);
}

#[test]
fn a_write_whose_connection_dropped_before_it_runs_once() {
    let Some(db) = database() else { return };
    let name = format!("hm_write_drop_{}", std::process::id());
    let mut store = harness_memory::store::PostgresStore::connect(&db.url_with(&format!("application_name={name}")))
        .expect("connect");
    store
        .remember(&repo(REPO), &draft("Before", "written before the drop", NoteKind::Note))
        .expect("before");

    let mut admin = db.admin();
    admin
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name = $1",
            &[&name],
        )
        .expect("terminate");

    let saved = store
        .remember(&repo(REPO), &draft("After", "written after the drop", NoteKind::Note))
        .expect("the write is tried again on a new connection");
    assert!(saved.created);
    assert_eq!(saved.revision, 1);

    let count: i64 = admin
        .query_one("SELECT count(*) FROM notes WHERE title = 'After'", &[])
        .expect("count")
        .get(0);
    assert_eq!(count, 1);
}

#[test]
fn a_null_written_by_hand_is_an_error_not_a_panic() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let saved = store
        .remember(
            &repo(REPO),
            &draft("Nulled", "a row a teammate breaks by hand", NoteKind::Note),
        )
        .expect("remember");

    db.admin()
        .batch_execute("ALTER TABLE notes ALTER COLUMN title DROP NOT NULL; UPDATE notes SET title = NULL;")
        .expect("null the title");

    let searched = store.search(&query("", both(REPO)));
    assert!(matches!(searched, Err(StoreError::Backend(_))), "{searched:?}");

    let matched = store.search(&query("teammate", both(REPO)));
    assert!(matches!(matched, Err(StoreError::Backend(_))), "{matched:?}");

    let got = store.get(&both(REPO), &[saved.id]);
    assert!(matches!(got, Err(StoreError::Backend(_))), "{got:?}");
}

#[test]
fn text_written_by_hand_comes_back_cut_to_the_limits() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let mut note = draft("Huge", "a note a teammate inflates by hand", NoteKind::Note);
    note.entities = vec!["widget".to_owned()];
    let saved = store.remember(&repo(REPO), &note).expect("remember");

    db.admin()
        .batch_execute(
            "UPDATE notes SET title = repeat('t', 100000), body = repeat('inflated ', 200000);
             UPDATE entities SET name = repeat('e', 100000);",
        )
        .expect("inflate");

    let notes = store.get(&both(REPO), &[saved.id]).expect("get");
    let note = &notes[0];
    assert!(note.title.chars().count() <= TITLE_MAX + 1, "{}", note.title.len());
    assert!(note.body.chars().count() <= BODY_MAX + 1, "{}", note.body.len());
    assert!(note.entities[0].chars().count() <= ENTITY_MAX + 1);

    // A search checks the words of the text it reads, not only the index, so the inflated note no
    // longer answers the words the server saw when it saved it; the list of recent notes still
    // shows it cut to the limits.
    assert!(store.search(&query("teammate", both(REPO))).expect("search").is_empty());

    let hits = store.search(&query("", both(REPO))).expect("recent");
    assert_eq!(hits.len(), 1);
    assert!(hits[0].title.chars().count() <= TITLE_MAX + 1);
}

#[test]
fn server_text_in_an_error_never_reaches_the_agent() {
    let Some(db) = database() else { return };
    let mut store = db.open();

    db.admin()
        .batch_execute(
            "CREATE FUNCTION hm_refuse() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
                 IF NEW.title = 'Duplicate' THEN
                     RAISE EXCEPTION 'leaked secret sk-server-text' USING ERRCODE = '23505',
                         DETAIL = 'leaked detail', HINT = 'leaked hint';
                 END IF;
                 RAISE EXCEPTION 'leaked secret sk-server-text' USING DETAIL = 'leaked detail';
             END $$;
             CREATE TRIGGER hm_refuse BEFORE INSERT ON notes FOR EACH ROW EXECUTE FUNCTION hm_refuse();",
        )
        .expect("trigger");

    let unknown = store
        .remember(&repo(REPO), &draft("Anything", "body", NoteKind::Note))
        .expect_err("refused");
    assert_eq!(unknown.to_string(), "memory database error: database error P0001");

    let known = store
        .remember(&repo(REPO), &draft("Duplicate", "body", NoteKind::Note))
        .expect_err("refused");
    let text = known.to_string();
    assert!(text.contains("SQLSTATE 23505"), "{text}");
    assert!(!text.contains("leaked"), "{text}");
}
