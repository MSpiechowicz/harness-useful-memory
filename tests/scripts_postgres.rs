//! Scripts in the PostgreSQL store: the schema 2 migration, the shared script store cases
//! (`common::scripts`) and what only PostgreSQL needs (`LIKE` characters in queries, the database
//! role of each write, the project cascade, the newer-schema guard), one schema per test. They
//! skip unless `HARNESS_MEMORY_TEST_POSTGRES_URL` names a server (see `postgres_support`).

mod common;
mod postgres_support;

use common::scripts::{REPO, script};
use common::{both, draft, global_only, repo, repo_key};
use harness_memory::limits::{SCRIPT_BODY_MAX, SCRIPT_DESCRIPTION_MAX, SCRIPT_NAME_MAX, SCRIPT_TIMEOUT_MAX};
use harness_memory::store::postgres::schema::SCHEMA_VERSION;
use harness_memory::store::{Language, View, script_sha256};
use harness_memory::store::{MemoryStore, NoteKind, PostgresStore, Scope, ScriptRef, ScriptStore, StoreError, Target};
use postgres_support::database;
use pretty_assertions::assert_eq;

fn save(store: &mut PostgresStore, target: &Target, name: &str, description: &str) -> i64 {
    store
        .save_script(target, &script(name, description, &format!("echo {name}\n")))
        .expect("save")
        .script
        .summary
        .id
}

fn listed(store: &mut PostgresStore, view: &View, query: Option<&str>) -> Vec<(Scope, String)> {
    store
        .list_scripts(view, query)
        .expect("list")
        .into_iter()
        .map(|summary| (summary.scope, summary.name))
        .collect()
}

/// Runs the shared case `case` on a fresh schema.
fn conforms(case: common::scripts::Case) {
    let Some(db) = database() else { return };
    case(&mut db.open());
}

#[test]
fn postgres_scripts_saving_upserts_by_name() {
    conforms(common::scripts::saving_upserts_by_name);
}

#[test]
fn postgres_scripts_bad_drafts_store_nothing() {
    conforms(common::scripts::bad_drafts_store_nothing);
}

#[test]
fn postgres_scripts_list_repo_first_then_by_name() {
    conforms(common::scripts::lists_repo_first_then_by_name);
}

#[test]
fn postgres_scripts_queries_match_name_or_description_ignoring_case() {
    conforms(common::scripts::queries_match_name_or_description_ignoring_case);
}

#[test]
fn postgres_scripts_views_decide_what_is_visible() {
    conforms(common::scripts::views_decide_what_is_visible);
}

#[test]
fn postgres_scripts_names_resolve_repo_first_and_delete() {
    conforms(common::scripts::names_resolve_repo_first_and_delete);
}

#[test]
fn postgres_scripts_runs_are_counted() {
    conforms(common::scripts::runs_are_counted);
}

#[test]
fn postgres_scripts_a_run_of_a_missing_script_is_no_error() {
    conforms(common::scripts::a_run_of_a_missing_script_is_no_error);
}

#[test]
fn postgres_scripts_deleting_many_takes_only_visible_ids() {
    conforms(common::scripts::deleting_many_takes_only_visible_ids);
}

#[test]
fn a_schema_1_database_migrates_to_schema_2_and_keeps_its_notes() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let note = store
        .remember(
            &repo(REPO),
            &draft("Build cache", "Lives in target/", NoteKind::Location),
        )
        .expect("note");
    drop(store);

    // Back to schema 1: the same tables without `scripts`.
    db.admin()
        .batch_execute("DROP TABLE scripts; UPDATE harness_memory_meta SET value = 1 WHERE key = 'schema_version'")
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
    assert_eq!((version, SCHEMA_VERSION), (2, 2));

    let notes = store.get(&both(REPO), &[note.id]).expect("get");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].body, "Lives in target/");

    let saved = store
        .save_script(
            &repo(REPO),
            &script("open-prs", "Lists open pull requests", "gh pr list\n"),
        )
        .expect("save after the migration");
    assert!(saved.created);
}

#[test]
fn like_characters_in_a_query_are_plain_characters() {
    let Some(db) = database() else { return };
    let mut store = db.open();

    save(&mut store, &Target::Global, "deploy", "Ships 100% of it");
    save(&mut store, &Target::Global, "release", "Tags it");

    assert_eq!(
        listed(&mut store, &global_only(), Some("100%")),
        [(Scope::Global, "deploy".to_owned())]
    );
    assert_eq!(listed(&mut store, &global_only(), Some("%s")), []);
    assert_eq!(listed(&mut store, &global_only(), Some("_")), []);
}

#[test]
fn a_body_reads_back_byte_for_byte() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let body = "#!/usr/bin/env bash\nprintf '%s\\n' \"żółw $1\" \t\n\n";

    let saved = store
        .save_script(&Target::Global, &script("turtle", "", body))
        .expect("save");
    let found = store
        .get_script(&global_only(), &ScriptRef::Name("turtle".to_owned()))
        .expect("get")
        .expect("found");

    assert_eq!(found, saved.script);
    assert_eq!(found.body, body);
    assert_eq!(found.summary.sha256, script_sha256(Language::Bash, body));
}

#[test]
fn every_script_write_records_the_database_role() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    save(&mut store, &repo(REPO), "open-prs", "");

    let row = db
        .admin()
        .query_one("SELECT written_by = current_user FROM scripts", &[])
        .expect("row");
    assert!(row.get::<_, bool>(0));
}

#[test]
fn removing_a_project_removes_its_scripts_only() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    save(&mut store, &repo(REPO), "mine", "");
    save(&mut store, &Target::Global, "shared", "");

    db.admin()
        .execute("DELETE FROM projects WHERE key = $1", &[&repo_key(REPO).key])
        .expect("drop the project");

    assert_eq!(
        listed(&mut store, &both(REPO), None),
        [(Scope::Global, "shared".to_owned())]
    );
}

#[test]
fn a_run_of_a_script_deleted_meanwhile_is_not_an_error() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let id = save(&mut store, &Target::Global, "gone", "");

    assert!(store.delete_script(&global_only(), &ScriptRef::Id(id)).expect("delete"));
    store.record_run(id, Some(0), 1).expect("nothing left to count");
    assert_eq!(listed(&mut store, &global_only(), None), []);
}

#[test]
fn a_newer_schema_refuses_scripts() {
    let Some(db) = database() else { return };
    drop(db.open());

    db.admin()
        .execute(
            "UPDATE harness_memory_meta SET value = $1 WHERE key = 'schema_version'",
            &[&(SCHEMA_VERSION + 1)],
        )
        .expect("bump");

    let mut store = db.open();
    let saved = store.save_script(&Target::Global, &script("open-prs", "", "gh pr list\n"));

    assert!(matches!(saved, Err(StoreError::NewerSchema)));
    assert!(matches!(
        store.list_scripts(&both(REPO), None),
        Err(StoreError::NewerSchema)
    ));
    assert!(matches!(
        store.get_script(&both(REPO), &ScriptRef::Id(1)),
        Err(StoreError::NewerSchema)
    ));
    assert!(matches!(
        store.delete_script(&both(REPO), &ScriptRef::Id(1)),
        Err(StoreError::NewerSchema)
    ));
    assert!(matches!(store.record_run(1, Some(0), 1), Err(StoreError::NewerSchema)));
}

#[test]
fn rows_changed_by_hand_are_clamped_or_left_out_without_breaking_the_list() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    for name in ["bulky", "fine", "huge", "negative", "wordy", "named"] {
        save(&mut store, &Target::Global, name, "");
    }

    // Exactly at the limit still lists; one over is left out.
    let at_limit = "d".repeat(SCRIPT_DESCRIPTION_MAX);
    let mut admin = db.admin();
    admin
        .execute("UPDATE scripts SET description = $1 WHERE name = 'fine'", &[&at_limit])
        .expect("at the limit");
    admin
        .batch_execute(&format!(
            "UPDATE scripts SET timeout_secs = -5 WHERE name = 'negative';
             UPDATE scripts SET timeout_secs = 9223372036854775807 WHERE name = 'huge';
             UPDATE scripts SET description = '{}' WHERE name = 'wordy';
             UPDATE scripts SET body = '{}' WHERE name = 'bulky';
             UPDATE scripts SET name = '{}' WHERE name = 'named';",
            "d".repeat(SCRIPT_DESCRIPTION_MAX + 1),
            "a".repeat(SCRIPT_BODY_MAX + 1),
            "n".repeat(SCRIPT_NAME_MAX + 1),
        ))
        .expect("tamper");

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

#[test]
fn names_are_trimmed_in_every_lookup() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let id = save(&mut store, &Target::Global, "padded", "");
    let padded = ScriptRef::Name("  padded \n".to_owned());

    let found = store.get_script(&both(REPO), &padded).expect("get").expect("found");
    assert_eq!(found.summary.id, id);
    assert!(store.delete_script(&both(REPO), &padded).expect("delete"));
}
