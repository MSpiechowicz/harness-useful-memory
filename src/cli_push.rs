//! The options `harness-memory push` (notes) and `harness-memory scripts push` share:
//! `[--dry-run] [--json] [--overwrite none|all|ID[,ID…]|-] [--delete-copied] [--scope repo|global]
//! [--repo <key|.>] [--no-global] [--remote-local] [--selection -]` ([`PushArgs`]), the conflicts
//! `--overwrite` names and the notes `--selection` limits the push to ([`chosen`], read from stdin),
//! and the repository `--repo` names ([`repo_key`]).
//!
//! `--selection -` reads
//! `{"only": [{"id": N, "name": "<note title or script name>", "updatedAt": "<localUpdatedAt>"}, ...],
//! "overwrite": [N, ...]}` from stdin, both lists required and no other key (not in an entry
//! either): `only` holds each local entry as the dry run listed it, its `id`, its title or name
//! and its `localUpdatedAt` as printed (compared as text). An entry is pushed (and, with
//! `--delete-copied`, deleted) only while its id, name and update time still match the row, so a
//! row changed after the review, or a new row that reused a deleted row's id, is neither pushed
//! nor deleted but listed in `failed`. So is every id of `only` when there is nothing to push from (no
//! local database, or no scope taken): each is listed in `failed`, exit 1, nothing is copied or deleted,
//! and only an empty `only` answers `nothing`. Whatever else the scope holds stays as it is. `overwrite`
//! names the conflicts among them that may replace the team's text. It does not go with
//! `--overwrite`. Stdin is read up to [`crate::limits::STDIN_IDS_BYTES_MAX`] bytes, each list holds
//! at most [`crate::limits::IDS_MAX`] entries, ids are positive and no id repeats in `only`.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::limits::IDS_MAX;
use crate::project::{ProjectResolver, normalize_remote};
use crate::push::{Overwrite, Seen};
use crate::store::Scope;
use crate::support::{Env, ids};

/// The options of a push.
#[derive(Debug, Default)]
pub struct PushArgs {
    pub dry_run: bool,
    /// `--json`: one JSON object instead of the text report.
    pub json: bool,
    /// `--repo`: a repository key, a remote URL, or `.` for the current checkout's repository.
    pub repo: Option<String>,
    pub no_global: bool,
    /// `--overwrite`: which conflicts replace the team's text; `None` for the default (all).
    pub overwrite: Option<OverwriteArg>,
    /// `--delete-copied`: delete the local entries the push copied.
    pub delete_copied: bool,
    /// `--scope`: only this scope; it also names a `remote+local` scope to push.
    pub scope: Option<Scope>,
    /// `--remote-local`: every scope set to `remote+local`, and only those, in one run.
    pub remote_local: bool,
    /// `--selection -`: only the entries a JSON object on stdin names.
    pub selection: bool,
}

/// `--overwrite` as given: the conflicts to overwrite, or `-` for local ids read from stdin.
#[derive(Debug)]
pub enum OverwriteArg {
    Given(Overwrite),
    Stdin,
}

impl PushArgs {
    /// The options after `push`; `None` for an unknown, repeated or incomplete one. A value
    /// follows its option or is joined to it with `=` (`--repo=.`).
    pub fn parse(args: &[Option<&str>]) -> Option<Self> {
        let mut parsed = Self::default();
        let mut args = args.iter();

        while let Some(arg) = args.next() {
            let arg = (*arg)?;
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) if name.starts_with("--") => (name, Some(value)),
                _ => (arg, None),
            };
            let flag = inline.is_none();

            match name {
                "--dry-run" if flag && !parsed.dry_run => parsed.dry_run = true,
                "--json" if flag && !parsed.json => parsed.json = true,
                "--no-global" if flag && !parsed.no_global => parsed.no_global = true,
                "--delete-copied" if flag && !parsed.delete_copied => parsed.delete_copied = true,
                "--remote-local" if flag && !parsed.remote_local => parsed.remote_local = true,
                "--selection" if !parsed.selection => {
                    // Only stdin can hold what was reviewed, so `-` is the one value.
                    if option_value(inline, &mut args)? != "-" {
                        return None;
                    }

                    parsed.selection = true;
                }
                "--repo" if parsed.repo.is_none() => {
                    parsed.repo = Some(option_value(inline, &mut args)?.to_owned());
                }
                "--overwrite" if parsed.overwrite.is_none() => {
                    parsed.overwrite = Some(overwrite_arg(option_value(inline, &mut args)?)?);
                }
                "--scope" if parsed.scope.is_none() => {
                    parsed.scope = match option_value(inline, &mut args)? {
                        "repo" => Some(Scope::Repo),
                        "global" => Some(Scope::Global),
                        _ => return None,
                    };
                }
                _ => return None,
            }
        }

        if parsed.repo.as_deref().is_some_and(|repo| repo.trim().is_empty()) {
            return None;
        }

        Some(parsed)
    }

    /// Why these options do not go together, if so; `nouns` names what is pushed (`notes`).
    pub fn conflict(&self, nouns: &str) -> Option<String> {
        if self.delete_copied && self.dry_run {
            return Some(format!(
                "--delete-copied deletes the {nouns} a push copied, so it does not go with --dry-run"
            ));
        }

        if self.selection && self.overwrite.is_some() {
            return Some(
                "--selection - names the conflicts to overwrite itself (its \"overwrite\" list), so it does not go \
                 with --overwrite"
                    .to_owned(),
            );
        }

        if self.remote_local && self.scope.is_some() {
            return Some(
                "--remote-local takes every scope set to \"remote+local\", so it does not go with --scope".to_owned(),
            );
        }

        if self.scope == Some(Scope::Global) && self.repo.is_some() {
            return Some("--scope global leaves every repository out, so it does not go with --repo".to_owned());
        }

        if self.scope == Some(Scope::Global) && self.no_global {
            return Some("--scope global and --no-global leave nothing to push".to_owned());
        }

        None
    }
}

/// The value of an option: joined to it with `=`, else the next argument.
fn option_value<'a>(inline: Option<&'a str>, rest: &mut std::slice::Iter<'_, Option<&'a str>>) -> Option<&'a str> {
    match inline {
        Some(value) => Some(value),
        None => rest.next().copied().flatten(),
    }
}

/// `--overwrite none`, `all`, `-` or local ids (`12,40`); `None` for anything else.
fn overwrite_arg(raw: &str) -> Option<OverwriteArg> {
    match raw {
        "none" => Some(OverwriteArg::Given(Overwrite::None)),
        "all" => Some(OverwriteArg::Given(Overwrite::All)),
        "-" => Some(OverwriteArg::Stdin),
        ids => {
            let ids = local_ids(ids)?;

            if ids.is_empty() {
                return None;
            }

            Some(OverwriteArg::Given(Overwrite::Ids(ids)))
        }
    }
}

/// Ids separated by commas or blanks (see [`ids::note_ids`]); `None` when one is not a positive
/// whole number or there are too many.
fn local_ids(raw: &str) -> Option<BTreeSet<i64>> {
    ids::note_ids(raw).ok().map(|ids| ids.into_iter().collect())
}

/// What a push is limited to and which conflicts it overwrites.
#[derive(Debug)]
pub struct Chosen {
    pub overwrite: Overwrite,
    /// The local entries `--selection -` limits the push to, by id, as they were reviewed; `None`
    /// for a push of the whole scope.
    pub only: Option<BTreeMap<i64, Seen>>,
}

/// What `--selection -` holds on stdin.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    only: Vec<Reviewed>,
    overwrite: Vec<i64>,
}

/// One entry of `only`: a row as the dry run listed it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reviewed {
    id: i64,
    name: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
}

/// The entries `args` push and the conflicts they overwrite, reading stdin for `--selection -` and
/// `--overwrite -`; `None` (with the reason printed) when stdin cannot be read or holds something
/// else.
pub fn chosen(args: &PushArgs) -> Option<Chosen> {
    if !args.selection {
        return Some(Chosen {
            overwrite: overwrite(args)?,
            only: None,
        });
    }

    let read = ids::read_stdin().and_then(|raw| selection(&raw));

    match read {
        Ok((only, overwrite)) => Some(Chosen {
            overwrite: Overwrite::Ids(overwrite),
            only: Some(only),
        }),
        Err(reason) => {
            eprintln!(
                "harness-memory: --selection - reads {{\"only\": [{{\"id\": N, \"name\": \"...\", \"updatedAt\": \
                 \"...\"}}], \"overwrite\": [N]}} from stdin: {reason}"
            );
            None
        }
    }
}

/// The entries of `only` (by id) and the ids of `overwrite` in the JSON object `raw`.
fn selection(raw: &str) -> Result<(BTreeMap<i64, Seen>, BTreeSet<i64>), String> {
    let parsed: Selection = serde_json::from_str(raw).map_err(|error| error.to_string())?;

    if parsed.only.len() > IDS_MAX {
        return Err(format!("\"only\" holds more than {IDS_MAX} entries"));
    }

    let mut only = BTreeMap::new();

    for entry in parsed.only {
        if entry.id <= 0 {
            return Err("\"only\" takes positive whole numbers as ids".to_owned());
        }

        let seen = Seen {
            name: entry.name,
            updated_at: entry.updated_at,
        };

        if only.insert(entry.id, seen).is_some() {
            return Err(format!("\"only\" names id {} twice", entry.id));
        }
    }

    if parsed.overwrite.len() > IDS_MAX {
        return Err(format!("\"overwrite\" holds more than {IDS_MAX} ids"));
    }

    if parsed.overwrite.iter().any(|id| *id <= 0) {
        return Err("\"overwrite\" takes positive whole numbers".to_owned());
    }

    Ok((only, parsed.overwrite.into_iter().collect()))
}

/// The conflicts `args` overwrite, reading the ids from stdin for `--overwrite -`; `None` (with
/// the reason printed) when stdin cannot be read or holds something else.
fn overwrite(args: &PushArgs) -> Option<Overwrite> {
    match &args.overwrite {
        None => Some(Overwrite::Default),
        Some(OverwriteArg::Given(overwrite)) => Some(overwrite.clone()),
        Some(OverwriteArg::Stdin) => {
            let read = ids::read_stdin().and_then(|raw| ids::note_ids(&raw).map_err(|error| error.to_string()));

            match read {
                Ok(ids) => Some(Overwrite::Ids(ids.into_iter().collect())),
                Err(reason) => {
                    eprintln!("harness-memory: --overwrite - reads local ids from stdin: {reason}");
                    None
                }
            }
        }
    }
}

/// The repository key `--repo` names: `.` is the repository of the working directory (found the
/// way the MCP server finds it), a `git:…` or `path:…` key is taken as written, and anything else
/// is read as a remote (`github.com/acme/widget`, `https://github.com/acme/widget.git`). The
/// reason is printed when there is none.
pub fn repo_key(env: &Env, raw: &str) -> Option<String> {
    let raw = raw.trim();

    if raw == "." {
        let mut projects = ProjectResolver::new(env.clone(), std::env::current_dir().ok());

        return match projects.resolve(None) {
            Ok(Some(project)) => Some(project.key),
            Ok(None) | Err(_) => {
                eprintln!(
                    "harness-memory: this folder is not inside a repository; give its key to --repo instead (as \
                     push --dry-run lists them)"
                );
                None
            }
        };
    }

    if raw.starts_with("git:") || raw.starts_with("path:") {
        return Some(raw.to_owned());
    }

    match normalize_remote(raw) {
        Some(remote) => Some(format!("git:{remote}")),
        None => {
            eprintln!(
                "harness-memory: --repo takes a repository key (git:host/owner/repo or path:/folder), a remote \
                 URL or ."
            );
            None
        }
    }
}
