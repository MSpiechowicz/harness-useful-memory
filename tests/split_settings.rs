//! The English notes setting through the stores that wrap others: a split store keeps it in the
//! team database when a scope saves there and in the local file otherwise, and a lazily opened
//! store passes it on.

use std::path::Path;

use harness_memory::backend::NotesPlace;
use harness_memory::store::{HiddenNotes, Lazy, MemoryStore, SplitStore, SqliteStore};
use pretty_assertions::assert_eq;
use tempfile::TempDir;

/// A split store over `team.sqlite3` and `local.sqlite3` in `dir`, routed as given.
fn split(dir: &Path, repo: NotesPlace, global: NotesPlace) -> SplitStore {
    let team = SqliteStore::open(&dir.join("team.sqlite3")).expect("team");
    let local = SqliteStore::open(&dir.join("local.sqlite3")).expect("local");

    SplitStore::routed(Box::new(team), Box::new(local), repo, global, None::<HiddenNotes>)
}

fn setting(dir: &Path, file: &str) -> bool {
    SqliteStore::open(&dir.join(file))
        .expect("open")
        .english_notes()
        .expect("read")
}

#[test]
fn a_setting_for_notes_saved_in_the_team_database_lives_there() {
    let dir = TempDir::new().expect("dir");
    let mut store = split(dir.path(), NotesPlace::Remote, NotesPlace::Remote);
    assert!(store.english_notes().expect("default"));

    store.set_english_notes(false).expect("set");

    assert!(!store.english_notes().expect("read"));
    assert_eq!(
        (
            setting(dir.path(), "team.sqlite3"),
            setting(dir.path(), "local.sqlite3")
        ),
        (false, true)
    );
}

#[test]
fn a_setting_for_notes_saved_on_this_computer_lives_there() {
    let dir = TempDir::new().expect("dir");

    for place in [NotesPlace::Local, NotesPlace::RemoteLocal] {
        let mut store = split(dir.path(), place, place);
        store.set_english_notes(false).expect("set");

        assert!(!store.english_notes().expect("read"));
        assert_eq!(
            (
                setting(dir.path(), "team.sqlite3"),
                setting(dir.path(), "local.sqlite3")
            ),
            (true, false)
        );

        store.set_english_notes(true).expect("reset");
    }
}

#[test]
fn a_setting_is_written_to_both_stores_when_the_scopes_save_in_both() {
    let dir = TempDir::new().expect("dir");
    let mut store = split(dir.path(), NotesPlace::Remote, NotesPlace::Local);

    store.set_english_notes(false).expect("set");

    assert_eq!(
        (
            setting(dir.path(), "team.sqlite3"),
            setting(dir.path(), "local.sqlite3")
        ),
        (false, false)
    );
}

#[test]
fn a_lazy_store_passes_the_setting_on() {
    let dir = TempDir::new().expect("dir");
    let path = dir.path().join("memory.sqlite3");
    let mut lazy: Lazy<dyn MemoryStore> = Lazy::new(move || {
        SqliteStore::open(&path)
            .map(|store| Box::new(store) as Box<dyn MemoryStore>)
            .map_err(|error| error.to_string())
    });

    assert!(lazy.english_notes().expect("default"));
    lazy.set_english_notes(false).expect("set");

    assert!(!lazy.english_notes().expect("read"));
    assert_eq!(setting(dir.path(), "memory.sqlite3"), false);
}
