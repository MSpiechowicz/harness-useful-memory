//! `harness-memory sync --json`: copies what was saved on this computer while the team database was
//! unreachable (the offline fallback, see [`crate::store::pending`] and
//! [`crate::store::pending_updates`]), now, instead of at the next call of a server, and says what
//! still waits. Harness Desktop runs it when it sees the team database again.
//!
//! It prints `{"online": bool, "waiting": {"notes": N, "scripts": N, "updates": N}}` (exit 0, also
//! while the team database is unreachable): the notes and scripts saved offline that wait to be
//! copied, and the changes to team notes that wait to be applied, after one attempt. `online` is
//! false when the team database could not be reached.
//!
//! When nothing waits it opens nothing and makes no connection: `online` is then only whether the
//! last failed contact with the team database ([`crate::store::Reach`]) is still recent, since
//! nothing needs the database to be asked.

use std::path::Path;
use std::process::ExitCode;

use serde_json::json;

use crate::backend::{Backend, Selection};
use crate::cli::{self, Args};
use crate::open::{open_notes, open_scripts, reach_of};
use crate::push::GLOBAL;
use crate::store::pending::{Kind, Pending};
use crate::store::pending_updates::Queue;
use crate::support::Env;

/// The usage of the command, for the executable's usage line.
pub const USAGE: &str = "sync --json";

/// What waits for the team database.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Waiting {
    notes: usize,
    scripts: usize,
    updates: usize,
}

impl Waiting {
    fn none(self) -> bool {
        self.notes == 0 && self.scripts == 0 && self.updates == 0
    }

    fn json(self) -> serde_json::Value {
        json!({ "notes": self.notes, "scripts": self.scripts, "updates": self.updates })
    }
}

/// `harness-memory sync <args>`.
pub fn sync(args: &[Option<&str>]) -> ExitCode {
    let Some(parsed) = Args::parse(args, &["--json"], &[]) else {
        return cli::usage(USAGE);
    };

    if !parsed.flag("--json") || !parsed.positional.is_empty() {
        return cli::usage(USAGE);
    }

    let Some((env, selection)) = cli::settings() else {
        return ExitCode::FAILURE;
    };

    // Without the offline fallback, or a team database, nothing is ever kept for it.
    let database =
        crate::database_path(&env).filter(|_| selection.offline_fallback && selection.backend != Backend::Local);

    let Some(database) = database else {
        return report(true, Waiting::default());
    };

    let waiting = count(&database, &selection);

    if waiting.none() {
        let online = reach_of(&env, &selection).cooling().is_none();
        return report(online, waiting);
    }

    let Some(online) = settle(&env, &selection) else {
        return ExitCode::FAILURE;
    };

    report(online, count(&database, &selection))
}

/// Prints the answer.
fn report(online: bool, waiting: Waiting) -> ExitCode {
    println!("{}", json!({ "online": online, "waiting": waiting.json() }));
    ExitCode::SUCCESS
}

/// What waits next to `database` for the team database of `selection`, as a server would copy it:
/// the notes and scripts of a scope that still saves to the team database (`remote`, or `remote+local`
/// with `remote_local_saves` `remote`), and every queued change.
fn count(database: &Path, selection: &Selection) -> Waiting {
    let pending = Pending::beside(database);
    let team = selection.backend.team_key();
    let secrets = selection.backend.secrets();

    let notes = pending
        .entries(Kind::Notes, &team)
        .into_iter()
        .filter(|entry| {
            let place = if entry.scope == GLOBAL {
                selection.routing.global
            } else {
                selection.routing.repository
            };
            place.writes_team(selection.routing.remote_local_saves)
        })
        .count();

    let scripts = if selection
        .scripts_place()
        .writes_team(selection.routing.remote_local_saves)
    {
        pending.entries(Kind::Scripts, &team).len()
    } else {
        0
    };

    let updates = match Queue::beside(database).entries(&team) {
        Ok(waiting) => waiting.len(),
        Err(message) => {
            eprintln!("harness-memory: {}", secrets.scrub(&message));
            0
        }
    };

    Waiting {
        notes,
        scripts,
        updates,
    }
}

/// Opens the stores the way the server does and lets each settle once: it copies what was saved
/// offline and applies the queued changes when the team database is back. Whether it was reachable;
/// `None`, with the reason printed, when the notes store cannot be opened.
fn settle(env: &Env, selection: &Selection) -> Option<bool> {
    let mut notes = open_notes(env, selection)?;
    let notes_up = notes.ready();

    // Scripts only go to the team database when they are read from it.
    if !selection.scripts_place().reads_team() {
        return Some(notes_up);
    }

    let scripts_up = open_scripts(env, selection).ready();

    Some(notes_up && scripts_up)
}
