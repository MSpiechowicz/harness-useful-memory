//! `harness-memory pull [--dry-run] --json [--ids -|ID[,ID...]] [--repo <key|.>] [--no-global]`:
//! copies team notes to this computer's file for the scopes set to `remote+local` ([`crate::pull`]),
//! the other way round from `push`; `scripts pull` does the same for scripts
//! ([`crate::cli_scripts::pull`]). The team database is only read.
//!
//! A dry run lists the team notes of the scopes taken that this computer can be given, with what
//! it would do with each; a copy needs `--ids` (team ids, `-` reads them from stdin, one per line
//! or separated by commas), so nothing is ever copied by default (exit 2 without them). `--repo`
//! narrows the repository notes to one repository (`.` is the current checkout's), `--no-global`
//! leaves the global notes out. Only scopes set to `remote+local` are taken, never `remote` or
//! `local`. `--json` is required and prints one object, the same for a dry run and a copy:
//!
//! ```json
//! {"dryRun": true, "nothing": null,
//!  "notes": {"added": 0, "identical": 0, "localDiffers": 0, "failed": 0},
//!  "changes": [{"teamId": 7, "localId": null, "scope": "repo", "repo": "github.com/o/r",
//!    "title": "Build cache", "action": "add", "teamUpdatedAt": "RFC 3339", "localUpdatedAt": null,
//!    "failed": false}],
//!  "failed": [{"teamId": 9, "message": "…"}]}
//! ```
//!
//! `action` is `add` (this computer has no note of that title, so it is copied), `identical` or
//! `localDiffers` (it has one of that title, with the same or another text, left as it is).
//! `localId` is the note this computer has, or the new copy; `null` for a dry run that would add
//! it. `nothing` says why there was nothing to pull (exit 0): no scope is `remote+local`, or no ids
//! were given on stdin. The totals leave out the notes that failed (`failed: true`, counted in
//! `notes.failed`, which also counts ids that name no note that can be copied, hidden notes
//! included). Exit 0 when nothing failed, 1 when something did, 2 for bad usage; a team database
//! that cannot be reached exits 1 with nothing copied and `{"error": "failed", "message"}` on
//! stdout. Without a team database it exits 2.

use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::backend::{self, Backend, CONFIG_VAR, NotesPlace, Secrets, URL_VAR};
use crate::cli::{self, Args};
use crate::cli_push::repo_key;
use crate::location::{Migration, migrate_legacy};
use crate::open::open_hidden;
use crate::pull::{self, Options, Summary};
use crate::support::time::rfc3339;
use crate::support::{Env, ids};

/// The usage of the command, for the executable's usage line.
pub const USAGE: &str = "pull [--dry-run] --json [--ids -|ID[,ID...]] [--repo <key|.>] [--no-global]";

/// The options of a pull.
#[derive(Debug)]
pub struct PullArgs {
    pub dry_run: bool,
    /// `--repo`: a repository key, a remote URL, or `.` for the current checkout's repository.
    pub repo: Option<String>,
    pub no_global: bool,
    /// `--ids`: the team ids to copy.
    pub ids: Option<IdsArg>,
}

/// `--ids` as given: the ids, or `-` for ids read from stdin.
#[derive(Debug)]
pub enum IdsArg {
    Given(Vec<i64>),
    Stdin,
}

impl PullArgs {
    /// The options after `pull` (or `scripts pull`); `None` for an unknown, repeated or incomplete
    /// one, or without `--json`.
    pub fn parse(args: &[Option<&str>]) -> Option<Self> {
        let parsed = Args::parse(args, &["--dry-run", "--json", "--no-global"], &["--ids", "--repo"])?;

        if !parsed.positional.is_empty() || !parsed.flag("--json") {
            return None;
        }

        let repo = parsed.value("--repo").map(str::to_owned);

        if repo.as_deref().is_some_and(|repo| repo.trim().is_empty()) {
            return None;
        }

        let ids = match parsed.value("--ids") {
            None => None,
            Some("-") => Some(IdsArg::Stdin),
            Some(raw) => {
                let ids = ids::note_ids(raw).ok().filter(|ids| !ids.is_empty())?;
                Some(IdsArg::Given(ids))
            }
        };

        Some(Self {
            dry_run: parsed.flag("--dry-run"),
            repo,
            no_global: parsed.flag("--no-global"),
            ids,
        })
    }

    /// Refuses a copy without ids: nothing is ever copied by default. Exit 2 with the reason.
    pub fn needs_ids(&self, nouns: &str) -> Option<ExitCode> {
        if self.dry_run || self.ids.is_some() {
            return None;
        }

        eprintln!(
            "harness-memory: pull copies only the {nouns} it is given: list them with --dry-run, then name them with \
             --ids"
        );
        Some(ExitCode::from(2))
    }

    /// The ids to copy, reading stdin for `--ids -`: `Ok(None)` without `--ids`. Exit 2, with the
    /// reason printed, when stdin cannot be read or holds something else.
    pub fn read_ids(&self) -> Result<Option<Vec<i64>>, ExitCode> {
        match &self.ids {
            None => Ok(None),
            Some(IdsArg::Given(ids)) => Ok(Some(ids.clone())),
            Some(IdsArg::Stdin) => {
                let read = ids::read_stdin().and_then(|raw| ids::note_ids(&raw).map_err(|error| error.to_string()));

                match read {
                    Ok(ids) => Ok(Some(ids)),
                    Err(reason) => {
                        eprintln!("harness-memory: --ids - reads team ids from stdin: {reason}");
                        Err(ExitCode::from(2))
                    }
                }
            }
        }
    }

    /// The options of the library for a pull of `nouns` (`notes`, `scripts`) with `repo` as
    /// `--repo` named it; a scope is left out for the reason given (a scope that is not
    /// `remote+local`), and the global one by `--no-global` too.
    pub fn options(
        &self,
        nouns: &str,
        repo: Option<String>,
        repository_left: Option<String>,
        global_left: Option<String>,
    ) -> Options {
        let no_global = self
            .no_global
            .then(|| format!("--no-global leaves the global {nouns} out"));

        Options {
            dry_run: self.dry_run,
            repo,
            repository_left,
            global_left: global_left.or(no_global),
            ids: None,
        }
    }
}

/// `--repo` as a key; `Ok(None)` without it, exit 2 (reason printed) when it names none.
pub(crate) fn repo_of(env: &Env, args: &PullArgs) -> Result<Option<String>, ExitCode> {
    match &args.repo {
        Some(raw) => repo_key(env, raw).map(Some).ok_or(ExitCode::from(2)),
        None => Ok(None),
    }
}

/// The local file a pull reads and writes. A copy first moves in the database Harness Desktop used
/// to keep, as every server start does, so a fresh data folder does not hide it; exit 1 when that
/// fails or the data folder is unknown.
pub(crate) fn local_file(env: &Env, dry_run: bool, secrets: &Secrets) -> Result<PathBuf, ExitCode> {
    let Some(local) = crate::database_path(env) else {
        eprintln!("harness-memory: the data folder is unknown");
        return Err(ExitCode::FAILURE);
    };

    if !dry_run && let Migration::Failed(reason) = migrate_legacy(env, &local) {
        return Err(cli::failed(&secrets.scrub(&format!(
            "could not copy the Harness Desktop memory database ({reason}), so nothing was copied"
        ))));
    }

    Ok(local)
}

/// The reason a scope placed at `place` is left out of a pull, if so.
pub(crate) fn left_out(place: NotesPlace, words: &str) -> Option<String> {
    (place != NotesPlace::RemoteLocal).then(|| {
        format!(
            "{words} are \"{}\", and pull only takes the scopes set to \"remote+local\"",
            place.as_str()
        )
    })
}

/// Prints the object of a pull that ran; exit 0 when nothing failed, else 1. Shared by `pull` and
/// `scripts pull`.
pub(crate) fn finish(printed: Value, summary: &Summary) -> ExitCode {
    println!("{printed}");

    if summary.failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Prints the object of a pull with nothing to do (its `nothing` says why); exit 0.
pub(crate) fn nothing(object: Value) -> ExitCode {
    println!("{object}");
    ExitCode::SUCCESS
}

/// The changes of `summary` as `pull --json` lists them; `label` is the key of a note's title or a
/// script's name.
pub(crate) fn changes_json(summary: &Summary, label: &str) -> Vec<Value> {
    summary
        .changes
        .iter()
        .map(|change| {
            let mut shown = json!({
                "teamId": change.team_id,
                "localId": change.local_id,
                "scope": if change.repo.is_some() { "repo" } else { "global" },
                "repo": change.repo.as_deref().map(|key| key.strip_prefix("git:").unwrap_or(key)),
            });

            shown[label] = json!(change.title);
            shown["action"] = json!(change.action.as_str());
            shown["teamUpdatedAt"] = json!(rfc3339(change.team_updated_at));
            shown["localUpdatedAt"] = json!(change.local_updated_at.map(rfc3339));
            shown["failed"] = json!(change.failed);
            shown
        })
        .collect()
}

/// The failures of `summary` as `pull --json` lists them, without the database credentials.
pub(crate) fn failures_json(summary: &Summary, secrets: &Secrets) -> Vec<Value> {
    summary
        .failed
        .iter()
        .map(|failure| json!({ "teamId": failure.team_id, "message": secrets.scrub(&failure.message) }))
        .collect()
}

/// The `pull --json` object (see the module docs).
fn pull_json(dry_run: bool, nothing: Option<&str>, summary: &Summary, secrets: &Secrets) -> Value {
    json!({
        "dryRun": dry_run,
        "nothing": nothing,
        "notes": {
            "added": summary.added,
            "identical": summary.identical,
            "localDiffers": summary.local_differs,
            "failed": summary.failed_entries(),
        },
        "changes": changes_json(summary, "title"),
        "failed": failures_json(summary, secrets),
    })
}

/// Prints the object that says there is nothing to pull, and why.
fn nothing_to_pull(args: &PullArgs, reason: &str) -> ExitCode {
    let empty = Summary::default();
    nothing(pull_json(args.dry_run, Some(reason), &empty, &Secrets::default()))
}

/// `harness-memory pull`.
pub fn pull(args: &[Option<&str>]) -> ExitCode {
    let Some(args) = PullArgs::parse(args) else {
        return cli::usage(USAGE);
    };

    if let Some(refused) = args.needs_ids("notes") {
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
            "harness-memory: pull copies team notes to this computer, and no team database is set: set {URL_VAR} or \
             database_url in {config}"
        );
        return ExitCode::from(2);
    }

    let routing = selection.routing;
    let repository_left = left_out(routing.repository, "repository notes");
    let global_left = left_out(routing.global, "global notes");

    let repo = match repo_of(&env, &args) {
        Ok(repo) => repo,
        Err(code) => return code,
    };

    let options = args.options("notes", repo, repository_left, global_left);

    if let Some(reason) = nothing_to_take(&options) {
        return nothing_to_pull(&args, &reason);
    }

    let ids = match args.read_ids() {
        Ok(ids) => ids,
        Err(code) => return code,
    };

    if ids.as_ref().is_some_and(Vec::is_empty) {
        return nothing_to_pull(&args, "no ids were given");
    }

    let options = Options { ids, ..options };

    let secrets = backend.secrets();

    let local = match local_file(&env, args.dry_run, &secrets) {
        Ok(local) => local,
        Err(code) => return code,
    };

    let mut opened = match backend::open(backend) {
        Some(Ok(opened)) => opened,
        Some(Err(message)) => return cli::failed(&secrets.scrub(&message)),
        None => return ExitCode::FAILURE,
    };

    if opened.newer {
        return cli::failed(&format!(
            "the team database ({}) was set up by a newer harness-memory; update this one (run the install command \
             again), then pull. Nothing was copied",
            backend.describe()
        ));
    }

    // A team note hidden here is never copied, so a list that cannot be read stops it all.
    let hidden = match open_hidden(&env, &selection).map(|mut hidden| hidden.try_ids()) {
        Some(Ok(ids)) => ids,
        Some(Err(error)) => {
            return cli::failed(&secrets.scrub(&format!("nothing was copied: {error}")));
        }
        None => Vec::new(),
    };

    let summary = match pull::pull(&local, opened.store.as_mut(), &hidden, &options) {
        Ok(summary) => summary,
        Err(error) => return cli::failed(&secrets.scrub(&format!("pull failed, nothing was copied: {error}"))),
    };

    finish(pull_json(args.dry_run, None, &summary, &secrets), &summary)
}

/// Why a pull with `options` has nothing to take, if so: no scope is set to `remote+local`, or
/// `--repo` names a repository whose notes are not.
fn nothing_to_take(options: &Options) -> Option<String> {
    if let (Some(_), Some(reason)) = (&options.repo, &options.repository_left) {
        return Some(format!("--repo names a repository, but {reason}"));
    }

    if options.repository_left.is_none() || options.global_left.is_none() {
        return None;
    }

    let reasons: Vec<&str> = [&options.repository_left, &options.global_left]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect();

    Some(reasons.join(" and "))
}
