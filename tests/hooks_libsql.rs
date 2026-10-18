//! Team hooks in a libSQL team database: the shared hook cases (`common::hooks`), the version 3 to
//! 4 migration and rows changed by hand, on the server `HARNESS_MEMORY_TEST_LIBSQL_URL` names (see
//! `libsql_support`). Without the variable every case is skipped with a message.

mod common;
mod libsql_support;

use common::hooks::{self, WIDGET, draft, id};
use common::scripts::script;
use harness_memory::limits::HOOK_BODY_MAX;
use harness_memory::store::sqlite::schema::{SCHEMA_VERSION, V1, V2, V3};
use harness_memory::store::{HookPut, HookStore, LibsqlStore, ScriptStore, Target};
use libsql_support::Database;
use pretty_assertions::assert_eq;

/// A hook store on the emptied server database, or `None` when no server is configured.
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

/// `cases!(name)` runs `common::hooks::name` on the emptied server database.
macro_rules! cases {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                if let Some((_db, mut store)) = server() {
                    hooks::$name(&mut store);
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
    a_scope_holds_at_most_64_hooks,
    bad_drafts_store_nothing,
);

#[test]
fn a_version_3_database_is_migrated_and_keeps_its_scripts() {
    let Some(db) = Database::libsql() else {
        return;
    };

    db.batch(V1);
    db.batch(V2);
    db.batch(V3);
    db.batch("PRAGMA user_version = 3;");

    let mut store = connect(&db);
    assert_eq!(db.integer("PRAGMA user_version"), SCHEMA_VERSION);

    store
        .save_script(&Target::Global, &script("open-prs", "", "gh pr list\n"))
        .expect("scripts still work");

    let put = store
        .put_hook(&draft(&id(1), Some(WIDGET), "Repo rules"), None)
        .expect("put");
    assert!(matches!(put, HookPut::Saved(_)), "{put:?}");
    assert_eq!(db.integer("SELECT count(*) FROM hooks"), 1);
    assert_eq!(db.integer("SELECT count(*) FROM scripts"), 1);
}

#[test]
fn rows_changed_by_hand_are_left_out_without_breaking_the_list() {
    let Some((db, mut store)) = server() else {
        return;
    };

    for n in 1..=2 {
        store
            .put_hook(&draft(&id(n), None, &format!("Hook {n}")), None)
            .expect("put");
    }

    db.batch(&format!(
        "UPDATE hooks SET body = '{}' WHERE id = '{}';",
        "a".repeat(HOOK_BODY_MAX + 1),
        id(2),
    ));

    let page = store
        .list_hooks(None, 0, 50)
        .expect("one bad row does not fail the list");
    let ids: Vec<String> = page.hooks.into_iter().map(|hook| hook.id).collect();
    assert_eq!((ids, page.total), (vec![id(1)], 2));
}
