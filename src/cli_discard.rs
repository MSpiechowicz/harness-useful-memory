//! What `notes discard-local` and `scripts discard-local` share: their options
//! `(--all | ID[,ID...] | -) [--scope repo|global] --yes [--json]` ([`Discard::parse`]), the refusal
//! without `--yes`, and the report. Ids are the local file's own (`localId` in `push --json` and
//! `scripts push --json`), given as one comma-separated argument or, with `-`, read from stdin
//! (separated by commas or blanks, at most [`crate::limits::STDIN_IDS_BYTES_MAX`] bytes and
//! [`crate::limits::IDS_MAX`] ids, exit 2 beyond).
//!
//! An entry saved while the team database was unreachable and still listed as waiting for it
//! ([`crate::store::pending`]) is never discarded, by `--all` or by id: the team database has not
//! received it, so deleting it would lose it. It is counted in `waiting` of the answer instead, and
//! can be discarded once a sync (or a push) has copied it and taken it off the list. A listed entry
//! is told apart by its id and scope, which is all the local file's rows give before they are read.
//! A list that exists but cannot be read (a link, another owner or mode, oversized, malformed) is
//! not taken for an empty one: nothing is deleted, the reason is printed and it exits 1
//! ([`unreadable`]).

use std::collections::HashSet;
use std::path::Path;
use std::process::ExitCode;

use crate::backend::Selection;
use crate::cli::Args;
use crate::push::GLOBAL;
use crate::store::Scope;
use crate::store::pending::{Entry, Kind, Pending};
use crate::support::ids;

/// What a discard did: the entries deleted, and those it left because they still wait for the
/// team database.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Discarded {
    pub deleted: usize,
    pub waiting: usize,
}

/// The entries of one kind that wait to be copied to the team database of a selection.
#[derive(Debug, Clone, Default)]
pub(crate) struct Waiting(Vec<Entry>);

impl Waiting {
    /// What the list beside the local `database` holds of `kind` for the team database `selection`
    /// names; why not when the list exists but cannot be read (see [`Pending::entries_strict`]):
    /// without it nothing can be told apart as waiting, so nothing may be deleted.
    pub(crate) fn of(kind: Kind, database: &Path, selection: &Selection) -> Result<Self, String> {
        let team = selection.backend.team_key();
        Pending::beside(database).entries_strict(kind, &team).map(Self)
    }

    /// True when the local row `id` of the repository `project` (`None`: global) is listed.
    pub(crate) fn holds(&self, id: i64, project: Option<&str>) -> bool {
        let scope = project.unwrap_or(GLOBAL);
        self.0.iter().any(|entry| entry.id == id && entry.scope == scope)
    }
}

/// The entries asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Wanted {
    All,
    Ids(HashSet<i64>),
}

impl Wanted {
    /// True when the local entry `id` is asked for.
    pub(crate) fn takes(&self, id: i64) -> bool {
        match self {
            Self::All => true,
            Self::Ids(ids) => ids.contains(&id),
        }
    }
}

/// The options after `discard-local`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Discard {
    pub wanted: Wanted,
    pub scope: Option<Scope>,
    pub yes: bool,
    pub json: bool,
}

impl Discard {
    /// Reads `args`, with stdin giving the ids for `-`; `None` for a usage error (the reason
    /// printed when the ids are wrong).
    pub(crate) fn parse(args: &[Option<&str>]) -> Option<Self> {
        parse(args, stdin)
    }
}

/// Refuses `<noun>s discard-local` without `--yes`; exit 2.
pub(crate) fn unconfirmed(noun: &str) -> ExitCode {
    eprintln!(
        "harness-memory: `{noun}s discard-local` deletes {noun}s from this computer and cannot be undone. Add --yes \
         to delete them"
    );
    ExitCode::from(2)
}

/// Says that the list of entries waiting for the team database cannot be read, so nothing was
/// deleted; exit 1.
pub(crate) fn unreadable(reason: &str) -> ExitCode {
    eprintln!(
        "harness-memory: nothing was deleted: {reason}. Entries that still wait for the team database could not be \
         told apart, so nothing is discarded until the list is readable (fix pending-sync.json)"
    );
    ExitCode::FAILURE
}

/// Prints how many `noun`s were deleted and how many were left because they still wait for the
/// team database: `{"deleted": N, "waiting": M}` with `json`, else a line (the second half only
/// when some waited).
pub(crate) fn report(noun: &str, discarded: Discarded, json: bool) {
    let Discarded { deleted, waiting } = discarded;

    if json {
        println!("{}", serde_json::json!({ "deleted": deleted, "waiting": waiting }));
        return;
    }

    let plural = if deleted == 1 { "" } else { "s" };
    let left = if waiting == 0 {
        String::new()
    } else {
        format!(", kept {waiting} that still wait for the team database")
    };

    println!("deleted {deleted} {noun}{plural} from this computer{left}");
}

/// [`Discard::parse`] with `stdin` giving the ids for `-`.
fn parse(args: &[Option<&str>], stdin: impl FnOnce() -> Option<String>) -> Option<Discard> {
    // `-` is not an option: it stands for the ids on stdin.
    let piped = args.iter().filter(|arg| **arg == Some("-")).count();
    let rest: Vec<Option<&str>> = args.iter().copied().filter(|arg| *arg != Some("-")).collect();
    let parsed = Args::parse(&rest, &["--all", "--yes", "--json"], &["--scope"])?;
    let scope = parsed.scope().ok()?;

    let wanted = match (parsed.flag("--all"), &parsed.positional[..], piped) {
        (true, [], 0) => Wanted::All,
        (false, [], 1) => Wanted::Ids(local_ids(&stdin()?)?),
        (false, [listed], 0) => Wanted::Ids(local_ids(listed)?),
        _ => return None,
    };

    Some(Discard {
        wanted,
        scope,
        yes: parsed.flag("--yes"),
        json: parsed.flag("--json"),
    })
}

/// The ids in `raw`, separated by commas or blanks (see [`ids::note_ids`]); `None` when there is
/// none or one is not a positive number, and (with the reason printed) when there are too many.
fn local_ids(raw: &str) -> Option<HashSet<i64>> {
    match ids::note_ids(raw) {
        Ok(ids) => (!ids.is_empty()).then(|| ids.into_iter().collect()),
        Err(ids::IdsError::NotIds) => None,
        Err(error @ ids::IdsError::TooMany) => {
            eprintln!("harness-memory: {error}");
            None
        }
    }
}

/// Standard input, capped (see [`ids::read_stdin`]); `None`, with the reason printed, when it
/// cannot be read or is too long.
fn stdin() -> Option<String> {
    ids::read_stdin()
        .map_err(|reason| eprintln!("harness-memory: {reason}"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(raw: &[&'a str]) -> Vec<Option<&'a str>> {
        raw.iter().map(|arg| Some(*arg)).collect()
    }

    fn no_stdin() -> Option<String> {
        panic!("stdin is not read")
    }

    #[test]
    fn discard_takes_all_or_ids_from_the_argument_or_stdin() {
        let all = parse(&args(&["--all", "--yes", "--json", "--scope", "repo"]), no_stdin).expect("valid");
        assert_eq!(
            all,
            Discard {
                wanted: Wanted::All,
                scope: Some(Scope::Repo),
                yes: true,
                json: true
            }
        );

        let listed = parse(&args(&["3,7", "--yes"]), no_stdin).expect("valid");
        assert_eq!(listed.wanted, Wanted::Ids(HashSet::from([3, 7])));

        let piped = parse(&args(&["-", "--yes"]), || Some("3, 7\n9".to_owned())).expect("valid");
        assert_eq!(piped.wanted, Wanted::Ids(HashSet::from([3, 7, 9])));

        let many = vec!["5"; crate::limits::IDS_MAX + 1].join(",");
        assert!(parse(&args(&["-", "--yes"]), || Some(many)).is_none(), "too many ids");

        assert!(!parse(&args(&["--all"]), no_stdin).expect("valid").yes);

        for bad in [
            &[][..],
            &["--all", "3"],
            &["3", "4"],
            &["x"],
            &["0"],
            &["3,,x"],
            &["--all", "--scope", "team"],
            &["-", "-"],
            &["-", "3"],
            &["--all", "-"],
        ] {
            assert!(parse(&args(bad), no_stdin).is_none(), "{bad:?}");
        }

        assert!(parse(&args(&["-"]), || Some(" \n".to_owned())).is_none(), "no ids");
    }
}
