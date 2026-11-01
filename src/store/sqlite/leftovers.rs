//! The files of the code graph that versions before 3.0 left in the data folder. The code graph
//! moved to Harness Code (`harness-code`), so nothing here reads them any more.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use crate::support::Env;

/// The files the code graph kept next to the database.
const FILES: [&str; 6] = [
    "code.sqlite3",
    "code.sqlite3-wal",
    "code.sqlite3-shm",
    "code.sqlite3-journal",
    "code.toml",
    "code-worker.pid",
];

/// The folder of the code graph's build status files.
const BUILDS: &str = "code-builds";

/// Removes the leftovers from `folder` once per process (a process opens the same data folder
/// again and again). Best effort: an error is ignored.
pub(super) fn remove_once(folder: &Path) {
    static DONE: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

    let mut done = DONE.lock().unwrap_or_else(PoisonError::into_inner);

    if done.get_or_insert_with(HashSet::new).insert(folder.to_path_buf()) {
        remove(folder);
    }
}

/// Where Harness Code keeps its files when the user moved them (its `HARNESS_CODE_DIR`).
const CODE_DIR_VAR: &str = "HARNESS_CODE_DIR";

/// Written once the leftovers are gone, so the cleanup runs a single time per data folder and can
/// never reach files a Harness Code pointed at this folder writes later.
const DONE_MARKER: &str = "code-graph-moved";

/// Removes the leftover regular files of `folder`, and the build status folder with what is in it.
/// A link is never followed, and never removed. Nothing is removed when the cleanup already ran
/// here, or when Harness Code was pointed at this same folder, since the files are then its live
/// index and switch.
fn remove(folder: &Path) {
    let marker = folder.join(DONE_MARKER);

    if std::fs::symlink_metadata(&marker).is_ok() || shared_with_code(folder) {
        return;
    }

    for name in FILES {
        remove_file(&folder.join(name));
    }

    let builds = folder.join(BUILDS);

    // `remove_dir_all` never follows a link, also one swapped in after this check
    if is_folder(&builds) {
        std::fs::remove_dir_all(&builds).ok();
    }

    std::fs::write(&marker, b"").ok();
}

/// True when `HARNESS_CODE_DIR`, read as Harness Code reads it (`~` expanded, absolute only), is
/// `folder`.
fn shared_with_code(folder: &Path) -> bool {
    let Some(raw) = std::env::var_os(CODE_DIR_VAR) else {
        return false;
    };
    let Ok(env) = Env::from_process() else {
        return false;
    };
    let code = env.expand_home(&raw.to_string_lossy());

    code.is_absolute() && same_folder(&code, folder)
}

fn remove_file(path: &Path) {
    let is_file = std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file());

    if is_file {
        std::fs::remove_file(path).ok();
    }
}

fn same_folder(left: &Path, right: &Path) -> bool {
    match (dunce::canonicalize(left), dunce::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// True for a real folder, not a link to one.
fn is_folder(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

#[cfg(test)]
mod tests {
    use super::super::SqliteStore;
    use super::{remove, same_folder};

    const LEFTOVERS: [&str; 6] = [
        "code.sqlite3",
        "code.sqlite3-wal",
        "code.sqlite3-shm",
        "code.sqlite3-journal",
        "code.toml",
        "code-worker.pid",
    ];

    #[test]
    fn opening_the_database_removes_what_the_code_graph_left() {
        let data = tempfile::tempdir().expect("data");

        for name in LEFTOVERS {
            std::fs::write(data.path().join(name), "old").expect("leftover");
        }

        let builds = data.path().join("code-builds");
        std::fs::create_dir(&builds).expect("builds");
        std::fs::write(builds.join("0123456789abcdef.json"), "{}").expect("status");
        std::fs::write(data.path().join("notes.txt"), "mine").expect("other");

        SqliteStore::open(&data.path().join("memory.sqlite3")).expect("open");

        for name in LEFTOVERS {
            assert!(!data.path().join(name).exists(), "{name} is still there");
        }

        assert!(!builds.exists());
        assert!(data.path().join("notes.txt").exists());
        assert!(data.path().join("memory.sqlite3").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_build_folder_and_a_linked_file_are_left_alone() {
        let data = tempfile::tempdir().expect("data");
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        std::fs::write(elsewhere.path().join("keep.json"), "{}").expect("kept");
        std::os::unix::fs::symlink(elsewhere.path(), data.path().join("code-builds")).expect("folder link");
        std::os::unix::fs::symlink(elsewhere.path().join("keep.json"), data.path().join("code.toml"))
            .expect("file link");

        SqliteStore::open(&data.path().join("memory.sqlite3")).expect("open");

        assert!(elsewhere.path().join("keep.json").exists());
        assert!(data.path().join("code-builds").symlink_metadata().is_ok());
        assert!(data.path().join("code.toml").symlink_metadata().is_ok());
    }

    #[test]
    fn the_folder_harness_code_was_pointed_at_is_the_same_folder() {
        let data = tempfile::tempdir().expect("data");
        let other = tempfile::tempdir().expect("other");

        assert!(same_folder(data.path(), &data.path().join(".")));
        assert!(!same_folder(data.path(), other.path()));
        assert!(!same_folder(&data.path().join("missing"), data.path()));

        // without the variable the files go once, and a file written after the cleanup stays
        std::fs::write(data.path().join("code.toml"), "enabled = false").expect("switch");
        remove(data.path());
        assert!(!data.path().join("code.toml").exists());
        assert!(data.path().join("code-graph-moved").exists());

        std::fs::write(data.path().join("code.toml"), "enabled = false").expect("switch again");
        remove(data.path());
        assert!(data.path().join("code.toml").exists());
    }
}
