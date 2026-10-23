//! The MongoDB store in its Amazon DocumentDB mode, against the MongoDB test server: the schema
//! without a text index, search and similar notes over the `terms`, and the shared listing,
//! script, team hook and team routine cases. They skip unless `HARNESS_MEMORY_TEST_MONGO_URL`
//! names a server (see `mongo_support`); `HARNESS_MEMORY_TEST_MONGO_FLAVOR=documentdb` runs the
//! whole MongoDB suite in this mode too.

mod common;
mod mongo_support;

use common::{both, draft, query, repo};
use harness_memory::store::mongo::schema::SCHEMA_VERSION;
use harness_memory::store::{Forget, Hit, Link, MemoryStore, MongoStore, NoteKind, NotePatch, ScriptStore};
use harness_memory::store::{StoreError, Target, TeamDocStore};
use mongo_support::TestDb;
use mongodb::bson::{Bson, Document, doc};
use pretty_assertions::assert_eq;

const REPO: &str = "github.com/acme/widget";

fn ids(hits: &[Hit]) -> Vec<i64> {
    hits.iter().map(|hit| hit.id).collect()
}

fn tagged(title: &str, body: &str, tags: &[&str]) -> harness_memory::store::NoteDraft {
    let mut note = draft(title, body, NoteKind::Note);
    note.tags = tags.iter().map(|tag| (*tag).to_owned()).collect();
    note
}

fn save(store: &mut MongoStore, title: &str, body: &str, tags: &[&str]) -> i64 {
    store
        .remember(&repo(REPO), &tagged(title, body, tags))
        .expect("save")
        .id
}

#[test]
fn documentdb_migration_creates_no_text_index() {
    let Some((db, store)) = mongo_support::documentdb_store() else {
        return;
    };

    assert!(!store.is_newer());
    assert_eq!(db.schema_version(), SCHEMA_VERSION);
    assert_eq!(
        db.index_names("notes"),
        [
            "_id_",
            "notes_entities",
            "notes_recent",
            "notes_terms",
            "notes_terms_en",
            "notes_title"
        ]
    );
    assert!(db.index_names("scripts").contains(&"scripts_name".to_owned()));
    assert!(db.index_names("hooks").contains(&"hooks_repo".to_owned()));
    assert!(db.index_names("routines").contains(&"routines_repo".to_owned()));
}

#[test]
fn documentdb_search_ranks_title_then_tags_then_body_matches() {
    let Some((_db, mut store)) = mongo_support::documentdb_store() else {
        return;
    };
    // Saved best match first, so the most recently updated note is the weakest match.
    let title = save(&mut store, "WebKit flags", "set at build time", &[]);
    let tags = save(&mut store, "Rendering notes", "nothing else", &["webkit"]);
    let body = save(&mut store, "Startup order", "the webkit renderer starts first", &[]);
    save(&mut store, "Unrelated", "release checklist", &["ci"]);

    let hits = store.search(&query("webkit", both(REPO))).expect("search");
    assert_eq!(ids(&hits), vec![title, tags, body]);

    let several = store.search(&query("webkit flags", both(REPO))).expect("two words");
    assert_eq!(ids(&several)[0], title, "the note matching both words comes first");
}

#[test]
fn documentdb_search_folds_case_and_diacritics_and_matches_prefixes() {
    let Some((_db, mut store)) = mongo_support::documentdb_store() else {
        return;
    };
    let cafe = save(&mut store, "Café menu", "Crème brûlée", &[]);
    let hardening = save(&mut store, "Hardening of git", "flags", &[]);

    for (text, expected) in [("CAFE", cafe), ("creme", cafe), ("brul", cafe), ("hard", hardening)] {
        assert_eq!(
            ids(&store.search(&query(text, both(REPO))).expect("search")),
            vec![expected],
            "{text}"
        );
    }

    let hit = store.search(&query("brulee", both(REPO))).expect("search").remove(0);
    assert!(hit.snippet.contains("brûlée"), "{}", hit.snippet);
}

#[test]
fn documentdb_global_notes_rank_after_repository_ones() {
    let Some((_db, mut store)) = mongo_support::documentdb_store() else {
        return;
    };
    let global = store
        .remember(
            &Target::Global,
            &draft("Webkit rendering webkit", "webkit webkit webkit", NoteKind::Note),
        )
        .expect("global");
    let local = save(&mut store, "Sidebar", "mentions webkit once", &[]);

    let hits = store.search(&query("webkit", both(REPO))).expect("search");
    assert_eq!(ids(&hits), vec![local, global.id]);
}

#[test]
fn documentdb_similar_notes_share_words_and_leave_out_the_note_itself() {
    let Some((_db, mut store)) = mongo_support::documentdb_store() else {
        return;
    };
    let old = save(
        &mut store,
        "Webview freezes on resize",
        "WebKitGTK compositor stalls while resizing",
        &[],
    );
    save(&mut store, "Release checklist", "Tag, build, publish", &[]);
    let new = save(
        &mut store,
        "Resize freezes fixed",
        "Disabled the compositor during resize",
        &[],
    );

    let similar = store.similar(&both(REPO), new, 3).expect("similar");
    assert_eq!(ids(&similar), vec![old]);
}

#[test]
fn documentdb_notes_are_saved_updated_and_forgotten() {
    let Some((_db, mut store)) = mongo_support::documentdb_store() else {
        return;
    };
    let first = save(&mut store, "Pool size", "ten connections", &[]);
    let again = save(&mut store, "Pool size", "twenty connections", &[]);
    assert_eq!(again, first, "the same title updates the note");

    let note = store.get(&both(REPO), &[first]).expect("get").remove(0);
    assert_eq!(note.body, "twenty connections");
    assert_eq!(note.revision, 2);

    assert_eq!(
        ids(&store.search(&query("twenty", both(REPO))).expect("search")),
        vec![first]
    );
    assert!(store.search(&query("ten", both(REPO))).expect("search").is_empty());

    let forgotten = store
        .forget(&both(REPO), &repo(REPO), &harness_memory::store::Forget::Note(first))
        .expect("forget");
    assert_eq!(forgotten, 1);
    assert!(store.get(&both(REPO), &[first]).expect("get").is_empty());
}

#[test]
fn documentdb_listing_cases() {
    for (name, case) in common::listing::CASES {
        eprintln!("list case {name}");
        let Some((_db, mut store)) = mongo_support::documentdb_store() else {
            return;
        };
        case(&mut store);
    }
}

#[test]
fn documentdb_script_cases() {
    for (name, case) in common::scripts::CASES {
        eprintln!("script case {name}");
        let Some((_db, mut store)) = mongo_support::documentdb_store() else {
            return;
        };
        case(&mut store as &mut dyn ScriptStore);
    }
}

#[test]
fn documentdb_hook_cases() {
    for (name, case) in common::hooks::CASES {
        eprintln!("hook case {name}");
        let Some((_db, mut store)) = mongo_support::documentdb_store() else {
            return;
        };
        case(&mut store);
    }
}

#[test]
fn documentdb_routine_cases() {
    for (name, case) in common::routines::CASES {
        eprintln!("routine case {name}");
        let Some((_db, mut store)) = mongo_support::documentdb_store() else {
            return;
        };
        case(&mut store as &mut dyn TeamDocStore);
    }
}

#[test]
fn a_database_set_up_for_documentdb_still_opens_as_mongodb_and_says_what_search_needs() {
    let Some(db) = TestDb::new() else { return };
    let mut documentdb = db.connect_documentdb();
    let id = save(&mut documentdb, "Shared", "one database", &[]);

    let mut mongodb = MongoStore::connect(&db.url).expect("the same database as MongoDB");
    assert_eq!(mongodb.get(&both(REPO), &[id]).expect("get").len(), 1);

    let error = mongodb
        .search(&query("database", both(REPO)))
        .expect_err("no text index");
    assert!(matches!(error, StoreError::Backend(_)), "{error:?}");
    assert!(error.to_string().contains("database_kind = \"documentdb\""), "{error}");

    // The other way round, a database MongoDB set up searches the same in the DocumentDB mode.
    let Some(other) = TestDb::new() else { return };
    let mut mongodb = MongoStore::connect(&other.url).expect("MongoDB");
    let id = save(&mut mongodb, "Shared", "one database", &[]);
    let mut documentdb = other.connect_documentdb();
    assert_eq!(
        ids(&documentdb.search(&query("database", both(REPO))).expect("search")),
        vec![id]
    );
}

#[test]
fn an_unreachable_documentdb_cluster_points_at_the_vpn() {
    // Nothing listens on port 1; the driver gives up after the server selection timeout.
    let url =
        "mongodb://alice:s3cret-pass@127.0.0.1:1/team?directConnection=true&tls=false&serverSelectionTimeoutMS=200";
    let flavor = harness_memory::store::mongo::Flavor::DocumentDb;
    let error = MongoStore::connect_as(url, &flavor).expect_err("unreachable");

    let message = error.to_string();
    assert!(matches!(error, StoreError::Backend(_)), "{error:?}");
    assert!(
        message.ends_with(
            "Amazon DocumentDB only accepts connections from inside its AWS network: connect to your VPN and try \
             again."
        ),
        "{message}"
    );
    assert!(!message.contains("s3cret") && !message.contains(';'), "{message}");

    let mongodb = MongoStore::connect(url).expect_err("unreachable").to_string();
    assert!(!mongodb.contains("VPN"), "{mongodb}");
}

#[test]
fn a_text_index_mongodb_cannot_build_points_at_the_documentdb_kind() {
    let Some(db) = TestDb::new() else { return };

    // A server that refuses the text index, as Amazon DocumentDB does: here an index of the same
    // name with other keys is in the way.
    db.raw()
        .collection::<mongodb::bson::Document>("notes")
        .create_index(
            mongodb::IndexModel::builder()
                .keys(mongodb::bson::doc! { "title": 1 })
                .options(
                    mongodb::options::IndexOptions::builder()
                        .name("notes_text".to_owned())
                        .build(),
                )
                .build(),
        )
        .run()
        .expect("an index in the way");

    let error = MongoStore::connect(&db.url).expect_err("no text index");
    let message = error.to_string();
    assert!(matches!(error, StoreError::Backend(_)), "{error:?}");
    assert!(message.contains("set database_kind = \"documentdb\""), "{message}");
    assert!(!message.contains(';'), "{message}");

    let store = db.connect_documentdb();
    assert!(!store.is_newer());
    assert_eq!(db.schema_version(), SCHEMA_VERSION);
}

/// A fresh database whose every operation the server records (in `system.profile`), with a store
/// on it in the Amazon DocumentDB mode.
fn profiled_store() -> Option<(TestDb, MongoStore)> {
    let db = TestDb::new()?;
    let raw = db.raw();
    raw.run_command(doc! { "create": "system.profile", "capped": true, "size": 64_i64 * 1024 * 1024 })
        .run()
        .expect("profile collection");
    raw.run_command(doc! { "profile": 2 }).run().expect("profiling on");

    let store = db.connect_documentdb();
    Some((db, store))
}

/// The `find` and `findAndModify` commands the server received whose projection holds more than
/// field inclusions and exclusions: an expression there is MongoDB 4.4 and newer, which Amazon
/// DocumentDB refuses.
fn expression_projections(db: &TestDb) -> Vec<Document> {
    let profiled: Vec<Document> = db
        .raw()
        .collection::<Document>("system.profile")
        .find(doc! {})
        .run()
        .expect("profile")
        .collect::<Result<_, _>>()
        .expect("read profile");
    assert!(!profiled.is_empty(), "the server recorded the operations");

    profiled
        .into_iter()
        .filter_map(|entry| {
            let command = entry.get_document("command").ok()?;
            let projection = if command.contains_key("find") {
                command.get_document("projection").ok()?
            } else if command.contains_key("findAndModify") {
                command.get_document("fields").ok()?
            } else {
                return None;
            };

            projection.values().any(|value| !plain(value)).then(|| command.clone())
        })
        .collect()
}

/// True for a field inclusion or exclusion.
fn plain(value: &Bson) -> bool {
    match value {
        Bson::Int32(value) => matches!(value, 0 | 1),
        Bson::Int64(value) => matches!(value, 0 | 1),
        Bson::Double(value) => *value == 0.0 || *value == 1.0,
        Bson::Boolean(_) => true,
        _ => false,
    }
}

#[test]
fn documentdb_note_reads_and_writes_send_no_projection_expression() {
    let Some((db, mut store)) = profiled_store() else {
        return;
    };
    let mut note = tagged("Parser cache", "the diff parser caches tokens", &["perf"]);
    note.entities = vec!["Zeta".to_owned(), "alpha".to_owned(), "Mid".to_owned()];
    let first = store.remember(&repo(REPO), &note).expect("save").id;
    let second = save(&mut store, "Parser speed", "tokens are cached by the parser", &[]);

    let patch = NotePatch {
        append: Some("and evicts them".to_owned()),
        ..NotePatch::default()
    };
    store.update(&both(REPO), first, &patch).expect("update");

    let link = Link {
        from: "alpha".to_owned(),
        relation: "uses".to_owned(),
        to: "Zeta".to_owned(),
        note_id: Some(second),
    };
    let edge = store.link(&repo(REPO), &link).expect("link");
    assert_eq!((edge.from.as_str(), edge.to.as_str()), ("alpha", "Zeta"));

    // A counted read returns the note as it was before its own hit, entities by normalized name.
    let read = store.get(&both(REPO), &[first]).expect("get").remove(0);
    assert_eq!(read.entities, ["alpha", "Mid", "Zeta"]);
    assert_eq!(read.hits, 0);
    assert_eq!(store.get(&both(REPO), &[first]).expect("get again")[0].hits, 1);
    assert_eq!(store.peek(&both(REPO), &[first]).expect("peek")[0].hits, 2);

    assert_eq!(
        ids(&store.search(&query("", both(REPO))).expect("recent")),
        vec![first, second]
    );
    assert_eq!(
        ids(&store.search(&query("parser", both(REPO))).expect("search")).len(),
        2
    );
    assert_eq!(
        ids(&store.search(&query("evict", both(REPO))).expect("prefix")),
        vec![first]
    );
    store.similar(&both(REPO), second, 3).expect("similar");

    let around = store.neighbors(&both(REPO), "alpha", 2, 20).expect("neighbors");
    assert_eq!(around.edges.len(), 1);
    let mut linked: Vec<i64> = around.notes.iter().map(|(id, _)| *id).collect();
    linked.sort_unstable();
    assert_eq!(
        linked,
        vec![first, second],
        "the relation's note and the entities' note"
    );

    let found = store.find_by_title(&repo(REPO), "parser SPEED").expect("by title");
    assert_eq!(found.map(|note| note.id), Some(second));

    store
        .forget(&both(REPO), &repo(REPO), &Forget::Entity("Mid".to_owned()))
        .expect("forget entity");
    store
        .forget(&both(REPO), &repo(REPO), &Forget::Note(second))
        .expect("forget note");
    store.forget_notes(&both(REPO), &[]).expect("forget all");

    assert_eq!(expression_projections(&db), Vec::<Document>::new());
}

#[test]
fn documentdb_listing_script_hook_and_routine_cases_send_no_projection_expression() {
    let mut sent: Vec<Document> = Vec::new();

    for (name, case) in common::listing::CASES {
        eprintln!("list case {name}");
        let Some((db, mut store)) = profiled_store() else {
            return;
        };
        case(&mut store);
        sent.extend(expression_projections(&db));
    }

    for (name, case) in common::scripts::CASES {
        eprintln!("script case {name}");
        let Some((db, mut store)) = profiled_store() else {
            return;
        };
        case(&mut store as &mut dyn ScriptStore);
        sent.extend(expression_projections(&db));
    }

    for (name, case) in common::hooks::CASES {
        eprintln!("hook case {name}");
        let Some((db, mut store)) = profiled_store() else {
            return;
        };
        case(&mut store);
        sent.extend(expression_projections(&db));
    }

    for (name, case) in common::routines::CASES {
        eprintln!("routine case {name}");
        let Some((db, mut store)) = profiled_store() else {
            return;
        };
        case(&mut store as &mut dyn TeamDocStore);
        sent.extend(expression_projections(&db));
    }

    assert_eq!(sent, Vec::<Document>::new());
}
