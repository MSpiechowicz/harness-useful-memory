//! `scripts hide`, `scripts unhide`, and the hidden team scripts `scripts list --hidden` and
//! `scripts get --hidden` show. The hidden list is the local file's ([`crate::store::hidden`]); the
//! scripts themselves are read from the team database directly, past the store that leaves them
//! out.
//!
//! - `scripts hide <id> [--project PATH] [--json]` hides a visible team script on this computer
//!   (the team database is not changed) and prints `hid script <id> <name>`, or `{"hidden": <id>}`.
//! - `scripts unhide <id> [--project PATH] [--json]` shows it again: `unhid script <id>`, or
//!   `{"unhidden": <id>}`.
//!
//! A script saved on this computer is refused (delete it instead), an id that is not a visible
//! (or, for `unhide`, hidden) team script is not found, and without scripts read from a team
//! database there is nothing to hide: each exits 1.

use std::process::ExitCode;

use serde_json::json;

use super::{Opened, not_found};
use crate::backend;
use crate::cli;
use crate::store::split::is_local_id;
use crate::store::{HiddenEntry, Scope, Script, ScriptRef, ScriptStore, ScriptSummary, StoreError, clean};

/// `scripts hide <id>`.
pub(crate) fn hide(args: &[Option<&str>]) -> ExitCode {
    let Some((id, repos, json)) = cli::id_options(args) else {
        return crate::cli_trust::usage();
    };

    let Some(mut opened) = Opened::open(&repos) else {
        return ExitCode::FAILURE;
    };

    if let Some(refused) = refusal(&opened, id) {
        return refused;
    }

    match hide_script(&mut opened, id) {
        Ok(Some(name)) => {
            let line = format!("hid script {id} {}", clean::line(&name));
            cli::print_either(json, json!({ "hidden": id }), &line);
            ExitCode::SUCCESS
        }
        Ok(None) => not_found(id),
        Err(error) => opened.failed(&error.to_string()),
    }
}

/// `scripts unhide <id>`.
pub(crate) fn unhide(args: &[Option<&str>]) -> ExitCode {
    let Some((id, repos, json)) = cli::id_options(args) else {
        return crate::cli_trust::usage();
    };

    let Some(mut opened) = Opened::open(&repos) else {
        return ExitCode::FAILURE;
    };

    if let Some(refused) = refusal(&opened, id) {
        return refused;
    }

    let Some(hidden) = opened.hidden.as_mut() else {
        return not_found(id);
    };

    match hidden.unhide(id) {
        Ok(true) => {
            cli::print_either(json, json!({ "unhidden": id }), &format!("unhid script {id}"));
            ExitCode::SUCCESS
        }
        Ok(false) => not_found(id),
        Err(error) => opened.failed(&error.to_string()),
    }
}

/// Why `id` cannot be hidden or shown again here, printed (exit 1); `None` when it can be.
fn refusal(opened: &Opened, id: i64) -> Option<ExitCode> {
    if opened.hidden.is_none() {
        eprintln!(
            "harness-memory: no team scripts are read here (no team database is set, or scripts is \"local\"), so \
             there is no team script to hide"
        );
        return Some(ExitCode::FAILURE);
    }

    if is_local_id(id) {
        eprintln!("harness-memory: script {id} is saved on this computer, delete it instead");
        return Some(ExitCode::FAILURE);
    }

    None
}

/// Hides the visible team script `id` (one already hidden stays so); its name, `None` when there
/// is no such script.
fn hide_script(opened: &mut Opened, id: i64) -> Result<Option<String>, StoreError> {
    if let Some(known) = in_view(opened).into_iter().find(|hidden| hidden.id == id) {
        return Ok(Some(known.label));
    }

    let Some(script) = opened.scripts.get_script(&opened.view, &ScriptRef::Id(id))? else {
        return Ok(None);
    };

    if let Some(hidden) = opened.hidden.as_mut() {
        hidden.hide(&script.summary)?;
    }

    Ok(Some(script.summary.name))
}

/// The hidden team scripts of the view: the repository of the view for repository scripts, and
/// the global ones.
fn in_view(opened: &mut Opened) -> Vec<HiddenEntry> {
    let Some(hidden) = opened.hidden.as_mut() else {
        return Vec::new();
    };

    let repo = opened.view.repo.as_ref().map(|key| key.key.as_str());
    let every = opened.view.sees_every_repo();

    hidden
        .list()
        .into_iter()
        .filter(|entry| match entry.scope {
            Scope::Repo => entry
                .project
                .as_deref()
                .is_some_and(|project| every || Some(project) == repo),
            Scope::Global => opened.view.global,
        })
        .collect()
}

/// How many team scripts of the view are hidden, as the store leaves them out: the rows of scripts
/// the team database no longer has (or gave their id to a newer script) leave the list first when
/// a prune is due ([`crate::store::HiddenScripts::prune_if_due`], which the store's reads run too).
pub(crate) fn count(opened: &mut Opened) -> Result<usize, StoreError> {
    let due = opened.hidden.as_mut().is_some_and(|hidden| hidden.prune_due());

    if due {
        let mut team = team_store(opened)?;

        if let Some(hidden) = opened.hidden.as_mut() {
            hidden.prune_if_due(team.as_mut())?;
        }
    }

    Ok(in_view(opened).len())
}

/// The hidden team scripts of the view that `query` keeps, read from the team database; a hidden
/// script the team database no longer has (or gave its id to a newer script) leaves the list.
pub(crate) fn listed(opened: &mut Opened, query: Option<&str>) -> Result<Vec<ScriptSummary>, StoreError> {
    let entries = in_view(opened);

    if entries.is_empty() {
        return Ok(Vec::new());
    }

    let team_scripts = team_store(opened)?.list_scripts(&opened.view, None)?;
    let (found, gone) = matched(&entries, team_scripts);
    forget(opened, &gone);

    let needle = query.map(str::to_lowercase);

    Ok(found
        .into_iter()
        .filter(|summary| {
            needle.as_ref().is_none_or(|needle| {
                summary.name.to_lowercase().contains(needle) || summary.description.to_lowercase().contains(needle)
            })
        })
        .collect())
}

/// The hidden team script `id` of the view with its body, read from the team database; `None`
/// when it is not hidden, or gone (then it leaves the hidden list).
pub(crate) fn script(opened: &mut Opened, id: i64) -> Result<Option<Script>, StoreError> {
    let Some(entry) = in_view(opened).into_iter().find(|entry| entry.id == id) else {
        return Ok(None);
    };

    let read = team_store(opened)?.get_script(&opened.view, &ScriptRef::Id(id))?;

    match read {
        Some(script) if script.summary.created_at == entry.team_created_at => Ok(Some(script)),
        _ => {
            forget(opened, &[id]);
            Ok(None)
        }
    }
}

/// The scripts of `listed` the hidden `entries` name (same id, created at the same time), and the
/// ids of the entries none matches.
fn matched(entries: &[HiddenEntry], listed: Vec<ScriptSummary>) -> (Vec<ScriptSummary>, Vec<i64>) {
    let same = |entry: &HiddenEntry, summary: &ScriptSummary| {
        summary.id == entry.id && summary.created_at == entry.team_created_at
    };

    let gone = entries
        .iter()
        .filter(|entry| !listed.iter().any(|summary| same(entry, summary)))
        .map(|entry| entry.id)
        .collect();
    let found = listed
        .into_iter()
        .filter(|summary| entries.iter().any(|entry| same(entry, summary)))
        .collect();

    (found, gone)
}

/// Drops `gone` from the hidden list.
fn forget(opened: &mut Opened, gone: &[i64]) {
    if let Some(list) = opened.hidden.as_mut().filter(|_| !gone.is_empty()) {
        list.remove(gone);
    }
}

/// The team database itself, past the store that leaves the hidden scripts out.
fn team_store(opened: &Opened) -> Result<Box<dyn ScriptStore>, StoreError> {
    match backend::open_scripts(&opened.selection.backend) {
        Some(Ok(team)) => Ok(team),
        Some(Err(message)) => Err(StoreError::Backend(message)),
        None => Err(StoreError::Invalid("no team database is set".to_owned())),
    }
}
