//! `harness-memory scripts pull`: copies team scripts to this computer for `scripts =
//! "remote+local"`, as `pull` does for notes ([`crate::pull`], whose [`Options`], [`Action`],
//! [`Change`], [`Failure`] and [`Summary`] it shares). The team database is only read.
//!
//! A team script is matched with the local script of the same name in the same scope and
//! repository. None: [`Action::Add`], the script is copied. The same version ([`script_sha256`] of
//! the language and body): [`Action::Identical`]. Another version: [`Action::LocalDiffers`]. Neither
//! of the last two is ever written.
//!
//! **Trust is never carried.** A copy is saved as a script of this computer and nothing trusts it:
//! not the approval this computer gave the team's script, not the setting that trusts team scripts
//! (which covers only scripts read from the team database), so it needs approval here before it
//! runs, like any script a person has not reviewed. The scripts are read as summaries in one read
//! (the trait has no pages), and only the body of a script that is copied is read by itself, after
//! every team read that may fail came first, so a team database that cannot be reached fails the run
//! with nothing copied.
//!
//! A team script hidden on this computer is never listed nor copied, even by id. The writer's
//! provenance and who added and last changed the script are kept; the description is masked and the
//! name, body and timeout are checked as in any local save ([`ScriptStore::save_script`]).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::pull::{Action, Change, Options, Summary, known_projects, write_target};
use crate::push_scripts::draft_of;
use crate::store::hidden::view_of;
use crate::store::{ProjectKey, Script, ScriptRef, ScriptStore, ScriptSummary, SqliteStore, StoreError, View};

/// Pulls from the team scripts `team` into the local file at `local` as `options` say, never
/// taking the team scripts `hidden` on this computer (their ids in `team`).
///
/// The local file is migrated first like the server would; a dry run never creates it, and a copy
/// creates it only when a script is to be written. A copy without [`Options::ids`] is refused.
pub fn pull_scripts(
    local: &Path,
    team: &mut dyn ScriptStore,
    hidden: &[i64],
    options: &Options,
) -> Result<Summary, StoreError> {
    if !options.dry_run && options.ids.is_none() {
        return Err(StoreError::Invalid(
            "pull copies only the scripts it is given: list them with --dry-run, then name them with --ids".to_owned(),
        ));
    }

    let hidden: HashSet<i64> = hidden.iter().copied().collect();
    let mut summary = Summary::default();

    // Ids are told from the scripts outside the scopes taken, so they are looked for everywhere.
    let view = if options.ids.is_some() {
        View::every_repo()
    } else {
        options.team_view()
    };

    let listed = team.list_scripts(&view, None)?;
    let taken = taken(listed, &hidden, options, &mut summary);

    let mut local_store = open_existing(local)?;
    let mut adds: Vec<(usize, &ScriptSummary)> = Vec::new();

    for theirs in &taken {
        let seen = match local_store.as_mut() {
            Some(store) => store.get_script(&view_of(theirs.project.clone()), &ScriptRef::Name(theirs.name.clone()))?,
            None => None,
        };

        let action = match &seen {
            None => Action::Add,
            Some(mine) if mine.summary.sha256 == theirs.sha256 => Action::Identical,
            Some(_) => Action::LocalDiffers,
        };

        let at = summary.record(Change {
            team_id: theirs.id,
            local_id: seen.as_ref().map(|mine| mine.summary.id),
            repo: theirs.project.clone(),
            title: theirs.name.clone(),
            action,
            team_updated_at: theirs.updated_at,
            local_updated_at: seen.as_ref().map(|mine| mine.summary.updated_at),
            failed: false,
        });

        if action == Action::Add {
            adds.push((at, theirs));
        }
    }

    if options.dry_run || adds.is_empty() {
        return Ok(summary);
    }

    // Every read of the team database comes before the first write here.
    let mut bodies: Vec<(usize, Script)> = Vec::new();

    for (at, theirs) in adds {
        let read = team.get_script(&view_of(theirs.project.clone()), &ScriptRef::Id(theirs.id))?;

        match read {
            Some(script) => bodies.push((at, script)),
            None => {
                summary.uncount(at);
                summary.fail(
                    theirs.id,
                    format!(
                        "team script {} {} disappeared while it was read",
                        theirs.id, theirs.name
                    ),
                );
            }
        }
    }

    let mut store = match local_store {
        Some(store) => store,
        None => open_for_writing(local)?,
    };
    let projects = known_projects(local);

    for (at, script) in bodies {
        copy_script(&mut store, &projects, &script, at, &mut summary);
    }

    Ok(summary)
}

/// The team scripts to take, oldest change first: those `options` take that are not hidden, or the
/// ones its ids name, in the order given. An id that is hidden here, names no script, or names one
/// outside the scopes taken fails (in `summary`) and is left out.
fn taken(
    listed: Vec<ScriptSummary>,
    hidden: &HashSet<i64>,
    options: &Options,
    summary: &mut Summary,
) -> Vec<ScriptSummary> {
    let Some(ids) = &options.ids else {
        let mut scripts: Vec<ScriptSummary> = listed
            .into_iter()
            .filter(|script| !hidden.contains(&script.id) && options.refusal(script.project.as_deref()).is_none())
            .collect();

        scripts.sort_by_key(|script| (script.updated_at, script.id));
        return scripts;
    };

    let mut by_id: HashMap<i64, ScriptSummary> = listed.into_iter().map(|script| (script.id, script)).collect();
    let mut seen = HashSet::new();
    let mut scripts = Vec::new();

    for id in ids.iter().copied().filter(|id| seen.insert(*id)) {
        if hidden.contains(&id) {
            summary.fail(
                id,
                format!(
                    "team script {id} is hidden on this computer, so it is not copied: show it again with \
                     harness-memory scripts unhide {id}"
                ),
            );
            continue;
        }

        let Some(script) = by_id.remove(&id) else {
            summary.fail(id, format!("team script {id} was not found in the team database"));
            continue;
        };

        if let Some(reason) = options.refusal(script.project.as_deref()) {
            summary.fail(id, format!("team script {id} {} is not taken: {reason}", script.name));
            continue;
        }

        scripts.push(script);
    }

    scripts
}

/// The local file at `path` when there is one (migrated, never created); `None` otherwise.
fn open_existing(path: &Path) -> Result<Option<SqliteStore>, StoreError> {
    if !path.is_file() {
        return Ok(None);
    }

    open_for_writing(path).map(Some)
}

/// The local file at `path`, created when missing.
fn open_for_writing(path: &Path) -> Result<SqliteStore, StoreError> {
    let store = SqliteStore::open(path)?;

    if store.is_newer() {
        return Err(StoreError::NewerSchema);
    }

    Ok(store)
}

/// Saves the team's `script` on this computer. Nothing is trusted: the copy is a new script here
/// that no approval covers.
fn copy_script(
    store: &mut SqliteStore,
    projects: &HashMap<String, ProjectKey>,
    script: &Script,
    at: usize,
    summary: &mut Summary,
) {
    let target = write_target(projects, script.summary.project.as_deref());
    let (id, name) = (script.summary.id, &script.summary.name);

    match store.save_script(&target, &draft_of(script)) {
        Ok(saved) if saved.created => summary.changes[at].local_id = Some(saved.script.summary.id),
        Ok(_) => {
            summary.uncount(at);
            summary.fail(
                id,
                format!(
                    "team script {id} {name}: a script of that name was saved on this computer meanwhile and was \
                     changed by the copy"
                ),
            );
        }
        Err(error) => {
            summary.uncount(at);
            summary.fail(id, format!("team script {id} {name}: {error}"));
        }
    }
}
