//! The additive schema extensions of the MongoDB (and, with `HARNESS_MEMORY_TEST_MONGO_FLAVOR=
//! documentdb`, the Amazon DocumentDB) store, see `mongo_support`: the `terms_en` index and the
//! English fields are added on every open without a new schema version, and an older server keeps
//! reading and writing the upgraded database with its writes picked up by the backfill.

mod common;
mod mongo_support;

use common::listing::listing;
use common::{both, draft, query, repo};
use harness_memory::store::mongo::schema::SCHEMA_VERSION;
use harness_memory::store::{LangFilter, MemoryStore, NoteKind, NoteListQuery, NotePatch};
use mongo_support::{TestDb, store};
use mongodb::bson::{Bson, Document, doc};
use pretty_assertions::assert_eq;

const REPO: &str = "github.com/acme/widget";

const ENGLISH: &str = "The sync job reads every changed note from the team database, then writes the new revision \
                       to the local cache so the next search finds it without another round trip.";

const POLISH: &str = "Zadanie synchronizacji czyta każdą zmienioną notatkę z bazy zespołu, a potem zapisuje nową \
                      wersję w lokalnej pamięci podręcznej, żeby następne wyszukiwanie znalazło ją od razu.";

fn notes(db: &TestDb) -> mongodb::sync::Collection<Document> {
    db.raw().collection("notes")
}

fn note(db: &TestDb, id: i64) -> Document {
    notes(db)
        .find_one(doc! { "_id": id })
        .run()
        .expect("read")
        .expect("the note")
}

fn strings(document: &Document, key: &str) -> Vec<String> {
    document
        .get_array(key)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// A note as an older server inserts it: no `terms_en`, `lang` or `en_rev`.
fn old_insert(db: &TestDb, id: i64, title: &str, body: &str) {
    notes(db)
        .insert_one(doc! {
            "_id": id,
            "project": Bson::Null,
            "title_norm": title.to_lowercase(),
            "created_at": 2_i64,
            "revision": 1_i64,
            "hits": 0_i64,
            "entities": Vec::<String>::new(),
            "title": title,
            "body": body,
            "kind": "note",
            "tags": Vec::<String>::new(),
            "terms": body.to_lowercase().split_whitespace().map(str::to_owned).collect::<Vec<_>>(),
            "updated_at": 2_i64,
            "source": Bson::Null,
            "writer_root": Bson::Null,
            "via_override": false,
        })
        .run()
        .expect("an older server's insert");
}

/// Takes the extensions out again: the database as the previous server left it.
fn without_extensions(db: &TestDb) {
    notes(db).drop_index("notes_terms_en").run().expect("drop the index");
    notes(db)
        .update_many(doc! {}, doc! { "$unset": { "terms_en": "", "lang": "", "en_rev": "" } })
        .run()
        .expect("unset the fields");
}

#[test]
fn an_empty_database_gets_the_terms_index_and_no_new_version() {
    let Some((db, store)) = store() else { return };

    assert!(store.has_extensions());
    assert_eq!(db.schema_version(), SCHEMA_VERSION);
    assert!(db.index_names("notes").contains(&"notes_terms_en".to_owned()));
    assert!(
        db.index_names("notes").contains(&"notes_terms".to_owned()),
        "the old index stays"
    );
}

#[test]
fn a_current_database_without_them_is_upgraded_in_place() {
    let Some((db, mut store)) = store() else { return };
    let id = store
        .remember(
            &repo(REPO),
            &draft("Build cache", "Caches are stored in target", NoteKind::Location),
        )
        .expect("remember")
        .id;
    drop(store);

    without_extensions(&db);
    assert!(!db.index_names("notes").contains(&"notes_terms_en".to_owned()));

    let mut store = db.connect();
    assert!(store.has_extensions());
    assert_eq!(db.schema_version(), SCHEMA_VERSION, "no schema version change");
    assert!(db.index_names("notes").contains(&"notes_terms_en".to_owned()));

    // The stems and the language of the note that was already there were filled in on open.
    let upgraded = note(&db, id);
    assert_eq!(
        strings(&upgraded, "terms_en"),
        vec!["build", "cach", "are", "store", "in", "target"]
    );
    assert!(
        matches!(upgraded.get_str("lang"), Ok("en" | "unknown")),
        "too short to be sure: {upgraded:?}"
    );
    assert_eq!(upgraded.get_i64("en_rev"), upgraded.get_i64("revision"));

    assert_eq!(store.search(&query("cache", both(REPO))).expect("search").len(), 1);
}

#[test]
fn opening_again_changes_nothing() {
    let Some((db, mut store)) = store() else { return };
    store
        .remember(&repo(REPO), &draft("Lint", ENGLISH, NoteKind::Note))
        .expect("remember");
    drop(store);

    let before = db.index_names("notes");
    let mut store = db.connect();
    drop(db.connect());

    assert_eq!(db.index_names("notes"), before);
    assert_eq!(db.count("notes", doc! {}), 1);
    assert_eq!(store.search(&query("lint", both(REPO))).expect("search").len(), 1);
}

#[test]
fn an_older_servers_writes_are_picked_up_by_the_next_open() {
    let Some((db, mut store)) = store() else { return };

    old_insert(&db, 900, "Release steps", "Tag the commit then publish the changelog");

    // The newer server still reads, searches and writes the database, with no schema error.
    assert_eq!(store.search(&query("changelog", both(REPO))).expect("search").len(), 1);
    store
        .remember(&repo(REPO), &draft("Lint", ENGLISH, NoteKind::Note))
        .expect("remember");
    assert_eq!(db.count("notes", doc! { "terms_en": { "$exists": false } }), 1);
    drop(store);

    // The next open gives the note its stems and language.
    let _store = db.connect();
    let filled = note(&db, 900);
    assert!(
        strings(&filled, "terms_en").contains(&"publish".to_owned()),
        "{filled:?}"
    );
    assert!(matches!(filled.get_str("lang"), Ok("en" | "unknown")), "{filled:?}");
    assert_eq!(db.count("notes", doc! { "terms_en": { "$exists": false } }), 0);
    assert_eq!(db.schema_version(), SCHEMA_VERSION);
}

#[test]
fn an_older_servers_edit_makes_the_stems_stale_and_the_next_open_refreshes_them() {
    let Some((db, mut store)) = store() else { return };
    let id = store
        .remember(&repo(REPO), &draft("Sync job", ENGLISH, NoteKind::Note))
        .expect("remember")
        .id;

    // A new server's edits keep the English fields current.
    let patch = NotePatch {
        title: Some("Synchronizacja".to_owned()),
        body: Some(POLISH.to_owned()),
        ..NotePatch::default()
    };
    store.update(&both(REPO), id, &patch).expect("update");
    let edited = note(&db, id);
    assert_eq!(edited.get_str("lang"), Ok("other"));
    assert_eq!(edited.get_i64("en_rev"), edited.get_i64("revision"));

    // An older server edits the body and raises the revision, nothing else.
    notes(&db)
        .update_one(
            doc! { "_id": id },
            doc! { "$set": { "body": format!("{ENGLISH} Rewritten by an old client") }, "$inc": { "revision": 1_i64 } },
        )
        .run()
        .expect("an older server's edit");
    assert_ne!(note(&db, id).get_i64("en_rev"), note(&db, id).get_i64("revision"));

    drop(store);
    let _store = db.connect();

    let refreshed = note(&db, id);
    assert_eq!(refreshed.get_i64("en_rev"), refreshed.get_i64("revision"));
    assert_eq!(refreshed.get_str("lang"), Ok("en"), "{refreshed:?}");
}

/// An older server's edit leaves the stored `lang` describing the previous text until the next
/// open backfills it: until then the note shows no language, and the filters and counts agree.
#[test]
fn a_note_an_older_server_edited_has_no_language_until_the_backfill() {
    let Some((db, mut store)) = store() else { return };
    let id = store
        .remember(&repo(REPO), &draft("Sync job", ENGLISH, NoteKind::Note))
        .expect("remember")
        .id;
    store
        .remember(&repo(REPO), &draft("Zadanie", POLISH, NoteKind::Note))
        .expect("remember");

    notes(&db)
        .update_one(
            doc! { "_id": id },
            doc! { "$set": { "body": POLISH }, "$inc": { "revision": 1_i64 } },
        )
        .run()
        .expect("an older server's edit");

    let with = |lang| NoteListQuery {
        lang,
        ..listing(both(REPO))
    };
    let titles = |store: &mut dyn MemoryStore, lang| -> (usize, Vec<String>) {
        let page = store.list_notes(&with(lang)).expect("list");
        let mut titles: Vec<String> = page.rows.into_iter().map(|row| row.title).collect();
        titles.sort();
        (page.total, titles)
    };

    let stale = store.list_notes(&with(None)).expect("list");
    let row = stale.rows.iter().find(|row| row.title == "Sync job").expect("the row");
    assert_eq!(row.lang, None, "stale: its language is unknown");

    assert_eq!(titles(&mut store, Some(LangFilter::En)), (0, vec![]));
    assert_eq!(
        titles(&mut store, Some(LangFilter::Other)),
        (1, vec!["Zadanie".to_owned()])
    );
    assert_eq!(
        titles(&mut store, Some(LangFilter::Unknown)),
        (1, vec!["Sync job".to_owned()])
    );

    drop(store);
    let mut store = db.connect();

    assert_eq!(titles(&mut store, Some(LangFilter::Unknown)), (0, vec![]));
    assert_eq!(titles(&mut store, Some(LangFilter::En)), (0, vec![]));
    assert_eq!(
        titles(&mut store, Some(LangFilter::Other)),
        (2, vec!["Sync job".to_owned(), "Zadanie".to_owned()])
    );
}

#[test]
fn the_english_notes_setting_defaults_on_and_is_shared() {
    let Some((db, mut store)) = store() else { return };
    assert!(store.english_notes().expect("default"), "on when nothing is stored");

    store.set_english_notes(false).expect("set off");
    assert!(!store.english_notes().expect("off"));

    let mut other = db.connect();
    assert!(!other.english_notes().expect("another server sees it"));

    other.set_english_notes(true).expect("set on");
    assert!(store.english_notes().expect("on"));
    assert_eq!(db.count("meta", doc! { "_id": "settings", "english_notes": true }), 1);
}
