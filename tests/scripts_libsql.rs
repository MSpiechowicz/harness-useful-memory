//! Scripts in a libSQL team database: the shared script cases (`common::scripts`) and the version
//! 2 to 3 migration, on the server `HARNESS_MEMORY_TEST_LIBSQL_URL` names (see
//! `libsql_support`). Without the variable every case is skipped with a message.

mod common;
mod libsql_support;

use common::scripts::{self, REPO, script};
use common::{both, query, repo};
use harness_memory::limits::{SCRIPT_BODY_MAX, SCRIPT_DESCRIPTION_MAX, SCRIPT_TIMEOUT_MAX};
use harness_memory::store::sqlite::schema::{SCHEMA_VERSION, V1, V2};
use harness_memory::store::{LibsqlStore, ScriptRef, ScriptStore, Target};
use libsql_support::Database;
use pretty_assertions::assert_eq;

/// A script store on the emptied server database, or `None` when no server is configured.
fn server() -> Option<(Database, LibsqlStore)> {
    let db = Database::libsql()?;
    let store = connect(&db);
    Some((db, store))
}

fn connect(db: &Database) -> LibsqlStore {
    let Database::Libsql(server) = db else {
        unreachable!("a libSQL database");
    };

    LibsqlStore::connect(&server.url, server.token.as_deref()).expect("connect")
}

/// `cases!(name)` runs `common::scripts::name` on the emptied server database.
macro_rules! cases {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                if let Some((_db, mut store)) = server() {
                    scripts::$name(&mut store);
                }
            }
        )*
    };
}

cases!(
    saving_upserts_by_name,
    bad_drafts_store_nothing,
    lists_repo_first_then_by_name,
    queries_match_name_or_description_ignoring_case,
    views_decide_what_is_visible,
    names_resolve_repo_first_and_delete,
    runs_are_counted,
    a_run_of_a_missing_script_is_no_error,
    deleting_many_takes_only_visible_ids,
);

#[test]
fn a_version_2_database_is_migrated_and_keeps_its_notes() {
    let Some(db) = Database::libsql() else {
        return;
    };

    db.batch(V1);
    db.batch(V2);
    db.batch(
        "INSERT INTO projects (key, root_path, remote, created_at, last_seen_at)
             VALUES ('git:github.com/acme/widget', '/work/github.com/acme/widget', 'github.com/acme/widget', 1, 1);
         INSERT INTO notes (project_id, kind, title, title_norm, body, source, created_at, updated_at, writer_root)
             VALUES (1, 'location', 'Build cache', 'build cache', 'Lives in target/', 'omp', 1, 1, '/elsewhere');
         PRAGMA user_version = 2;",
    );

    let mut notes = db.open();
    let hits = notes.search(&query("cache", both(REPO))).expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(db.integer("PRAGMA user_version"), SCHEMA_VERSION);

    let mut store = connect(&db);
    let saved = store
        .save_script(&repo(REPO), &script("open-prs", "", "gh pr list\n"))
        .expect("save");
    assert_eq!(
        saved.script.summary.project.as_deref(),
        Some("git:github.com/acme/widget")
    );
    assert_eq!(db.integer("SELECT count(*) FROM scripts"), 1);
    assert_eq!(db.integer("SELECT count(*) FROM notes"), 1);
}

#[test]
fn rows_changed_by_hand_are_clamped_or_left_out_without_breaking_the_list() {
    let Some((db, mut store)) = server() else {
        return;
    };

    for name in ["bulky", "fine", "huge", "negative", "wordy"] {
        store
            .save_script(&Target::Global, &script(name, "", "true"))
            .expect("save");
    }

    db.batch(&format!(
        "UPDATE scripts SET timeout_secs = -5 WHERE name = 'negative';
         UPDATE scripts SET timeout_secs = 9223372036854775807 WHERE name = 'huge';
         UPDATE scripts SET description = '{}' WHERE name = 'wordy';
         UPDATE scripts SET body = '{}' WHERE name = 'bulky';",
        "d".repeat(SCRIPT_DESCRIPTION_MAX + 1),
        "a".repeat(SCRIPT_BODY_MAX + 1),
    ));

    let listed: Vec<(String, u64)> = store
        .list_scripts(&both(REPO), None)
        .expect("one bad row does not fail the list")
        .into_iter()
        .map(|summary| (summary.name, summary.timeout_secs))
        .collect();
    assert_eq!(
        listed,
        [
            ("fine".to_owned(), 60),
            ("huge".to_owned(), SCRIPT_TIMEOUT_MAX),
            ("negative".to_owned(), 1),
        ]
    );

    for name in ["bulky", "wordy"] {
        let found = store
            .get_script(&both(REPO), &ScriptRef::Name(name.to_owned()))
            .expect("get");
        assert_eq!(found, None, "{name}");
    }
}
