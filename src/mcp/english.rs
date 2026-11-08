//! The English notes setting for agents' writes: a clearly non-English `note_save` or
//! `note_update` is refused before the save guards run ([`super::guards`]), nothing is saved or
//! held. The user's own saves ([`super::user`]) are never refused, they only warn.
//!
//! The reply has the shape of a guard refusal: a normal result (not an error) with
//! `structuredContent` `{"saved": false, "reason": "not-english"}`. Unlike a guard's, the agent
//! can fix it: rewrite the note in English and call the tool again.
//!
//! The check is conservative ([`crate::support::lang::detect`]): short, mixed and code-heavy
//! notes pass. A setting that cannot be read does not block a save: the store may be unreachable
//! for a moment, and refusing then would lose a note the user may well want.
//!
//! A `note_update` of a team note while the team database is unreachable cannot read the note it
//! changes (the change is queued, see [`crate::store::pending_updates`]): the title, body and text to
//! append the call gives are checked as they are, so the change is refused like an online one.

use serde::Deserialize;
use serde_json::{Value, json};

use super::Server;
use super::calls::Reply;
use super::scope::view;
use super::user::unverifiable;
use crate::support::lang::{self, Lang};

/// The reason in `structuredContent`.
pub(super) const REASON: &str = "not-english";

/// The arguments of `note_save` the check reads; anything else is for the tool to reject.
#[derive(Deserialize)]
struct SaveText {
    title: String,
    body: String,
}

/// The arguments of `note_update` the check reads.
#[derive(Deserialize)]
struct UpdateText {
    id: i64,
    title: Option<String>,
    body: Option<String>,
    append: Option<String>,
    scope: Option<String>,
    project_path: Option<String>,
}

/// The refusal when tool `name` is called with a note that is clearly not English while the
/// English notes setting is on; `None` lets the call go on.
pub(super) fn gate(server: &mut Server, name: &str, arguments: &Value) -> Option<Reply> {
    let (title, body, again) = match name {
        "note_save" => {
            let text: SaveText = serde_json::from_value(arguments.clone()).ok()?;
            (text.title, text.body, "note_save")
        }
        "note_update" => {
            let (title, body) = updated(server, arguments)?;
            (title, body, "note_update")
        }
        _ => return None,
    };

    if lang::detect(&title, &body) != Some(Lang::Other) {
        return None;
    }

    // Read last: a note in English never costs a read of the setting.
    if !server.store.english_notes().unwrap_or(false) {
        return None;
    }

    Some(refusal(again))
}

/// The title and body note `id` has after the `note_update` `arguments`; `None` when the call
/// changes neither (the tool answers it) or the note cannot be read (the tool says why). A team note
/// that cannot be read because the team database is down gives the text the call supplies alone.
fn updated(server: &mut Server, arguments: &Value) -> Option<(String, String)> {
    let args: UpdateText = serde_json::from_value(arguments.clone()).ok()?;

    if args.title.is_none() && args.body.is_none() && args.append.is_none() {
        return None;
    }

    let view = view(server, args.scope.as_deref(), args.project_path.as_deref()).ok()?;
    let note = server.store.peek(&view, &[args.id]).ok()?.into_iter().next();

    let Some(note) = note else {
        return unverifiable(server, args.id).then(|| supplied(args));
    };

    let title = args.title.unwrap_or(note.title);
    let mut body = args.body.unwrap_or(note.body);

    if let Some(append) = args.append {
        body.push('\n');
        body.push_str(&append);
    }

    Some((title, body))
}

/// The title and body the call `args` gives, with the text to append after the body.
fn supplied(args: UpdateText) -> (String, String) {
    let title = args.title.unwrap_or_default();
    let body = [args.body, args.append]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");

    (title, body)
}

fn refusal(again: &str) -> Reply {
    Reply {
        text: format!(
            "This note is not in English, and this store keeps notes in English. Rewrite the title and body in \
             English and call {again} again. Nothing was saved."
        ),
        is_error: false,
        structured: Some(json!({ "saved": false, "reason": REASON })),
    }
}
