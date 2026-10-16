//! The entity graph: depth-bounded walks, cycles, limits, scopes and linked notes.

mod common;

use common::{both, draft, repo, temp_store};
use harness_memory::store::{Edge, Link, MemoryStore, NoteKind, SqliteStore, StoreError, Target};
use pretty_assertions::assert_eq;

const REPO: &str = "github.com/acme/widget";

fn link(store: &mut SqliteStore, from: &str, relation: &str, to: &str) -> Edge {
    let link = Link {
        from: from.to_owned(),
        relation: relation.to_owned(),
        to: to.to_owned(),
        note_id: None,
    };
    store.link(&repo(REPO), &link).expect("link")
}

fn triples(edges: &[Edge]) -> Vec<String> {
    let mut out: Vec<String> = edges
        .iter()
        .map(|edge| format!("{} {} {}", edge.from, edge.relation, edge.to))
        .collect();
    out.sort();
    out
}

#[test]
fn depth_bounds_the_walk() {
    let (_dir, mut store) = temp_store();
    link(&mut store, "A", "calls", "B");
    link(&mut store, "B", "calls", "C");
    link(&mut store, "C", "calls", "D");

    let one = store.neighbors(&both(REPO), "a", 1, 20).expect("depth 1");
    assert_eq!(triples(&one.edges), vec!["A calls B"]);

    let two = store.neighbors(&both(REPO), "A", 2, 20).expect("depth 2");
    assert_eq!(triples(&two.edges), vec!["A calls B", "B calls C"]);

    let clamped = store.neighbors(&both(REPO), "A", 9, 20).expect("depth clamped to 2");
    assert_eq!(clamped.edges.len(), 2);

    let middle = store.neighbors(&both(REPO), "C", 1, 20).expect("both directions");
    assert_eq!(triples(&middle.edges), vec!["B calls C", "C calls D"]);
}

#[test]
fn cycles_end_the_walk() {
    let (_dir, mut store) = temp_store();
    link(&mut store, "A", "calls", "B");
    link(&mut store, "B", "calls", "C");
    link(&mut store, "C", "calls", "A");
    link(&mut store, "A", "calls", "B");

    let around = store.neighbors(&both(REPO), "A", 2, 50).expect("cycle");
    assert_eq!(triples(&around.edges), vec!["A calls B", "B calls C", "C calls A"]);
    assert_eq!(around.more_edges, 0);
}

#[test]
fn the_limit_caps_relations_and_reports_the_rest() {
    let (_dir, mut store) = temp_store();
    for index in 0..60 {
        link(&mut store, "Hub", "has", &format!("leaf {index}"));
    }

    let capped = store.neighbors(&both(REPO), "hub", 1, 500).expect("clamped");
    assert_eq!(capped.edges.len(), 50);
    assert_eq!(capped.more_edges, 10);

    let small = store.neighbors(&both(REPO), "hub", 1, 5).expect("small");
    assert_eq!(small.edges.len(), 5);
    assert_eq!(small.more_edges, 55);
}

#[test]
fn links_are_upserted_normalized_and_scoped() {
    let (_dir, mut store) = temp_store();
    let saved = store
        .remember(
            &repo(REPO),
            &draft("GitCli location", "business/src/git/cli.rs", NoteKind::Location),
        )
        .expect("note");

    let first = link(&mut store, "GitCli", "Located In", "cli.rs");
    assert_eq!(first.relation, "located_in");

    let with_note = Link {
        from: "gitcli".to_owned(),
        relation: "located_in".to_owned(),
        to: "CLI.RS".to_owned(),
        note_id: Some(saved.id),
    };
    let again = store.link(&repo(REPO), &with_note).expect("upsert");
    assert_eq!(
        (again.from.as_str(), again.to.as_str()),
        ("GitCli", "cli.rs"),
        "first spelling kept"
    );
    assert_eq!(again.note_id, Some(saved.id));

    let around = store.neighbors(&both(REPO), "GitCli", 1, 20).expect("neighbors");
    assert_eq!(around.edges.len(), 1);
    assert_eq!(around.notes, vec![(saved.id, "GitCli location".to_owned())]);

    // The repository's entities are invisible from another repository.
    let elsewhere = store.neighbors(&both("github.com/acme/other"), "GitCli", 1, 20);
    assert!(matches!(elsewhere, Err(StoreError::NotFound(_))));

    // A global link cannot point at a repository note, and an entity cannot link to itself.
    let global_note = Link {
        note_id: Some(saved.id),
        ..with_note.clone()
    };
    assert!(matches!(
        store.link(&Target::Global, &global_note),
        Err(StoreError::NotFound(_))
    ));

    let itself = Link {
        to: "GITCLI".to_owned(),
        note_id: None,
        ..with_note
    };
    assert!(matches!(store.link(&repo(REPO), &itself), Err(StoreError::Invalid(_))));
}

#[test]
fn notes_linked_to_reached_entities_are_listed() {
    let (_dir, mut store) = temp_store();
    let mut note = draft("Retry fix", "Retries the flaky upload", NoteKind::Fix);
    note.entities = vec!["Uploader".to_owned()];
    let saved = store.remember(&repo(REPO), &note).expect("note");
    link(&mut store, "Sync", "uses", "Uploader");

    let around = store.neighbors(&both(REPO), "Sync", 1, 20).expect("neighbors");
    assert_eq!(around.notes, vec![(saved.id, "Retry fix".to_owned())]);
}
