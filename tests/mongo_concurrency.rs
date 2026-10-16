//! Several servers on one MongoDB database (migration at once, concurrent writes to one note), a
//! database written by a newer server, and the connection rules. The server tests skip unless
//! `HARNESS_MEMORY_TEST_MONGO_URL` names a server (see `mongo_support`).

mod common;
mod mongo_support;

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use common::{both, draft, global_only, query, repo};
use harness_memory::store::mongo::schema::SCHEMA_VERSION;
use harness_memory::store::{MemoryStore, MongoStore, NoteKind, NotePatch, StoreError, Target};
use mongo_support::TestDb;
use mongodb::bson::{Document, doc};
use pretty_assertions::assert_eq;

const REPO: &str = "github.com/acme/widget";

/// Runs `work(index, store)` on `count` threads, each with its own store on `db`, started together.
fn together<T: Send + 'static>(
    db: &TestDb,
    count: usize,
    work: impl Fn(usize, &mut MongoStore) -> T + Send + Sync + 'static,
) -> Vec<T> {
    let barrier = Arc::new(Barrier::new(count));
    let work = Arc::new(work);

    let threads: Vec<_> = (0..count)
        .map(|index| {
            let url = db.url.clone();
            let barrier = Arc::clone(&barrier);
            let work = Arc::clone(&work);

            thread::spawn(move || {
                let mut store = MongoStore::connect(&url).expect("connect");
                barrier.wait();
                work(index, &mut store)
            })
        })
        .collect();

    threads
        .into_iter()
        .map(|thread| thread.join().expect("thread"))
        .collect()
}

#[test]
fn mongo_servers_starting_at_once_migrate_once() {
    let Some(db) = TestDb::new() else { return };
    let barrier = Arc::new(Barrier::new(6));

    let starts: Vec<_> = (0..6)
        .map(|_| {
            let url = db.url.clone();
            let barrier = Arc::clone(&barrier);

            thread::spawn(move || {
                barrier.wait();
                MongoStore::connect(&url).map(|store| store.is_newer())
            })
        })
        .collect();

    for start in starts {
        let newer = start.join().expect("thread").expect("connect");
        assert!(!newer);
    }

    assert_eq!(db.schema_version(), SCHEMA_VERSION);
    assert_eq!(
        db.count("meta", doc! { "_id": "migration_lock" }),
        0,
        "the lock is released"
    );

    let indexes = db
        .raw()
        .collection::<Document>("notes")
        .list_index_names()
        .run()
        .expect("indexes");
    for name in ["notes_title", "notes_text", "notes_terms"] {
        assert!(indexes.iter().any(|index| index == name), "{name}: {indexes:?}");
    }
}

#[test]
fn mongo_a_migration_lock_with_an_invalid_expiry_is_taken_over() {
    let invalid = [
        mongodb::bson::Bson::String("never".to_owned()),
        mongodb::bson::Bson::Null,
        mongodb::bson::Bson::Int64(i64::MAX),
    ];

    for expires_at in invalid {
        let Some(db) = TestDb::new() else { return };
        db.raw()
            .collection::<Document>("meta")
            .insert_one(doc! { "_id": "migration_lock", "owner": "gone", "expires_at": expires_at.clone() })
            .run()
            .expect("plant the lock");

        let started = Instant::now();
        let store = MongoStore::connect(&db.url).unwrap_or_else(|error| panic!("{expires_at:?}: {error}"));

        assert!(!store.is_newer());
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{expires_at:?}: {:?}",
            started.elapsed()
        );
        assert_eq!(db.schema_version(), SCHEMA_VERSION);
        assert_eq!(db.count("meta", doc! { "_id": "migration_lock" }), 0, "{expires_at:?}");
    }
}

#[test]
fn mongo_two_stores_write_without_failures() {
    let Some(db) = TestDb::new() else { return };
    const PER_WRITER: usize = 40;

    together(&db, 2, |writer, store| {
        for index in 0..PER_WRITER {
            let title = format!("writer {writer} note {index}");
            store
                .remember(&repo(REPO), &draft(&title, "shared body text", NoteKind::Note))
                .expect("remember");
            store.search(&query("shared", both(REPO))).expect("search");
        }
    });

    assert_eq!(
        db.count("notes", doc! {}),
        u64::try_from(2 * PER_WRITER).expect("small")
    );
}

#[test]
fn mongo_concurrent_remembers_of_one_title_lose_no_revision() {
    let Some(db) = TestDb::new() else { return };
    const WRITERS: usize = 4;
    const PER_WRITER: usize = 10;

    let mut note = draft("Shared counter", "start", NoteKind::Note);
    note.entities = vec!["Counter".to_owned()];

    together(&db, WRITERS, move |writer, store| {
        for index in 0..PER_WRITER {
            let mut note = note.clone();
            note.body = format!("writer {writer} wrote {index}");
            store.remember(&Target::Global, &note).expect("remember");
        }
    });

    assert_eq!(db.count("notes", doc! {}), 1, "one note per title");
    assert_eq!(db.count("entities", doc! {}), 1, "one entity per name");

    let mut store = db.connect();
    let hits = store.search(&query("shared counter", global_only())).expect("search");
    let stored = store.get(&global_only(), &[hits[0].id]).expect("get").remove(0);
    assert_eq!(stored.revision, i64::try_from(WRITERS * PER_WRITER).expect("small"));
    assert_eq!(stored.entities, vec!["Counter"]);
}

#[test]
fn mongo_concurrent_appends_to_one_note_are_all_kept() {
    let Some(db) = TestDb::new() else { return };
    const WRITERS: usize = 4;
    const PER_WRITER: usize = 10;

    let mut store = db.connect();
    let saved = store
        .remember(&repo(REPO), &draft("Changelog", "entries:", NoteKind::Note))
        .expect("create");
    let id = saved.id;

    together(&db, WRITERS, move |writer, store| {
        for index in 0..PER_WRITER {
            let patch = NotePatch {
                append: Some(format!("w{writer}e{index}")),
                ..NotePatch::default()
            };
            store.update(&both(REPO), id, &patch).expect("append");
        }
    });

    let stored = store.get(&both(REPO), &[id]).expect("get").remove(0);
    assert_eq!(stored.revision, 1 + i64::try_from(WRITERS * PER_WRITER).expect("small"));

    for writer in 0..WRITERS {
        for index in 0..PER_WRITER {
            let entry = format!("w{writer}e{index}");
            assert!(
                stored.body.lines().any(|line| line == entry),
                "{entry} lost: {}",
                stored.body
            );
        }
    }
}

#[test]
fn mongo_an_oversized_entity_list_fails_at_once() {
    let Some((_db, mut store)) = mongo_support::store() else {
        return;
    };
    let mut note = draft("Flood", "body", NoteKind::Note);
    note.entities = (0..100_000).map(|index| format!("entity {index}")).collect();

    let started = Instant::now();
    let result = store.remember(&repo(REPO), &note);

    assert!(matches!(result, Err(StoreError::Invalid(_))), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
}

#[test]
fn mongo_a_newer_database_is_refused_and_left_untouched() {
    let Some(db) = TestDb::new() else { return };
    let mut running = db.connect();
    running
        .remember(&repo(REPO), &draft("Before", "written by this version", NoteKind::Note))
        .expect("write before the upgrade");

    db.raw()
        .collection::<Document>("meta")
        .update_one(
            doc! { "_id": "schema" },
            doc! { "$set": { "version": SCHEMA_VERSION + 1 } },
        )
        .run()
        .expect("bump");

    // A server that connected before the upgrade stops writing too.
    let error = running
        .remember(&repo(REPO), &draft("After", "Body", NoteKind::Note))
        .expect_err("refused");
    assert!(matches!(error, StoreError::NewerSchema), "{error:?}");
    assert!(matches!(
        running.search(&query("", both(REPO))),
        Err(StoreError::NewerSchema)
    ));

    let mut store = db.connect();
    assert!(store.is_newer());
    assert!(matches!(
        store.remember(&repo(REPO), &draft("Title", "Body", NoteKind::Note)),
        Err(StoreError::NewerSchema)
    ));
    assert!(matches!(store.get(&both(REPO), &[1]), Err(StoreError::NewerSchema)));

    assert_eq!(db.schema_version(), SCHEMA_VERSION + 1);
    assert_eq!(db.count("notes", doc! {}), 1);
}

// Connection rules (no server needed).

#[test]
fn mongo_remote_servers_must_use_tls() {
    for url in [
        "mongodb://alice:s3cret-pass@db.example.com:27017/team?tls=false",
        "mongodb://alice:s3cret-pass@db.example.com/team?ssl=false",
        "mongodb+srv://alice:s3cret-pass@cluster.example.com/team?tls=false",
        "mongodb://alice:s3cret-pass@db.example.com/team?tls=true&tlsAllowInvalidCertificates=true",
        "mongodb://alice:s3cret-pass@db.example.com/team?tlsInsecure=true",
    ] {
        let error = MongoStore::connect(url).expect_err("refused");
        assert!(matches!(error, StoreError::Invalid(_)), "{url}: {error:?}");
        assert!(!error.to_string().contains("s3cret"), "{error}");
    }
}

#[test]
fn mongo_loopback_servers_skip_tls_only_on_a_direct_connection() {
    // The replica set may list members on other machines, and the driver would connect to them.
    for url in [
        "mongodb://localhost/?tls=false&replicaSet=rs0",
        "mongodb://127.0.0.1:27017/?tls=false",
    ] {
        let error = MongoStore::connect(url).expect_err("refused");
        assert!(matches!(error, StoreError::Invalid(_)), "{url}: {error:?}");
    }

    // Accepted without TLS: only reaching the server fails (nothing listens on port 1).
    let url = "mongodb://127.0.0.1:1/?directConnection=true&tls=false&serverSelectionTimeoutMS=200";
    let error = MongoStore::connect(url).expect_err("unreachable");
    assert!(matches!(error, StoreError::Backend(_)), "{error:?}");
}

#[test]
fn mongo_other_schemes_are_refused() {
    for url in [
        "postgres://localhost/memory",
        "http://localhost:27017",
        "localhost:27017",
    ] {
        let error = MongoStore::connect(url).expect_err("refused");
        assert!(matches!(error, StoreError::Invalid(_)), "{url}: {error:?}");
    }
}

#[test]
fn mongo_connection_errors_do_not_show_the_password() {
    // Nothing listens on port 1; the driver gives up after the server selection timeout.
    let url = "mongodb://alice:s3cret-pass@localhost:1/team?serverSelectionTimeoutMS=200&connectTimeoutMS=200";
    let error = MongoStore::connect(url).expect_err("unreachable");
    assert!(matches!(error, StoreError::Backend(_)), "{error:?}");
    assert!(!error.to_string().contains("s3cret"), "{error}");
    assert!(!format!("{error:?}").contains("s3cret"), "{error:?}");

    let invalid = MongoStore::connect("mongodb://alice:s3cret-pass@localhost:1/team?w=bogus&journal=maybe")
        .expect_err("invalid option");
    assert!(!invalid.to_string().contains("s3cret"), "{invalid}");
}
