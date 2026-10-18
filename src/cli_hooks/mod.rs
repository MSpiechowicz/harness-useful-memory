//! The team hook commands of the `harness-memory` executable, written for Harness Desktop acting
//! on the user's behalf. There is no MCP tool for hooks, but these are plain commands: any process
//! running as the user, an agent with a shell tool included, can run `hooks put` and share a hook
//! with everyone on the team database. They read and write the team database only while
//! `hooks = "remote"` and one is set ([`Selection::hooks_place`]).
//!
//! - `harness-memory hooks config --json` prints `{"hooks": "remote"|"local", "trustTeamHooks":
//!   bool}` (exit 0 whenever the settings load), so Harness Desktop can probe for these commands.
//!   `trustTeamHooks` is `trust_team_hooks` while hooks are shared, else `false`
//!   ([`Selection::team_hooks_trusted`]).
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
//!   reads a hook file (at most 16384 bytes) from stdin and stores it (see [`crate::store::hooks`]
//!   for the body rules): without `--expect-sha` as a new hook, with it as the new version of the
//!   hook with that id while its `sha256` is still the one given. Prints the hook object. A
//!   project hook's `project` is the repository key of `--project`. The author is `--author`, else
//!   the user name (`USER`, `USERNAME`).
//! - `harness-memory hooks delete <id> [--expect-sha SHA256] --json` prints `{"deleted": bool}`
//!   (`false` when no hook has the id).
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
use crate::store::hooks::format::BodyProblem;
use crate::store::hooks::{is_hook_id, is_sha256};
use crate::store::{HookDelete, HookStore, ProjectKey, TeamHook};
use crate::support::Env;
use crate::support::time::rfc3339;

/// The usage of these commands, for the executable's usage line.
pub const USAGE: &str = "hooks config --json | hooks list --json [--project PATH] [--offset N] [--limit N] | hooks \
                         get <id> --json | hooks put --json [--project PATH] [--expect-sha SHA256] [--author NAME] \
                         < hook.json | hooks delete <id> [--expect-sha SHA256] --json";

/// `harness-memory hooks <args>`.
pub fn hooks(args: &[Option<&str>]) -> ExitCode {
    match args {
        [Some("config"), rest @ ..] => config(rest),
        [Some("list"), rest @ ..] => list(rest),
        [Some("get"), rest @ ..] => get(rest),
        [Some("put"), rest @ ..] => put::put(rest),
        [Some("delete"), rest @ ..] => delete(rest),
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    cli::usage(USAGE)
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

/// `hooks config`.
fn config(args: &[Option<&str>]) -> ExitCode {
    if !json_only(args) {
        return usage();
    }

    let Some((_, selection)) = cli::settings() else {
        return ExitCode::FAILURE;
    };

    println!(
        "{}",
        json!({
            "hooks": selection.hooks_place().as_str(),
            "trustTeamHooks": selection.team_hooks_trusted(),
        })
    );
    ExitCode::SUCCESS
}

/// `hooks list`.
fn list(args: &[Option<&str>]) -> ExitCode {
    let Some(parsed) = Args::parse(args, &["--json"], &["--project", "--offset", "--limit"]) else {
        return usage();
    };

    let (Some(page), Ok(project)) = (Page::of(&parsed), parsed.project()) else {
        return usage();
    };

    if !parsed.positional.is_empty() || !parsed.flag("--json") {
        return usage();
    }

    let Some(mut opened) = Opened::open() else {
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
            json!({ "total": 0, "nextOffset": null, "project": shown, "hooks": [] })
        );
        return ExitCode::SUCCESS;
    };

    let store = match store {
        Ok(store) => store,
        Err(message) => return opened.failed(&message),
    };

    // A `path:` key names a folder on this computer: it is never sent to the team database.
    let repo = identity.filter(is_shareable).map(|identity| identity.key);

    match store.list_hooks(repo.as_deref(), page.offset, page.limit) {
        Ok(listed) => {
            let hooks: Vec<Value> = listed.hooks.iter().map(row_json).collect();
            let reply = json!({
                "total": listed.total,
                "nextOffset": listed.next_offset,
                "project": shown,
                "hooks": hooks,
            });
            println!("{reply}");
            ExitCode::SUCCESS
        }
        Err(error) => opened.failed(&error.to_string()),
    }
}

/// `hooks get`.
fn get(args: &[Option<&str>]) -> ExitCode {
    let Some(parsed) = Args::parse(args, &["--json"], &[]) else {
        return usage();
    };

    let Some(id) = one_id(&parsed) else {
        return usage();
    };

    if !parsed.flag("--json") {
        return usage();
    }

    let Some(mut opened) = Opened::open() else {
        return ExitCode::FAILURE;
    };

    let store = match opened.shared() {
        Ok(store) => store,
        Err(exit) => return exit,
    };

    match store.get_hook(id) {
        Ok(Some(hook)) => {
            println!("{}", row_json(&hook));
            ExitCode::SUCCESS
        }
        Ok(None) => opened.refused(Code::NotFound, &format!("no team hook has the id {id}")),
        Err(error) => opened.failed(&error.to_string()),
    }
}

/// `hooks delete`.
fn delete(args: &[Option<&str>]) -> ExitCode {
    let Some(parsed) = Args::parse(args, &["--json"], &["--expect-sha"]) else {
        return usage();
    };

    let (Some(id), Some(expect)) = (one_id(&parsed), expected_sha(&parsed)) else {
        return usage();
    };

    if !parsed.flag("--json") {
        return usage();
    }

    let Some(mut opened) = Opened::open() else {
        return ExitCode::FAILURE;
    };

    let store = match opened.shared() {
        Ok(store) => store,
        Err(exit) => return exit,
    };

    match store.delete_hook(id, expect.as_deref()) {
        Ok(outcome @ (HookDelete::Deleted | HookDelete::Missing)) => {
            println!("{}", json!({ "deleted": outcome == HookDelete::Deleted }));
            ExitCode::SUCCESS
        }
        Ok(HookDelete::Changed) => opened.refused(Code::Conflict, &changed(id)),
        Err(error) => opened.failed(&error.to_string()),
    }
}

fn changed(id: &str) -> String {
    format!("team hook {id} changed since it was read (its sha256 is not the one given): read it again")
}

/// True for `--json` alone.
fn json_only(args: &[Option<&str>]) -> bool {
    Args::parse(args, &["--json"], &[]).is_some_and(|parsed| parsed.positional.is_empty() && parsed.flag("--json"))
}

/// The one positional argument, when it is a hook id.
fn one_id<'a>(parsed: &Args<'a>) -> Option<&'a str> {
    let [id] = parsed.positional[..] else {
        return None;
    };

    is_hook_id(id).then_some(id)
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

/// The hook object of `hook` (see the module docs).
fn row_json(hook: &TeamHook) -> Value {
    json!({
        "id": hook.id,
        "repo": hook.repo,
        "sha256": hook.sha256,
        "author": hook.author,
        "createdAt": rfc3339(hook.created_at),
        "updatedAt": rfc3339(hook.updated_at),
        "body": hook.body,
    })
}

/// The settings and, on first use, the team database.
struct Opened {
    env: Env,
    selection: Selection,
    secrets: Secrets,
    store: Option<Box<dyn HookStore>>,
}

impl Opened {
    /// The settings; `None`, with the reason printed, when they are wrong.
    fn open() -> Option<Self> {
        let (env, selection) = cli::settings()?;

        Some(Self {
            env,
            secrets: selection.backend.secrets(),
            selection,
            store: None,
        })
    }

    /// The team database, connected on first use; `None` when hooks are not shared.
    fn team(&mut self) -> Option<Result<&mut Box<dyn HookStore>, String>> {
        if self.selection.hooks_place() == Place::Local {
            return None;
        }

        if self.store.is_none() {
            match backend::open_hooks(&self.selection.backend)? {
                Ok(store) => self.store = Some(store),
                Err(message) => return Some(Err(message)),
            }
        }

        self.store.as_mut().map(Ok)
    }

    /// The team database, or the exit of a refusal (`local`) or a failure to connect.
    fn shared(&mut self) -> Result<&mut Box<dyn HookStore>, ExitCode> {
        if self.selection.hooks_place() == Place::Local {
            return Err(self.not_shared());
        }

        let secrets = self.secrets.clone();

        match self.team() {
            Some(Ok(store)) => Ok(store),
            Some(Err(message)) => Err(failure(&secrets, &message)),
            None => Err(failure(&secrets, "no team database is set")),
        }
    }

    fn not_shared(&self) -> ExitCode {
        self.refused(
            Code::Local,
            "team hooks are not shared: set hooks = \"remote\" in the config file, next to a team database_url",
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
    eprintln!("harness-memory: {}", secrets.scrub(message));
    ExitCode::FAILURE
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
}
