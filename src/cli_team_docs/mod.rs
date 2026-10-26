//! The team document commands of the `harness-memory` executable, `hooks` and `routines` (one
//! [`DocKind`] each, the same commands and replies), written for Harness Desktop acting on the
//! user's behalf. There is no MCP tool for them, but these are plain commands: any process running
//! as the user, an agent with a shell tool included, can run `hooks put` or `routines put` and
//! share a file with everyone on the team database. They read and write the team database only
//! while `hooks = "remote"` (`routines = "remote"`) and one is set ([`Selection::docs_place`]).
//!
//! Below for hooks; routines are the same with `routines` for `hooks` and `trustTeamRoutines` for
//! `trustTeamHooks`.
//!
//! - `harness-memory hooks config --json` prints `{"hooks": "remote"|"local", "trustTeamHooks":
//!   bool}` (exit 0 whenever the settings load), so Harness Desktop can probe for these commands.
//!   `trustTeamHooks` is `trust_team_hooks` while hooks are shared, else `false`
//!   ([`Selection::team_docs_trusted`]).
//! - `harness-memory hooks list --json [--project PATH] [--offset N] [--limit N]` prints
//!   `{"total": N, "nextOffset": N|null, "project": {"key", "shareable"}|null, "hooks": [...]}`:
//!   one page of the global hooks and those of `--project`'s repository, global first, then by
//!   `createdAt`, then by id. A row this server never writes is left out of `hooks` but counts in
//!   `total`, so a page can hold fewer hooks than `--limit`: `nextOffset` is the `--offset` of the
//!   next page (after the last row read, left out ones included), `null` when no rows follow.
//!   `project` is the identity of `--project` (`shareable` when it is a `git:` key; a `path:` key
//!   is never sent to the database), `null` without it. Not shared: `total` 0, no hooks.
//!   `--limit` is 1 to 200, 50 when not given.
//! - `harness-memory hooks get <id> --json` prints the hook object.
//! - `harness-memory hooks put --json [--project PATH] [--expect-sha SHA256] [--author NAME]`
//!   reads a hook file (at most 16384 bytes) from stdin and stores it (see
//!   [`crate::store::team_docs`] for the body rules): without `--expect-sha` as a new hook, with
//!   it as the new version of the hook with that id while its `sha256` is still the one given.
//!   Prints the hook object. A project hook's `project` is the repository key of `--project`. The
//!   author is `--author`, else the user name (`USER`, `USERNAME`), else `unknown`.
//! - `harness-memory hooks delete <id> [--expect-sha SHA256] --json` prints `{"deleted": bool}`
//!   (`false` when no hook has the id).
//! - `harness-memory routines delete --mine [--author NAME] --json` (routines only) deletes every
//!   team routine whose author is `--author`, else the user name `put` would record, and prints
//!   `{"deleted": N, "ids": [...]}`. Without `--author` and without a usable user name it is
//!   `refused` (it never deletes the routines of the shared `unknown` author). The author is a
//!   free-text label anyone can write, so this is a convenience, not a permission.
//!
//! A hook object is `{"id", "repo" (the `git:` key, `null` for a global hook), "sha256" (of the
//! body bytes), "author", "createdAt", "updatedAt" (RFC 3339 UTC), "body"}`.
//!
//! A refusal prints `{"error": CODE, "message": TEXT}` on stdout and exits 1, the message also
//! the last line of stderr: `conflict` (the id is taken, or the hook changed since its
//! `sha256`), `notFound`, `invalid` (the body breaks the hook file format, or a bad author),
//! `refused` (a Harness family hook, a repository without an `origin` remote, a `project` that is
//! not the repository of `--project`), `limit` (the scope holds 64 hooks), `local` (hooks are not
//! shared). Other failures (settings, a `--project` that is not a folder, the database) exit 1
//! with only the stderr line; usage errors exit 2, as in [`crate::cli`].

mod put;

use std::process::ExitCode;

use serde_json::{Value, json};

use crate::backend::{self, Place, Secrets, Selection};
use crate::cli::{self, Args, Page};
use crate::limits::TEAM_DOC_AUTHOR_MAX;
use crate::store::team_docs::{BodyProblem, check_author, is_doc_id, is_sha256};
use crate::store::{DocDelete, DocKind, ProjectKey, TeamDoc, TeamDocStore};
use crate::support::Env;
use crate::support::time::rfc3339;

/// The usage of the hook commands, for the executable's usage line.
pub const HOOKS_USAGE: &str = "hooks config --json | hooks list --json [--project PATH] [--offset N] [--limit N] | \
                               hooks get <id> --json | hooks put --json [--project PATH] [--expect-sha SHA256] \
                               [--author NAME] < hook.json | hooks delete <id> [--expect-sha SHA256] --json";

/// The usage of the routine commands, for the executable's usage line.
pub const ROUTINES_USAGE: &str = "routines config --json | routines list --json [--project PATH] [--offset N] \
                                  [--limit N] | routines get <id> --json | routines put --json [--project PATH] \
                                  [--expect-sha SHA256] [--author NAME] < routine.json | routines delete <id> \
                                  [--expect-sha SHA256] --json | routines delete --mine [--author NAME] --json";

/// The author when neither `--author` nor a usable user name gives one.
const UNKNOWN_AUTHOR: &str = "unknown";

/// `harness-memory hooks <args>`.
pub fn hooks(args: &[Option<&str>]) -> ExitCode {
    run(DocKind::Hooks, args)
}

/// `harness-memory routines <args>`.
pub fn routines(args: &[Option<&str>]) -> ExitCode {
    run(DocKind::Routines, args)
}

fn run(kind: DocKind, args: &[Option<&str>]) -> ExitCode {
    match args {
        [Some("config"), rest @ ..] => config(kind, rest),
        [Some("list"), rest @ ..] => list(kind, rest),
        [Some("get"), rest @ ..] => get(kind, rest),
        [Some("put"), rest @ ..] => put::put(kind, rest),
        [Some("delete"), rest @ ..] => delete(kind, rest),
        _ => usage(kind),
    }
}

fn usage(kind: DocKind) -> ExitCode {
    match kind {
        DocKind::Hooks => cli::usage(HOOKS_USAGE),
        DocKind::Routines => cli::usage(ROUTINES_USAGE),
    }
}

/// The `config` key saying whether the team's documents of `kind` run without approval.
fn trust_key(kind: DocKind) -> &'static str {
    match kind {
        DocKind::Hooks => "trustTeamHooks",
        DocKind::Routines => "trustTeamRoutines",
    }
}

/// A refusal with a code (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Code {
    Conflict,
    NotFound,
    Invalid,
    Refused,
    Limit,
    Local,
}

impl Code {
    fn as_str(self) -> &'static str {
        match self {
            Self::Conflict => "conflict",
            Self::NotFound => "notFound",
            Self::Invalid => "invalid",
            Self::Refused => "refused",
            Self::Limit => "limit",
            Self::Local => "local",
        }
    }
}

/// `config`.
fn config(kind: DocKind, args: &[Option<&str>]) -> ExitCode {
    if !json_only(args) {
        return usage(kind);
    }

    let Some((_, selection)) = cli::settings() else {
        return ExitCode::FAILURE;
    };

    println!(
        "{}",
        json!({
            kind.table(): selection.docs_place(kind).as_str(),
            trust_key(kind): selection.team_docs_trusted(kind),
        })
    );
    ExitCode::SUCCESS
}

/// `list`.
fn list(kind: DocKind, args: &[Option<&str>]) -> ExitCode {
    let Some(parsed) = Args::parse(args, &["--json"], &["--project", "--offset", "--limit"]) else {
        return usage(kind);
    };

    let (Some(page), Ok(project)) = (Page::of(&parsed), parsed.project()) else {
        return usage(kind);
    };

    if !parsed.positional.is_empty() || !parsed.flag("--json") {
        return usage(kind);
    }

    let Some(mut opened) = Opened::open(kind) else {
        return ExitCode::FAILURE;
    };

    let identity = match project {
        Some(path) => match cli::repository(&opened.env, Some(&path)) {
            Some(identity) => identity,
            None => return ExitCode::FAILURE,
        },
        None => None,
    };

    let shown = identity
        .as_ref()
        .map(|identity| json!({ "key": identity.key, "shareable": is_shareable(identity) }));

    let Some(store) = opened.team() else {
        println!(
            "{}",
            json!({ "total": 0, "nextOffset": null, "project": shown, kind.table(): [] })
        );
        return ExitCode::SUCCESS;
    };

    let store = match store {
        Ok(store) => store,
        Err(message) => return opened.failed(&message),
    };

    // A `path:` key names a folder on this computer: it is never sent to the team database.
    let repo = identity.filter(is_shareable).map(|identity| identity.key);

    match store.list_docs(kind, repo.as_deref(), page.offset, page.limit) {
        Ok(listed) => {
            let docs: Vec<Value> = listed.docs.iter().map(row_json).collect();
            let reply = json!({
                "total": listed.total,
                "nextOffset": listed.next_offset,
                "project": shown,
                kind.table(): docs,
            });
            println!("{reply}");
            ExitCode::SUCCESS
        }
        Err(error) => opened.failed(&error.to_string()),
    }
}

/// `get`.
fn get(kind: DocKind, args: &[Option<&str>]) -> ExitCode {
    let Some(parsed) = Args::parse(args, &["--json"], &[]) else {
        return usage(kind);
    };

    let Some(id) = one_id(&parsed) else {
        return usage(kind);
    };

    if !parsed.flag("--json") {
        return usage(kind);
    }

    let Some(mut opened) = Opened::open(kind) else {
        return ExitCode::FAILURE;
    };

    let store = match opened.shared() {
        Ok(store) => store,
        Err(exit) => return exit,
    };

    match store.get_doc(kind, id) {
        Ok(Some(doc)) => {
            println!("{}", row_json(&doc));
            ExitCode::SUCCESS
        }
        Ok(None) => opened.refused(Code::NotFound, &missing(kind, id)),
        Err(error) => opened.failed(&error.to_string()),
    }
}

/// `delete <id>`, and `delete --mine` for routines.
fn delete(kind: DocKind, args: &[Option<&str>]) -> ExitCode {
    let mine = kind == DocKind::Routines;
    let (flags, valued): (&[&str], &[&str]) = if mine {
        (&["--json", "--mine"], &["--expect-sha", "--author"])
    } else {
        (&["--json"], &["--expect-sha"])
    };

    let Some(parsed) = Args::parse(args, flags, valued) else {
        return usage(kind);
    };

    if !parsed.flag("--json") {
        return usage(kind);
    }

    if parsed.flag("--mine") {
        return delete_mine(kind, &parsed);
    }

    let (Some(id), Some(expect)) = (one_id(&parsed), expected_sha(&parsed)) else {
        return usage(kind);
    };

    if parsed.value("--author").is_some() {
        return usage(kind);
    }

    let Some(mut opened) = Opened::open(kind) else {
        return ExitCode::FAILURE;
    };

    let store = match opened.shared() {
        Ok(store) => store,
        Err(exit) => return exit,
    };

    match store.delete_doc(kind, id, expect.as_deref()) {
        Ok(outcome @ (DocDelete::Deleted | DocDelete::Missing)) => {
            println!("{}", json!({ "deleted": outcome == DocDelete::Deleted }));
            ExitCode::SUCCESS
        }
        Ok(DocDelete::Changed) => opened.refused(Code::Conflict, &changed(kind, id)),
        Err(error) => opened.failed(&error.to_string()),
    }
}

/// `delete --mine [--author NAME] --json`: every document of the author `put` would record.
fn delete_mine(kind: DocKind, parsed: &Args) -> ExitCode {
    if !parsed.positional.is_empty() || parsed.value("--expect-sha").is_some() {
        return usage(kind);
    }

    let Some(mut opened) = Opened::open(kind) else {
        return ExitCode::FAILURE;
    };

    if opened.selection.docs_place(kind) == Place::Local {
        return opened.not_shared();
    }

    let given = parsed.value("--author");

    // Without a user name `put` records the shared `unknown` author: deleting those would remove
    // other people's routines too.
    if given.is_none() && user_author(&opened.env).is_none() {
        return opened.refused(
            Code::Refused,
            "no user name (USER, USERNAME) says who you are: set --author",
        );
    }

    let author = match opened.author(given) {
        Ok(author) => author,
        Err(exit) => return exit,
    };

    let store = match opened.shared() {
        Ok(store) => store,
        Err(exit) => return exit,
    };

    match store.delete_docs_by(kind, &author) {
        Ok(ids) => {
            println!("{}", json!({ "deleted": ids.len(), "ids": ids }));
            ExitCode::SUCCESS
        }
        Err(error) => opened.failed(&error.to_string()),
    }
}

fn missing(kind: DocKind, id: &str) -> String {
    format!("no team {} has the id {id}", kind.noun())
}

fn changed(kind: DocKind, id: &str) -> String {
    format!(
        "team {} {id} changed since it was read (its sha256 is not the one given): read it again",
        kind.noun()
    )
}

/// True for `--json` alone.
fn json_only(args: &[Option<&str>]) -> bool {
    Args::parse(args, &["--json"], &[]).is_some_and(|parsed| parsed.positional.is_empty() && parsed.flag("--json"))
}

/// The one positional argument, when it is a document id.
fn one_id<'a>(parsed: &Args<'a>) -> Option<&'a str> {
    let [id] = parsed.positional[..] else {
        return None;
    };

    is_doc_id(id).then_some(id)
}

/// `--expect-sha` in lowercase: `Some(None)` when not given, `None` when it is not 64 hex digits.
fn expected_sha(parsed: &Args) -> Option<Option<String>> {
    match parsed.value("--expect-sha") {
        None => Some(None),
        Some(raw) => {
            let lower = raw.to_ascii_lowercase();
            is_sha256(&lower).then_some(Some(lower))
        }
    }
}

/// True for a repository key that may leave this computer (`git:`).
fn is_shareable(identity: &ProjectKey) -> bool {
    identity.key.starts_with("git:")
}

/// The object of `doc` (see the module docs).
fn row_json(doc: &TeamDoc) -> Value {
    json!({
        "id": doc.id,
        "repo": doc.repo,
        "sha256": doc.sha256,
        "author": doc.author,
        "createdAt": rfc3339(doc.created_at),
        "updatedAt": rfc3339(doc.updated_at),
        "body": doc.body,
    })
}

/// [`user_author`], else [`UNKNOWN_AUTHOR`].
fn default_author(env: &Env) -> String {
    user_author(env).unwrap_or_else(|| UNKNOWN_AUTHOR.to_owned())
}

/// The user name (`USER`, else `USERNAME`) trimmed and cut to [`TEAM_DOC_AUTHOR_MAX`]
/// characters; `None` when there is none or it holds a control or invisible character.
fn user_author(env: &Env) -> Option<String> {
    let name = env.var("USER").or_else(|| env.var("USERNAME")).unwrap_or_default();
    let cut: String = name.trim().chars().take(TEAM_DOC_AUTHOR_MAX).collect();

    check_author(&cut).ok()
}

/// The settings and, on first use, the team database, for one kind.
struct Opened {
    kind: DocKind,
    env: Env,
    selection: Selection,
    secrets: Secrets,
    store: Option<Box<dyn TeamDocStore>>,
}

impl Opened {
    /// The settings; `None`, with the reason printed, when they are wrong.
    fn open(kind: DocKind) -> Option<Self> {
        let (env, selection) = cli::settings()?;

        Some(Self {
            kind,
            env,
            secrets: selection.backend.secrets(),
            selection,
            store: None,
        })
    }

    /// The team database, connected on first use; `None` when the kind is not shared.
    fn team(&mut self) -> Option<Result<&mut Box<dyn TeamDocStore>, String>> {
        if self.selection.docs_place(self.kind) == Place::Local {
            return None;
        }

        if self.store.is_none() {
            match backend::open_team_docs(&self.selection.backend)? {
                Ok(store) => self.store = Some(store),
                Err(message) => {
                    let hint = crate::open::docs_hint(self.kind.table());
                    return Some(Err(format!("{message}. {hint}")));
                }
            }
        }

        self.store.as_mut().map(Ok)
    }

    /// The team database, or the exit of a refusal (`local`) or a failure to connect.
    fn shared(&mut self) -> Result<&mut Box<dyn TeamDocStore>, ExitCode> {
        if self.selection.docs_place(self.kind) == Place::Local {
            return Err(self.not_shared());
        }

        let secrets = self.secrets.clone();

        match self.team() {
            Some(Ok(store)) => Ok(store),
            Some(Err(message)) => Err(failure(&secrets, &message)),
            None => Err(failure(&secrets, "no team database is set")),
        }
    }

    /// `--author` checked, else [`default_author`]; the exit of an `invalid` refusal.
    fn author(&self, given: Option<&str>) -> Result<String, ExitCode> {
        match given {
            Some(author) => check_author(author).map_err(|error| self.refused(Code::Invalid, &error.to_string())),
            None => Ok(default_author(&self.env)),
        }
    }

    fn not_shared(&self) -> ExitCode {
        let plural = self.kind.table();

        self.refused(
            Code::Local,
            &format!(
                "team {plural} are not shared: set {plural} = \"remote\" in the config file, next to a team \
                 database_url"
            ),
        )
    }

    fn problem(&self, problem: &BodyProblem) -> ExitCode {
        let code = match problem {
            BodyProblem::Invalid(_) => Code::Invalid,
            BodyProblem::Refused(_) => Code::Refused,
        };

        self.refused(code, &problem.message())
    }

    /// Prints the refusal `code` with `message` on stdout and stderr (without the database
    /// credentials); exit 1.
    fn refused(&self, code: Code, message: &str) -> ExitCode {
        let message = self.secrets.scrub(message);

        println!("{}", json!({ "error": code.as_str(), "message": message }));
        eprintln!("harness-memory: {message}");
        ExitCode::FAILURE
    }

    /// Prints `message` without the database credentials; exit 1.
    fn failed(&self, message: &str) -> ExitCode {
        failure(&self.secrets, message)
    }
}

fn failure(secrets: &Secrets, message: &str) -> ExitCode {
    crate::cli::failed(&secrets.scrub(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(raw: &[&'a str]) -> Vec<Option<&'a str>> {
        raw.iter().map(|arg| Some(*arg)).collect()
    }

    #[test]
    fn ids_and_expected_hashes_are_checked() {
        let id = "1a2b3c4d5e6f70818293a4b5c6d7000b";
        let parsed = Args::parse(&args(&[id, "--json"]), &["--json"], &[]).expect("parsed");
        assert_eq!(one_id(&parsed), Some(id));

        let upper = "AB".repeat(32);
        let parsed = Args::parse(&args(&["--expect-sha", &upper]), &[], &["--expect-sha"]).expect("parsed");
        assert_eq!(expected_sha(&parsed), Some(Some("ab".repeat(32))));

        for bad in ["1A2B3C4D5E6F70818293A4B5C6D7000B", "7", "x"] {
            let parsed = Args::parse(&args(&[bad]), &[], &[]).expect("parsed");
            assert_eq!(one_id(&parsed), None, "{bad}");
        }

        let short = "a".repeat(63);
        let parsed = Args::parse(&args(&["--expect-sha", &short]), &[], &["--expect-sha"]).expect("parsed");
        assert_eq!(expected_sha(&parsed), None);
        assert!(json_only(&args(&["--json"])));
        assert!(!json_only(&args(&[])));
        assert!(!json_only(&args(&["--json", "x"])));
    }

    #[test]
    fn the_default_author_is_the_user_name() {
        let env = |name: &str, value: &str| Env::for_tests("/home/u").with_var(name, value);

        assert_eq!(default_author(&env("USER", " ada ")), "ada");
        assert_eq!(default_author(&env("USERNAME", "grace")), "grace");
        assert_eq!(default_author(&env("USER", &"a".repeat(100))), "a".repeat(80));
        assert_eq!(default_author(&env("USER", "a\u{202E}b")), UNKNOWN_AUTHOR);
        assert_eq!(default_author(&Env::for_tests("/home/u")), UNKNOWN_AUTHOR);
        assert_eq!(user_author(&Env::for_tests("/home/u")), None);
        assert_eq!(user_author(&env("USER", "\u{7}")), None);
    }
}
