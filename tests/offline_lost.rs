//! A team database that is reached and lost afterwards: the connection works when the call begins
//! and fails with `Unreachable` during it, as a dropped connection or a stopped server does. With
//! the offline fallback the call is made again from this computer and nothing is lost: a save is
//! kept here and copied back, a change to a team note is queued and applied when the database is
//! back, and the calls after the first do not wait for the dead connection again. Without the
//! fallback the call fails with what to change, and the next call connects again.
//!
//! The team database is a SQLite file behind a wire that can be cut, so these cases need no server.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{draft, global_only, query};
use harness_memory::backend::{NotesPlace, Secrets};
use harness_memory::store::pending_updates::{Queue, Update};
use harness_memory::store::{
    Author, Edge, Forget, Forgotten, HiddenNotes, Hit, Lazy, Link, MemoryStore, Neighborhood, Note, NoteKind,
    NoteListQuery, NotePage, NotePatch, Provenance, Reach, Saved, SearchQuery, SplitStore, SqliteStore, StoreError,
    Target, View,
};
use tempfile::TempDir;

const TEAM: &str = "team:a";

/// What the wire does to the team database behind it.
#[derive(Clone)]
struct Wire {
    /// Every call fails with `Unreachable` while it is set.
    cut: Arc<AtomicBool>,
    /// How many more calls get through before it fails (a cut that comes in the middle of a replay).
    calls_left: Arc<AtomicI64>,
    /// The calls that reached it, answered or not.
    attempts: Arc<AtomicUsize>,
    /// How often the team database was opened.
    opens: Arc<AtomicUsize>,
    /// A teammate's text appended to the note just before the next update of it is written.
    before_update: Arc<Mutex<Option<String>>>,
    /// The next update is written, and its answer is lost on the way (the connection drops).
    lose_reply: Arc<AtomicBool>,
}

impl Wire {
    fn new() -> Self {
        Self {
            cut: Arc::new(AtomicBool::new(false)),
            calls_left: Arc::new(AtomicI64::new(i64::MAX)),
            attempts: Arc::new(AtomicUsize::new(0)),
            opens: Arc::new(AtomicUsize::new(0)),
            before_update: Arc::new(Mutex::new(None)),
            lose_reply: Arc::new(AtomicBool::new(false)),
        }
    }

    fn pass(&self) -> Result<(), StoreError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);

        if self.cut.load(Ordering::SeqCst) || self.calls_left.fetch_sub(1, Ordering::SeqCst) <= 0 {
            return Err(StoreError::Unreachable("the connection was lost".to_owned()));
        }

        Ok(())
    }

    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

/// The team's notes, reached through the wire.
struct TeamNotes {
    inner: SqliteStore,
    wire: Wire,
}

impl MemoryStore for TeamNotes {
    fn remember(&mut self, target: &Target, draft: &harness_memory::store::NoteDraft) -> Result<Saved, StoreError> {
        self.wire.pass()?;
        self.inner.remember(target, draft)
    }

    fn update(&mut self, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError> {
        self.wire.pass()?;

        let teammate = self.wire.before_update.lock().expect("lock").take();

        if let Some(text) = teammate {
            self.inner.update(view, id, &append(&text))?;
        }

        let saved = self.inner.update(view, id, patch)?;

        if self.wire.lose_reply.swap(false, Ordering::SeqCst) {
            return Err(StoreError::Unreachable("the connection was lost".to_owned()));
        }

        Ok(saved)
    }

    fn get(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        self.wire.pass()?;
        self.inner.get(view, ids)
    }

    fn peek(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        self.wire.pass()?;
        self.inner.peek(view, ids)
    }

    fn list_notes(&mut self, query: &NoteListQuery) -> Result<NotePage, StoreError> {
        self.wire.pass()?;
        self.inner.list_notes(query)
    }

    fn search(&mut self, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
        self.wire.pass()?;
        self.inner.search(query)
    }

    fn hits_for(&mut self, query: &SearchQuery, ids: &[i64]) -> Result<Vec<Hit>, StoreError> {
        self.wire.pass()?;
        self.inner.hits_for(query, ids)
    }

    fn similar(&mut self, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
        self.wire.pass()?;
        self.inner.similar(view, id, limit)
    }

    fn link(&mut self, target: &Target, link: &Link) -> Result<Edge, StoreError> {
        self.wire.pass()?;
        self.inner.link(target, link)
    }

    fn neighbors(&mut self, view: &View, entity: &str, depth: u8, limit: usize) -> Result<Neighborhood, StoreError> {
        self.wire.pass()?;
        self.inner.neighbors(view, entity, depth, limit)
    }

    fn forget(&mut self, view: &View, target: &Target, what: &Forget) -> Result<usize, StoreError> {
        self.wire.pass()?;
        self.inner.forget(view, target, what)
    }

    fn forget_notes(&mut self, view: &View, exclude: &[i64]) -> Result<Forgotten, StoreError> {
        self.wire.pass()?;
        self.inner.forget_notes(view, exclude)
    }

    fn find_by_title(&mut self, target: &Target, title: &str) -> Result<Option<Note>, StoreError> {
        self.wire.pass()?;
        self.inner.find_by_title(target, title)
    }

    fn english_notes(&mut self) -> Result<bool, StoreError> {
        self.wire.pass()?;
        self.inner.english_notes()
    }
}

/// A folder with this computer's database, the team's, and the wire between them.
struct Rig {
    dir: TempDir,
    wire: Wire,
    /// What the team database's notes are told when the connection is lost.
    reach: Reach,
}

impl Rig {
    fn new() -> Self {
        Self {
            dir: TempDir::new().expect("temp dir"),
            wire: Wire::new(),
            // Long: a cooldown that ends by itself would make "no second attempt" depend on the clock.
            reach: Reach::with_cooldown(Duration::from_secs(600)),
        }
    }

    fn local(&self) -> PathBuf {
        self.dir.path().join("memory.sqlite3")
    }

    fn team_file(&self) -> PathBuf {
        self.dir.path().join("team.sqlite3")
    }

    fn pending(&self) -> PathBuf {
        self.dir.path().join("pending-sync.json")
    }

    fn queue(&self) -> Queue {
        Queue::beside(&self.local())
    }

    /// The team database: opens while the wire is whole, and loses its connection when it is cut.
    fn team(&self, hint: Option<&'static str>) -> Box<dyn MemoryStore> {
        let wire = self.wire.clone();
        let file = self.team_file();

        let lazy: Lazy<dyn MemoryStore> = Lazy::classified(move || {
            wire.opens.fetch_add(1, Ordering::SeqCst);
            wire.pass()?;

            let inner = SqliteStore::open(&file).map_err(|error| StoreError::Backend(error.to_string()))?;
            Ok(Box::new(TeamNotes {
                inner,
                wire: wire.clone(),
            }) as Box<dyn MemoryStore>)
        });
        let lazy = lazy.with_reach(self.reach.clone());

        match hint {
            Some(hint) => Box::new(lazy.with_lost_hint(hint)),
            None => Box::new(lazy),
        }
    }

    fn store(&self, fallback: bool) -> SplitStore {
        let local = Box::new(SqliteStore::open(&self.local()).expect("local file"));
        let hint = (!fallback).then_some("Turn on the offline fallback");
        let split = SplitStore::routed(self.team(hint), local, NotesPlace::Remote, NotesPlace::Remote, None);

        if fallback {
            split.with_offline_fallback(&self.local(), TEAM.to_owned(), Secrets::default())
        } else {
            split
        }
    }

    /// [`Self::store`] with the fallback, and the team notes hidden on this computer left out.
    fn store_hiding(&self) -> SplitStore {
        let local = Box::new(SqliteStore::open(&self.local()).expect("local file"));
        let hidden = HiddenNotes::new(self.local(), TEAM);

        SplitStore::routed(
            self.team(None),
            local,
            NotesPlace::Remote,
            NotesPlace::Remote,
            Some(hidden),
        )
        .with_offline_fallback(&self.local(), TEAM.to_owned(), Secrets::default())
    }

    /// The wire is whole again and the cooldown of the lost connection is over.
    fn recover(&self) {
        self.wire.cut.store(false, Ordering::SeqCst);
        self.wire.calls_left.store(i64::MAX, Ordering::SeqCst);
        self.reach.opened();
    }

    fn cut(&self) {
        self.wire.cut.store(true, Ordering::SeqCst);
    }

    /// The team database's own file, as a teammate sees it.
    fn teammate(&self) -> SqliteStore {
        SqliteStore::open(&self.team_file()).expect("team file")
    }

    fn here(&self) -> SqliteStore {
        SqliteStore::open(&self.local()).expect("local file")
    }
}

fn titles(store: &mut dyn MemoryStore, text: &str) -> Vec<String> {
    store
        .search(&query(text, global_only()))
        .expect("search")
        .into_iter()
        .map(|hit| hit.title)
        .collect()
}

fn save(store: &mut dyn MemoryStore, title: &str, body: &str) -> Result<Saved, StoreError> {
    store.remember(&Target::Global, &draft(title, body, NoteKind::Note))
}

fn body_of(store: &mut dyn MemoryStore, id: i64) -> String {
    let notes = store.peek(&View::every_repo(), &[id]).expect("peek");
    notes.into_iter().next().expect("the note").body
}

fn append(text: &str) -> NotePatch {
    NotePatch {
        append: Some(text.to_owned()),
        ..NotePatch::default()
    }
}

fn replace(body: &str) -> NotePatch {
    NotePatch {
        body: Some(body.to_owned()),
        ..NotePatch::default()
    }
}

#[test]
fn a_save_after_the_connection_dropped_lands_here_waits_and_is_copied_back() {
    let rig = Rig::new();
    let mut store = rig.store(true);

    save(&mut store, "before the drop", "reached the team database").expect("saved online");
    assert_eq!(titles(&mut rig.teammate(), "reached"), ["before the drop"]);
    assert!(!store.offline());

    // The connection drops while the server runs: the save is not lost, and not an error.
    rig.cut();
    let saved = save(&mut store, "after the drop", "must not be lost").expect("saved on this computer");
    assert!(store.offline());
    assert!(
        saved.id >= harness_memory::store::split::LOCAL_ID_OFFSET,
        "a note of this computer"
    );
    assert_eq!(titles(&mut rig.here(), "lost"), ["after the drop"]);
    assert!(rig.pending().is_file(), "it waits to be copied");
    assert!(titles(&mut rig.teammate(), "lost").is_empty());

    // The wire is back: the next call copies it, and this computer's copy goes.
    rig.recover();
    titles(&mut store, "drop");
    assert!(!store.offline());
    assert_eq!(titles(&mut rig.teammate(), "lost"), ["after the drop"]);
    assert!(!rig.pending().exists(), "nothing waits any more");
    assert!(
        titles(&mut rig.here(), "lost").is_empty(),
        "the team holds it as it is, so this computer's copy is deleted"
    );
}

#[test]
fn reads_fall_back_after_a_drop_and_the_calls_after_make_no_team_attempt_while_it_cools() {
    let rig = Rig::new();
    let mut store = rig.store(true);
    let id = save(&mut store, "kept in the team", "body").expect("saved online").id;
    save(&mut rig.here(), "kept here", "body").expect("a note of this computer");

    rig.cut();
    let before = rig.wire.attempts();

    // The first read finds the drop and is answered from this computer.
    assert_eq!(titles(&mut store, "kept"), ["kept here"]);
    assert_eq!(rig.wire.attempts(), before + 1, "one attempt found it out");
    assert!(store.offline());

    // The later reads, of any kind, go straight to this computer.
    assert_eq!(titles(&mut store, "kept"), ["kept here"]);
    store.peek(&View::every_repo(), &[id]).expect("peek");
    store.get(&View::every_repo(), &[id]).expect("get");
    store.similar(&global_only(), id, 3).expect("similar");
    store
        .list_notes(&common::listing::listing(global_only()))
        .expect("list");
    store.find_by_title(&Target::Global, "kept here").expect("find");
    assert_eq!(
        rig.wire.attempts(),
        before + 1,
        "the cooldown spared the dead connection"
    );
}

#[test]
fn without_the_fallback_a_lost_connection_fails_with_the_hint_and_the_next_call_connects_again() {
    let rig = Rig::new();
    let mut store = rig.store(false);

    save(&mut store, "first", "body").expect("saved");
    assert_eq!(rig.wire.opens.load(Ordering::SeqCst), 1);

    rig.cut();
    let error = save(&mut store, "second", "body").expect_err("the connection dropped");
    assert!(matches!(error, StoreError::Unreachable(_)), "{error:?}");
    assert!(
        error.to_string().contains("Turn on the offline fallback"),
        "the first failure says what to change too: {error}"
    );
    assert!(
        titles(&mut rig.here(), "second").is_empty(),
        "nothing is saved here instead"
    );
    assert!(!store.offline());

    // The next call connects again at once (the server may be up), instead of reusing the dead
    // connection or waiting for a cooldown.
    rig.wire.cut.store(false, Ordering::SeqCst);
    save(&mut store, "third", "body").expect("saved after reconnecting");
    assert_eq!(rig.wire.opens.load(Ordering::SeqCst), 2);
    assert_eq!(titles(&mut rig.teammate(), "third"), ["third"]);
}

#[test]
fn an_update_that_finds_the_outage_is_queued_and_applied_by_its_author_when_it_is_back() {
    let rig = Rig::new();
    let mut store = rig.store(true);
    let id = save(&mut store, "Ledger", "first line").expect("saved online").id;
    assert!(id < harness_memory::store::split::LOCAL_ID_OFFSET);

    rig.cut();
    let patch = NotePatch {
        append: Some("added offline".to_owned()),
        provenance: Provenance {
            source: Some("offline-client".to_owned()),
            author: Author::new("Ada", Some("offline-client")),
            ..Provenance::default()
        },
        expected_revision: Some(1),
        ..NotePatch::default()
    };
    let saved = store.update(&global_only(), id, &patch).expect("queued, not failed");

    assert!(saved.queued, "the reply says it only waits");
    assert_eq!(saved.id, id);
    assert!(store.offline());

    let waiting = rig.queue().entries(TEAM).expect("queue");
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].base_revision, Some(1), "the expected revision is the base");
    assert_eq!(
        body_of(&mut rig.teammate(), id),
        "first line",
        "nothing reached the team yet"
    );

    // A second change while down is queued behind the first, in order.
    store.update(&global_only(), id, &append("and more")).expect("queued");

    rig.recover();
    titles(&mut store, "Ledger");

    assert!(!store.offline());
    assert_eq!(body_of(&mut rig.teammate(), id), "first line\nadded offline\nand more");
    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());
    assert!(!rig.dir.path().join("pending-updates.json").exists());

    let note = rig.teammate().peek(&global_only(), &[id]).expect("peek").remove(0);
    assert!(note.revision >= 3, "both changes were applied");
}

#[test]
fn the_author_of_a_queued_change_is_kept_when_it_is_applied() {
    let rig = Rig::new();
    let mut store = rig.store(true);
    let id = save(&mut store, "Ledger", "first line").expect("saved online").id;

    rig.cut();
    let patch = NotePatch {
        append: Some("by Ada".to_owned()),
        provenance: Provenance {
            source: Some("offline-client".to_owned()),
            author: Author::new("Ada", Some("offline-client")),
            ..Provenance::default()
        },
        ..NotePatch::default()
    };
    store.update(&global_only(), id, &patch).expect("queued");

    rig.recover();
    titles(&mut store, "Ledger");

    let note = rig.teammate().peek(&global_only(), &[id]).expect("peek").remove(0);
    assert_eq!(note.body, "first line\nby Ada");
    assert_eq!(note.provenance.source.as_deref(), Some("offline-client"));
    assert_eq!(note.authors.updated_by, Author::new("Ada", Some("offline-client")));
}

/// Queues `patch`, made at `at` (Unix seconds) against revision `base` when given, for team note `id`.
fn queue_change(rig: &Rig, id: i64, patch: &NotePatch, at: i64, base: Option<i64>) {
    let patch = NotePatch {
        expected_revision: base,
        ..patch.clone()
    };
    let update = Update::new(TEAM, id, &global_only(), &patch, at).expect("update");
    rig.queue().add(update).expect("queued");
}

/// A team note with `body`, and a store that is down with the changes `queued` waiting, each made
/// at the time it names (Unix seconds) and, when given, against the revision it names.
fn waiting_changes(rig: &Rig, body: &str, queued: &[(NotePatch, i64, Option<i64>)]) -> (SplitStore, i64) {
    let id = save(&mut rig.teammate(), "Ledger", body).expect("team note").id;

    for (patch, at, base) in queued {
        queue_change(rig, id, patch, *at, *base);
    }

    (rig.store(true), id)
}

/// A team note and two replacements of it queued, the second made in the very second the note was
/// written, and a pause after, so that whatever writes the note first is newer than the second
/// change (as the first change's write always is in a real replay).
fn waiting_follower(rig: &Rig, first: &NotePatch, second: &NotePatch) -> (SplitStore, i64) {
    let id = save(&mut rig.teammate(), "Ledger", "first").expect("team note").id;
    let written = rig
        .teammate()
        .peek(&global_only(), &[id])
        .expect("peek")
        .remove(0)
        .updated_at;

    queue_change(rig, id, first, LATER, None);
    queue_change(rig, id, second, written, None);
    std::thread::sleep(Duration::from_millis(1100));

    (rig.store(true), id)
}

const LONG_AGO: i64 = 1;
const LATER: i64 = 4_000_000_000;

#[test]
fn an_appended_text_is_applied_whatever_happened_to_the_note_since() {
    let rig = Rig::new();
    let (mut store, id) = waiting_changes(&rig, "first", &[(append("mine"), LONG_AGO, None)]);

    // A teammate changed the note after the change was made.
    rig.teammate()
        .update(&global_only(), id, &replace("teammate text"))
        .expect("teammate");

    titles(&mut store, "Ledger");

    assert_eq!(body_of(&mut rig.teammate(), id), "teammate text\nmine");
    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());
}

#[test]
fn an_appended_text_the_note_already_ends_with_is_applied_when_it_was_never_started() {
    let rig = Rig::new();
    let (mut store, id) = waiting_changes(&rig, "first\nmine", &[(append("mine"), LONG_AGO, None)]);

    titles(&mut store, "Ledger");

    assert_eq!(
        body_of(&mut rig.teammate(), id),
        "first\nmine\nmine",
        "a repeat that was meant is not taken for an earlier application"
    );
    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());
}

#[test]
fn an_append_whose_answer_was_lost_is_not_applied_twice() {
    let rig = Rig::new();
    let (mut store, id) = waiting_changes(&rig, "first", &[(append("mine"), LONG_AGO, None)]);

    // The write reaches the team database and its answer is lost: the change stays queued.
    rig.wire.lose_reply.store(true, Ordering::SeqCst);
    titles(&mut store, "Ledger");
    assert_eq!(body_of(&mut rig.teammate(), id), "first\nmine");
    assert_eq!(rig.queue().entries(TEAM).expect("queue").len(), 1);

    rig.recover();
    titles(&mut rig.store(true), "Ledger");

    assert_eq!(body_of(&mut rig.teammate(), id), "first\nmine", "applied once");
    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());
}

#[test]
fn a_replacement_behind_one_whose_answer_was_lost_still_follows_it() {
    let rig = Rig::new();
    let (mut store, id) = waiting_changes(
        &rig,
        "first",
        &[(replace("second"), LATER, Some(1)), (replace("third"), LATER, Some(1))],
    );

    rig.wire.lose_reply.store(true, Ordering::SeqCst);
    titles(&mut store, "Ledger");
    assert_eq!(body_of(&mut rig.teammate(), id), "second");

    // The next replay finds the first change in the note, and the second follows it.
    rig.recover();
    titles(&mut rig.store(true), "Ledger");

    assert_eq!(body_of(&mut rig.teammate(), id), "third");
    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());
    assert!(
        rig.here()
            .find_by_title(&Target::Global, "Ledger (offline change)")
            .expect("find")
            .is_none(),
        "the person's own earlier change is no conflict"
    );
}

#[test]
fn a_replacement_is_applied_to_a_note_that_was_not_changed_since() {
    let rig = Rig::new();
    let (mut store, id) = waiting_changes(&rig, "first", &[(replace("rewritten"), LATER, None)]);

    titles(&mut store, "Ledger");

    assert_eq!(body_of(&mut rig.teammate(), id), "rewritten");
    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());
    assert!(
        titles(&mut rig.here(), "offline").is_empty(),
        "nothing had to be kept here"
    );
}

#[test]
fn a_replacement_the_note_has_already_is_dropped_without_a_conflict() {
    let rig = Rig::new();
    let (mut store, id) = waiting_changes(&rig, "first", &[(replace("rewritten"), LONG_AGO, None)]);

    // The note was changed after the change was made, to the very text the change gives (the
    // process applied it, and stopped before it took the change off the queue).
    rig.teammate()
        .update(&global_only(), id, &replace("rewritten"))
        .expect("applied");

    titles(&mut store, "Ledger");

    assert_eq!(body_of(&mut rig.teammate(), id), "rewritten");
    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());
    assert!(
        rig.here()
            .find_by_title(&Target::Global, "Ledger (offline change)")
            .expect("find")
            .is_none(),
        "nothing is kept as a note: the change is in the team note"
    );
}

#[test]
fn a_replacement_after_a_teammates_change_is_kept_as_a_note_here_and_leaves_nothing_queued() {
    let rig = Rig::new();
    let (mut store, id) = waiting_changes(&rig, "first", &[(replace("my rewrite"), LONG_AGO, None)]);
    rig.teammate()
        .update(&global_only(), id, &replace("teammate rewrite"))
        .expect("teammate");

    titles(&mut store, "Ledger");

    assert_eq!(
        body_of(&mut rig.teammate(), id),
        "teammate rewrite",
        "the teammate's text stays"
    );
    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());

    let kept = rig
        .here()
        .find_by_title(&Target::Global, "Ledger (offline change)")
        .expect("find")
        .expect("saved here for the person to decide");
    assert_eq!(kept.body, "my rewrite");
    assert!(
        !rig.pending().exists(),
        "it is not listed as waiting for the team database"
    );
}

#[test]
fn a_replacement_against_another_revision_than_the_one_read_is_a_conflict() {
    let rig = Rig::new();
    let (mut store, id) = waiting_changes(
        &rig,
        "first",
        &[
            (replace("read at revision 1"), LATER, Some(1)),
            (replace("read at revision 7"), LATER, Some(7)),
        ],
    );

    titles(&mut store, "Ledger");

    assert_eq!(body_of(&mut rig.teammate(), id), "read at revision 1");
    let kept = rig
        .here()
        .find_by_title(&Target::Global, "Ledger (offline change)")
        .expect("find")
        .expect("saved here");
    assert_eq!(kept.body, "read at revision 7");
    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());
}

#[test]
fn changes_one_person_made_in_a_row_do_not_fall_out_with_each_other() {
    let rig = Rig::new();
    let (mut store, id) = waiting_changes(
        &rig,
        "first",
        &[
            (replace("second"), LATER, None),
            (replace("third"), LATER, None),
            (append("and more"), LATER, None),
        ],
    );

    titles(&mut store, "Ledger");

    assert_eq!(
        body_of(&mut rig.teammate(), id),
        "third\nand more",
        "each followed the one before"
    );
    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());
    assert!(
        rig.here()
            .find_by_title(&Target::Global, "Ledger (offline change)")
            .expect("find")
            .is_none(),
        "nothing was kept as a note"
    );
}

#[test]
fn a_change_made_before_the_write_of_the_one_before_it_follows_it_in_the_same_replay() {
    let rig = Rig::new();
    let (mut store, id) = waiting_follower(&rig, &replace("second"), &replace("third"));

    titles(&mut store, "Ledger");

    assert_eq!(body_of(&mut rig.teammate(), id), "third");
    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());
    assert!(
        rig.here()
            .find_by_title(&Target::Global, "Ledger (offline change)")
            .expect("find")
            .is_none(),
        "the person's own earlier change is no conflict"
    );
}

#[test]
fn a_replay_that_stops_between_two_changes_goes_on_without_a_conflict() {
    let rig = Rig::new();
    let (mut store, id) = waiting_follower(&rig, &replace("second"), &replace("third"));

    // The connection opens, the first change is applied (a read, a write), then it is lost.
    rig.wire.calls_left.store(3, Ordering::SeqCst);
    titles(&mut store, "Ledger");
    assert_eq!(body_of(&mut rig.teammate(), id), "second");

    rig.recover();
    titles(&mut rig.store(true), "Ledger");

    assert_eq!(body_of(&mut rig.teammate(), id), "third");
    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());
    assert!(
        rig.here()
            .find_by_title(&Target::Global, "Ledger (offline change)")
            .expect("find")
            .is_none()
    );
}

#[test]
fn a_replacement_queued_behind_an_append_does_not_overwrite_a_teammates_later_edit() {
    let rig = Rig::new();
    let (mut store, id) = waiting_changes(
        &rig,
        "first",
        &[(append("one"), LONG_AGO, None), (replace("mine"), LONG_AGO, None)],
    );

    // A teammate edited the note after both changes were made.
    rig.teammate()
        .update(&global_only(), id, &replace("teammate text"))
        .expect("teammate");

    titles(&mut store, "Ledger");

    assert_eq!(
        body_of(&mut rig.teammate(), id),
        "teammate text\none",
        "the append goes on, the replacement does not blot out the teammate"
    );
    let kept = rig
        .here()
        .find_by_title(&Target::Global, "Ledger (offline change)")
        .expect("find")
        .expect("saved here");
    assert_eq!(kept.body, "mine");
    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());
}

#[test]
fn a_replacement_is_not_rebased_when_someone_wrote_between_the_read_and_the_write() {
    let rig = Rig::new();
    let (mut store, id) = waiting_changes(
        &rig,
        "first",
        &[(append("one"), LATER, None), (replace("mine"), LATER, Some(1))],
    );

    // A teammate's write lands between the replay's read of the note and its own write.
    *rig.wire.before_update.lock().expect("lock") = Some("teammate".to_owned());
    titles(&mut store, "Ledger");

    assert_eq!(
        body_of(&mut rig.teammate(), id),
        "first\nteammate\none",
        "the teammate's text stays"
    );
    let kept = rig
        .here()
        .find_by_title(&Target::Global, "Ledger (offline change)")
        .expect("find")
        .expect("saved here");
    assert_eq!(kept.body, "mine");
}

#[test]
fn at_most_a_hundred_changes_are_applied_per_call_and_the_rest_follows_at_the_next_one() {
    let rig = Rig::new();
    let queued: Vec<_> = (0..101)
        .map(|number| (append(&format!("line {number}")), LONG_AGO, None))
        .collect();
    let (mut store, id) = waiting_changes(&rig, "first", &queued);

    titles(&mut store, "Ledger");

    assert_eq!(
        rig.queue().entries(TEAM).expect("queue").len(),
        1,
        "a call applies a hundred"
    );
    assert!(body_of(&mut rig.teammate(), id).ends_with("line 99"));

    // No delay: the very next call goes on.
    titles(&mut store, "Ledger");

    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());
    assert!(body_of(&mut rig.teammate(), id).ends_with("line 100"));
}

#[test]
fn a_change_to_a_note_deleted_meanwhile_is_kept_as_a_note_here() {
    let rig = Rig::new();
    let (mut store, id) = waiting_changes(&rig, "first", &[(append("text of the change"), LONG_AGO, None)]);
    rig.teammate()
        .forget(&View::every_repo(), &Target::Global, &Forget::Note(id))
        .expect("deleted");

    titles(&mut store, "Ledger");

    let kept = rig
        .here()
        .find_by_title(&Target::Global, &format!("Offline change to note {id}"))
        .expect("find")
        .expect("saved here");
    assert_eq!(kept.body, "text of the change");
    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());
}

#[test]
fn two_changes_kept_the_same_way_do_not_replace_each_other() {
    let rig = Rig::new();
    let (mut store, id) = waiting_changes(
        &rig,
        "first",
        &[(replace("one"), LONG_AGO, None), (replace("two"), LONG_AGO, None)],
    );
    rig.teammate()
        .update(&global_only(), id, &replace("teammate"))
        .expect("teammate");

    titles(&mut store, "Ledger");

    let mut here = rig.here();
    let bodies: Vec<String> = ["Ledger (offline change)", "Ledger (offline change) 2"]
        .iter()
        .map(|title| {
            here.find_by_title(&Target::Global, title)
                .expect("find")
                .unwrap_or_else(|| panic!("{title} was not saved"))
                .body
        })
        .collect();
    assert_eq!(bodies, ["one", "two"]);
}

#[test]
fn a_connection_lost_during_the_replay_keeps_the_rest_in_order() {
    let rig = Rig::new();
    let (mut store, id) = waiting_changes(
        &rig,
        "first",
        &[
            (append("one"), LONG_AGO, None),
            (append("two"), LONG_AGO, None),
            (append("three"), LONG_AGO, None),
        ],
    );

    // The team database opens and answers for the first change (a read and a write), then the
    // connection is lost.
    rig.wire.calls_left.store(3, Ordering::SeqCst);
    titles(&mut store, "Ledger");

    assert!(
        store.offline(),
        "the lost connection is served from this computer from now on"
    );
    let rest: Vec<String> = rig
        .queue()
        .entries(TEAM)
        .expect("queue")
        .into_iter()
        .filter_map(|update| update.patch.append)
        .collect();
    assert_eq!(rest, ["two", "three"], "what was not applied is kept, in order");
    assert_eq!(body_of(&mut rig.teammate(), id), "first\none");

    // The next server to start finishes it, once, in order (this one tries again after a minute).
    rig.recover();
    titles(&mut rig.store(true), "Ledger");

    assert_eq!(body_of(&mut rig.teammate(), id), "first\none\ntwo\nthree");
    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());
}

#[test]
fn another_process_applying_the_queue_leaves_it_alone() {
    let rig = Rig::new();
    let (mut store, id) = waiting_changes(&rig, "first", &[(append("mine"), LONG_AGO, None)]);

    let _other = rig.queue().replaying().expect("lock").expect("free");
    titles(&mut store, "Ledger");

    assert_eq!(
        body_of(&mut rig.teammate(), id),
        "first",
        "skipped while another process holds it"
    );
    assert_eq!(rig.queue().entries(TEAM).expect("queue").len(), 1);
}

#[test]
fn deleting_a_team_note_while_the_database_is_down_says_so_and_changes_nothing() {
    let rig = Rig::new();
    let mut store = rig.store(true);
    let id = save(&mut store, "Ledger", "first").expect("saved online").id;

    rig.cut();
    // The first call finds the outage: it is not "not found", and the note stays.
    let error = store
        .forget(&global_only(), &Target::Global, &Forget::Note(id))
        .expect_err("refused");
    let message = error.to_string();
    assert!(
        message.contains("unreachable") && message.contains("nothing was changed"),
        "{message}"
    );
    assert!(!message.contains("not found"), "{message}");

    // Down already, the answer is the same.
    let again = store
        .forget(&global_only(), &Target::Global, &Forget::Note(id))
        .expect_err("refused");
    assert!(again.to_string().contains("nothing was changed"), "{again}");

    rig.recover();
    assert_eq!(body_of(&mut store, id), "first", "the note is still there");
}

#[test]
fn an_update_the_queue_cannot_hold_is_refused_visibly() {
    let rig = Rig::new();
    let mut store = rig.store(true);
    let id = save(&mut store, "Ledger", "first").expect("saved online").id;
    rig.cut();

    // A file that cannot be read is never written over.
    std::fs::write(rig.dir.path().join("pending-updates.json"), "not json").expect("write");
    let error = store.update(&global_only(), id, &append("lost?")).expect_err("refused");
    assert!(error.to_string().contains("pending-updates.json"), "{error}");
    assert_eq!(
        std::fs::read_to_string(rig.dir.path().join("pending-updates.json")).expect("read"),
        "not json"
    );
}

#[test]
fn an_update_whose_body_and_append_together_are_too_long_is_refused_when_it_is_queued() {
    let rig = Rig::new();
    let mut store = rig.store(true);
    let id = save(&mut store, "Ledger", "first").expect("saved online").id;
    rig.cut();

    let patch = NotePatch {
        body: Some("a".repeat(3000)),
        append: Some("b".repeat(3000)),
        ..NotePatch::default()
    };
    let error = store
        .update(&global_only(), id, &patch)
        .expect_err("refused as the online path does");

    assert!(matches!(error, StoreError::Invalid(_)), "{error:?}");
    assert!(rig.queue().entries(TEAM).expect("queue").is_empty());
}

#[test]
fn a_queued_change_too_long_to_apply_is_kept_cut_and_does_not_stop_the_queue() {
    let rig = Rig::new();
    let too_long = NotePatch {
        body: Some("x".repeat(4000)),
        append: Some("tail".to_owned()),
        ..NotePatch::default()
    };
    let (mut store, id) = waiting_changes(
        &rig,
        "first",
        &[(too_long, LATER, None), (append("next"), LONG_AGO, None)],
    );

    titles(&mut store, "Ledger");

    assert!(rig.queue().entries(TEAM).expect("queue").is_empty(), "nothing is stuck");
    assert_eq!(
        body_of(&mut rig.teammate(), id),
        "first\nnext",
        "the change behind it was applied"
    );

    let kept = rig
        .here()
        .find_by_title(&Target::Global, "Ledger (offline change)")
        .expect("find")
        .expect("saved here");
    assert!(kept.body.chars().count() <= harness_memory::limits::BODY_MAX);
    assert!(kept.body.starts_with("xxxx"), "the start of the text is kept");
    assert!(kept.body.contains("cut"), "and it says it was cut");
}

#[test]
fn a_change_to_a_note_hidden_since_keeps_only_the_text_of_the_change() {
    let rig = Rig::new();
    let id = save(&mut rig.teammate(), "Ledger", "text only the team may see")
        .expect("team note")
        .id;
    let note = rig.teammate().peek(&global_only(), &[id]).expect("peek").remove(0);
    assert!(HiddenNotes::new(rig.local(), TEAM).hide(&note).expect("hide"));
    queue_change(&rig, id, &append("mine"), LONG_AGO, None);

    titles(&mut rig.store_hiding(), "Ledger");

    // Titled after the change alone: the hidden note's title is not shown either.
    let kept = rig
        .here()
        .find_by_title(&Target::Global, &format!("Offline change to note {id}"))
        .expect("find")
        .expect("saved here");
    assert_eq!(kept.body, "mine", "the hidden note's text is not copied");
}

#[test]
fn an_appended_text_is_masked_in_the_queue_file() {
    let rig = Rig::new();
    let mut store = rig.store(true);
    let id = save(&mut store, "Ledger", "first").expect("saved online").id;
    rig.cut();

    // Built at run time: a token written out would trip secret scanners on the source.
    let token = format!("ghp_{}", "a".repeat(36));
    store
        .update(&global_only(), id, &append(&format!("key {token}")))
        .expect("queued");

    let file = std::fs::read_to_string(rig.dir.path().join("pending-updates.json")).expect("queue file");
    assert!(!file.contains(&token), "the token is not stored as written");
    assert!(file.contains("key"), "the rest of the text is kept");
}

mod scripts {
    use super::*;
    use common::scripts::script;
    use harness_memory::store::{
        SavedScript, Script, ScriptDraft, ScriptOrigin, ScriptRef, ScriptStore, ScriptSummary, SplitScripts, TrustStore,
    };

    /// The team's scripts, reached through the wire.
    struct TeamScripts {
        inner: SqliteStore,
        wire: Wire,
    }

    impl ScriptStore for TeamScripts {
        fn save_script(&mut self, target: &Target, draft: &ScriptDraft) -> Result<SavedScript, StoreError> {
            self.wire.pass()?;
            self.inner.save_script(target, draft)
        }

        fn list_scripts(&mut self, view: &View, query: Option<&str>) -> Result<Vec<ScriptSummary>, StoreError> {
            self.wire.pass()?;
            self.inner.list_scripts(view, query)
        }

        fn get_script(&mut self, view: &View, script: &ScriptRef) -> Result<Option<Script>, StoreError> {
            self.wire.pass()?;
            self.inner.get_script(view, script)
        }

        fn delete_script(&mut self, view: &View, script: &ScriptRef) -> Result<bool, StoreError> {
            self.wire.pass()?;
            self.inner.delete_script(view, script)
        }

        fn record_run(&mut self, id: i64, exit_code: Option<i32>, at: i64) -> Result<(), StoreError> {
            self.wire.pass()?;
            self.inner.record_run(id, exit_code, at)
        }

        fn origin(&self, _id: i64) -> ScriptOrigin {
            ScriptOrigin::Single
        }
    }

    fn team(rig: &Rig, hint: Option<&'static str>) -> Box<dyn ScriptStore> {
        let wire = rig.wire.clone();
        let file = rig.team_file();

        let lazy: Lazy<dyn ScriptStore> = Lazy::classified(move || {
            wire.opens.fetch_add(1, Ordering::SeqCst);
            wire.pass()?;

            let inner = SqliteStore::open(&file).map_err(|error| StoreError::Backend(error.to_string()))?;
            Ok(Box::new(TeamScripts {
                inner,
                wire: wire.clone(),
            }) as Box<dyn ScriptStore>)
        });
        let lazy = lazy.with_reach(rig.reach.clone());

        match hint {
            Some(hint) => Box::new(lazy.with_lost_hint(hint)),
            None => Box::new(lazy),
        }
    }

    fn store(rig: &Rig, fallback: bool) -> SplitScripts {
        let local = Box::new(SqliteStore::open(&rig.local()).expect("local file"));
        let hint = (!fallback).then_some("Turn on the offline fallback");
        let split = SplitScripts::new(team(rig, hint), local, NotesPlace::Remote, None);

        if !fallback {
            return split;
        }

        let trust = Box::new(SqliteStore::open(&rig.local()).expect("trust")) as Box<dyn TrustStore>;
        split.with_offline_fallback(&rig.local(), trust, TEAM.to_owned(), Secrets::default())
    }

    fn named(scripts: &mut dyn ScriptStore, name: &str) -> Option<Script> {
        scripts
            .get_script(&global_only(), &ScriptRef::Name(name.to_owned()))
            .expect("get")
    }

    #[test]
    fn a_script_saved_after_the_connection_dropped_lands_here_waits_and_is_copied_back() {
        let rig = Rig::new();
        let mut scripts = store(&rig, true);

        scripts
            .save_script(&Target::Global, &script("before", "reached the team", "echo 1"))
            .expect("saved online");
        assert!(named(&mut rig.teammate(), "before").is_some());

        rig.cut();
        scripts
            .save_script(&Target::Global, &script("after", "must not be lost", "echo 2"))
            .expect("saved on this computer");
        assert!(scripts.offline());
        assert!(named(&mut rig.here(), "after").is_some());
        assert!(rig.pending().is_file(), "it waits to be copied");

        rig.recover();
        scripts.list_scripts(&global_only(), None).expect("list");

        assert!(!scripts.offline());
        assert_eq!(named(&mut rig.teammate(), "after").expect("copied").body, "echo 2");
        assert!(!rig.pending().exists());
        assert!(
            named(&mut rig.here(), "after").is_none(),
            "the team holds it as it is, so this computer's copy is deleted"
        );
    }

    #[test]
    fn script_reads_fall_back_after_a_drop_and_make_no_team_attempt_while_it_cools() {
        let rig = Rig::new();
        let mut scripts = store(&rig, true);
        scripts
            .save_script(&Target::Global, &script("shared", "in the team", "echo 1"))
            .expect("saved online");
        rig.here()
            .save_script(&Target::Global, &script("mine", "on this computer", "echo 2"))
            .expect("a script of this computer");

        rig.cut();
        let before = rig.wire.attempts();

        let listed = scripts.list_scripts(&global_only(), None).expect("falls back");
        let names: Vec<&str> = listed.iter().map(|script| script.name.as_str()).collect();
        assert_eq!(names, ["mine"]);
        assert_eq!(rig.wire.attempts(), before + 1, "one attempt found it out");

        scripts.list_scripts(&global_only(), None).expect("list");
        named(&mut scripts, "mine").expect("get");
        scripts.record_run(1_000_000_001, Some(0), 1).expect("record");
        assert_eq!(
            rig.wire.attempts(),
            before + 1,
            "the cooldown spared the dead connection"
        );
    }

    #[test]
    fn without_the_fallback_a_lost_script_connection_fails_with_the_hint_and_connects_again() {
        let rig = Rig::new();
        let mut scripts = store(&rig, false);
        scripts
            .save_script(&Target::Global, &script("first", "", "echo 1"))
            .expect("saved");

        rig.cut();
        let error = scripts
            .save_script(&Target::Global, &script("second", "", "echo 2"))
            .expect_err("the connection dropped");
        assert!(matches!(error, StoreError::Unreachable(_)), "{error:?}");
        assert!(error.to_string().contains("Turn on the offline fallback"), "{error}");
        assert!(
            named(&mut rig.here(), "second").is_none(),
            "nothing is saved here instead"
        );

        rig.wire.cut.store(false, Ordering::SeqCst);
        scripts
            .save_script(&Target::Global, &script("third", "", "echo 3"))
            .expect("saved after reconnecting");
        assert_eq!(rig.wire.opens.load(Ordering::SeqCst), 2);
        assert!(named(&mut rig.teammate(), "third").is_some());
    }
}
