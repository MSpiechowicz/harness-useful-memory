//! The user's own saves, for the user-only commands (`notes save`, `notes update`, `scripts
//! save`, `held save`, see [`crate::cli_held`]): the bodies of `note_save`, `note_update` and
//! `script_save` without the save guards, which only stop agents' MCP calls. Nothing here is
//! reachable through MCP.

use serde_json::{Value, json};

use super::Server;
use super::calls::{self, parse};
use super::calls_scripts::{self, Trusting};
use crate::store::{Note, Saved};

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
