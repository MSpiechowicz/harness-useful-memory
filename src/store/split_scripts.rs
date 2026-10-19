//! Scripts read from the team database and this computer, as `scripts` says.
//!
//! [`SplitScripts`] serves the script tools and `harness-memory scripts` whenever scripts are read
//! from the team database ([`NotesPlace::Remote`] and [`NotesPlace::RemoteLocal`], see
//! [`crate::backend::Routing::scripts`]); `local` scripts use the local store alone. Each side is
//! opened on its first use, so with `remote` the local file is never opened.
//!
//! - **`remote`** reads and writes the team database, under the team's ids.
//! - **`remote+local`** reads both and saves new scripts here. The same name may be in both
//!   stores: both are listed, and a name is looked up on this computer first (this computer's
//!   repository scripts, the team's, this computer's global ones, the team's). The team's scripts
//!   are read-only: deleting one is refused ([`read_only_error`], naming `harness-memory scripts
//!   hide`), deleting many leaves them. A run of one is still counted in the team database.
//!
//! **Ids.** Like [`crate::store::SplitStore`], this computer's scripts are shown with
//! [`crate::store::split::LOCAL_ID_OFFSET`] added (local script 7 is script 1000000007), the
//! team's as they are. A list shows the repository scripts first, then by name, this computer's
//! before the team's.
//!
//! **Hidden team scripts.** With [`HiddenScripts`] (read on every call) the team scripts hidden on
//! this computer are left out of every read, in `remote` and `remote+local` alike: a hidden id or
//! name is not found and lists leave it out (only [`ScriptStore::hidden_scripts`] lists them, for
//! checks such as whether saved code is a waiting script's). A read that cannot read the hidden
//! list hides nothing, but every change to the team's scripts (saving, deleting one or many) fails
//! instead ([`HiddenScripts::try_ids`]). Saving a script in `remote` under the name of a hidden team script
//! is refused ([`hidden_name_error`]) rather than changing the script nobody sees. At most once per
//! [`crate::store::hidden::PRUNE_EVERY`] the rows of team scripts that are gone, or whose id the
//! team database gave to a newer script, are dropped ([`HiddenScripts::prune_if_due`]).

use crate::backend::{NotesPlace, Place};
use crate::store::split::{Side, decode, encode_as, narrow_by, target_scope};
use crate::store::{
    HiddenScripts, SavedScript, Scope, Script, ScriptDraft, ScriptOrigin, ScriptRef, ScriptStore, ScriptSummary,
    StoreError, Target, View,
};

/// The team database's scripts and this computer's, as `scripts` routes them. See the module docs.
pub struct SplitScripts {
    team: Box<dyn ScriptStore>,
    local: Box<dyn ScriptStore>,
    place: NotesPlace,
    hidden: Option<HiddenScripts>,
}

impl std::fmt::Debug for SplitScripts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SplitScripts")
            .field("place", &self.place)
            .field("hidden", &self.hidden)
            .finish_non_exhaustive()
    }
}

impl SplitScripts {
    /// Reads and writes `team` and `local` as `place` says, leaving out the team scripts `hidden`
    /// hides (none without it).
    pub fn new(
        team: Box<dyn ScriptStore>,
        local: Box<dyn ScriptStore>,
        place: NotesPlace,
        hidden: Option<HiddenScripts>,
    ) -> Self {
        Self {
            team,
            local,
            place,
            hidden,
        }
    }

    /// True when the store `side` is read.
    fn reads(&self, side: Side) -> bool {
        match side {
            Side::Team => self.place.reads_team(),
            Side::Local => self.place.reads_local(),
        }
    }

    /// The store new scripts are saved to.
    fn write_side(&self) -> Side {
        match self.place.writes() {
            Place::Remote => Side::Team,
            Place::Local => Side::Local,
        }
    }

    /// True when the team's scripts cannot be changed from here (`remote+local`).
    fn team_read_only(&self) -> bool {
        self.place == NotesPlace::RemoteLocal
    }

    fn store(&mut self, side: Side) -> &mut dyn ScriptStore {
        match side {
            Side::Team => self.team.as_mut(),
            Side::Local => self.local.as_mut(),
        }
    }

    /// The ids of the team scripts hidden on this computer (none without [`HiddenScripts`] or when
    /// the list cannot be read), the list pruned first when it is due.
    fn hidden_ids(&mut self) -> Vec<i64> {
        let Some(hidden) = self.hidden.as_mut() else {
            return Vec::new();
        };

        // A team database that cannot be read fails the call itself, so a failed prune waits for
        // the next turn.
        let _ = hidden.prune_if_due(self.team.as_mut());

        hidden.ids()
    }

    /// The ids [`Self::hidden_ids`] reads, for a change to the team's scripts: an error when the
    /// hidden list cannot be read, rather than none hidden.
    fn hidden_ids_strict(&mut self) -> Result<Vec<i64>, StoreError> {
        let Some(hidden) = self.hidden.as_mut() else {
            return Ok(Vec::new());
        };

        let _ = hidden.prune_if_due(self.team.as_mut());
        hidden.try_ids()
    }

    /// Refuses to save `name` in the team scope `target` when a team script with that name is
    /// hidden on this computer (saving would change it unseen).
    fn check_hidden_name(&mut self, target: &Target, name: &str) -> Result<(), StoreError> {
        let hidden = self.hidden_ids_strict()?;

        if hidden.is_empty() {
            return Ok(());
        }

        let scope = target_scope(target);
        let Some(view) = narrow_by(&View::around(target), |kept| kept == scope) else {
            return Ok(());
        };

        match self.team.get_script(&view, &ScriptRef::Name(name.trim().to_owned()))? {
            Some(script) if hidden.contains(&script.summary.id) => Err(hidden_name_error(script.summary.id)),
            _ => Ok(()),
        }
    }

    /// The script `name` names in `view`, looked up on this computer first: its repository scripts,
    /// the team's, its global ones, the team's.
    fn find_by_name(&mut self, view: &View, name: &str) -> Result<Option<Script>, StoreError> {
        let hidden = self.hidden_ids();
        let wanted = ScriptRef::Name(name.to_owned());

        for scope in [Scope::Repo, Scope::Global] {
            let Some(narrowed) = narrow_by(view, |kept| kept == scope) else {
                continue;
            };

            for side in [Side::Local, Side::Team] {
                if !self.reads(side) {
                    continue;
                }

                let Some(script) = self.store(side).get_script(&narrowed, &wanted)? else {
                    continue;
                };

                if side == Side::Team && hidden.contains(&script.summary.id) {
                    continue;
                }

                return shown(side, script).map(Some);
            }
        }

        Ok(None)
    }

    /// The script shown as `id`, when its store is read, it is visible in `view` and not hidden.
    fn find_by_id(&mut self, view: &View, id: i64) -> Result<Option<Script>, StoreError> {
        let (side, raw) = decode(id);

        if !self.reads(side) {
            return Ok(None);
        }

        if side == Side::Team && self.hidden_ids().contains(&raw) {
            return Ok(None);
        }

        match self.store(side).get_script(view, &ScriptRef::Id(raw))? {
            Some(script) => shown(side, script).map(Some),
            None => Ok(None),
        }
    }
}

/// `script` of the store `side` with the id it is shown as.
fn shown(side: Side, script: Script) -> Result<Script, StoreError> {
    let id = encode_as(side, script.summary.id, "script")?;

    Ok(Script {
        summary: ScriptSummary { id, ..script.summary },
        ..script
    })
}

/// The order of a merged list: repository scripts first, then by name, this computer's before the
/// team's.
fn listed_order(summary: &ScriptSummary) -> (bool, String, bool) {
    let team = decode(summary.id).0 == Side::Team;
    (summary.scope == Scope::Global, summary.name.clone(), team)
}

/// The refusal to save a script under the name of the hidden team script `id`.
pub fn hidden_name_error(id: i64) -> StoreError {
    StoreError::Invalid(format!(
        "a hidden team script has this name (script {id}): unhide it with harness-memory scripts unhide {id}, or \
         pick another name"
    ))
}

/// The refusal to change team script `id` (`name`) while scripts are `remote+local`.
pub fn read_only_error(id: i64, name: &str) -> StoreError {
    StoreError::Invalid(format!(
        "script {id} {name} is a team script and read-only here: scripts is \"remote+local\", which reads the \
         team's scripts and saves new scripts on this computer only. Save a script of your own instead, or stop \
         seeing this one with harness-memory scripts hide {id} (Hide on this computer in Harness Desktop)"
    ))
}

impl ScriptStore for SplitScripts {
    fn save_script(&mut self, target: &Target, draft: &ScriptDraft) -> Result<SavedScript, StoreError> {
        let side = self.write_side();

        if side == Side::Team {
            self.check_hidden_name(target, &draft.name)?;
        }

        let saved = self.store(side).save_script(target, draft)?;

        Ok(SavedScript {
            script: shown(side, saved.script)?,
            ..saved
        })
    }

    fn list_scripts(&mut self, view: &View, query: Option<&str>) -> Result<Vec<ScriptSummary>, StoreError> {
        let hidden = self.hidden_ids();
        let mut listed: Vec<ScriptSummary> = Vec::new();

        for side in [Side::Local, Side::Team] {
            if !self.reads(side) {
                continue;
            }

            for summary in self.store(side).list_scripts(view, query)? {
                if side == Side::Team && hidden.contains(&summary.id) {
                    continue;
                }

                let id = encode_as(side, summary.id, "script")?;
                listed.push(ScriptSummary { id, ..summary });
            }
        }

        listed.sort_by_cached_key(listed_order);
        Ok(listed)
    }

    fn get_script(&mut self, view: &View, script: &ScriptRef) -> Result<Option<Script>, StoreError> {
        match script {
            ScriptRef::Id(id) => self.find_by_id(view, *id),
            ScriptRef::Name(name) => self.find_by_name(view, name),
        }
    }

    /// Deletes the script `script` names (looked up as [`ScriptStore::get_script`] does); a team
    /// script is refused in `remote+local`, and not found when the hidden list says so (read
    /// strictly).
    fn delete_script(&mut self, view: &View, script: &ScriptRef) -> Result<bool, StoreError> {
        let Some(found) = self.get_script(view, script)? else {
            return Ok(false);
        };

        let id = found.summary.id;
        let (side, raw) = decode(id);

        if side == Side::Team {
            if self.team_read_only() {
                return Err(read_only_error(id, &found.summary.name));
            }

            if self.hidden_ids_strict()?.contains(&raw) {
                return Ok(false);
            }
        }

        self.store(side).delete_script(view, &ScriptRef::Id(raw))
    }

    /// Deletes the scripts among `ids` that are visible in `view`: the team's only in `remote` and
    /// never a hidden one (nothing there when the hidden list cannot be read), this computer's in
    /// `remote+local`. A failure on the second store leaves the first's deletes.
    fn delete_scripts(&mut self, view: &View, ids: &[i64]) -> Result<usize, StoreError> {
        let mut team: Vec<i64> = Vec::new();
        let mut local: Vec<i64> = Vec::new();

        for id in ids {
            match decode(*id) {
                (Side::Team, raw) => team.push(raw),
                (Side::Local, raw) => local.push(raw),
            }
        }

        let mut deleted = 0;

        if self.reads(Side::Team) && !self.team_read_only() && !team.is_empty() {
            let hidden = self.hidden_ids_strict()?;
            team.retain(|raw| !hidden.contains(raw));
            deleted += self.team.delete_scripts(view, &team)?;
        }

        if self.reads(Side::Local) && !local.is_empty() {
            deleted += self.local.delete_scripts(view, &local)?;
        }

        Ok(deleted)
    }

    /// The team's scripts in `view` the hidden list names; an error when the list cannot be read
    /// (read strictly: a check of the hidden scripts must not pass because none could be seen).
    fn hidden_scripts(&mut self, view: &View) -> Result<Vec<ScriptSummary>, StoreError> {
        if !self.reads(Side::Team) {
            return Ok(Vec::new());
        }

        let hidden = self.hidden_ids_strict()?;

        if hidden.is_empty() {
            return Ok(Vec::new());
        }

        let mut listed = Vec::new();

        for summary in self.team.list_scripts(view, None)? {
            if !hidden.contains(&summary.id) {
                continue;
            }

            let id = encode_as(Side::Team, summary.id, "script")?;
            listed.push(ScriptSummary { id, ..summary });
        }

        Ok(listed)
    }

    fn record_run(&mut self, id: i64, exit_code: Option<i32>, at: i64) -> Result<(), StoreError> {
        let (side, raw) = decode(id);

        if !self.reads(side) {
            return Ok(());
        }

        self.store(side).record_run(raw, exit_code, at)
    }

    fn origin(&self, id: i64) -> ScriptOrigin {
        match decode(id) {
            (Side::Team, raw) => ScriptOrigin::Team {
                raw,
                read_only: self.team_read_only(),
            },
            (Side::Local, raw) => ScriptOrigin::Local { raw },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::split::LOCAL_ID_OFFSET;

    #[test]
    fn local_scripts_are_shown_past_the_offset() {
        let script = |id| Script {
            summary: ScriptSummary {
                id,
                scope: Scope::Global,
                project: None,
                name: "x".to_owned(),
                description: String::new(),
                language: crate::store::Language::Sh,
                sha256: String::new(),
                timeout_secs: 1,
                provenance: crate::store::Provenance::default(),
                created_at: 0,
                updated_at: 0,
                run_count: 0,
                last_run_at: None,
                last_exit_code: None,
            },
            body: "true".to_owned(),
        };

        assert_eq!(
            shown(Side::Local, script(7)).expect("local").summary.id,
            LOCAL_ID_OFFSET + 7
        );
        assert_eq!(shown(Side::Team, script(7)).expect("team").summary.id, 7);
        assert!(shown(Side::Team, script(LOCAL_ID_OFFSET)).is_err());
    }
}
