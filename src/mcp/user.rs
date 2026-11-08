//! The user's own saves, for the user-only commands (`notes save`, `notes update`, `scripts
//! save`, `held save`, see [`crate::cli_held`]): the bodies of `note_save`, `note_update` and
//! `script_save` without the save guards, which only stop agents' MCP calls. Nothing here is
//! reachable through MCP.

use serde_json::{Value, json};

use super::Server;
pub(crate) use super::calls::UpdateError;
use super::calls::{self, parse};
use super::calls_scripts::{self, Trusting};
use crate::backend::Place;
use crate::store::split::is_local_id;
use crate::store::{Note, Saved};
use crate::support::lang::{self, Lang};

/// Saves the note `arguments` describe (`note_save`'s arguments).
pub(crate) fn save_note(server: &mut Server, arguments: Value) -> Result<Saved, String> {
    let args = parse(arguments)?;
    calls::save_note(server, args).map(|(_, saved)| saved)
}

/// [`save_note`] into the store `place` names, for a scope routed `remote+local` (the caller checks
/// the routing, and the store refuses any other).
pub(crate) fn save_note_to(server: &mut Server, arguments: Value, place: Place) -> Result<Saved, String> {
    let args = parse(arguments)?;
    calls::save_note_to(server, args, Some(place)).map(|(_, saved)| saved)
}

/// [`update_note`] that changes the note only if it is still at revision `expect` (when given), and
/// tells that apart from another failure.
pub(crate) fn update_note_expecting(
    server: &mut Server,
    arguments: Value,
    expect: Option<i64>,
) -> Result<Saved, UpdateError> {
    let mut args: calls::UpdateArgs = parse(arguments).map_err(UpdateError::Failed)?;
    args.expect_revision = expect;
    calls::update_note_typed(server, args)
}

/// Decides now whether the team database is reachable, as every tool call does first: after it,
/// [`unverifiable`] tells a team note that cannot be read from one that is not there.
pub(crate) fn settle(server: &mut Server) {
    let _ = server.store.ready();
}

/// True when note `id` is a team note while the team database is unreachable: it cannot be read
/// now, so that it is not found says nothing about it. Asked after a read that came back empty.
pub(crate) fn unverifiable(server: &Server, id: i64) -> bool {
    server.store.offline() && !is_local_id(id)
}

/// Saves the script `arguments` describe (`script_save`'s arguments), trusted on this machine as
/// `script_save` trusts it: the `structuredContent` of `script_save` (`{"script", "created"}`).
pub(crate) fn save_script(server: &mut Server, arguments: Value) -> Result<Value, String> {
    let args = parse(arguments)?;
    let reply = calls_scripts::save(server, args, Trusting::AsSaved)?;
    Ok(reply.structured.unwrap_or(Value::Null))
}

/// [`save_script`] into the store `place` names, for scripts routed `remote+local` (the caller
/// checks the routing, and the store refuses any other).
pub(crate) fn save_script_to(server: &mut Server, arguments: Value, place: Place) -> Result<Value, String> {
    let args = parse(arguments)?;
    let reply = calls_scripts::save_to(server, args, Trusting::AsSaved, Some(place))?;
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

/// [`save_held_script`] into the store `place` names, for scripts routed `remote+local` (the
/// caller checks the routing, and the store refuses any other).
pub(crate) fn save_held_script_to(server: &mut Server, arguments: Value, place: Place) -> Result<Value, String> {
    let args = parse(arguments)?;
    let reply = calls_scripts::save_to(server, args, Trusting::Held, Some(place))?;
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
        "revision": note.revision,
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
