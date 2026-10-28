//! The additive schema extensions of the SQLite-dialect stores (the English index, the `lang` column
//! and the settings) on both backends (see `libsql_support`): they are added on every open without
//! a new schema version, and an older server keeps reading and writing the upgraded database with
//! its writes still indexed.

mod common;
mod libsql_support;

use common::listing::listing;
use common::{both, draft, query, repo};
use harness_memory::store::sqlite::schema::{SCHEMA_VERSION, V1, V2, V3, V4, V5};
use harness_memory::store::{NoteKind, NotePatch, NoteUsage, StoreError};
use libsql_support::Database;
use pretty_assertions::assert_eq;

const REPO: &str = "github.com/acme/widget";

const ENGLISH: &str = "The sync job reads every changed note from the team database, then writes the new revision \
                       to the local cache so the next search finds it without another round trip.";

const POLISH: &str = "Zadanie synchronizacji czyta każdą zmienioną notatkę z bazy zespołu, a potem zapisuje nową \
                      wersję w lokalnej pamięci podręcznej, żeby następne wyszukiwanie znalazło ją od razu.";

store_cases!(
    an_empty_database_gets_the_extensions,
    a_current_database_without_them_is_upgraded_in_place,
    opening_again_changes_nothing,
    an_older_servers_writes_stay_indexed_and_get_a_language,
    an_older_servers_edit_clears_the_language_and_a_new_servers_edit_keeps_it,
    the_english_notes_setting_defaults_on_and_is_shared,
    a_database_without_counters_gets_them_and_keeps_its_notes,
    counting_a_note_changes_nothing_else_of_it,
    a_counter_never_waits_long_for_the_write_lock,
    counters_that_gave_up_leave_the_write_lock_free,
    a_counter_does_not_write_to_a_newer_schema,
    an_older_server_keeps_working_on_a_database_with_counters,
);

/// The extension objects the database has, out of the six there are.
fn objects(db: &Database) -> i64 {
    db.integer(
        "SELECT count(*) FROM sqlite_master WHERE name IN ('notes_fts_en', 'notes_fts_en_insert', \
         'notes_fts_en_delete', 'notes_fts_en_update', 'settings', 'notes_lang_reset')",
    )
}

/// The usage counter columns the database has, out of the three there are.
fn counter_columns(db: &Database) -> i64 {
    db.integer("SELECT count(*) FROM pragma_table_info('notes') WHERE name IN ('shown', 'useful', 'last_useful_at')")
}

fn has_lang_column(db: &Database) -> i64 {
    db.integer("SELECT count(*) FROM pragma_table_info('notes') WHERE name = 'lang'")
}

/// A database at the current schema version built the way the previous server did, without the
/// extensions, holding one repository note and one global note.
fn without_extensions(db: &Database) {
    for sql in [V1, V2, V3, V4, V5] {
        db.batch(sql);
    }

    db.batch(&format!(
        "INSERT INTO projects (key, root_path, remote, created_at, last_seen_at)
             VALUES ('git:{REPO}', '/work/{REPO}', '{REPO}', 1, 1);
         INSERT INTO notes (project_id, kind, title, title_norm, body, created_at, updated_at)
             VALUES (1, 'location', 'Build cache', 'build cache', 'Caches are stored in target', 1, 1);
         INSERT INTO notes (project_id, kind, title, title_norm, body, created_at, updated_at)
             VALUES (NULL, 'note', 'Editor tabs', 'editor tabs', 'Four wide, indented with spaces', 1, 1);
         PRAGMA user_version = {SCHEMA_VERSION};"
    ));
}

/// What an older server runs to save a global note: no `lang`, nothing about the English index.
fn old_insert(db: &Database, title: &str, body: &str) {
    db.batch(&format!(
        "INSERT INTO notes (project_id, kind, title, title_norm, body, created_at, updated_at)
             VALUES (NULL, 'note', '{title}', lower('{title}'), '{body}', 2, 2);"
    ));
}

fn an_empty_database_gets_the_extensions(db: &Database) {
    let store = db.open();
    drop(store);

    assert_eq!(objects(db), 6);
    assert_eq!(has_lang_column(db), 1);
    assert_eq!(db.integer("PRAGMA user_version"), SCHEMA_VERSION);
}

fn a_current_database_without_them_is_upgraded_in_place(db: &Database) {
    without_extensions(db);
    assert_eq!(objects(db), 0);

    let mut store = db.open();

    assert_eq!(objects(db), 6);
    assert_eq!(has_lang_column(db), 1);
    assert_eq!(
        db.integer("PRAGMA user_version"),
        SCHEMA_VERSION,
        "no schema version change"
    );

    // The English index was built from the notes that were already there: "caches" and "stored"
    // are found by their stems.
    assert_eq!(
        db.integer("SELECT count(*) FROM notes_fts_en WHERE notes_fts_en MATCH 'cache'"),
        1
    );
    assert_eq!(
        db.integer("SELECT count(*) FROM notes_fts_en WHERE notes_fts_en MATCH 'store'"),
        1
    );

    // Their language was filled in on open.
    assert_eq!(db.integer("SELECT count(*) FROM notes WHERE lang IS NULL"), 0);

    // The existing search is unchanged.
    let hits = store.search(&query("cache", both(REPO))).expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "Build cache");
}

fn opening_again_changes_nothing(db: &Database) {
    without_extensions(db);
    drop(db.open());

    let mut store = db.open();
    store
        .remember(&repo(REPO), &draft("Lint", ENGLISH, NoteKind::Note))
        .expect("remember");
    drop(store);

    let mut store = db.open();

    assert_eq!(objects(db), 6);
    assert_eq!(db.integer("SELECT count(*) FROM notes"), 3);
    assert_eq!(
        db.integer("SELECT count(*) FROM notes_fts_en WHERE notes_fts_en MATCH 'cache'"),
        2
    );
    assert_eq!(db.integer("SELECT count(*) FROM notes WHERE lang IS NULL"), 0);
    assert_eq!(store.search(&query("lint", both(REPO))).expect("search").len(), 1);
}

fn an_older_servers_writes_stay_indexed_and_get_a_language(db: &Database) {
    without_extensions(db);
    let mut store = db.open();

    old_insert(db, "Release steps", "Tag the commit then publish the changelog");

    // The older server's insert fired the triggers of both indexes.
    assert_eq!(
        db.integer("SELECT count(*) FROM notes_fts WHERE notes_fts MATCH 'changelog'"),
        1
    );
    assert_eq!(
        db.integer("SELECT count(*) FROM notes_fts_en WHERE notes_fts_en MATCH 'publishing'"),
        1
    );
    assert_eq!(db.integer("SELECT count(*) FROM notes WHERE lang IS NULL"), 1);

    // The newer server still reads, searches and writes the database, with no schema error.
    assert_eq!(store.search(&query("changelog", both(REPO))).expect("search").len(), 1);
    store
        .remember(&repo(REPO), &draft("Lint", ENGLISH, NoteKind::Note))
        .expect("remember");
    drop(store);

    // The next open fills the language the older server could not write.
    let _store = db.open();
    assert_eq!(db.integer("SELECT count(*) FROM notes WHERE lang IS NULL"), 0);
    assert_eq!(
        db.integer("SELECT count(*) FROM notes WHERE title = 'Release steps' AND lang IN ('en', 'unknown')"),
        1
    );
    assert_eq!(db.integer("PRAGMA user_version"), SCHEMA_VERSION);
}

fn an_older_servers_edit_clears_the_language_and_a_new_servers_edit_keeps_it(db: &Database) {
    let mut store = db.open();
    let saved = store
        .remember(&repo(REPO), &draft("Sync job", ENGLISH, NoteKind::Note))
        .expect("remember");
    let id = saved.id;

    assert_eq!(
        db.integer(&format!("SELECT count(*) FROM notes WHERE id = {id} AND lang = 'en'")),
        1
    );

    // A new server's edit that leaves the language as it was keeps it: the trigger that guards
    // against older servers' edits does not clobber a language written after the edit.
    let patch = NotePatch {
        body: Some(format!("{ENGLISH} It also drops notes it can no longer read.")),
        ..NotePatch::default()
    };
    store.update(&both(REPO), id, &patch).expect("update");
    assert_eq!(
        db.integer(&format!("SELECT count(*) FROM notes WHERE id = {id} AND lang = 'en'")),
        1
    );

    // The same through a save under the same title.
    store
        .remember(
            &repo(REPO),
            &draft("Sync job", &format!("{ENGLISH} Again."), NoteKind::Note),
        )
        .expect("save again");
    assert_eq!(
        db.integer(&format!("SELECT count(*) FROM notes WHERE id = {id} AND lang = 'en'")),
        1
    );

    // A new server's edit that changes the language writes the new one.
    let patch = NotePatch {
        body: Some(POLISH.to_owned()),
        title: Some("Synchronizacja".to_owned()),
        ..NotePatch::default()
    };
    store.update(&both(REPO), id, &patch).expect("update");
    assert_eq!(
        db.integer(&format!(
            "SELECT count(*) FROM notes WHERE id = {id} AND lang = 'other'"
        )),
        1
    );

    // An older server's edit (no `lang` in it) clears the language, so it is looked at again.
    db.batch(&format!(
        "UPDATE notes SET title = 'Sync job', title_norm = 'sync job', body = 'Rewritten in English by an old client',
             revision = revision + 1 WHERE id = {id};"
    ));
    assert_eq!(
        db.integer(&format!("SELECT count(*) FROM notes WHERE id = {id} AND lang IS NULL")),
        1
    );
    assert_eq!(
        db.integer("SELECT count(*) FROM notes_fts_en WHERE notes_fts_en MATCH 'clients'"),
        1
    );

    drop(store);
    let _store = db.open();
    assert_eq!(
        db.integer(&format!("SELECT count(*) FROM notes WHERE id = {id} AND lang IS NULL")),
        0
    );
}

fn the_english_notes_setting_defaults_on_and_is_shared(db: &Database) {
    let mut store = db.open();
    assert!(store.english_notes().expect("default"), "on when nothing is stored");

    store.set_english_notes(false).expect("set off");
    assert!(!store.english_notes().expect("off"));

    let mut other = db.open();
    assert!(!other.english_notes().expect("another server sees it"));

    other.set_english_notes(true).expect("set on");
    assert!(store.english_notes().expect("on"));
    assert_eq!(
        db.integer("SELECT count(*) FROM settings WHERE key = 'english_notes'"),
        1
    );
}

/// A database the previous server left, with its extensions (the language, the English index, the
/// settings) but without the usage counters.
fn without_counters(db: &Database) {
    without_extensions(db);
    drop(db.open());
    db.batch(
        "ALTER TABLE notes DROP COLUMN shown;
         ALTER TABLE notes DROP COLUMN useful;
         ALTER TABLE notes DROP COLUMN last_useful_at;",
    );
    assert_eq!(counter_columns(db), 0);
    assert_eq!(objects(db), 6);
}

fn a_database_without_counters_gets_them_and_keeps_its_notes(db: &Database) {
    without_counters(db);

    let mut store = db.open();

    assert_eq!(counter_columns(db), 3);
    assert_eq!(
        db.integer("PRAGMA user_version"),
        SCHEMA_VERSION,
        "no schema version change"
    );

    // Every note that was there starts uncounted.
    let page = store.list_notes(&listing(both(REPO))).expect("list");
    assert_eq!(page.total, 2);

    for row in &page.rows {
        assert_eq!(
            row.usage,
            Some(NoteUsage {
                shown: 0,
                useful: 0,
                last_useful_at: None
            }),
            "{}",
            row.title
        );
    }

    // Opening again adds nothing and changes nothing.
    store.record_shown(&[page.rows[0].id]).expect("shown");
    drop(store);

    let mut again = db.open();
    assert_eq!(counter_columns(db), 3);
    let counted = again.list_notes(&listing(both(REPO))).expect("list");
    assert_eq!(
        counted
            .rows
            .iter()
            .map(|row| row.usage.map_or(0, |usage| usage.shown))
            .sum::<i64>(),
        1
    );
}

fn counting_a_note_changes_nothing_else_of_it(db: &Database) {
    let mut store = db.open();
    let saved = store
        .remember(&repo(REPO), &draft("Sync job", ENGLISH, NoteKind::Note))
        .expect("remember");
    let id = saved.id;

    // The update time, the revision, the hits and whether the language is known.
    let state = |db: &Database| {
        ["updated_at", "revision", "hits", "lang = 'en'"]
            .map(|column| db.integer(&format!("SELECT {column} FROM notes WHERE id = {id}")))
    };
    let before = state(db);
    let indexed = "SELECT count(*) FROM notes_fts_en WHERE notes_fts_en MATCH 'sync'";
    assert_eq!(db.integer(indexed), 1);

    store.record_shown(&[id, id]).expect("shown");
    store.record_useful(&[id], 4_000).expect("useful");

    assert_eq!(state(db), before, "only the counters changed");
    assert_eq!(db.integer(indexed), 1, "the search index is as it was");
    assert_eq!(
        db.integer(&format!(
            "SELECT shown * 100 + useful * 10 + (last_useful_at = 4000) FROM notes WHERE id = {id}"
        )),
        111
    );

    // The note is still found, and an edit of its text keeps what was counted.
    assert_eq!(store.search(&query("sync", both(REPO))).expect("search").len(), 1);

    let patch = NotePatch {
        append: Some("It also retries.".to_owned()),
        ..NotePatch::default()
    };
    store.update(&both(REPO), id, &patch).expect("update");

    let note = store.peek(&both(REPO), &[id]).expect("peek").remove(0);
    assert_eq!(
        note.usage,
        Some(NoteUsage {
            shown: 1,
            useful: 1,
            last_useful_at: Some(4_000)
        })
    );
    assert_eq!(note.revision, 2);
}

/// A counter is dropped, not waited for, when another client holds the write lock: the call comes
/// back in about a second at most, whatever the server does with a writer that queues.
fn a_counter_never_waits_long_for_the_write_lock(db: &Database) {
    let mut store = db.open();
    let id = store
        .remember(&repo(REPO), &draft("Sync job", ENGLISH, NoteKind::Note))
        .expect("remember")
        .id;

    let lock = db.hold_write_lock();

    // Each counter alone is what one request waits for.
    let started = std::time::Instant::now();
    let _ = store.record_shown(&[id]);
    let shown_took = started.elapsed();

    let started = std::time::Instant::now();
    let _ = store.record_useful(&[id], 4_000);
    let useful_took = started.elapsed();
    drop(lock);

    let limit = std::time::Duration::from_millis(1_000);
    assert!(shown_took < limit, "a counter took {shown_took:?}");
    assert!(useful_took < limit, "a counter took {useful_took:?}");

    // Once the lock is free, counting works again. A libSQL server finishes the counters that gave
    // up when it can, so the count is at least this one.
    store.record_shown(&[id]).expect("shown");
    assert!(db.integer(&format!("SELECT shown FROM notes WHERE id = {id}")) >= 1);
}

/// A counter that gave up leaves nothing open on the server: once the other client lets go of the
/// write lock, a write is not held up by the counters that stopped waiting for it.
fn counters_that_gave_up_leave_the_write_lock_free(db: &Database) {
    let mut store = db.open();
    let id = store
        .remember(&repo(REPO), &draft("Sync job", ENGLISH, NoteKind::Note))
        .expect("remember")
        .id;
    let mut other = db.open();

    let lock = db.hold_write_lock();

    for round in 0..5 {
        let started = std::time::Instant::now();
        let _ = if round % 2 == 0 {
            store.record_shown(&[id])
        } else {
            store.record_useful(&[id], 4_000)
        };

        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_millis(1_000),
            "counter {round} took {took:?}"
        );
    }

    drop(lock);

    let started = std::time::Instant::now();
    other
        .remember(&repo(REPO), &draft("Release steps", "Tag the commit", NoteKind::Note))
        .expect("remember");

    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_millis(200),
        "a write waited {took:?} for the counters that gave up"
    );
}

/// A server that migrated the database since this one connected is not written to by a counter
/// (a shared server only: a local file is opened by one version at a time).
fn a_counter_does_not_write_to_a_newer_schema(db: &Database) {
    if !db.is_libsql() {
        return;
    }

    let mut store = db.open();
    let id = store
        .remember(&repo(REPO), &draft("Sync job", ENGLISH, NoteKind::Note))
        .expect("remember")
        .id;

    db.batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1));

    assert!(matches!(store.record_shown(&[id]), Err(StoreError::NewerSchema)));
    assert!(matches!(
        store.record_useful(&[id], 4_000),
        Err(StoreError::NewerSchema)
    ));
    assert_eq!(
        db.integer(&format!("SELECT shown + useful FROM notes WHERE id = {id}")),
        0
    );
}

fn an_older_server_keeps_working_on_a_database_with_counters(db: &Database) {
    drop(db.open());
    assert_eq!(counter_columns(db), 3);

    // What the previous server runs: it knows no counter column and names none.
    old_insert(db, "Release steps", "Tag the commit then publish the changelog");
    db.batch("UPDATE notes SET revision = revision + 1, updated_at = 5 WHERE title = 'Release steps';");

    assert_eq!(
        db.integer("SELECT shown + useful FROM notes WHERE title = 'Release steps'"),
        0,
        "a note it wrote starts uncounted"
    );
    assert_eq!(db.integer("SELECT count(*) FROM notes WHERE last_useful_at IS NULL"), 1);

    let mut store = db.open();
    let page = store.list_notes(&listing(both(REPO))).expect("list");
    assert_eq!(page.total, 1);
    assert_eq!(
        page.rows[0].usage,
        Some(NoteUsage {
            shown: 0,
            useful: 0,
            last_useful_at: None
        })
    );
}
