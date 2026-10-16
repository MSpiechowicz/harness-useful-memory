//! Notes on both SQLite-dialect backends (local file and libSQL server, see `libsql_support`):
//! create-or-update by title, patches, reads, deletion with cascade, secret masking and limits.

mod common;
mod libsql_support;

use common::{both, draft, global_only, repo, repo_only};
use harness_memory::store::{Forget, Link, NoteKind, NotePatch, Scope, StoreError, Target};
use libsql_support::Database;
use pretty_assertions::assert_eq;

const REPO: &str = "github.com/acme/widget";
const OTHER: &str = "github.com/acme/gadget";

store_cases!(
    same_title_updates_and_counts_revisions,
    titles_are_unique_per_scope,
    update_patches_and_refuses_conflicts,
    get_counts_hits_and_caps_ids,
    forgetting_cascades,
    forgetting_a_relation_keeps_the_rest,
    secrets_are_masked,
    per_call_limits,
    find_by_title_is_exact_and_counts_no_hit,
);

fn same_title_updates_and_counts_revisions(db: &Database) {
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
    assert_eq!(notes[0].title, "build CACHE");
}

fn titles_are_unique_per_scope(db: &Database) {
    let mut store = db.open();
    let note = draft("Release steps", "Run the release workflow", NoteKind::Note);

    let in_repo = store.remember(&repo(REPO), &note).expect("repo");
    let in_other = store.remember(&repo(OTHER), &note).expect("other repo");
    let in_global = store.remember(&Target::Global, &note).expect("global");

    assert!(in_repo.created && in_other.created && in_global.created);
    assert_eq!(in_global.scope, Scope::Global);
    assert_eq!(in_global.project, None);

    assert!(store.get(&both(REPO), &[in_other.id]).expect("get").is_empty());
    assert_eq!(store.get(&both(REPO), &[in_global.id]).expect("get").len(), 1);
    assert!(store.get(&repo_only(REPO), &[in_global.id]).expect("get").is_empty());
}

fn update_patches_and_refuses_conflicts(db: &Database) {
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
    assert_eq!(store.get(&both(REPO), &[first.id]).expect("get")[0].revision, 2);
}

fn get_counts_hits_and_caps_ids(db: &Database) {
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

    assert!(matches!(
        store.get(&global_only(), &[1, 2, 3, 4, 5, 6]),
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(store.get(&global_only(), &[]), Err(StoreError::Invalid(_))));
}

fn forgetting_cascades(db: &Database) {
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
    assert!(matches!(
        store.neighbors(&both(REPO), "diff_parse.rs", 1, 20),
        Err(StoreError::NotFound(_))
    ));
    assert_eq!(db.integer("SELECT count(*) FROM relations"), 0, "cascaded");

    let again = store.forget(&both(REPO), &repo(REPO), &Forget::Note(saved.id));
    assert!(matches!(again, Err(StoreError::NotFound(_))));
}

fn forgetting_a_relation_keeps_the_rest(db: &Database) {
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

fn secrets_are_masked(db: &Database) {
    let mut store = db.open();
    let token = "[REDACTED]";
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

fn per_call_limits(db: &Database) {
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

    let mut entities = draft("entities", "body", NoteKind::Note);
    entities.entities = (0..11).map(|index| format!("entity {index}")).collect();
    assert!(invalid(store.remember(&target, &entities)));

    let link = Link {
        from: "A".to_owned(),
        relation: "r".repeat(41),
        to: "B".to_owned(),
        note_id: None,
    };
    assert!(invalid(store.link(&target, &link)));
}

fn find_by_title_is_exact_and_counts_no_hit(db: &Database) {
    let mut store = db.open();
    let mut note = draft("Build cache", "Lives in target/", NoteKind::Location);
    note.entities = vec!["Cargo".to_owned()];
    let saved = store.remember(&repo(REPO), &note).expect("create");
    store
        .remember(&Target::Global, &draft("Build cache notes", "global", NoteKind::Note))
        .expect("global");

    let found = store
        .find_by_title(&repo(REPO), "  build   CACHE ")
        .expect("find")
        .expect("found");
    assert_eq!(found.id, saved.id);
    assert_eq!(found.entities, vec!["Cargo"]);
    assert_eq!(found.hits, 0);

    assert_eq!(store.find_by_title(&Target::Global, "Build cache").expect("find"), None);
    assert_eq!(store.find_by_title(&repo(OTHER), "Build cache").expect("find"), None);
    assert_eq!(store.get(&both(REPO), &[saved.id]).expect("get")[0].hits, 0);
}
