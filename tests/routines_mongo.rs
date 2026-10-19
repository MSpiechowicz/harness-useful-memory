//! Team routines in the MongoDB backend: the shared routine cases (`common::routines`), the
//! schema 3 to 4 migration of a database holding team hooks and the newer-schema guard, one
//! database per test. They skip unless `HARNESS_MEMORY_TEST_MONGO_URL` names a server (see
//! `mongo_support`).

mod common;
mod mongo_support;

use common::routines::{WIDGET, draft, id};
use harness_memory::store::mongo::schema::SCHEMA_VERSION;
use harness_memory::store::{HookPut, HookStore, RoutinePut, RoutineStore, StoreError};
use mongo_support::TestDb;
use mongodb::bson::{Document, doc};
use pretty_assertions::assert_eq;

/// Runs the shared case `case` on a fresh database.
fn conforms(case: common::routines::Case) {
    let Some((_db, mut store)) = mongo_support::store() else {
        return;
    };
    case(&mut store);
}

#[test]
fn mongo_routines_puts_insert_and_read_back() {
    conforms(common::routines::puts_insert_and_read_back);
}

#[test]
fn mongo_routines_an_insert_of_a_taken_id_changes_nothing() {
    conforms(common::routines::an_insert_of_a_taken_id_changes_nothing);
}

#[test]
fn mongo_routines_updates_need_the_current_sha() {
    conforms(common::routines::updates_need_the_current_sha);
}

#[test]
fn mongo_routines_lists_global_first_then_by_creation() {
    conforms(common::routines::lists_global_first_then_by_creation);
}

#[test]
fn mongo_routines_deletes_honour_the_expected_sha() {
    conforms(common::routines::deletes_honour_the_expected_sha);
}

#[test]
fn mongo_routines_a_scope_holds_at_most_64_routines() {
    conforms(common::routines::a_scope_holds_at_most_64_routines);
}

#[test]
fn mongo_routines_bad_drafts_store_nothing() {
    conforms(common::routines::bad_drafts_store_nothing);
}

#[test]
fn mongo_routines_deleting_mine_keeps_other_authors_rows() {
    conforms(common::routines::deleting_mine_keeps_other_authors_rows);
}

#[test]
fn mongo_routines_hooks_and_routines_are_kept_apart() {
    conforms(common::routines::hooks_and_routines_are_kept_apart);
}

#[test]
fn mongo_a_version_3_database_with_hooks_migrates_to_routines_and_keeps_them() {
    let Some(db) = TestDb::new() else { return };

    // A version 3 database: what this server writes, with a hook and without the routines.
    let hook = common::hooks::draft(&id(7), Some(WIDGET), "Kept hook");
    assert!(matches!(db.connect().put_hook(&hook, None), Ok(HookPut::Saved(_))));
    db.raw()
        .collection::<Document>("routines")
        .drop()
        .run()
        .expect("drop routines");
    db.raw()
        .collection::<Document>("meta")
        .update_one(doc! { "_id": "schema" }, doc! { "$set": { "version": 3_i64 } })
        .run()
        .expect("downgrade");

    let mut store = db.connect();
    assert!(!store.is_newer());
    assert_eq!(db.schema_version(), SCHEMA_VERSION);
    assert_eq!(SCHEMA_VERSION, 4);
    assert_eq!(db.count("hooks", doc! {}), 1);

    let kept = store.get_hook(&id(7)).expect("get").expect("the hook is kept");
    assert_eq!(kept.body, hook.body);

    let indexes = db
        .raw()
        .collection::<Document>("routines")
        .list_index_names()
        .run()
        .expect("indexes");
    assert!(indexes.contains(&"routines_repo".to_owned()), "{indexes:?}");

    let put = store
        .put_routine(&draft(&id(1), Some(WIDGET), "Repo triage"), None)
        .expect("put");
    assert!(matches!(put, RoutinePut::Saved(_)), "{put:?}");
    assert_eq!(db.count("routines", doc! { "repo": WIDGET }), 1);
}

#[test]
fn mongo_a_newer_schema_refuses_routines() {
    let Some(db) = TestDb::new() else { return };
    drop(db.connect());

    db.raw()
        .collection::<Document>("meta")
        .update_one(
            doc! { "_id": "schema" },
            doc! { "$set": { "version": SCHEMA_VERSION + 1 } },
        )
        .run()
        .expect("bump");

    let mut store = db.connect();
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

/// A document written by hand without a string `_id` or with a body over the limit is left out of
/// the list, and `delete --mine` still deletes it when the author matches.
#[test]
fn mongo_documents_changed_by_hand_are_left_out_and_still_deleted_as_mine() {
    let Some((db, mut store)) = mongo_support::store() else {
        return;
    };

    store.put_routine(&draft(&id(1), None, "Kept"), None).expect("put");
    db.raw()
        .collection::<Document>("routines")
        .insert_many([
            doc! { "_id": 42_i64, "repo": null, "body": "{}", "author": "ada", "created_at": 0_i64 },
            doc! { "_id": id(2), "repo": null, "body": "a".repeat(16 * 1024 + 1), "author": "grace" },
        ])
        .run()
        .expect("tamper");

    let page = store.list_routines(None, 0, 50).expect("list");
    let ids: Vec<String> = page.routines.into_iter().map(|routine| routine.id).collect();
    assert_eq!((ids, page.total), (vec![id(1)], 3));

    let deleted = store.delete_routines_by("ada").expect("delete mine");
    assert_eq!(deleted.len(), 2, "{deleted:?}");
    assert_eq!(db.count("routines", doc! {}), 1);
}
