//! `harness-memory recall --hook [--project PATH] [--limit N]`: the notes (and saved scripts) a chat
//! message gets before the agent starts on it, found by the same hybrid ranked search `note_search`
//! runs, for Harness Desktop to put in the prompt.
//!
//! The message is on standard input (at most [`MESSAGE_MAX`] bytes are read). A leading
//! `<harness-hook-context>` or `<harness-memory-recall>` block is taken off first, and the search
//! keywords are the message's longer words without common English ones ([`text::query_of`]). The
//! project is `--project`, else `HARNESS_HOOK_PROJECT`, else the working folder's: its repository's
//! notes and the global ones, never the notes hidden on this computer. `--limit` is 1 to
//! [`LIMIT_MAX`] notes (3 when not given).
//!
//! It prints one `<harness-memory-recall>` block of at most [`text::RECALL_BUDGET`] bytes ([`text`]
//! describes it), or nothing (exit 0) when the message says too little to search for, the local
//! database does not exist yet and any notes or scripts are read from it (a message never creates
//! or migrates the local store, with a team database too), nothing matches, or the work does not
//! finish within [`DEADLINE`].
//! A failure (the settings are wrong, the database cannot be opened or read) exits 1 with one line on
//! stderr and prints nothing, never half a block. Usage errors exit 2.
//!
//! The notes of the block count as shown, as when an agent's `note_search` shows them (see
//! [`crate::mcp::Server::after_reply`]): written after the block is printed and flushed, in a
//! single attempt of at most [`COUNT_BUDGET`], and dropped silently when it fails. Memory settings
//! only ever limit saving, never reading, so they do not change what this prints.

mod text;

use std::io::{Read, Write};
use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use serde_json::{Value, json};

use crate::backend::{Backend, Selection};
use crate::cli::{self, Args};
use crate::mcp::Server;
use crate::open::open_server;

/// The usage of this command, for the executable's usage line.
pub const USAGE: &str = "recall --hook [--project PATH] [--limit N]";

/// How long the whole command may take.
pub const DEADLINE: Duration = Duration::from_millis(2500);

/// How long counting the notes as shown may take once the block is out.
pub const COUNT_BUDGET: Duration = Duration::from_millis(300);

/// Most bytes of the message that are read.
pub const MESSAGE_MAX: usize = 64 * 1024;

/// Most notes `--limit` may ask for, and its default.
pub const LIMIT_MAX: usize = 5;
const LIMIT_DEFAULT: usize = 3;

/// What the work found.
enum Outcome {
    Block(String),
    Nothing,
    Failed(String),
}

/// `harness-memory recall <args>`.
pub fn recall(args: &[Option<&str>]) -> ExitCode {
    let Some(parsed) = Args::parse(args, &["--hook"], &["--project", "--limit"]) else {
        return cli::usage(USAGE);
    };

    if !parsed.flag("--hook") || !parsed.positional.is_empty() {
        return cli::usage(USAGE);
    }

    let Some(limit) = limit_of(parsed.value("--limit")) else {
        return cli::usage(USAGE);
    };

    let project = parsed
        .value("--project")
        .map(str::to_owned)
        .or_else(|| std::env::var("HARNESS_HOOK_PROJECT").ok())
        .map(|project| project.trim().to_owned())
        .filter(|project| !project.is_empty());

    let (answer, answered) = mpsc::channel();
    let (counted, wait_counted) = mpsc::channel();

    // On a thread of its own: a parent that keeps standard input open and silent, or a database that
    // hangs, costs the deadline, not the message it decorates. A thread still running when the
    // process ends is taken along.
    let spawned = std::thread::Builder::new().name("recall".to_owned()).spawn(move || {
        run(Box::new(std::io::stdin()), project, limit, &answer, &counted);
    });

    if spawned.is_err() {
        return fail("cannot start the search");
    }

    match answered.recv_timeout(DEADLINE) {
        Ok(Outcome::Block(block)) => {
            let mut out = std::io::stdout().lock();

            if writeln!(out, "{block}").and_then(|()| out.flush()).is_err() {
                return ExitCode::FAILURE;
            }

            // The counter write ends well within the time a hook gets; a longer one is given up on.
            let _ = wait_counted.recv_timeout(COUNT_BUDGET);
            ExitCode::SUCCESS
        }
        Ok(Outcome::Nothing) | Err(RecvTimeoutError::Timeout) => ExitCode::SUCCESS,
        Ok(Outcome::Failed(message)) => fail(&message),
        Err(RecvTimeoutError::Disconnected) => fail("the search stopped unexpectedly"),
    }
}

/// `--limit`'s value: 1 to [`LIMIT_MAX`], [`LIMIT_DEFAULT`] when not given; `None` for any other.
fn limit_of(value: Option<&str>) -> Option<usize> {
    let Some(value) = value else {
        return Some(LIMIT_DEFAULT);
    };

    value.parse().ok().filter(|limit| (1..=LIMIT_MAX).contains(limit))
}

fn fail(message: &str) -> ExitCode {
    eprintln!("harness-memory: recall failed: {message}");
    ExitCode::FAILURE
}

/// Reads the message and searches, sends the outcome, then counts the notes of a block as shown;
/// `counted` hears when that is done.
fn run(
    input: Box<dyn Read + Send>,
    project: Option<String>,
    limit: usize,
    answer: &mpsc::Sender<Outcome>,
    counted: &mpsc::Sender<()>,
) {
    match find(input, project, limit) {
        Ok(Some((block, mut server))) => {
            let _ = answer.send(Outcome::Block(block));
            server.after_reply();
        }
        Ok(None) => {
            let _ = answer.send(Outcome::Nothing);
        }
        Err(message) => {
            let _ = answer.send(Outcome::Failed(message));
        }
    }

    let _ = counted.send(());
}

/// The block for the message in `input` and the server whose shown counters are still to be
/// written; `None` when there is nothing to recall.
fn find(
    mut input: Box<dyn Read + Send>,
    project: Option<String>,
    limit: usize,
) -> Result<Option<(String, Server)>, String> {
    let Some(message) = read_message(&mut input) else {
        return Ok(None);
    };

    let Some(query) = text::query_of(text::bare_message(&message)) else {
        return Ok(None);
    };

    let Some((env, mut selection)) = cli::settings() else {
        return Err("the settings could not be read".to_owned());
    };

    // A read never creates (or migrates) the local database, whatever the routing: when notes or
    // scripts are read from this computer's file and it does not exist yet, there is nothing to
    // recall (the team database is not searched alone, so a message never opens the local store).
    let no_local_file = crate::database_path(&env).is_none_or(|path| !path.exists());

    if reads_local(&selection) && no_local_file {
        return Ok(None);
    }

    // The same holds while the team database is unreachable: without a local file the `remote`
    // scopes must not fall back to this computer (that would create the file), so the read fails
    // and the desktop says it did not look.
    if no_local_file {
        selection.offline_fallback = false;
    }

    // A hook never waits for another process to add the schema extensions.
    crate::store::patience::skip_schema_waits();

    let Some(mut server) = open_server(&env, &selection, std::env::current_dir().ok()) else {
        return Err("the memory database cannot be opened".to_owned());
    };

    let reply = search(&mut server, &query, project.as_deref(), limit)
        .map_err(|message| selection.backend.secrets().scrub(&message))?;

    Ok(text::block_of(&reply).map(|block| (block, server)))
}

/// True when a search under `selection` reads this computer's file: it is the only store, or any
/// scope of the notes or the scripts is read from it (`local` or `remote+local`).
fn reads_local(selection: &Selection) -> bool {
    let routing = selection.routing;

    selection.backend == Backend::Local
        || routing.is_all_local()
        || routing.repository.reads_local()
        || routing.global.reads_local()
        || selection.scripts_place().reads_local()
}

/// The text of the `note_search` reply for `query`: the notes of `project`'s repository (the working
/// folder's without one) and the global ones, and the saved scripts that match.
fn search(server: &mut Server, query: &str, project: Option<&str>, limit: usize) -> Result<String, String> {
    let mut arguments = json!({ "query": query, "limit": limit });

    if let Some(project) = project {
        arguments["project_path"] = json!(project);
    }

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "note_search", "arguments": arguments },
    });

    let reply = server.handle(request).ok_or("the search was not answered")?;
    let result = reply.get("result").ok_or("the search was refused")?;
    let text = result
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    if result.get("isError").and_then(Value::as_bool).unwrap_or(false) {
        return Err(text.lines().next().unwrap_or("the search failed").to_owned());
    }

    Ok(text)
}

/// The first [`MESSAGE_MAX`] bytes of `input` as text (a cut character is dropped); `None` when it
/// cannot be read or is blank.
fn read_message(input: &mut dyn Read) -> Option<String> {
    let mut bytes = Vec::new();
    input.take(MESSAGE_MAX as u64).read_to_end(&mut bytes).ok()?;

    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            let valid = error.utf8_error().valid_up_to();
            String::from_utf8_lossy(&error.as_bytes()[..valid]).into_owned()
        }
    };

    (!text.trim().is_empty()).then_some(text)
}
