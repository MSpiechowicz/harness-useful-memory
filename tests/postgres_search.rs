//! Search in the PostgreSQL store (see `postgres_support`): the SQLite suite `store_search`, run
//! on a throwaway schema. Ranking across scopes, snippets, hostile query text, recency.

mod common;
mod postgres_support;

use common::{both, draft, global_only, query, repo, repo_only};
use harness_memory::store::{MemoryStore, NoteKind, Scope, SearchQuery, StoreError, Target};
use postgres_support::database;
use pretty_assertions::assert_eq;

const REPO: &str = "github.com/acme/widget";

fn ids(hits: &[harness_memory::store::Hit]) -> Vec<i64> {
    hits.iter().map(|hit| hit.id).collect()
}

#[test]
fn one_ranking_for_repository_and_global_notes() {
    let Some(db) = database() else { return };
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
    // A clearly more relevant global note beats a weak repository one.
    assert_eq!(ids(&hits), vec![global.id, local.id]);
    assert_eq!(hits[0].scope, Scope::Global);
    assert_eq!(hits[1].scope, Scope::Repo);
    assert!(hits[0].score_milli > hits[1].score_milli);

    assert_eq!(
        ids(&store.search(&query("webkit", global_only())).expect("global")),
        vec![global.id]
    );
    assert_eq!(
        ids(&store.search(&query("webkit", repo_only(REPO))).expect("repo")),
        vec![local.id]
    );

    // A repository without notes yet sees the global scope only.
    let unknown = store
        .search(&query("webkit", both("github.com/acme/new")))
        .expect("unknown repo");
    assert_eq!(ids(&unknown), vec![global.id]);

    // Equally relevant notes: the repository one wins.
    let twin_global = store
        .remember(
            &Target::Global,
            &draft("Layout rules", "grid layout rules", NoteKind::Note),
        )
        .expect("twin global");
    let twin_repo = store
        .remember(&repo(REPO), &draft("Layout rules", "grid layout rules", NoteKind::Note))
        .expect("twin repo");
    let twins = store.search(&query("layout", both(REPO))).expect("twins");
    assert_eq!(ids(&twins), vec![twin_repo.id, twin_global.id]);
}

#[test]
fn title_matches_outrank_body_matches_and_prefixes_match() {
    let Some(db) = database() else { return };
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

#[test]
fn snippets_are_short_and_on_one_line() {
    let Some(db) = database() else { return };
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

#[test]
fn hostile_query_text_is_plain_text() {
    let Some(db) = database() else { return };
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
        "a:b:c",
        "NEAR(col title, 2)",
        "'; DROP TABLE notes; --",
        "{title body}: x",
    ] {
        let result = store.search(&query(text, both(REPO)));
        assert!(result.is_ok(), "{text:?}: {result:?}");
    }

    let found = store.search(&query("title:near", both(REPO))).expect("column syntax");
    assert_eq!(found.len(), 1, "words of column syntax still match as words");

    let long = store.search(&query(&"x".repeat(257), both(REPO)));
    assert!(matches!(long, Err(StoreError::Invalid(_))));
}

#[test]
fn an_empty_query_lists_the_most_recent_notes() {
    let Some(db) = database() else { return };
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

#[test]
fn diacritics_and_case_are_ignored() {
    let Some(db) = database() else { return };
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

#[test]
fn limits_are_clamped_and_hits_carry_entities() {
    let Some(db) = database() else { return };
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

#[test]
fn similar_notes_share_words_and_leave_out_the_note_itself() {
    let Some(db) = database() else { return };
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

// Shared keyword search cases (see `common::search_cases`).

fn with_store(case: fn(&mut dyn MemoryStore)) {
    let Some(db) = database() else { return };
    let mut store = db.open();
    case(&mut store);
}

#[test]
fn stems_find_other_forms_of_a_word() {
    with_store(common::search_cases::stems_find_other_forms_of_a_word);
}

#[test]
fn a_query_of_stopwords_finds_nothing_but_an_empty_one_lists_recent_notes() {
    with_store(common::search_cases::a_query_of_stopwords_finds_nothing_but_an_empty_one_lists_recent_notes);
}

#[test]
fn a_topic_the_notes_lack_finds_nothing() {
    with_store(common::search_cases::a_topic_the_notes_lack_finds_nothing);
}

#[test]
fn a_note_needs_half_of_the_query_words() {
    with_store(common::search_cases::a_note_needs_half_of_the_query_words);
}

#[test]
fn a_query_word_naming_an_entity_or_a_path_boosts_the_note() {
    with_store(common::search_cases::a_query_word_naming_an_entity_or_a_path_boosts_the_note);
}

#[test]
fn an_empty_store_finds_nothing() {
    with_store(common::search_cases::an_empty_store_finds_nothing);
}

#[test]
fn similar_notes_are_unchanged() {
    with_store(common::search_cases::similar_notes_are_unchanged);
}

#[test]
fn hits_for_loads_the_notes_a_search_may_see() {
    with_store(common::search_cases::hits_for_loads_the_notes_a_search_may_see);
}
