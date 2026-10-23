//! Starting with a download, and switching to a new model while the old one keeps answering.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use super::support::{Fixture, Notes, World, fake_manifest, ids, target};
use crate::embed::protocol::{Reply, Request};
use crate::embed::{Event, Step, locked, model};

const OLD: &str = "fake@1";
const NEW: &str = "fake@2";

/// The notes of a first run, with their ids and the text that finds each one best.
struct First {
    dir: TempDir,
    notes: Notes,
    fixture: Fixture,
    texts: Vec<(i64, String)>,
}

/// A first run of the old model: it downloads, loads and indexes four notes.
fn first_run() -> First {
    let dir = TempDir::new().expect("temp dir");
    let stores = dir.path().join("stores");
    std::fs::create_dir_all(&stores).expect("stores");
    let mut notes = Notes::new(&stores, "main");

    let mut texts = Vec::new();
    for (title, body) in [
        ("Tokens", "the login token expires after the laptop sleeps"),
        ("Cargo", "run cargo clean before committing the target folder"),
        ("Pool", "the connection pool fills up under heavy load"),
        ("Vite", "the dev server owns port 1420 for the user"),
    ] {
        texts.push((notes.add(title, body), format!("{title}\n{body}")));
    }

    let fixture = Fixture::serving(&fake_manifest(OLD));
    let world = World::open(dir, fake_manifest(OLD), Some(fixture.url()), vec![notes.source()]);

    world.service.prepare().expect("first start");
    world.service.backfill_pass(true);
    assert_eq!(world.service.vectors(OLD), 4);

    let World { dir, .. } = world;
    First {
        dir,
        notes,
        fixture,
        texts,
    }
}

impl First {
    /// The service of a restart that ships the new model.
    fn restart(self) -> (World, Notes, Fixture, Vec<(i64, String)>) {
        let root = self.dir.path().join("data");
        let world = World::reopen(
            self.dir,
            root,
            fake_manifest(NEW),
            Some(self.fixture.url()),
            vec![self.notes.source()],
        );

        (world, self.notes, self.fixture, self.texts)
    }
}

/// What a search saw at one step of an update.
#[derive(Clone)]
struct Observed {
    step: Step,
    answer: Reply,
    old_rows: usize,
    new_rows: usize,
}

fn reply(world: &World, text: &str, key: &str) -> Reply {
    world.ask(text, &[target(key, &[None])])
}

#[test]
fn a_fresh_start_downloads_loads_and_reports_ready() {
    let dir = TempDir::new().expect("temp dir");
    let manifest = fake_manifest(OLD);
    let fixture = Fixture::serving(&manifest);
    let world = World::open(dir, manifest, Some(fixture.url()), Vec::new());

    world.service.prepare().expect("prepare");

    let names = world.events.names();
    let position = |name: &str| names.iter().position(|seen| seen == name).unwrap_or(usize::MAX);
    assert!(position("download") < position("verifying"), "{names:?}");
    assert!(position("verifying") < position("loading"), "{names:?}");
    assert!(position("loading") < position("ready"), "{names:?}");
    assert_eq!(world.service.active_model().as_deref(), Some(OLD));
}

#[test]
fn a_failed_first_download_leaves_nothing_ready() {
    let dir = TempDir::new().expect("temp dir");
    let manifest = fake_manifest(OLD);
    let fixture = Fixture::serving(&manifest);
    fixture.fail.store(true, Ordering::SeqCst);
    let world = World::open(dir, manifest, Some(fixture.url()), Vec::new());

    assert!(world.service.prepare().is_err());
    assert_eq!(world.service.active_model(), None);
    assert_eq!(
        world.ask("anything", &[]).error.as_deref(),
        Some("the model is not ready")
    );

    fixture.fail.store(false, Ordering::SeqCst);
    world.service.prepare().expect("the next try works");
    assert_eq!(world.service.active_model().as_deref(), Some(OLD));
}

#[test]
fn searches_use_only_the_old_model_until_the_switch_and_never_stop() {
    let (world, notes, _fixture, texts) = first_run().restart();
    world.service.prepare().expect("restart");

    let pong = world.service.handle(Request::Ping);
    assert_eq!(pong["model"], OLD);
    assert_eq!(pong["pending"], NEW);

    // A query that matches several notes partly, so every cosine tells which vectors were used.
    let query = "the login token and the connection pool under load";
    let before = reply(&world, query, &notes.key);
    assert_eq!(before.model.as_deref(), Some(OLD));
    assert_eq!(before.hits.len(), 4);

    let seen: Arc<Mutex<Vec<Observed>>> = Arc::default();
    let log = Arc::clone(&seen);
    let service = Arc::downgrade(&world.service);
    let key = notes.key.clone();

    world.service.set_checkpoint(move |step| {
        let Some(service) = service.upgrade() else {
            return false;
        };

        let request = Request::Search {
            text: query.to_owned(),
            targets: vec![target(&key, &[None])],
            limit: 20,
            floor: 0.0,
        };
        let answer: Reply = serde_json::from_value(service.handle(request)).expect("reply");
        locked(&log).push(Observed {
            step,
            answer,
            old_rows: service.vectors(OLD),
            new_rows: service.vectors(NEW),
        });
        true
    });

    assert_eq!(world.service.update(), Ok(true));
    let seen = locked(&seen).clone();

    assert!(
        seen.iter().any(|seen| seen.step == Step::Reembed),
        "the steps were observed"
    );

    for Observed {
        step,
        answer,
        old_rows,
        new_rows,
    } in &seen
    {
        assert!(answer.ok, "no gap: a search answered at {step:?}");

        match step {
            Step::Downloaded | Step::Loaded | Step::Reembed | Step::Reembedded => {
                assert_eq!(answer.model.as_deref(), Some(OLD), "{step:?}");
                assert_eq!(
                    answer.hits, before.hits,
                    "only the old model's rows are compared at {step:?}"
                );
                assert_eq!(*old_rows, 4, "{step:?}");
                assert!(*new_rows <= 4);
            }
            Step::Switched | Step::RowsDeleted | Step::FilesDeleted => {
                assert_eq!(answer.model.as_deref(), Some(NEW), "{step:?}");
                assert_eq!(answer.hits.len(), 4, "{step:?}");
                assert_ne!(
                    answer.hits, before.hits,
                    "the new model's rows are compared at {step:?}"
                );
            }
        }
    }

    let reembedded = seen
        .iter()
        .find(|seen| seen.step == Step::Reembedded)
        .expect("reembedded");
    assert_eq!(
        reembedded.new_rows, 4,
        "every note has a vector of the new model before the switch"
    );

    let after = reply(&world, &texts[0].1, &notes.key);
    assert_eq!(after.model.as_deref(), Some(NEW));
    assert_eq!(ids(&after)[0], texts[0].0);

    assert_eq!(world.service.models_with_vectors(), vec![NEW.to_owned()]);
    assert!(!model::model_dir(&world.root, OLD).exists());
    assert!(model::model_dir(&world.root, NEW).exists());
    assert_eq!(world.service.pending_model(), None);

    let names = world.events.names();
    let position = |name: &str| names.iter().position(|seen| seen == name).unwrap_or(usize::MAX);
    assert!(position("model-update") < position("reembed"), "{names:?}");
    assert!(position("reembed") < position("switched"), "{names:?}");
    assert!(position("switched") < position("cleanup-done"), "{names:?}");
    assert!(world.events.all().contains(&Event::ModelUpdate {
        from: OLD.to_owned(),
        to: NEW.to_owned()
    }));
}

#[test]
fn a_killed_update_leaves_one_consistent_model_at_every_step() {
    let steps = [
        (Step::Downloaded, 1),
        (Step::Loaded, 1),
        (Step::Reembed, 1),
        (Step::Reembed, 3),
        (Step::Reembedded, 1),
        (Step::Switched, 1),
        (Step::RowsDeleted, 1),
        (Step::FilesDeleted, 1),
    ];

    for (killed_at, occurrence) in steps {
        let (world, notes, fixture, texts) = first_run().restart();
        world.service.prepare().expect("restart");

        let seen = Arc::new(AtomicUsize::new(0));
        world
            .service
            .set_checkpoint(move |step| step != killed_at || seen.fetch_add(1, Ordering::SeqCst) + 1 < occurrence);

        let outcome = world.service.update();
        assert_eq!(
            outcome,
            Err("stopped".to_owned()),
            "killed at {killed_at:?} #{occurrence}"
        );

        // The process is gone; a new one starts on what it left.
        let World { dir, root, .. } = world;
        let again = World::reopen(dir, root, fake_manifest(NEW), Some(fixture.url()), vec![notes.source()]);
        again.service.prepare().expect("restart after the kill");

        let switched = matches!(killed_at, Step::Switched | Step::RowsDeleted | Step::FilesDeleted);
        let serving = if switched { NEW } else { OLD };
        let context = format!("killed at {killed_at:?} #{occurrence}");

        let answer = reply(&again, &texts[1].1, &notes.key);
        assert!(answer.ok, "{context}");
        assert_eq!(answer.model.as_deref(), Some(serving), "{context}");
        assert_eq!(ids(&answer)[0], texts[1].0, "{context}");
        assert!(
            answer.hits[0].cosine > 0.99,
            "compared against its own model's vector: {context}"
        );
        assert_eq!(answer.hits.len(), 4, "no duplicates, no gaps: {context}");

        if switched {
            assert_eq!(again.service.models_with_vectors(), vec![NEW.to_owned()], "{context}");
            assert!(!model::model_dir(&again.root, OLD).exists(), "{context}");
        }

        // The update then finishes from whatever was written.
        let finished = again.service.update();
        assert_eq!(finished, Ok(!switched), "{context}");

        let answer = reply(&again, &texts[2].1, &notes.key);
        assert_eq!(answer.model.as_deref(), Some(NEW), "{context}");
        assert_eq!(ids(&answer)[0], texts[2].0, "{context}");
        assert_eq!(again.service.models_with_vectors(), vec![NEW.to_owned()], "{context}");
        assert_eq!(again.service.vectors(NEW), 4, "{context}");
        assert!(!model::model_dir(&again.root, OLD).exists(), "{context}");
        assert!(
            model::is_present(&model::model_dir(&again.root, NEW), &fake_manifest(NEW)),
            "{context}"
        );
    }
}

#[test]
fn a_failed_download_of_the_new_model_keeps_the_old_one_and_is_tried_again() {
    let (world, notes, fixture, texts) = first_run().restart();
    world.service.prepare().expect("restart");

    fixture.fail.store(true, Ordering::SeqCst);
    assert!(world.service.update().is_err());

    let answer = reply(&world, &texts[0].1, &notes.key);
    assert!(answer.ok);
    assert_eq!(answer.model.as_deref(), Some(OLD));
    assert_eq!(world.service.pending_model().as_deref(), Some(NEW));
    assert!(model::model_dir(&world.root, OLD).exists());

    fixture.fail.store(false, Ordering::SeqCst);
    assert_eq!(world.service.update(), Ok(true));
    assert_eq!(reply(&world, &texts[0].1, &notes.key).model.as_deref(), Some(NEW));
}

#[test]
fn notes_saved_during_the_update_are_in_the_new_model_after_the_switch() {
    let (world, notes, _fixture, _texts) = first_run().restart();
    world.service.prepare().expect("restart");

    let key = notes.key.clone();
    let added = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&added);
    let calls = AtomicUsize::new(0);
    let writer = Mutex::new(notes);

    world.service.set_checkpoint(move |step| {
        if step == Step::Reembed && calls.fetch_add(1, Ordering::SeqCst) == 1 {
            *locked(&slot) = Some(locked(&writer).add("Late", "saved while the update ran about terraform"));
        }

        true
    });

    assert_eq!(world.service.update(), Ok(true));
    world.service.backfill_pass(false);

    let late = locked(&added).expect("a note was saved during the update");
    let found = reply(&world, "saved while the update ran about terraform", &key);
    assert_eq!(found.model.as_deref(), Some(NEW));
    assert_eq!(ids(&found)[0], late);
}

#[test]
fn an_old_model_whose_files_are_gone_gives_way_to_the_new_one_at_start() {
    let first = first_run();
    std::fs::remove_dir_all(model::model_dir(&first.dir.path().join("data"), OLD)).expect("remove the old files");

    let (world, notes, _fixture, texts) = first.restart();
    world.service.prepare().expect("restart");

    assert_eq!(world.service.active_model().as_deref(), Some(NEW));
    assert!(
        world.service.models_with_vectors().is_empty(),
        "the old vectors cannot be told apart from a query"
    );

    world.service.backfill_pass(true);
    let answer = reply(&world, &texts[3].1, &notes.key);
    assert_eq!(answer.model.as_deref(), Some(NEW));
    assert_eq!(ids(&answer)[0], texts[3].0);
}
