//! Scripts in the MongoDB backend: the schema 2 migration, the shared script store cases
//! (`common::scripts`) and what only MongoDB needs (the unique index on global names, regular
//! expression syntax in queries, documents changed by hand), one database per test. They skip
//! unless `HARNESS_MEMORY_TEST_MONGO_URL` names a server (see `mongo_support`).

mod common;
mod mongo_support;

use std::sync::{Arc, Barrier};
use std::thread;

use common::scripts::{OTHER, REPO, script};
use common::{both, draft, repo};
use harness_memory::limits::{SCRIPT_BODY_MAX, SCRIPT_DESCRIPTION_MAX, SCRIPT_TIMEOUT_MAX};
use harness_memory::store::mongo::schema::SCHEMA_VERSION;
use harness_memory::store::{Language, View, script_sha256};
use harness_memory::store::{MemoryStore, MongoStore, NoteKind, ScriptRef, ScriptStore, StoreError, Target};
use mongo_support::TestDb;
use mongodb::bson::{Document, doc};
use pretty_assertions::assert_eq;

fn save(store: &mut MongoStore, target: &Target, name: &str, description: &str) -> i64 {
    store
        .save_script(target, &script(name, description, &format!("echo {name}\n")))
        .expect("save")
        .script
        .summary
        .id
}

fn listed(store: &mut MongoStore, view: &View, query: Option<&str>) -> Vec<String> {
    store
        .list_scripts(view, query)
        .expect("list")
        .into_iter()
        .map(|summary| summary.name)
        .collect()
}

/// Runs the shared case `case` on a fresh database.
fn conforms(case: common::scripts::Case) {
    let Some((_db, mut store)) = mongo_support::store() else {
        return;
    };
    case(&mut store);
}

#[test]
fn mongo_scripts_saving_upserts_by_name() {
    conforms(common::scripts::saving_upserts_by_name);
}

#[test]
fn mongo_scripts_bad_drafts_store_nothing() {
    conforms(common::scripts::bad_drafts_store_nothing);
}

#[test]
fn mongo_scripts_list_repo_first_then_by_name() {
    conforms(common::scripts::lists_repo_first_then_by_name);
}

#[test]
fn mongo_scripts_queries_match_name_or_description_ignoring_case() {
    conforms(common::scripts::queries_match_name_or_description_ignoring_case);
}

#[test]
fn mongo_scripts_views_decide_what_is_visible() {
    conforms(common::scripts::views_decide_what_is_visible);
}

#[test]
fn mongo_scripts_names_resolve_repo_first_and_delete() {
    conforms(common::scripts::names_resolve_repo_first_and_delete);
}

#[test]
fn mongo_scripts_runs_are_counted() {
    conforms(common::scripts::runs_are_counted);
}

#[test]
fn mongo_scripts_a_run_of_a_missing_script_is_no_error() {
    conforms(common::scripts::a_run_of_a_missing_script_is_no_error);
}

#[test]
fn mongo_scripts_deleting_many_takes_only_visible_ids() {
    conforms(common::scripts::deleting_many_takes_only_visible_ids);
}

#[test]
fn mongo_a_version_1_database_migrates_to_scripts_and_keeps_its_notes() {
    let Some(db) = TestDb::new() else { return };

    // A version 1 database: what this server writes, without the scripts collection.
    let note = db
        .connect()
        .remember(&repo(REPO), &draft("Deploy", "run make deploy", NoteKind::Note))
        .expect("note")
        .id;
    db.raw()
        .collection::<Document>("scripts")
        .drop()
        .run()
        .expect("drop scripts");
    db.raw()
        .collection::<Document>("meta")
        .update_one(doc! { "_id": "schema" }, doc! { "$set": { "version": 1_i64 } })
        .run()
        .expect("downgrade");

    let mut store = db.connect();

    assert!(!store.is_newer());
    assert_eq!(db.schema_version(), SCHEMA_VERSION);
    assert_eq!(store.get(&both(REPO), &[note]).expect("get")[0].body, "run make deploy");

    let indexes = db
        .raw()
        .collection::<Document>("scripts")
        .list_index_names()
        .run()
        .expect("indexes");
    assert!(indexes.contains(&"scripts_name".to_owned()), "{indexes:?}");

    save(&mut store, &repo(REPO), "open-prs", "Lists open pull requests");
    assert_eq!(listed(&mut store, &both(REPO), None), ["open-prs"]);
}

#[test]
fn mongo_the_unique_index_keeps_global_names_unique() {
    let Some((db, mut store)) = mongo_support::store() else {
        return;
    };

    save(&mut store, &repo(REPO), "deploy", "repo");
    save(&mut store, &repo(OTHER), "deploy", "other");
    save(&mut store, &Target::Global, "deploy", "global");
    let again = store
        .save_script(&Target::Global, &script("deploy", "global again", "true"))
        .expect("replace global");

    assert!(!again.created);
    assert_eq!(db.count("scripts", doc! {}), 3);

    let scripts = db.raw().collection::<Document>("scripts");
    let duplicate = scripts
        .insert_one(doc! { "_id": 999_i64, "project": null, "name": "deploy" })
        .run();
    assert!(duplicate.is_err(), "a second global deploy was stored");
}

#[test]
fn mongo_concurrent_first_saves_of_one_name_create_one_script() {
    let Some(db) = TestDb::new() else { return };
    const WRITERS: usize = 4;

    let barrier = Arc::new(Barrier::new(WRITERS));
    let writers: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let url = db.url.clone();
            let barrier = Arc::clone(&barrier);

            thread::spawn(move || {
                let mut store = MongoStore::connect(&url).expect("connect");
                let draft = script("shared", "", &format!("echo {writer}\n"));
                barrier.wait();
                store.save_script(&Target::Global, &draft).expect("save")
            })
        })
        .collect();

    let saved: Vec<_> = writers
        .into_iter()
        .map(|writer| writer.join().expect("writer"))
        .collect();

    assert_eq!(saved.iter().filter(|saved| saved.created).count(), 1);
    let id = saved[0].script.summary.id;
    assert!(saved.iter().all(|one| one.script.summary.id == id));
    assert_eq!(db.count("scripts", doc! {}), 1);
}

#[test]
fn mongo_query_text_is_matched_literally() {
    let Some((_db, mut store)) = mongo_support::store() else {
        return;
    };

    save(&mut store, &repo(REPO), "ci-status", "Shows the CI (build) status");
    save(&mut store, &Target::Global, "weather", "Größe of the forecast.*");

    assert_eq!(listed(&mut store, &both(REPO), Some("(BUILD)")), ["ci-status"]);
    assert_eq!(listed(&mut store, &both(REPO), Some(".*")), ["weather"]);
    assert_eq!(listed(&mut store, &both(REPO), Some("größe")), ["weather"]);

    for hostile in ["^ci|weather", "[", "a\0b\\", "(?i)", "$where", "\\"] {
        assert_eq!(
            listed(&mut store, &both(REPO), Some(hostile)),
            Vec::<String>::new(),
            "{hostile:?}"
        );
    }
}

#[test]
fn mongo_scripts_changed_behind_the_server_read_back_safely() {
    let Some((db, mut store)) = mongo_support::store() else {
        return;
    };

    let id = save(&mut store, &repo(REPO), "tampered", "fine");
    let scripts = db.raw().collection::<Document>("scripts");

    // A body swapped by hand carries the hash of the body read, never the stored one.
    let changed = doc! {
        "body": "rm -rf ~\n",
        "timeout_secs": 99_999_i64,
        "run_count": "many",
    };
    scripts
        .update_one(doc! { "_id": id }, doc! { "$set": changed })
        .run()
        .expect("tamper");

    let read = store
        .get_script(&both(REPO), &ScriptRef::Id(id))
        .expect("get")
        .expect("found");
    assert_eq!(read.body, "rm -rf ~\n");
    assert_eq!(read.summary.sha256, script_sha256(Language::Bash, "rm -rf ~\n"));
    assert_eq!(read.summary.timeout_secs, SCRIPT_TIMEOUT_MAX);
    assert_eq!(read.summary.run_count, 0);

    scripts
        .update_one(doc! { "_id": id }, doc! { "$set": { "timeout_secs": -3_i64 } })
        .run()
        .expect("tamper");
    assert_eq!(store.list_scripts(&both(REPO), None).expect("list")[0].timeout_secs, 1);

    // A description or body over its limit is left out, the other scripts still list.
    save(&mut store, &Target::Global, "fine", "");
    scripts
        .update_one(
            doc! { "_id": id },
            doc! { "$set": { "description": "d".repeat(SCRIPT_DESCRIPTION_MAX + 1) } },
        )
        .run()
        .expect("tamper");
    assert_eq!(listed(&mut store, &both(REPO), None), ["fine"]);

    scripts
        .update_one(
            doc! { "_id": id },
            doc! { "$set": { "description": "fine", "body": "a".repeat(SCRIPT_BODY_MAX + 1) } },
        )
        .run()
        .expect("tamper");
    assert_eq!(listed(&mut store, &both(REPO), None), ["fine"]);
    assert_eq!(store.get_script(&both(REPO), &ScriptRef::Id(id)).expect("get"), None);

    // A language this server cannot run is left out.
    scripts
        .update_one(
            doc! { "_id": id },
            doc! { "$set": { "language": "ruby", "body": "true" } },
        )
        .run()
        .expect("tamper");
    assert_eq!(listed(&mut store, &both(REPO), None), ["fine"]);

    let by_name = ScriptRef::Name("tampered".to_owned());
    assert_eq!(store.get_script(&both(REPO), &by_name).expect("get"), None);
}

#[test]
fn mongo_a_deleted_script_records_no_run() {
    let Some((db, mut store)) = mongo_support::store() else {
        return;
    };

    let id = save(&mut store, &repo(REPO), "short-lived", "");
    assert!(store.delete_script(&both(REPO), &ScriptRef::Id(id)).expect("delete"));

    store
        .record_run(id, Some(0), 1_700_000_000)
        .expect("run of a deleted script");
    assert_eq!(db.count("scripts", doc! {}), 0);
}

#[test]
fn mongo_a_newer_database_refuses_script_calls() {
    let Some((db, mut store)) = mongo_support::store() else {
        return;
    };

    db.raw()
        .collection::<Document>("meta")
        .update_one(
            doc! { "_id": "schema" },
            doc! { "$set": { "version": SCHEMA_VERSION + 1 } },
        )
        .run()
        .expect("bump");

    let refused = store.save_script(&repo(REPO), &script("deploy", "", "true"));
    assert!(matches!(refused, Err(StoreError::NewerSchema)), "{refused:?}");
    assert!(matches!(
        store.list_scripts(&both(REPO), None),
        Err(StoreError::NewerSchema)
    ));
    assert!(matches!(store.record_run(1, Some(0), 1), Err(StoreError::NewerSchema)));

    let reconnected = db.connect();
    assert!(reconnected.is_newer());
    assert_eq!(db.schema_version(), SCHEMA_VERSION + 1);
}
