//! The `code` command of the `harness-memory` executable, where the person at the terminal (and Harness
//! Desktop on their behalf) reads and changes the code index switch of this computer. There is no MCP
//! tool for it.
//!
//! - `harness-memory code status [--json]` prints `{"enabled": bool, "source": "file"|"default"|"env"}`:
//!   whether agents get the `code_*` tools, and what decided it (`code.toml`, nothing set so the default
//!   on, or the `HARNESS_MEMORY_CODE_INDEX` variable).
//! - `harness-memory code enable [--json]` and `code disable [--json]` write `code.toml` and print the
//!   status as `status` does (so `source` is `env` while the variable overrides the file).
//!
//! - `harness-memory code hint --project PATH --query TEXT [--json]` prints the one line [`hint`] gives for a
//!   name (`{"hint":"..."}` or `{"hint":null}` with `--json`, the line or nothing without). It exits 0 whatever
//!   the answer, and never builds, refreshes or creates the index.
//!
//! - `harness-memory code repo --project PATH [--json]` prints what the index holds for a project. It reads
//!   the index and asks `git` for the repository root ([`repo_state`], never builds and never creates the index
//!   (`code.sqlite3`) or the build status folder): `{"state":"indexed"|"none"|"tooLarge"|"failed"|
//!   "refused"|"off","files":N,"symbols":N,"indexedAt":unixSeconds|null}`. It exits 0 for every state.
//! - `harness-memory code build --project PATH [--json]` builds the index of a project that has none, as a
//!   `code_*` call would (the status file Harness Desktop reads included, one build however many calls of the
//!   index it takes), and prints `{"built":true,"files":N}`. It ends within
//!   [`MAX_RUN`](crate::code::prepare::MAX_RUN) (617 seconds, plus rare short waits for a lock).
//!   A project in any other state is left alone: `{"built":false,"reason":"<state>"}`, so opening a project
//!   again, or one that is too large, does no work. An error prints `{"built":false,"reason":"error",
//!   "error":"..."}`. It exits 0 whatever happened.
//!
//! Without `--json` the answer is one line of text. Failures exit 1 with the reason on stderr, usage
//! errors exit 2, as in [`crate::cli`].

use std::process::ExitCode;

use serde_json::json;

use crate::cli::{self, Args};
use crate::code::hint::hint;
use crate::code::prepare::{Built, Repo, State as RepoState, build, repo_state};
use crate::code::switch::{self, State};
use crate::support::Env;
use crate::support::private::data_folder;

/// The usage of this command, for the executable's usage line.
pub const USAGE: &str = "code status|enable|disable [--json] | code hint --project PATH --query TEXT [--json] | \
                         code repo|build --project PATH [--json]";

/// What `code` was asked to do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ask {
    Status,
    Set(bool),
}

/// `harness-memory code <args>`.
pub fn code(args: &[Option<&str>]) -> ExitCode {
    if let [Some("hint"), rest @ ..] = args {
        return hinted(rest);
    }

    if let [Some("repo"), rest @ ..] = args {
        return repo(rest);
    }

    if let [Some("build"), rest @ ..] = args {
        return built(rest);
    }

    let Some((ask, json)) = parse(args) else {
        return cli::usage(USAGE);
    };

    let Ok(env) = Env::from_process() else {
        eprintln!("harness-memory: the home folder is unknown");
        return ExitCode::FAILURE;
    };

    let Some(root) = data_folder(&env) else {
        eprintln!("harness-memory: the data folder is unknown");
        return ExitCode::FAILURE;
    };

    if let Ask::Set(on) = ask
        && let Err(message) = switch::set_enabled(&root, on)
    {
        eprintln!("harness-memory: {message}");
        return ExitCode::FAILURE;
    }

    println!("{}", shown(switch::state(&env, &root), json));
    ExitCode::SUCCESS
}

/// `code hint`: always exits 0, unless the arguments are wrong.
fn hinted(args: &[Option<&str>]) -> ExitCode {
    let Some((project, query, json)) = parse_hint(args) else {
        return cli::usage(USAGE);
    };
    let line = Env::from_process().ok().and_then(|env| hint(&env, project, query));

    if json {
        println!("{}", json!({ "hint": line }));
    } else if let Some(line) = line {
        println!("{line}");
    }

    ExitCode::SUCCESS
}

/// `code repo`: always exits 0, unless the arguments are wrong.
fn repo(args: &[Option<&str>]) -> ExitCode {
    let Some((project, json)) = parse_project(args) else {
        return cli::usage(USAGE);
    };
    let found = match Env::from_process() {
        Ok(env) => repo_state(&env, project),
        Err(_) => Repo {
            state: RepoState::Off,
            files: 0,
            symbols: 0,
            indexed_at: None,
        },
    };

    println!("{}", shown_repo(&found, json));

    ExitCode::SUCCESS
}

/// `code build`: always exits 0, unless the arguments are wrong.
fn built(args: &[Option<&str>]) -> ExitCode {
    let Some((project, json)) = parse_project(args) else {
        return cli::usage(USAGE);
    };
    let outcome = match Env::from_process() {
        Ok(env) => build(&env, project),
        Err(_) => Built::Error("the home folder is unknown".to_owned()),
    };

    println!("{}", shown_build(&outcome, json));

    ExitCode::SUCCESS
}

/// `--project PATH [--json]`; `None` when the project is missing or anything else is given.
fn parse_project<'a>(args: &[Option<&'a str>]) -> Option<(&'a str, bool)> {
    let parsed = Args::parse(args, &["--json"], &["--project"])?;

    if !parsed.positional.is_empty() {
        return None;
    }

    Some((parsed.value("--project")?, parsed.flag("--json")))
}

/// `--project PATH --query TEXT [--json]`; `None` when one is missing or anything else is given.
fn parse_hint<'a>(args: &[Option<&'a str>]) -> Option<(&'a str, &'a str, bool)> {
    let parsed = Args::parse(args, &["--json"], &["--project", "--query"])?;

    if !parsed.positional.is_empty() {
        return None;
    }

    Some((
        parsed.value("--project")?,
        parsed.value("--query")?,
        parsed.flag("--json"),
    ))
}

/// The answer for `state`: the JSON object, or one line of text.
fn shown(state: State, json: bool) -> String {
    if json {
        return json!({ "enabled": state.enabled, "source": state.source.as_str() }).to_string();
    }

    let word = if state.enabled { "on" } else { "off" };

    format!("The code index is {word} ({}).", state.source.as_str())
}

/// The answer of `code repo`: the JSON object, or one line of text.
fn shown_repo(repo: &Repo, json: bool) -> String {
    if json {
        return json!({
            "state": repo.state.as_str(),
            "files": repo.files,
            "symbols": repo.symbols,
            "indexedAt": repo.indexed_at,
        })
        .to_string();
    }

    match repo.state {
        RepoState::Indexed => format!(
            "The code index holds {} and {} of this project.",
            counted(repo.files, "file", "files"),
            counted(repo.symbols, "symbol", "symbols")
        ),
        RepoState::None => "The code index has nothing of this project yet.".to_owned(),
        RepoState::TooLarge => "This project is too large for the code index.".to_owned(),
        RepoState::Failed => "The code index failed on this project.".to_owned(),
        RepoState::Refused => "The code index does not read this folder.".to_owned(),
        RepoState::Off => "The code index is off.".to_owned(),
    }
}

/// `1 file` or `N files`.
fn counted(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The answer of `code build`: the JSON object, or one line of text.
fn shown_build(built: &Built, json: bool) -> String {
    if json {
        return match built {
            Built::Done(files) => json!({ "built": true, "files": files }),
            Built::Skipped(state) => json!({ "built": false, "reason": state.as_str() }),
            Built::Error(error) => json!({ "built": false, "reason": "error", "error": error }),
        }
        .to_string();
    }

    match built {
        Built::Done(files) => format!(
            "Built the code index of this project ({}).",
            counted(*files, "file", "files")
        ),
        Built::Skipped(state) => format!("The code index was not built ({}).", state.as_str()),
        Built::Error(error) => format!("The code index could not be built: {error}"),
    }
}

/// `status`, `enable` or `disable`, and whether `--json` was given; `None` for anything else.
fn parse(args: &[Option<&str>]) -> Option<(Ask, bool)> {
    let (command, rest) = args.split_first()?;
    let parsed = Args::parse(rest, &["--json"], &[])?;

    if !parsed.positional.is_empty() {
        return None;
    }

    let ask = match (*command)? {
        "status" => Ask::Status,
        "enable" => Ask::Set(true),
        "disable" => Ask::Set(false),
        _ => return None,
    };

    Some((ask, parsed.flag("--json")))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::code::switch::Source;

    #[test]
    fn the_commands_and_the_json_flag_are_read() {
        assert!(parse(&[Some("status"), Some("--json")]) == Some((Ask::Status, true)));
        assert!(parse(&[Some("enable")]) == Some((Ask::Set(true), false)));
        assert!(parse(&[Some("disable"), Some("--json")]) == Some((Ask::Set(false), true)));
        assert!(parse(&[]).is_none());
        assert!(parse(&[Some("rebuild")]).is_none());
        assert!(parse(&[Some("status"), Some("extra")]).is_none());
        assert!(parse(&[Some("status"), Some("--yes")]).is_none());
    }

    #[test]
    fn the_hint_needs_a_project_and_a_query() {
        let ok = parse_hint(&[
            Some("--project"),
            Some("/w"),
            Some("--query"),
            Some("name"),
            Some("--json"),
        ]);

        assert_eq!(ok, Some(("/w", "name", true)));
        assert_eq!(
            parse_hint(&[Some("--query=name"), Some("--project=/w")]),
            Some(("/w", "name", false))
        );
        assert_eq!(parse_hint(&[Some("--project"), Some("/w")]), None);
        assert_eq!(parse_hint(&[Some("--query"), Some("name")]), None);
        assert_eq!(
            parse_hint(&[Some("--project"), Some("/w"), Some("--query"), Some("n"), Some("x")]),
            None
        );
        assert_eq!(
            parse_hint(&[Some("--project"), Some("/w"), Some("--query"), Some("n"), Some("--yes")]),
            None
        );
    }

    #[test]
    fn the_status_is_one_json_object_or_one_line() {
        let state = State {
            enabled: true,
            source: Source::Default,
        };

        assert_eq!(shown(state, true), r#"{"enabled":true,"source":"default"}"#);
        assert_eq!(shown(state, false), "The code index is on (default).");
    }
}
