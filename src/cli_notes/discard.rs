//! `notes discard-local (--all | ID[,ID...] | -) [--scope repo|global] --yes [--json]`: deletes
//! notes from the local file in the scopes routed `remote`, where nothing reads them (left there
//! from before the routing changed, or after `push` copied them). `--all` takes every such note,
//! every repository's; ids are the local file's own (`localId` in `push --json`), given as one
//! comma-separated argument or, with `-`, read from stdin (separated by commas or blanks, at most
//! [`crate::limits::STDIN_IDS_BYTES_MAX`] bytes and [`crate::limits::IDS_MAX`] ids, exit 2 beyond);
//! an id of another scope, or of no note, is left alone. `--scope` takes one scope, which has to be
//! routed `remote`; without it every scope that is. Each note goes as `note_delete` deletes one, by
//! its id as read (also for `--all`), so a note saved after the list was read is never deleted.
//!
//! A note saved while the team database was unreachable that still waits to be copied to it is
//! left (see [`crate::cli_discard`]), and counted in the `waiting` of the answer
//! (`{"deleted": N, "waiting": M}`).
//!
//! Without `--yes` nothing is deleted and it exits 2; a scope not routed `remote` exits 1, and so
//! does a list of waiting notes that exists but cannot be read (nothing is deleted).

use std::process::ExitCode;

use crate::backend::{Backend, NotesPlace, Selection};
use crate::cli;
use crate::cli_discard::{self, Discard, Discarded, Waiting, Wanted};
use crate::open::notes_place;
use crate::store::hidden::view_of;
use crate::store::pending::Kind;
use crate::store::{Forget, MemoryStore, Scope, SqliteStore, StoreError, Target};

/// `notes discard-local`.
pub(super) fn discard_local(args: &[Option<&str>]) -> ExitCode {
    let Some(options) = Discard::parse(args) else {
        return super::usage();
    };

    if !options.yes {
        return cli_discard::unconfirmed("note");
    }

    let Some((env, selection)) = cli::settings() else {
        return ExitCode::FAILURE;
    };

    let scopes = match scopes(&selection, options.scope) {
        Ok(scopes) => scopes,
        Err(reason) => {
            eprintln!("harness-memory: {reason}");
            return ExitCode::FAILURE;
        }
    };

    let Some(path) = crate::database_path(&env) else {
        eprintln!("harness-memory: the data folder is unknown");
        return ExitCode::FAILURE;
    };

    let waiting = match Waiting::of(Kind::Notes, &path, &selection) {
        Ok(waiting) => waiting,
        Err(reason) => return cli_discard::unreadable(&reason),
    };

    let discarded = if path.is_file() {
        SqliteStore::open(&path).and_then(|mut store| discard(&mut store, &scopes, &options.wanted, &waiting))
    } else {
        Ok(Discarded::default())
    };

    match discarded {
        Ok(discarded) => {
            cli_discard::report("note", discarded, options.json);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("harness-memory: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The scopes to discard from: `asked`, else every scope routed `remote`; why not when one is not.
fn scopes(selection: &Selection, asked: Option<Scope>) -> Result<Vec<Scope>, String> {
    if selection.backend == Backend::Local {
        return Err(
            "no team database is set, so every note is read from this computer and none is discarded".to_owned(),
        );
    }

    let remote = |scope: Scope| notes_place(selection, scope) == NotesPlace::Remote;

    if let Some(scope) = asked {
        if !remote(scope) {
            let (words, key) = names(scope);
            let place = notes_place(selection, scope).as_str();
            return Err(format!(
                "{words} are read from this computer ({key} = \"{place}\"), and discard-local only deletes the \
                 notes of a scope routed \"remote\""
            ));
        }

        return Ok(vec![scope]);
    }

    let scopes: Vec<Scope> = [Scope::Repo, Scope::Global]
        .into_iter()
        .filter(|scope| remote(*scope))
        .collect();

    if scopes.is_empty() {
        return Err(
            "no note scope is routed \"remote\", so this computer's notes are all read and none is discarded"
                .to_owned(),
        );
    }

    Ok(scopes)
}

/// `repository notes` and `repository_notes`, or the global ones.
fn names(scope: Scope) -> (&'static str, &'static str) {
    match scope {
        Scope::Repo => ("repository notes", "repository_notes"),
        Scope::Global => ("global notes", "global_notes"),
    }
}

/// The notes a discard is about to delete, read from the local file: by id, with their repository
/// key (`None`: global), and how many were left because they still wait for the team database.
struct Plan {
    notes: Vec<(i64, Option<String>)>,
    waiting: usize,
}

/// Reads the notes of `scopes` in `store` that `wanted` names, apart from those still `waiting` for
/// the team database.
fn plan(store: &mut SqliteStore, scopes: &[Scope], wanted: &Wanted, waiting: &Waiting) -> Result<Plan, StoreError> {
    let named: Vec<(i64, Scope, Option<String>)> = store
        .note_scopes()?
        .into_iter()
        .filter(|(id, scope, _)| scopes.contains(scope) && wanted.takes(*id))
        .collect();

    let (kept, notes): (Vec<_>, Vec<_>) = named
        .into_iter()
        .partition(|(id, _, project)| waiting.holds(*id, project.as_deref()));

    Ok(Plan {
        notes: notes.into_iter().map(|(id, _, project)| (id, project)).collect(),
        waiting: kept.len(),
    })
}

/// Deletes the notes of `plan` by their ids: a note saved after the plan was read is not among them.
fn apply(store: &mut SqliteStore, plan: Plan) -> Result<Discarded, StoreError> {
    let mut deleted = 0;

    for (id, project) in plan.notes {
        deleted += store.forget(&view_of(project), &Target::Global, &Forget::Note(id))?;
    }

    Ok(Discarded {
        deleted,
        waiting: plan.waiting,
    })
}

/// Deletes the notes of `scopes` in `store` that `wanted` names, except those still `waiting` for
/// the team database; how many went and how many were left for that.
fn discard(
    store: &mut SqliteStore,
    scopes: &[Scope],
    wanted: &Wanted,
    waiting: &Waiting,
) -> Result<Discarded, StoreError> {
    let plan = plan(store, scopes, wanted, waiting)?;
    apply(store, plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NoteDraft, NoteKind, Provenance};

    fn draft(title: &str) -> NoteDraft {
        NoteDraft {
            title: title.to_owned(),
            body: "Saved here".to_owned(),
            kind: NoteKind::Note,
            tags: Vec::new(),
            entities: Vec::new(),
            provenance: Provenance::default(),
        }
    }

    #[test]
    fn a_note_saved_after_the_list_was_read_survives_discard_all() {
        let dir = tempfile::tempdir().expect("dir");
        let mut store = SqliteStore::open(&dir.path().join("memory.sqlite3")).expect("store");
        store.remember(&Target::Global, &draft("Read")).expect("note");

        let read = plan(&mut store, &[Scope::Global], &Wanted::All, &Waiting::default()).expect("plan");
        let later = store
            .remember(&Target::Global, &draft("Saved later"))
            .expect("later note");

        let discarded = apply(&mut store, read).expect("apply");

        assert_eq!(discarded, Discarded { deleted: 1, waiting: 0 });

        let left: Vec<i64> = store
            .note_scopes()
            .expect("scopes")
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        assert_eq!(left, [later.id]);
    }
}
