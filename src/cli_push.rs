//! The options `harness-memory push` (notes) and `harness-memory scripts push` share:
//! `[--dry-run] [--json] [--overwrite none|all|ID[,ID…]|-] [--delete-copied] [--scope repo|global]
//! [--repo <key|.>] [--no-global]` ([`PushArgs`]), the conflicts `--overwrite` names
//! ([`overwrite`], local ids from stdin with `-`), and the repository `--repo` names
//! ([`repo_key`]).

use std::collections::BTreeSet;

use crate::project::{ProjectResolver, normalize_remote};
use crate::push::Overwrite;
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

/// The conflicts `args` overwrite, reading the ids from stdin for `--overwrite -`; `None` (with
/// the reason printed) when stdin cannot be read or holds something else.
pub fn overwrite(args: &PushArgs) -> Option<Overwrite> {
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
