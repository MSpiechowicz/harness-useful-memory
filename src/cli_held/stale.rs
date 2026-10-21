//! A held `note_update` shown against a note that changed since it was held (see `held save` in
//! [`super`]).

use serde_json::{Value, json};

use super::{Code, Saver};
use crate::held::{HeldItem, HeldTool};
use crate::mcp::user;
use crate::store::Note;

/// For a held `note_update`, the note as it is now (as `current`) when its title, body, kind or
/// tags differ from the held `current`; `None` when they do not, or for another tool. A deleted
/// note is `gone`.
pub(super) fn stale_current(saver: &mut Saver, item: &HeldItem) -> Result<Option<Value>, (Code, String)> {
    if item.tool != HeldTool::NoteUpdate {
        return Ok(None);
    }

    let id = item.arguments.get("id").and_then(Value::as_i64).unwrap_or_default();
    let scope = item.arguments.get("scope").and_then(Value::as_str);
    let live = user::note(&mut saver.server, id, scope).map_err(|message| (Code::Refused, message))?;

    match live {
        None => Err((Code::Gone, format!("note {id} was deleted since the agent changed it"))),
        Some(live) if shown_as_current(&live, &item.current) => Ok(None),
        Some(live) => Ok(Some(user::note_current(&live))),
    }
}

/// `note N` for the note a held `note_update` changes.
pub(super) fn note_named(item: &HeldItem) -> String {
    let id = item.arguments.get("id").and_then(Value::as_i64).unwrap_or_default();
    format!("note {id}")
}

/// True when the title, body, kind and tags of the note as it is now (`live`) are those of the
/// held `current`, the note the user is shown the update against.
fn shown_as_current(live: &Note, current: &Value) -> bool {
    current["title"] == json!(live.title)
        && current["body"] == json!(live.body)
        && current["kind"] == json!(live.kind.as_str())
        && current["tags"] == json!(live.tags)
}
