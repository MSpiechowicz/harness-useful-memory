//! Keyword search cases every backend passes: stemming, stopwords, the half-of-the-words rule, the
//! context boost, an empty store and the unchanged similar-note lookup. Each takes a fresh store.

use harness_memory::store::{Hit, MemoryStore, NoteKind, Target};
use pretty_assertions::assert_eq;

use super::{both, draft, query, repo};

const REPO: &str = "github.com/acme/widget";

fn titles(hits: &[Hit]) -> Vec<&str> {
    hits.iter().map(|hit| hit.title.as_str()).collect()
}

fn search(store: &mut dyn MemoryStore, text: &str) -> Vec<Hit> {
    store.search(&query(text, both(REPO))).expect("search")
}

fn save(store: &mut dyn MemoryStore, title: &str, body: &str) -> i64 {
    store
        .remember(&repo(REPO), &draft(title, body, NoteKind::Note))
        .expect("save")
        .id
}

/// Notes about nothing the cases look for, so no query word is in most of the notes (a word in
/// more than half of them has next to no weight in SQLite).
fn fillers(store: &mut dyn MemoryStore) {
    for (title, body) in [
        ("Lunch menu", "soup and bread on monday"),
        ("Standup agenda", "short round of updates"),
        ("Travel budget", "flights and hotels"),
        ("Garden tools", "spade and rake in the shed"),
        ("Reading list", "novels for the winter"),
    ] {
        save(store, title, body);
    }
}

fn save_global(store: &mut dyn MemoryStore, title: &str, body: &str) -> i64 {
    store
        .remember(&Target::Global, &draft(title, body, NoteKind::Note))
        .expect("save global")
        .id
}

pub fn stems_find_other_forms_of_a_word(store: &mut dyn MemoryStore) {
    fillers(store);
    save(
        store,
        "Invalidating the caching layer",
        "Cached rows survive a schema upgrade.",
    );
    save(store, "Release checklist", "Tag, build, publish.");

    assert_eq!(
        titles(&search(store, "cache invalidation")),
        ["Invalidating the caching layer"]
    );
    assert_eq!(titles(&search(store, "layers")), ["Invalidating the caching layer"]);

    // The last word is still a prefix.
    assert_eq!(titles(&search(store, "inval")), ["Invalidating the caching layer"]);
}

pub fn a_query_of_stopwords_finds_nothing_but_an_empty_one_lists_recent_notes(store: &mut dyn MemoryStore) {
    save(store, "The way it is", "What this was about");
    save(store, "Release checklist", "Tag, build, publish.");

    assert!(search(store, "what is the").is_empty());
    assert!(search(store, "and the or to of").is_empty());
    assert_eq!(search(store, "").len(), 2);
    assert_eq!(search(store, "  ").len(), 2);
}

pub fn a_topic_the_notes_lack_finds_nothing(store: &mut dyn MemoryStore) {
    save(store, "Schema migration order", "Run the migration before the release.");
    save(store, "Release plan", "The plan is to tag on Friday.");
    save_global(store, "Helm release", "Tag and publish through the pipeline.");

    // Each note has one of "zebra", "migration" and "plan": less than half of the stems.
    assert!(search(store, "what is the zebra migration plan").is_empty());
    assert!(search(store, "kubernetes helm chart deployment").is_empty());
}

pub fn a_note_needs_half_of_the_query_words(store: &mut dyn MemoryStore) {
    fillers(store);
    let both_words = save(store, "Alpha beta", "two words");
    let alpha = save(store, "Alpha", "one word");
    save(store, "Gamma", "another word");

    let ids = |hits: Vec<Hit>| hits.iter().map(|hit| hit.id).collect::<Vec<_>>();

    // Two of four words: only the note that has both.
    assert_eq!(ids(search(store, "alpha beta gamma delta")), vec![both_words]);

    // One of two words is enough, and the note with both comes first.
    assert_eq!(ids(search(store, "alpha beta")), vec![both_words, alpha]);

    // Three words need two: "alpha gamma epsilon" has one note with alpha and one with gamma.
    assert!(search(store, "alpha gamma epsilon").is_empty());
}

pub fn a_query_word_naming_an_entity_or_a_path_boosts_the_note(store: &mut dyn MemoryStore) {
    fillers(store);

    // Three equally relevant notes (the path and its words are the same tokens), the boosted ones
    // saved first so that a tie would put them last.
    let mut named = draft("Alpha cache", "tuning notes in src cache rows rs", NoteKind::Note);
    named.entities = vec!["Cache".to_owned()];
    let named = store.remember(&repo(REPO), &named).expect("named").id;
    let pathy = save(store, "Beta cache", "tuning notes in src/cache/rows.rs");
    let plain = save(store, "Gamma cache", "tuning notes in src cache rows rs");

    let hits = search(store, "cache");
    let ids: Vec<i64> = hits.iter().map(|hit| hit.id).collect();

    assert_eq!(ids.len(), 3);
    assert_eq!(ids[2], plain);
    assert!(ids[..2].contains(&named) && ids[..2].contains(&pathy), "{ids:?}");
    assert!(hits[1].score_milli > hits[2].score_milli);
}

pub fn an_empty_store_finds_nothing(store: &mut dyn MemoryStore) {
    assert!(search(store, "anything at all").is_empty());
    assert!(search(store, "").is_empty());
    assert!(search(store, "what is the").is_empty());
}

/// The notes `similar` and `similar_text` give for a fixed set of notes, as the lookup answered
/// before the keyword search was reworked (captured from the release before it, on every backend):
/// unstemmed words, the repository first.
pub fn similar_notes_are_unchanged(store: &mut dyn MemoryStore) {
    save(
        store,
        "Webview freezes on resize",
        "WebKitGTK compositor stalls while resizing",
    );
    save_global(store, "Webview resize jitter", "compositor stalls resize webview");
    save(store, "Resizing compositor notes", "stalls compositor webview resizing");
    save(store, "Caching layers", "layer caching rules");
    save(store, "Release checklist", "Tag, build, publish");
    let source = save(
        store,
        "Webview resize freezes fixed",
        "compositor stalls webkitgtk webview resize",
    );

    let similar = store.similar(&both(REPO), source, 5).expect("similar");
    assert_eq!(
        titles(&similar),
        [
            "Webview freezes on resize",
            "Resizing compositor notes",
            "Webview resize jitter"
        ],
        "the repository first, the global note last"
    );

    let text = store
        .similar_text(&both(REPO), ("Layer caching", "caching layers rules"), 5)
        .expect("similar text");
    assert_eq!(titles(&text), ["Caching layers"]);
}
