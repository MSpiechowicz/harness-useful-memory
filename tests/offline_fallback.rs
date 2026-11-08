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
use harness_memory::backend::{NotesPlace, Place, Secrets};
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
        let split = SplitStore::routed(self.team(), local, repo, global, None).saving_to(Place::Local);

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
    assert!(
        fixture.local_titles("offline").is_empty(),
        "the team database holds it as it is, so this computer's copy is deleted"
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
    assert!(matches!(error, StoreError::Unreachable(_)), "{error:?}");
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
    assert!(matches!(error, StoreError::Unreachable(_)), "{error:?}");
}

#[test]
fn an_offline_save_over_a_row_kept_here_is_listed_and_copied_all_the_same() {
    let fixture = Fixture::new();

    // A row kept on this computer (a leftover of an earlier save), then an offline save under the
    // same title for a scope that is `remote`: the text saved last is meant for the team database.
    save(&mut fixture.store(NotesPlace::Local, true), "same title").expect("kept here");
    save(&mut fixture.store(NotesPlace::Remote, true), "same title").expect("saved offline");
    assert!(
        fixture.pending().is_file(),
        "it waits to be copied, though it replaced a row"
    );

    fixture.up.store(true, Ordering::SeqCst);
    titles(&mut fixture.store(NotesPlace::Remote, true), "title");

    assert_eq!(fixture.team_titles("title"), ["same title"]);
    assert!(!fixture.pending().exists());
}

#[test]
fn an_offline_save_leaves_a_waiting_list_it_cannot_read_in_place() {
    let fixture = Fixture::new();
    std::fs::write(fixture.pending(), "not json").expect("a damaged list");

    let mut store = fixture.store(NotesPlace::Remote, true);
    save(&mut store, "kept here anyway").expect("saved on this computer");

    assert_eq!(
        fixture.local_titles("anyway"),
        ["kept here anyway"],
        "the note is saved here"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.pending()).expect("list"),
        "not json",
        "the damaged list was not replaced by a small new one"
    );

    // The team database coming back copies nothing it cannot tell was meant for it.
    fixture.up.store(true, Ordering::SeqCst);
    wait_out_the_cooldown();
    titles(&mut store, "anyway");
    assert!(fixture.team_titles("anyway").is_empty());
    assert_eq!(fixture.local_titles("anyway"), ["kept here anyway"]);
}

#[test]
fn an_entry_whose_note_is_gone_from_this_computer_leaves_the_list() {
    let fixture = Fixture::new();
    let mut offline = fixture.store(NotesPlace::Remote, true);
    let doomed = save(&mut offline, "doomed").expect("saved");
    offline
        .forget(&View::every_repo(), &Target::Global, &Forget::Note(doomed))
        .expect("deleted");
    assert!(fixture.pending().is_file(), "the list was not told");

    fixture.up.store(true, Ordering::SeqCst);
    titles(&mut fixture.store(NotesPlace::Remote, true), "doomed");

    assert!(!fixture.pending().exists(), "nothing waits for a note that is not here");
    assert!(fixture.team_titles("doomed").is_empty());
}

#[test]
fn an_entry_whose_note_has_another_title_now_stays_listed() {
    let fixture = Fixture::new();
    let mut offline = fixture.store(NotesPlace::Remote, true);
    let id = save(&mut offline, "first title").expect("saved");

    // The title was changed behind the list's back.
    let renamed = NotePatch {
        title: Some("other title".to_owned()),
        ..NotePatch::default()
    };
    let raw = id - harness_memory::store::split::LOCAL_ID_OFFSET;
    SqliteStore::open(&fixture.local())
        .expect("local file")
        .update(&global_only(), raw, &renamed)
        .expect("renamed");

    fixture.up.store(true, Ordering::SeqCst);
    titles(&mut fixture.store(NotesPlace::Remote, true), "title");

    assert!(fixture.pending().is_file(), "it stays, as it did");
    assert!(fixture.team_titles("title").is_empty());
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
        expected_revision: None,
    };
    store.update(&global_only(), id, &patch).expect("renamed");

    fixture.up.store(true, Ordering::SeqCst);
    wait_out_the_cooldown();
    titles(&mut store, "title");

    assert_eq!(fixture.team_titles("second"), ["second title"]);
    assert!(!fixture.pending().exists());
}

#[test]
fn a_note_kept_on_purpose_once_the_team_database_is_back_is_not_copied_first() {
    let fixture = Fixture::new();
    let mut store = fixture.shared(Place::Remote);
    let target = Target::Global;

    save(&mut store, "waiting").expect("saved here for now");
    assert!(fixture.pending().is_file());

    // The team database is back, but nothing has asked it yet: the pick comes first.
    fixture.up.store(true, Ordering::SeqCst);
    wait_out_the_cooldown();

    store
        .remember_to(Place::Local, &target, &draft("waiting", "mine", NoteKind::Note))
        .expect("kept here");

    assert!(
        fixture.team_titles("waiting").is_empty(),
        "the old text was never copied to the team"
    );
    assert_eq!(local_body(&fixture, "waiting").as_deref(), Some("mine"));
    assert!(!fixture.pending().exists(), "it no longer waits");
}

#[test]
fn a_waiting_note_saved_again_in_another_letter_case_is_copied_under_the_new_title() {
    for place in [NotesPlace::Remote, NotesPlace::RemoteLocal] {
        let fixture = Fixture::new();
        let mut store = match place {
            NotesPlace::Remote => fixture.store(NotesPlace::Remote, true),
            _ => fixture.shared(Place::Remote),
        };

        save(&mut store, "deploy steps").expect("saved offline");
        store
            .remember(&Target::Global, &draft("Deploy Steps", "second text", NoteKind::Note))
            .expect("saved again");

        fixture.up.store(true, Ordering::SeqCst);
        wait_out_the_cooldown();
        titles(&mut store, "deploy");

        assert_eq!(fixture.team_titles("deploy"), ["Deploy Steps"], "{place:?}");
        assert!(
            fixture.local_titles("deploy").is_empty(),
            "{place:?}: no copy is left here"
        );
        assert!(!fixture.pending().exists(), "{place:?}: nothing waits any more");
    }
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

    #[test]
    fn a_script_saved_over_one_kept_here_is_listed_and_copied_all_the_same() {
        let fixture = Fixture::new();

        SqliteStore::open(&fixture.local())
            .expect("local file")
            .save_script(&Target::Global, &script("greet", "kept", "echo kept"))
            .expect("kept here");
        store(&fixture)
            .save_script(&Target::Global, &script("greet", "says hi", "echo hi"))
            .expect("saved on this computer");
        assert!(
            fixture.pending().is_file(),
            "it waits to be copied, though it replaced a script"
        );

        fixture.up.store(true, Ordering::SeqCst);
        store(&fixture).list_scripts(&global_only(), None).expect("list");

        let mut team = SqliteStore::open(&fixture.team_file()).expect("team file");
        let copied = team
            .get_script(&global_only(), &ScriptRef::Name("greet".to_owned()))
            .expect("get");
        assert_eq!(copied.expect("copied to the team database").body, "echo hi");
        assert!(!fixture.pending().exists());
    }

    #[test]
    fn an_entry_whose_script_is_gone_from_this_computer_leaves_the_list() {
        let fixture = Fixture::new();
        let mut scripts = store(&fixture);
        scripts
            .save_script(&Target::Global, &script("doomed", "", "echo 1"))
            .expect("saved");
        scripts
            .delete_script(&global_only(), &ScriptRef::Name("doomed".to_owned()))
            .expect("deleted");
        assert!(fixture.pending().is_file(), "the list was not told");

        fixture.up.store(true, Ordering::SeqCst);
        store(&fixture).list_scripts(&global_only(), None).expect("list");

        assert!(
            !fixture.pending().exists(),
            "nothing waits for a script that is not here"
        );
    }

    /// The scripts with `remote+local` routing, saving new scripts to `saves`, with the offline fallback.
    fn shared(fixture: &Fixture, saves: Place) -> SplitScripts {
        let local = Box::new(SqliteStore::open(&fixture.local()).expect("local file"));
        let trust = Box::new(SqliteStore::open(&fixture.local()).expect("trust")) as Box<dyn TrustStore>;

        SplitScripts::new(team(fixture), local, NotesPlace::RemoteLocal, None)
            .saving_to(saves)
            .with_offline_fallback(&fixture.local(), trust, TEAM.to_owned(), Secrets::default())
    }

    fn local_body(fixture: &Fixture, name: &str) -> Option<String> {
        SqliteStore::open(&fixture.local())
            .expect("local file")
            .get_script(&global_only(), &ScriptRef::Name(name.to_owned()))
            .expect("get")
            .map(|script| script.body)
    }

    #[test]
    fn a_script_kept_here_is_updated_where_it_is_while_remote_local_saves_to_the_team_and_it_is_down() {
        let fixture = Fixture::new();
        let mut scripts = shared(&fixture, Place::Remote);

        scripts
            .save_script_to(Place::Local, &Target::Global, &script("mine", "", "echo kept"))
            .expect("kept here");
        assert!(scripts.save_place(&Target::Global, "mine").expect("place") == Some(Place::Local));

        let saved = scripts
            .save_script(&Target::Global, &script("mine", "", "echo changed"))
            .expect("updated here");
        assert!(scripts.offline());
        assert!(!saved.created && !saved.not_listed);
        assert!(!fixture.pending().exists(), "a script kept here waits for nothing");

        // A name that is not here is new: it waits for the team database.
        scripts
            .save_script(&Target::Global, &script("fresh", "", "echo fresh"))
            .expect("saved here for now");
        assert!(fixture.pending().is_file());

        fixture.up.store(true, Ordering::SeqCst);
        wait_out_the_cooldown();
        scripts.list_scripts(&global_only(), None).expect("list");

        assert_eq!(local_body(&fixture, "mine").as_deref(), Some("echo changed"));
        assert_eq!(local_body(&fixture, "fresh"), None, "the new one was moved to the team");

        let mut team = SqliteStore::open(&fixture.team_file()).expect("team file");
        let names: Vec<String> = team
            .list_scripts(&global_only(), None)
            .expect("list")
            .into_iter()
            .map(|summary| summary.name)
            .collect();
        assert_eq!(names, ["fresh"]);
    }

    #[test]
    fn a_script_kept_here_is_updated_where_it_is_while_remote_local_saves_to_the_team_and_it_is_up() {
        let fixture = Fixture::new();
        fixture.up.store(true, Ordering::SeqCst);
        let mut scripts = shared(&fixture, Place::Remote);

        scripts
            .save_script_to(Place::Local, &Target::Global, &script("mine", "", "echo kept"))
            .expect("kept here");
        let saved = scripts
            .save_script(&Target::Global, &script("mine", "", "echo changed"))
            .expect("updated here");
        assert!(!saved.created);
        assert!(saved.script.summary.id >= harness_memory::store::split::LOCAL_ID_OFFSET);
        assert_eq!(local_body(&fixture, "mine").as_deref(), Some("echo changed"));

        scripts
            .save_script(&Target::Global, &script("fresh", "", "echo fresh"))
            .expect("saved to the team");
        assert!(scripts.save_place(&Target::Global, "fresh").expect("place") == Some(Place::Remote));

        let mut team = SqliteStore::open(&fixture.team_file()).expect("team file");
        let names: Vec<String> = team
            .list_scripts(&global_only(), None)
            .expect("list")
            .into_iter()
            .map(|summary| summary.name)
            .collect();
        assert_eq!(names, ["fresh"], "the script kept here was not published");

        // A place a person picked still wins: the team database gets it under the same name.
        scripts
            .save_script_to(Place::Remote, &Target::Global, &script("mine", "", "echo shared"))
            .expect("saved to the team");
        assert_eq!(local_body(&fixture, "mine").as_deref(), Some("echo changed"));
        let mut team = SqliteStore::open(&fixture.team_file()).expect("team file");
        assert_eq!(team.list_scripts(&global_only(), None).expect("list").len(), 2);
    }

    #[test]
    fn a_script_kept_on_purpose_leaves_the_list_of_what_waits_for_the_team_database() {
        let fixture = Fixture::new();
        let mut scripts = shared(&fixture, Place::Remote);

        scripts
            .save_script(&Target::Global, &script("waiting", "", "echo agent"))
            .expect("saved here for now");
        assert!(fixture.pending().is_file());

        scripts
            .save_script_to(Place::Local, &Target::Global, &script("waiting", "", "echo mine"))
            .expect("kept here");
        assert!(!fixture.pending().exists(), "it no longer waits");

        fixture.up.store(true, Ordering::SeqCst);
        wait_out_the_cooldown();
        scripts.list_scripts(&global_only(), None).expect("list");

        assert_eq!(local_body(&fixture, "waiting").as_deref(), Some("echo mine"));
        let mut team = SqliteStore::open(&fixture.team_file()).expect("team file");
        assert!(team.list_scripts(&global_only(), None).expect("list").is_empty());
    }

    #[test]
    fn a_script_kept_on_purpose_once_the_team_database_is_back_is_not_copied_first() {
        let fixture = Fixture::new();
        let mut scripts = shared(&fixture, Place::Remote);

        scripts
            .save_script(&Target::Global, &script("waiting", "", "echo agent"))
            .expect("saved here for now");
        assert!(fixture.pending().is_file());

        fixture.up.store(true, Ordering::SeqCst);
        wait_out_the_cooldown();

        scripts
            .save_script_to(Place::Local, &Target::Global, &script("waiting", "", "echo mine"))
            .expect("kept here");

        let mut team = SqliteStore::open(&fixture.team_file()).expect("team file");
        assert!(
            team.list_scripts(&global_only(), None).expect("list").is_empty(),
            "the old script was never copied to the team"
        );
        assert_eq!(local_body(&fixture, "waiting").as_deref(), Some("echo mine"));
        assert!(!fixture.pending().exists(), "it no longer waits");
    }
}

#[test]
fn a_team_database_that_answers_with_a_failure_is_not_offline_and_says_why() {
    let fixture = Fixture::new();
    let reach = Reach::beside(&fixture.local(), TEAM);
    let team: Lazy<dyn MemoryStore> =
        Lazy::classified(|| Err(StoreError::Backend("the migration needs a newer role".to_owned())));
    let local = Box::new(SqliteStore::open(&fixture.local()).expect("local file"));
    let mut store = SplitStore::routed(
        Box::new(team.with_reach(reach.clone())),
        local,
        NotesPlace::Remote,
        NotesPlace::Remote,
        None,
    )
    .with_offline_fallback(&fixture.local(), TEAM.to_owned(), Secrets::default());

    let error = save(&mut store, "kept nowhere").expect_err("the real reason");

    assert!(
        error.to_string().contains("the migration needs a newer role"),
        "{error}"
    );
    assert!(!store.offline(), "it can be reached, so it is not called unreachable");
    assert_eq!(reach.cooling(), None, "and no other process stops using it");
    assert!(
        fixture.local_titles("kept nowhere").is_empty(),
        "nothing is saved here instead"
    );
}

impl Fixture {
    /// The store with both scopes `remote+local`, saving new notes to `saves`.
    fn shared(&self, saves: Place) -> SplitStore {
        let local = Box::new(SqliteStore::open(&self.local()).expect("local file"));
        SplitStore::routed(
            self.team(),
            local,
            NotesPlace::RemoteLocal,
            NotesPlace::RemoteLocal,
            None,
        )
        .saving_to(saves)
        .with_offline_fallback(&self.local(), TEAM.to_owned(), Secrets::default())
    }

    fn updates(&self) -> PathBuf {
        self.dir.path().join("pending-updates.json")
    }
}

#[test]
fn a_remote_local_scope_saving_to_the_team_waits_offline_and_is_copied_when_it_is_back() {
    let fixture = Fixture::new();
    let mut store = fixture.shared(Place::Remote);

    save(&mut store, "written offline").expect("saved on this computer");
    assert!(store.offline());
    assert!(fixture.pending().is_file(), "the note waits to be copied");

    fixture.up.store(true, Ordering::SeqCst);
    wait_out_the_cooldown();

    titles(&mut store, "offline");
    assert!(!store.offline());
    assert_eq!(fixture.team_titles("offline"), ["written offline"]);
    assert!(!fixture.pending().exists(), "nothing waits any more");
    assert!(fixture.local_titles("offline").is_empty(), "and no copy is left here");

    // Back online a save goes straight to the team database and waits for nothing.
    save(&mut store, "written online").expect("saved");
    assert_eq!(fixture.team_titles("online"), ["written online"]);
    assert!(!fixture.pending().exists());
}

#[test]
fn a_remote_local_scope_saving_here_keeps_what_it_saves_offline_for_this_computer() {
    let fixture = Fixture::new();
    let mut store = fixture.shared(Place::Local);

    save(&mut store, "kept here").expect("saved");
    assert!(!fixture.pending().exists(), "this computer is where it was meant to go");

    fixture.up.store(true, Ordering::SeqCst);
    wait_out_the_cooldown();
    titles(&mut store, "kept");

    assert!(fixture.team_titles("kept").is_empty());
    assert_eq!(fixture.local_titles("kept"), ["kept here"]);
}

#[test]
fn a_note_a_person_keeps_here_is_not_copied_when_the_scope_saves_to_the_team() {
    let fixture = Fixture::new();
    let mut store = fixture.shared(Place::Remote);

    // Picked on purpose, offline and before: never listed as waiting.
    let target = Target::Global;
    store
        .remember_to(Place::Local, &target, &draft("kept on purpose", "mine", NoteKind::Note))
        .expect("kept here");
    assert!(store.offline(), "the team database is down");
    assert!(!fixture.pending().exists(), "a place a person picked waits for nothing");

    save(&mut store, "agent note").expect("saved offline");
    assert!(fixture.pending().is_file());

    fixture.up.store(true, Ordering::SeqCst);
    wait_out_the_cooldown();
    titles(&mut store, "note");

    assert_eq!(fixture.team_titles("agent"), ["agent note"]);
    assert!(
        fixture.team_titles("purpose").is_empty(),
        "the person's note stayed here"
    );
    assert_eq!(fixture.local_titles("purpose"), ["kept on purpose"]);
}

#[test]
fn a_change_to_a_team_note_waits_and_is_applied_when_the_scope_saves_to_the_team() {
    let fixture = Fixture::new();
    let team_note = SqliteStore::open(&fixture.team_file())
        .expect("team file")
        .remember(&Target::Global, &draft("team note", "the team's text", NoteKind::Note))
        .expect("team note")
        .id;

    let patch = NotePatch {
        append: Some("queued line".to_owned()),
        ..NotePatch::default()
    };
    let view = View::every_repo();

    // Scopes that save here keep the team's notes read-only: the change is refused.
    let mut here = fixture.shared(Place::Local);
    let refused = here.update(&view, team_note, &patch).expect_err("read-only");
    assert!(
        matches!(refused, StoreError::Unreachable(_) | StoreError::Invalid(_)),
        "{refused:?}"
    );
    assert!(!fixture.updates().exists());

    let mut store = fixture.shared(Place::Remote);
    let queued = store.update(&view, team_note, &patch).expect("queued");
    assert!(queued.queued, "{queued:?}");
    assert!(fixture.updates().is_file());

    fixture.up.store(true, Ordering::SeqCst);
    wait_out_the_cooldown();
    titles(&mut store, "note");

    let applied = SqliteStore::open(&fixture.team_file())
        .expect("team file")
        .peek(&view, &[team_note])
        .expect("peek")
        .remove(0);
    assert_eq!(applied.body, "the team's text\nqueued line");
    assert!(!fixture.updates().exists(), "nothing waits any more");
}

/// The body of the note titled `title` on this computer.
fn local_body(fixture: &Fixture, title: &str) -> Option<String> {
    SqliteStore::open(&fixture.local())
        .expect("local file")
        .find_by_title(&Target::Global, title)
        .expect("find")
        .map(|note| note.body)
}

#[test]
fn a_save_over_a_note_kept_here_updates_it_where_it_is_while_remote_local_saves_to_the_team_and_it_is_down() {
    let fixture = Fixture::new();
    let mut store = fixture.shared(Place::Remote);
    let target = Target::Global;

    store
        .remember_to(Place::Local, &target, &draft("own note", "mine", NoteKind::Note))
        .expect("kept here");

    let saved = store
        .remember(&target, &draft("own note", "changed", NoteKind::Note))
        .expect("updated here");
    assert!(store.offline());
    assert!(!saved.created && !saved.not_listed);
    assert!(!fixture.pending().exists(), "a note kept here waits for nothing");

    // A title that is not here is new: it waits for the team database.
    save(&mut store, "fresh").expect("saved here for now");
    assert!(fixture.pending().is_file());

    fixture.up.store(true, Ordering::SeqCst);
    wait_out_the_cooldown();
    titles(&mut store, "own");

    assert_eq!(local_body(&fixture, "own note").as_deref(), Some("changed"));
    assert_eq!(
        fixture.team_titles("own"),
        Vec::<String>::new(),
        "nothing was published"
    );
    assert_eq!(fixture.team_titles("fresh"), ["fresh"]);
    assert!(
        fixture.local_titles("fresh").is_empty(),
        "the new one was moved to the team"
    );
    assert!(!fixture.pending().exists());
}

#[test]
fn a_save_over_a_note_kept_here_updates_it_where_it_is_while_remote_local_saves_to_the_team_and_it_is_up() {
    let fixture = Fixture::new();
    fixture.up.store(true, Ordering::SeqCst);
    let mut store = fixture.shared(Place::Remote);
    let target = Target::Global;

    store
        .remember_to(Place::Local, &target, &draft("own note", "mine", NoteKind::Note))
        .expect("kept here");

    let saved = store
        .remember(&target, &draft("own note", "changed", NoteKind::Note))
        .expect("updated here");
    assert!(!saved.created);
    assert!(saved.id >= harness_memory::store::split::LOCAL_ID_OFFSET);
    assert_eq!(local_body(&fixture, "own note").as_deref(), Some("changed"));
    assert!(fixture.team_titles("own").is_empty(), "nothing was published");

    save(&mut store, "fresh").expect("saved to the team");
    assert_eq!(fixture.team_titles("fresh"), ["fresh"]);

    // A place a person picked still wins: the team database gets it under the same title.
    store
        .remember_to(Place::Remote, &target, &draft("own note", "shared", NoteKind::Note))
        .expect("saved to the team");
    assert_eq!(fixture.team_titles("own"), ["own note"]);
    assert_eq!(local_body(&fixture, "own note").as_deref(), Some("changed"));
}

#[test]
fn a_note_kept_on_purpose_leaves_the_list_of_what_waits_for_the_team_database() {
    let fixture = Fixture::new();
    let mut store = fixture.shared(Place::Remote);
    let target = Target::Global;

    save(&mut store, "waiting").expect("saved here for now");
    assert!(fixture.pending().is_file());

    store
        .remember_to(Place::Local, &target, &draft("waiting", "mine", NoteKind::Note))
        .expect("kept here");
    assert!(!fixture.pending().exists(), "it no longer waits");

    fixture.up.store(true, Ordering::SeqCst);
    wait_out_the_cooldown();
    titles(&mut store, "waiting");

    assert_eq!(local_body(&fixture, "waiting").as_deref(), Some("mine"));
    assert!(fixture.team_titles("waiting").is_empty());
}

#[test]
fn a_note_saved_offline_in_a_remote_scope_is_listed_even_when_it_replaced_one_kept_here() {
    let fixture = Fixture::new();
    SqliteStore::open(&fixture.local())
        .expect("local file")
        .remember(&Target::Global, &draft("leftover", "kept", NoteKind::Note))
        .expect("kept here");

    let mut store = fixture.store(NotesPlace::Remote, true);
    save(&mut store, "leftover").expect("saved on this computer");
    assert!(
        fixture.pending().is_file(),
        "a scope set to remote lists it all the same"
    );
}
