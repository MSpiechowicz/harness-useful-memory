//! `scripts pull [--dry-run] --json [--ids -|ID[,ID...]] [--repo <key|.>] [--no-global]`: copies team
//! scripts to this computer's file while scripts are `remote+local` ([`crate::pull_scripts`]), with
//! the options and the answer of `pull` ([`crate::cli_pull`]); the team database is only read.
//!
//! The answer is the same object with `scripts` for the totals and `name` for each change's `title`:
//!
//! ```json
//! {"dryRun": true, "nothing": null,
//!  "scripts": {"added": 0, "identical": 0, "localDiffers": 0, "failed": 0},
//!  "changes": [{"teamId": 7, "localId": null, "scope": "repo", "repo": "github.com/o/r",
//!    "name": "open-prs", "action": "add", "teamUpdatedAt": "RFC 3339", "localUpdatedAt": null,
//!    "failed": false}],
//!  "failed": [{"teamId": 9, "message": "…"}]}
//! ```
//!
//! A copy is never trusted: it is a new script on this computer that needs approval before it runs,
//! whatever the team's approval or the setting that trusts team scripts say.

use std::process::ExitCode;

use serde_json::{Value, json};

use crate::backend::{self, Backend, CONFIG_VAR, Secrets, URL_VAR};
use crate::cli;
use crate::cli_pull::{PullArgs, changes_json, failures_json, finish, left_out, local_file, nothing, repo_of};
use crate::open::open_hidden_scripts;
use crate::pull::{Options, Summary};
use crate::pull_scripts::pull_scripts;

/// `scripts pull`.
pub(crate) fn pull(args: &[Option<&str>]) -> ExitCode {
    let Some(args) = PullArgs::parse(args) else {
        return crate::cli_trust::usage();
    };

    if let Some(refused) = args.needs_ids("scripts") {
        return refused;
    }

    let Some((env, selection)) = cli::settings() else {
        return ExitCode::FAILURE;
    };

    let backend = &selection.backend;

    if *backend == Backend::Local {
        let config = backend::config_path(&env)
            .ok()
            .flatten()
            .map(|(path, _)| path.display().to_string())
            .unwrap_or_else(|| format!("the file {CONFIG_VAR} names"));
        eprintln!(
            "harness-memory: scripts pull copies team scripts to this computer, and no team database is set: set \
             {URL_VAR} or database_url in {config}"
        );
        return ExitCode::from(2);
    }

    if let Some(reason) = left_out(selection.scripts_place(), "scripts") {
        return nothing_to_pull(&args, &reason);
    }

    let repo = match repo_of(&env, &args) {
        Ok(repo) => repo,
        Err(code) => return code,
    };

    let ids = match args.read_ids() {
        Ok(ids) => ids,
        Err(code) => return code,
    };

    if ids.as_ref().is_some_and(Vec::is_empty) {
        return nothing_to_pull(&args, "no ids were given");
    }

    let options = Options {
        ids,
        ..args.options("scripts", repo, None, None)
    };

    let secrets = backend.secrets();

    let local = match local_file(&env, args.dry_run, &secrets) {
        Ok(local) => local,
        Err(code) => return code,
    };

    let mut team = match backend::open_scripts(backend) {
        Some(Ok(team)) => team,
        Some(Err(message)) => return cli::failed(&secrets.scrub(&message)),
        None => return ExitCode::FAILURE,
    };

    // A team script hidden here is never copied, so a list that cannot be read stops it all.
    let hidden = match open_hidden_scripts(&env, &selection).map(|mut hidden| hidden.try_ids()) {
        Some(Ok(ids)) => ids,
        Some(Err(error)) => return cli::failed(&secrets.scrub(&format!("nothing was copied: {error}"))),
        None => Vec::new(),
    };

    let summary = match pull_scripts(&local, team.as_mut(), &hidden, &options) {
        Ok(summary) => summary,
        Err(error) => {
            return cli::failed(&secrets.scrub(&format!("scripts pull failed, nothing was copied: {error}")));
        }
    };

    finish(pull_json(args.dry_run, None, &summary, &secrets), &summary)
}

/// Prints the object that says there is nothing to pull, and why.
fn nothing_to_pull(args: &PullArgs, reason: &str) -> ExitCode {
    nothing(pull_json(
        args.dry_run,
        Some(reason),
        &Summary::default(),
        &Secrets::default(),
    ))
}

/// The `scripts pull --json` object (see the module docs).
fn pull_json(dry_run: bool, nothing: Option<&str>, summary: &Summary, secrets: &Secrets) -> Value {
    json!({
        "dryRun": dry_run,
        "nothing": nothing,
        "scripts": {
            "added": summary.added,
            "identical": summary.identical,
            "localDiffers": summary.local_differs,
            "failed": summary.failed_entries(),
        },
        "changes": changes_json(summary, "name"),
        "failed": failures_json(summary, secrets),
    })
}
