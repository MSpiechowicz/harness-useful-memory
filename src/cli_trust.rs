//! The script commands of the `harness-memory` executable, for the person at the terminal (and
//! Harness Desktop on their behalf), never for an agent's MCP calls. Each sees the scripts the MCP
//! server's `script_list` would (the repository of `--project`, else of the working directory,
//! and the global scope), through the same store: with `scripts = "remote+local"` the team's
//! scripts and this computer's (shown past [`crate::store::split::LOCAL_ID_OFFSET`]), and never a
//! team script hidden on this computer (see [`crate::store::SplitScripts`]).
//!
//! - `harness-memory scripts list --json [--project PATH | --repo KEY | --all-repos] [--group needsReview|repo|global]
//!   [--scope repo|global] [--review needed|trusted] [--query TEXT]
//!   [--sort name|language|scope|place|runs|lastRun|updated] [--order asc|desc] [--offset N]
//!   [--limit N] [--hidden]` prints `{"total": N, "groups": {"needsReview": N, "repo": N,
//!   "global": N}, "scopes": {"repo": N, "global": N}, "review": {"needed": N, "trusted": N},
//!   "scripts": [...]}`: one page of the visible scripts as the objects `script_list` returns
//!   (`place` per script, `team` or `local`; `readOnly` for a team script while scripts are
//!   `remote+local`; `hidden`). The groups do not overlap: `needsReview` holds every script that
//!   needs approval, `repo` and `global` the others of their scope. `--scope` keeps the scripts of
//!   one scope, `--review needed` those that need approval (the `needsReview` group),
//!   `--review trusted` every other one; the two combine, and neither combines with `--group`.
//!   `--query` keeps the scripts whose name or description contains it, ignoring case (as
//!   `script_list` does). The group counts follow `--query`, the scope counts `--query` and
//!   `--review`, the review counts `--query` and `--scope`, the total every filter. By name (ties:
//!   repository scripts first, then this computer's) ascending unless `--sort updated` (newest
//!   first), `language`, `scope` (global scripts first), `place` (this computer's first, then by
//!   id), `runs` (the run count) or `lastRun` (the latest run; a script never run comes last either
//!   way), all ascending; `--order` turns any round, ties go by id. `--limit` is 1 to 200, 50 when
//!   not given; `--untrusted` is `--group needsReview`. Approval is decided on this machine, so
//!   every visible script is read and the page is cut here, not in the database. With a team
//!   database the object also has `hiddenCount` (the team scripts of the view hidden on this
//!   computer) and `localCounts` (`{"repo": N, "global": N}`: the scripts in the local file per
//!   scope, every repository's, 0 without the file). `--hidden` lists only the hidden team scripts
//!   of the view instead (the filters, the order and the paging apply to them, `total` counts
//!   them); a hidden script the team database no longer has is dropped from the hidden list.
//! - `harness-memory scripts get <id> --json [--project PATH | --repo KEY | --all-repos] [--hidden]` prints
//!   `{"script": {... "body"}}`: the list object with the body. A hidden team script is not found
//!   unless `--hidden` is given.
//! - `harness-memory scripts hide|unhide <id> [--project PATH] [--json]` hide a team script on this
//!   computer or show it again (see [`crate::cli_scripts::hidden`]).
//! - `harness-memory scripts delete <id> [--project PATH | --repo KEY | --all-repos]` deletes a visible script as
//!   `script_delete` does (a version that needs approval is remembered first, so saving its code
//!   again trusts nothing) and prints `deleted script <id> <name>`; a script that is not visible
//!   exits 1 with `harness-memory: script <id> not found`, a read-only team script exits 1 naming
//!   `harness-memory scripts hide`.
//! - `harness-memory scripts delete --all --yes [--project PATH | --repo KEY] [--json]` does that for every
//!   script `scripts list` shows without filters (all groups, all pages), deleted in one write per
//!   store, and prints `deleted N scripts` (and how many came from the team database), or
//!   `{"deleted": N, "team": M}` with `--json` (`team`: how many were kept in the team database).
//!   With `remote+local` only the scripts on this computer go: the team scripts stay, counted as
//!   `kept` (`{"deleted": N, "team": 0, "kept": K}`). Without `--yes` it deletes nothing and exits
//!   2.
//! - `harness-memory scripts discard-local (--all | ID[,ID...] | -) [--scope repo|global] --yes
//!   [--json]` deletes scripts from the local file while scripts are `remote` (see
//!   [`crate::cli_scripts::discard`]).
//! - `harness-memory scripts push [...]` copies the scripts saved on this computer into the team
//!   database (see [`crate::cli_scripts::push`]).
//! - `harness-memory trust <id>=<sha256>... [--project PATH | --repo KEY | --all-repos]` approves those scripts on this
//!   machine when each still has the `sha256` that was reviewed (the `id` and `sha256` of
//!   `scripts list`); a script changed since is not approved. `harness-memory trust <id>...` and
//!   `harness-memory trust --all [--project PATH]` (every visible script that needs it) approve the
//!   versions there are now, for a person who reads them in the terminal: they run only with a
//!   terminal as standard input (an agent's shell tool has none, so it cannot approve what no one
//!   reviewed), else they exit 2. One line per script on stdout; exit 1 when an id is not found,
//!   a script changed since its review, or a body holds a hidden character or declares an encoding
//!   other than UTF-8 (such a script can never run); the others are still approved.
//!
//! Exit codes and errors as in [`crate::cli`].

use std::io::IsTerminal;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::backend::{NotesPlace, Place, Selection};
use crate::cli::{self, Args, Delete, PICK_VALUED, Repos};
use crate::cli_scripts::{Opened, discard, hidden, list, not_found, push};
use crate::store::{Scope, Script, ScriptRef, TrustOrigin, TrustedVersion};
use crate::store::{foreign_encoding, foreign_encoding_error, hidden_character, hidden_character_error};

/// The usage of these commands, for the executable's usage line.
pub const USAGE: &str = "scripts list --json [--project PATH | --repo KEY | --all-repos] [--group needsReview|repo|global] [--scope \
                         repo|global] [--review needed|trusted] [--query TEXT] [--sort \
                         name|language|scope|place|runs|lastRun|updated] [--order asc|desc] [--offset N] [--limit N] \
                         [--hidden] | scripts repos --json | scripts get <id> --json [--project PATH | --repo KEY | --all-repos] [--hidden] | scripts hide <id> \
                         [--project PATH | --repo KEY | --all-repos] [--json] | scripts unhide <id> [--project PATH | --repo KEY | --all-repos] [--json] | scripts delete \
                         <id> [--project PATH | --repo KEY | --all-repos] | scripts delete --all --yes [--project PATH | --repo KEY] [--json] | scripts \
                         discard-local (--all | ID[,ID...] | -) [--scope repo|global] --yes [--json] | scripts push \
                         [--dry-run] [--json] [--overwrite none|all|ID[,ID...]|-] [--delete-copied] [--scope \
                         repo|global] [--repo <key|.>] [--no-global] | trust <id>[=<sha256>]... | --all [--project \
                         PATH]";

/// The line `harness-memory check` adds when every team script runs without approval.
pub fn check_line(selection: &Selection) -> Option<&'static str> {
    selection
        .team_scripts_trusted()
        .then_some("team scripts: trusted automatically")
}

/// `harness-memory scripts <args>`.
pub fn scripts(args: &[Option<&str>]) -> ExitCode {
    match args {
        [Some("get"), rest @ ..] => get(rest),
        [Some("repos"), rest @ ..] => repos(rest),
        [Some("hide"), rest @ ..] => hidden::hide(rest),
        [Some("unhide"), rest @ ..] => hidden::unhide(rest),
        [Some("delete"), rest @ ..] => delete(rest),
        [Some("discard-local"), rest @ ..] => discard::discard_local(rest),
        [Some("push"), rest @ ..] => push::push(rest),
        _ => list::list(args),
    }
}

/// `scripts repos --json`: the repositories that have scripts, each with its key, the label
/// scripts show it by and how many scripts it has, by label, and how many global scripts there are.
fn repos(args: &[Option<&str>]) -> ExitCode {
    match Args::parse(args, &["--json"], &[]) {
        Some(parsed) if parsed.flag("--json") && parsed.positional.is_empty() => {}
        _ => return usage(),
    }

    let Some(mut opened) = Opened::open(&Repos::Every) else {
        return ExitCode::FAILURE;
    };

    let listed = match opened.scripts.list_scripts(&opened.view, None) {
        Ok(listed) => listed,
        Err(error) => return opened.failed(&error.to_string()),
    };

    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();

    let mut global = 0_usize;

    for summary in &listed {
        match summary.project.as_deref() {
            Some(key) => *counts.entry(key.to_owned()).or_default() += 1,
            None => global += 1,
        }
    }

    println!(
        "{}",
        serde_json::json!({ "repos": cli::repos_json(counts), "global": global })
    );
    ExitCode::SUCCESS
}

/// `scripts get`.
fn get(args: &[Option<&str>]) -> ExitCode {
    let Some((id, repos, show_hidden)) = parse_get(args) else {
        return usage();
    };

    let Some(mut opened) = Opened::open(&repos) else {
        return ExitCode::FAILURE;
    };

    let found = match opened.scripts.get_script(&opened.view, &ScriptRef::Id(id)) {
        Ok(Some(script)) => Ok(Some((script, false))),
        Ok(None) if show_hidden => hidden::script(&mut opened, id).map(|script| script.map(|script| (script, true))),
        Ok(None) => Ok(None),
        Err(error) => Err(error),
    };

    let (script, is_hidden) = match found {
        Ok(Some(found)) => found,
        Ok(None) => return not_found(id),
        Err(error) => return opened.failed(&error.to_string()),
    };

    let approval = match opened.approval(&script.summary) {
        Ok(approval) => approval,
        Err(message) => return opened.failed(&message),
    };

    let mut shown = opened.json(&script.summary, approval, is_hidden);
    shown["body"] = Value::String(script.body);

    println!("{}", json!({ "script": shown }));
    ExitCode::SUCCESS
}

/// The script id, `--project` and `--hidden` after `scripts get`, which needs `--json`; `None` for
/// anything else.
fn parse_get(args: &[Option<&str>]) -> Option<(i64, Repos, bool)> {
    let parsed = Args::parse(args, &["--json", "--hidden", "--all-repos"], &PICK_VALUED)?;

    let [id] = parsed.positional[..] else {
        return None;
    };

    if !parsed.flag("--json") {
        return None;
    }

    Some((id.parse().ok()?, parsed.repos().ok()?, parsed.flag("--hidden")))
}

/// `scripts delete`.
fn delete(args: &[Option<&str>]) -> ExitCode {
    match Delete::parse(args) {
        Some(Delete::One { id, pick }) => delete_one(id, &pick),
        Some(Delete::All { pick, scope, json }) => delete_all(&pick, scope, json),
        Some(Delete::Unconfirmed) => cli::unconfirmed("script"),
        None => usage(),
    }
}

/// `scripts delete <id>`: deletes the visible script `id` as `script_delete` does, after deciding
/// its approval (which remembers a version that needs it).
fn delete_one(id: i64, repos: &Repos) -> ExitCode {
    let Some(mut opened) = Opened::open(repos) else {
        return ExitCode::FAILURE;
    };

    let found = match opened.scripts.get_script(&opened.view, &ScriptRef::Id(id)) {
        Ok(found) => found,
        Err(error) => return opened.failed(&error.to_string()),
    };

    let Some(Script { summary, .. }) = found else {
        return not_found(id);
    };

    if let Err(message) = opened.approval(&summary) {
        return opened.failed(&message);
    }

    match opened.scripts.delete_script(&opened.view, &ScriptRef::Id(id)) {
        Ok(true) => {
            println!("deleted script {id} {}", summary.name);
            ExitCode::SUCCESS
        }
        Ok(false) => not_found(id),
        Err(error) => opened.failed(&error.to_string()),
    }
}

/// `scripts delete --all --yes`.
fn delete_all(repos: &Repos, scope: Option<Scope>, json: bool) -> ExitCode {
    let Some(mut opened) = Opened::open(repos) else {
        return ExitCode::FAILURE;
    };

    opened.view = cli::narrowed(opened.view, scope);

    // Listing decides each script's approval, which remembers the versions that need it.
    let listed: Vec<(i64, Place)> = match opened.list(None) {
        Ok(listed) => listed
            .iter()
            .map(|(summary, _)| (summary.id, opened.spot(summary.id).0))
            .collect(),
        Err(message) => return opened.failed(&message),
    };

    let ids: Vec<i64> = listed.iter().map(|(id, _)| *id).collect();
    let deleted = match opened.scripts.delete_scripts(&opened.view, &ids) {
        Ok(deleted) => deleted,
        Err(error) => return opened.failed(&error.to_string()),
    };

    let place = opened.selection.scripts_place();
    let team = if place == NotesPlace::Remote { deleted } else { 0 };

    // With `remote+local` the team scripts are read-only, so they stay.
    let kept =
        (place == NotesPlace::RemoteLocal).then(|| listed.iter().filter(|(_, side)| *side == Place::Remote).count());

    cli::deleted_all_keeping("script", (deleted, team), kept, json);
    ExitCode::SUCCESS
}

/// `harness-memory trust <args>`.
pub fn trust(args: &[Option<&str>]) -> ExitCode {
    let Some(options) = parse_trust(args) else {
        return usage();
    };

    if !options.reviewed_elsewhere() && !std::io::stdin().is_terminal() {
        eprintln!(
            "harness-memory: run this in your own terminal: `trust <id>` and `trust --all` approve scripts as they \
             are now and need a terminal (give <id>=<sha256> from `scripts list --json` to approve a reviewed \
             version)"
        );
        return ExitCode::from(2);
    }

    let Some(mut opened) = Opened::open(&options.repos) else {
        return ExitCode::FAILURE;
    };

    let picks = match options.picks {
        Some(picks) => picks,
        None => match opened.list(None) {
            Ok(listed) => listed
                .iter()
                .filter(|(_, approval)| !approval.is_trusted())
                .map(|(summary, _)| Pick {
                    id: summary.id,
                    sha256: None,
                })
                .collect(),
            Err(message) => return opened.failed(&message),
        },
    };

    if picks.is_empty() {
        println!("no scripts need approval");
        return ExitCode::SUCCESS;
    }

    // Printed after everything else the stores may write to stderr, so the last line says why.
    let mut refusals = Vec::new();

    for pick in picks {
        match approve(&mut opened, &pick) {
            Ok(Approved::Line(line)) => println!("{line}"),
            Ok(Approved::Refused(message)) => refusals.push(message),
            Err(message) => return opened.failed(&message),
        }
    }

    for message in &refusals {
        eprintln!("harness-memory: {message}");
    }

    if refusals.len() > 1 {
        eprintln!("harness-memory: {} scripts were not trusted", refusals.len());
    }

    if refusals.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Approves the script `pick` names (its id as `scripts list` shows it) as read now, unless it
/// changed since the `sha256` the person reviewed or its body holds a [`hidden_character`] or
/// declares a [`foreign_encoding`]. The trust is recorded where the script is kept, under its id
/// there.
fn approve(opened: &mut Opened, pick: &Pick) -> Result<Approved, String> {
    let id = pick.id;
    let script = opened
        .scripts
        .get_script(&opened.view, &ScriptRef::Id(id))
        .map_err(|error| error.to_string())?;

    let Some(script) = script else {
        return Ok(Approved::Refused(format!("script {id} not found")));
    };

    let Script { summary, body } = &script;
    let name = &summary.name;

    if pick.sha256.as_ref().is_some_and(|reviewed| *reviewed != summary.sha256) {
        return Ok(Approved::Refused(format!(
            "script {id} {name} changed since you reviewed it"
        )));
    }

    if let Some(found) = hidden_character(body) {
        return Ok(Approved::Refused(format!(
            "script {id} {name} was not trusted, it can never run: {}",
            hidden_character_error(found)
        )));
    }

    if let Some(encoding) = foreign_encoding(summary.language, body) {
        return Ok(Approved::Refused(format!(
            "script {id} {name} was not trusted, it can never run: {}",
            foreign_encoding_error(&encoding)
        )));
    }

    let (side, raw) = opened.spot(id);
    let key = opened.selection.scripts_trust_key(side);
    let trusted = opened
        .trust
        .trust_of(&key, raw, &summary.sha256)
        .map_err(|error| error.to_string())?;

    if trusted.is_some_and(|record| record.is_for(summary)) {
        return Ok(Approved::Line(format!("script {id} {name} is already trusted")));
    }

    let version = TrustedVersion::of(&key, raw, &script, TrustOrigin::Approved);
    opened.trust.trust(&version).map_err(|error| error.to_string())?;

    Ok(Approved::Line(format!("trusted script {id} {name}")))
}

/// The options of `trust`: the scripts, or `None` for `--all`.
#[derive(Debug)]
struct TrustOptions {
    picks: Option<Vec<Pick>>,
    repos: Repos,
}

impl TrustOptions {
    /// True when every script comes with the `sha256` reviewed elsewhere (Harness Desktop), so no
    /// terminal is needed.
    fn reviewed_elsewhere(&self) -> bool {
        self.picks
            .as_ref()
            .is_some_and(|picks| picks.iter().all(|pick| pick.sha256.is_some()))
    }
}

/// A script to approve: its id and, when the person reviewed it elsewhere, the `sha256` they saw.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pick {
    id: i64,
    sha256: Option<String>,
}

impl Pick {
    /// `<id>` or `<id>=<sha256>` (64 hex digits, any case); `None` for anything else.
    fn parse(arg: &str) -> Option<Self> {
        let (id, sha256) = match arg.split_once('=') {
            Some((id, sha256)) => (id, Some(sha256)),
            None => (arg, None),
        };

        let id = id.parse::<i64>().ok()?;
        let sha256 = match sha256 {
            Some(sha256) if sha256.len() == 64 && sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
                Some(sha256.to_ascii_lowercase())
            }
            Some(_) => return None,
            None => None,
        };

        Some(Self { id, sha256 })
    }
}

/// What approving one script printed: a line for stdout, or why it was not approved (stderr).
enum Approved {
    Line(String),
    Refused(String),
}

/// The options after `trust`; `None` unless they name scripts or `--all` (not both).
fn parse_trust(args: &[Option<&str>]) -> Option<TrustOptions> {
    let parsed = Args::parse(args, &["--all", "--all-repos"], &PICK_VALUED)?;
    let repos = parsed.repos().ok()?;
    let picks = parsed
        .positional
        .iter()
        .map(|arg| Pick::parse(arg))
        .collect::<Option<Vec<_>>>()?;

    match (parsed.flag("--all"), picks.is_empty()) {
        (true, true) if repos != Repos::Every => Some(TrustOptions { picks: None, repos }),
        (false, false) => Some(TrustOptions {
            picks: Some(picks),
            repos,
        }),
        _ => None,
    }
}

pub(crate) fn usage() -> ExitCode {
    cli::usage(USAGE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(raw: &[&'a str]) -> Vec<Option<&'a str>> {
        raw.iter().map(|arg| Some(*arg)).collect()
    }

    #[test]
    fn check_names_trusted_team_scripts_only_when_scripts_go_to_the_team_database() {
        use crate::backend::{Backend, Routing};

        let selection = |backend: Backend, scripts: NotesPlace, trust_team_scripts: bool| Selection {
            backend,
            routing: Routing {
                scripts,
                ..Routing::default()
            },
            trust_team_scripts,
            hooks: Place::Local,
            trust_team_hooks: false,
            routines: Place::Local,
            trust_team_routines: false,
            warnings: Vec::new(),
        };
        let team = || Backend::Postgres {
            url: "postgres://db/memory".to_owned(),
        };

        for scripts in [NotesPlace::Remote, NotesPlace::RemoteLocal] {
            assert_eq!(
                check_line(&selection(team(), scripts, true)),
                Some("team scripts: trusted automatically")
            );
        }

        assert_eq!(check_line(&selection(team(), NotesPlace::Remote, false)), None);
        assert_eq!(check_line(&selection(team(), NotesPlace::Local, true)), None);
        assert_eq!(check_line(&selection(Backend::Local, NotesPlace::Remote, true)), None);
    }

    #[test]
    fn trust_options_take_ids_or_all() {
        let sha = "AB".repeat(32);
        let reviewed = format!("12={sha}");
        let ids = parse_trust(&args(&["3", &reviewed, "--project", "/w"])).expect("ids");
        assert_eq!(
            ids.picks,
            Some(vec![
                Pick { id: 3, sha256: None },
                Pick {
                    id: 12,
                    sha256: Some("ab".repeat(32))
                }
            ])
        );
        assert_eq!(ids.repos, Repos::Project(Some("/w".to_owned())));
        assert_eq!(parse_trust(&args(&["--all"])).expect("all").picks, None);

        let short = format!("3={}", "a".repeat(63));
        let not_hex = format!("3={}", "g".repeat(64));
        for bad in [
            &[][..],
            &["--all", "3"],
            &["x"],
            &["3", "--all"],
            &["--all", "--all"],
            &["3", "--force"],
            &["3="],
            &[&short],
            &[&not_hex],
            &["=abc"],
        ] {
            assert!(parse_trust(&args(bad)).is_none(), "{bad:?}");
        }
    }
}
