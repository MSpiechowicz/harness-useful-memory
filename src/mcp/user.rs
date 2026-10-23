//! The user's own saves, for the user-only commands (`notes save`, `notes update`, `scripts
//! save`, `held save`, see [`crate::cli_held`]): the bodies of `note_save`, `note_update` and
//! `script_save` without the save guards, which only stop agents' MCP calls. Nothing here is
//! reachable through MCP.

use serde_json::{Value, json};

use super::Server;
use super::calls::{self, parse};
use super::calls_scripts::{self, Trusting};
use crate::store::{Note, Saved};
use crate::support::lang::{self, Lang};

/// Saves the note `arguments` describe (`note_save`'s arguments).
pub(crate) fn save_note(server: &mut Server, arguments: Value) -> Result<Saved, String> {
    let args = parse(arguments)?;
    calls::save_note(server, args).map(|(_, saved)| saved)
}

/// Changes the note `arguments` name (`note_update`'s arguments).
pub(crate) fn update_note(server: &mut Server, arguments: Value) -> Result<Saved, String> {
    let args = parse(arguments)?;
    calls::update_note(server, args)
}

/// Saves the script `arguments` describe (`script_save`'s arguments), trusted on this machine as
/// `script_save` trusts it: the `structuredContent` of `script_save` (`{"script", "created"}`).
pub(crate) fn save_script(server: &mut Server, arguments: Value) -> Result<Value, String> {
    let args = parse(arguments)?;
    let reply = calls_scripts::save(server, args, Trusting::AsSaved)?;
    Ok(reply.structured.unwrap_or(Value::Null))
}

/// Saves the held script `arguments` describe, never trusted, not even as a team script while
/// every team script is trusted: it needs the user's approval before it runs. The
/// `structuredContent` of `script_save`.
pub(crate) fn save_held_script(server: &mut Server, arguments: Value) -> Result<Value, String> {
    let args = parse(arguments)?;
    let reply = calls_scripts::save(server, args, Trusting::Held)?;
    Ok(reply.structured.unwrap_or(Value::Null))
}

/// Note `id`, when the notes of `scope` (`repo`, `global`, both when `None`) show it, counting no
/// hit.
pub(crate) fn note(server: &mut Server, id: i64, scope: Option<&str>) -> Result<Option<Note>, String> {
    let view = super::scope::view(server, scope, None)?;
    let notes = server.store.peek(&view, &[id]).map_err(|error| error.to_string())?;
    Ok(notes.into_iter().next())
}

/// `note` as a held `note_update` records it as `current`: `{id, title, body, kind, tags,
/// entities, scope}`.
pub(crate) fn note_current(note: &Note) -> Value {
    json!({
        "id": note.id,
        "title": note.title,
        "body": note.body,
        "kind": note.kind.as_str(),
        "tags": note.tags,
        "entities": note.entities,
        "scope": note.scope.as_str(),
    })
}

/// What the user is told on stderr when their own note is not in English and the store keeps
/// English notes.
pub(crate) const NOT_ENGLISH_WARNING: &str = "this note is not in English, and this store keeps notes in English. It was saved as written: rewriting it in English helps agents find it";

/// True when note `id` (in `scope`) is clearly not English and the English notes setting is on.
/// The user's save is never refused for it, so this only tells them. A setting that cannot be read
/// gives no warning.
pub(crate) fn not_english(server: &mut Server, id: i64, scope: Option<&str>) -> bool {
    if !server.store.english_notes().unwrap_or(false) {
        return false;
    }

    match note(server, id, scope) {
        Ok(Some(note)) => lang::detect(&note.title, &note.body) == Some(Lang::Other),
        _ => false,
    }
}
