//! `scripts discard-local (--all | ID[,ID...] | -) [--scope repo|global] --yes [--json]`: deletes
//! scripts from the local file while scripts are `remote`, where nothing reads them (left there
//! from before the routing changed, or after `scripts push` copied them). `--all` takes every
//! script, every repository's, `--scope` those of one scope; ids are the local file's own
//! (`localId` in `scripts push --json`, see [`crate::cli_discard`]), and an id of another scope,
//! or of no script, is left alone. It prints `deleted N scripts from this computer`, or
//! `{"deleted": N}`.
//!
//! Without `--yes` nothing is deleted and it exits 2; without a team database, or with scripts
//! not `remote`, it exits 1.

use std::collections::BTreeMap;
use std::process::ExitCode;

use crate::backend::{Backend, NotesPlace, Selection};
use crate::cli;
use crate::cli_discard::{self, Discard, Wanted};
use crate::store::hidden::view_of;
use crate::store::{Scope, ScriptStore, SqliteStore, StoreError};

/// `scripts discard-local`.
pub(crate) fn discard_local(args: &[Option<&str>]) -> ExitCode {
    let Some(options) = Discard::parse(args) else {
        return crate::cli_trust::usage();
    };

    if !options.yes {
        return cli_discard::unconfirmed("script");
    }

    let Some((env, selection)) = cli::settings() else {
        return ExitCode::FAILURE;
    };

    if let Err(reason) = check_routing(&selection) {
        eprintln!("harness-memory: {reason}");
        return ExitCode::FAILURE;
    }

    let Some(path) = crate::database_path(&env) else {
        eprintln!("harness-memory: the data folder is unknown");
        return ExitCode::FAILURE;
    };

    let deleted = if path.is_file() {
        SqliteStore::open(&path).and_then(|mut store| discard(&mut store, options.scope, &options.wanted))
    } else {
        Ok(0)
    };

    match deleted {
        Ok(deleted) => {
            cli_discard::report("script", deleted, options.json);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("harness-memory: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Why this computer's scripts cannot be discarded under `selection`: they are read.
fn check_routing(selection: &Selection) -> Result<(), String> {
    if selection.backend == Backend::Local {
        return Err(
            "no team database is set, so every script is read from this computer and none is discarded".to_owned(),
        );
    }

    let place = selection.scripts_place();

    if place != NotesPlace::Remote {
        return Err(format!(
            "scripts are read from this computer (scripts = \"{}\"), and discard-local only deletes scripts while \
             scripts is \"remote\"",
            place.as_str()
        ));
    }

    Ok(())
}

/// Deletes the scripts of `store` that `wanted` names (of `scope` when given); how many went.
fn discard(store: &mut SqliteStore, scope: Option<Scope>, wanted: &Wanted) -> Result<usize, StoreError> {
    let mut by_project: BTreeMap<Option<String>, Vec<i64>> = BTreeMap::new();

    for (script, _) in store.every_script()? {
        let summary = script.summary;

        if scope.is_none_or(|scope| summary.scope == scope) && wanted.takes(summary.id) {
            by_project.entry(summary.project).or_default().push(summary.id);
        }
    }

    let mut deleted = 0;

    for (project, ids) in by_project {
        deleted += store.delete_scripts(&view_of(project), &ids)?;
    }

    Ok(deleted)
}
