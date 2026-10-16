//! Notes in the PostgreSQL store (see `postgres_support`): the SQLite suite `store_notes`, run on
//! a throwaway schema. Create-or-update by title, patches, reads, deletion with cascade, secret
//! masking and per-call limits.

mod common;
mod postgres_support;

use common::{both, draft, global_only, repo, repo_only};
use harness_memory::store::{Forget, Link, MemoryStore, NoteKind, NotePatch, Scope, StoreError, Target};
use postgres_support::database;
use pretty_assertions::assert_eq;

const REPO: &str = "github.com/acme/widget";
const OTHER: &str = "github.com/acme/gadget";

#[test]
fn the_same_title_in_a_scope_updates_the_note_and_counts_revisions() {
    let Some(db) = database() else { return };
    let mut store = db.open();

    let first = store
        .remember(
            &repo(REPO),
            &draft("Build cache", "Lives in target/", NoteKind::Location),
        )
        .expect("create");
    assert!(first.created);
    assert_eq!(first.revision, 1);
    assert_eq!(first.scope, Scope::Repo);
    assert_eq!(first.project.as_deref(), Some("git:github.com/acme/widget"));

    let second = store
        .remember(
            &repo(REPO),
            &draft("  build   CACHE ", "Moved to .cache/", NoteKind::Decision),
        )
        .expect("update");
    assert!(!second.created);
    assert_eq!(second.id, first.id);
    assert_eq!(second.revision, 2);

    let notes = store.get(&both(REPO), &[first.id]).expect("get");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].body, "Moved to .cache/");
    assert_eq!(notes[0].kind, NoteKind::Decision);
    assert_eq!(notes[0].title, "build CACHE", "whitespace in a title is collapsed");
}

#[test]
fn titles_are_unique_per_scope_not_across_scopes() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let note = draft("Release steps", "Run the release workflow", NoteKind::Note);

    let in_repo = store.remember(&repo(REPO), &note).expect("repo");
    let in_other = store.remember(&repo(OTHER), &note).expect("other repo");
    let in_global = store.remember(&Target::Global, &note).expect("global");

    assert!(in_repo.created && in_other.created && in_global.created);
    assert_eq!(in_global.scope, Scope::Global);
    assert_eq!(in_global.project, None);

    // Another repository's note is invisible, the global one is visible from everywhere.
    assert!(store.get(&both(REPO), &[in_other.id]).expect("get").is_empty());
    assert_eq!(store.get(&both(REPO), &[in_global.id]).expect("get").len(), 1);
    assert!(store.get(&repo_only(REPO), &[in_global.id]).expect("get").is_empty());
}

#[test]
fn update_patches_fields_and_refuses_conflicts() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let first = store
        .remember(&repo(REPO), &draft("Flaky test", "Times out on CI", NoteKind::Problem))
        .expect("first");
    let other = store
        .remember(
            &repo(REPO),
            &draft("Slow build", "Linking takes long", NoteKind::Problem),
        )
        .expect("other");

    let patch = NotePatch {
        append: Some("Fixed by raising the timeout".to_owned()),
        kind: Some(NoteKind::Fix),
        tags: Some(vec!["CI".to_owned(), "tests".to_owned()]),
        ..NotePatch::default()
    };
    let saved = store.update(&both(REPO), first.id, &patch).expect("update");
    assert_eq!(saved.revision, 2);

    let note = &store.get(&both(REPO), &[first.id]).expect("get")[0];
    assert_eq!(note.body, "Times out on CI\nFixed by raising the timeout");
    assert_eq!(note.kind, NoteKind::Fix);
    assert_eq!(note.tags, vec!["ci", "tests"]);

    let rename = NotePatch {
        title: Some("slow BUILD".to_owned()),
        ..NotePatch::default()
    };
    let conflict = store.update(&both(REPO), first.id, &rename).expect_err("title taken");
    assert!(
        conflict.to_string().contains(&format!("note {}", other.id)),
        "{conflict}"
    );

    let nothing = store.update(&both(REPO), first.id, &NotePatch::default());
    assert!(matches!(nothing, Err(StoreError::Invalid(_))));

    let elsewhere = store.update(&both(OTHER), first.id, &patch);
    assert!(matches!(elsewhere, Err(StoreError::NotFound(_))));

    let too_long = NotePatch {
        append: Some("x".repeat(3990)),
        ..NotePatch::default()
    };
    assert!(matches!(
        store.update(&both(REPO), first.id, &too_long),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn get_counts_hits_skips_unknown_ids_and_caps_the_id_list() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let saved = store
        .remember(
            &Target::Global,
            &draft("Editor config", "Tabs are 4 wide", NoteKind::Decision),
        )
        .expect("create");

    store.get(&global_only(), &[saved.id]).expect("first read");
    let notes = store
        .get(&global_only(), &[saved.id, 999, saved.id])
        .expect("second read");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].hits, 1, "the second read sees the first one's hit");
    assert_eq!(notes[0].provenance.source.as_deref(), Some("tests"));

    let six = [1, 2, 3, 4, 5, 6];
    assert!(matches!(store.get(&global_only(), &six), Err(StoreError::Invalid(_))));
    assert!(matches!(store.get(&global_only(), &[]), Err(StoreError::Invalid(_))));
}

#[test]
fn forgetting_cascades_to_links_and_unused_entities() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let mut note = draft(
        "Parser lives in diff_parse",
        "business/src/git/diff_parse.rs",
        NoteKind::Location,
    );
    note.entities = vec!["DiffParser".to_owned(), "Lonely".to_owned()];
    let saved = store.remember(&repo(REPO), &note).expect("create");

    let link = Link {
        from: "DiffParser".to_owned(),
        relation: "located_in".to_owned(),
        to: "diff_parse.rs".to_owned(),
        note_id: Some(saved.id),
    };
    store.link(&repo(REPO), &link).expect("link");

    let deleted = store
        .forget(&both(REPO), &repo(REPO), &Forget::Note(saved.id))
        .expect("forget note");
    assert_eq!(deleted, 1);
    assert!(store.get(&both(REPO), &[saved.id]).expect("get").is_empty());

    // The relation survives without its note; the entity used only by the note is gone.
    let around = store.neighbors(&both(REPO), "DiffParser", 1, 20).expect("neighbors");
    assert_eq!(around.edges.len(), 1);
    assert_eq!(around.edges[0].note_id, None);
    assert!(around.notes.is_empty());
    assert!(matches!(
        store.neighbors(&both(REPO), "Lonely", 1, 20),
        Err(StoreError::NotFound(_))
    ));

    let removed = store
        .forget(&both(REPO), &repo(REPO), &Forget::Entity("diffparser".to_owned()))
        .expect("forget entity");
    assert_eq!(removed, 2, "the entity and its relation");
    assert!(
        matches!(
            store.neighbors(&both(REPO), "diff_parse.rs", 1, 20),
            Err(StoreError::NotFound(_))
        ),
        "the other end had nothing else and went with it"
    );

    let again = store.forget(&both(REPO), &repo(REPO), &Forget::Note(saved.id));
    assert!(matches!(again, Err(StoreError::NotFound(_))));
}

#[test]
fn forgetting_a_relation_keeps_the_rest_of_the_graph() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    for (from, to) in [("A", "B"), ("A", "C")] {
        let link = Link {
            from: from.to_owned(),
            relation: "depends on".to_owned(),
            to: to.to_owned(),
            note_id: None,
        };
        store.link(&repo(REPO), &link).expect("link");
    }

    let forget = Forget::Relation {
        from: "a".to_owned(),
        relation: "DEPENDS_ON".to_owned(),
        to: "b".to_owned(),
    };
    assert_eq!(store.forget(&both(REPO), &repo(REPO), &forget).expect("forget"), 1);

    let around = store.neighbors(&both(REPO), "A", 1, 20).expect("neighbors");
    assert_eq!(around.edges.len(), 1);
    assert_eq!(around.edges[0].to, "C");

    let missing = store.forget(&both(REPO), &repo(REPO), &forget);
    assert!(matches!(missing, Err(StoreError::NotFound(_))));
}

#[test]
fn secrets_are_masked_before_they_are_stored() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let token = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789";
    let mut note = draft(
        &format!("Key {token}"),
        &format!("Use ANTHROPIC_API_KEY={token} and https://bob:hunter2@example.com/x"),
        NoteKind::Note,
    );
    note.entities = vec![format!("cred {token}")];
    let saved = store.remember(&repo(REPO), &note).expect("create");

    let stored = &store.get(&both(REPO), &[saved.id]).expect("get")[0];
    let everything = format!("{} {} {:?}", stored.title, stored.body, stored.entities);
    assert!(!everything.contains("abcdefghijklmnop"), "{everything}");
    assert!(!everything.contains("hunter2"), "{everything}");
    assert!(stored.body.contains("••••"), "{}", stored.body);
}

fn invalid<T>(result: Result<T, StoreError>) -> bool {
    matches!(result, Err(StoreError::Invalid(_)))
}

#[test]
fn per_call_limits_are_enforced() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let target = repo(REPO);

    assert!(invalid(
        store.remember(&target, &draft(&"t".repeat(121), "body", NoteKind::Note))
    ));
    assert!(invalid(store.remember(&target, &draft("   ", "body", NoteKind::Note))));
    assert!(invalid(store.remember(&target, &draft("title", "", NoteKind::Note))));
    assert!(invalid(
        store.remember(&target, &draft("title", &"b".repeat(4001), NoteKind::Note))
    ));
    assert!(!invalid(store.remember(
        &target,
        &draft(&"t".repeat(120), &"b".repeat(4000), NoteKind::Note)
    )));

    let mut tags = draft("tags", "body", NoteKind::Note);
    tags.tags = (0..9).map(|index| format!("tag{index}")).collect();
    assert!(invalid(store.remember(&target, &tags)));
    tags.tags = vec!["Not A Tag!".to_owned()];
    assert!(invalid(store.remember(&target, &tags)));
    tags.tags = vec!["x".repeat(33)];
    assert!(invalid(store.remember(&target, &tags)));

    let mut entities = draft("entities", "body", NoteKind::Note);
    entities.entities = (0..11).map(|index| format!("entity {index}")).collect();
    assert!(invalid(store.remember(&target, &entities)));
    entities.entities = vec!["e".repeat(81)];
    assert!(invalid(store.remember(&target, &entities)));

    let link = Link {
        from: "A".to_owned(),
        relation: "r".repeat(41),
        to: "B".to_owned(),
        note_id: None,
    };
    assert!(invalid(store.link(&target, &link)));
}
