//! Team routines in the local SQLite file (where only these tests fill the table): the shared
//! routine cases (`common::routines`), the version 4 to 5 migration of a database holding team
//! hooks, and rows changed by hand.

mod common;

use common::routines::{self, WIDGET, draft, id};
use common::temp_store;
use harness_memory::limits::ROUTINE_BODY_MAX;
use harness_memory::store::sqlite::schema::{SCHEMA_VERSION, V1, V2, V3, V4, user_version};
use harness_memory::store::{HookStore, RoutinePut, RoutineStore, SqliteStore};
use pretty_assertions::assert_eq;
use tempfile::TempDir;

/// `cases!(name)` runs `common::routines::name` on a fresh local file.
macro_rules! cases {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                let (_dir, mut store) = temp_store();
                routines::$name(&mut store);
            }
        )*
    };
}

cases!(
    puts_insert_and_read_back,
    an_insert_of_a_taken_id_changes_nothing,
    updates_need_the_current_sha,
    lists_global_first_then_by_creation,
    deletes_honour_the_expected_sha,
    a_scope_holds_at_most_64_routines,
    bad_drafts_store_nothing,
    deleting_mine_keeps_other_authors_rows,
    hooks_and_routines_are_kept_apart,
);

#[test]
fn every_case_passes_through_conformance() {
    let mut dirs: Vec<TempDir> = Vec::new();

    routines::conformance(&mut || {
        let (dir, store) = temp_store();
        dirs.push(dir);
        Box::new(store)
    });
}

/// A team hook row as version 4 wrote it.
fn hook_row(n: u32) -> String {
    let body = common::hooks::body(&id(n), None, "Kept hook").replace('\'', "''");

    format!(
        "INSERT INTO hooks (id, repo, body, sha256, author, created_at, updated_at)
         VALUES ('{}', NULL, '{body}', 'x', 'grace', 1700000000, 1700000000);",
        id(n)
    )
}

#[test]
fn a_version_4_database_with_hooks_is_migrated_and_keeps_them() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("memory.sqlite3");

    let conn = rusqlite::Connection::open(&path).expect("raw");
    for step in [V1, V2, V3, V4] {
        conn.execute_batch(step).expect("schema step");
    }
    conn.execute_batch(&format!("{} PRAGMA user_version = 4;", hook_row(7)))
        .expect("v4 rows");
    drop(conn);

    let mut store = SqliteStore::open(&path).expect("open migrates");

    let hook = store.get_hook(&id(7)).expect("get").expect("the hook is kept");
    assert_eq!((hook.author.as_str(), hook.created_at), ("grace", 1_700_000_000));

    let put = store
        .put_routine(&draft(&id(1), Some(WIDGET), "Repo triage"), None)
        .expect("put");
    assert!(matches!(put, RoutinePut::Saved(_)), "{put:?}");
    drop(store);

    let conn = rusqlite::Connection::open(&path).expect("raw");
    assert_eq!(user_version(&conn).expect("version"), SCHEMA_VERSION);
    assert_eq!(SCHEMA_VERSION, 5);

    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |row| row.get(0)).expect("count") };
    assert_eq!(count("SELECT count(*) FROM routines"), 1);
    assert_eq!(count("SELECT count(*) FROM hooks"), 1);
    assert_eq!(
        count("SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'routines_repo'"),
        1
    );
}

#[test]
fn rows_changed_by_hand_are_left_out_without_breaking_the_list() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("memory.sqlite3");
    let mut store = SqliteStore::open(&path).expect("open");

    for n in 1..=5 {
        store
            .put_routine(&draft(&id(n), None, &format!("Routine {n}")), None)
            .expect("put");
    }

    let conn = rusqlite::Connection::open(&path).expect("raw");
    conn.execute_batch(&format!(
        "UPDATE routines SET body = '{}' WHERE id = '{}';
         UPDATE routines SET id = 'not-a-routine-id' WHERE id = '{}';
         UPDATE routines SET repo = '{WIDGET}' WHERE id = '{}';
         UPDATE routines SET body = replace(body, '\"weekdays\"', '\"fortnightly\"') WHERE id = '{}';",
        "a".repeat(ROUTINE_BODY_MAX + 1),
        id(2),
        id(3),
        id(4),
        id(5),
    ))
    .expect("tamper");

    let page = store
        .list_routines(Some(WIDGET), 0, 50)
        .expect("one bad row does not fail the list");
    let ids: Vec<String> = page.routines.into_iter().map(|routine| routine.id).collect();
    assert_eq!(ids, [id(1)]);
    assert_eq!(page.total, 5, "the total counts the rows in the database");

    for n in [2, 4, 5] {
        assert_eq!(store.get_routine(&id(n)).expect("get"), None, "{n}");
    }

    let deleted = store.delete_routines_by("ada").expect("delete mine");
    assert_eq!(deleted.len(), 5, "rows left out are still the author's: {deleted:?}");
    assert!(deleted.contains(&"not-a-routine-id".to_owned()), "{deleted:?}");
}
