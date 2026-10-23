//! Backfill, search and the request handler, over real SQLite stores and the fake engine.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tempfile::TempDir;

use super::support::{Notes, REPO, World, fake_manifest, ids, install, target};
use crate::embed::protocol::{NoteInput, Reply, Request};
use crate::embed::{Event, Source, locked};
use crate::store::{Lazy, MemoryStore};

const MODEL: &str = "fake@1";

/// A world with the model in place and loaded, over the given databases.
fn world(dir: TempDir, notes: &[&Notes]) -> World {
    let manifest = fake_manifest(MODEL);
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).expect("data folder");
    install(&root, &manifest);

    let world = World::open(dir, manifest, None, notes.iter().map(|notes| notes.source()).collect());
    world.service.prepare().expect("prepare");
    world
}

fn stores_dir(dir: &TempDir) -> std::path::PathBuf {
    let stores = dir.path().join("stores");
    std::fs::create_dir_all(&stores).expect("stores folder");
    stores
}

#[test]
fn an_empty_store_finishes_its_backfill_with_zero() {
    let dir = TempDir::new().expect("temp dir");
    let notes = Notes::new(&stores_dir(&dir), "main");
    let world = world(dir, &[&notes]);

    world.service.backfill_pass(true);

    assert!(world.events.all().contains(&Event::Backfill {
        store: notes.key.clone(),
        done: 0,
        total: 0,
    }));
    assert_eq!(world.service.vectors(MODEL), 0);
    assert!(world.ask("anything", &[target(&notes.key, &[None])]).hits.is_empty());
}

#[test]
fn ready_names_the_model_and_its_dimension() {
    let dir = TempDir::new().expect("temp dir");
    let notes = Notes::new(&stores_dir(&dir), "main");
    let world = world(dir, &[&notes]);

    assert!(world.events.all().contains(&Event::Ready {
        model: MODEL.to_owned(),
        dim: 384,
    }));
    assert_eq!(world.events.names().first().map(String::as_str), Some("loading"));
}

#[test]
fn new_edited_and_deleted_notes_follow_the_store() {
    let dir = TempDir::new().expect("temp dir");
    let mut notes = Notes::new(&stores_dir(&dir), "main");
    let kept = notes.add("Vite port", "the dev server listens on port 1420");
    let edited = notes.add("Token", "the login token expires after the laptop sleeps");
    let removed = notes.add("Cargo", "run cargo clean before committing");
    let world = world(dir, &[&notes]);
    let everything = [target(&notes.key, &[None])];

    world.service.backfill_pass(true);
    assert_eq!(world.service.vectors(MODEL), 3);
    assert_eq!(ids(&world.ask("login token laptop sleeps", &everything))[0], edited);

    // A new note and an edited one are found by an ordinary pass.
    let added = notes.add("Postgres", "the connection pool fills under load");
    notes.edit(edited, "refresh the session cookie before the first request");
    world.service.backfill_pass(false);

    assert_eq!(world.service.vectors(MODEL), 4);
    assert_eq!(ids(&world.ask("connection pool load", &everything))[0], added);
    assert_eq!(ids(&world.ask("session cookie refresh", &everything))[0], edited);

    // A deleted one is dropped by the pass that reads every note.
    notes.delete(removed);
    world.service.backfill_pass(false);
    assert_eq!(
        world.service.vectors(MODEL),
        4,
        "an ordinary pass does not look for deletions"
    );

    world.service.backfill_pass(true);
    assert_eq!(world.service.vectors(MODEL), 3);
    assert!(!ids(&world.ask("cargo clean committing", &everything)).contains(&removed));
    assert!(ids(&world.ask("port 1420 dev server", &everything)).contains(&kept));
}

#[test]
fn a_note_that_did_not_change_is_not_embedded_again() {
    let dir = TempDir::new().expect("temp dir");
    let mut notes = Notes::new(&stores_dir(&dir), "main");
    notes.add("One", "first body text");
    notes.add("Two", "second body text");
    let world = world(dir, &[&notes]);

    world.service.backfill_pass(true);
    let embedded = world.fake.embeds.load(Ordering::SeqCst);
    assert_eq!(embedded, 2);

    world.service.backfill_pass(true);
    world.service.backfill_pass(false);
    assert_eq!(world.fake.embeds.load(Ordering::SeqCst), embedded);
}

#[test]
fn notes_sharing_one_second_are_all_read_across_pages() {
    let dir = TempDir::new().expect("temp dir");
    let mut notes = Notes::new(&stores_dir(&dir), "main");

    // More than a page (100), all saved within a second or two.
    for number in 0..230 {
        notes.add(
            &format!("Note {number}"),
            &format!("body number {number} about topic {}", number % 7),
        );
    }

    let world = world(dir, &[&notes]);
    world.service.backfill_pass(true);

    assert_eq!(world.service.vectors(MODEL), 230);
}

#[test]
fn search_keeps_to_the_stores_and_projects_asked_for() {
    let dir = TempDir::new().expect("temp dir");
    let stores = stores_dir(&dir);
    let mut first = Notes::new(&stores, "first");
    let mut second = Notes::new(&stores, "second");

    let global = first.add("Global", "shared knowledge about deploys");
    let repo = first.add_to_repo("Repo", "repository knowledge about deploys");
    let other = second.add("Other", "other store knowledge about deploys");

    let world = world(dir, &[&first, &second]);
    world.service.backfill_pass(true);

    let both = ids(&world.ask("knowledge about deploys", &[target(&first.key, &[None, Some(REPO)])]));
    assert_eq!(both, vec![global, repo]);
    assert_ne!(other, 0);

    let only_global = ids(&world.ask("knowledge about deploys", &[target(&first.key, &[None])]));
    assert_eq!(only_global, vec![global]);

    let only_repo = ids(&world.ask("knowledge about deploys", &[target(&first.key, &[Some(REPO)])]));
    assert_eq!(only_repo, vec![repo]);

    let nothing = world.ask(
        "knowledge about deploys",
        &[target(&first.key, &[Some("git:example.com/other/repo")])],
    );
    assert!(nothing.hits.is_empty());

    let second_only = world.ask("knowledge about deploys", &[target(&second.key, &[None])]);
    assert_eq!(
        second_only
            .hits
            .iter()
            .map(|hit| (hit.store.as_str(), hit.id))
            .collect::<Vec<_>>(),
        vec![(second.key.as_str(), other)]
    );

    let unknown = world.ask("knowledge about deploys", &[target("local:nowhere", &[None])]);
    assert!(unknown.hits.is_empty());
}

#[test]
fn search_orders_by_cosine_and_applies_the_limit_and_floor() {
    let dir = TempDir::new().expect("temp dir");
    let mut notes = Notes::new(&stores_dir(&dir), "main");
    let exact = notes.add("Exact", "alpha beta gamma");
    let partial = notes.add("Partial", "alpha beta delta epsilon");
    notes.add("Unrelated", "zeta eta theta iota");
    let world = world(dir, &[&notes]);
    world.service.backfill_pass(true);
    let everything = [target(&notes.key, &[None])];

    let reply = world.ask("Exact alpha beta gamma", &everything);
    assert_eq!(reply.model.as_deref(), Some(MODEL));
    assert_eq!(ids(&reply)[..2], [exact, partial]);
    assert!(reply.hits[0].cosine > 0.99, "{:?}", reply.hits);
    assert!(reply.hits.windows(2).all(|pair| pair[0].cosine >= pair[1].cosine));

    let limited = world.service.handle(Request::Search {
        text: "Exact alpha beta gamma".to_owned(),
        targets: everything.to_vec(),
        limit: 1,
        floor: 0.0,
    });
    let limited: Reply = serde_json::from_value(limited).expect("reply");
    assert_eq!(ids(&limited).len(), 1);

    let floored = world.service.handle(Request::Search {
        text: "Exact alpha beta gamma".to_owned(),
        targets: everything.to_vec(),
        limit: 20,
        floor: 0.9,
    });
    let floored: Reply = serde_json::from_value(floored).expect("reply");
    assert_eq!(ids(&floored), vec![exact]);
}

#[test]
fn upsert_and_forget_change_what_search_finds_at_once() {
    let dir = TempDir::new().expect("temp dir");
    let notes = Notes::new(&stores_dir(&dir), "main");
    let world = world(dir, &[&notes]);
    let everything = [target(&notes.key, &[None])];

    let note = NoteInput {
        id: 900,
        project: None,
        title: "Fresh".to_owned(),
        body: "saved a moment ago about kubernetes".to_owned(),
        tags: vec!["ops".to_owned()],
    };

    let reply = world.service.handle(Request::Upsert {
        store: notes.key.clone(),
        notes: vec![note],
    });
    assert_eq!(reply["ok"], true);
    assert_eq!(ids(&world.ask("kubernetes saved moment", &everything)), vec![900]);

    let reply = world.service.handle(Request::Forget {
        store: notes.key.clone(),
        ids: vec![900],
    });
    assert_eq!(reply["ok"], true);
    assert!(world.ask("kubernetes saved moment", &everything).hits.is_empty());
}

#[test]
fn the_vectors_survive_a_restart() {
    let dir = TempDir::new().expect("temp dir");
    let mut notes = Notes::new(&stores_dir(&dir), "main");
    let id = notes.add("Kept", "persistent vectors across restarts");
    let world = world(dir, &[&notes]);
    world.service.backfill_pass(true);

    let (dir, root) = (world.dir, world.root);
    let again = World::reopen(dir, root, fake_manifest(MODEL), None, vec![notes.source()]);
    again.service.prepare().expect("prepare");

    assert_eq!(
        ids(&again.ask("persistent vectors restarts", &[target(&notes.key, &[None])])),
        vec![id]
    );

    let queries = again.fake.embeds.load(Ordering::SeqCst);
    again.service.backfill_pass(true);
    assert_eq!(
        again.fake.embeds.load(Ordering::SeqCst),
        queries,
        "no note is embedded again"
    );
}

#[test]
fn a_store_that_cannot_be_read_reports_one_error_and_the_others_go_on() {
    let dir = TempDir::new().expect("temp dir");
    let mut notes = Notes::new(&stores_dir(&dir), "main");
    let id = notes.add("Fine", "still indexed");

    let broken: Lazy<dyn MemoryStore> = Lazy::new(|| Err("cannot connect to the team database".to_owned()));
    let manifest = fake_manifest(MODEL);
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).expect("data folder");
    install(&root, &manifest);

    let sources = vec![Source::new("postgres:abc", Box::new(broken)), notes.source()];
    let world = World::open(dir, manifest, None, sources);
    world.service.prepare().expect("prepare");

    world.service.backfill_pass(true);
    world.service.backfill_pass(false);

    assert_eq!(world.events.errors().len(), 1, "{:?}", world.events.errors());
    assert!(world.events.errors()[0].starts_with("postgres:abc: memory database error: cannot connect"));
    assert_eq!(
        ids(&world.ask("still indexed", &[target(&notes.key, &[None])])),
        vec![id]
    );
}

#[test]
fn a_failure_of_a_team_store_is_reported_without_its_url_secrets() {
    let dir = TempDir::new().expect("temp dir");
    let url = "postgres://writer:hunter2pass@db.example.com/memory";
    let broken: Lazy<dyn MemoryStore> =
        Lazy::new(move || Err(format!("cannot connect to {url} with the password hunter2pass")));

    let manifest = fake_manifest(MODEL);
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).expect("data folder");
    install(&root, &manifest);

    let source = Source::new("postgres:abc", Box::new(broken)).with_secrets(crate::backend::Secrets::of(url, None));
    let world = World::open(dir, manifest, None, vec![source]);
    world.service.prepare().expect("prepare");
    world.service.backfill_pass(true);

    let errors = world.events.errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("cannot connect to"), "{errors:?}");
    assert!(
        !errors[0].contains("hunter2pass") && !errors[0].contains("db.example.com"),
        "{errors:?}"
    );
}

#[test]
fn a_local_database_that_does_not_exist_yet_is_skipped_without_creating_it() {
    let dir = TempDir::new().expect("temp dir");
    let stores = stores_dir(&dir);
    let missing = stores.join("later.sqlite3");
    let opened = missing.clone();

    let lazy: Lazy<dyn MemoryStore> = Lazy::new(move || {
        crate::store::SqliteStore::open(&opened)
            .map(|store| Box::new(store) as Box<dyn MemoryStore>)
            .map_err(|error| error.to_string())
    });
    let source = Source::new("local:later", Box::new(lazy)).when_file_exists(missing.clone());

    let manifest = fake_manifest(MODEL);
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).expect("data folder");
    install(&root, &manifest);

    let world = World::open(dir, manifest, None, vec![source]);
    world.service.prepare().expect("prepare");
    world.service.backfill_pass(true);

    assert!(!missing.exists());
    assert!(world.events.errors().is_empty());
}

#[test]
fn an_idle_model_is_dropped_and_loaded_again_by_the_next_search() {
    let dir = TempDir::new().expect("temp dir");
    let mut notes = Notes::new(&stores_dir(&dir), "main");
    let id = notes.add("Idle", "wake me when searched");

    let manifest = fake_manifest(MODEL);
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).expect("data folder");
    install(&root, &manifest);

    let world = World::reopen_with(dir, root, manifest, None, vec![notes.source()], |options| {
        options.idle = Duration::from_millis(40);
    });
    let (service, events, fake) = (
        Arc::clone(&world.service),
        Arc::clone(&world.events),
        Arc::clone(&world.fake),
    );

    service.prepare().expect("prepare");
    service.backfill_pass(true);
    assert_eq!(fake.loads.load(Ordering::SeqCst), 1);

    let watcher = Arc::clone(&service);
    let thread = std::thread::spawn(move || watcher.idle_loop());

    let started = std::time::Instant::now();
    while !events.all().contains(&Event::Idle) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the model was never dropped"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(!locked(&service.active).as_ref().expect("active").resident.is_loaded());

    let reply = service.handle(Request::Search {
        text: "wake me when searched".to_owned(),
        targets: vec![target(&notes.key, &[None])],
        limit: 5,
        floor: 0.0,
    });
    let reply: Reply = serde_json::from_value(reply).expect("reply");

    assert_eq!(
        ids(&reply),
        vec![id],
        "the vectors stayed loaded and the model came back"
    );
    assert_eq!(fake.loads.load(Ordering::SeqCst), 2);

    service.stop().stop();
    thread.join().expect("idle loop");
}
