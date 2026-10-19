//! `notes discard-local (--all | ID[,ID...] | -) [--scope repo|global] --yes [--json]`: deletes
//! notes from the local file in the scopes routed `remote`, where nothing reads them (left there
//! from before the routing changed, or after `push` copied them). `--all` takes every such note,
//! every repository's; ids are the local file's own (`localId` in `push --json`), given as one
//! comma-separated argument or, with `-`, read from stdin (separated by commas or blanks, at most
//! [`crate::limits::STDIN_IDS_BYTES_MAX`] bytes and [`crate::limits::IDS_MAX`] ids, exit 2 beyond);
//! an id of another scope, or of no note, is left alone. `--scope` takes one scope, which has to be
//! routed `remote`; without it every scope that is. Each note goes as `note_delete` deletes one.
//!
//! Without `--yes` nothing is deleted and it exits 2; a scope not routed `remote` exits 1.

use std::process::ExitCode;

use crate::backend::{Backend, NotesPlace, Selection};
use crate::cli;
use crate::cli_discard::{self, Discard, Wanted};
use crate::open::notes_place;
use crate::store::hidden::view_of;
use crate::store::{Forget, Forgotten, MemoryStore, Scope, SqliteStore, StoreError, Target};

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

    let deleted = if path.is_file() {
        SqliteStore::open(&path).and_then(|mut store| discard(&mut store, &scopes, &options.wanted))
    } else {
        Ok(0)
    };

    match deleted {
        Ok(deleted) => {
            cli_discard::report("note", deleted, options.json);
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

/// Deletes the notes of `scopes` in `store` that `wanted` names; how many went.
fn discard(store: &mut SqliteStore, scopes: &[Scope], wanted: &Wanted) -> Result<usize, StoreError> {
    let notes: Vec<(i64, Scope, Option<String>)> = store
        .note_scopes()?
        .into_iter()
        .filter(|(id, scope, _)| scopes.contains(scope) && wanted.takes(*id))
        .collect();

    if *wanted == Wanted::All {
        let mut places: Vec<Option<String>> = Vec::new();

        for (_, _, project) in &notes {
            if !places.contains(project) {
                places.push(project.clone());
            }
        }

        let mut forgotten = Forgotten::default();

        for project in places {
            forgotten = forgotten + store.forget_notes(&view_of(project), &[])?;
        }

        return Ok(forgotten.total());
    }

    let mut deleted = 0;

    for (id, _, project) in notes {
        deleted += store.forget(&view_of(project), &Target::Global, &Forget::Note(id))?;
    }

    Ok(deleted)
}
