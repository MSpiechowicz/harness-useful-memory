//! `harness-memory scripts push`: copies the scripts of the local SQLite file into the team
//! database, as `push` does for notes ([`crate::push`], whose [`Options`], [`Action`] and
//! [`Failure`] it shares).
//!
//! A local script is matched with the team script of the same scope, repository and name. The same
//! version ([`script_sha256`] of the language and body), description and timeout is
//! [`Action::Identical`]; otherwise a team script changed at or after the local one is
//! [`Action::TeamNewer`]; a newer local script with the team's version only changes its
//! description or timeout ([`Action::Updated`]); a newer local script with another version is a
//! conflict, which [`Options::overwrite`] overwrites ([`Action::Overwritten`]) or leaves
//! ([`Action::Kept`]). Scripts are written with [`ScriptStore::save_script`]. A team script hidden on
//! this computer is never written over, nor counted as copied: that script fails instead, in a dry
//! run as well, so the preview lists the same failures as the push.
//!
//! **Trust.** A script this computer trusts stays trusted once copied: after an add, update,
//! overwrite or identical, when this computer trusts the local version (`local`, its id, its
//! [`script_sha256`], as the script it is now), the team script is trusted as well (under the team
//! database's key and the team's id), with the same origin, once the team's row is checked to hold
//! that version under that name, scope and repository. A local version waiting for approval is
//! never trusted by a copy. A local version held back from team trust
//! ([`TrustStore::hold_back`]) is held back on this computer as the team's copy too, by name
//! before it is copied and by the team's id after, so `trust_team_scripts` does not trust it. Failing to trust, or to
//! hold back, is a failure of that script: it is left out of the totals and not deleted.
//!
//! **`--delete-copied`.** After a push that is not a dry run, the local scripts the team now holds
//! as they are here (added, updated, overwritten, identical) are deleted, unless their push failed
//! or they changed since they were read (their update time or version; such a script is marked
//! [`ScriptChange::changed_since`]). Any failure that names no script deletes nothing.

use std::collections::HashSet;
use std::path::Path;

use crate::backend::LOCAL_KEY;
use crate::push::{Action, Failure, Options, Overwrite};
use crate::store::hidden::view_of;
use crate::store::split::narrow_by;
use crate::store::split_scripts::hidden_name_error;
use crate::store::{
    ProjectKey, Scope, Script, ScriptDraft, ScriptRef, ScriptStore, SqliteStore, StoreError, Target, TrustStore,
    TrustedVersion, View, script_sha256,
};

/// One local script a push took, and what it did (or, for a dry run, would do) with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptChange {
    /// The script's id in the local file.
    pub local_id: i64,
    pub scope: Scope,
    /// The repository key; `None` for a global script.
    pub repo: Option<String>,
    pub name: String,
    pub action: Action,
    /// True when the local script was newer and its version differed from the team's: an
    /// overwrite candidate, whether [`Options::overwrite`] took it or not.
    pub conflict: bool,
    /// The script's id in the team database; `None` for a script a dry run would add, or whose
    /// adding failed.
    pub team_id: Option<i64>,
    /// Unix seconds.
    pub local_updated_at: i64,
    /// [`script_sha256`] of the local script as read.
    pub local_sha256: String,
    /// Unix seconds; `None` when the team had no such script.
    pub team_updated_at: Option<i64>,
    /// True when `--delete-copied` deleted the local script.
    pub deleted: bool,
    /// True when `--delete-copied` would have deleted the local script, but it changed (or went)
    /// after the push read it, so it stayed.
    pub changed_since: bool,
    /// True when writing or trusting the team's script failed (see [`ScriptPush::failed`]); the
    /// script keeps its [`ScriptChange::action`] but is left out of the totals.
    pub failed: bool,
    /// True when this computer's trust in the local version was carried to the team's script.
    pub trusted: bool,
}

/// What a script push did (or, for a dry run, would do).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptPush {
    /// Scripts added; this and the other totals leave out the scripts whose push failed.
    pub added: usize,
    pub updated: usize,
    pub overwritten: usize,
    pub kept: usize,
    pub identical: usize,
    pub team_newer: usize,
    /// Every local script the push took, in push order (oldest first).
    pub changes: Vec<ScriptChange>,
    /// Scripts that could not be written or trusted, and local scripts that could not be deleted.
    pub failed: Vec<Failure>,
    /// How many local scripts `--delete-copied` deleted; `None` when it was not asked for or the
    /// push was a dry run.
    pub deleted: Option<usize>,
}

/// The team database a push writes to.
pub struct TeamScripts<'a> {
    pub scripts: &'a mut dyn ScriptStore,
    /// Its name in this machine's script trust (`team:<hash>`).
    pub key: &'a str,
    /// The ids of its scripts hidden on this computer (read strictly: a push stops when the list
    /// cannot be read).
    pub hidden: &'a [i64],
}

/// Pushes the scripts of the local file at `local` into `team` as `options` say, carrying this
/// machine's trust (`trust`) along. The local file is migrated first like the server would (it is
/// never created: a missing file is an error).
pub fn push_scripts(
    local: &Path,
    team: &mut TeamScripts,
    trust: &mut dyn TrustStore,
    options: &Options,
) -> Result<ScriptPush, StoreError> {
    let scripts = read_local(local)?;
    let mut summary = ScriptPush::default();

    for (script, project) in &scripts {
        let target = match project {
            Some(key) => Target::Repo(key.clone()),
            None => Target::Global,
        };

        if !options.takes(&target) {
            continue;
        }

        if let Err(error) = push_one(team, trust, script, &target, options, &mut summary) {
            let local_id = script.summary.id;
            uncount(&mut summary, local_id);

            summary.failed.push(Failure {
                local_id: Some(local_id),
                message: format!("script {local_id} {}: {error}", script.summary.name),
                deleting: false,
            });
        }
    }

    if options.delete_copied && !options.dry_run {
        summary.deleted = Some(delete_copied(local, &mut summary));
    }

    Ok(summary)
}

/// Classifies one local script and, unless a dry run, writes and trusts its team copy.
fn push_one(
    team: &mut TeamScripts,
    trust: &mut dyn TrustStore,
    local: &Script,
    target: &Target,
    options: &Options,
    summary: &mut ScriptPush,
) -> Result<(), StoreError> {
    let scope = scope_of(target);
    let only_scope = narrow_by(&View::around(target), |kept| kept == scope);
    let name = ScriptRef::Name(local.summary.name.clone());

    let existing = match &only_scope {
        Some(view) => team.scripts.get_script(view, &name)?,
        None => None,
    };

    let (action, conflict) = match &existing {
        Some(theirs) => compare(local, theirs, &options.overwrite),
        None => (Action::Added, false),
    };

    let index = record(summary, local, target, action, conflict, existing.as_ref());

    // Checked before a dry run returns, so the preview fails what the push would.
    if let Some(theirs) = &existing
        && action.copies()
        && team.hidden.contains(&theirs.summary.id)
    {
        return Err(hidden_name_error(theirs.summary.id));
    }

    if options.dry_run {
        return Ok(());
    }

    let held_back = trust.is_held_back(LOCAL_KEY, local.summary.id, &local.summary.name, &local.summary.sha256)?;

    if held_back {
        trust.hold_back(team.key, None, &local.summary.name, &local.summary.sha256)?;
    }

    let stored = match action {
        Action::Added | Action::Updated | Action::Overwritten => {
            let saved = team.scripts.save_script(target, &draft_of(local))?;
            summary.changes[index].team_id = Some(saved.script.summary.id);
            saved.script
        }
        Action::Identical => match existing {
            Some(theirs) => theirs,
            None => return Ok(()),
        },
        Action::Kept | Action::TeamNewer => return Ok(()),
    };

    if !same_version(&stored, local) {
        return Err(StoreError::Backend(
            "the team database holds another version than the one pushed".to_owned(),
        ));
    }

    if held_back {
        trust.hold_back(
            team.key,
            Some(stored.summary.id),
            &stored.summary.name,
            &stored.summary.sha256,
        )?;
    }

    summary.changes[index].trusted = carry_trust(trust, team.key, local, &stored)?;
    Ok(())
}

/// What pushing `local` over the team's script `theirs` with the same name does, and whether it is
/// a conflict (the local script is newer and its version differs).
fn compare(local: &Script, theirs: &Script, overwrite: &Overwrite) -> (Action, bool) {
    let (mine, team) = (&local.summary, &theirs.summary);
    let same_version = mine.sha256 == team.sha256;

    if same_version && mine.description == team.description && mine.timeout_secs == team.timeout_secs {
        return (Action::Identical, false);
    }

    if mine.updated_at <= team.updated_at {
        return (Action::TeamNewer, false);
    }

    if same_version {
        return (Action::Updated, false);
    }

    if overwrite.takes(mine.id) {
        (Action::Overwritten, true)
    } else {
        (Action::Kept, true)
    }
}

/// True when `stored` (the team's row) holds the version of `local` under its name, scope and
/// repository.
fn same_version(stored: &Script, local: &Script) -> bool {
    let (theirs, mine) = (&stored.summary, &local.summary);

    stored.body == local.body
        && theirs.language == mine.language
        && theirs.name == mine.name
        && theirs.scope == mine.scope
        && theirs.project == mine.project
}

/// Trusts `stored` (the team's copy of `local`) under `team_key` when this machine trusts the
/// local version as the script it is; true when it did.
fn carry_trust(
    trust: &mut dyn TrustStore,
    team_key: &str,
    local: &Script,
    stored: &Script,
) -> Result<bool, StoreError> {
    let record = trust.trust_of(LOCAL_KEY, local.summary.id, &local.summary.sha256)?;
    let Some(record) = record.filter(|record| record.is_for(&local.summary)) else {
        return Ok(false);
    };

    let version = TrustedVersion::of(team_key, stored.summary.id, stored, record.origin);
    trust.trust(&version)?;
    Ok(true)
}

/// `local` as a draft to save in the team database.
fn draft_of(local: &Script) -> ScriptDraft {
    let summary = &local.summary;

    ScriptDraft {
        name: summary.name.clone(),
        description: summary.description.clone(),
        language: summary.language,
        body: local.body.clone(),
        timeout_secs: summary.timeout_secs,
        provenance: summary.provenance.clone(),
    }
}

/// Counts `action` for `local` (against the team's `existing` script, if any) and lists it in
/// [`ScriptPush::changes`]; its index there.
fn record(
    summary: &mut ScriptPush,
    local: &Script,
    target: &Target,
    action: Action,
    conflict: bool,
    existing: Option<&Script>,
) -> usize {
    *total(summary, action) += 1;

    let repo = match target {
        Target::Repo(key) => Some(key.key.clone()),
        Target::Global => None,
    };

    summary.changes.push(ScriptChange {
        local_id: local.summary.id,
        scope: scope_of(target),
        repo,
        name: local.summary.name.clone(),
        action,
        conflict,
        team_id: existing.map(|theirs| theirs.summary.id),
        local_updated_at: local.summary.updated_at,
        local_sha256: local.summary.sha256.clone(),
        team_updated_at: existing.map(|theirs| theirs.summary.updated_at),
        deleted: false,
        changed_since: false,
        failed: false,
        trusted: false,
    });

    summary.changes.len() - 1
}

/// The total of `summary` that counts `action`.
fn total(summary: &mut ScriptPush, action: Action) -> &mut usize {
    match action {
        Action::Added => &mut summary.added,
        Action::Updated => &mut summary.updated,
        Action::Overwritten => &mut summary.overwritten,
        Action::Kept => &mut summary.kept,
        Action::Identical => &mut summary.identical,
        Action::TeamNewer => &mut summary.team_newer,
    }
}

/// Marks the change of the local script `local_id` failed (not trusted) and takes it out of its
/// total; nothing when it was not recorded yet (reading the team's script failed).
fn uncount(summary: &mut ScriptPush, local_id: i64) {
    let Some(change) = summary
        .changes
        .iter_mut()
        .rev()
        .find(|change| change.local_id == local_id)
    else {
        return;
    };

    change.failed = true;
    change.trusted = false;
    let action = change.action;

    let count = total(summary, action);
    *count = count.saturating_sub(1);
}

/// Deletes from the local file at `path` the scripts [`ScriptPush::changes`] says the team now
/// holds as they are here, leaving any whose push failed or that changed since they were read; how
/// many went. Nothing is deleted after a failure that names no script. What cannot be deleted is
/// added to [`ScriptPush::failed`].
fn delete_copied(path: &Path, summary: &mut ScriptPush) -> usize {
    if summary.failed.iter().any(|failure| failure.local_id.is_none()) {
        summary.failed.push(deleting_failure(
            None,
            "the copied scripts were not deleted from this computer, because something failed. Push again",
        ));
        return 0;
    }

    let mut store = match SqliteStore::open(path) {
        Ok(store) => store,
        Err(error) => {
            summary.failed.push(deleting_failure(
                None,
                &format!("the copied scripts were not deleted from this computer: {error}"),
            ));
            return 0;
        }
    };

    let failed: HashSet<i64> = summary.failed.iter().filter_map(|failure| failure.local_id).collect();
    let mut errors = Vec::new();
    let mut deleted = 0;

    for change in &mut summary.changes {
        if !change.action.copies() || change.failed || failed.contains(&change.local_id) {
            continue;
        }

        match delete_unchanged(&mut store, change) {
            Ok(true) => {
                change.deleted = true;
                deleted += 1;
            }
            Ok(false) => change.changed_since = true,
            Err(error) => errors.push(deleting_failure(
                Some(change.local_id),
                &format!(
                    "script {} {} was copied but not deleted from this computer: {error}",
                    change.local_id, change.name
                ),
            )),
        }
    }

    summary.failed.extend(errors);
    deleted
}

/// Deletes the local script of `change` unless it changed since the push read it (a server may
/// have written it meanwhile, or deleted it); true when it went, false when it changed.
fn delete_unchanged(store: &mut SqliteStore, change: &ScriptChange) -> Result<bool, StoreError> {
    let view = view_of(change.repo.clone());
    let script = ScriptRef::Id(change.local_id);

    let unchanged = store.get_script(&view, &script)?.is_some_and(|current| {
        current.summary.updated_at == change.local_updated_at
            && script_sha256(current.summary.language, &current.body) == change.local_sha256
    });

    if !unchanged {
        return Ok(false);
    }

    store.delete_script(&view, &script)
}

fn deleting_failure(local_id: Option<i64>, message: &str) -> Failure {
    Failure {
        local_id,
        message: message.to_owned(),
        deleting: true,
    }
}

/// The scope `target` writes to.
fn scope_of(target: &Target) -> Scope {
    match target {
        Target::Repo(_) => Scope::Repo,
        Target::Global => Scope::Global,
    }
}

/// The scripts of the local file at `path`, oldest first, with their repositories.
fn read_local(path: &Path) -> Result<Vec<(Script, Option<ProjectKey>)>, StoreError> {
    if !path.is_file() {
        return Err(StoreError::NotFound(format!(
            "there is no local memory database at {}",
            path.display()
        )));
    }

    // Opening migrates an older file, and reports a newer one.
    let mut store = SqliteStore::open(path)?;

    if store.is_newer() {
        return Err(StoreError::NewerSchema);
    }

    store.every_script()
}
