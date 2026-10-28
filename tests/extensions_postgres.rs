//! The additive schema extensions of the PostgreSQL store (see `postgres_support`): the English
//! search column, `lang` and its reset trigger are added on every open without a new schema
//! version, and an older server keeps reading and writing the upgraded database with its writes
//! still searchable. The usefulness counters of a note are added the same way, and a role that
//! cannot change the schema keeps what it has without them.

mod common;
mod postgres_support;

use common::listing::listing;
use common::{both, draft, query, repo};
use harness_memory::store::postgres::schema::SCHEMA_VERSION;
use harness_memory::store::{MemoryStore, NoteKind, NotePatch, NoteUsage, UsefulFilter};
use postgres::Client;
use postgres_support::{Database, database};
use pretty_assertions::assert_eq;

const REPO: &str = "github.com/acme/widget";

const ENGLISH: &str = "The sync job reads every changed note from the team database, then writes the new revision \
                       to the local cache so the next search finds it without another round trip.";

const POLISH: &str = "Zadanie synchronizacji czyta każdą zmienioną notatkę z bazy zespołu, a potem zapisuje nową \
                      wersję w lokalnej pamięci podręcznej, żeby następne wyszukiwanie znalazło ją od razu.";

/// Counts the rows `sql` returns in its first column.
/// The extension installs of every test take one database-wide migration lock (with a lock timeout), so tests that
/// run in parallel on one server would time each other out: they run one at a time.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

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

/// The usage counter columns the schema has, out of the three there are.
fn counter_columns(client: &mut Client) -> i64 {
    count(
        client,
        "SELECT count(*) FROM pg_attribute WHERE attrelid = to_regclass('notes')
             AND attname IN ('shown', 'useful', 'last_useful_at') AND NOT attisdropped",
    )
}

fn drop_counters(client: &mut Client) {
    client
        .batch_execute("ALTER TABLE notes DROP COLUMN shown, DROP COLUMN useful, DROP COLUMN last_useful_at;")
        .expect("drop the counters");
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
    let _serial = serial();
    let Some(db) = database() else { return };
    drop(open(&db));

    let mut admin = db.admin();
    assert_eq!(objects(&mut admin), 5);
    assert_eq!(schema_version(&mut admin), SCHEMA_VERSION);
}

#[test]
fn a_current_database_without_them_is_upgraded_in_place() {
    let _serial = serial();
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
    let _serial = serial();
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
    let _serial = serial();
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
    let _serial = serial();
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
    let _serial = serial();
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
    let _serial = serial();
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
    let _serial = serial();
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

const UNCOUNTED: NoteUsage = NoteUsage {
    shown: 0,
    useful: 0,
    last_useful_at: None,
};

#[test]
fn a_database_without_counters_gets_them_and_keeps_its_notes() {
    let _serial = serial();
    let Some(db) = database() else { return };
    let mut store = open(&db);
    store
        .remember(&repo(REPO), &draft("Lint", ENGLISH, NoteKind::Note))
        .expect("remember");
    drop(store);

    let mut admin = db.admin();
    drop_counters(&mut admin);
    assert_eq!(counter_columns(&mut admin), 0);

    let mut store = open(&db);
    assert_eq!(counter_columns(&mut admin), 3);
    assert_eq!(schema_version(&mut admin), SCHEMA_VERSION, "no schema version change");

    let page = store.list_notes(&listing(both(REPO))).expect("list");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].usage, Some(UNCOUNTED));
}

#[test]
fn counting_a_note_changes_nothing_else_of_it() {
    let _serial = serial();
    let Some(db) = database() else { return };
    let mut store = open(&db);
    let id = store
        .remember(&repo(REPO), &draft("Sync job", ENGLISH, NoteKind::Note))
        .expect("remember")
        .id;

    let mut admin = db.admin();
    let state = |admin: &mut Client| {
        ["updated_at", "revision", "hits"]
            .map(|column| count(admin, &format!("SELECT {column} FROM notes WHERE id = {id}")))
    };
    let before = state(&mut admin);

    store.record_shown(&[id, id]).expect("shown");
    store.record_useful(&[id], 4_000).expect("useful");

    assert_eq!(state(&mut admin), before, "only the counters changed");
    assert_eq!(count(&mut admin, "SELECT count(*) FROM notes WHERE lang = 'en'"), 1);
    assert_eq!(store.search(&query("sync", both(REPO))).expect("search").len(), 1);

    let note = store.peek(&both(REPO), &[id]).expect("peek").remove(0);
    assert_eq!(
        note.usage,
        Some(NoteUsage {
            shown: 1,
            useful: 1,
            last_useful_at: Some(4_000)
        })
    );
}

#[test]
fn a_counter_never_waits_long_for_a_row_another_client_holds() {
    let _serial = serial();
    let Some(db) = database() else { return };
    let mut store = open(&db);
    let id = store
        .remember(&repo(REPO), &draft("Sync job", ENGLISH, NoteKind::Note))
        .expect("remember")
        .id;

    // Another client keeps the note's row locked.
    let mut admin = db.admin();
    admin
        .batch_execute(&format!("BEGIN; UPDATE notes SET hits = hits WHERE id = {id}"))
        .expect("lock the row");

    let started = std::time::Instant::now();
    assert!(store.record_shown(&[id]).is_err(), "the count is dropped");
    assert!(store.record_useful(&[id], 4_000).is_err(), "so is this one");
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_millis(1_500),
        "two counters took {took:?}"
    );

    admin.batch_execute("ROLLBACK").expect("unlock");
    assert_eq!(
        count(&mut admin, &format!("SELECT shown + useful FROM notes WHERE id = {id}")),
        0
    );

    // A migration another test runs on the shared server can drop a count too: try again.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while store.record_shown(&[id]).is_err() {
        assert!(std::time::Instant::now() < deadline, "counting never works again");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(
        count(&mut admin, &format!("SELECT shown FROM notes WHERE id = {id}")),
        1
    );
}

#[test]
fn a_role_that_cannot_change_the_schema_keeps_the_language_without_the_counters() {
    let _serial = serial();
    let Some(db) = database() else { return };
    let mut owner = open(&db);
    owner
        .remember(&repo(REPO), &draft("Sync job", ENGLISH, NoteKind::Note))
        .expect("remember");
    drop(owner);

    let mut admin = db.admin();
    drop_counters(&mut admin);

    let name = format!("hm_readonly_{}", std::process::id());
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

    assert!(store.has_extensions(), "the language and the English index are there");
    assert_eq!(counter_columns(&mut admin), 0, "the role could not add the counters");

    // The language is shown, the counters are not, and nothing fails for the missing columns.
    let page = store.list_notes(&listing(both(REPO))).expect("list");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].usage, None);
    assert_eq!(page.rows[0].lang, Some(harness_memory::support::lang::Lang::En));

    let note = store.peek(&both(REPO), &[page.rows[0].id]).expect("peek").remove(0);
    assert_eq!(note.usage, None);

    for useful in [UsefulFilter::Ever, UsefulFilter::Never] {
        for sort in [
            harness_memory::store::NoteSort::Useful,
            harness_memory::store::NoteSort::LastUseful,
        ] {
            let filtered = store
                .list_notes(&harness_memory::store::NoteListQuery {
                    useful: Some(useful),
                    sort,
                    ..listing(both(REPO))
                })
                .expect("list");
            assert_eq!((filtered.total, filtered.rows.len()), (0, 0), "{useful:?}");
        }
    }

    store.record_shown(&[page.rows[0].id]).expect("counts nothing");
    store.record_useful(&[page.rows[0].id], 5).expect("counts nothing");
}
