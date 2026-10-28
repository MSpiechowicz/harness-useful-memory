//! The offline fallback of `SplitStore`: while the team database cannot be opened, scopes set to
//! `remote` (and `remote+local`) are read and saved on this computer; the notes saved for a
//! `remote` scope are copied to the team database once it opens again. Without the fallback the
//! calls that need the team database fail.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{draft, global_only, query};
use harness_memory::backend::{NotesPlace, Secrets};
use harness_memory::store::{
    Forget, Lazy, MemoryStore, NoteKind, NotePatch, Reach, SplitStore, SqliteStore, StoreError, Target, View,
};
use tempfile::TempDir;

const COOLDOWN: Duration = Duration::from_millis(30);

const TEAM: &str = "team:a";

struct Fixture {
    dir: TempDir,
    /// Whether the team database opens.
    up: Arc<AtomicBool>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: TempDir::new().expect("temp dir"),
            up: Arc::new(AtomicBool::new(false)),
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

    /// The team database, which opens once `up` is set.
    fn team(&self) -> Box<dyn MemoryStore> {
        let up = Arc::clone(&self.up);
        let file = self.team_file();

        let lazy: Lazy<dyn MemoryStore> = Lazy::new(move || {
            if !up.load(Ordering::SeqCst) {
                return Err("cannot open the team database".to_owned());
            }

            SqliteStore::open(&file)
                .map(|store| Box::new(store) as Box<dyn MemoryStore>)
                .map_err(|error| error.to_string())
        });

        Box::new(lazy.with_reach(Reach::with_cooldown(COOLDOWN)))
    }

    fn store(&self, place: NotesPlace, fallback: bool) -> SplitStore {
        self.places(place, place, fallback, TEAM)
    }

    /// The store with repository notes in `repo`, global notes in `global`, meant for the team
    /// database named `team`.
    fn places(&self, repo: NotesPlace, global: NotesPlace, fallback: bool, team: &str) -> SplitStore {
        let local = Box::new(SqliteStore::open(&self.local()).expect("local file"));
        let split = SplitStore::routed(self.team(), local, repo, global, None);

        if fallback {
            split.with_offline_fallback(&self.local(), team.to_owned(), Secrets::default())
        } else {
            split
        }
    }

    /// The notes of the team database matching `text`.
    fn team_titles(&self, text: &str) -> Vec<String> {
        titles(&mut SqliteStore::open(&self.team_file()).expect("team file"), text)
    }

    fn local_titles(&self, text: &str) -> Vec<String> {
        titles(&mut SqliteStore::open(&self.local()).expect("local file"), text)
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

fn save(store: &mut SplitStore, title: &str) -> Result<i64, StoreError> {
    store
        .remember(
            &Target::Global,
            &draft(title, "kept for the offline test", NoteKind::Note),
        )
        .map(|saved| saved.id)
}

fn wait_out_the_cooldown() {
    std::thread::sleep(COOLDOWN * 3);
}

#[test]
fn a_remote_scope_is_served_from_this_computer_and_copied_when_the_team_database_is_back() {
    let fixture = Fixture::new();
    let mut store = fixture.store(NotesPlace::Remote, true);

    save(&mut store, "written offline").expect("saved on this computer");
    assert!(store.offline());
    assert!(fixture.pending().is_file(), "the note waits to be copied");

    let found = titles(&mut store, "offline");
    assert_eq!(found, ["written offline"], "the note is read back while offline");
    assert!(!fixture.team_file().exists(), "the team database was never created");

    fixture.up.store(true, Ordering::SeqCst);
    wait_out_the_cooldown();

    // The next call finds the team database open and copies what was saved offline.
    titles(&mut store, "offline");
    assert!(!store.offline());
    assert_eq!(fixture.team_titles("offline"), ["written offline"]);
    assert!(!fixture.pending().exists(), "nothing waits any more");
    assert_eq!(
        fixture.local_titles("offline"),
        ["written offline"],
        "this computer's copy stays"
    );

    // Back online, a save goes to the team database again and waits for nothing.
    save(&mut store, "written online").expect("saved");
    assert_eq!(fixture.team_titles("online"), ["written online"]);
    assert!(!fixture.pending().exists());
}

#[test]
fn a_counter_neither_opens_the_team_database_nor_copies_the_notes_saved_offline() {
    let fixture = Fixture::new();
    let mut store = fixture.store(NotesPlace::Remote, true);

    let id = save(&mut store, "written offline").expect("saved on this computer");
    assert!(store.offline());

    // The team database is back, but only a call that settles may notice it.
    fixture.up.store(true, Ordering::SeqCst);
    wait_out_the_cooldown();

    store.record_shown(&[id]).expect("shown");
    store.record_useful(&[id], 100).expect("useful");

    assert!(store.offline(), "a counter did not settle");
    assert!(fixture.pending().is_file(), "the note still waits to be copied");
    assert!(!fixture.team_file().exists(), "the team database was not opened");

    // The next call that settles copies it.
    titles(&mut store, "offline");
    assert!(!store.offline());
    assert_eq!(fixture.team_titles("offline"), ["written offline"]);
}

#[test]
fn notes_saved_offline_are_copied_when_a_later_server_starts() {
    let fixture = Fixture::new();
    save(&mut fixture.store(NotesPlace::Remote, true), "left behind").expect("saved");
    assert!(fixture.pending().is_file());

    fixture.up.store(true, Ordering::SeqCst);
    let mut later = fixture.store(NotesPlace::Remote, true);
    titles(&mut later, "behind");

    assert_eq!(fixture.team_titles("behind"), ["left behind"]);
    assert!(!fixture.pending().exists());
}

#[test]
fn a_note_kept_here_on_purpose_is_not_copied() {
    let fixture = Fixture::new();

    // Saved while the scope is `remote+local`: it was meant for this computer.
    save(&mut fixture.store(NotesPlace::RemoteLocal, true), "private").expect("saved");
    assert!(!fixture.pending().exists(), "the destination was this computer anyway");

    fixture.up.store(true, Ordering::SeqCst);
    let mut store = fixture.store(NotesPlace::Remote, true);
    titles(&mut store, "private");

    assert!(fixture.team_titles("private").is_empty());
}

#[test]
fn a_remote_local_scope_reads_this_computer_alone_while_the_team_database_is_down() {
    let fixture = Fixture::new();
    let mut store = fixture.store(NotesPlace::RemoteLocal, true);

    save(&mut store, "mine").expect("saved");
    assert_eq!(titles(&mut store, "mine"), ["mine"]);
    assert!(store.offline());
}

#[test]
fn without_the_fallback_an_unreachable_team_database_fails_the_save() {
    let fixture = Fixture::new();
    let mut store = fixture.store(NotesPlace::Remote, false);

    let error = save(&mut store, "nowhere").expect_err("no team database");
    assert!(matches!(error, StoreError::Backend(_)), "{error:?}");
    assert!(!store.offline());
    assert!(!fixture.pending().exists());
    assert!(
        fixture.local_titles("nowhere").is_empty(),
        "nothing is saved here instead"
    );
}

#[test]
fn a_row_that_reuses_a_deleted_pending_notes_id_is_not_copied() {
    let fixture = Fixture::new();

    // Saved offline for a `remote` scope, then deleted: its id is free again.
    let mut offline = fixture.store(NotesPlace::Remote, true);
    let doomed = save(&mut offline, "doomed").expect("saved");
    offline
        .forget(&View::every_repo(), &Target::Global, &Forget::Note(doomed))
        .expect("deleted");

    // A private note kept on this computer on purpose takes the id.
    save(&mut fixture.store(NotesPlace::Local, true), "private").expect("saved");
    assert_eq!(fixture.local_titles("private"), ["private"]);

    fixture.up.store(true, Ordering::SeqCst);
    titles(&mut fixture.store(NotesPlace::Remote, true), "private");

    assert!(
        fixture.team_titles("private").is_empty(),
        "the private note stayed here"
    );
}

#[test]
fn notes_are_not_copied_after_their_scope_moved_to_this_computer() {
    let fixture = Fixture::new();
    save(&mut fixture.store(NotesPlace::Remote, true), "was remote").expect("saved");

    // Repository notes still read the team database; global notes now stay on this computer.
    fixture.up.store(true, Ordering::SeqCst);
    let mut moved = fixture.places(NotesPlace::Remote, NotesPlace::Local, true, TEAM);
    titles(&mut moved, "remote");

    assert!(fixture.team_titles("remote").is_empty());
    assert!(fixture.pending().is_file(), "it keeps waiting");
}

#[test]
fn notes_saved_for_another_team_database_are_not_copied_to_this_one() {
    let fixture = Fixture::new();
    save(&mut fixture.store(NotesPlace::Remote, true), "team a note").expect("saved");

    fixture.up.store(true, Ordering::SeqCst);
    let mut other = fixture.places(NotesPlace::Remote, NotesPlace::Remote, true, "team:b");
    titles(&mut other, "note");

    assert!(fixture.team_titles("note").is_empty());
    assert!(fixture.pending().is_file());
}

#[test]
fn without_the_fallback_a_search_reaching_the_team_database_fails_instead_of_going_empty() {
    let fixture = Fixture::new();
    let mut store = SplitStore::routed(
        fixture.team(),
        Box::new(SqliteStore::open(&fixture.local()).expect("local file")),
        NotesPlace::Local,
        NotesPlace::Remote,
        None,
    );

    let both = View {
        repo: Some(common::repo_key("github.com/acme/widget")),
        global: true,
    };
    let error = store
        .search(&query("anything", both))
        .expect_err("the team database is down");
    assert!(matches!(error, StoreError::Backend(_)), "{error:?}");
}

#[test]
fn an_offline_save_that_replaces_a_note_kept_here_does_not_copy_it() {
    let fixture = Fixture::new();

    // A note kept on this computer on purpose, then an offline save under the same title.
    save(&mut fixture.store(NotesPlace::Local, true), "same title").expect("kept here");
    save(&mut fixture.store(NotesPlace::Remote, true), "same title").expect("saved offline");
    assert!(!fixture.pending().exists(), "it replaced a kept row, so nothing waits");

    fixture.up.store(true, Ordering::SeqCst);
    titles(&mut fixture.store(NotesPlace::Remote, true), "title");

    assert!(fixture.team_titles("title").is_empty(), "the kept note stayed here");
}

#[test]
fn a_note_renamed_while_still_offline_is_copied_under_its_new_title() {
    let fixture = Fixture::new();
    let mut store = fixture.store(NotesPlace::Remote, true);
    let id = save(&mut store, "first title").expect("saved offline");

    let patch = NotePatch {
        title: Some("second title".to_owned()),
        body: None,
        append: None,
        kind: None,
        tags: None,
        provenance: draft("x", "y", NoteKind::Note).provenance,
    };
    store.update(&global_only(), id, &patch).expect("renamed");

    fixture.up.store(true, Ordering::SeqCst);
    wait_out_the_cooldown();
    titles(&mut store, "title");

    assert_eq!(fixture.team_titles("second"), ["second title"]);
    assert!(!fixture.pending().exists());
}

mod scripts {
    use super::*;
    use common::scripts::script;
    use harness_memory::store::{ScriptRef, ScriptStore, SplitScripts, TrustStore};

    fn team(fixture: &Fixture) -> Box<dyn ScriptStore> {
        let up = Arc::clone(&fixture.up);
        let file = fixture.team_file();

        let lazy: Lazy<dyn ScriptStore> = Lazy::new(move || {
            if !up.load(Ordering::SeqCst) {
                return Err("cannot open the team database".to_owned());
            }

            SqliteStore::open(&file)
                .map(|store| Box::new(store) as Box<dyn ScriptStore>)
                .map_err(|error| error.to_string())
        });

        Box::new(lazy.with_reach(Reach::with_cooldown(COOLDOWN)))
    }

    fn store(fixture: &Fixture) -> SplitScripts {
        let local = Box::new(SqliteStore::open(&fixture.local()).expect("local file"));
        let trust = Box::new(SqliteStore::open(&fixture.local()).expect("trust")) as Box<dyn TrustStore>;

        SplitScripts::new(team(fixture), local, NotesPlace::Remote, None).with_offline_fallback(
            &fixture.local(),
            trust,
            TEAM.to_owned(),
            Secrets::default(),
        )
    }

    #[test]
    fn scripts_saved_offline_are_listed_here_and_copied_when_the_team_database_is_back() {
        let fixture = Fixture::new();
        let mut scripts = store(&fixture);

        scripts
            .save_script(&Target::Global, &script("greet", "says hi", "echo hi"))
            .expect("saved on this computer");
        assert!(scripts.offline());
        assert!(fixture.pending().is_file());

        let listed = scripts.list_scripts(&global_only(), None).expect("list");
        assert_eq!(
            listed.iter().map(|script| script.name.as_str()).collect::<Vec<_>>(),
            ["greet"]
        );

        fixture.up.store(true, Ordering::SeqCst);
        wait_out_the_cooldown();

        scripts.list_scripts(&global_only(), None).expect("list");
        assert!(!scripts.offline());
        assert!(!fixture.pending().exists());

        let mut team = SqliteStore::open(&fixture.team_file()).expect("team file");
        let copied = team
            .get_script(&global_only(), &ScriptRef::Name("greet".to_owned()))
            .expect("get");
        assert_eq!(copied.expect("copied to the team database").body, "echo hi");
    }
}
