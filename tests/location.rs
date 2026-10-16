//! The one-time copy of Harness Desktop's database to the server's own folder.

mod common;

use std::path::{Path, PathBuf};

use common::{draft, global_only, query};
use harness_memory::location::{DIR_VAR, Migration, database_path, legacy_database_path, migrate_legacy};
use harness_memory::store::sqlite::schema::{SCHEMA_VERSION, user_version};
use harness_memory::store::{MemoryStore, NoteKind, SqliteStore, Target};
use harness_memory::support::{Env, OsKind};
use pretty_assertions::assert_eq;
use rusqlite::Connection;
use tempfile::TempDir;

/// A Linux environment whose data folder is inside `root` (the paths work on every OS).
fn env(root: &TempDir) -> Env {
    Env::for_tests(root.path().join("home"))
        .with_os(OsKind::Linux)
        .with_var("XDG_DATA_HOME", root.path().join("data").to_string_lossy())
}

fn paths(env: &Env) -> (PathBuf, PathBuf) {
    let new = database_path(env).expect("database path");
    let legacy = legacy_database_path(env).expect("legacy path");
    (new, legacy)
}

fn save_global_note(store: &mut SqliteStore, title: &str) {
    store
        .remember(
            &Target::Global,
            &draft(title, "kept across the move", NoteKind::Decision),
        )
        .expect("remember");
}

fn titles(path: &Path) -> Vec<String> {
    let mut store = SqliteStore::open(path).expect("store");
    let hits = store.search(&query("move", global_only())).expect("search");
    hits.into_iter().map(|hit| hit.title).collect()
}

#[test]
fn the_legacy_database_is_copied_with_its_notes_and_left_as_it_was() {
    let root = TempDir::new().expect("temp");
    let env = env(&root);
    let (new, legacy) = paths(&env);

    // A writer still holds the legacy file: the second note is only in its write-ahead log.
    let mut writer = SqliteStore::open(&legacy).expect("legacy store");
    save_global_note(&mut writer, "Checkpointed note");
    let wal = Connection::open(&legacy).expect("raw connection");
    wal.pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .expect("checkpoint");
    save_global_note(&mut writer, "Logged note");
    let legacy_bytes = std::fs::read(&legacy).expect("legacy bytes");

    assert_eq!(migrate_legacy(&env, &new), Migration::Copied);

    let copied = Connection::open(&new).expect("copy");
    assert_eq!(user_version(&copied).expect("version"), SCHEMA_VERSION);
    drop(copied);

    let mut found = titles(&new);
    found.sort();
    assert_eq!(found, ["Checkpointed note", "Logged note"]);
    assert_eq!(std::fs::read(&legacy).expect("legacy bytes"), legacy_bytes);
    assert_eq!(titles(&legacy).len(), 2);

    let leftovers: Vec<_> = std::fs::read_dir(new.parent().expect("folder"))
        .expect("list")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("migrating"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = std::fs::metadata(&new).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn an_existing_database_is_never_replaced() {
    let root = TempDir::new().expect("temp");
    let env = env(&root);
    let (new, legacy) = paths(&env);

    save_global_note(&mut SqliteStore::open(&legacy).expect("legacy"), "Legacy move");
    save_global_note(&mut SqliteStore::open(&new).expect("new"), "Current move");

    assert_eq!(migrate_legacy(&env, &new), Migration::NotNeeded);
    assert_eq!(titles(&new), ["Current move"]);
}

#[test]
fn without_a_legacy_database_the_server_starts_fresh() {
    let root = TempDir::new().expect("temp");
    let env = env(&root);
    let (new, _) = paths(&env);

    assert_eq!(migrate_legacy(&env, &new), Migration::NotNeeded);
    assert!(!new.exists());
    assert!(titles(&new).is_empty());
}

#[test]
fn the_folder_override_skips_the_legacy_database() {
    let root = TempDir::new().expect("temp");
    let (_, legacy) = paths(&env(&root));
    save_global_note(&mut SqliteStore::open(&legacy).expect("legacy"), "Legacy move");

    let env = env(&root).with_var(DIR_VAR, root.path().join("own").to_string_lossy());
    let new = database_path(&env).expect("database path");

    assert_eq!(new, root.path().join("own").join("memory.sqlite3"));
    assert_eq!(migrate_legacy(&env, &new), Migration::NotNeeded);
    assert!(titles(&new).is_empty());
}

#[test]
fn an_unreadable_legacy_file_fails_without_touching_anything() {
    let root = TempDir::new().expect("temp");
    let env = env(&root);
    let (new, legacy) = paths(&env);
    std::fs::create_dir_all(legacy.parent().expect("folder")).expect("mkdir");
    std::fs::write(
        &legacy,
        "not a database, just text that is long enough to have a header",
    )
    .expect("write");

    assert!(matches!(migrate_legacy(&env, &new), Migration::Failed(_)));
    assert!(!new.exists());
    assert_eq!(
        std::fs::read_to_string(&legacy).expect("legacy"),
        "not a database, just text that is long enough to have a header"
    );
    assert!(titles(&new).is_empty());
}
