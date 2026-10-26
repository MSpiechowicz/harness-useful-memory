//! The notes and scripts saved on this computer while the team database was unreachable.
//!
//! With the offline fallback (see [`crate::backend::Selection::offline_fallback`]) a scope set to
//! `remote` saves to this computer while the team database cannot be opened. Each of those notes
//! and scripts is listed in `pending-sync.json` next to the local database, as an [`Entry`] that
//! names the team database it was meant for and what the row held (its id, scope and title or
//! name), so that when the team database is back only those rows, and not what a person kept here
//! on purpose or a row that reuses a deleted row's id, are copied to it. The list is private to the
//! owner (`0600`), written through a unique temporary file, and one that cannot be read counts as
//! empty: nothing is copied by mistake, and the notes stay here (`harness-memory push` copies them).
//! Two servers changing the list at once may drop an entry for the same reason.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::support::private::{read_private, write_atomic};

/// The file name of the list, next to the local database.
const FILE: &str = "pending-sync.json";

/// The most bytes read from the list.
const MAX_BYTES: usize = 4 * 1024 * 1024;

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
}

impl Pending {
    /// The list kept next to the local database `database`.
    pub fn beside(database: &Path) -> Self {
        let path = database.with_file_name(FILE);
        let folder = path.parent().map(Path::to_path_buf).unwrap_or_default();

        Self { folder, path }
    }

    fn read(&self) -> List {
        read_private(&self.path, MAX_BYTES)
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn write(&self, list: &List) -> Result<(), String> {
        if list.notes.is_empty() && list.scripts.is_empty() {
            return match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(format!("removing the list of notes saved offline failed: {error}")),
            };
        }

        let bytes = serde_json::to_vec(list).map_err(|error| error.to_string())?;
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

    /// True when nothing is waiting to be copied.
    pub fn is_empty(&self) -> bool {
        let list = self.read();
        list.notes.is_empty() && list.scripts.is_empty()
    }

    /// Adds `entry` to the list of `kind`.
    pub fn add(&self, kind: Kind, entry: Entry) -> Result<(), String> {
        let mut list = self.read();

        if list.of(kind).insert(entry) {
            self.write(&list)?;
        }

        Ok(())
    }

    /// Takes `done` off the list of `kind` (the file goes when nothing is left).
    pub fn remove(&self, kind: Kind, done: &[Entry]) -> Result<(), String> {
        let mut list = self.read();
        let before = list.of(kind).len();
        list.of(kind).retain(|entry| !done.contains(entry));

        if list.of(kind).len() == before {
            return Ok(());
        }

        self.write(&list)
    }
    /// Gives the entry of `kind` for the local row `id` in `scope` the new name `name` (a note's title
    /// edited while still offline), so it keeps matching its row.
    pub fn rename(&self, kind: Kind, team: &str, id: i64, scope: &str, name: &str) -> Result<(), String> {
        let mut list = self.read();
        let found: Vec<Entry> = list
            .of(kind)
            .iter()
            .filter(|entry| entry.team == team && entry.id == id && entry.scope == scope)
            .cloned()
            .collect();

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

        self.write(&list)
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
