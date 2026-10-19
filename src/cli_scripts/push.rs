//! `scripts push [--dry-run] [--json] [--overwrite none|all|ID[,ID…]|-] [--delete-copied]
//! [--scope repo|global] [--repo <key|.>] [--no-global]`: copies the scripts saved on this
//! computer into the team database ([`crate::push_scripts`]), with the options of `push`
//! ([`crate::cli_push`]).
//!
//! It pushes while scripts are `remote`; with `remote+local` only when `--scope` names the scope
//! to push, and never with `local`. `--json` prints one object, the same for a dry run and a real
//! one:
//!
//! ```json
//! {"dryRun": true, "nothing": null,
//!  "scripts": {"added": 0, "updated": 0, "overwritten": 0, "kept": 0, "identical": 0, "teamNewer": 0, "failed": 0},
//!  "deleted": null,
//!  "changes": [{"localId": 3, "scope": "repo", "repo": "github.com/o/r", "name": "open-prs",
//!    "action": "add", "conflict": false, "teamId": null, "localUpdatedAt": "RFC 3339",
//!    "teamUpdatedAt": null, "deleted": false, "changedSince": false, "failed": false, "trusted": false}],
//!  "failed": [{"localId": 3, "message": "…"}]}
//! ```
//!
//! `nothing` says why there was nothing to push (exit 0); the totals leave out the scripts whose
//! push failed (`failed: true`, counted in `scripts.failed`). `changedSince: true` marks a copied
//! script `--delete-copied` left because it changed after the push read it. Exit 0 when nothing failed, 1 when
//! something did (or the team database cannot be reached), 2 for bad usage; without `--json`
//! "nothing to push" exits 2 too, as `push` does.

use std::collections::BTreeSet;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::backend::{self, Backend, CONFIG_VAR, NotesPlace, Place, Secrets, URL_VAR};
use crate::cli;
use crate::cli_push::{PushArgs, overwrite, repo_key};
use crate::mcp::repo_label;
use crate::open::{open_hidden_scripts, open_trust};
use crate::push::{Action, Options};
use crate::push_scripts::{ScriptPush, TeamScripts, push_scripts};
use crate::store::Scope;
use crate::support::time::rfc3339;

/// `scripts push`.
pub(crate) fn push(args: &[Option<&str>]) -> ExitCode {
    let Some(args) = PushArgs::parse(args) else {
        return crate::cli_trust::usage();
    };

    if let Some(problem) = args.conflict("scripts") {
        eprintln!("harness-memory: {problem}");
        return ExitCode::from(2);
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
            "harness-memory: scripts push copies the scripts saved on this computer into a team database, and none \
             is set: set {URL_VAR} or database_url in {config}"
        );
        return ExitCode::from(2);
    }

    if let Some(reason) = left_out(selection.scripts_place(), args.scope) {
        return report_nothing(&args, reason);
    }

    let Some(local) = crate::database_path(&env) else {
        eprintln!("harness-memory: the data folder is unknown");
        return ExitCode::FAILURE;
    };

    if !local.is_file() {
        return report_nothing(&args, "no scripts are saved on this computer");
    }

    let Some(overwrite) = overwrite(&args) else {
        return ExitCode::from(2);
    };

    let repo = match &args.repo {
        Some(raw) => match repo_key(&env, raw) {
            Some(key) => Some(key),
            None => return ExitCode::from(2),
        },
        None => None,
    };

    let secrets = backend.secrets();
    let failed = |message: &str| {
        eprintln!("harness-memory: {}", secrets.scrub(message));
        ExitCode::FAILURE
    };

    let mut team = match backend::open_scripts(backend) {
        Some(Ok(team)) => team,
        Some(Err(message)) => return failed(&message),
        None => return ExitCode::FAILURE,
    };

    // A team script hidden here is never written over, so a list that cannot be read stops it all.
    let hidden = match open_hidden_scripts(&env, &selection).map(|mut hidden| hidden.try_ids()) {
        Some(Ok(ids)) => ids,
        Some(Err(error)) => return failed(&format!("nothing was pushed: {error}")),
        None => Vec::new(),
    };

    let options = Options {
        dry_run: args.dry_run,
        repo,
        skip_global: args.no_global || args.scope == Some(Scope::Repo),
        skip_repositories: args.scope == Some(Scope::Global),
        overwrite,
        delete_copied: args.delete_copied,
    };

    let key = selection.scripts_trust_key(Place::Remote);
    let mut target = TeamScripts {
        scripts: team.as_mut(),
        key: &key,
        hidden: &hidden,
    };
    let mut trust = open_trust(&env);

    let summary = match push_scripts(&local, &mut target, trust.as_mut(), &options) {
        Ok(summary) => summary,
        Err(error) => return failed(&format!("scripts push failed: {error}")),
    };

    if args.json {
        println!("{}", push_json(args.dry_run, None, &summary, &secrets));
    } else {
        report(&options, &summary, &backend.describe(), &secrets);
    }

    if summary.failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Why scripts kept in `place` are not pushed with `--scope` `scope`, if so.
fn left_out(place: NotesPlace, scope: Option<Scope>) -> Option<&'static str> {
    match (place, scope) {
        (NotesPlace::Local, _) => Some("scripts stay on this computer (scripts = \"local\")"),
        (NotesPlace::RemoteLocal, None) => Some(
            "scripts are \"remote+local\", so only --scope repo or --scope global pushes the ones saved on this \
             computer",
        ),
        _ => None,
    }
}

/// Says there is nothing to push and why: in the JSON object (exit 0) with `--json`, else on
/// stderr (exit 2).
fn report_nothing(args: &PushArgs, reason: &str) -> ExitCode {
    if args.json {
        let empty = ScriptPush::default();
        println!("{}", push_json(args.dry_run, Some(reason), &empty, &Secrets::default()));
        return ExitCode::SUCCESS;
    }

    eprintln!("harness-memory: nothing to push: {reason}");
    ExitCode::from(2)
}

/// The `scripts push --json` object (see the module docs).
fn push_json(dry_run: bool, nothing: Option<&str>, summary: &ScriptPush, secrets: &Secrets) -> Value {
    let failed_scripts: BTreeSet<i64> = summary
        .failed
        .iter()
        .filter(|failure| !failure.deleting)
        .filter_map(|failure| failure.local_id)
        .collect();

    let changes: Vec<Value> = summary
        .changes
        .iter()
        .map(|change| {
            json!({
                "localId": change.local_id,
                "scope": change.scope.as_str(),
                "repo": change.repo.as_deref().map(repo_label),
                "name": change.name,
                "action": change.action.as_str(),
                "conflict": change.conflict,
                "teamId": change.team_id,
                "localUpdatedAt": rfc3339(change.local_updated_at),
                "teamUpdatedAt": change.team_updated_at.map(rfc3339),
                "deleted": change.deleted,
                "changedSince": change.changed_since,
                "failed": change.failed,
                "trusted": change.trusted,
            })
        })
        .collect();

    let failed: Vec<Value> = summary
        .failed
        .iter()
        .map(|failure| json!({ "localId": failure.local_id, "message": secrets.scrub(&failure.message) }))
        .collect();

    json!({
        "dryRun": dry_run,
        "nothing": nothing,
        "scripts": {
            "added": summary.added,
            "updated": summary.updated,
            "overwritten": summary.overwritten,
            "kept": summary.kept,
            "identical": summary.identical,
            "teamNewer": summary.team_newer,
            "failed": failed_scripts.len(),
        },
        "deleted": summary.deleted,
        "changes": changes,
        "failed": failed,
    })
}

/// Prints what a push did: the totals, the scripts a dry run would write (by repository), the
/// conflicts, what was deleted, and on stderr what failed.
fn report(options: &Options, summary: &ScriptPush, place: &str, secrets: &Secrets) {
    let kept = if summary.kept > 0 {
        format!(", {} kept", summary.kept)
    } else {
        String::new()
    };
    let totals = format!(
        "{} added, {} updated, {} overwritten{kept}, {} skipped",
        summary.added,
        summary.updated,
        summary.overwritten,
        summary.identical + summary.team_newer
    );

    if options.dry_run {
        println!("dry run, nothing written to {place}: scripts {totals}");
    } else {
        println!("pushed the scripts saved on this computer to {place}: scripts {totals}");
    }

    for change in summary.changes.iter().filter(|change| !change.failed) {
        let scope = change.repo.as_deref().map_or_else(|| "global".to_owned(), repo_label);
        let action = match (change.action, options.dry_run) {
            (Action::Added, true) => "add",
            (Action::Updated, true) => "update",
            (Action::Overwritten, true) => "overwrite the team's version of",
            (Action::Overwritten, false) => "overwrote the team's version of",
            (Action::Kept, _) => "keep the team's version of",
            _ => continue,
        };

        println!("{scope}: {action} {}", change.name);
    }

    if let Some(deleted) = summary.deleted {
        println!("deleted {deleted} copied scripts from this computer");
    }

    for failure in &summary.failed {
        let message = secrets.scrub(&failure.message);

        if failure.deleting {
            eprintln!("harness-memory: {message}");
        } else {
            eprintln!("harness-memory: not pushed: {message}");
        }
    }
}
