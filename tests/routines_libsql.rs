//! Team routines in a libSQL team database: the shared routine cases (`common::routines`), the
//! version 4 to 5 migration of a database holding team hooks and rows changed by hand, on the
//! server `HARNESS_MEMORY_TEST_LIBSQL_URL` names (see `libsql_support`). Without the variable
//! every case is skipped with a message.

mod common;
mod libsql_support;

use common::routines::{self, WIDGET, draft, id};
use harness_memory::limits::ROUTINE_BODY_MAX;
use harness_memory::store::sqlite::schema::{SCHEMA_VERSION, V1, V2, V3, V4};
use harness_memory::store::{HookStore, LibsqlStore, RoutinePut, RoutineStore};
use libsql_support::Database;
use pretty_assertions::assert_eq;

/// A routine store on the emptied server database, or `None` when no server is configured.
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

/// `cases!(name)` runs `common::routines::name` on the emptied server database.
macro_rules! cases {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                if let Some((_db, mut store)) = server() {
                    routines::$name(&mut store);
                }
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
fn a_version_4_database_with_hooks_is_migrated_and_keeps_them() {
    let Some(db) = Database::libsql() else {
        return;
    };

    db.batch(V1);
    db.batch(V2);
    db.batch(V3);
    db.batch(V4);

    let hook = common::hooks::body(&id(7), None, "Kept hook").replace('\'', "''");
    db.batch(&format!(
        "INSERT INTO hooks (id, repo, body, sha256, author, created_at, updated_at)
         VALUES ('{}', NULL, '{hook}', 'x', 'grace', 1700000000, 1700000000);
         PRAGMA user_version = 4;",
        id(7)
    ));

    let mut store = connect(&db);
    assert_eq!(db.integer("PRAGMA user_version"), SCHEMA_VERSION);

    let kept = store.get_hook(&id(7)).expect("get").expect("the hook is kept");
    assert_eq!(kept.author, "grace");

    let put = store
        .put_routine(&draft(&id(1), Some(WIDGET), "Repo triage"), None)
        .expect("put");
    assert!(matches!(put, RoutinePut::Saved(_)), "{put:?}");
    assert_eq!(db.integer("SELECT count(*) FROM routines"), 1);
    assert_eq!(db.integer("SELECT count(*) FROM hooks"), 1);
}

#[test]
fn rows_changed_by_hand_are_left_out_without_breaking_the_list() {
    let Some((db, mut store)) = server() else {
        return;
    };

    for n in 1..=2 {
        store
            .put_routine(&draft(&id(n), None, &format!("Routine {n}")), None)
            .expect("put");
    }

    db.batch(&format!(
        "UPDATE routines SET body = '{}' WHERE id = '{}';",
        "a".repeat(ROUTINE_BODY_MAX + 1),
        id(2),
    ));

    let page = store
        .list_routines(None, 0, 50)
        .expect("one bad row does not fail the list");
    let ids: Vec<String> = page.routines.into_iter().map(|routine| routine.id).collect();
    assert_eq!((ids, page.total), (vec![id(1)], 2));
}
