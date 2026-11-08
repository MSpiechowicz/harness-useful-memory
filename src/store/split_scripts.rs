//! Scripts read from the team database and this computer, as `scripts` says.
//!
//! [`SplitScripts`] serves the script tools and `harness-memory scripts` whenever scripts are read
//! from the team database ([`NotesPlace::Remote`] and [`NotesPlace::RemoteLocal`], see
//! [`crate::backend::Routing::scripts`]); `local` scripts use the local store alone. Each side is
//! opened on its first use, so with `remote` the local file is never opened.
//!
//! - **`remote`** reads and writes the team database, under the team's ids.
//! - **`remote+local`** reads both and saves new scripts where [`SplitScripts::saving_to`] says
//!   (`remote_local_saves`, the team database by default). The same name may be in both stores: both
//!   are listed, and a name is looked up on this computer first (this computer's repository scripts,
//!   the team's, this computer's global ones, the team's). When it saves to this computer the team's
//!   scripts are read-only: deleting one is refused ([`read_only_error`], naming `harness-memory
//!   scripts hide`), deleting many leaves them. A run of one is still counted in the team database.
//!   When it saves to the team database they can be changed and deleted, as in `remote`.
//!
//! **Scripts kept here.** In `remote+local` saving to the team database, a save without a place under
//! a name a script of this computer already has, in the same scope, replaces that script where it is,
//! online or not, and lists nothing as waiting: only a new name follows `remote_local_saves`, and only
//! the script an offline save created is listed. With `remote` whatever is saved offline is listed.
//! [`ScriptStore::save_place`] tells which place a save is meant for.
//!
//! **Picking the place.** With `remote+local`, [`ScriptStore::save_script_to`] saves a script to the
//! place the user picked (`scripts save --place`), as [`crate::store::SplitStore`] does for notes and
//! whatever `remote_local_saves` says: `local` straight to this computer, `remote` straight to the
//! team database (hidden name check included) and failing, saving nothing, when it cannot be reached.
//! Neither is recorded as waiting for the team database.
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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::backend::{NotesPlace, Place, Secrets};
use crate::push::{Options, scope_label};
use crate::push_scripts::{TeamScripts, push_scripts_only};
use crate::store::pending::{Entry, Kind, Pending};
use crate::store::split::{Side, decode, encode_as, narrow_by, target_scope};
use crate::store::{
    HiddenScripts, SavedScript, Scope, Script, ScriptDraft, ScriptOrigin, ScriptRef, ScriptStore, ScriptSummary,
    StoreError, Target, TrustStore, View,
};

/// How long a copy of the scripts saved offline that failed is not tried again.
const RETRY_COPY: Duration = Duration::from_secs(60);

/// The team database's scripts and this computer's, as `scripts` routes them. See the module docs.
pub struct SplitScripts {
    team: Box<dyn ScriptStore>,
    local: Box<dyn ScriptStore>,
    place: NotesPlace,
    /// Where new scripts are saved with `remote+local`.
    saves: Place,
    hidden: Option<HiddenScripts>,
    fallback: Option<Fallback>,
    /// True while the offline fallback serves the scripts from this computer.
    down: bool,
}

/// The offline fallback of a [`SplitScripts`], see [`SplitScripts::with_offline_fallback`].
struct Fallback {
    /// The scripts saved on this computer for scripts set to `remote`.
    pending: Pending,
    /// The local database those scripts are copied from.
    database: PathBuf,
    /// This machine's script trust, carried along to the team database.
    trust: Box<dyn TrustStore>,
    /// The name the team database has in this machine's script trust, which names it in the
    /// pending list too ([`crate::backend::Backend::team_key`]).
    key: String,
    /// Masks the database credentials out of what is printed.
    secrets: Secrets,
    /// Whether the pending scripts are copied when the team database is next reachable.
    sync_due: bool,
    /// Not before this time: a copy that failed is tried again after a while.
    retry_at: Instant,
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
            saves: Place::default(),
            hidden,
            fallback: None,
            down: false,
        }
    }

    /// The store saving new scripts of `remote+local` to `saves` (`remote_local_saves`; the team
    /// database without this). The team's scripts can be changed from here only when that is the
    /// team database.
    pub fn saving_to(mut self, saves: Place) -> Self {
        self.saves = saves;
        self
    }

    /// The store falling back to this computer while the team database cannot be opened: scripts
    /// are then read and saved in the local database `database` (the team's are not shown), the
    /// ones saved while scripts are set to `remote` are listed in [`Pending`], and they are copied
    /// to the team database (named `key` in `trust`) when it is reachable again, at the next call
    /// or when a later server starts. Without it an unreachable team database fails the calls that
    /// need it.
    pub fn with_offline_fallback(
        mut self,
        database: &Path,
        trust: Box<dyn TrustStore>,
        key: String,
        secrets: Secrets,
    ) -> Self {
        self.fallback = Some(Fallback {
            pending: Pending::beside(database),
            database: database.to_owned(),
            trust,
            key,
            secrets,
            sync_due: true,
            retry_at: Instant::now(),
        });
        self
    }

    /// Where scripts are read and saved now: the local database alone while the offline fallback
    /// is serving.
    fn effective(&self) -> NotesPlace {
        match self.place {
            NotesPlace::Remote | NotesPlace::RemoteLocal if self.down => NotesPlace::Local,
            place => place,
        }
    }

    /// True when scripts are read from the team database and the offline fallback serves meanwhile.
    fn falls_back(&self) -> bool {
        self.fallback.is_some() && self.place.reads_team()
    }

    /// Serves from this computer from now on: the team database is not reachable.
    fn go_down(&mut self) {
        if !self.down {
            self.down = true;
            eprintln!(
                "harness-memory: the team database is unreachable: scripts are read and saved on this computer \
                 until it is back"
            );
        }

        if let Some(fallback) = self.fallback.as_mut() {
            fallback.sync_due = true;
        }
    }

    /// Runs `call`; when the team database turns out to be unreachable during it (a connection that
    /// worked when the call began and dropped since), the connection is dropped, so the calls
    /// until the cooldown ends fail at once, and with the offline fallback the call is made once
    /// more, now served from this computer (and recorded for the team database as an offline
    /// call is). Without the fallback the call fails, with what to change when the store knows,
    /// and the next call connects again. A failure of the database that answered is not repeated.
    fn attempt<T>(&mut self, call: impl Fn(&mut Self) -> Result<T, StoreError>) -> Result<T, StoreError> {
        let reason = match call(self) {
            // Already serving from this computer: the team database is not what failed.
            Err(StoreError::Unreachable(reason)) if !self.down => reason,
            other => return other,
        };

        let falls_back = self.falls_back();
        let message = self.team.connection_lost(&reason, falls_back);

        if !falls_back {
            return Err(StoreError::Unreachable(message));
        }

        self.go_down();
        call(self)
    }

    /// Decides before a call whether the team database is reachable (a failed open is not tried
    /// again for a while), and copies the scripts saved offline when it is back.
    fn settle(&mut self) {
        if !self.falls_back() {
            return;
        }

        let up = self.team.ready();

        if !up {
            self.go_down();
            return;
        }

        self.down = false;

        let Some(fallback) = self.fallback.as_mut() else {
            return;
        };

        if fallback.sync_due && Instant::now() >= fallback.retry_at {
            fallback.sync_due = false;
            self.copy_offline_scripts();
        }
    }

    /// Copies the scripts saved offline to the team database; the ones that were not copied stay
    /// listed and are tried again after a while. A script the team database now holds as it is here
    /// is deleted from this computer, while one it holds differently (a teammate's newer version, or
    /// one the team kept) stays here for the person to decide, as `harness-memory scripts push`
    /// leaves it.
    fn copy_offline_scripts(&mut self) {
        let Some(mut fallback) = self.fallback.take() else {
            return;
        };

        // An entry whose script is gone from this computer waits for nothing.
        let listed = fallback.pending.entries(Kind::Scripts, &fallback.key);
        let listed = self.without_gone_scripts(&fallback, listed);

        // Only what is meant for this team database, while new scripts are still saved there.
        let waiting = if self.place.writes_team(self.saves) {
            listed
        } else {
            Vec::new()
        };

        if !waiting.is_empty() && !self.push_offline_scripts(&mut fallback, &waiting) {
            fallback.sync_due = true;
            fallback.retry_at = Instant::now() + RETRY_COPY;
        }

        self.fallback = Some(fallback);
    }

    /// `entries` without those whose local script is gone, which are taken off the list too. An entry
    /// whose script is there under another name stays, and so does everything when the scripts
    /// cannot be asked: nothing is dropped unseen.
    fn without_gone_scripts(&mut self, fallback: &Fallback, entries: Vec<Entry>) -> Vec<Entry> {
        let mut gone: Vec<Entry> = Vec::new();

        for entry in &entries {
            match self.local.get_script(&View::every_repo(), &ScriptRef::Id(entry.id)) {
                Ok(Some(_)) => {}
                Ok(None) => gone.push(entry.clone()),
                Err(_) => return entries,
            }
        }

        if gone.is_empty() {
            return entries;
        }

        if let Err(error) = fallback.pending.remove(Kind::Scripts, &gone) {
            eprintln!("harness-memory: {}", fallback.secrets.scrub(&error));
        }

        entries.into_iter().filter(|entry| !gone.contains(entry)).collect()
    }

    /// Copies `waiting` to the team database; false when something has to be tried again.
    fn push_offline_scripts(&mut self, fallback: &mut Fallback, waiting: &[Entry]) -> bool {
        let secrets = fallback.secrets.clone();

        let hidden = match self.hidden.as_mut().map(HiddenScripts::try_ids) {
            Some(Ok(hidden)) => hidden,
            Some(Err(error)) => {
                eprintln!(
                    "harness-memory: scripts saved offline were not copied to the team database: {}",
                    secrets.scrub(&error.to_string())
                );
                return false;
            }
            None => Vec::new(),
        };

        let mut team = TeamScripts {
            scripts: self.team.as_mut(),
            key: &fallback.key,
            hidden: &hidden,
        };

        let summary = match push_scripts_only(
            &fallback.database,
            &mut team,
            fallback.trust.as_mut(),
            &Options {
                delete_copied: true,
                ..Options::default()
            },
            waiting,
        ) {
            Ok(summary) => summary,
            Err(error) => {
                eprintln!(
                    "harness-memory: scripts saved offline were not copied to the team database: {}",
                    secrets.scrub(&error.to_string())
                );
                return false;
            }
        };

        // A script that failed stays listed; one that is gone from this computer leaves the list.
        let kept: BTreeSet<i64> = summary.failed.iter().filter_map(|failure| failure.local_id).collect();
        let failed_without_script = summary.failed.iter().any(|failure| failure.local_id.is_none());

        // Only a script the push took leaves the list: one renamed behind the list's back stays.
        let matched: BTreeSet<i64> = summary.changes.iter().map(|change| change.local_id).collect();

        let done: Vec<Entry> = waiting
            .iter()
            .filter(|entry| !failed_without_script && matched.contains(&entry.id) && !kept.contains(&entry.id))
            .cloned()
            .collect();

        if let Err(error) = fallback.pending.remove(Kind::Scripts, &done) {
            eprintln!("harness-memory: {}", secrets.scrub(&error));
        }

        let copied = summary
            .changes
            .iter()
            .filter(|change| !kept.contains(&change.local_id) && change.action.copies())
            .count();

        if copied > 0 {
            eprintln!("harness-memory: copied {copied} script(s) saved offline to the team database");
        }

        for failure in &summary.failed {
            eprintln!("harness-memory: {}", secrets.scrub(&failure.message));
        }

        kept.is_empty() && !failed_without_script
    }

    /// Lists the local script `id` named `name`, saved in the scope of `target` while scripts are
    /// set to `remote` and the team database was unreachable, as waiting to be copied. False when the
    /// list could not be changed: the script stays here, unlisted.
    fn record_offline(&self, target: &Target, id: i64, name: &str) -> bool {
        let Some(fallback) = &self.fallback else {
            return true;
        };

        let entry = Entry {
            team: fallback.key.clone(),
            id,
            scope: scope_label(target),
            name: name.to_owned(),
        };

        if let Err(error) = fallback.pending.add(Kind::Scripts, entry) {
            eprintln!(
                "harness-memory: the script was saved here but could not be listed for copying: {}",
                fallback.secrets.scrub(&error)
            );

            return false;
        }

        true
    }

    /// True when the store `side` is read.
    fn reads(&self, side: Side) -> bool {
        match side {
            Side::Team => self.effective().reads_team(),
            Side::Local => self.effective().reads_local(),
        }
    }

    /// The store new scripts are saved to.
    fn write_side(&self) -> Side {
        match self.effective().writes(self.saves) {
            Place::Remote => Side::Team,
            Place::Local => Side::Local,
        }
    }

    /// True when the team's scripts cannot be changed from here (`remote+local` saving to this
    /// computer).
    fn team_read_only(&self) -> bool {
        self.effective().team_read_only(self.saves)
    }

    fn store(&mut self, side: Side) -> &mut dyn ScriptStore {
        match side {
            Side::Team => self.team.as_mut(),
            Side::Local => self.local.as_mut(),
        }
    }

    /// The ids of the team scripts hidden on this computer (none without [`HiddenScripts`] or when
    /// the list cannot be read), the list pruned first when it is due; an error only for a team
    /// database that cannot be reached.
    fn hidden_ids(&mut self) -> Result<Vec<i64>, StoreError> {
        let Some(hidden) = self.hidden.as_mut() else {
            return Ok(Vec::new());
        };

        // A team database that cannot be read fails the call itself, so a failed prune waits for
        // the next turn. A connection that dropped is the call's to handle ([`Self::attempt`]); one
        // that is down is not read at all.
        if !self.down {
            lost_connection(hidden.prune_if_due(self.team.as_mut()))?;
        }

        Ok(hidden.ids())
    }

    /// The ids [`Self::hidden_ids`] reads, for a change to the team's scripts: an error when the
    /// hidden list cannot be read, rather than none hidden.
    fn hidden_ids_strict(&mut self) -> Result<Vec<i64>, StoreError> {
        let Some(hidden) = self.hidden.as_mut() else {
            return Ok(Vec::new());
        };

        if !self.down {
            lost_connection(hidden.prune_if_due(self.team.as_mut()))?;
        }

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
        let hidden = self.hidden_ids()?;
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

        if side == Side::Team && self.hidden_ids()?.contains(&raw) {
            return Ok(None);
        }

        match self.store(side).get_script(view, &ScriptRef::Id(raw))? {
            Some(script) => shown(side, script).map(Some),
            None => Ok(None),
        }
    }
}

/// `pruned` as a failed prune of the hidden list leaves it: only a team database that cannot be
/// reached is an error, which fails the call that asked.
fn lost_connection(pruned: Result<usize, StoreError>) -> Result<(), StoreError> {
    match pruned {
        Err(error @ StoreError::Unreachable(_)) => Err(error),
        _ => Ok(()),
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

/// The refusal to change team script `id` (`name`) while scripts are `remote+local` and saved to this
/// computer.
pub fn read_only_error(id: i64, name: &str) -> StoreError {
    StoreError::Invalid(format!(
        "script {id} {name} is a team script and read-only here: scripts is \"remote+local\" and \
         remote_local_saves is \"local\", which reads the team's scripts and saves new scripts on this computer \
         only. Save a script of your own instead, or stop seeing this one with harness-memory scripts hide {id} \
         (Hide on this computer in Harness Desktop)"
    ))
}

impl SplitScripts {
    fn save_script_once(&mut self, target: &Target, draft: &ScriptDraft) -> Result<SavedScript, StoreError> {
        self.settle();
        let side = self.save_side(target, &draft.name)?;

        if side == Side::Team {
            self.check_hidden_name(target, &draft.name)?;
        }

        let saved = self.store(side).save_script(target, draft)?;

        // Listed whether the save made the script or replaced one already here (a leftover of an
        // earlier offline save, say): what was saved last while scripts are set to `remote` is meant
        // for the team database. The list is a set, so a script listed already is not listed twice.
        // Scripts routed `remote+local` list only the script the save made: one that was here already
        // is a person's own.
        let mut not_listed = false;
        let lists = saved.created || self.place == NotesPlace::Remote;

        if side == Side::Local && self.down && self.place.writes_team(self.saves) && lists {
            not_listed = !self.record_offline(target, saved.script.summary.id, &saved.script.summary.name);
        }

        Ok(SavedScript {
            script: shown(side, saved.script)?,
            not_listed,
            ..saved
        })
    }

    /// The store a save without a picked place writes the script `name` of `target` to: where new
    /// scripts go, except that a script kept on this computer under that name in that scope is
    /// updated where it is, in `remote+local` online or not.
    fn save_side(&mut self, target: &Target, name: &str) -> Result<Side, StoreError> {
        let side = self.write_side();

        if side == Side::Team && self.place == NotesPlace::RemoteLocal && self.kept_here(target, name)? {
            return Ok(Side::Local);
        }

        Ok(side)
    }

    /// True when a script of this computer has `name` in the scope of `target`.
    fn kept_here(&mut self, target: &Target, name: &str) -> Result<bool, StoreError> {
        let scope = target_scope(target);
        let Some(view) = narrow_by(&View::around(target), |kept| kept == scope) else {
            return Ok(false);
        };

        let wanted = ScriptRef::Name(name.trim().to_owned());

        Ok(self.local.get_script(&view, &wanted)?.is_some())
    }

    /// Takes the local script `id` saved in the scope of `target` off the list of what waits for the
    /// team database: the person kept it here.
    fn unlist_offline(&self, target: &Target, id: i64) -> Result<(), StoreError> {
        let Some(fallback) = &self.fallback else {
            return Ok(());
        };

        fallback
            .pending
            .forget(Kind::Scripts, &fallback.key, id, &scope_label(target))
            .map_err(|error| {
                StoreError::Backend(format!(
                    "the script was saved on this computer, but could not be taken off the list of what waits \
                     for the team database, so it may still be copied there: {}",
                    fallback.secrets.scrub(&error)
                ))
            })
    }

    /// Saves the script to this computer whatever the routing says: the place a person picked. It
    /// records nothing as waiting, so the script stays here, also while the team database is
    /// unreachable, and a script listed as waiting is taken off the list.
    fn save_script_local(&mut self, target: &Target, draft: &ScriptDraft) -> Result<SavedScript, StoreError> {
        let saved = self.local.save_script(target, draft)?;

        // Off the list before anything settles: a settle copies what waits to the team database and
        // deletes the script here, which would send the old text there and drop the person's own.
        self.unlist_offline(target, saved.script.summary.id)?;
        self.settle();

        Ok(SavedScript {
            script: shown(Side::Local, saved.script)?,
            ..saved
        })
    }

    /// Saves the script to the team database whatever the routing says, with the rules of a save
    /// there: the hidden name check, then the store's own. It never serves this computer and
    /// records nothing as waiting, so it takes no part in the offline fallback.
    fn save_script_remote(&mut self, target: &Target, draft: &ScriptDraft) -> Result<SavedScript, StoreError> {
        self.check_hidden_name(target, &draft.name)?;

        let saved = self.team.save_script(target, draft)?;

        Ok(SavedScript {
            script: shown(Side::Team, saved.script)?,
            ..saved
        })
    }

    fn list_scripts_once(&mut self, view: &View, query: Option<&str>) -> Result<Vec<ScriptSummary>, StoreError> {
        self.settle();
        let hidden = self.hidden_ids()?;
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

    fn get_script_once(&mut self, view: &View, script: &ScriptRef) -> Result<Option<Script>, StoreError> {
        self.settle();
        match script {
            ScriptRef::Id(id) => self.find_by_id(view, *id),
            ScriptRef::Name(name) => self.find_by_name(view, name),
        }
    }

    /// Deletes the script `script` names (looked up as [`ScriptStore::get_script`] does); a team
    /// script is refused in `remote+local` saving to this computer, and not found when the hidden
    /// list says so (read strictly).
    fn delete_script_once(&mut self, view: &View, script: &ScriptRef) -> Result<bool, StoreError> {
        self.settle();
        let Some(found) = self.get_script_once(view, script)? else {
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

    /// Deletes the scripts among `ids` that are visible in `view`: the team's only where they can be
    /// changed (`remote`, or `remote+local` saving to the team database) and never a hidden one
    /// (nothing there when the hidden list cannot be read), this computer's in `remote+local`. A failure on the second store leaves the first's deletes.
    fn delete_scripts_once(&mut self, view: &View, ids: &[i64]) -> Result<usize, StoreError> {
        self.settle();
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
    fn hidden_scripts_once(&mut self, view: &View) -> Result<Vec<ScriptSummary>, StoreError> {
        self.settle();
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

    fn record_run_once(&mut self, id: i64, exit_code: Option<i32>, at: i64) -> Result<(), StoreError> {
        self.settle();
        let (side, raw) = decode(id);

        if !self.reads(side) {
            return Ok(());
        }

        self.store(side).record_run(raw, exit_code, at)
    }
}

impl ScriptStore for SplitScripts {
    fn save_script(&mut self, target: &Target, draft: &ScriptDraft) -> Result<SavedScript, StoreError> {
        self.attempt(|this| this.save_script_once(target, draft))
    }

    fn save_script_to(
        &mut self,
        place: Place,
        target: &Target,
        draft: &ScriptDraft,
    ) -> Result<SavedScript, StoreError> {
        if self.place != NotesPlace::RemoteLocal {
            return Err(StoreError::Invalid(
                "a place can be picked only for scripts routed \"remote+local\"".to_owned(),
            ));
        }

        match place {
            Place::Local => self.save_script_local(target, draft),
            Place::Remote => match self.save_script_remote(target, draft) {
                Err(StoreError::Unreachable(reason)) => {
                    let message = self.team.connection_lost(&reason, false);

                    Err(StoreError::Unreachable(format!(
                        "nothing was saved, not even on this computer: {message}"
                    )))
                }
                other => other,
            },
        }
    }

    fn keep_here(&mut self, target: &Target, name: &str) -> Result<(), StoreError> {
        if self.place != NotesPlace::RemoteLocal || self.fallback.is_none() {
            return Ok(());
        }

        let scope = target_scope(target);
        let Some(view) = narrow_by(&View::around(target), |kept| kept == scope) else {
            return Ok(());
        };

        let wanted = ScriptRef::Name(name.trim().to_owned());

        match self.local.get_script(&view, &wanted)? {
            Some(script) => self.unlist_offline(target, script.summary.id),
            None => Ok(()),
        }
    }

    fn list_scripts(&mut self, view: &View, query: Option<&str>) -> Result<Vec<ScriptSummary>, StoreError> {
        self.attempt(|this| this.list_scripts_once(view, query))
    }

    fn get_script(&mut self, view: &View, script: &ScriptRef) -> Result<Option<Script>, StoreError> {
        self.attempt(|this| this.get_script_once(view, script))
    }

    fn delete_script(&mut self, view: &View, script: &ScriptRef) -> Result<bool, StoreError> {
        self.attempt(|this| this.delete_script_once(view, script))
    }

    fn delete_scripts(&mut self, view: &View, ids: &[i64]) -> Result<usize, StoreError> {
        self.attempt(|this| this.delete_scripts_once(view, ids))
    }

    fn hidden_scripts(&mut self, view: &View) -> Result<Vec<ScriptSummary>, StoreError> {
        self.attempt(|this| this.hidden_scripts_once(view))
    }

    fn record_run(&mut self, id: i64, exit_code: Option<i32>, at: i64) -> Result<(), StoreError> {
        self.attempt(|this| this.record_run_once(id, exit_code, at))
    }

    /// The place the script is meant for: this computer's when it keeps the script already
    /// (`remote+local`), else where new scripts are meant to go, which while the team database is
    /// unreachable is still the team database (what is saved meanwhile waits for it).
    fn save_place(&mut self, target: &Target, name: &str) -> Result<Option<Place>, StoreError> {
        if self.place == NotesPlace::RemoteLocal && self.kept_here(target, name)? {
            return Ok(Some(Place::Local));
        }

        Ok(Some(self.place.writes(self.saves)))
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

    fn ready(&mut self) -> bool {
        self.settle();
        !self.down
    }

    fn offline(&self) -> bool {
        self.down
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
                authors: crate::store::Authors::default(),
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
