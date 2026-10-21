//! The saving notice of the read tools: `note_search`, `note_get` and `entity_neighbors` (notes),
//! `script_list` and `script_get` (scripts) end their reply with one line when the save guards
//! ([`super::guards`]) do not plainly let the agent save that kind in this call, so an agent
//! knows before it tries. The guard files are read on every call. Scripts replies also carry
//! `structuredContent.saving`: `{"allowed": false, "reason"}` or `{"ask": true}`.
//!
//! The notice never changes or fails the read: a guard file that cannot be read gives the
//! `guard-unreadable` notice.

use serde_json::{Value, json};

use super::Server;
use super::calls::Reply;
use super::guards::{Kind, Reason, Verdict, verdict};

/// The notice one read adds to its reply.
pub(super) struct Notice {
    line: String,
    saving: Value,
}

impl Notice {
    /// The bytes the notice adds to a reply, which a capped reply keeps free for it.
    pub(super) fn room(&self) -> usize {
        self.line.len() + 1
    }

    /// Ends the successful `reply` with the notice line, and puts it in its `structuredContent`
    /// when the reply has one. An error reply stays as it is.
    pub(super) fn add_to(self, reply: &mut Reply) {
        if reply.is_error {
            return;
        }

        if !reply.text.is_empty() {
            reply.text.push('\n');
        }

        reply.text.push_str(&self.line);

        if let Some(Value::Object(structured)) = &mut reply.structured {
            structured.insert("saving".to_owned(), self.saving);
        }
    }
}

/// The notice of read tool `name` with `arguments`; `None` for another tool, or when saving is
/// plainly allowed.
pub(super) fn notice(server: &mut Server, name: &str, arguments: &Value) -> Option<Notice> {
    let kind = read_kind(name)?;

    match verdict(server, kind, arguments) {
        Verdict::Allow => None,
        Verdict::Ask { .. } => Some(Notice {
            line: ask_line(kind),
            saving: json!({ "ask": true }),
        }),
        Verdict::Refuse(reason) => Some(Notice {
            line: refused_line(reason, kind)?,
            saving: json!({ "allowed": false, "reason": reason.as_str() }),
        }),
    }
}

/// The kind read tool `name` reads.
fn read_kind(name: &str) -> Option<Kind> {
    match name {
        "note_search" | "note_get" | "entity_neighbors" => Some(Kind::Notes),
        "script_list" | "script_get" => Some(Kind::Scripts),
        _ => None,
    }
}

fn ask_line(kind: Kind) -> String {
    let words = match kind {
        Kind::Notes => "Notes",
        Kind::Scripts => "Scripts",
    };

    format!("{words} you save in this chat wait for the user's review before they are kept.")
}

/// The notice line for saving `kind` refused for `reason`; `None` for the reasons only a save
/// meets.
fn refused_line(reason: Reason, kind: Kind) -> Option<String> {
    let words = kind.words();

    let line = match reason {
        Reason::ChatOff => {
            format!(
                "Saving {words} is off for this chat (the user's setting). Reading works. Do not try to save {words}."
            )
        }
        Reason::ChatPaused => {
            "Saving is paused for this chat (the user's setting). Reading works. Do not try to save.".to_owned()
        }
        Reason::RepoOff => format!(
            "Saving {words} is off for this project (the user's setting). Reading works. Do not try to save {words}."
        ),
        Reason::GlobalPaused => format!(
            "Saving {words} is paused everywhere (the user's setting). Reading works. Do not try to save {words}."
        ),
        Reason::GuardUnreadable => {
            format!("Saving {words} is blocked: the save settings could not be read. Reading works.")
        }
        Reason::NoDesktop | Reason::HeldLimit => return None,
    };

    Some(line)
}
