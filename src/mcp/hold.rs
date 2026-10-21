//! Holding a `note_save`, `note_update` or `script_save` for the user's review (see
//! [`super::guards`] and [`crate::held`]). The arguments are checked first as the tool would
//! check them, so an invalid call gets the tool's own error and nothing waits that could not be
//! saved. They are held in normal form: only the fields the tool knows (others are dropped), the
//! scope, kind and language as the tool reads them, the note text, tags and entities and the
//! script description masked as the store masks them (a script body is kept as sent). A
//! `note_update` needs the note it changes, and records it as `current`; a `script_save` that
//! replaces a script of the same name in its scope records that script.

use std::path::PathBuf;

use serde_json::{Map, Value, json};

use super::Server;
use super::calls::{RememberArgs, Reply, UpdateArgs, parse};
use super::calls_scripts::{SaveArgs, draft_of};
use super::guard_repos::chat_repos;
use super::guards::{Reason, held, kind_of, refused};
use super::scope::{kind, scope_arg, target, view};
use super::script_blocks::shown_text;
use crate::held::{HeldItem, HeldStore, HeldTool, HoldError};
use crate::store::{NoteDraft, NotePatch, Provenance, ScriptRef, Target, View, clean};
use crate::support::redact::redact_text;

/// The longest client name a held item shows.
const CLIENT_MAX: usize = 64;

/// Holds the call of `tool` with `arguments` for chat `chat`, in the data folder `data`: the
/// held reply, a refusal (`held-limit`, or `guard-unreadable` when the item cannot be written),
/// or the tool's error for arguments it would refuse. `chat_root` is the chat's project root from
/// its guard file, the item's repository when the call's own is unknown.
pub(super) fn hold(
    server: &mut Server,
    tool: HeldTool,
    arguments: &Value,
    chat: &str,
    data: PathBuf,
    chat_root: Option<String>,
) -> Reply {
    let name = tool_name(tool);

    let (normal, current) = match checked(server, tool, arguments) {
        Ok(checked) => checked,
        Err(text) => {
            return Reply {
                text,
                is_error: true,
                structured: None,
            };
        }
    };

    let client = server
        .client
        .as_deref()
        .map_or_else(|| "unknown".to_owned(), |client| shown_text(client, CLIENT_MAX));
    let project_path = project_path(server, arguments);
    let repo_root = chat_repos(server, arguments)
        .first()
        .map(|project| project.root.to_string_lossy().into_owned())
        .or(chat_root);

    let mut item = HeldItem::new(chat, &client, tool, Value::Object(normal), current);
    item.project_path = project_path;
    item.repo_root = repo_root;

    match HeldStore::in_folder(data).hold(item) {
        Ok(held_id) => held(&held_id),
        Err(HoldError::Limit) => refused(Reason::HeldLimit, kind_of(tool), name),
        Err(HoldError::Failed(_)) => refused(Reason::GuardUnreadable, kind_of(tool), name),
    }
}

fn tool_name(tool: HeldTool) -> &'static str {
    match tool {
        HeldTool::NoteSave => "note_save",
        HeldTool::NoteUpdate => "note_update",
        HeldTool::ScriptSave => "script_save",
    }
}

/// The arguments to hold, in normal form (see the module docs), and the `current` note or
/// script, after checking them as the tool would; the tool's error otherwise.
fn checked(server: &mut Server, tool: HeldTool, arguments: &Value) -> Result<(Map<String, Value>, Value), String> {
    match tool {
        HeldTool::NoteSave => note_save(server, arguments),
        HeldTool::NoteUpdate => note_update(server, arguments),
        HeldTool::ScriptSave => script_save(server, arguments),
    }
}

fn note_save(server: &mut Server, arguments: &Value) -> Result<(Map<String, Value>, Value), String> {
    let args: RememberArgs = parse(arguments.clone())?;
    let scope = scope_arg(args.scope.as_deref(), "repo")?;
    target(server, Some(scope), args.project_path.as_deref())?;

    let draft = NoteDraft {
        title: args.title,
        body: args.body,
        kind: kind(&args.kind)?,
        tags: args.tags.unwrap_or_default(),
        entities: args.entities.unwrap_or_default(),
        provenance: Provenance::default(),
    };
    let cleaned = clean::draft(&draft).map_err(|error| error.to_string())?;
    let tags: Vec<&str> = cleaned.tags.split_whitespace().collect();

    let mut normal = Map::new();
    normal.insert("title".to_owned(), json!(redact_text(&draft.title)));
    normal.insert("body".to_owned(), json!(redact_text(&draft.body)));
    normal.insert("kind".to_owned(), json!(draft.kind.as_str()));
    normal.insert("tags".to_owned(), json!(tags));
    normal.insert("entities".to_owned(), json!(cleaned.entities));
    normal.insert("scope".to_owned(), json!(scope));
    insert_project_path(&mut normal, args.project_path.as_deref());

    Ok((normal, Value::Null))
}

fn note_update(server: &mut Server, arguments: &Value) -> Result<(Map<String, Value>, Value), String> {
    let args: UpdateArgs = parse(arguments.clone())?;
    let scope = args
        .scope
        .as_deref()
        .filter(|raw| !raw.trim().is_empty())
        .map(|raw| scope_arg(Some(raw), "both"))
        .transpose()?;
    let view = view(server, scope, args.project_path.as_deref())?;

    let patch = NotePatch {
        title: args.title,
        body: args.body,
        append: args.append,
        kind: args.kind.as_deref().map(kind).transpose()?,
        tags: args.tags,
        provenance: Provenance::default(),
    };
    let cleaned = clean::patch(&patch).map_err(|error| error.to_string())?;

    let note = server
        .store
        .peek(&view, &[args.id])
        .map_err(|error| error.to_string())?
        .into_iter()
        .next()
        .ok_or_else(|| format!("note {} not found in this repository or the global scope", args.id))?;

    let current = super::user::note_current(&note);

    let mut normal = Map::new();
    normal.insert("id".to_owned(), json!(args.id));

    let texts = [
        ("title", &patch.title),
        ("body", &patch.body),
        ("append", &patch.append),
    ];

    for (field, text) in texts {
        if let Some(text) = text {
            normal.insert(field.to_owned(), json!(redact_text(text)));
        }
    }

    if let Some(kind) = patch.kind {
        normal.insert("kind".to_owned(), json!(kind.as_str()));
    }

    if let Some(tags) = cleaned.tags {
        normal.insert("tags".to_owned(), json!(tags));
    }

    if let Some(scope) = scope {
        normal.insert("scope".to_owned(), json!(scope));
    }

    insert_project_path(&mut normal, args.project_path.as_deref());

    Ok((normal, current))
}

fn script_save(server: &mut Server, arguments: &Value) -> Result<(Map<String, Value>, Value), String> {
    let args: SaveArgs = parse(arguments.clone())?;
    let project_path = args.project_path.clone();
    let (target, draft) = draft_of(server, args)?;

    let (scope_name, scope) = match &target {
        Target::Repo(key) => (
            "repo",
            View {
                repo: Some(key.clone()),
                global: false,
            },
        ),
        Target::Global => (
            "global",
            View {
                repo: None,
                global: true,
            },
        ),
    };

    let replaced = server
        .scripts
        .get_script(&scope, &ScriptRef::Name(draft.name.clone()))
        .map_err(|error| error.to_string())?;

    let current = replaced.map_or(Value::Null, |script| {
        json!({
            "name": script.summary.name,
            "description": script.summary.description,
            "language": script.summary.language.as_str(),
            "body": script.body,
            "timeoutSecs": script.summary.timeout_secs,
            "scope": script.summary.scope.as_str(),
        })
    });

    // `checked_draft` trimmed the name and masked the description as the store keeps them.
    let mut normal = Map::new();
    normal.insert("name".to_owned(), json!(draft.name));
    normal.insert("description".to_owned(), json!(draft.description));
    normal.insert("language".to_owned(), json!(draft.language.as_str()));
    normal.insert("body".to_owned(), json!(draft.body));
    normal.insert("timeout_secs".to_owned(), json!(draft.timeout_secs));
    normal.insert("scope".to_owned(), json!(scope_name));
    insert_project_path(&mut normal, project_path.as_deref());

    Ok((normal, current))
}

/// Adds `project_path`, trimmed, when it is given and not blank.
fn insert_project_path(normal: &mut Map<String, Value>, project_path: Option<&str>) {
    if let Some(path) = project_path.map(str::trim).filter(|path| !path.is_empty()) {
        normal.insert("project_path".to_owned(), json!(path));
    }
}

/// The folder the call is about: its `project_path`, else the server's working folder (made
/// absolute).
fn project_path(server: &Server, arguments: &Value) -> Option<String> {
    let given = arguments
        .get("project_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|raw| !raw.is_empty());

    if let Some(given) = given {
        return Some(given.to_owned());
    }

    let cwd = server.projects.cwd()?;
    let absolute = std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    Some(absolute.to_string_lossy().into_owned())
}
