//! Team scripts hidden on this computer (`harness-memory scripts hide`), in `hidden_team_scripts`:
//! see the parent module.

use std::sync::atomic::AtomicBool;

use super::{Hidden, HiddenEntry, Kind, Table};
use crate::store::{ScriptStore, ScriptSummary, StoreError, View};

/// The tables, created when missing.
const CREATE: &str = "CREATE TABLE IF NOT EXISTS hidden_team_scripts (
    team TEXT NOT NULL,
    script_id INTEGER NOT NULL,
    scope TEXT NOT NULL CHECK (scope IN ('repo', 'global')),
    project TEXT,
    name TEXT NOT NULL,
    team_created_at INTEGER NOT NULL,
    hidden_at INTEGER NOT NULL,
    PRIMARY KEY (team, script_id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS hidden_team_scripts_meta (
    team TEXT PRIMARY KEY,
    pruned_at INTEGER NOT NULL
) WITHOUT ROWID";

static TABLE: Table = Table {
    name: "hidden_team_scripts",
    meta: "hidden_team_scripts_meta",
    id: "script_id",
    label: "name",
    noun: "script",
    create: CREATE,
    warned: AtomicBool::new(false),
};

/// Team scripts, as [`Hidden`] hides them.
#[derive(Debug)]
pub struct Scripts;

/// The team scripts of one team database hidden on this computer (the [`HiddenEntry::label`] of
/// each is the script's name).
pub type HiddenScripts = Hidden<Scripts>;

impl Kind for Scripts {
    type Item = ScriptSummary;
    type Store = dyn ScriptStore;

    fn table() -> &'static Table {
        &TABLE
    }

    fn entry(script: &ScriptSummary) -> HiddenEntry {
        HiddenEntry {
            id: script.id,
            scope: script.scope,
            project: script.project.clone(),
            label: script.name.clone(),
            team_created_at: script.created_at,
            hidden_at: 0,
        }
    }

    /// Lists the scripts of the view once.
    fn gone(team: &mut Self::Store, view: &View, entries: &[&HiddenEntry]) -> Result<Vec<i64>, StoreError> {
        let listed = team.list_scripts(view, None)?;

        Ok(entries
            .iter()
            .filter(|entry| {
                !listed
                    .iter()
                    .any(|script| script.id == entry.id && script.created_at == entry.team_created_at)
            })
            .map(|entry| entry.id)
            .collect())
    }
}
