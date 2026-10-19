//! The stores the server uses. The notes store ([`open_notes`]) opens at the start. The script
//! store and the trust store open on their first call ([`Lazy`]), so a server whose agent runs no
//! script neither connects to the team database for scripts nor creates the local file; a failed
//! open is a tool error, not a stopped server.

use std::path::Path;

use crate::backend::{self, Backend, NotesPlace, Selection};
use crate::location::{DIR_VAR, Migration, legacy_database_path, migrate_legacy, override_is_relative};
use crate::store::{
    HiddenNotes, HiddenScripts, Lazy, MemoryStore, Scope, ScriptStore, SplitScripts, SplitStore, SqliteStore,
    TrustStore,
};
use crate::support::Env;

/// The scripts: in the local SQLite file without a team database or with `scripts = "local"`
/// ([`Selection::scripts_place`]), else a [`SplitScripts`] reading the team database (its own
/// connection), and this computer's file too with `remote+local`, leaving out the team scripts
/// hidden on this computer ([`open_hidden_scripts`]).
pub fn open_scripts(env: &Env, selection: &Selection) -> Box<dyn ScriptStore> {
    let place = selection.scripts_place();
    let local = local_scripts(env);

    if place == NotesPlace::Local {
        return local;
    }

    let team = team_scripts(&selection.backend);
    let hidden = open_hidden_scripts(env, selection);
    Box::new(SplitScripts::new(team, local, place, hidden))
}

/// The script store scripts are not read from, when there is one: the team database when they
/// stay on this computer, else the local SQLite file. `script_save` looks there for a script that
/// waits for approval, so copying one does not trust it. `None` without a team database, with
/// `remote+local` (both are read already), or when scripts and notes all stay on this computer
/// (the team database is then never contacted).
pub fn open_other_scripts(env: &Env, selection: &Selection) -> Option<Box<dyn ScriptStore>> {
    if selection.backend == Backend::Local {
        return None;
    }

    match selection.scripts_place() {
        NotesPlace::RemoteLocal => None,
        NotesPlace::Remote => Some(local_scripts(env)),
        NotesPlace::Local if selection.routing.is_all_local() => None,
        NotesPlace::Local => Some(team_scripts(&selection.backend)),
    }
}

/// The team scripts hidden on this computer, in the local SQLite file under the team database's
/// [`Backend::team_key`]; `None` when scripts are not read from the team database or the data
/// folder is unknown. Nothing is opened (or created) yet.
pub fn open_hidden_scripts(env: &Env, selection: &Selection) -> Option<HiddenScripts> {
    if !selection.scripts_place().reads_team() {
        return None;
    }

    let path = crate::database_path(env)?;
    Some(HiddenScripts::new(path, selection.backend.team_key()))
}

/// The scripts of the local SQLite file, opened on first use.
fn local_scripts(env: &Env) -> Box<dyn ScriptStore> {
    let path = crate::database_path(env);
    let env = env.clone();

    Box::new(Lazy::new(move || {
        open_local(&env, path.as_deref()).map(|store| Box::new(store) as Box<dyn ScriptStore>)
    }))
}

/// The scripts of the team database `backend`, opened on first use.
fn team_scripts(backend: &Backend) -> Box<dyn ScriptStore> {
    let team = backend.clone();

    Box::new(Lazy::new(move || {
        backend::open_scripts(&team).unwrap_or_else(|| Err("no team database is set".to_owned()))
    }))
}

/// The script versions this machine trusts: always the local SQLite file, wherever scripts are.
pub fn open_trust(env: &Env) -> Box<dyn TrustStore> {
    let path = crate::database_path(env);
    let env = env.clone();

    Box::new(Lazy::new(move || {
        open_local(&env, path.as_deref()).map(|store| Box::new(store) as Box<dyn TrustStore>)
    }))
}

/// The local SQLite file at `path`, after the one-time move from Harness Desktop's folder (a
/// failed move leaves the legacy file and starts empty, as for notes).
fn open_local(env: &Env, path: Option<&Path>) -> Result<SqliteStore, String> {
    let Some(path) = path else {
        return Err("the data folder is unknown".to_owned());
    };

    // The notes store reports a failed move when it uses this file; scripts start without it.
    let _moved: Migration = migrate_legacy(env, path);

    SqliteStore::open(path).map_err(|error| error.to_string())
}

/// The notes store the server and `harness-memory notes` use: the local one, or the team database
/// with the local file as [`crate::backend::Routing`] says (a [`SplitStore`], which also leaves out
/// the team notes hidden on this computer, see [`open_hidden`]); `None` (with the reason printed)
/// when one cannot be opened. With every scope in the team database the local file is only opened
/// when a call needs it, which none does.
pub fn open_notes(env: &Env, selection: &Selection) -> Option<Box<dyn MemoryStore>> {
    let Selection { backend, routing, .. } = selection;

    if *backend == Backend::Local || routing.is_all_local() {
        return open_local_notes(env);
    }

    let team = open_team_notes(backend)?;

    let local: Box<dyn MemoryStore> = if routing.needs_both_stores() {
        open_local_notes(env)?
    } else {
        let env = env.clone();
        Box::new(Lazy::new(move || {
            open_local_notes(&env).ok_or_else(|| "cannot open the local database".to_owned())
        }))
    };

    let hidden = open_hidden(env, selection);
    Some(Box::new(SplitStore::routed(
        team,
        local,
        routing.repository,
        routing.global,
        hidden,
    )))
}

/// Where the notes of `scope` are read and saved under `selection`, as [`open_notes`] opens them.
pub fn notes_place(selection: &Selection, scope: Scope) -> NotesPlace {
    let Selection { backend, routing, .. } = selection;

    if *backend == Backend::Local || routing.is_all_local() {
        return NotesPlace::Local;
    }

    match scope {
        Scope::Repo => routing.repository,
        Scope::Global => routing.global,
    }
}

/// The team notes hidden on this computer, in the local SQLite file under the team database's
/// [`Backend::team_key`]; `None` when no scope reads the team database or the data folder is
/// unknown. Nothing is opened (or created) yet.
pub fn open_hidden(env: &Env, selection: &Selection) -> Option<HiddenNotes> {
    let reads_team = [Scope::Repo, Scope::Global]
        .into_iter()
        .any(|scope| notes_place(selection, scope).reads_team());

    if !reads_team {
        return None;
    }

    let path = crate::database_path(env)?;
    Some(HiddenNotes::new(path, selection.backend.team_key()))
}

/// The team database; `None` (with the reason printed) when it cannot be opened. That stops the
/// server: an empty local database instead would look like the team's memory had gone.
fn open_team_notes(backend: &Backend) -> Option<Box<dyn MemoryStore>> {
    match backend::open(backend)? {
        Ok(opened) => {
            if opened.newer {
                eprintln!("harness-memory: the team database is newer than this server; tools will fail");
            }

            Some(opened.store)
        }
        Err(message) => {
            eprintln!("harness-memory: {message}");
            None
        }
    }
}

/// The local SQLite store, after the one-time move from Harness Desktop's folder; `None` (with
/// the reason printed) when it cannot be opened.
fn open_local_notes(env: &Env) -> Option<Box<dyn MemoryStore>> {
    let Some(path) = crate::database_path(env) else {
        eprintln!("harness-memory: the data folder is unknown");
        return None;
    };

    if override_is_relative(env) {
        eprintln!(
            "harness-memory: {DIR_VAR} must be an absolute folder; using {}",
            path.display()
        );
    }

    // A failed move is not fatal: the server starts on an empty database and the legacy one stays.
    if let Migration::Failed(reason) = migrate_legacy(env, &path) {
        eprintln!(
            "harness-memory: could not copy the Harness Desktop memory database ({reason}); starting empty. \
             The old notes are still in {}; to retry, stop every harness-memory server and delete {}",
            legacy_database_path(env)
                .map(|legacy| legacy.display().to_string())
                .unwrap_or_default(),
            path.display()
        );
    }

    let store = match SqliteStore::open(&path) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("harness-memory: {error}");
            return None;
        }
    };

    if store.is_newer() {
        eprintln!("harness-memory: memory database is newer than this server; tools will fail");
    }

    Some(Box::new(store))
}
