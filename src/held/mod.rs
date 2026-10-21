//! Held items: what an agent asked to save while the user wants to review it first ("Ask before
//! saving", see the save guards in [`crate::mcp`]). Each is one file
//! `held/<chatKey>/<heldId>.json` next to the database (`0600` in `0700` folders of this user,
//! one per chat, written to a temporary file and renamed, read without following a symbolic link,
//! see [`crate::support::private`]), never in a team database. A held item is not saved: it is in
//! no store, search, list or trust until the user saves it with `harness-memory held save`
//! ([`crate::cli_held`]).
//!
//! - At most [`HELD_PER_CHAT`] items and [`HELD_CHAT_BYTES`] bytes per chat: one more is refused,
//!   none is dropped. Only the chat's own folder is read to count them, or to list its items.
//! - Holding the same note title (`note_save`), note id (`note_update`) or script name
//!   (`script_save`) again in the same chat replaces the earlier item, keeping its `heldId` and
//!   `createdAt`; its `revision` (the SHA-256 of its canonical content, [`HeldItem::digest`])
//!   changes with the content.
//! - Saving or skipping claims the item first ([`HeldStore::claim`]): its file is renamed to a
//!   claim name in the same folder, so two saves of one item cannot both save it, and a hold of
//!   the same key meanwhile is held as a new item (a hold moves the earlier file aside the same way
//!   before it replaces it).
//! - At most [`HELD_FILE_MAX`] bytes per file; files older than 30 days are swept.
//! - A file that cannot be read, of another version, whose `heldId` is not its name, whose
//!   `chatKey` is not its folder's or whose `revision` does not match its content is skipped, and
//!   so are files of the older flat layout `held/<heldId>.json`.

mod store;

use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub use store::{Claim, HeldStore, HoldError};

/// The folder next to the database that holds the held items.
pub const HELD_FOLDER: &str = "held";

/// The most items one chat may have waiting.
pub const HELD_PER_CHAT: usize = 50;

/// The largest held file, in bytes.
pub const HELD_FILE_MAX: usize = 512 * 1024;

/// The most bytes one chat's held files may take together.
pub const HELD_CHAT_BYTES: u64 = 4 * 1024 * 1024;

/// The version of the held file format.
const VERSION: u32 = 1;

/// What a held item would save.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeldKind {
    Note,
    Script,
}

/// The tool call that was held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldTool {
    NoteSave,
    NoteUpdate,
    ScriptSave,
}

impl HeldTool {
    /// The tool named `name`, when it can be held.
    pub fn of(name: &str) -> Option<Self> {
        match name {
            "note_save" => Some(Self::NoteSave),
            "note_update" => Some(Self::NoteUpdate),
            "script_save" => Some(Self::ScriptSave),
            _ => None,
        }
    }

    pub fn kind(self) -> HeldKind {
        match self {
            Self::NoteSave | Self::NoteUpdate => HeldKind::Note,
            Self::ScriptSave => HeldKind::Script,
        }
    }
}

/// One `held/<chatKey>/<heldId>.json` file (camelCase, version 1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeldItem {
    pub version: u32,
    pub held_id: String,
    pub chat_key: String,
    /// RFC 3339.
    pub created_at: String,
    pub client: String,
    pub kind: HeldKind,
    pub tool: HeldTool,
    /// The folder the call was about: its `project_path`, else the server's working folder.
    pub project_path: Option<String>,
    /// The chat's repository (a canonical git root), when there is one.
    pub repo_root: Option<String>,
    /// The tool arguments in normal form: the fields the tool knows, as it reads them, with note
    /// text, tags, entities and script descriptions masked as the store masks them.
    pub arguments: Value,
    /// The note or script the call would change, as saved now; `null` for a new one.
    pub current: Value,
    /// The [`Self::digest`] of the content, set when it is held.
    pub revision: String,
}

impl HeldItem {
    /// A new item of `tool` for chat `chat_key` (its id and time are set when it is held).
    pub fn new(chat_key: &str, client: &str, tool: HeldTool, arguments: Value, current: Value) -> Self {
        Self {
            version: VERSION,
            held_id: String::new(),
            chat_key: chat_key.to_owned(),
            created_at: String::new(),
            client: client.to_owned(),
            kind: tool.kind(),
            tool,
            project_path: None,
            repo_root: None,
            arguments,
            current,
            revision: String::new(),
        }
    }

    /// The lowercase hex SHA-256 of the item's canonical JSON (keys sorted, no blanks) without
    /// its `revision`.
    pub fn digest(&self) -> String {
        let mut value = serde_json::to_value(self).unwrap_or(Value::Null);

        if let Value::Object(fields) = &mut value {
            fields.remove("revision");
        }

        let text = serde_json::to_vec(&sorted(value)).unwrap_or_default();
        hex(&Sha256::digest(&text))
    }

    /// True when holding `other` replaces this item: the same chat and tool, and the same note
    /// title (ignoring case and surrounding blanks), note id or script name.
    fn is_replaced_by(&self, other: &Self) -> bool {
        self.chat_key == other.chat_key && self.tool == other.tool && self.key().is_some() && self.key() == other.key()
    }

    fn key(&self) -> Option<String> {
        let field = |name: &str| self.arguments.get(name);

        match self.tool {
            HeldTool::NoteSave => field("title")?.as_str().map(|title| title.trim().to_lowercase()),
            HeldTool::NoteUpdate => field("id")?.as_i64().map(|id| id.to_string()),
            HeldTool::ScriptSave => field("name")?.as_str().map(|name| name.trim().to_owned()),
        }
    }
}

/// `bytes` as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `value` with the keys of every object sorted.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(fields) => {
            let mut fields: Vec<(String, Value)> = fields.into_iter().collect();
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                fields
                    .into_iter()
                    .map(|(key, value)| (key, sorted(value)))
                    .collect::<Map<_, _>>(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// Unix seconds now.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests;
