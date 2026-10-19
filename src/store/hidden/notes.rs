//! Team notes hidden on this computer (`harness-memory notes hide`), in `hidden_team_notes`: see
//! the parent module.

use std::sync::atomic::AtomicBool;

use super::{Hidden, HiddenEntry, Kind, Table};
use crate::limits::GET_IDS_MAX;
use crate::store::{MemoryStore, Note, StoreError, View};

/// The tables, created when missing.
const CREATE: &str = "CREATE TABLE IF NOT EXISTS hidden_team_notes (
    team TEXT NOT NULL,
    note_id INTEGER NOT NULL,
    scope TEXT NOT NULL CHECK (scope IN ('repo', 'global')),
    project TEXT,
    title TEXT NOT NULL,
    team_created_at INTEGER NOT NULL,
    hidden_at INTEGER NOT NULL,
    PRIMARY KEY (team, note_id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS hidden_team_notes_meta (
    team TEXT PRIMARY KEY,
    pruned_at INTEGER NOT NULL
) WITHOUT ROWID";

static TABLE: Table = Table {
    name: "hidden_team_notes",
    meta: "hidden_team_notes_meta",
    id: "note_id",
    label: "title",
    noun: "note",
    create: CREATE,
    warned: AtomicBool::new(false),
};

/// Team notes, as [`Hidden`] hides them.
#[derive(Debug)]
pub struct Notes;

/// The team notes of one team database hidden on this computer.
pub type HiddenNotes = Hidden<Notes>;

/// A hidden team note (its [`HiddenEntry::label`] is the title).
pub type HiddenNote = HiddenEntry;

impl Kind for Notes {
    type Item = Note;
    type Store = dyn MemoryStore;

    fn table() -> &'static Table {
        &TABLE
    }

    fn entry(note: &Note) -> HiddenEntry {
        HiddenEntry {
            id: note.id,
            scope: note.scope,
            project: note.project.clone(),
            label: note.title.clone(),
            team_created_at: note.created_at,
            hidden_at: 0,
        }
    }

    /// Reads the notes [`GET_IDS_MAX`] at a time, without counting a hit.
    fn gone(team: &mut Self::Store, view: &View, entries: &[&HiddenEntry]) -> Result<Vec<i64>, StoreError> {
        let mut gone = Vec::new();

        for chunk in entries.chunks(GET_IDS_MAX) {
            let ids: Vec<i64> = chunk.iter().map(|entry| entry.id).collect();
            let notes = team.peek(view, &ids)?;

            for entry in chunk {
                let same = notes
                    .iter()
                    .any(|note| note.id == entry.id && note.created_at == entry.team_created_at);

                if !same {
                    gone.push(entry.id);
                }
            }
        }

        Ok(gone)
    }
}
