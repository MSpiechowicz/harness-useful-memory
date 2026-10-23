//! The additive schema extensions of the SQLite-dialect stores (the English index, the `lang` column
//! and the settings) on both backends (see `libsql_support`): they are added on every open without
//! a new schema version, and an older server keeps reading and writing the upgraded database with
//! its writes still indexed.

mod common;
mod libsql_support;

use common::{both, draft, query, repo};
use harness_memory::store::sqlite::schema::{SCHEMA_VERSION, V1, V2, V3, V4, V5};
use harness_memory::store::{NoteKind, NotePatch};
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
);

/// The extension objects the database has, out of the six there are.
fn objects(db: &Database) -> i64 {
    db.integer(
        "SELECT count(*) FROM sqlite_master WHERE name IN ('notes_fts_en', 'notes_fts_en_insert', \
         'notes_fts_en_delete', 'notes_fts_en_update', 'settings', 'notes_lang_reset')",
    )
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
