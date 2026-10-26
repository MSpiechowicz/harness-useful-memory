//! The stores the server uses. The notes store ([`open_notes`]) opens at the start. The script
//! store and the trust store open on their first call ([`Lazy`]), so a server whose agent runs no
//! script neither connects to the team database for scripts nor creates the local file; a failed
//! open is a tool error, not a stopped server.

use std::path::{Path, PathBuf};

use crate::Server;
use crate::backend::{self, Backend, NotesPlace, Place, Selection};
use crate::location::{DIR_VAR, Migration, legacy_database_path, migrate_legacy, override_is_relative};
use crate::mcp::Vectors;
use crate::project::ProjectResolver;
use crate::store::{
    HiddenNotes, HiddenScripts, Lazy, MemoryStore, Reach, Scope, ScriptStore, SplitScripts, SplitStore, SqliteStore,
    TrustStore,
};
use crate::support::Env;

/// The MCP server over the stores `selection` names, started in `cwd`; `None` (with the reason
/// printed) when the notes store cannot be opened.
pub fn open_server(env: &Env, selection: &Selection, cwd: Option<PathBuf>) -> Option<Server> {
    let store = open_notes(env, selection)?;
    let scripts = open_scripts(env, selection);
    let other_scripts = open_other_scripts(env, selection);
    let trust = open_trust(env);
    let projects = ProjectResolver::new(env.clone(), cwd);
    let mut server = Server::new(store, scripts, trust, projects)
        .with_scripts_place(selection.scripts_place())
        .with_team_key(selection.backend.team_key())
        .with_trust_team_scripts(selection.team_scripts_trusted());

    if let Some(other) = other_scripts {
        server = server.with_other_scripts(other);
    }

    if let Some(vectors) = Vectors::for_selection(env, selection) {
        server = server.with_vectors(vectors);
    }

    Some(server)
}

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

    let team = team_scripts(&selection.backend, Reach::default(), selection.offline_fallback);
    let hidden = open_hidden_scripts(env, selection);
    let split = SplitScripts::new(team, local, place, hidden);

    match crate::database_path(env).filter(|_| selection.offline_fallback) {
        Some(database) => {
            let key = selection.scripts_trust_key(Place::Remote);
            let secrets = selection.backend.secrets();
            Box::new(split.with_offline_fallback(&database, open_trust(env), key, secrets))
        }
        None => Box::new(split),
    }
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
        NotesPlace::Local => Some(team_scripts(
            &selection.backend,
            Reach::default(),
            selection.offline_fallback,
        )),
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

/// The scripts of the team database `backend`, opened on first use; the failed open starts the
/// cooldown of `reach`. Without the offline fallback the failure names what to change.
fn team_scripts(backend: &Backend, reach: Reach, fallback: bool) -> Box<dyn ScriptStore> {
    let team = backend.clone();

    let store = Lazy::new(move || match backend::open_scripts(&team) {
        Some(Err(message)) if fallback => {
            eprintln!("harness-memory: {message}");
            Err(message)
        }
        Some(Err(message)) => Err(format!("{message}. {SCRIPTS_HINT}")),
        Some(Ok(store)) => Ok(store),
        None => Err("no team database is set".to_owned()),
    });

    Box::new(store.with_reach(reach))
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

    if local_alone(selection) {
        return open_local_notes(env);
    }

    let reach = Reach::default();
    let team = open_team_notes(backend, reach, selection.offline_fallback);

    let local: Box<dyn MemoryStore> = if routing.needs_both_stores() {
        open_local_notes(env)?
    } else {
        let env = env.clone();
        Box::new(Lazy::new(move || {
            open_local_notes(&env).ok_or_else(|| "cannot open the local database".to_owned())
        }))
    };

    let hidden = open_hidden(env, selection);
    let split = SplitStore::routed(team, local, routing.repository, routing.global, hidden);

    match crate::database_path(env).filter(|_| selection.offline_fallback) {
        Some(database) => {
            let team_key = selection.backend.team_key();
            let secrets = selection.backend.secrets();
            Some(Box::new(split.with_offline_fallback(&database, team_key, secrets)))
        }
        None => Some(Box::new(split)),
    }
}

/// Where the notes of `scope` are read and saved under `selection`, as [`open_notes`] opens them.
pub fn notes_place(selection: &Selection, scope: Scope) -> NotesPlace {
    if local_alone(selection) {
        return NotesPlace::Local;
    }

    match scope {
        Scope::Repo => selection.routing.repository,
        Scope::Global => selection.routing.global,
    }
}

/// Where every note is kept when [`open_notes`] opens one store alone (the local file, without a
/// team database or with every scope routed `local`); `None` when it opens a [`SplitStore`], which
/// tells its notes apart itself.
pub fn lone_notes_place(selection: &Selection) -> Option<Place> {
    local_alone(selection).then_some(Place::Local)
}

/// True when [`open_notes`] opens the local file alone.
fn local_alone(selection: &Selection) -> bool {
    selection.backend == Backend::Local || selection.routing.is_all_local()
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

/// The team database, opened by the first call that needs it, so an unreachable one (VPN off,
/// server down) never stops the server. With `fallback` the [`SplitStore`] serves this computer's
/// notes meanwhile; without it the calls that need the team database fail with the reason and
/// what to change, and the next call tries again once `reach`'s cooldown has ended.
fn open_team_notes(backend: &Backend, reach: Reach, fallback: bool) -> Box<dyn MemoryStore> {
    let backend = backend.clone();
    let mut warned_newer = false;

    let team = Lazy::new(move || match backend::open(&backend) {
        Some(Ok(opened)) => {
            if opened.newer && !warned_newer {
                warned_newer = true;
                eprintln!("harness-memory: the team database is newer than this server; tools will fail");
            }

            Ok(opened.store)
        }
        Some(Err(message)) if fallback => {
            eprintln!("harness-memory: {message}");
            Err(message)
        }
        Some(Err(message)) => {
            eprintln!("harness-memory: {message}");
            Err(format!("{message}. {OFFLINE_HINT}"))
        }
        None => Err("no team database is set".to_owned()),
    });

    Box::new(team.with_reach(reach))
}

/// What a user can do when the team database is unreachable (VPN off, server down), the notes are
/// set to `remote` and `offline_fallback` is off.
const OFFLINE_HINT: &str = "Notes in scopes set to \"remote\" are only in the team database, so they are \
     unavailable until it is reachable again (connect to the VPN or network; the next call retries). \
     To keep working now, turn on the offline fallback (offline_fallback in Harness Desktop's memory \
     settings), which serves this computer's notes meanwhile, or set repository_notes and global_notes to \
     \"local\"";

/// The same for scripts set to `remote`.
const SCRIPTS_HINT: &str = "Scripts set to \"remote\" are only in the team database, so they are \
     unavailable until it is reachable again (connect to the VPN or network; the next call retries). \
     To keep working now, turn on the offline fallback (offline_fallback in Harness Desktop's memory \
     settings), which serves this computer's scripts meanwhile, or set scripts to \"local\"";

/// The same for the team hooks or routines (`kind` is `hooks` or `routines`), which exist only in the
/// team database.
pub fn docs_hint(kind: &str) -> String {
    format!(
        "Team {kind} are shared through the team database, so they are unavailable until it is reachable \
         again (connect to the VPN or network; the next call retries). To work without them, set {kind} to \
         \"local\" in Harness Desktop's memory settings"
    )
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
