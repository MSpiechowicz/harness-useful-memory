//! `harness-memory notes save` and `notes update`: the user's own notes, saved as `note_save` and
//! `note_update` save an agent's but past the save guards (see [`crate::cli_held`] for the
//! refusals).
//!
//! - `notes save --json [--project PATH] [--client NAME] [--place remote|local]` reads `{title, body,
//!   kind, tags, entities, scope}` (`scope` `repo`, the default, or `global`) on standard input and
//!   prints `{"id": N, "title": "...", "scope": "repo"}`.
//! - `--place` picks where the note goes when the notes of its scope are set to `remote+local`
//!   (without it a note goes where `remote_local_saves` says, the team database by default, except
//!   that a title a note on this computer has already is updated there, online or not, and listed
//!   as waiting for nothing). `local` saves to this computer whatever that setting says, and stays
//!   there: the note is not listed as waiting for the team database, also while it is unreachable,
//!   and one an agent saved there while it was down is taken off the list. `remote` saves to the
//!   team database, as a save to a `remote` scope does (the same limits, the hidden title check and
//!   the same-title conflict), and fails, saving nothing, when it cannot be reached: the note is not
//!   kept on this computer instead, nor listed as waiting. For any other routing `--place` is
//!   `invalid`. The answer then adds `"place": "remote"|"local"`.
//! - A note the user saves is never refused for its language. When the English notes setting is on
//!   and the note is clearly not English, the line `harness-memory: this note is not in English
//!   ...` goes to stderr and the JSON gets `"warning": "not-english"`.
//! - `notes update <id> --json [--project PATH] [--client NAME] [--expect-revision N]` reads `{title, body, kind,
//!   tags}` (the fields to change) and prints `{"id": N, "title": "..."}`; a note that is not visible
//!   is `notFound`. With `--expect-revision` the note is changed only if it is still at revision `N`
//!   (what the editor read), checked in the same step as the write: any other revision is
//!   `{"error": "changed", "message": "...", "revision": M}` (the revision it is at now), exit 1,
//!   and nothing is changed. While the team database is unreachable (the offline fallback) a change
//!   to a team note is kept on this computer and applied when it is back: the answer is `{"id": N,
//!   "title": ..., "queued": true, "message": "saved on this computer, applied when the remote
//!   database is back"}` (`title` is the new title when the change gives one, else `null`), and
//!   `N` is the note's id as given. The revision then becomes the base the change is judged
//!   against when it is applied (see [`crate::store::pending_updates`]).
//!
//! Both are saved from `--project` (else the working folder), as an agent started there saves, and
//! written by client `--client` (the author the note shows, as `clientInfo.name` names an MCP
//! client; none without it). A `--client` the store would not keep as given is `invalid`.

use std::process::ExitCode;

use serde_json::{Map, Value, json};

use crate::cli::{self, Args};
use crate::cli_held::{
    Code, PATCH_FIELDS, Saver, arguments_of, check_place, client_of, place_of, read_draft, refused, refused_with,
};
use crate::mcp::user::{self, UpdateError};
use crate::store::pending::mark_not_listed;
use crate::store::pending_updates::QUEUED_MESSAGE;

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
    let Some(Options {
        id: None,
        project,
        client,
        place,
        ..
    }) = options(args, false)
    else {
        return cli::usage(super::USAGE);
    };

    let client = match client_of(client) {
        Ok(client) => client,
        Err(exit) => return exit,
    };

    let place = match place_of(place) {
        Ok(place) => place,
        Err(exit) => return exit,
    };

    let arguments = match draft(&NOTE_FIELDS) {
        Ok(arguments) => arguments,
        Err(exit) => return exit,
    };

    let Some((mut saver, routed)) = Saver::open_routed(project, client) else {
        return ExitCode::FAILURE;
    };

    if place.is_some()
        && let Some((routing, setting)) = routed.notes_of(&arguments)
        && let Err(exit) = check_place(routing, setting)
    {
        return exit;
    }

    // A place a person picked wins over `remote_local_saves`, either way.
    let arguments = Value::Object(arguments);
    let saved = match place {
        Some(place) => user::save_note_to(&mut saver.server, arguments, place),
        None => user::save_note(&mut saver.server, arguments),
    };

    let saved = match saved {
        Ok(saved) => saved,
        Err(message) => return saver.refused(Code::Refused, &message),
    };

    let title = title_of(&mut saver, saved.id);
    let mut shown = json!({ "id": saved.id, "title": title, "scope": saved.scope.as_str() });
    warn_not_english(&mut saver, saved.id, &mut shown);

    if let Some(place) = place {
        shown["place"] = json!(place.as_str());
    }

    if saved.not_listed {
        mark_not_listed(&mut shown);
    }

    println!("{shown}");
    ExitCode::SUCCESS
}

/// `notes update`.
pub(super) fn update(args: &[Option<&str>]) -> ExitCode {
    let Some(Options {
        id: Some(id),
        project,
        client,
        expect,
        ..
    }) = options(args, true)
    else {
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

    // Decides now whether the team database is reachable, so a team note that cannot be read is told
    // from one that is not there.
    user::settle(&mut saver.server);

    match user::note(&mut saver.server, id, None) {
        Ok(Some(_)) => {}
        Ok(None) if user::unverifiable(&saver.server, id) => {}
        Ok(None) => return saver.refused(Code::NotFound, &format!("note {id} not found")),
        Err(message) => return saver.refused(Code::Refused, &message),
    }

    arguments.insert("id".to_owned(), json!(id));
    let new_title = arguments.get("title").and_then(Value::as_str).map(str::to_owned);

    let saved = match user::update_note_expecting(&mut saver.server, Value::Object(arguments), expect) {
        Ok(saved) => saved,
        Err(UpdateError::Stale { revision, message }) => {
            return refused_with(
                Code::Changed,
                &saver.secrets.scrub(&message),
                ("revision", json!(revision)),
            );
        }
        Err(error) => return saver.refused(Code::Refused, &error.message()),
    };

    // The change waits for the team database: the note cannot be read to tell its title.
    if saved.queued {
        let shown = json!({ "id": saved.id, "title": new_title, "queued": true, "message": QUEUED_MESSAGE });

        println!("{shown}");
        return ExitCode::SUCCESS;
    }

    let title = title_of(&mut saver, saved.id);
    let mut shown = json!({ "id": saved.id, "title": title });
    warn_not_english(&mut saver, saved.id, &mut shown);

    println!("{shown}");
    ExitCode::SUCCESS
}

/// What `notes save` and `notes update` are given.
struct Options<'a> {
    /// The note id (for `update`).
    id: Option<i64>,
    project: Option<&'a str>,
    client: Option<&'a str>,
    /// The revision the note must still be at (`--expect-revision`, for `update`).
    expect: Option<i64>,
    /// Where a new note goes (`--place`, for `save`), as given.
    place: Option<&'a str>,
}

/// The note id (for `update`), `--project`, `--client` and, when `expecting`, `--expect-revision`
/// (else `--place`); `None` without `--json`, for more than one positional argument, an id that is
/// not a number, a revision that is not a positive number, or a blank `--project`.
fn options<'a>(args: &[Option<&'a str>], expecting: bool) -> Option<Options<'a>> {
    let valued: &[&str] = if expecting {
        &["--project", "--client", "--expect-revision"]
    } else {
        &["--project", "--client", "--place"]
    };
    let parsed = Args::parse(args, &["--json"], valued)?;

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

    let expect = match parsed.value("--expect-revision") {
        Some(raw) => Some(raw.parse::<i64>().ok().filter(|revision| *revision > 0)?),
        None => None,
    };

    Some(Options {
        id,
        project,
        client: parsed.value("--client"),
        expect,
        place: parsed.value("--place"),
    })
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

/// Tells the user, on stderr and as `"warning": "not-english"` in `shown`, when note `id` is not
/// in English and the store keeps English notes. The note is saved either way.
fn warn_not_english(saver: &mut Saver, id: i64, shown: &mut Value) {
    if user::not_english(&mut saver.server, id, None) {
        eprintln!("harness-memory: {}", user::NOT_ENGLISH_WARNING);
        shown["warning"] = json!("not-english");
    }
}

/// The title note `id` has now (as the store cleaned it).
fn title_of(saver: &mut Saver, id: i64) -> Value {
    match user::note(&mut saver.server, id, None) {
        Ok(Some(note)) => json!(note.title),
        _ => Value::Null,
    }
}
