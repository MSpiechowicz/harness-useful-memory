//! The notes and scripts saved on this computer while the team database was unreachable.
//!
//! With the offline fallback (see [`crate::backend::Selection::offline_fallback`]) a scope set to
//! `remote` saves to this computer while the team database cannot be opened. Each of those notes
//! and scripts is listed in `pending-sync.json` next to the local database, as an [`Entry`] that
//! names the team database it was meant for and what the row held (its id, scope and title or
//! name), so that when the team database is back only those rows, and not what a person kept here
//! on purpose or a row that reuses a deleted row's id, are copied to it. The list is private to the
//! owner (`0600`), written through a unique temporary file, and one that cannot be read counts as
//! empty when only looked at: nothing is copied by mistake, and the notes stay here (`harness-memory
//! push` copies them). `discard-local` must not take an unreadable list for an empty one, since that
//! would delete what still waits to be copied: it reads strictly ([`Pending::entries_strict`]) and
//! stops. A change (`add`, `remove`, `rename`) reads strictly too: a list that cannot be read is
//! left in place and the change fails, so it is never replaced by a small new list that a later
//! `discard-local` would take for the whole of what waits.
//! Every change reads the list, changes it and writes it back under an advisory lock on
//! `pending-sync.json.lock` ([`FileLock`]), so two servers changing the list at once lose no entry.
//! Reading takes no lock: a write replaces the file whole. A change that grows the list stops
//! below the size read ([`MAX_WRITE_BYTES`]) and fails with the reason, so the list stays readable and
//! can always be shortened.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::support::private::{FileLock, read_private, write_atomic};

/// The file name of the list, next to the local database.
const FILE: &str = "pending-sync.json";

/// The most bytes read from the list.
const MAX_BYTES: usize = 4 * 1024 * 1024;

/// The most bytes written to it by a change that grows it, below [`MAX_BYTES`] so that what was
/// written can always be read, and so can be shortened again.
const MAX_WRITE_BYTES: usize = 3 * 1024 * 1024;

/// What a save is told to say, in every reply that reports it, when the note or script stayed on
/// this computer but could not be listed as waiting for the team database.
pub const NOT_LISTED_MESSAGE: &str = "saved on this computer, but could not be listed to sync automatically";

/// Adds `"notListed": true` and the [`NOT_LISTED_MESSAGE`] as `"message"` to the JSON object `shown`
/// that reports a save.
pub fn mark_not_listed(shown: &mut serde_json::Value) {
    shown["notListed"] = serde_json::json!(true);
    shown["message"] = serde_json::json!(NOT_LISTED_MESSAGE);
}

/// What is saved offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Notes,
    Scripts,
}

/// One note or script saved offline and waiting to be copied.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Entry {
    /// The team database it is meant for ([`crate::backend::Backend::team_key`]).
    pub team: String,
    /// The row's id in the local database.
    pub id: i64,
    /// The repository key, or `global`.
    pub scope: String,
    /// The note's title or the script's name.
    pub name: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct List {
    #[serde(default)]
    notes: BTreeSet<Entry>,
    #[serde(default)]
    scripts: BTreeSet<Entry>,
}

impl List {
    fn of(&mut self, kind: Kind) -> &mut BTreeSet<Entry> {
        match kind {
            Kind::Notes => &mut self.notes,
            Kind::Scripts => &mut self.scripts,
        }
    }
}

/// The list of what was saved offline, kept in a file.
#[derive(Debug, Clone)]
pub struct Pending {
    folder: PathBuf,
    path: PathBuf,
    /// The most bytes a change that grows the list writes ([`MAX_WRITE_BYTES`]).
    limit: usize,
}

impl Pending {
    /// The list kept next to the local database `database`.
    pub fn beside(database: &Path) -> Self {
        let path = database.with_file_name(FILE);
        let folder = path.parent().map(Path::to_path_buf).unwrap_or_default();

        Self {
            folder,
            path,
            limit: MAX_WRITE_BYTES,
        }
    }

    /// Holds the lock of the list while a change reads it and writes it back.
    fn locked(&self) -> Result<FileLock, String> {
        FileLock::exclusive(&self.path, "list of notes saved offline")
    }

    /// The list; empty when there is none. Why not when the file exists but cannot be read: a
    /// link, another owner or mode, over [`MAX_BYTES`], or not a list.
    fn read_strict(&self) -> Result<List, String> {
        let Some(bytes) = read_private(&self.path, MAX_BYTES)? else {
            return Ok(List::default());
        };

        serde_json::from_slice(&bytes).map_err(|error| format!("{FILE} is not a list of notes saved offline: {error}"))
    }

    /// The list, empty when it cannot be read (see [`Pending::entries_strict`] for the caller that
    /// must tell the two apart). Changes read strictly instead.
    fn read(&self) -> List {
        self.read_strict().unwrap_or_default()
    }

    /// Writes `list`, refusing more than `limit` bytes: a change that grows the list stays below
    /// [`MAX_BYTES`], so the list can always be read and shortened again.
    fn write_within(&self, list: &List, limit: usize) -> Result<(), String> {
        if list.notes.is_empty() && list.scripts.is_empty() {
            return match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(format!("removing the list of notes saved offline failed: {error}")),
            };
        }

        let bytes = serde_json::to_vec(list).map_err(|error| error.to_string())?;

        if bytes.len() > limit {
            return Err(format!(
                "too many notes and scripts are waiting for the team database to list another ({} MB at most): \
                 connect to it so they are copied, then save it again",
                limit.div_ceil(1024 * 1024)
            ));
        }

        write_atomic(
            &self.folder,
            &self.path,
            &bytes,
            "pending-sync-",
            "list of notes saved offline",
        )
    }

    /// The entries of `kind` meant for the team database `team`.
    pub fn entries(&self, kind: Kind, team: &str) -> Vec<Entry> {
        self.read()
            .of(kind)
            .iter()
            .filter(|entry| entry.team == team)
            .cloned()
            .collect()
    }

    /// Like [`Pending::entries`], but a list that exists and cannot be read is an error with the
    /// reason, not an empty list. For what must never act on a list it could not read.
    pub fn entries_strict(&self, kind: Kind, team: &str) -> Result<Vec<Entry>, String> {
        let mut list = self.read_strict()?;

        Ok(list
            .of(kind)
            .iter()
            .filter(|entry| entry.team == team)
            .cloned()
            .collect())
    }

    /// True when nothing is waiting to be copied.
    pub fn is_empty(&self) -> bool {
        let list = self.read();
        list.notes.is_empty() && list.scripts.is_empty()
    }

    /// Adds `entry` to the list of `kind`. A list that exists and cannot be read is left as it is
    /// and is the error: replacing it would make a later `discard-local` take a small new list for
    /// the whole of what waits.
    pub fn add(&self, kind: Kind, entry: Entry) -> Result<(), String> {
        let _lock = self.locked()?;
        let mut list = self.read_strict()?;

        if list.of(kind).insert(entry) {
            self.write_within(&list, self.limit)?;
        }

        Ok(())
    }

    /// Takes `done` off the list of `kind` (the file goes when nothing is left).
    pub fn remove(&self, kind: Kind, done: &[Entry]) -> Result<(), String> {
        let _lock = self.locked()?;
        let mut list = self.read_strict()?;
        let before = list.of(kind).len();
        list.of(kind).retain(|entry| !done.contains(entry));

        if list.of(kind).len() == before {
            return Ok(());
        }

        self.write_within(&list, MAX_BYTES)
    }

    /// Takes the entries of `kind` for the local row `id` in `scope`, meant for the team database
    /// `team`, off the list, whatever name they hold: a row a person keeps here on purpose waits for
    /// nothing. A list that cannot be read has nothing copied from it, so it is left alone.
    pub fn forget(&self, kind: Kind, team: &str, id: i64, scope: &str) -> Result<(), String> {
        let matches = |entry: &Entry| entry.team == team && entry.id == id && entry.scope == scope;

        if !self.read().of(kind).iter().any(matches) {
            return Ok(());
        }

        let _lock = self.locked()?;
        let mut list = self.read_strict()?;
        let before = list.of(kind).len();
        list.of(kind).retain(|entry| !matches(entry));

        if list.of(kind).len() == before {
            return Ok(());
        }

        self.write_within(&list, MAX_BYTES)
    }

    /// Gives the entry of `kind` for the local row `id` in `scope` the new name `name` (a note's title
    /// edited while still offline), so it keeps matching its row.
    pub fn rename(&self, kind: Kind, team: &str, id: i64, scope: &str, name: &str) -> Result<(), String> {
        let matches = |entry: &Entry| entry.team == team && entry.id == id && entry.scope == scope;

        // Nothing listed for the row: no lock to take, and no file to touch. A list that cannot be
        // read is the error, as for any change.
        if !self.read_strict()?.of(kind).iter().any(matches) {
            return Ok(());
        }

        let _lock = self.locked()?;
        let mut list = self.read_strict()?;
        let found: Vec<Entry> = list.of(kind).iter().filter(|entry| matches(entry)).cloned().collect();

        if found.is_empty() {
            return Ok(());
        }

        for entry in found {
            list.of(kind).remove(&entry);
            list.of(kind).insert(Entry {
                name: name.to_owned(),
                ..entry
            });
        }

        self.write_within(&list, self.limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(team: &str, id: i64, name: &str) -> Entry {
        Entry {
            team: team.to_owned(),
            id,
            scope: "global".to_owned(),
            name: name.to_owned(),
        }
    }

    #[test]
    fn entries_are_kept_per_kind_and_team_and_the_file_goes_with_the_last_one() {
        let dir = tempfile::tempdir().expect("dir");
        let pending = Pending::beside(&dir.path().join("memory.sqlite3"));
        assert!(pending.is_empty());

        pending.add(Kind::Notes, entry("a", 4, "one")).expect("add");
        pending.add(Kind::Notes, entry("a", 4, "one")).expect("again");
        pending.add(Kind::Notes, entry("b", 5, "other team")).expect("add");
        pending.add(Kind::Scripts, entry("a", 4, "tool")).expect("script");

        assert_eq!(pending.entries(Kind::Notes, "a"), [entry("a", 4, "one")]);
        assert_eq!(pending.entries(Kind::Notes, "b"), [entry("b", 5, "other team")]);
        assert_eq!(pending.entries(Kind::Scripts, "a"), [entry("a", 4, "tool")]);

        pending
            .remove(Kind::Notes, &[entry("a", 4, "one"), entry("b", 5, "other team")])
            .expect("remove");
        assert!(pending.entries(Kind::Notes, "a").is_empty());
        assert!(!pending.is_empty(), "the script is still waiting");

        pending.remove(Kind::Scripts, &[entry("a", 4, "tool")]).expect("remove");
        assert!(pending.is_empty());
        assert!(!dir.path().join(FILE).exists());
    }

    #[test]
    fn a_list_that_would_pass_its_cap_refuses_the_entry_and_stays_readable_and_shrinkable() {
        let dir = tempfile::tempdir().expect("dir");
        let mut pending = Pending::beside(&dir.path().join("memory.sqlite3"));
        pending.add(Kind::Notes, entry("a", 1, "one")).expect("add");
        pending.add(Kind::Notes, entry("a", 2, "two")).expect("add");

        // Room for the two entries and not for a third.
        let size = std::fs::metadata(dir.path().join(FILE)).expect("metadata").len();
        pending.limit = usize::try_from(size).expect("size") + 10;

        let refused = pending.add(Kind::Notes, entry("a", 3, "three")).expect_err("full");
        assert!(refused.contains("too many notes and scripts"), "{refused}");
        assert!(
            pending.rename(Kind::Notes, "a", 1, "global", &"x".repeat(100)).is_err(),
            "rename"
        );

        // What was written stays readable, and the list can still shrink.
        assert_eq!(
            pending.entries_strict(Kind::Notes, "a").expect("readable"),
            [entry("a", 1, "one"), entry("a", 2, "two")]
        );
        pending.remove(Kind::Notes, &[entry("a", 1, "one")]).expect("remove");
        assert_eq!(pending.entries_strict(Kind::Notes, "a"), Ok(vec![entry("a", 2, "two")]));

        // A removal is allowed up to the size that is read, even from a list past the write cap.
        pending.limit = 1;
        pending.remove(Kind::Notes, &[entry("a", 2, "two")]).expect("last");
        assert!(pending.is_empty());
    }

    #[test]
    fn an_entry_needs_the_same_id_and_name_to_match_a_row() {
        // A row that reuses a deleted row's id is another entry, so it is never taken for it.
        assert_ne!(entry("a", 7, "deleted note"), entry("a", 7, "private note"));
    }

    #[test]
    fn a_list_that_cannot_be_read_counts_as_empty() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join(FILE), "not json").expect("write");
        let pending = Pending::beside(&dir.path().join("memory.sqlite3"));

        assert!(pending.is_empty());
        assert!(pending.entries(Kind::Notes, "a").is_empty());
    }

    #[test]
    fn a_strict_read_tells_an_unreadable_list_from_none() {
        let dir = tempfile::tempdir().expect("dir");
        let pending = Pending::beside(&dir.path().join("memory.sqlite3"));
        assert_eq!(pending.entries_strict(Kind::Notes, "a"), Ok(Vec::new()), "no list");

        std::fs::write(dir.path().join(FILE), "not json").expect("write");
        assert!(pending.entries_strict(Kind::Notes, "a").is_err(), "malformed");

        std::fs::remove_file(dir.path().join(FILE)).expect("remove");
        pending.add(Kind::Notes, entry("a", 4, "one")).expect("add");
        assert_eq!(pending.entries_strict(Kind::Notes, "a"), Ok(vec![entry("a", 4, "one")]));
        assert!(pending.entries_strict(Kind::Notes, "b").expect("other team").is_empty());
    }

    #[test]
    fn a_change_leaves_a_list_that_cannot_be_read_in_place_and_fails() {
        let dir = tempfile::tempdir().expect("dir");
        let list = dir.path().join(FILE);
        std::fs::write(&list, "not json").expect("write");
        let pending = Pending::beside(&dir.path().join("memory.sqlite3"));

        assert!(pending.add(Kind::Notes, entry("a", 4, "one")).is_err(), "add");
        assert!(pending.remove(Kind::Notes, &[entry("a", 4, "one")]).is_err(), "remove");
        assert!(pending.rename(Kind::Notes, "a", 4, "global", "new").is_err(), "rename");

        assert_eq!(
            std::fs::read_to_string(&list).expect("read"),
            "not json",
            "left as it was"
        );
        assert!(pending.entries_strict(Kind::Notes, "a").is_err(), "still unreadable");
    }

    #[cfg(unix)]
    #[test]
    fn a_strict_read_refuses_a_link_and_a_list_others_can_write() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("dir");
        let pending = Pending::beside(&dir.path().join("memory.sqlite3"));
        pending.add(Kind::Notes, entry("a", 4, "one")).expect("add");

        let list = dir.path().join(FILE);
        std::fs::set_permissions(&list, std::fs::Permissions::from_mode(0o666)).expect("chmod");
        assert!(pending.entries_strict(Kind::Notes, "a").is_err(), "open mode");

        std::fs::set_permissions(&list, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let elsewhere = dir.path().join("elsewhere.json");
        std::fs::rename(&list, &elsewhere).expect("move");
        std::os::unix::fs::symlink(&elsewhere, &list).expect("link");
        assert!(pending.entries_strict(Kind::Notes, "a").is_err(), "link");
    }

    #[test]
    fn servers_adding_at_once_lose_no_entry() {
        let dir = tempfile::tempdir().expect("dir");
        let database = dir.path().join("memory.sqlite3");

        let threads: Vec<_> = (0..4_i64)
            .map(|thread| {
                let database = database.clone();

                std::thread::spawn(move || {
                    let pending = Pending::beside(&database);

                    for id in 0..25_i64 {
                        let entry = entry("a", thread * 100 + id, "x");
                        pending.add(Kind::Notes, entry).expect("add");
                    }
                })
            })
            .collect();

        for thread in threads {
            thread.join().expect("thread");
        }

        let pending = Pending::beside(&database);
        assert_eq!(pending.entries(Kind::Notes, "a").len(), 100, "every entry is listed");
    }

    #[cfg(unix)]
    #[test]
    fn the_list_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("dir");
        let pending = Pending::beside(&dir.path().join("memory.sqlite3"));
        pending.add(Kind::Notes, entry("a", 1, "x")).expect("add");

        let mode = std::fs::metadata(dir.path().join(FILE))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
