//! Search on both SQLite-dialect backends (see `libsql_support`): ranking across scopes,
//! snippets, hostile query text, recency, diacritics, limits and similar notes.

mod common;
mod libsql_support;

use common::{both, draft, global_only, query, repo, repo_only};
use harness_memory::store::{Hit, NoteKind, Scope, SearchQuery, StoreError, Target};
use libsql_support::Database;
use pretty_assertions::assert_eq;

const REPO: &str = "github.com/acme/widget";

fn ids(hits: &[Hit]) -> Vec<i64> {
    hits.iter().map(|hit| hit.id).collect()
}

store_cases!(
    repository_notes_rank_first,
    titles_outrank_bodies_and_prefixes_match,
    snippets_are_short_single_lines,
    hostile_query_text_is_plain_text,
    empty_query_lists_recent_notes,
    diacritics_and_case_are_ignored,
    limits_are_clamped_and_hits_carry_entities,
    similar_notes_share_words,
    checkout_filter_keeps_this_checkout,
);

fn repository_notes_rank_first(db: &Database) {
    let mut store = db.open();
    let global = store
        .remember(
            &Target::Global,
            &draft("Webkit rendering webkit", "webkit webkit webkit", NoteKind::Note),
        )
        .expect("global");
    let local = store
        .remember(&repo(REPO), &draft("Sidebar", "mentions webkit once", NoteKind::Note))
        .expect("repo");

    let hits = store.search(&query("webkit", both(REPO))).expect("search");
    assert_eq!(ids(&hits), vec![local.id, global.id]);
    assert_eq!(hits[0].scope, Scope::Repo);
    assert_eq!(hits[1].scope, Scope::Global);

    assert_eq!(
        ids(&store.search(&query("webkit", global_only())).expect("global")),
        vec![global.id]
    );
    assert_eq!(
        ids(&store.search(&query("webkit", repo_only(REPO))).expect("repo")),
        vec![local.id]
    );

    let unknown = store
        .search(&query("webkit", both("github.com/acme/new")))
        .expect("unknown repo");
    assert_eq!(ids(&unknown), vec![global.id]);
}

fn titles_outrank_bodies_and_prefixes_match(db: &Database) {
    let mut store = db.open();
    let body = store
        .remember(
            &repo(REPO),
            &draft("Startup", "the hardening flags matter", NoteKind::Note),
        )
        .expect("body");
    let title = store
        .remember(&repo(REPO), &draft("Hardening of git", "flags", NoteKind::Decision))
        .expect("title");

    let hits = store.search(&query("hard", both(REPO))).expect("prefix search");
    assert_eq!(ids(&hits), vec![title.id, body.id]);
}

fn snippets_are_short_single_lines(db: &Database) {
    let mut store = db.open();
    let long = format!(
        "{}\nneedle here\n{}",
        "lorem ipsum ".repeat(150),
        "dolor sit ".repeat(150)
    );
    store
        .remember(&repo(REPO), &draft("Long note", &long, NoteKind::Note))
        .expect("create");

    let hits = store.search(&query("needle", both(REPO))).expect("search");
    assert_eq!(hits.len(), 1);
    assert!(hits[0].snippet.contains("needle"), "{}", hits[0].snippet);
    assert!(hits[0].snippet.chars().count() <= 240);
    assert!(!hits[0].snippet.contains('\n'));

    let recent = store.search(&query("", both(REPO))).expect("recent");
    assert!(recent[0].snippet.chars().count() <= 240);
}

fn hostile_query_text_is_plain_text(db: &Database) {
    let mut store = db.open();
    store
        .remember(
            &repo(REPO),
            &draft("Near the column", "col title body near", NoteKind::Note),
        )
        .expect("create");

    for text in [
        "\"",
        "NEAR(",
        "*",
        "-",
        "col:",
        "title:near",
        "near AND OR NOT",
        "\"unterminated",
        ")(",
        "^title",
        "NEAR(col title, 2)",
        "'; DROP TABLE notes; --",
        "{title body}: x",
    ] {
        let result = store.search(&query(text, both(REPO)));
        assert!(result.is_ok(), "{text:?}: {result:?}");
    }

    let found = store.search(&query("title:near", both(REPO))).expect("column syntax");
    assert_eq!(found.len(), 1);

    let long = store.search(&query(&"x".repeat(257), both(REPO)));
    assert!(matches!(long, Err(StoreError::Invalid(_))));
}

fn empty_query_lists_recent_notes(db: &Database) {
    let mut store = db.open();
    let first = store
        .remember(&repo(REPO), &draft("First", "one", NoteKind::Note))
        .expect("first");
    let second = store
        .remember(&repo(REPO), &draft("Second", "two", NoteKind::Fix))
        .expect("second");
    let global = store
        .remember(&Target::Global, &draft("Third", "three", NoteKind::Note))
        .expect("global");

    // The same second for all three: the id breaks the tie, newest first.
    let hits = store.search(&query("  ", both(REPO))).expect("recent");
    assert_eq!(ids(&hits), vec![second.id, first.id, global.id]);

    let punctuation = store.search(&query("*-\"", both(REPO))).expect("only punctuation");
    assert_eq!(ids(&punctuation), vec![second.id, first.id, global.id]);

    let fixes = SearchQuery {
        kind: Some(NoteKind::Fix),
        ..query("", both(REPO))
    };
    assert_eq!(ids(&store.search(&fixes).expect("kind filter")), vec![second.id]);
}

fn diacritics_and_case_are_ignored(db: &Database) {
    let mut store = db.open();
    let saved = store
        .remember(&repo(REPO), &draft("Café menu", "Crème brûlée", NoteKind::Note))
        .expect("create");

    assert_eq!(
        ids(&store.search(&query("CAFE", both(REPO))).expect("search")),
        vec![saved.id]
    );
    assert_eq!(
        ids(&store.search(&query("creme", both(REPO))).expect("search")),
        vec![saved.id]
    );
}

fn limits_are_clamped_and_hits_carry_entities(db: &Database) {
    let mut store = db.open();
    for index in 0..25 {
        let mut note = draft(&format!("Alpha {index}"), "alpha body", NoteKind::Note);
        note.entities = vec!["Alpha".to_owned()];
        store.remember(&repo(REPO), &note).expect("create");
    }

    let many = SearchQuery {
        limit: 500,
        ..query("alpha", both(REPO))
    };
    let hits = store.search(&many).expect("search");
    assert_eq!(hits.len(), 20);
    assert_eq!(hits[0].entities, vec!["Alpha"]);
}

fn similar_notes_share_words(db: &Database) {
    let mut store = db.open();
    let old = store
        .remember(
            &repo(REPO),
            &draft(
                "Webview freezes on resize",
                "WebKitGTK compositor stalls while resizing",
                NoteKind::Problem,
            ),
        )
        .expect("old");
    store
        .remember(
            &repo(REPO),
            &draft("Release checklist", "Tag, build, publish", NoteKind::Note),
        )
        .expect("unrelated");
    let new = store
        .remember(
            &repo(REPO),
            &draft(
                "Resize freezes fixed",
                "Disabled the compositor during resize",
                NoteKind::Fix,
            ),
        )
        .expect("new");

    let similar = store.similar(&both(REPO), new.id, 3).expect("similar");
    assert_eq!(ids(&similar), vec![old.id]);
}

fn checkout_filter_keeps_this_checkout(db: &Database) {
    let mut store = db.open();
    let mut here = draft("Here", "shared word", NoteKind::Note);
    here.provenance.root = Some("/work/a".into());
    let mut there = draft("There", "shared word", NoteKind::Note);
    there.provenance.root = Some("/work/b".into());

    let here = store.remember(&repo(REPO), &here).expect("here");
    store.remember(&repo(REPO), &there).expect("there");
    store
        .remember(&Target::Global, &draft("Global", "shared word", NoteKind::Note))
        .expect("global");

    let recall = SearchQuery {
        checkout: Some("/work/a".into()),
        ..query("shared", both(REPO))
    };
    assert_eq!(ids(&store.search(&recall).expect("recall")), vec![here.id]);
}
