//! Team hooks in the local SQLite file (where only these tests fill the table): the shared hook
//! cases (`common::hooks`), the version 3 to 4 migration, and rows changed by hand.

mod common;

use common::hooks::{self, WIDGET, draft, id};
use common::temp_store;
use harness_memory::limits::HOOK_BODY_MAX;
use harness_memory::store::sqlite::schema::{SCHEMA_VERSION, V1, V2, V3, user_version};
use harness_memory::store::{HookPut, HookStore, Language, ScriptRef, ScriptStore, SqliteStore, View};
use pretty_assertions::assert_eq;
use tempfile::TempDir;

/// `cases!(name)` runs `common::hooks::name` on a fresh local file.
macro_rules! cases {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                let (_dir, mut store) = temp_store();
                hooks::$name(&mut store);
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
    a_scope_holds_at_most_64_hooks,
    bad_drafts_store_nothing,
);

#[test]
fn every_case_passes_through_conformance() {
    let mut dirs: Vec<TempDir> = Vec::new();

    hooks::conformance(&mut || {
        let (dir, store) = temp_store();
        dirs.push(dir);
        Box::new(store)
    });
}

#[test]
fn a_version_3_database_is_migrated_and_keeps_its_scripts() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("memory.sqlite3");

    let conn = rusqlite::Connection::open(&path).expect("raw");
    for step in [V1, V2, V3] {
        conn.execute_batch(step).expect("schema step");
    }
    conn.execute_batch(
        "INSERT INTO scripts (name, language, body, sha256, timeout_secs, created_at, updated_at)
             VALUES ('open-prs', 'bash', 'gh pr list', 'x', 60, 1, 1);
         PRAGMA user_version = 3;",
    )
    .expect("v3 rows");
    drop(conn);

    let mut store = SqliteStore::open(&path).expect("open migrates");
    let script = store
        .get_script(
            &View {
                repo: None,
                global: true,
            },
            &ScriptRef::Name("open-prs".to_owned()),
        )
        .expect("get")
        .expect("kept");
    assert_eq!(
        (script.body.as_str(), script.summary.language),
        ("gh pr list", Language::Bash)
    );

    let put = store
        .put_hook(&draft(&id(1), Some(WIDGET), "Repo rules"), None)
        .expect("put");
    assert!(matches!(put, HookPut::Saved(_)), "{put:?}");
    drop(store);

    let conn = rusqlite::Connection::open(&path).expect("raw");
    assert_eq!(user_version(&conn).expect("version"), SCHEMA_VERSION);
    assert_eq!(SCHEMA_VERSION, 5);
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM hooks WHERE repo = ?1", [WIDGET], |row| row.get(0))
        .expect("count");
    assert_eq!(rows, 1);
}

#[test]
fn rows_changed_by_hand_are_left_out_without_breaking_the_list() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("memory.sqlite3");
    let mut store = SqliteStore::open(&path).expect("open");

    for n in 1..=6 {
        store
            .put_hook(&draft(&id(n), None, &format!("Hook {n}")), None)
            .expect("put");
    }

    let conn = rusqlite::Connection::open(&path).expect("raw");
    conn.execute_batch(&format!(
        "UPDATE hooks SET body = '{}' WHERE id = '{}';
         UPDATE hooks SET id = 'not-a-hook-id' WHERE id = '{}';
         UPDATE hooks SET repo = '{WIDGET}' WHERE id = '{}';
         UPDATE hooks SET author = 'ada' || char(8238) WHERE id = '{}';
         UPDATE hooks SET body = replace(body, '{}', '{}') WHERE id = '{}';",
        "a".repeat(HOOK_BODY_MAX + 1),
        id(2),
        id(3),
        id(4),
        id(5),
        id(6),
        id(7),
        id(6),
    ))
    .expect("tamper");

    let page = store
        .list_hooks(Some(WIDGET), 0, 50)
        .expect("one bad row does not fail the list");
    let ids: Vec<String> = page.hooks.into_iter().map(|hook| hook.id).collect();
    assert_eq!(ids, [id(1)]);
    assert_eq!(page.total, 6, "the total counts the rows in the database");

    let first = store.list_hooks(Some(WIDGET), 1, 2).expect("page");
    assert_eq!(
        (first.hooks.len(), first.next_offset),
        (0, Some(3)),
        "left out rows still move the offset"
    );

    for n in [2, 4, 5, 6] {
        assert_eq!(store.get_hook(&id(n)).expect("get"), None, "{n}");
    }
}
