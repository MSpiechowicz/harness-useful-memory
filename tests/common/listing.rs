//! `MemoryStore::list_notes` (with `MemoryStore::peek`, the other read that counts no hit, and
//! `MemoryStore::forget_notes`, which deletes what a list shows) cases every backend (and
//! `SplitStore`) must pass. Each case takes an
//! empty store and panics on a mismatch. The ids and the times of the latest write differ between
//! backends (and a split store shows local ids above the offset), so the cases check the order of
//! updated-time sorts against the rows' own times and ids, and exact orders for title sorts.

use harness_memory::store::{
    Author, AuthorFilter, Forgotten, LangFilter, Link, MemoryStore, NoteDraft, NoteKind, NoteListQuery, NotePage,
    NoteRow, NoteSort, NoteUsage, Scope, SearchQuery, StoreError, Target, UsefulFilter, View,
};
use harness_memory::support::lang::Lang;
use pretty_assertions::assert_eq;

use super::{both, draft, global_only, query, repo, repo_only};

pub const REPO: &str = "github.com/acme/widget";
pub const OTHER: &str = "github.com/acme/gadget";

/// A case: an empty store in, panics out.
pub type Case = fn(&mut dyn MemoryStore);

/// Every case, by name.
pub const CASES: [(&str, Case); 17] = [
    ("lists_visible_notes_newest_first", lists_visible_notes_newest_first),
    (
        "excluded_ids_leave_lists_and_searches",
        excluded_ids_leave_lists_and_searches,
    ),
    ("pages_cover_the_list_once", pages_cover_the_list_once),
    ("text_and_kind_filter_and_count", text_and_kind_filter_and_count),
    ("titles_sort_both_ways", titles_sort_both_ways),
    ("entities_filter_and_fill_rows", entities_filter_and_fill_rows),
    ("views_decide_what_is_listed", views_decide_what_is_listed),
    ("peek_reads_without_counting", peek_reads_without_counting),
    (
        "forget_notes_deletes_every_listed_note",
        forget_notes_deletes_every_listed_note,
    ),
    (
        "forget_notes_leaves_the_excluded_notes",
        forget_notes_leaves_the_excluded_notes,
    ),
    (
        "kinds_scopes_and_places_sort_and_page",
        kinds_scopes_and_places_sort_and_page,
    ),
    (
        "scope_text_and_kind_filter_together_and_count",
        scope_text_and_kind_filter_together_and_count,
    ),
    ("lang_shows_filters_and_pages", lang_shows_filters_and_pages),
    (
        "usefulness_is_counted_listed_sorted_and_filtered",
        usefulness_is_counted_listed_sorted_and_filtered,
    ),
    ("authors_are_added_kept_and_updated", super::authors::notes),
    (
        "added_by_filters_sorts_and_counts_who_added",
        added_by_filters_sorts_and_counts_who_added,
    ),
    (
        "added_by_pages_cover_the_list_and_the_facet_is_capped",
        added_by_pages_cover_the_list_and_the_facet_is_capped,
    ),
];

/// The first page (of up to 200) of `view`, newest first.
pub fn listing(view: View) -> NoteListQuery {
    NoteListQuery {
        view,
        text: String::new(),
        kind: None,
        scope: None,
        place: None,
        entity: None,
        lang: None,
        useful: None,
        author: AuthorFilter::Any,
        want_authors: true,
        sort: NoteSort::Updated,
        descending: true,
        offset: 0,
        limit: 200,
        exclude: Vec::new(),
    }
}

fn list(store: &mut dyn MemoryStore, query: &NoteListQuery) -> NotePage {
    store.list_notes(query).expect("list")
}

fn titles(rows: &[NoteRow]) -> Vec<&str> {
    rows.iter().map(|row| row.title.as_str()).collect()
}

fn count(page: &NotePage, kind: NoteKind) -> usize {
    page.kinds
        .iter()
        .find(|(listed, _)| *listed == kind)
        .map(|(_, count)| *count)
        .expect("every kind is counted")
}

fn scope_count(page: &NotePage, scope: Scope) -> usize {
    page.scopes
        .iter()
        .find(|(listed, _)| *listed == scope)
        .map(|(_, count)| *count)
        .expect("both scopes are counted")
}

fn save(store: &mut dyn MemoryStore, target: &Target, note: &NoteDraft) -> i64 {
    store.remember(target, note).expect("remember").id
}

fn tagged(title: &str, body: &str, kind: NoteKind, tags: &[&str], entities: &[&str]) -> NoteDraft {
    NoteDraft {
        tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
        entities: entities.iter().map(|entity| (*entity).to_owned()).collect(),
        ..draft(title, body, kind)
    }
}

/// Asserts `rows` are ordered by the latest write, then by id, newest first.
fn assert_newest_first(rows: &[NoteRow]) {
    for pair in rows.windows(2) {
        let earlier = (pair[0].updated_at, pair[0].id);
        let later = (pair[1].updated_at, pair[1].id);
        assert!(earlier > later, "{earlier:?} before {later:?}");
    }
}

/// The ids a list or a search leaves out (the hidden team notes) are gone from its rows and its
/// counts; more than 500 is an error.
pub fn excluded_ids_leave_lists_and_searches(store: &mut dyn MemoryStore) {
    let flaky = save(
        store,
        &repo(REPO),
        &draft("Flaky deploy", "deploy times out", NoteKind::Problem),
    );
    let fix = save(
        store,
        &repo(REPO),
        &draft("Deploy fix", "deploy retries", NoteKind::Fix),
    );
    let tabs = save(
        store,
        &Target::Global,
        &draft("Deploy tabs", "deploy four wide", NoteKind::Decision),
    );

    let left_out = NoteListQuery {
        exclude: vec![flaky, tabs],
        ..listing(both(REPO))
    };
    let page = list(store, &left_out);
    assert_eq!(titles(&page.rows), ["Deploy fix"]);
    assert_eq!(page.total, 1);
    assert_eq!(
        (scope_count(&page, Scope::Repo), scope_count(&page, Scope::Global)),
        (1, 0)
    );
    assert_eq!(
        (
            count(&page, NoteKind::Problem),
            count(&page, NoteKind::Fix),
            count(&page, NoteKind::Decision)
        ),
        (0, 1, 0)
    );

    let global = NoteListQuery {
        scope: Some(Scope::Global),
        ..left_out.clone()
    };
    assert_eq!(list(store, &global).total, 0);
    assert_eq!(
        list(store, &listing(both(REPO))).total,
        3,
        "nothing left out by default"
    );

    for text in ["deploy", ""] {
        let searched = SearchQuery {
            exclude: vec![flaky, tabs],
            ..query(text, both(REPO))
        };
        let hits: Vec<i64> = store
            .search(&searched)
            .expect("search")
            .iter()
            .map(|hit| hit.id)
            .collect();
        assert_eq!(hits, [fix], "{text:?}");
    }

    let too_many: Vec<i64> = (1..=501).collect();
    let listed = NoteListQuery {
        exclude: too_many.clone(),
        ..listing(both(REPO))
    };
    assert!(matches!(store.list_notes(&listed), Err(StoreError::Invalid(_))));

    let searched = SearchQuery {
        exclude: too_many,
        ..query("deploy", both(REPO))
    };
    assert!(matches!(store.search(&searched), Err(StoreError::Invalid(_))));
}

pub fn lists_visible_notes_newest_first(store: &mut dyn MemoryStore) {
    save(store, &repo(REPO), &draft("Flaky test", "Times out", NoteKind::Problem));
    save(
        store,
        &repo(REPO),
        &draft("Raise the timeout", "To 30 s", NoteKind::Fix),
    );
    save(store, &repo(OTHER), &draft("Gadget only", "Not here", NoteKind::Note));
    save(
        store,
        &Target::Global,
        &draft("Editor tabs", "Four wide", NoteKind::Decision),
    );

    let page = list(store, &listing(both(REPO)));

    assert_eq!(page.total, 3);
    assert_newest_first(&page.rows);

    let mut listed = titles(&page.rows);
    listed.sort_unstable();
    assert_eq!(listed, ["Editor tabs", "Flaky test", "Raise the timeout"]);

    let kinds: Vec<NoteKind> = page.kinds.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(kinds, NoteKind::ALL);
    assert_eq!(
        (
            count(&page, NoteKind::Problem),
            count(&page, NoteKind::Fix),
            count(&page, NoteKind::Decision)
        ),
        (1, 1, 1)
    );
    assert_eq!(
        count(&page, NoteKind::Note),
        0,
        "another repository's note is not counted"
    );

    let oldest_first = list(
        store,
        &NoteListQuery {
            descending: false,
            ..listing(both(REPO))
        },
    );
    let mut reversed = oldest_first.rows.clone();
    reversed.reverse();
    assert_eq!(reversed, page.rows);

    let row = page.rows.iter().find(|row| row.title == "Flaky test").expect("listed");
    assert_eq!(row.project.as_deref(), Some("git:github.com/acme/widget"));
    assert_eq!(row.provenance.source.as_deref(), Some("tests"));
    assert_eq!((row.revision, row.hits), (1, 0));

    let notes = store.get(&both(REPO), &[row.id]).expect("get");
    assert_eq!(notes[0].hits, 0, "listing counts no hit");
}

pub fn pages_cover_the_list_once(store: &mut dyn MemoryStore) {
    for index in 0..7 {
        let target = if index % 2 == 0 { repo(REPO) } else { Target::Global };
        save(store, &target, &draft(&format!("Note {index}"), "Body", NoteKind::Note));
    }

    let all = list(store, &listing(both(REPO)));
    assert_eq!(all.total, 7);

    let mut paged: Vec<NoteRow> = Vec::new();

    for (offset, size) in [(0, 3), (3, 3), (6, 1), (9, 0)] {
        let page = list(
            store,
            &NoteListQuery {
                offset,
                limit: 3,
                ..listing(both(REPO))
            },
        );

        assert_eq!(page.total, 7, "the total is the same on every page");
        assert_eq!(page.rows.len(), size, "offset {offset}");
        paged.extend(page.rows);
    }

    assert_eq!(paged, all.rows);

    let clamped = list(
        store,
        &NoteListQuery {
            limit: 0,
            ..listing(both(REPO))
        },
    );
    assert_eq!(clamped.rows.len(), 1, "a limit below 1 lists one row");
}

pub fn text_and_kind_filter_and_count(store: &mut dyn MemoryStore) {
    let note = |title: &str, body: &str, kind: NoteKind, tags: &[&str]| tagged(title, body, kind, tags, &[]);

    save(
        store,
        &repo(REPO),
        &note("Build CACHE", "Lives in target/", NoteKind::Location, &[]),
    );
    save(
        store,
        &repo(REPO),
        &note("Linker", "The cache of objects", NoteKind::Fix, &[]),
    );
    save(
        store,
        &Target::Global,
        &note("Tabs", "Four wide", NoteKind::Decision, &["cache-policy"]),
    );
    save(
        store,
        &Target::Global,
        &note("Coverage", "Stays at 50% or more", NoteKind::Decision, &[]),
    );
    save(
        store,
        &Target::Global,
        &note("Snake", "Names use a_b", NoteKind::Note, &[]),
    );

    let cached = list(
        store,
        &NoteListQuery {
            text: " cache ".to_owned(),
            ..listing(both(REPO))
        },
    );
    let mut found = titles(&cached.rows);
    found.sort_unstable();
    assert_eq!(
        found,
        ["Build CACHE", "Linker", "Tabs"],
        "title, body and tags, ignoring case"
    );
    assert_eq!(cached.total, 3);
    assert_eq!(
        (
            count(&cached, NoteKind::Location),
            count(&cached, NoteKind::Fix),
            count(&cached, NoteKind::Decision)
        ),
        (1, 1, 1)
    );

    let fixes = list(
        store,
        &NoteListQuery {
            text: "cache".to_owned(),
            kind: Some(NoteKind::Fix),
            ..listing(both(REPO))
        },
    );
    assert_eq!(titles(&fixes.rows), ["Linker"]);
    assert_eq!(fixes.total, 1, "the total follows the kind");
    assert_eq!(count(&fixes, NoteKind::Decision), 1, "the counts ignore the kind");

    for (text, expected) in [("50%", "Coverage"), ("a_b", "Snake")] {
        let literal = list(
            store,
            &NoteListQuery {
                text: text.to_owned(),
                ..listing(both(REPO))
            },
        );
        assert_eq!(titles(&literal.rows), [expected], "{text} is matched literally");
    }

    let nothing = list(
        store,
        &NoteListQuery {
            text: "nowhere".to_owned(),
            ..listing(both(REPO))
        },
    );
    assert_eq!((nothing.total, nothing.rows.len()), (0, 0));

    let long = NoteListQuery {
        text: "x".repeat(257),
        ..listing(both(REPO))
    };
    assert!(matches!(store.list_notes(&long), Err(StoreError::Invalid(_))));
}

pub fn titles_sort_both_ways(store: &mut dyn MemoryStore) {
    save(store, &repo(REPO), &draft("beta", "Body", NoteKind::Note));
    save(store, &Target::Global, &draft("Alpha", "Body", NoteKind::Note));
    save(store, &repo(REPO), &draft("gamma", "Body", NoteKind::Note));
    save(store, &Target::Global, &draft("Delta", "Body", NoteKind::Note));

    let by_title = NoteListQuery {
        sort: NoteSort::Title,
        descending: false,
        ..listing(both(REPO))
    };

    assert_eq!(
        titles(&list(store, &by_title).rows),
        ["Alpha", "beta", "Delta", "gamma"]
    );
    assert_eq!(
        titles(
            &list(
                store,
                &NoteListQuery {
                    descending: true,
                    ..by_title.clone()
                }
            )
            .rows
        ),
        ["gamma", "Delta", "beta", "Alpha"]
    );

    let second = NoteListQuery {
        offset: 1,
        limit: 2,
        ..by_title
    };
    assert_eq!(titles(&list(store, &second).rows), ["beta", "Delta"]);
}

pub fn entities_filter_and_fill_rows(store: &mut dyn MemoryStore) {
    save(
        store,
        &repo(REPO),
        &tagged("Auth flow", "Body", NoteKind::Note, &["auth"], &["Zeta", "auth.rs"]),
    );
    save(
        store,
        &repo(REPO),
        &tagged("Unrelated", "Body", NoteKind::Note, &[], &["db.rs"]),
    );
    save(
        store,
        &Target::Global,
        &tagged("Global auth", "Body", NoteKind::Note, &[], &["AUTH.rs"]),
    );

    let all = list(store, &listing(both(REPO)));
    let auth = all.rows.iter().find(|row| row.title == "Auth flow").expect("listed");
    assert_eq!(auth.entities, ["auth.rs", "Zeta"], "alphabetically, ignoring case");
    assert_eq!(auth.tags, ["auth"]);

    let linked = list(
        store,
        &NoteListQuery {
            entity: Some("Auth.RS".to_owned()),
            sort: NoteSort::Title,
            descending: false,
            ..listing(both(REPO))
        },
    );
    assert_eq!(titles(&linked.rows), ["Auth flow", "Global auth"]);
    assert_eq!(linked.total, 2);

    let repo_side = list(
        store,
        &NoteListQuery {
            entity: Some("auth.rs".to_owned()),
            ..listing(repo_only(REPO))
        },
    );
    assert_eq!(titles(&repo_side.rows), ["Auth flow"]);
}

pub fn views_decide_what_is_listed(store: &mut dyn MemoryStore) {
    save(store, &repo(REPO), &draft("Mine", "Body", NoteKind::Note));
    save(store, &repo(OTHER), &draft("Theirs", "Body", NoteKind::Note));
    save(store, &Target::Global, &draft("Shared", "Body", NoteKind::Note));

    let listed = |store: &mut dyn MemoryStore, view: View| {
        let mut found: Vec<String> = list(store, &listing(view))
            .rows
            .into_iter()
            .map(|row| row.title)
            .collect();
        found.sort_unstable();
        found
    };

    assert_eq!(listed(store, repo_only(REPO)), ["Mine"]);
    assert_eq!(listed(store, global_only()), ["Shared"]);
    assert_eq!(listed(store, both(OTHER)), ["Shared", "Theirs"]);
    assert_eq!(listed(store, both("github.com/acme/never-written")), ["Shared"]);

    let none = View {
        repo: None,
        global: false,
    };
    let page = list(store, &listing(none));
    assert_eq!((page.total, page.rows.len()), (0, 0));
}

pub fn peek_reads_without_counting(store: &mut dyn MemoryStore) {
    let mine = save(
        store,
        &repo(REPO),
        &tagged("Mine", "Body", NoteKind::Note, &["ci"], &["auth.rs"]),
    );
    let theirs = save(store, &repo(OTHER), &draft("Theirs", "Body", NoteKind::Note));
    let shared = save(store, &Target::Global, &draft("Shared", "Body", NoteKind::Note));

    let hits = |store: &mut dyn MemoryStore| -> Vec<(String, i64)> {
        store
            .peek(&both(REPO), &[shared, mine, theirs, mine])
            .expect("peek")
            .into_iter()
            .map(|note| (note.title, note.hits))
            .collect()
    };

    let peeked = store.peek(&both(REPO), &[mine]).expect("peek");
    assert_eq!(peeked[0].entities, ["auth.rs"]);
    assert_eq!(peeked[0].tags, ["ci"]);

    let none = [("Shared".to_owned(), 0), ("Mine".to_owned(), 0)];
    assert_eq!(
        hits(store),
        none,
        "in the order asked, each once, the other repository's left out"
    );
    assert_eq!(hits(store), none, "peeking twice counts no hit");

    store.get(&both(REPO), &[mine]).expect("get");
    assert_eq!(
        hits(store),
        [("Shared".to_owned(), 0), ("Mine".to_owned(), 1)],
        "get counts one"
    );

    assert!(matches!(store.peek(&both(REPO), &[]), Err(StoreError::Invalid(_))));
}

/// More global notes than one list page holds, so deleting covers every page.
const FILLERS: usize = 201;

pub fn forget_notes_deletes_every_listed_note(store: &mut dyn MemoryStore) {
    let mine = save(
        store,
        &repo(REPO),
        &tagged("Mine", "Body", NoteKind::Note, &[], &["auth.rs", "db.rs"]),
    );
    save(
        store,
        &repo(REPO),
        &tagged("Second", "Body", NoteKind::Fix, &[], &["cache.rs"]),
    );
    save(
        store,
        &repo(OTHER),
        &tagged("Theirs", "Body", NoteKind::Note, &[], &["auth.rs"]),
    );

    for index in 0..FILLERS {
        let title = format!("Shared {index}");
        save(
            store,
            &Target::Global,
            &tagged(&title, "Body", NoteKind::Note, &[], &["tabs.md"]),
        );
    }

    store
        .link(
            &repo(REPO),
            &Link {
                from: "auth.rs".to_owned(),
                relation: "depends_on".to_owned(),
                to: "db.rs".to_owned(),
                note_id: Some(mine),
            },
        )
        .expect("link");

    let forgotten = store.forget_notes(&both(REPO), &[]).expect("forget notes");
    assert_eq!(
        forgotten,
        Forgotten {
            repo: 2,
            global: FILLERS
        }
    );

    assert_eq!(list(store, &listing(both(REPO))).total, 0);
    assert_eq!(
        titles(&list(store, &listing(both(OTHER))).rows),
        ["Theirs"],
        "another repository's notes stay"
    );

    let around = store
        .neighbors(&repo_only(REPO), "auth.rs", 1, 10)
        .expect("a related entity stays");
    assert_eq!(around.edges.len(), 1);
    assert_eq!(around.edges[0].note_id, None, "the relation lost its note");

    for (view, entity) in [(repo_only(REPO), "cache.rs"), (global_only(), "tabs.md")] {
        assert!(
            matches!(store.neighbors(&view, entity, 1, 10), Err(StoreError::NotFound(_))),
            "{entity} is used by nothing any more"
        );
    }

    assert!(
        store.neighbors(&repo_only(OTHER), "auth.rs", 1, 10).is_ok(),
        "another repository's entity stays"
    );

    assert_eq!(
        store.forget_notes(&both(REPO), &[]).expect("again"),
        Forgotten::default()
    );
}

pub fn forget_notes_leaves_the_excluded_notes(store: &mut dyn MemoryStore) {
    let kept = save(
        store,
        &repo(REPO),
        &tagged("Kept", "Body", NoteKind::Note, &[], &["kept.rs", "note-only.md"]),
    );
    save(
        store,
        &repo(REPO),
        &tagged("Gone", "Body", NoteKind::Fix, &[], &["gone.rs"]),
    );
    let rule = save(
        store,
        &Target::Global,
        &tagged("Rule", "Body", NoteKind::Note, &[], &["tabs.md"]),
    );
    save(store, &Target::Global, &draft("Other rule", "Body", NoteKind::Note));

    store
        .link(
            &repo(REPO),
            &Link {
                from: "kept.rs".to_owned(),
                relation: "uses".to_owned(),
                to: "lib.rs".to_owned(),
                note_id: Some(kept),
            },
        )
        .expect("link");

    let forgotten = store.forget_notes(&both(REPO), &[kept, rule]).expect("forget notes");
    assert_eq!(forgotten, Forgotten { repo: 1, global: 1 });

    let mut left = titles(&list(store, &listing(both(REPO))).rows)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    left.sort();
    assert_eq!(left, ["Kept", "Rule"]);

    let mut entities = store.peek(&both(REPO), &[kept]).expect("peek").remove(0).entities;
    entities.sort();
    assert_eq!(
        entities,
        ["kept.rs", "note-only.md"],
        "the notes left keep their entities"
    );

    let around = store.neighbors(&repo_only(REPO), "kept.rs", 1, 10).expect("neighbors");
    assert_eq!(around.edges[0].note_id, Some(kept), "the relation keeps its note");
    assert!(
        matches!(
            store.neighbors(&repo_only(REPO), "gone.rs", 1, 10),
            Err(StoreError::NotFound(_))
        ),
        "the deleted note's entity goes"
    );

    let too_many: Vec<i64> = (1..=501).collect();
    assert!(matches!(
        store.forget_notes(&both(REPO), &too_many),
        Err(StoreError::Invalid(_))
    ));
}

/// The notes of `view` in `sort` order (`descending` when asked), read one row per page, so the
/// pages are checked to follow the order too.
fn paged(store: &mut dyn MemoryStore, sort: NoteSort, descending: bool) -> Vec<NoteRow> {
    let query = NoteListQuery {
        sort,
        descending,
        ..listing(both(REPO))
    };
    let whole = list(store, &query).rows;

    let pages: Vec<NoteRow> = (0..whole.len() + 1)
        .flat_map(|offset| {
            list(
                store,
                &NoteListQuery {
                    offset,
                    limit: 1,
                    ..query.clone()
                },
            )
            .rows
        })
        .collect();

    assert_eq!(pages, whole, "{sort:?} pages one row at a time");
    whole
}

/// Asserts `rows` reversed are `reversed`.
fn assert_reversed(rows: &[NoteRow], reversed: &[NoteRow]) {
    let mut turned = rows.to_vec();
    turned.reverse();
    assert_eq!(turned, reversed);
}

/// Sorts by kind name and by scope (global first) break ties by id; a sort by place (one place
/// per store, so by id there) keeps every note once; each turns round with the order.
pub fn kinds_scopes_and_places_sort_and_page(store: &mut dyn MemoryStore) {
    save(store, &repo(REPO), &draft("Repo fix", "Body", NoteKind::Fix));
    save(
        store,
        &Target::Global,
        &draft("Global decision", "Body", NoteKind::Decision),
    );
    save(store, &repo(REPO), &draft("Repo decision", "Body", NoteKind::Decision));
    save(store, &Target::Global, &draft("Global note", "Body", NoteKind::Note));
    save(store, &repo(OTHER), &draft("Hidden", "Body", NoteKind::Location));

    let by_kind = paged(store, NoteSort::Kind, false);
    let kinds: Vec<&str> = by_kind.iter().map(|row| row.kind.as_str()).collect();
    assert_eq!(kinds, ["decision", "decision", "fix", "note"]);
    assert!(by_kind[0].id < by_kind[1].id, "a tie goes by id");
    assert_reversed(&by_kind, &paged(store, NoteSort::Kind, true));

    let by_scope = paged(store, NoteSort::Scope, false);
    let scopes: Vec<Scope> = by_scope.iter().map(|row| row.scope).collect();
    assert_eq!(scopes, [Scope::Global, Scope::Global, Scope::Repo, Scope::Repo]);
    assert!(by_scope[0].id < by_scope[1].id && by_scope[2].id < by_scope[3].id);
    assert_reversed(&by_scope, &paged(store, NoteSort::Scope, true));

    let by_place = paged(store, NoteSort::Place, false);
    let mut placed = titles(&by_place);
    placed.sort_unstable();
    assert_eq!(placed, ["Global decision", "Global note", "Repo decision", "Repo fix"]);
    assert_reversed(&by_place, &paged(store, NoteSort::Place, true));
}

/// The scope keeps the notes of one scope and combines with the text and the kind: the counts per
/// kind follow the text and the scope, the counts per scope the text and the kind, the total all
/// three; without a scope the counts per scope still come, under the text and the kind.
pub fn scope_text_and_kind_filter_together_and_count(store: &mut dyn MemoryStore) {
    save(store, &repo(REPO), &draft("Repo cache fix", "Clear it", NoteKind::Fix));
    save(store, &repo(REPO), &draft("Repo decision", "Tabs", NoteKind::Decision));
    save(store, &repo(REPO), &draft("Repo note", "The cache", NoteKind::Note));
    save(store, &repo(OTHER), &draft("Other cache fix", "Hidden", NoteKind::Fix));
    save(
        store,
        &Target::Global,
        &draft("Global cache fix", "Clear it", NoteKind::Fix),
    );
    save(
        store,
        &Target::Global,
        &draft("Global decision", "Spaces", NoteKind::Decision),
    );

    let scoped = |scope: Scope| NoteListQuery {
        scope: Some(scope),
        sort: NoteSort::Title,
        descending: false,
        ..listing(both(REPO))
    };

    let all = list(store, &listing(both(REPO)));
    assert_eq!(all.total, 5);
    assert_eq!(all.scopes, [(Scope::Repo, 3), (Scope::Global, 2)], "repository first");

    let repo_side = list(store, &scoped(Scope::Repo));
    assert_eq!(
        titles(&repo_side.rows),
        ["Repo cache fix", "Repo decision", "Repo note"]
    );
    assert_eq!(repo_side.total, 3, "the total follows the scope");
    assert_eq!(repo_side.scopes, all.scopes, "the counts per scope ignore the scope");
    assert_eq!(
        (
            count(&repo_side, NoteKind::Fix),
            count(&repo_side, NoteKind::Decision),
            count(&repo_side, NoteKind::Note)
        ),
        (1, 1, 1),
        "the counts per kind follow the scope"
    );

    let global_fixes = list(
        store,
        &NoteListQuery {
            kind: Some(NoteKind::Fix),
            ..scoped(Scope::Global)
        },
    );
    assert_eq!(titles(&global_fixes.rows), ["Global cache fix"]);
    assert_eq!(global_fixes.total, 1);
    assert_eq!(
        global_fixes.scopes,
        [(Scope::Repo, 1), (Scope::Global, 1)],
        "the counts per scope follow the kind"
    );
    assert_eq!(
        (
            count(&global_fixes, NoteKind::Fix),
            count(&global_fixes, NoteKind::Decision),
            count(&global_fixes, NoteKind::Note)
        ),
        (1, 1, 0)
    );

    let cached = list(
        store,
        &NoteListQuery {
            text: "CACHE".to_owned(),
            ..scoped(Scope::Repo)
        },
    );
    assert_eq!(titles(&cached.rows), ["Repo cache fix", "Repo note"]);
    assert_eq!(cached.total, 2);
    assert_eq!(cached.scopes, [(Scope::Repo, 2), (Scope::Global, 1)], "under the text");
    assert_eq!((count(&cached, NoteKind::Fix), count(&cached, NoteKind::Note)), (1, 1));

    let cached_fixes = list(
        store,
        &NoteListQuery {
            text: "cache".to_owned(),
            kind: Some(NoteKind::Fix),
            ..scoped(Scope::Repo)
        },
    );
    assert_eq!(titles(&cached_fixes.rows), ["Repo cache fix"]);
    assert_eq!(cached_fixes.total, 1);
    assert_eq!(cached_fixes.scopes, [(Scope::Repo, 1), (Scope::Global, 1)]);
    assert_eq!(count(&cached_fixes, NoteKind::Note), 1);

    let unscoped_fixes = list(
        store,
        &NoteListQuery {
            kind: Some(NoteKind::Fix),
            ..listing(both(REPO))
        },
    );
    assert_eq!(unscoped_fixes.total, 2);
    assert_eq!(unscoped_fixes.scopes, [(Scope::Repo, 1), (Scope::Global, 1)]);
    assert_eq!(count(&unscoped_fixes, NoteKind::Decision), 2, "every scope");

    let second = list(
        store,
        &NoteListQuery {
            offset: 1,
            limit: 1,
            ..scoped(Scope::Repo)
        },
    );
    assert_eq!(titles(&second.rows), ["Repo decision"], "pages follow the scope");
    assert_eq!(second.total, 3);

    let outside = list(
        store,
        &NoteListQuery {
            view: repo_only(REPO),
            ..scoped(Scope::Global)
        },
    );
    assert_eq!((outside.total, outside.rows.len()), (0, 0), "the view still decides");
    assert_eq!(outside.scopes, [(Scope::Repo, 3), (Scope::Global, 0)]);

    let global_view = list(
        store,
        &NoteListQuery {
            view: global_only(),
            ..scoped(Scope::Repo)
        },
    );
    assert_eq!((global_view.total, global_view.rows.len()), (0, 0));
    assert_eq!(scope_count(&global_view, Scope::Global), 2);
}

const ENGLISH_BODY: &str = "The sync job reads every changed note from the team database, then writes the new revision \
                            to the local cache so the next search finds it without another round trip.";

const POLISH_BODY: &str = "Zadanie synchronizacji czyta każdą zmienioną notatkę z bazy zespołu, a potem zapisuje nową \
                           wersję w lokalnej pamięci podręcznej, żeby następne wyszukiwanie znalazło ją od razu.";

/// Rows show the language a note was saved with, `--lang` filters the page and every count in the
/// database, and the filtered list pages like the unfiltered one (here over 200 notes).
pub fn lang_shows_filters_and_pages(store: &mut dyn MemoryStore) {
    const ENGLISH: usize = 15;
    const OTHER: usize = 205;

    for index in 0..ENGLISH {
        let target = if index % 3 == 0 { Target::Global } else { repo(REPO) };
        save(
            store,
            &target,
            &draft(&format!("Sync note {index}"), ENGLISH_BODY, NoteKind::Note),
        );
    }

    for index in 0..OTHER {
        let target = if index % 5 == 0 { Target::Global } else { repo(REPO) };
        let note = tagged(
            &format!("Notatka synchronizacji {index}"),
            POLISH_BODY,
            NoteKind::Fix,
            &[],
            &[],
        );
        save(store, &target, &note);
    }

    save(store, &repo(REPO), &draft("ok", "x", NoteKind::Note));

    let with_lang = |lang: Option<LangFilter>| NoteListQuery {
        lang,
        ..listing(both(REPO))
    };

    let everything = list(store, &with_lang(None));
    assert_eq!(everything.total, ENGLISH + OTHER + 1);

    for row in &everything.rows {
        let expected = if row.title.starts_with("Sync note") {
            Some(Lang::En)
        } else if row.title.starts_with("Notatka") {
            Some(Lang::Other)
        } else {
            None
        };

        assert_eq!(row.lang, expected, "{}", row.title);
    }

    let english = list(store, &with_lang(Some(LangFilter::En)));
    assert_eq!(english.total, ENGLISH);
    assert_eq!(english.rows.len(), ENGLISH);
    assert!(english.rows.iter().all(|row| row.lang == Some(Lang::En)));
    assert_eq!(english.kinds.iter().map(|(_, count)| count).sum::<usize>(), ENGLISH);
    assert_eq!(english.scopes.iter().map(|(_, count)| count).sum::<usize>(), ENGLISH);

    let unknown = list(store, &with_lang(Some(LangFilter::Unknown)));
    assert_eq!(unknown.total, 1);
    assert_eq!(unknown.rows[0].title, "ok");
    assert_eq!(unknown.rows[0].lang, None);

    // The filtered list is paged from the database: the pages cover it once, in order.
    let other = with_lang(Some(LangFilter::Other));
    let whole = list(store, &other);
    assert_eq!(whole.total, OTHER);
    assert_eq!(whole.rows.len(), 200, "a page holds at most 200 rows");

    let rest = list(
        store,
        &NoteListQuery {
            offset: 200,
            ..other.clone()
        },
    );
    assert_eq!((rest.total, rest.rows.len()), (OTHER, OTHER - 200));

    let mut paged: Vec<i64> = whole.rows.iter().chain(&rest.rows).map(|row| row.id).collect();
    assert!(
        whole
            .rows
            .iter()
            .chain(&rest.rows)
            .all(|row| row.lang == Some(Lang::Other))
    );
    paged.sort_unstable();
    paged.dedup();
    assert_eq!(paged.len(), OTHER);

    // It combines with the other filters and sorts like the kind filter does.
    let combined = list(
        store,
        &NoteListQuery {
            scope: Some(Scope::Global),
            kind: Some(NoteKind::Fix),
            sort: NoteSort::Title,
            descending: false,
            ..other
        },
    );
    assert_eq!(combined.total, OTHER / 5);
    assert!(combined.rows.windows(2).all(|pair| pair[0].title <= pair[1].title));
    assert_eq!(
        combined
            .scopes
            .iter()
            .find(|(scope, _)| *scope == Scope::Repo)
            .map(|(_, count)| *count),
        Some(OTHER - OTHER / 5),
        "the counts per scope follow the language filter but not the scope"
    );

    let note = store.peek(&both(REPO), &[whole.rows[0].id]).expect("peek");
    assert_eq!(note[0].lang, Some(Lang::Other));
}

/// The usage of the row of note `id` on a page.
fn usage_of(page: &NotePage, id: i64) -> NoteUsage {
    page.rows
        .iter()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("note {id} is listed"))
        .usage
        .expect("the store has the counters")
}

fn ids(rows: &[NoteRow]) -> Vec<i64> {
    rows.iter().map(|row| row.id).collect()
}

/// Counters start at 0, count each listed note once per call and nothing else of it, and sort and
/// filter the list: `ever` keeps the notes found useful, `never` those shown at least 3 times
/// without a use, whatever the sort, and the counts follow the filter.
pub fn usefulness_is_counted_listed_sorted_and_filtered(store: &mut dyn MemoryStore) {
    // Global and repository notes, so a split store has notes in both its stores.
    let saved: Vec<i64> = ["Alpha", "Bravo", "Charlie", "Delta", "Echo"]
        .iter()
        .enumerate()
        .map(|(index, title)| {
            let target = if index % 2 == 0 { Target::Global } else { repo(REPO) };
            save(store, &target, &draft(title, "How the build is cached", NoteKind::Fix))
        })
        .collect();
    let [a, b, c, d, e] = saved[..] else {
        panic!("five notes");
    };

    let fresh = list(store, &listing(both(REPO)));
    let untouched = NoteUsage {
        shown: 0,
        useful: 0,
        last_useful_at: None,
    };

    for id in [a, b, c, d, e] {
        assert_eq!(usage_of(&fresh, id), untouched, "note {id} starts uncounted");
    }

    // Three rounds of results show a to d (a listed twice in the first counts once), e once.
    store.record_shown(&[a, b, c, d, e, a]).expect("first round");
    store.record_shown(&[a, b, c, d]).expect("second round");
    store.record_shown(&[d, c, b, a]).expect("third round");
    store.record_shown(&[]).expect("no notes counts nothing");
    store.record_shown(&[a + 7_000_000]).expect("a missing note is ignored");

    store.record_useful(&[a], 1_000).expect("a is useful");
    store.record_useful(&[b], 2_000).expect("b is useful");
    store.record_useful(&[b, b], 3_000).expect("b is useful again, once");

    let counted = list(store, &listing(both(REPO)));
    let counters = |shown, useful, last_useful_at| NoteUsage {
        shown,
        useful,
        last_useful_at,
    };

    assert_eq!(usage_of(&counted, a), counters(3, 1, Some(1_000)));
    assert_eq!(usage_of(&counted, b), counters(3, 2, Some(3_000)));
    assert_eq!(usage_of(&counted, c), counters(3, 0, None));
    assert_eq!(usage_of(&counted, d), counters(3, 0, None));
    assert_eq!(usage_of(&counted, e), counters(1, 0, None));

    // Only the counters changed: not the update time, the revision nor the hits.
    for (before, after) in fresh.rows.iter().zip(&counted.rows) {
        assert_eq!(
            (before.id, before.updated_at, before.revision, before.hits),
            (after.id, after.updated_at, after.revision, after.hits)
        );
    }

    let read = store.peek(&both(REPO), &[b]).expect("peek");
    assert_eq!(read[0].usage, Some(counters(3, 2, Some(3_000))));
    assert_eq!((read[0].revision, read[0].hits), (1, 0));

    // Most useful first, ties by id the same way; a note never found useful sorts as 0.
    let by_useful = NoteListQuery {
        sort: NoteSort::Useful,
        ..listing(both(REPO))
    };
    let mut rest = [c, d, e];
    rest.sort_unstable();
    rest.reverse();
    let mut expected = vec![b, a];
    expected.extend(rest);
    assert_eq!(ids(&list(store, &by_useful).rows), expected);

    let ascending = NoteListQuery {
        descending: false,
        ..by_useful
    };
    expected.reverse();
    assert_eq!(ids(&list(store, &ascending).rows), expected);

    let by_last_useful = NoteListQuery {
        sort: NoteSort::LastUseful,
        ..listing(both(REPO))
    };
    let mut expected = vec![b, a];
    expected.extend(rest);
    assert_eq!(ids(&list(store, &by_last_useful).rows), expected);

    // The filters narrow the page and every count.
    let ever = list(
        store,
        &NoteListQuery {
            useful: Some(UsefulFilter::Ever),
            sort: NoteSort::LastUseful,
            ..listing(both(REPO))
        },
    );
    assert_eq!(ids(&ever.rows), vec![b, a]);
    assert_eq!(ever.total, 2);
    assert_eq!(ever.kinds.iter().map(|(_, count)| count).sum::<usize>(), 2);
    assert_eq!(ever.scopes.iter().map(|(_, count)| count).sum::<usize>(), 2);

    let never = NoteListQuery {
        useful: Some(UsefulFilter::Never),
        descending: false,
        sort: NoteSort::Title,
        ..listing(both(REPO))
    };
    let never_page = list(store, &never);
    assert_eq!(
        titles(&never_page.rows),
        vec!["Charlie", "Delta"],
        "echo was shown once only"
    );
    assert_eq!(never_page.total, 2);
    assert_eq!(never_page.kinds.iter().map(|(_, count)| count).sum::<usize>(), 2);

    let second = list(
        store,
        &NoteListQuery {
            offset: 1,
            limit: 1,
            ..never
        },
    );
    assert_eq!((titles(&second.rows), second.total), (vec!["Delta"], 2));

    // A fourth round shows echo twice more: now it qualifies too.
    store.record_shown(&[e]).expect("echo");
    store.record_shown(&[e]).expect("echo");
    let again = list(
        store,
        &NoteListQuery {
            useful: Some(UsefulFilter::Never),
            ..listing(both(REPO))
        },
    );
    assert_eq!(again.total, 3);
}

/// `draft` written by `name` through the client `via` (nobody for `None`).
fn written_by(draft: NoteDraft, name: Option<&str>, via: Option<&str>) -> NoteDraft {
    let mut draft = draft;
    draft.provenance.author = name.map(|name| Author {
        name: name.to_owned(),
        via: via.map(str::to_owned),
    });

    draft
}

/// The people and the unknown count `rows` say, as [`NotePage::authors`] lists them: the most
/// notes first, then by ASCII-folded name, then by name.
fn facet_of_rows(rows: &[NoteRow]) -> (Vec<(String, usize)>, usize) {
    let mut people: Vec<(String, usize)> = Vec::new();
    let mut unknown = 0;

    for row in rows {
        match &row.authors.added_by {
            Some(author) => match people.iter_mut().find(|(name, _)| *name == author.name) {
                Some(person) => person.1 += 1,
                None => people.push((author.name.clone(), 1)),
            },
            None => unknown += 1,
        }
    }

    people.sort_by(|(a, a_count), (b, b_count)| {
        b_count
            .cmp(a_count)
            .then_with(|| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()))
            .then_with(|| a.cmp(b))
    });

    (people, unknown)
}

fn facet_of_page(page: &NotePage) -> (Vec<(String, usize)>, usize) {
    (page.authors.people(), page.authors.unknown())
}

/// The names of whoever added `rows`, `None` for an unknown author.
fn added_by(rows: &[NoteRow]) -> Vec<Option<&str>> {
    rows.iter()
        .map(|row| row.authors.added_by.as_ref().map(|author| author.name.as_str()))
        .collect()
}

/// Asserts `rows` are sorted by who added them, as `--sort addedBy` promises: names with ASCII
/// letters folded in the direction asked, ties by id the same way, the unknown last either way.
fn assert_added_by_order(rows: &[NoteRow], descending: bool) {
    let direct = |order: std::cmp::Ordering| if descending { order.reverse() } else { order };

    for pair in rows.windows(2) {
        let name = |row: &NoteRow| {
            row.authors
                .added_by
                .as_ref()
                .map(|author| author.name.to_ascii_lowercase())
        };

        let order = match (name(&pair[0]), name(&pair[1])) {
            (Some(a), Some(b)) => direct(a.cmp(&b).then(pair[0].id.cmp(&pair[1].id))),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => direct(pair[0].id.cmp(&pair[1].id)),
        };

        assert!(
            order == std::cmp::Ordering::Less,
            "{:?} (id {}) before {:?} (id {}), descending {descending}",
            pair[0].authors.added_by,
            pair[0].id,
            pair[1].authors.added_by,
            pair[1].id
        );
    }
}

/// Asserts the facet of `query` counts what the rows of the same query without an author filter
/// say, whichever author filter `query` has.
fn assert_facet_follows_rows(store: &mut dyn MemoryStore, query: &NoteListQuery) {
    let everyone = list(
        store,
        &NoteListQuery {
            author: AuthorFilter::Any,
            ..query.clone()
        },
    );
    let expected = facet_of_rows(&everyone.rows);
    assert_eq!(facet_of_page(&everyone), expected, "{query:?}");

    for author in [
        AuthorFilter::Named("Ada".to_owned()),
        AuthorFilter::Named("Nobody".to_owned()),
        AuthorFilter::Unknown,
    ] {
        let page = list(
            store,
            &NoteListQuery {
                author: author.clone(),
                ..query.clone()
            },
        );
        assert_eq!(
            facet_of_page(&page),
            expected,
            "the facet ignores {author:?} for {query:?}"
        );
    }
}

/// The author filter keeps the notes one person added (exactly, case-sensitive, whatever client the
/// write went through) or nobody known, and narrows the page, the total and the counts; the facet
/// counts who added the notes every other filter keeps; the sort puts the unknown last.
pub fn added_by_filters_sorts_and_counts_who_added(store: &mut dyn MemoryStore) {
    let note = |title: &str, kind: NoteKind| tagged(title, "A body", kind, &[], &[]);
    let cached = |title: &str, kind: NoteKind| tagged(title, "A body", kind, &[], &["cache"]);

    let alpha = save(
        store,
        &repo(REPO),
        &written_by(cached("Alpha", NoteKind::Fix), Some("Ada"), Some("claude-code")),
    );
    let bravo = save(
        store,
        &repo(REPO),
        &written_by(note("Bravo", NoteKind::Decision), Some("Ada"), None),
    );
    let charlie = save(
        store,
        &Target::Global,
        &written_by(note("Charlie", NoteKind::Fix), Some("ada"), None),
    );
    let delta = save(
        store,
        &repo(REPO),
        &written_by(note("Delta", NoteKind::Note), Some("bob"), None),
    );
    let echo = save(
        store,
        &Target::Global,
        &written_by(note("Echo", NoteKind::Note), None, None),
    );
    let foxtrot = save(
        store,
        &repo(REPO),
        &written_by(note("Foxtrot", NoteKind::Fix), None, None),
    );
    let golf = save(
        store,
        &repo(REPO),
        &written_by(cached("Golf", NoteKind::Problem), Some("Grace"), Some("omp")),
    );
    let hotel = save(
        store,
        &repo(REPO),
        &written_by(note("Hotel", NoteKind::Fix), Some("Bob"), None),
    );
    save(
        store,
        &repo(OTHER),
        &written_by(note("Other", NoteKind::Fix), Some("Zed"), None),
    );

    let named = |name: &str| AuthorFilter::Named(name.to_owned());
    let with = |author: AuthorFilter| NoteListQuery {
        author,
        sort: NoteSort::Title,
        descending: false,
        ..listing(both(REPO))
    };
    let people = |counts: &[(&str, usize)]| -> Vec<(String, usize)> {
        counts
            .iter()
            .map(|(name, count)| ((*name).to_owned(), *count))
            .collect()
    };
    let everyone = [("Ada", 2), ("ada", 1), ("Bob", 1), ("bob", 1), ("Grace", 1)];

    // No filter: everything, and the facet says who added it. Ada and Ada via a client are one.
    let all = list(store, &with(AuthorFilter::Any));
    assert_eq!(all.total, 8);
    assert_eq!(facet_of_page(&all), (people(&everyone), 2));

    // A name keeps the notes it added, whatever client wrote them.
    let ada = list(store, &with(named("Ada")));
    assert_eq!(titles(&ada.rows), ["Alpha", "Bravo"]);
    assert_eq!(ada.total, 2);
    assert_eq!(count(&ada, NoteKind::Fix), 1);
    assert_eq!(count(&ada, NoteKind::Decision), 1);
    assert_eq!(count(&ada, NoteKind::Note), 0, "the counts per kind follow the author");
    assert_eq!(ada.scopes, [(Scope::Repo, 2), (Scope::Global, 0)]);
    assert_eq!(facet_of_page(&ada), facet_of_page(&all), "the facet ignores the author");

    // Names are not folded: another case is another person.
    assert_eq!(titles(&list(store, &with(named("ada"))).rows), ["Charlie"]);
    assert_eq!(titles(&list(store, &with(named("bob"))).rows), ["Delta"]);
    assert_eq!(titles(&list(store, &with(named("ADA"))).rows), Vec::<&str>::new());

    // A name nobody has keeps nothing, and the facet still lists the others.
    let nobody = list(store, &with(named("Nobody")));
    assert_eq!((nobody.rows.len(), nobody.total), (0, 0));
    assert_eq!(nobody.kinds.iter().map(|(_, count)| count).sum::<usize>(), 0);
    assert_eq!(facet_of_page(&nobody), facet_of_page(&all));

    // Unknown keeps the notes nobody known added.
    let unknown = list(store, &with(AuthorFilter::Unknown));
    assert_eq!(titles(&unknown.rows), ["Echo", "Foxtrot"]);
    assert_eq!(unknown.total, 2);
    assert_eq!(unknown.scopes, [(Scope::Repo, 1), (Scope::Global, 1)]);
    assert_eq!(facet_of_page(&unknown), facet_of_page(&all));

    // The author combines with the other filters; the facet follows those and not the author.
    let fixes = NoteListQuery {
        kind: Some(NoteKind::Fix),
        ..with(named("Ada"))
    };
    let page = list(store, &fixes);
    assert_eq!(titles(&page.rows), ["Alpha"]);
    assert_eq!(page.total, 1);
    assert_eq!(
        facet_of_page(&page),
        (people(&[("Ada", 1), ("ada", 1), ("Bob", 1)]), 1),
        "Alpha, Charlie, Foxtrot and Hotel are the fixes"
    );

    let global = NoteListQuery {
        scope: Some(Scope::Global),
        ..with(AuthorFilter::Any)
    };
    assert_eq!(facet_of_page(&list(store, &global)), (people(&[("ada", 1)]), 1));

    let text = NoteListQuery {
        text: "delta".to_owned(),
        ..with(named("Ada"))
    };
    let page = list(store, &text);
    assert_eq!((page.rows.len(), page.total), (0, 0));
    assert_eq!(facet_of_page(&page), (people(&[("bob", 1)]), 0));

    let cache = NoteListQuery {
        entity: Some("cache".to_owned()),
        ..with(AuthorFilter::Any)
    };
    assert_eq!(
        facet_of_page(&list(store, &cache)),
        (people(&[("Ada", 1), ("Grace", 1)]), 0)
    );

    let left_out = NoteListQuery {
        exclude: vec![alpha, echo],
        ..with(AuthorFilter::Any)
    };
    let page = list(store, &left_out);
    assert_eq!(page.total, 6);
    assert_eq!(
        facet_of_page(&page),
        (
            people(&[("Ada", 1), ("ada", 1), ("Bob", 1), ("bob", 1), ("Grace", 1)]),
            1
        )
    );

    // A list that does not ask who added the notes gets none, and everything else as before.
    let quiet = list(
        store,
        &NoteListQuery {
            want_authors: false,
            ..with(named("Ada"))
        },
    );
    assert_eq!(titles(&quiet.rows), ["Alpha", "Bravo"]);
    assert_eq!(quiet.total, 2);
    assert_eq!(facet_of_page(&quiet), (Vec::new(), 0));

    // Whatever the other filters are, the facet counts what their rows say.
    let mut filtered = vec![with(AuthorFilter::Any), fixes, global, text, cache, left_out];
    filtered.extend(
        [LangFilter::En, LangFilter::Other, LangFilter::Unknown].map(|lang| NoteListQuery {
            lang: Some(lang),
            ..with(AuthorFilter::Any)
        }),
    );
    filtered.extend([UsefulFilter::Ever, UsefulFilter::Never].map(|useful| NoteListQuery {
        useful: Some(useful),
        ..with(AuthorFilter::Any)
    }));

    for query in &filtered {
        assert_facet_follows_rows(store, query);
    }

    // Sorted by who added them: folded names, ties by id, the unknown last both ways.
    let sorted = |descending: bool| NoteListQuery {
        sort: NoteSort::AddedBy,
        descending,
        ..listing(both(REPO))
    };

    let folded = |rows: &[NoteRow]| -> Vec<Option<String>> {
        added_by(rows)
            .iter()
            .map(|name| name.map(str::to_ascii_lowercase))
            .collect()
    };
    let names = |named: [&str; 6]| -> Vec<Option<String>> {
        named
            .iter()
            .map(|name| Some((*name).to_owned()))
            .chain([None, None])
            .collect()
    };

    let ascending = list(store, &sorted(false));
    assert_eq!(
        folded(&ascending.rows),
        names(["ada", "ada", "ada", "bob", "bob", "grace"])
    );
    assert_added_by_order(&ascending.rows, false);

    let descending = list(store, &sorted(true));
    assert_added_by_order(&descending.rows, true);
    assert_eq!(
        folded(&descending.rows),
        names(["grace", "bob", "bob", "ada", "ada", "ada"])
    );

    // The sort combines with a filter and with paging.
    let second = list(
        store,
        &NoteListQuery {
            offset: 3,
            limit: 3,
            ..sorted(false)
        },
    );
    assert_eq!(ids(&second.rows), ids(&ascending.rows)[3..6]);

    let fixes_only = list(
        store,
        &NoteListQuery {
            kind: Some(NoteKind::Fix),
            ..sorted(true)
        },
    );
    assert_added_by_order(&fixes_only.rows, true);
    assert_eq!(fixes_only.rows.len(), 4);

    let every: Vec<i64> = vec![alpha, bravo, charlie, delta, echo, foxtrot, golf, hotel];
    let mut listed = ids(&ascending.rows);
    listed.sort_unstable();
    let mut wanted = every;
    wanted.sort_unstable();
    assert_eq!(listed, wanted, "the other repository's note is not listed");
}

/// A long list pages through the author sort both ways without a gap or an overlap, the facet
/// counts every person (its sum is the total) and the cap keeps the name that is asked for.
pub fn added_by_pages_cover_the_list_and_the_facet_is_capped(store: &mut dyn MemoryStore) {
    // 250 people: 100 added 6 notes each, 100 added 3, 50 added 1; 250 notes have no author.
    let mut writers: Vec<Option<usize>> = Vec::new();

    for person in 0..250_usize {
        let notes = match person {
            0..100 => 6,
            100..200 => 3,
            _ => 1,
        };
        writers.extend(std::iter::repeat_n(Some(person), notes));
    }

    writers.extend(std::iter::repeat_n(None, 250));

    for (index, writer) in writers.iter().enumerate() {
        let name = writer.map(|person| format!("Person {person:03}"));
        let note = written_by(
            draft(&format!("Note {index:04}"), "A body", NoteKind::Note),
            name.as_deref(),
            None,
        );
        save(store, &repo(REPO), &note);
    }

    let total = writers.len();
    assert_eq!(total, 1200);

    for descending in [false, true] {
        let mut seen: Vec<NoteRow> = Vec::new();

        for page in 0..6 {
            let rows = list(
                store,
                &NoteListQuery {
                    sort: NoteSort::AddedBy,
                    descending,
                    offset: page * 200,
                    limit: 200,
                    ..listing(both(REPO))
                },
            )
            .rows;
            assert_eq!(rows.len(), 200, "page {page}");
            seen.extend(rows);
        }

        let mut unique: Vec<i64> = ids(&seen);
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            total,
            "no row twice and none missing, descending {descending}"
        );
        assert_added_by_order(&seen, descending);
    }

    // The facet lists every person, whatever the author filter, and adds up to the list.
    let person = |index: usize| format!("Person {index:03}");

    for author in [
        AuthorFilter::Any,
        AuthorFilter::Named(person(249)),
        AuthorFilter::Unknown,
    ] {
        let page = list(
            store,
            &NoteListQuery {
                author,
                limit: 1,
                ..listing(both(REPO))
            },
        );
        let people = page.authors.people();
        assert_eq!(people.len(), 250);
        assert_eq!(
            people.iter().map(|(_, count)| count).sum::<usize>() + page.authors.unknown(),
            total
        );
        assert_eq!(page.authors.unknown(), 250);
    }

    // The most notes first, then by name; only 200 of the 250 are kept.
    let page = list(store, &listing(both(REPO)));
    let capped = page.authors.clone().capped(None, page.total);
    let expected: Vec<(String, usize)> = (0..200)
        .map(|index| (person(index), if index < 100 { 6 } else { 3 }))
        .collect();
    assert_eq!(capped, expected);

    // A name beyond the cap is kept, with the notes the list keeps.
    let picked = list(
        store,
        &NoteListQuery {
            author: AuthorFilter::Named(person(249)),
            ..listing(both(REPO))
        },
    );
    assert_eq!(picked.total, 1);
    let capped = picked.authors.clone().capped(Some(&person(249)), picked.total);
    assert_eq!(capped.len(), 200);
    assert!(capped.contains(&(person(249), 1)));

    // Even a name nobody has.
    let missing = list(
        store,
        &NoteListQuery {
            author: AuthorFilter::Named("Nobody".to_owned()),
            ..listing(both(REPO))
        },
    );
    assert_eq!(missing.total, 0);
    let capped = missing.authors.clone().capped(Some("Nobody"), missing.total);
    assert_eq!(capped.len(), 200);
    assert!(capped.contains(&("Nobody".to_owned(), 0)));
}
