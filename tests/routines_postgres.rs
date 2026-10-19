//! Team routines in the PostgreSQL store: the shared routine cases (`common::routines`), the
//! schema 3 to 4 migration of a database holding team hooks, the database role of each write and
//! the newer-schema guard, one schema per test. They skip unless
//! `HARNESS_MEMORY_TEST_POSTGRES_URL` names a server (see `postgres_support`).

mod common;
mod postgres_support;

use common::routines::{WIDGET, draft, id};
use harness_memory::store::postgres::schema::SCHEMA_VERSION;
use harness_memory::store::{HookPut, HookStore, RoutinePut, RoutineStore, StoreError};
use postgres_support::database;
use pretty_assertions::assert_eq;

/// Runs the shared case `case` on a fresh schema.
fn conforms(case: common::routines::Case) {
    let Some(db) = database() else { return };
    case(&mut db.open());
}

#[test]
fn postgres_routines_puts_insert_and_read_back() {
    conforms(common::routines::puts_insert_and_read_back);
}

#[test]
fn postgres_routines_an_insert_of_a_taken_id_changes_nothing() {
    conforms(common::routines::an_insert_of_a_taken_id_changes_nothing);
}

#[test]
fn postgres_routines_updates_need_the_current_sha() {
    conforms(common::routines::updates_need_the_current_sha);
}

#[test]
fn postgres_routines_lists_global_first_then_by_creation() {
    conforms(common::routines::lists_global_first_then_by_creation);
}

#[test]
fn postgres_routines_deletes_honour_the_expected_sha() {
    conforms(common::routines::deletes_honour_the_expected_sha);
}

#[test]
fn postgres_routines_a_scope_holds_at_most_64_routines() {
    conforms(common::routines::a_scope_holds_at_most_64_routines);
}

#[test]
fn postgres_routines_bad_drafts_store_nothing() {
    conforms(common::routines::bad_drafts_store_nothing);
}

#[test]
fn postgres_routines_deleting_mine_keeps_other_authors_rows() {
    conforms(common::routines::deleting_mine_keeps_other_authors_rows);
}

#[test]
fn postgres_routines_hooks_and_routines_are_kept_apart() {
    conforms(common::routines::hooks_and_routines_are_kept_apart);
}

#[test]
fn a_schema_3_database_with_hooks_migrates_to_schema_4_and_keeps_them() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let hook = common::hooks::draft(&id(7), Some(WIDGET), "Kept hook");
    assert!(matches!(store.put_hook(&hook, None), Ok(HookPut::Saved(_))));
    drop(store);

    // Back to schema 3: the same tables without `routines`, a hook row kept.
    db.admin()
        .batch_execute("DROP TABLE routines; UPDATE harness_memory_meta SET value = 3 WHERE key = 'schema_version'")
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
    assert_eq!((version, SCHEMA_VERSION), (4, 4));

    let kept = store.get_hook(&id(7)).expect("get").expect("the hook is kept");
    assert_eq!(kept.body, hook.body);

    let put = store
        .put_routine(&draft(&id(1), Some(WIDGET), "Repo triage"), None)
        .expect("put");
    assert!(matches!(put, RoutinePut::Saved(_)), "{put:?}");

    let index: i64 = db
        .admin()
        .query_one(
            "SELECT count(*) FROM pg_indexes WHERE tablename = 'routines' AND indexname = 'routines_repo' \
             AND schemaname = current_schema()",
            &[],
        )
        .expect("index")
        .get(0);
    assert_eq!(index, 1);
}

#[test]
fn every_routine_write_records_the_database_role() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    store.put_routine(&draft(&id(1), None, "Global"), None).expect("put");

    let row = db
        .admin()
        .query_one("SELECT written_by = current_user FROM routines", &[])
        .expect("row");
    assert!(row.get::<_, bool>(0));
}

#[test]
fn a_newer_schema_refuses_routines() {
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
        store.put_routine(&draft(&id(1), None, "Global"), None),
        Err(StoreError::NewerSchema)
    ));
    assert!(matches!(store.list_routines(None, 0, 50), Err(StoreError::NewerSchema)));
    assert!(matches!(store.get_routine(&id(1)), Err(StoreError::NewerSchema)));
    assert!(matches!(
        store.delete_routine(&id(1), None),
        Err(StoreError::NewerSchema)
    ));
    assert!(matches!(store.delete_routines_by("ada"), Err(StoreError::NewerSchema)));
}
