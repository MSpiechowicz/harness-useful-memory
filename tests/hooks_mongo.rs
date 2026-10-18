//! Team hooks in the MongoDB backend: the shared hook cases (`common::hooks`), the schema 2 to 3
//! migration and the newer-schema guard, one database per test. They skip unless
//! `HARNESS_MEMORY_TEST_MONGO_URL` names a server (see `mongo_support`).

mod common;
mod mongo_support;

use std::sync::Barrier;
use std::thread;

use common::hooks::{WIDGET, draft, id};
use common::scripts::script;
use harness_memory::store::mongo::schema::SCHEMA_VERSION;
use harness_memory::store::{HookPut, HookStore, ScriptStore, StoreError, Target};
use mongo_support::TestDb;
use mongodb::bson::{Document, doc};
use pretty_assertions::assert_eq;

/// Runs the shared case `case` on a fresh database.
fn conforms(case: common::hooks::Case) {
    let Some((_db, mut store)) = mongo_support::store() else {
        return;
    };
    case(&mut store);
}

#[test]
fn mongo_hooks_puts_insert_and_read_back() {
    conforms(common::hooks::puts_insert_and_read_back);
}

#[test]
fn mongo_hooks_an_insert_of_a_taken_id_changes_nothing() {
    conforms(common::hooks::an_insert_of_a_taken_id_changes_nothing);
}

#[test]
fn mongo_hooks_updates_need_the_current_sha() {
    conforms(common::hooks::updates_need_the_current_sha);
}

#[test]
fn mongo_hooks_lists_global_first_then_by_creation() {
    conforms(common::hooks::lists_global_first_then_by_creation);
}

#[test]
fn mongo_hooks_deletes_honour_the_expected_sha() {
    conforms(common::hooks::deletes_honour_the_expected_sha);
}

#[test]
fn mongo_hooks_a_scope_holds_at_most_64_hooks() {
    conforms(common::hooks::a_scope_holds_at_most_64_hooks);
}

#[test]
fn mongo_hooks_bad_drafts_store_nothing() {
    conforms(common::hooks::bad_drafts_store_nothing);
}

#[test]
fn mongo_a_version_2_database_migrates_to_hooks_and_keeps_its_scripts() {
    let Some(db) = TestDb::new() else { return };

    // A version 2 database: what this server writes, without the hooks collection.
    db.connect()
        .save_script(&Target::Global, &script("open-prs", "", "gh pr list\n"))
        .expect("script");
    db.raw()
        .collection::<Document>("hooks")
        .drop()
        .run()
        .expect("drop hooks");
    db.raw()
        .collection::<Document>("meta")
        .update_one(doc! { "_id": "schema" }, doc! { "$set": { "version": 2_i64 } })
        .run()
        .expect("downgrade");

    let mut store = db.connect();
    assert!(!store.is_newer());
    assert_eq!(db.schema_version(), SCHEMA_VERSION);
    assert_eq!(db.count("scripts", doc! {}), 1);

    let indexes = db
        .raw()
        .collection::<Document>("hooks")
        .list_index_names()
        .run()
        .expect("indexes");
    assert!(indexes.contains(&"hooks_repo".to_owned()), "{indexes:?}");

    let put = store
        .put_hook(&draft(&id(1), Some(WIDGET), "Repo rules"), None)
        .expect("put");
    assert!(matches!(put, HookPut::Saved(_)), "{put:?}");
    assert_eq!(db.count("hooks", doc! { "repo": WIDGET }), 1);
}

#[test]
fn mongo_a_newer_schema_refuses_hooks() {
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
        store.put_hook(&draft(&id(1), None, "Global"), None),
        Err(StoreError::NewerSchema)
    ));
    assert!(matches!(store.list_hooks(None, 0, 50), Err(StoreError::NewerSchema)));
    assert!(matches!(store.get_hook(&id(1)), Err(StoreError::NewerSchema)));
    assert!(matches!(store.delete_hook(&id(1), None), Err(StoreError::NewerSchema)));
}

/// Puts that race into a scope with room for 4 more add exactly 4: the scope guard makes the
/// snapshot transactions conflict instead of each counting the scope before the others' inserts.
#[test]
fn mongo_racing_puts_never_pass_the_scope_limit() {
    let Some((db, mut store)) = mongo_support::store() else {
        return;
    };

    if !store.uses_transactions() {
        eprintln!("skipped: racing puts are only kept to the limit on a replica set");
        return;
    }

    for n in 1..=60 {
        store
            .put_hook(&draft(&id(n), None, &format!("Global {n}")), None)
            .expect("put");
    }

    let racers = 8;
    let barrier = Barrier::new(racers);
    let outcomes: Vec<HookPut> = thread::scope(|scope| {
        let handles: Vec<_> = (0..racers)
            .map(|racer| {
                let (db, barrier) = (&db, &barrier);

                scope.spawn(move || {
                    let mut store = db.connect();
                    let n = 1000 + u32::try_from(racer).expect("small");
                    barrier.wait();
                    store.put_hook(&draft(&id(n), None, "Racer"), None).expect("put")
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("racer"))
            .collect()
    });

    let saved = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, HookPut::Saved(_)))
        .count();
    let full = outcomes.iter().filter(|outcome| **outcome == HookPut::Full).count();
    assert_eq!((saved, full), (4, 4), "{outcomes:?}");
    assert_eq!(db.count("hooks", doc! {}), 64);
}
