//! The script commands of the `harness-memory` executable, for the person at the terminal (and
//! Harness Desktop on their behalf), never for an agent's MCP calls:
//!
//! - `harness-memory scripts list --json [--project PATH] [--group needsReview|repo|global]
//!   [--scope repo|global] [--review needed|trusted] [--query TEXT]
//!   [--sort name|language|scope|place|runs|lastRun|updated] [--order asc|desc] [--offset N]
//!   [--limit N]` prints `{"total": N, "groups": {"needsReview": N, "repo": N, "global": N},
//!   "scopes": {"repo": N, "global": N}, "review": {"needed": N, "trusted": N}, "scripts": [...]}`:
//!   one page of the visible scripts (the repository of `--project`, else of the working
//!   directory, and the global scope) as the objects `script_list` returns. The groups do not
//!   overlap: `needsReview` holds every script that needs approval, `repo` and `global` the others
//!   of their scope. `--scope` keeps the scripts of one scope, `--review needed` those that need
//!   approval (the `needsReview` group), `--review trusted` every other one; the two combine,
//!   and neither combines with `--group`. `--query` keeps the scripts whose name or description
//!   contains it, ignoring case (as `script_list` does). The group counts follow `--query`, the
//!   scope counts `--query` and `--review`, the review counts `--query` and `--scope`, the total
//!   every filter. By name (ties: repository scripts first) ascending unless `--sort updated`
//!   (newest first), `language`, `scope` (global scripts first), `place` (every listed script is
//!   in one place, so by id), `runs` (the run count) or `lastRun` (the latest run; a script never
//!   run comes last either way), all ascending; `--order` turns any round, ties go by id.
//!   `--limit` is 1 to 200, 50 when not given; `--untrusted` is `--group needsReview`. Approval
//!   is decided on this machine, so every visible script is read and the page is cut here, not
//!   in the database.
//! - `harness-memory scripts delete <id> [--project PATH]` deletes a visible script as
//!   `script_delete` does (a version that needs approval is remembered first, so saving its code
//!   again trusts nothing) and prints `deleted script <id> <name>`; a script that is not visible
//!   exits 1 with `harness-memory: script <id> not found`.
//! - `harness-memory scripts delete --all --yes [--project PATH] [--json]` does that for every
//!   script `scripts list` shows without filters (all groups, all pages), deleted in one write,
//!   and prints `deleted N scripts` (and how many came from the team database), or
//!   `{"deleted": N, "team": M}` with `--json` (`team`: how many were kept in the team database).
//!   Without `--yes` it deletes nothing and exits 2.
//! - `harness-memory trust <id>=<sha256>... [--project PATH]` approves those scripts on this
//!   machine when each still has the `sha256` that was reviewed (the `sha256` of `scripts list`);
//!   a script changed since is not approved. `harness-memory trust <id>...` and
//!   `harness-memory trust --all [--project PATH]` (every visible script that needs it) approve the
//!   versions there are now, for a person who reads them in the terminal: they run only with a
//!   terminal as standard input (an agent's shell tool has none, so it cannot approve what no one
//!   reviewed), else they exit 2. One line per script on stdout; exit 1 when an id is not found,
//!   a script changed since its review, or a body holds a hidden character or declares an encoding
//!   other than UTF-8 (such a script can never run); the others are still approved.
//!
//! Exit codes and errors as in [`crate::cli`].

use std::cmp::Ordering;
use std::io::IsTerminal;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::backend::{self, Place, Selection};
use crate::cli::{self, Args, Delete, PAGE_OPTIONS, Page};
use crate::limits::QUERY_MAX;
use crate::mcp::{Approval, Shown, approval_of};
use crate::open::{open_scripts, open_trust};
use crate::store::{
    Scope, Script, ScriptRef, ScriptStore, ScriptSummary, TrustOrigin, TrustStore, TrustedVersion, View,
};
use crate::store::{foreign_encoding, foreign_encoding_error, hidden_character, hidden_character_error};

/// The usage of these commands, for the executable's usage line.
pub const USAGE: &str = "scripts list --json [--project PATH] [--group needsReview|repo|global] [--scope \
                         repo|global] [--review needed|trusted] [--query TEXT] [--sort name|language|scope|place|runs|lastRun|updated] [--order asc|desc] [--offset N] [--limit N] | scripts delete <id> \
                         [--project PATH] | scripts delete --all --yes [--project PATH] [--json] | trust \
                         <id>[=<sha256>]... | --all [--project PATH]";

/// The line `harness-memory check` adds when every team script runs without approval.
pub fn check_line(selection: &Selection) -> Option<&'static str> {
    selection
        .team_scripts_trusted()
        .then_some("team scripts: trusted automatically")
}

/// `harness-memory scripts <args>`.
pub fn scripts(args: &[Option<&str>]) -> ExitCode {
    match args {
        [Some("delete"), rest @ ..] => delete(rest),
        _ => list(args),
    }
}

/// `scripts delete`.
fn delete(args: &[Option<&str>]) -> ExitCode {
    match Delete::parse(args) {
        Some(Delete::One { id, project }) => delete_one(id, project.as_deref()),
        Some(Delete::All { project, json }) => delete_all(project.as_deref(), json),
        Some(Delete::Unconfirmed) => cli::unconfirmed("script"),
        None => usage(),
    }
}

/// `scripts delete <id>`.
fn delete_one(id: i64, project: Option<&str>) -> ExitCode {
    let Some(mut opened) = Opened::open(project) else {
        return ExitCode::FAILURE;
    };

    match opened.delete(id) {
        Ok(Some(name)) => {
            println!("deleted script {id} {name}");
            ExitCode::SUCCESS
        }
        Ok(None) => opened.failed(&format!("script {id} not found")),
        Err(message) => opened.failed(&message),
    }
}

/// `scripts delete --all --yes`.
fn delete_all(project: Option<&str>, json: bool) -> ExitCode {
    let Some(mut opened) = Opened::open(project) else {
        return ExitCode::FAILURE;
    };

    // Listing decides each script's approval, which remembers the versions that need it.
    let ids: Vec<i64> = match opened.list(None) {
        Ok(listed) => listed.iter().map(|(summary, _)| summary.id).collect(),
        Err(message) => return opened.failed(&message),
    };

    let deleted = match opened.scripts.delete_scripts(&opened.view, &ids) {
        Ok(deleted) => deleted,
        Err(error) => return opened.failed(&error.to_string()),
    };

    let team = if opened.place == Place::Remote { deleted } else { 0 };

    cli::deleted_all("script", deleted, team, json);
    ExitCode::SUCCESS
}

/// `scripts list`.
fn list(args: &[Option<&str>]) -> ExitCode {
    let Some(options) = parse_list(args) else {
        return usage();
    };

    let Some(mut opened) = Opened::open(options.project.as_deref()) else {
        return ExitCode::FAILURE;
    };

    let page = &options.page;

    if page.query.chars().count() > QUERY_MAX {
        return opened.failed(&format!("query is longer than {QUERY_MAX} characters"));
    }

    let query = (!page.query.is_empty()).then_some(page.query.as_str());
    let mut listed = match opened.list(query) {
        Ok(listed) => listed,
        Err(message) => return opened.failed(&message),
    };

    let count = |keeps: &dyn Fn(&ScriptSummary, Approval) -> bool| {
        listed
            .iter()
            .filter(|(summary, approval)| keeps(summary, *approval))
            .count()
    };
    let in_group = |group: Group| count(&|summary, approval| Group::of(summary, approval) == group);
    let in_scope = |scope: Scope| count(&|summary, approval| summary.scope == scope && options.review_keeps(approval));
    let in_review =
        |review: Review| count(&|summary, approval| Review::of(approval) == review && options.scope_keeps(summary));

    let groups = json!({
        "needsReview": in_group(Group::NeedsReview),
        "repo": in_group(Group::Repo),
        "global": in_group(Group::Global),
    });
    let scopes = json!({
        "repo": in_scope(Scope::Repo),
        "global": in_scope(Scope::Global),
    });
    let review = json!({
        "needed": in_review(Review::Needed),
        "trusted": in_review(Review::Trusted),
    });

    listed.retain(|(summary, approval)| options.keeps(summary, *approval));
    listed.sort_by(|(a, _), (b, _)| options.compare(a, b));

    let objects: Vec<Value> = listed
        .iter()
        .skip(page.offset)
        .take(page.limit)
        .map(|(summary, approval)| opened.json(summary, *approval))
        .collect();

    println!(
        "{}",
        json!({
            "total": listed.len(),
            "groups": groups,
            "scopes": scopes,
            "review": review,
            "scripts": objects,
        })
    );
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

    let Some(mut opened) = Opened::open(options.project.as_deref()) else {
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
        match opened.approve(&pick) {
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

/// The options of `scripts list --json`.
#[derive(Debug)]
struct ListOptions {
    group: Option<Group>,
    /// `--scope`; never given with [`Self::group`].
    scope: Option<Scope>,
    /// `--review`; never given with [`Self::group`].
    review: Option<Review>,
    project: Option<String>,
    sort: ScriptSort,
    descending: bool,
    page: Page,
}

impl ListOptions {
    /// True when the group, the scope and the review asked for keep the script.
    fn keeps(&self, summary: &ScriptSummary, approval: Approval) -> bool {
        let in_group = self.group.is_none_or(|group| Group::of(summary, approval) == group);

        in_group && self.scope_keeps(summary) && self.review_keeps(approval)
    }

    /// True when the scope asked for (if any) keeps the script.
    fn scope_keeps(&self, summary: &ScriptSummary) -> bool {
        self.scope.is_none_or(|scope| summary.scope == scope)
    }

    /// True when the review asked for (if any) keeps a script with `approval`.
    fn review_keeps(&self, approval: Approval) -> bool {
        self.review.is_none_or(|review| Review::of(approval) == review)
    }

    /// The order of the list: by [`Self::sort`], then by id, turned round when descending; a
    /// script never run comes after the others by `lastRun` either way.
    fn compare(&self, a: &ScriptSummary, b: &ScriptSummary) -> Ordering {
        let is_repo = |summary: &ScriptSummary| summary.scope == Scope::Repo;

        let ascending = match self.sort {
            ScriptSort::Name => a.name.cmp(&b.name).then(is_repo(b).cmp(&is_repo(a))),
            ScriptSort::Language => a.language.as_str().cmp(b.language.as_str()),
            ScriptSort::Scope => is_repo(a).cmp(&is_repo(b)),
            ScriptSort::Place => Ordering::Equal,
            ScriptSort::Runs => a.run_count.cmp(&b.run_count),
            ScriptSort::LastRun => match (a.last_run_at, b.last_run_at) {
                (Some(a_run), Some(b_run)) => a_run.cmp(&b_run),
                (Some(_), None) => return Ordering::Less,
                (None, Some(_)) => return Ordering::Greater,
                (None, None) => Ordering::Equal,
            },
            ScriptSort::Updated => a.updated_at.cmp(&b.updated_at),
        }
        .then(a.id.cmp(&b.id));

        if self.descending {
            ascending.reverse()
        } else {
            ascending
        }
    }
}

/// What `scripts list` sorts by (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptSort {
    Name,
    Language,
    Scope,
    Place,
    Runs,
    LastRun,
    Updated,
}

impl ScriptSort {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "name" => Some(Self::Name),
            "language" => Some(Self::Language),
            "scope" => Some(Self::Scope),
            "place" => Some(Self::Place),
            "runs" => Some(Self::Runs),
            "lastRun" => Some(Self::LastRun),
            "updated" => Some(Self::Updated),
            _ => None,
        }
    }
}

/// The group of a listed script: every script that needs approval, else its scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    NeedsReview,
    Repo,
    Global,
}

impl Group {
    fn of(summary: &ScriptSummary, approval: Approval) -> Self {
        if !approval.is_trusted() {
            return Self::NeedsReview;
        }

        match summary.scope {
            Scope::Repo => Self::Repo,
            Scope::Global => Self::Global,
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "needsReview" => Some(Self::NeedsReview),
            "repo" => Some(Self::Repo),
            "global" => Some(Self::Global),
            _ => None,
        }
    }
}

/// Whether a listed script needs approval (the [`Group::NeedsReview`] scripts) or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Review {
    Needed,
    Trusted,
}

impl Review {
    fn of(approval: Approval) -> Self {
        if approval.is_trusted() {
            Self::Trusted
        } else {
            Self::Needed
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "needed" => Some(Self::Needed),
            "trusted" => Some(Self::Trusted),
            _ => None,
        }
    }
}

/// The options of `trust`: the scripts, or `None` for `--all`.
#[derive(Debug, Default)]
struct TrustOptions {
    picks: Option<Vec<Pick>>,
    project: Option<String>,
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

/// The options after `scripts`; `None` for anything but `list --json` with known, unrepeated
/// options and valid values, and for a group (`--group`, `--untrusted`) with `--scope` or
/// `--review`.
fn parse_list(args: &[Option<&str>]) -> Option<ListOptions> {
    let valued = [
        &PAGE_OPTIONS[..],
        &["--project", "--group", "--scope", "--review", "--sort"],
    ]
    .concat();
    let parsed = Args::parse(args, &["--json", "--untrusted"], &valued)?;

    if parsed.positional != ["list"] || !parsed.flag("--json") {
        return None;
    }

    let group = match (parsed.flag("--untrusted"), parsed.value("--group")) {
        (true, Some(_)) => return None,
        (true, None) => Some(Group::NeedsReview),
        (false, Some(raw)) => Some(Group::parse(raw)?),
        (false, None) => None,
    };

    let scope = parsed.scope().ok()?;
    let review = match parsed.value("--review") {
        Some(raw) => Some(Review::parse(raw)?),
        None => None,
    };

    if group.is_some() && (scope.is_some() || review.is_some()) {
        return None;
    }

    let sort = match parsed.value("--sort") {
        Some(raw) => ScriptSort::parse(raw)?,
        None => ScriptSort::Name,
    };

    let page = Page::of(&parsed)?;

    Some(ListOptions {
        group,
        scope,
        review,
        project: parsed.project().ok()?,
        sort,
        descending: page.descending.unwrap_or(sort == ScriptSort::Updated),
        page,
    })
}

/// The options after `trust`; `None` unless they name scripts or `--all` (not both).
fn parse_trust(args: &[Option<&str>]) -> Option<TrustOptions> {
    let parsed = Args::parse(args, &["--all"], &["--project"])?;
    let project = parsed.project().ok()?;
    let picks = parsed
        .positional
        .iter()
        .map(|arg| Pick::parse(arg))
        .collect::<Option<Vec<_>>>()?;

    match (parsed.flag("--all"), picks.is_empty()) {
        (true, true) => Some(TrustOptions { picks: None, project }),
        (false, false) => Some(TrustOptions {
            picks: Some(picks),
            project,
        }),
        _ => None,
    }
}

fn usage() -> ExitCode {
    cli::usage(USAGE)
}

/// The script store, this machine's trust and the visible scripts, opened like the server does.
struct Opened {
    scripts: Box<dyn ScriptStore>,
    trust: Box<dyn TrustStore>,
    place: Place,
    /// The trust name of where the scripts are ([`Selection::scripts_key`]).
    key: String,
    team_trusted: bool,
    view: View,
    secrets: backend::Secrets,
}

impl Opened {
    /// Opens the stores for the scripts visible from `project` (else the working directory);
    /// `None`, with the reason printed, when the settings or the project are wrong.
    fn open(project: Option<&str>) -> Option<Self> {
        let (env, selection) = cli::settings()?;
        let repo = cli::repository(&env, project)?;

        Some(Self {
            scripts: open_scripts(&env, &selection),
            trust: open_trust(&env),
            place: selection.scripts_place(),
            key: selection.scripts_key(),
            team_trusted: selection.team_scripts_trusted(),
            view: View { repo, global: true },
            secrets: selection.backend.secrets(),
        })
    }

    /// Every visible script `query` keeps, with how this machine may run it, as `script_list`
    /// lists them.
    fn list(&mut self, query: Option<&str>) -> Result<Vec<(ScriptSummary, Approval)>, String> {
        let summaries = self
            .scripts
            .list_scripts(&self.view, query)
            .map_err(|error| error.to_string())?;

        summaries
            .into_iter()
            .map(|summary| {
                let approval = approval_of(
                    self.trust.as_mut(),
                    self.team_trusted,
                    &self.key,
                    &summary,
                    &summary.sha256,
                )?;
                Ok((summary, approval))
            })
            .collect()
    }

    /// Deletes the visible script `id` as `script_delete` does, after deciding its approval (which
    /// remembers a version that needs it); its name, `None` when it is not visible.
    fn delete(&mut self, id: i64) -> Result<Option<String>, String> {
        let found = self
            .scripts
            .get_script(&self.view, &ScriptRef::Id(id))
            .map_err(|error| error.to_string())?;

        let Some(Script { summary, .. }) = found else {
            return Ok(None);
        };

        approval_of(
            self.trust.as_mut(),
            self.team_trusted,
            &self.key,
            &summary,
            &summary.sha256,
        )?;

        let deleted = self
            .scripts
            .delete_script(&self.view, &ScriptRef::Id(id))
            .map_err(|error| error.to_string())?;

        Ok(deleted.then_some(summary.name))
    }

    /// The `script_list` object of `summary`.
    fn json(&self, summary: &ScriptSummary, approval: Approval) -> Value {
        Shown {
            summary,
            sha256: &summary.sha256,
            place: self.place,
            approval,
        }
        .json()
    }

    /// Approves the script `pick` names as read now, unless it changed since the `sha256` the
    /// person reviewed or its body holds a [`hidden_character`] or declares a [`foreign_encoding`].
    fn approve(&mut self, pick: &Pick) -> Result<Approved, String> {
        let id = pick.id;
        let script = self
            .scripts
            .get_script(&self.view, &ScriptRef::Id(id))
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

        let trusted = self
            .trust
            .trust_of(&self.key, id, &summary.sha256)
            .map_err(|error| error.to_string())?;

        if trusted.is_some_and(|record| record.is_for(summary)) {
            return Ok(Approved::Line(format!("script {id} {name} is already trusted")));
        }

        let version = TrustedVersion::of(&self.key, &script, TrustOrigin::Approved);
        self.trust.trust(&version).map_err(|error| error.to_string())?;

        Ok(Approved::Line(format!("trusted script {id} {name}")))
    }

    /// Prints `message` without the database credentials; exit 1.
    fn failed(&self, message: &str) -> ExitCode {
        eprintln!("harness-memory: {}", self.secrets.scrub(message));
        ExitCode::FAILURE
    }
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

        let selection = |backend: Backend, scripts: Place, trust_team_scripts: bool| Selection {
            backend,
            routing: Routing {
                scripts,
                ..Routing::default()
            },
            trust_team_scripts,
            hooks: Place::Local,
            trust_team_hooks: false,
            warnings: Vec::new(),
        };
        let team = || Backend::Postgres {
            url: "postgres://db/memory".to_owned(),
        };

        assert_eq!(
            check_line(&selection(team(), Place::Remote, true)),
            Some("team scripts: trusted automatically")
        );
        assert_eq!(check_line(&selection(team(), Place::Remote, false)), None);
        assert_eq!(check_line(&selection(team(), Place::Local, true)), None);
        assert_eq!(check_line(&selection(Backend::Local, Place::Remote, true)), None);
    }

    #[test]
    fn list_options_need_json_and_take_a_project() {
        let parsed = parse_list(&args(&["list", "--json", "--project", "/w", "--untrusted"])).expect("valid");
        assert_eq!(parsed.group, Some(Group::NeedsReview));
        assert_eq!((parsed.sort, parsed.descending), (ScriptSort::Name, false));
        assert_eq!(parsed.project.as_deref(), Some("/w"));

        for (raw, sort) in [
            ("language", ScriptSort::Language),
            ("scope", ScriptSort::Scope),
            ("place", ScriptSort::Place),
            ("runs", ScriptSort::Runs),
            ("lastRun", ScriptSort::LastRun),
        ] {
            let parsed = parse_list(&args(&["list", "--json", "--sort", raw])).expect("valid");
            assert_eq!((parsed.sort, parsed.descending), (sort, false), "{raw}");
        }

        let filtered =
            parse_list(&args(&["list", "--json", "--scope", "global", "--review", "needed"])).expect("valid");
        assert_eq!(
            (filtered.group, filtered.scope, filtered.review),
            (None, Some(Scope::Global), Some(Review::Needed))
        );

        let updated = parse_list(&args(&["list", "--json", "--group", "repo", "--sort", "updated"])).expect("valid");
        assert_eq!(
            (updated.group, updated.sort, updated.descending),
            (Some(Group::Repo), ScriptSort::Updated, true)
        );
        assert_eq!(
            parse_list(&args(&["list", "--json", "--project=/w"]))
                .expect("valid")
                .project
                .as_deref(),
            Some("/w")
        );

        for bad in [
            &["list"][..],
            &["--json"],
            &["show", "--json"],
            &["list", "--json", "--json"],
            &["list", "--json", "--project"],
            &["list", "--json", "--project", "/a", "--project", "/b"],
            &["list", "--json", "--yaml"],
            &["list", "--json", "--untrusted", "--group", "repo"],
            &["list", "--json", "--untrusted", "--scope", "repo"],
            &["list", "--json", "--group", "repo", "--review", "trusted"],
            &["list", "--json", "--scope", "both"],
            &["list", "--json", "--review", "pending"],
            &["list", "--json", "--group", "trusted"],
            &["list", "--json", "--sort", "size"],
            &["list", "--json", "--limit", "500"],
        ] {
            assert!(parse_list(&args(bad)).is_none(), "{bad:?}");
        }
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
        assert_eq!(ids.project.as_deref(), Some("/w"));
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
