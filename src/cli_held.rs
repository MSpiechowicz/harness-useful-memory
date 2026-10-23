//! The user-only commands of the save guards (see [`crate::mcp`]): what agents asked to save while
//! the user reviews saves first ([`crate::held`]), and the user's own saves. None is reachable
//! through MCP, and none is stopped by a guard: the guards stop agents' tool calls, the user
//! decides here.
//!
//! - `harness-memory held list --json [--chat KEY]` prints `{"held": [...]}`: the held files
//!   (camelCase, as written), of chat `KEY` only when given, newest first.
//! - `harness-memory held save <heldId> --json [--expect REVISION]` saves held item `heldId` and
//!   removes it, printing `{"saved": {"kind": "note", "id": N}}` or
//!   `{"saved": {"kind": "script", "name": "..."}}`; a note that is clearly not English, with the
//!   English notes setting on, is saved all the same and the object gets `"warning":
//!   "not-english"` (and a line on stderr). It claims the item first (see
//!   [`crate::held`]), so two saves of one item save it once, and puts it back when the save
//!   fails. With `--expect` it saves only the item of that `revision` (what the user saw): a
//!   held item replaced since is error `changed`.
//!   Standard input may hold an edited draft (a JSON object, read unless it is a terminal):
//!   `{title, body, kind, tags, entities, scope}` for a `note_save`, `{title, body, kind, tags}`
//!   for a `note_update` (applied to the note as it is now; a note deleted since is error `gone`;
//!   one whose title, body, kind or tags differ from the held `current` is error `changed`, and
//!   the held item is rewritten under its id with the note as it is now as `current` and a new
//!   `revision`, so the user reviews it again against that note),
//!   `{name, description, language, body, timeoutSecs, scope}` for a `script_save`; the fields
//!   given replace the agent's; without a draft the held arguments are saved as they are (a held
//!   `append` is appended to the note's body as it is now). A held script is saved untrusted: it
//!   needs the user's approval before it runs, also while every team script is trusted. The note or script is written as by the agent's client, from the folder the
//!   agent's call was about.
//! - `harness-memory held skip <heldId> --json [--expect REVISION]` removes it, printing
//!   `{"skipped": "<heldId>"}`. It claims the item as a save does; with `--expect` it skips only
//!   the item of that `revision`: a held item replaced since is error `changed` and keeps waiting.
//! - `notes save`, `notes update` and `scripts save` ([`crate::cli_notes`], [`crate::cli_trust`])
//!   save the user's own notes and scripts with these helpers.
//!
//! A refusal prints `{"error": CODE, "message": "..."}` on stdout and the message as the last line
//! of stderr, exit 1: `notFound` (no such held item or note, or another save took it), `gone`
//! (the note a held update changes was deleted), `changed` (the held item is not of the expected
//! revision), `invalid` (standard input is not a draft) or `refused` (the save failed, the message
//! says why). Other failures and usage errors as in [`crate::cli`].

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::{Map, Value, json};

use crate::Server;
use crate::backend::Secrets;
use crate::cli::{self, Args};
use crate::held::{Claim, HELD_FILE_MAX, HeldItem, HeldStore, HeldTool};
use crate::mcp::user;
use crate::open::open_server;
use crate::store::clean;
use crate::support::Env;
use crate::support::paths::validate_project_dir;
use crate::support::private::is_lowercase_uuid;
use stale::{note_named, stale_current};

/// The usage of these commands, for the executable's usage line.
pub const USAGE: &str = "held list --json [--chat KEY] | held save <heldId> --json [--expect REVISION] [< draft.json] | held skip <heldId> \
     --json [--expect REVISION]";

/// The fields of an edited `note_save` draft, each with the tool argument it replaces.
const NOTE_FIELDS: [(&str, &str); 6] = [
    ("title", "title"),
    ("body", "body"),
    ("kind", "kind"),
    ("tags", "tags"),
    ("entities", "entities"),
    ("scope", "scope"),
];

/// The fields of an edited `note_update` draft.
pub(crate) const PATCH_FIELDS: [(&str, &str); 4] =
    [("title", "title"), ("body", "body"), ("kind", "kind"), ("tags", "tags")];

/// The fields of an edited `script_save` draft.
const SCRIPT_FIELDS: [(&str, &str); 6] = [
    ("name", "name"),
    ("description", "description"),
    ("language", "language"),
    ("body", "body"),
    ("timeoutSecs", "timeout_secs"),
    ("scope", "scope"),
];

/// `harness-memory held <args>`.
pub fn held(args: &[Option<&str>]) -> ExitCode {
    match args {
        [Some("list"), rest @ ..] => list(rest),
        [Some("save"), rest @ ..] => save(rest),
        [Some("skip"), rest @ ..] => skip(rest),
        _ => cli::usage(USAGE),
    }
}

/// Why a command refused, as its `error` code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Code {
    NotFound,
    Gone,
    Changed,
    Invalid,
    Refused,
}

impl Code {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "notFound",
            Self::Gone => "gone",
            Self::Changed => "changed",
            Self::Invalid => "invalid",
            Self::Refused => "refused",
        }
    }
}

/// The server a user-only save goes through, with the database credentials to keep out of
/// messages.
pub(crate) struct Saver {
    pub server: Server,
    pub secrets: Secrets,
}

impl Saver {
    /// The server over the configured stores, started in `folder` (else the working folder) and
    /// writing as `client`; `None`, with the reason printed, when the settings, the folder or the
    /// store are wrong.
    pub(crate) fn open(folder: Option<&str>, client: Option<String>) -> Option<Self> {
        let (env, selection) = cli::settings()?;

        let cwd = match folder {
            Some(raw) => match validate_project_dir(&env, std::path::Path::new(raw)) {
                Ok(dir) => Some(dir),
                Err(_) => {
                    eprintln!("harness-memory: --project must be an existing directory that is not a filesystem root");
                    return None;
                }
            },
            None => std::env::current_dir().ok(),
        };

        Self::open_in(&env, &selection, cwd, client)
    }

    fn open_in(
        env: &Env,
        selection: &crate::backend::Selection,
        cwd: Option<PathBuf>,
        client: Option<String>,
    ) -> Option<Self> {
        let server = open_server(env, selection, cwd)?.with_client(client);

        Some(Self {
            server,
            secrets: selection.backend.secrets(),
        })
    }

    /// Prints the refusal `message` (credentials masked) with `code`; exit 1.
    pub(crate) fn refused(&self, code: Code, message: &str) -> ExitCode {
        refused(code, &self.secrets.scrub(message))
    }
}

/// The client `--client` names, as `initialize` names it over MCP (the author the note or script
/// shows); the `invalid` refusal for a name the store would not keep as given: blank, not one
/// line, with an invisible character or a secret, or longer than 64 characters.
pub(crate) fn client_of(given: Option<&str>) -> Result<Option<String>, ExitCode> {
    let Some(name) = given else {
        return Ok(None);
    };

    if clean::source(Some(name)).as_deref() != Some(name) {
        return Err(refused(
            Code::Invalid,
            "--client must be a name of 1 to 64 visible characters on one line",
        ));
    }

    Ok(Some(name.to_owned()))
}

/// Prints `{"error": code, "message"}` and the message on stderr; exit 1.
pub(crate) fn refused(code: Code, message: &str) -> ExitCode {
    println!("{}", json!({ "error": code.as_str(), "message": message }));
    eprintln!("harness-memory: {message}");
    ExitCode::FAILURE
}

/// The draft on standard input: a JSON object, or `None` when it is a terminal or blank.
pub(crate) fn read_draft() -> Result<Option<Map<String, Value>>, String> {
    let stdin = std::io::stdin();

    if stdin.is_terminal() {
        return Ok(None);
    }

    let mut bytes = Vec::new();
    let limit = u64::try_from(HELD_FILE_MAX + 1).unwrap_or(u64::MAX);
    stdin
        .lock()
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read standard input ({})", error.kind()))?;

    if bytes.len() > HELD_FILE_MAX {
        return Err(format!("the draft is larger than {HELD_FILE_MAX} bytes"));
    }

    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }

    match serde_json::from_slice(&bytes) {
        Ok(Value::Object(draft)) => Ok(Some(draft)),
        _ => Err("the draft on standard input is not a JSON object".to_owned()),
    }
}

/// The tool arguments `draft` gives, each field renamed as `fields` say; fails for a field not
/// among them.
pub(crate) fn arguments_of(draft: Map<String, Value>, fields: &[(&str, &str)]) -> Result<Map<String, Value>, String> {
    let mut arguments = Map::new();

    for (field, value) in draft {
        let Some((_, argument)) = fields.iter().find(|(name, _)| *name == field) else {
            let names: Vec<&str> = fields.iter().map(|(name, _)| *name).collect();
            return Err(format!(
                "the draft may only have {}, not {}",
                names.join(", "),
                field.chars().take(64).collect::<String>()
            ));
        };

        arguments.insert((*argument).to_owned(), value);
    }

    Ok(arguments)
}

/// `held list`.
fn list(args: &[Option<&str>]) -> ExitCode {
    let Some(parsed) = Args::parse(args, &["--json"], &["--chat"]) else {
        return cli::usage(USAGE);
    };

    let chat = parsed.value("--chat");

    if !parsed.flag("--json") || !parsed.positional.is_empty() || chat.is_some_and(|chat| !is_lowercase_uuid(chat)) {
        return cli::usage(USAGE);
    }

    let Some(store) = held_store() else {
        return ExitCode::FAILURE;
    };

    store.sweep();

    match store.list(chat) {
        Ok(items) => {
            println!("{}", json!({ "held": items }));
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("harness-memory: {message}");
            ExitCode::FAILURE
        }
    }
}

/// `held skip`.
fn skip(args: &[Option<&str>]) -> ExitCode {
    let Some(parsed) = Args::parse(args, &["--json"], &["--expect"]) else {
        return cli::usage(USAGE);
    };

    let Some(held_id) = held_id_of(&parsed) else {
        return cli::usage(USAGE);
    };

    let Some(store) = held_store() else {
        return ExitCode::FAILURE;
    };

    let claim = match claimed(&store, held_id, parsed.value("--expect")) {
        Ok(claim) => claim,
        Err(exit) => return exit,
    };

    match claim.finish() {
        Ok(()) => {
            println!("{}", json!({ "skipped": held_id }));
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("harness-memory: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Claims held item `held_id` in `store` (of revision `expect` when given): the refusal as the
/// exit code when it is not waiting (`notFound`), is of another revision (`changed`, put back),
/// or the claim fails.
fn claimed(store: &HeldStore, held_id: &str, expect: Option<&str>) -> Result<Claim, ExitCode> {
    let claim = match store.claim(held_id) {
        Ok(Some(claim)) => claim,
        Ok(None) => return Err(refused(Code::NotFound, &format!("held item {held_id} not found"))),
        Err(message) => {
            eprintln!("harness-memory: {message}");
            return Err(ExitCode::FAILURE);
        }
    };

    if expect.is_some_and(|expect| expect != claim.item.revision) {
        let restored = claim.restore();
        let message = format!("held item {held_id} changed since it was shown");
        return Err(refused(Code::Changed, &with_restore(&message, restored)));
    }

    Ok(claim)
}

/// `held save`.
fn save(args: &[Option<&str>]) -> ExitCode {
    let Some(parsed) = Args::parse(args, &["--json"], &["--expect"]) else {
        return cli::usage(USAGE);
    };

    let Some(held_id) = held_id_of(&parsed) else {
        return cli::usage(USAGE);
    };

    let expect = parsed.value("--expect");

    let edited = match read_draft() {
        Ok(edited) => edited,
        Err(message) => return refused(Code::Invalid, &message),
    };

    let Some((env, selection)) = cli::settings() else {
        return ExitCode::FAILURE;
    };

    let Some(store) = held_store() else {
        return ExitCode::FAILURE;
    };

    let mut claim = match claimed(&store, held_id, expect) {
        Ok(claim) => claim,
        Err(exit) => return exit,
    };

    // Saved from the folder the agent's call was about, as if from there.
    let cwd = claim
        .item
        .project_path
        .as_deref()
        .and_then(|raw| validate_project_dir(&env, std::path::Path::new(raw)).ok());

    let Some(mut saver) = Saver::open_in(&env, &selection, cwd, Some(claim.item.client.clone())) else {
        if let Err(message) = claim.restore() {
            eprintln!("harness-memory: {message}");
        }
        return ExitCode::FAILURE;
    };

    // An update shown against a note that changed since: the item is shown again against the note
    // as it is now.
    match stale_current(&mut saver, &claim.item) {
        Ok(None) => {}
        Ok(Some(current)) => {
            let mut message = format!("{} changed since the agent's update was held", note_named(&claim.item));

            if let Err(why) = claim.refresh(current) {
                message = format!("{message} ({why})");
            }

            let restored = claim.restore();
            return saver.refused(Code::Changed, &with_restore(&message, restored));
        }
        Err((code, message)) => {
            let restored = claim.restore();
            return saver.refused(code, &with_restore(&message, restored));
        }
    }

    let saved = match saved_item(&mut saver, &claim.item, edited) {
        Ok(saved) => saved,
        Err((code, message)) => {
            let restored = claim.restore();
            return saver.refused(code, &with_restore(&message, restored));
        }
    };

    if let Err(message) = claim.finish() {
        eprintln!("harness-memory: saved, but the held item stays: {message}");
    }

    println!("{}", json!({ "saved": saved }));
    ExitCode::SUCCESS
}

/// `message`, telling when the held item could not be put back after a refused save.
fn with_restore(message: &str, restored: Result<(), String>) -> String {
    match restored {
        Ok(()) => message.to_owned(),
        Err(why) => format!("{message} ({why})"),
    }
}

/// Saves `item`, with the fields of `edited` replacing the agent's: what was saved.
fn saved_item(saver: &mut Saver, item: &HeldItem, edited: Option<Map<String, Value>>) -> Result<Value, (Code, String)> {
    let mut arguments = item.arguments.as_object().cloned().unwrap_or_default();

    // The server runs in that folder already, and a `project_path` would mark the save as reaching
    // the repository from elsewhere.
    arguments.remove("project_path");

    let fields: &[(&str, &str)] = match item.tool {
        HeldTool::NoteSave => &NOTE_FIELDS,
        HeldTool::NoteUpdate => &PATCH_FIELDS,
        HeldTool::ScriptSave => &SCRIPT_FIELDS,
    };

    if let Some(edited) = edited {
        let edited = arguments_of(edited, fields).map_err(|message| (Code::Invalid, message))?;

        // An edited body replaces the note's whole body, not what the agent appended.
        if item.tool == HeldTool::NoteUpdate && edited.contains_key("body") {
            arguments.remove("append");
        }

        arguments.extend(edited);
    }

    let server = &mut saver.server;
    let refused = |message: String| (Code::Refused, message);

    match item.tool {
        HeldTool::NoteSave => {
            let saved = user::save_note(server, Value::Object(arguments)).map_err(refused)?;
            Ok(note_saved(server, saved.id))
        }
        HeldTool::NoteUpdate => {
            let id = arguments.get("id").and_then(Value::as_i64).unwrap_or_default();
            let scope = arguments.get("scope").and_then(Value::as_str).map(str::to_owned);

            if user::note(server, id, scope.as_deref()).map_err(refused)?.is_none() {
                return Err((Code::Gone, format!("note {id} was deleted since the agent changed it")));
            }

            let saved = user::update_note(server, Value::Object(arguments)).map_err(refused)?;
            Ok(note_saved(server, saved.id))
        }
        HeldTool::ScriptSave => {
            let saved = user::save_held_script(server, Value::Object(arguments)).map_err(refused)?;
            Ok(json!({ "kind": "script", "name": saved["script"]["name"] }))
        }
    }
}

/// `{"kind": "note", "id": N}` for the note the user saved, with `"warning": "not-english"` (and
/// the same line on stderr) when it is clearly not English and the store keeps English notes. The
/// user's save is never refused for it.
fn note_saved(server: &mut Server, id: i64) -> Value {
    let mut saved = json!({ "kind": "note", "id": id });

    if user::not_english(server, id, None) {
        eprintln!("harness-memory: {}", user::NOT_ENGLISH_WARNING);
        saved["warning"] = json!("not-english");
    }

    saved
}

/// The held id of `held save|skip <heldId> --json`, as parsed; `None` for anything else.
fn held_id_of<'a>(parsed: &Args<'a>) -> Option<&'a str> {
    let [held_id] = parsed.positional[..] else {
        return None;
    };

    (parsed.flag("--json") && is_lowercase_uuid(held_id)).then_some(held_id)
}

/// The held items of the data folder; `None`, with the reason printed, when it is unknown.
fn held_store() -> Option<HeldStore> {
    let Ok(env) = Env::from_process() else {
        eprintln!("harness-memory: the home folder is unknown");
        return None;
    };

    let store = HeldStore::open(&env);

    if store.is_none() {
        eprintln!("harness-memory: the data folder is unknown");
    }

    store
}

mod stale;

#[cfg(test)]
mod tests;
