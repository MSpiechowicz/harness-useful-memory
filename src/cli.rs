//! What the commands for a person (`scripts`, `trust`, `notes`; see [`crate::cli_trust`] and
//! [`crate::cli_notes`]) share: their options, the settings, the repository `--project` names, and
//! the paging options of the lists.
//!
//! Usage errors exit 2, failures (settings, database) exit 1. A failure ends stderr with the line
//! that says why (`harness-memory: …`, after any warning), so a caller may show the last line.

use std::process::ExitCode;

use crate::backend::{self, Selection};
use crate::limits::{LIST_PAGE_DEFAULT, LIST_PAGE_MAX};
use crate::project::ProjectResolver;
use crate::store::ProjectKey;
use crate::support::Env;

/// The options of the lists' paging: `--offset N`, `--limit N`, `--query TEXT`, `--order asc|desc`.
pub(crate) const PAGE_OPTIONS: [&str; 4] = ["--offset", "--limit", "--query", "--order"];

/// The arguments of a command, read against the options it takes.
#[derive(Debug, Default)]
pub(crate) struct Args<'a> {
    /// The arguments that do not start with `-`, in order.
    pub positional: Vec<&'a str>,
    flags: Vec<&'a str>,
    values: Vec<(&'a str, &'a str)>,
}

impl<'a> Args<'a> {
    /// Reads `args`: the `flags` stand alone, the `valued` options take a value (`--name VALUE`
    /// or `--name=VALUE`). `None` for an unknown or repeated option, a missing value, or an
    /// argument that is not UTF-8.
    pub(crate) fn parse(args: &[Option<&'a str>], flags: &[&str], valued: &[&str]) -> Option<Self> {
        let mut parsed = Self::default();
        let mut rest = args.iter().copied();

        while let Some(arg) = rest.next() {
            let arg = arg?;

            if !arg.starts_with('-') {
                parsed.positional.push(arg);
                continue;
            }

            if flags.contains(&arg) {
                if parsed.flag(arg) {
                    return None;
                }

                parsed.flags.push(arg);
                continue;
            }

            let (name, value) = match arg.split_once('=') {
                Some((name, value)) => (name, value),
                None => (arg, rest.next().flatten()?),
            };

            if !valued.contains(&name) || parsed.value(name).is_some() {
                return None;
            }

            parsed.values.push((name, value));
        }

        Some(parsed)
    }

    /// True when the flag `name` was given.
    pub(crate) fn flag(&self, name: &str) -> bool {
        self.flags.contains(&name)
    }

    /// The value of the option `name`, when given.
    pub(crate) fn value(&self, name: &str) -> Option<&'a str> {
        self.values
            .iter()
            .find(|(given, _)| *given == name)
            .map(|(_, value)| *value)
    }

    /// The path of `--project`; `Err` when it is blank.
    pub(crate) fn project(&self) -> Result<Option<String>, ()> {
        match self.value("--project") {
            Some(path) if path.trim().is_empty() => Err(()),
            Some(path) => Ok(Some(path.to_owned())),
            None => Ok(None),
        }
    }
}

/// The paging options of a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Page {
    pub offset: usize,
    /// 1 to [`LIST_PAGE_MAX`], [`LIST_PAGE_DEFAULT`] when not given.
    pub limit: usize,
    /// Blank when not given.
    pub query: String,
    /// `--order`: `Some(true)` for `desc`, `None` when not given (the sort's own order).
    pub descending: Option<bool>,
}

impl Page {
    /// The paging options of `args`; `None` when one is not a number, the limit is out of range,
    /// or the order is neither `asc` nor `desc`.
    pub(crate) fn of(args: &Args) -> Option<Self> {
        let offset = match args.value("--offset") {
            Some(offset) => offset.parse::<usize>().ok()?,
            None => 0,
        };

        let limit = match args.value("--limit") {
            Some(limit) => limit
                .parse::<usize>()
                .ok()
                .filter(|limit| (1..=LIST_PAGE_MAX).contains(limit))?,
            None => LIST_PAGE_DEFAULT,
        };

        let descending = match args.value("--order") {
            Some("asc") => Some(false),
            Some("desc") => Some(true),
            Some(_) => return None,
            None => None,
        };

        Some(Self {
            offset,
            limit,
            query: args.value("--query").unwrap_or_default().trim().to_owned(),
            descending,
        })
    }
}

/// What `notes delete` and `scripts delete` were asked to do, with `--project`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Delete {
    /// `delete <id>`.
    One { id: i64, project: Option<String> },
    /// `delete --all --yes [--json]`: every entry the list shows without filters.
    All { project: Option<String>, json: bool },
    /// `delete --all` without `--yes`.
    Unconfirmed,
}

impl Delete {
    /// The options after `delete`; `None` for anything but one id, or `--all` (with `--yes` and
    /// `--json` when given) and no id.
    pub(crate) fn parse(args: &[Option<&str>]) -> Option<Self> {
        let parsed = Args::parse(args, &["--all", "--yes", "--json"], &["--project"])?;
        let project = parsed.project().ok()?;

        if !parsed.flag("--all") {
            let [id] = parsed.positional[..] else {
                return None;
            };

            if parsed.flag("--yes") || parsed.flag("--json") {
                return None;
            }

            return Some(Self::One {
                id: id.parse().ok()?,
                project,
            });
        }

        if !parsed.positional.is_empty() {
            return None;
        }

        if !parsed.flag("--yes") {
            return Some(Self::Unconfirmed);
        }

        Some(Self::All {
            project,
            json: parsed.flag("--json"),
        })
    }
}

/// Refuses `<noun>s delete --all` without `--yes`; exit 2.
pub(crate) fn unconfirmed(noun: &str) -> ExitCode {
    eprintln!(
        "harness-memory: `{noun}s delete --all` deletes every {noun} `{noun}s list` shows and cannot be undone; add \
         --yes to delete them"
    );
    ExitCode::from(2)
}

/// Prints what `<noun>s delete --all` did: `{"deleted": N, "team": M}` with `json` (`team`: how
/// many were in the team database), else a line.
pub(crate) fn deleted_all(noun: &str, deleted: usize, team: usize, json: bool) {
    if json {
        println!("{}", serde_json::json!({ "deleted": deleted, "team": team }));
        return;
    }

    let plural = if deleted == 1 { "" } else { "s" };

    if team > 0 {
        println!("deleted {deleted} {noun}{plural}, {team} from the team database");
    } else {
        println!("deleted {deleted} {noun}{plural}");
    }
}

/// Prints the usage of `commands`; exit 2.
pub(crate) fn usage(commands: &str) -> ExitCode {
    eprintln!("usage: harness-memory {commands}");
    ExitCode::from(2)
}

/// The environment and the settings, after printing their warnings; `None`, with the reason
/// printed, when the home folder is unknown or the settings are wrong.
pub(crate) fn settings() -> Option<(Env, Selection)> {
    let Ok(env) = Env::from_process() else {
        eprintln!("harness-memory: the home folder is unknown");
        return None;
    };

    let selection = match backend::select(&env) {
        Ok(selection) => selection,
        Err(error) => {
            eprintln!("harness-memory: {error}");
            return None;
        }
    };

    for warning in &selection.warnings {
        eprintln!("harness-memory: {warning}");
    }

    Some((env, selection))
}

/// The repository of `project` (else of the working directory), found the way the MCP server
/// finds it: `Some(None)` outside one, `None`, with the reason printed, when the path is wrong.
pub(crate) fn repository(env: &Env, project: Option<&str>) -> Option<Option<ProjectKey>> {
    let mut projects = ProjectResolver::new(env.clone(), std::env::current_dir().ok());

    match projects.resolve(project) {
        Ok(repo) => Some(repo),
        Err(message) => {
            eprintln!("harness-memory: {}", message.replace("project_path", "--project"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(raw: &[&'a str]) -> Vec<Option<&'a str>> {
        raw.iter().map(|arg| Some(*arg)).collect()
    }

    #[test]
    fn options_take_values_either_way_and_never_twice() {
        let raw = args(&["list", "--json", "--project", "/w", "--query=a b", "7"]);
        let parsed = Args::parse(&raw, &["--json"], &["--project", "--query"]).expect("valid");

        assert_eq!(parsed.positional, ["list", "7"]);
        assert!(parsed.flag("--json"));
        assert_eq!(parsed.project(), Ok(Some("/w".to_owned())));
        assert_eq!(parsed.value("--query"), Some("a b"));

        for bad in [
            &["--json", "--json"][..],
            &["--project"],
            &["--project", "/a", "--project=/b"],
            &["--yaml"],
            &["--json=1"],
        ] {
            assert!(
                Args::parse(&args(bad), &["--json"], &["--project"]).is_none(),
                "{bad:?}"
            );
        }

        let blank = args(&["--project", " "]);
        assert_eq!(
            Args::parse(&blank, &[], &["--project"]).expect("parsed").project(),
            Err(())
        );
        assert!(Args::parse(&[None], &[], &[]).is_none(), "not UTF-8");
    }

    #[test]
    fn delete_takes_one_id_or_all_with_yes() {
        assert_eq!(
            Delete::parse(&args(&["7", "--project", "/w"])),
            Some(Delete::One {
                id: 7,
                project: Some("/w".to_owned())
            })
        );
        assert_eq!(
            Delete::parse(&args(&["--all", "--yes", "--json"])),
            Some(Delete::All {
                project: None,
                json: true
            })
        );
        assert_eq!(
            Delete::parse(&args(&["--yes", "--all", "--project=/w"])),
            Some(Delete::All {
                project: Some("/w".to_owned()),
                json: false
            })
        );
        assert_eq!(Delete::parse(&args(&["--all", "--json"])), Some(Delete::Unconfirmed));

        for bad in [
            &[][..],
            &["x"],
            &["7", "8"],
            &["7", "--yes"],
            &["7", "--json"],
            &["--yes"],
            &["--all", "--yes", "7"],
            &["--all", "--yes", "--project", " "],
            &["--all", "--all", "--yes"],
        ] {
            assert!(Delete::parse(&args(bad)).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn paging_has_defaults_and_bounds() {
        let none = Args::parse(&[], &[], &PAGE_OPTIONS).expect("empty");
        assert_eq!(
            Page::of(&none),
            Some(Page {
                offset: 0,
                limit: LIST_PAGE_DEFAULT,
                query: String::new(),
                descending: None
            })
        );

        let raw = args(&[
            "--offset", "400", "--limit", "200", "--query", " Cache ", "--order", "asc",
        ]);
        let page = Page::of(&Args::parse(&raw, &[], &PAGE_OPTIONS).expect("valid")).expect("page");
        assert_eq!(
            (page.offset, page.limit, page.query.as_str(), page.descending),
            (400, 200, "Cache", Some(false))
        );

        for bad in [
            &["--limit", "0"][..],
            &["--limit", "201"],
            &["--offset", "-1"],
            &["--offset", "x"],
            &["--order", "up"],
        ] {
            let parsed = Args::parse(&args(bad), &[], &PAGE_OPTIONS).expect("parsed");
            assert!(Page::of(&parsed).is_none(), "{bad:?}");
        }
    }
}
