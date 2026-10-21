//! `harness-memory notes save` and `notes update`: the user's own notes, saved as `note_save` and
//! `note_update` save an agent's but past the save guards (see [`crate::cli_held`] for the
//! refusals).
//!
//! - `notes save --json [--project PATH] [--client NAME]` reads `{title, body, kind, tags, entities, scope}`
//!   (`scope` `repo`, the default, or `global`) on standard input and prints `{"id": N, "title":
//!   "...", "scope": "repo"}`.
//! - `notes update <id> --json [--project PATH] [--client NAME]` reads `{title, body, kind, tags}` (the fields to
//!   change) and prints `{"id": N, "title": "..."}`; a note that is not visible is `notFound`.
//!
//! Both are saved from `--project` (else the working folder), as an agent started there saves, and
//! written by client `--client` (the author the note shows, as `clientInfo.name` names an MCP
//! client; none without it). A `--client` the store would not keep as given is `invalid`.

use std::process::ExitCode;

use serde_json::{Map, Value, json};

use crate::cli::{self, Args};
use crate::cli_held::{Code, PATCH_FIELDS, Saver, arguments_of, client_of, read_draft, refused};
use crate::mcp::user;

/// The fields of a new note.
const NOTE_FIELDS: [(&str, &str); 6] = [
    ("title", "title"),
    ("body", "body"),
    ("kind", "kind"),
    ("tags", "tags"),
    ("entities", "entities"),
    ("scope", "scope"),
];

/// `notes save`.
pub(super) fn save(args: &[Option<&str>]) -> ExitCode {
    let Some((None, project, client)) = options(args) else {
        return cli::usage(super::USAGE);
    };

    let client = match client_of(client) {
        Ok(client) => client,
        Err(exit) => return exit,
    };

    let arguments = match draft(&NOTE_FIELDS) {
        Ok(arguments) => arguments,
        Err(exit) => return exit,
    };

    let Some(mut saver) = Saver::open(project, client) else {
        return ExitCode::FAILURE;
    };

    let saved = match user::save_note(&mut saver.server, Value::Object(arguments)) {
        Ok(saved) => saved,
        Err(message) => return saver.refused(Code::Refused, &message),
    };

    let title = title_of(&mut saver, saved.id);
    println!(
        "{}",
        json!({ "id": saved.id, "title": title, "scope": saved.scope.as_str() })
    );
    ExitCode::SUCCESS
}

/// `notes update`.
pub(super) fn update(args: &[Option<&str>]) -> ExitCode {
    let Some((Some(id), project, client)) = options(args) else {
        return cli::usage(super::USAGE);
    };

    let client = match client_of(client) {
        Ok(client) => client,
        Err(exit) => return exit,
    };

    let mut arguments = match draft(&PATCH_FIELDS) {
        Ok(arguments) => arguments,
        Err(exit) => return exit,
    };

    let Some(mut saver) = Saver::open(project, client) else {
        return ExitCode::FAILURE;
    };

    match user::note(&mut saver.server, id, None) {
        Ok(Some(_)) => {}
        Ok(None) => return saver.refused(Code::NotFound, &format!("note {id} not found")),
        Err(message) => return saver.refused(Code::Refused, &message),
    }

    arguments.insert("id".to_owned(), json!(id));

    let saved = match user::update_note(&mut saver.server, Value::Object(arguments)) {
        Ok(saved) => saved,
        Err(message) => return saver.refused(Code::Refused, &message),
    };

    let title = title_of(&mut saver, saved.id);
    println!("{}", json!({ "id": saved.id, "title": title }));
    ExitCode::SUCCESS
}

/// The note id (for `update`), `--project` and `--client`; `None` without `--json`, for more than
/// one positional argument, an id that is not a number, or a blank `--project`.
fn options<'a>(args: &[Option<&'a str>]) -> Option<(Option<i64>, Option<&'a str>, Option<&'a str>)> {
    let parsed = Args::parse(args, &["--json"], &["--project", "--client"])?;

    if !parsed.flag("--json") {
        return None;
    }

    let id = match parsed.positional[..] {
        [] => None,
        [id] => Some(id.parse().ok()?),
        _ => return None,
    };

    let project = parsed.value("--project");

    if project.is_some_and(|path| path.trim().is_empty()) {
        return None;
    }

    Some((id, project, parsed.value("--client")))
}

/// The tool arguments the draft on standard input gives; the refusal (`invalid`) when there is
/// none or it is not one.
fn draft(fields: &[(&str, &str)]) -> Result<Map<String, Value>, ExitCode> {
    match read_draft() {
        Ok(Some(draft)) => arguments_of(draft, fields).map_err(|message| refused(Code::Invalid, &message)),
        Ok(None) => Err(refused(
            Code::Invalid,
            "the draft goes on standard input, as a JSON object",
        )),
        Err(message) => Err(refused(Code::Invalid, &message)),
    }
}

/// The title note `id` has now (as the store cleaned it).
fn title_of(saver: &mut Saver, id: i64) -> Value {
    match user::note(&mut saver.server, id, None) {
        Ok(Some(note)) => json!(note.title),
        _ => Value::Null,
    }
}
