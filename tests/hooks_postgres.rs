//! Team hooks in the PostgreSQL store: the shared hook cases (`common::hooks`), the schema 2 to 3
//! migration, the database role of each write and the newer-schema guard, one schema per test.
//! They skip unless `HARNESS_MEMORY_TEST_POSTGRES_URL` names a server (see `postgres_support`).

mod common;
mod postgres_support;

use common::hooks::{WIDGET, draft, id};
use common::scripts::script;
use harness_memory::store::postgres::schema::SCHEMA_VERSION;
use harness_memory::store::{HookPut, HookStore, ScriptStore, StoreError, Target};
use postgres_support::database;
use pretty_assertions::assert_eq;

/// Runs the shared case `case` on a fresh schema.
fn conforms(case: common::hooks::Case) {
    let Some(db) = database() else { return };
    case(&mut db.open());
}

#[test]
fn postgres_hooks_puts_insert_and_read_back() {
    conforms(common::hooks::puts_insert_and_read_back);
}

#[test]
fn postgres_hooks_an_insert_of_a_taken_id_changes_nothing() {
    conforms(common::hooks::an_insert_of_a_taken_id_changes_nothing);
}

#[test]
fn postgres_hooks_updates_need_the_current_sha() {
    conforms(common::hooks::updates_need_the_current_sha);
}

#[test]
fn postgres_hooks_lists_global_first_then_by_creation() {
    conforms(common::hooks::lists_global_first_then_by_creation);
}

#[test]
fn postgres_hooks_deletes_honour_the_expected_sha() {
    conforms(common::hooks::deletes_honour_the_expected_sha);
}

#[test]
fn postgres_hooks_a_scope_holds_at_most_64_hooks() {
    conforms(common::hooks::a_scope_holds_at_most_64_hooks);
}

#[test]
fn postgres_hooks_bad_drafts_store_nothing() {
    conforms(common::hooks::bad_drafts_store_nothing);
}

#[test]
fn a_schema_2_database_migrates_to_schema_3_and_keeps_its_scripts() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    store
        .save_script(&Target::Global, &script("open-prs", "", "gh pr list\n"))
        .expect("script");
    drop(store);

    // Back to schema 2: the same tables without `hooks`.
    db.admin()
        .batch_execute("DROP TABLE hooks; UPDATE harness_memory_meta SET value = 2 WHERE key = 'schema_version'")
        .expect("downgrade");

    let mut store = db.open();
    assert!(!store.is_newer());

    let version: i64 = db
        .admin()
        .query_one(
            "SELECT value FROM harness_memory_meta WHERE key = 'schema_version'",
            &[],
        )
        .expect("version")
        .get(0);
    assert_eq!((version, SCHEMA_VERSION), (3, 3));

    let scripts: i64 = db
        .admin()
        .query_one("SELECT count(*) FROM scripts", &[])
        .expect("count")
        .get(0);
    assert_eq!(scripts, 1);

    let put = store
        .put_hook(&draft(&id(1), Some(WIDGET), "Repo rules"), None)
        .expect("put");
    assert!(matches!(put, HookPut::Saved(_)), "{put:?}");
}

#[test]
fn every_hook_write_records_the_database_role() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    store.put_hook(&draft(&id(1), None, "Global"), None).expect("put");

    let row = db
        .admin()
        .query_one("SELECT written_by = current_user FROM hooks", &[])
        .expect("row");
    assert!(row.get::<_, bool>(0));
}

#[test]
fn a_newer_schema_refuses_hooks() {
    let Some(db) = database() else { return };
    drop(db.open());

    db.admin()
        .execute(
            "UPDATE harness_memory_meta SET value = $1 WHERE key = 'schema_version'",
            &[&(SCHEMA_VERSION + 1)],
        )
        .expect("bump");

    let mut store = db.open();
    assert!(matches!(
        store.put_hook(&draft(&id(1), None, "Global"), None),
        Err(StoreError::NewerSchema)
    ));
    assert!(matches!(store.list_hooks(None, 0, 50), Err(StoreError::NewerSchema)));
    assert!(matches!(store.get_hook(&id(1)), Err(StoreError::NewerSchema)));
    assert!(matches!(store.delete_hook(&id(1), None), Err(StoreError::NewerSchema)));
}
