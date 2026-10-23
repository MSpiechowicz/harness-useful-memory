//! The additive schema extensions of the PostgreSQL store (see `postgres_support`): the English
//! search column, `lang` and its reset trigger are added on every open without a new schema
//! version, and an older server keeps reading and writing the upgraded database with its writes
//! still searchable.

mod common;
mod postgres_support;

use common::{both, draft, query, repo};
use harness_memory::store::postgres::schema::SCHEMA_VERSION;
use harness_memory::store::{MemoryStore, NoteKind, NotePatch};
use postgres::Client;
use postgres_support::{Database, database};
use pretty_assertions::assert_eq;

const REPO: &str = "github.com/acme/widget";

const ENGLISH: &str = "The sync job reads every changed note from the team database, then writes the new revision \
                       to the local cache so the next search finds it without another round trip.";

const POLISH: &str = "Zadanie synchronizacji czyta każdą zmienioną notatkę z bazy zespołu, a potem zapisuje nową \
                      wersję w lokalnej pamięci podręcznej, żeby następne wyszukiwanie znalazło ją od razu.";

/// Counts the rows `sql` returns in its first column.
fn count(client: &mut Client, sql: &str) -> i64 {
    client.query_one(sql, &[]).expect("query").get(0)
}

/// The extension objects the schema has, out of the five there are.
fn objects(client: &mut Client) -> i64 {
    count(
        client,
        "SELECT (SELECT count(*) FROM pg_attribute WHERE attrelid = to_regclass('notes')
                     AND attname IN ('search_en', 'lang') AND NOT attisdropped)
              + (to_regclass('notes_search_en') IS NOT NULL)::int
              + (to_regprocedure('harness_memory_lang_reset()') IS NOT NULL)::int
              + (SELECT count(*) FROM pg_trigger WHERE tgrelid = to_regclass('notes')
                     AND tgname = 'notes_lang_reset')",
    )
}

fn schema_version(client: &mut Client) -> i64 {
    count(
        client,
        "SELECT value FROM harness_memory_meta WHERE key = 'schema_version'",
    )
}

/// Takes the extensions out again: the database as the previous server left it.
fn without_extensions(client: &mut Client) {
    client
        .batch_execute(
            "DROP TRIGGER notes_lang_reset ON notes;
             DROP FUNCTION harness_memory_lang_reset();
             DROP INDEX notes_search_en;
             ALTER TABLE notes DROP COLUMN search_en, DROP COLUMN lang;",
        )
        .expect("drop the extensions");
}

/// What an older server runs to save a global note: no `lang`, no `search_en`.
fn old_insert(client: &mut Client, title: &str, body: &str) {
    client
        .execute(
            "INSERT INTO notes (project_id, kind, title, title_norm, body, created_at, updated_at,
                                search_title, search_body)
             VALUES (NULL, 'note', $1, lower($1), $2, 2, 2, lower($1), lower($2))",
            &[&title, &body],
        )
        .expect("an older server's insert");
}

fn open(db: &Database) -> harness_memory::store::PostgresStore {
    db.open()
}

#[test]
fn an_empty_database_gets_the_extensions() {
    let Some(db) = database() else { return };
    drop(open(&db));

    let mut admin = db.admin();
    assert_eq!(objects(&mut admin), 5);
    assert_eq!(schema_version(&mut admin), SCHEMA_VERSION);
}

#[test]
fn a_current_database_without_them_is_upgraded_in_place() {
    let Some(db) = database() else { return };
    let mut store = open(&db);
    store
        .remember(
            &repo(REPO),
            &draft("Build cache", "Caches are stored in target", NoteKind::Location),
        )
        .expect("remember");
    drop(store);

    let mut admin = db.admin();
    without_extensions(&mut admin);
    assert_eq!(objects(&mut admin), 0);

    let mut store = open(&db);
    assert_eq!(objects(&mut admin), 5);
    assert_eq!(schema_version(&mut admin), SCHEMA_VERSION, "no schema version change");

    // The English column was generated from the notes that were already there.
    assert_eq!(
        count(
            &mut admin,
            "SELECT count(*) FROM notes WHERE search_en @@ plainto_tsquery('english', 'caching')"
        ),
        1
    );
    assert_eq!(
        count(&mut admin, "SELECT count(*) FROM notes WHERE lang IS NULL"),
        0,
        "language backfilled"
    );

    let hits = store.search(&query("cache", both(REPO))).expect("search");
    assert_eq!(hits.len(), 1);
}

#[test]
fn opening_again_changes_nothing() {
    let Some(db) = database() else { return };
    drop(open(&db));
    let mut store = open(&db);
    store
        .remember(&repo(REPO), &draft("Lint", ENGLISH, NoteKind::Note))
        .expect("remember");
    drop(store);

    let mut store = open(&db);
    let mut admin = db.admin();

    assert_eq!(objects(&mut admin), 5);
    assert_eq!(count(&mut admin, "SELECT count(*) FROM notes"), 1);
    assert_eq!(count(&mut admin, "SELECT count(*) FROM notes WHERE lang = 'en'"), 1);
    assert_eq!(store.search(&query("lint", both(REPO))).expect("search").len(), 1);
}

#[test]
fn an_older_servers_writes_stay_searchable_and_get_a_language() {
    let Some(db) = database() else { return };
    let mut store = open(&db);
    let mut admin = db.admin();

    old_insert(&mut admin, "Release steps", "Tag the commit then publish the changelog");

    // The generated column indexed the older server's insert.
    assert_eq!(
        count(
            &mut admin,
            "SELECT count(*) FROM notes WHERE search_en @@ plainto_tsquery('english', 'publishing')"
        ),
        1
    );
    assert_eq!(count(&mut admin, "SELECT count(*) FROM notes WHERE lang IS NULL"), 1);

    // The newer server still reads, searches and writes the database, with no schema error.
    assert_eq!(store.search(&query("changelog", both(REPO))).expect("search").len(), 1);
    store
        .remember(&repo(REPO), &draft("Lint", ENGLISH, NoteKind::Note))
        .expect("remember");
    drop(store);

    // The next open fills the language the older server could not write.
    let _store = open(&db);
    assert_eq!(count(&mut admin, "SELECT count(*) FROM notes WHERE lang IS NULL"), 0);
    assert_eq!(schema_version(&mut admin), SCHEMA_VERSION);
}

#[test]
fn an_older_servers_edit_clears_the_language_and_a_new_servers_edit_keeps_it() {
    let Some(db) = database() else { return };
    let mut store = open(&db);
    let mut admin = db.admin();

    let id = store
        .remember(&repo(REPO), &draft("Sync job", ENGLISH, NoteKind::Note))
        .expect("remember")
        .id;
    let has = |admin: &mut Client, lang: &str| {
        count(
            admin,
            &format!("SELECT count(*) FROM notes WHERE id = {id} AND lang = '{lang}'"),
        )
    };

    assert_eq!(has(&mut admin, "en"), 1);

    // A new server's edit that leaves the language as it was keeps it: the trigger that guards
    // against older servers' edits does not clobber a language written after the edit.
    let patch = NotePatch {
        body: Some(format!("{ENGLISH} It also drops notes it can no longer read.")),
        ..NotePatch::default()
    };
    store.update(&both(REPO), id, &patch).expect("update");
    assert_eq!(has(&mut admin, "en"), 1);

    store
        .remember(
            &repo(REPO),
            &draft("Sync job", &format!("{ENGLISH} Again."), NoteKind::Note),
        )
        .expect("save again");
    assert_eq!(has(&mut admin, "en"), 1);

    // A new server's edit that changes the language writes the new one.
    let patch = NotePatch {
        title: Some("Synchronizacja".to_owned()),
        body: Some(POLISH.to_owned()),
        ..NotePatch::default()
    };
    store.update(&both(REPO), id, &patch).expect("update");
    assert_eq!(has(&mut admin, "other"), 1);

    // An older server's edit (no `lang` in it) clears the language, so it is looked at again.
    admin
        .execute(
            "UPDATE notes SET title = 'Sync job', title_norm = 'sync job', body = 'Rewritten in English by an old client',
                 revision = revision + 1, search_title = 'sync job',
                 search_body = 'rewritten in english by an old client' WHERE id = $1",
            &[&id],
        )
        .expect("an older server's edit");
    assert_eq!(
        count(
            &mut admin,
            &format!("SELECT count(*) FROM notes WHERE id = {id} AND lang IS NULL")
        ),
        1
    );
    assert_eq!(
        count(
            &mut admin,
            "SELECT count(*) FROM notes WHERE search_en @@ plainto_tsquery('english', 'clients')"
        ),
        1
    );

    drop(store);
    let _store = open(&db);
    assert_eq!(
        count(
            &mut admin,
            &format!("SELECT count(*) FROM notes WHERE id = {id} AND lang IS NULL")
        ),
        0
    );
}

#[test]
fn the_english_notes_setting_defaults_on_and_is_shared() {
    let Some(db) = database() else { return };
    let mut store = open(&db);
    assert!(store.english_notes().expect("default"), "on when nothing is stored");

    store.set_english_notes(false).expect("set off");
    assert!(!store.english_notes().expect("off"));

    let mut other = open(&db);
    assert!(!other.english_notes().expect("another server sees it"));

    other.set_english_notes(true).expect("set on");
    assert!(store.english_notes().expect("on"));

    let mut admin = db.admin();
    assert_eq!(
        count(
            &mut admin,
            "SELECT value FROM harness_memory_meta WHERE key = 'english_notes'"
        ),
        1
    );
}

/// A login role without the right to change the schema; dropped on drop.
struct LimitedRole {
    admin: Client,
    name: String,
}

impl Drop for LimitedRole {
    fn drop(&mut self) {
        let name = &self.name;
        self.admin
            .batch_execute(&format!("DROP OWNED BY {name}; DROP ROLE IF EXISTS {name};"))
            .ok();
    }
}

#[test]
fn a_role_that_cannot_change_the_schema_runs_without_the_extensions() {
    let Some(db) = database() else { return };
    drop(open(&db));

    let mut admin = db.admin();
    without_extensions(&mut admin);

    // A role of its own per run, with the rights of a team member who only reads and writes notes.
    let name = format!("hm_limited_{}", std::process::id());
    let schema = count_text(&mut admin, "SELECT current_schema()");
    admin
        .batch_execute(&format!(
            "CREATE ROLE {name} LOGIN PASSWORD 'limited';
             GRANT USAGE ON SCHEMA \"{schema}\" TO {name};
             GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA \"{schema}\" TO {name};
             GRANT USAGE ON ALL SEQUENCES IN SCHEMA \"{schema}\" TO {name};"
        ))
        .expect("create the limited role");
    let _role = LimitedRole {
        admin: db.admin(),
        name: name.clone(),
    };

    let url = db.url();
    let (scheme, rest) = url.split_once("://").expect("a URL");
    let (_, host) = rest.split_once('@').expect("a URL with a user");
    let mut store =
        harness_memory::store::PostgresStore::connect(&format!("{scheme}://{name}:limited@{host}")).expect("connect");

    assert!(!store.has_extensions());
    assert_eq!(objects(&mut admin), 0, "nothing was added");

    // Notes are saved and found as before, with no language stored.
    store
        .remember(&repo(REPO), &draft("Lint", ENGLISH, NoteKind::Note))
        .expect("remember");
    assert_eq!(store.search(&query("lint", both(REPO))).expect("search").len(), 1);
    assert_eq!(count(&mut admin, "SELECT count(*) FROM notes"), 1);
}

/// A table lock another session holds (a long transaction, a dump) makes the `ALTER TABLE` wait
/// and block everything queued behind it: the open gives up after a few seconds, works without the
/// extensions, and adds them at the next open once the lock is gone.
#[test]
fn a_table_lock_held_elsewhere_makes_the_open_run_without_the_extensions_for_now() {
    let Some(db) = database() else { return };
    drop(open(&db));

    let mut admin = db.admin();
    without_extensions(&mut admin);

    let mut holder = db.admin();
    holder
        .batch_execute("BEGIN; SELECT count(*) FROM notes;")
        .expect("a transaction that has read notes");

    let started = std::time::Instant::now();
    let mut store = open(&db);

    assert!(!store.has_extensions());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(9),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(objects(&mut admin), 0, "nothing was added");
    assert!(
        store.search(&query("anything", both(REPO))).is_ok(),
        "searches work as before"
    );
    drop(store);

    holder.batch_execute("COMMIT").expect("release");
    let store = open(&db);

    assert!(store.has_extensions());
    assert_eq!(objects(&mut admin), 5);
}

fn count_text(client: &mut Client, sql: &str) -> String {
    client.query_one(sql, &[]).expect("query").get(0)
}
