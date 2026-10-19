//! The MCP server: newline-delimited JSON-RPC 2.0 over stdio, read synchronously one bounded line
//! at a time.
//!
//! - A line longer than [`LINE_MAX`] is answered with an error and skipped up to its newline.
//! - Unparsable JSON gets `-32700`, a batch (array) or a malformed request `-32600`, an unknown
//!   method `-32601`; serving continues after each.
//! - Notifications are never answered; answers from the client are ignored.
//! - Tool validation and store failures are tool results with `isError: true`.
//! - Nothing a client sends is written to stderr.
//! - A running script stops when the client cancels its request, closes the input, or the server
//!   gets SIGTERM ([`input`]).

mod approval;
mod blocks;
mod calls;
mod calls_scripts;
mod input;
pub mod protocol;
mod reply;
mod script_blocks;
mod search_scripts;
pub mod tools;
mod tools_scripts;

pub(crate) use approval::approval_of;
pub use approval::{ALIVE_FILE, APPROVAL_WAIT_VAR, REQUESTS_FOLDER, SCRIPT_APPROVAL_WAIT};
pub use input::stop_on_sigterm;
pub(crate) use script_blocks::{Approval, Shown, author_json, place_label, repo_label};

use std::io::{self, BufRead, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use serde_json::{Value, json};

use crate::backend::{NotesPlace, Place};
use crate::limits::LINE_MAX;
use crate::project::ProjectResolver;
use crate::store::{MemoryStore, ScriptStore, TrustStore};
use crate::support::runner::Stop;
use input::Watch;
use protocol::{INVALID_PARAMS, INVALID_REQUEST, Line, METHOD_NOT_FOUND, PARSE_ERROR};

/// How often the serving loop looks whether the server got SIGTERM while no input comes.
const TERMINATE_POLL: Duration = Duration::from_millis(100);

/// The state one server process keeps: the note store, the script store and where scripts are
/// read, the other script store (see [`Self::with_other_scripts`]), this machine's script trust
/// (and whether team scripts count as approved), the project resolver and the client's name.
pub struct Server {
    store: Box<dyn MemoryStore>,
    scripts: Box<dyn ScriptStore>,
    scripts_place: NotesPlace,
    /// The trust name of the team database ([`crate::backend::Backend::team_key`]).
    team_key: String,
    other_scripts: Option<Box<dyn ScriptStore>>,
    trust: Box<dyn TrustStore>,
    trust_team_scripts: bool,
    projects: ProjectResolver,
    client: Option<String>,
    /// Stops the script running now ([`input`]).
    stop: Stop,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// A server over the notes in `store` and the scripts in `scripts` (on this computer unless
    /// [`Self::with_scripts_place`] says otherwise), trusting script versions through `trust` and
    /// resolving repositories with `projects`.
    pub fn new(
        store: Box<dyn MemoryStore>,
        scripts: Box<dyn ScriptStore>,
        trust: Box<dyn TrustStore>,
        projects: ProjectResolver,
    ) -> Self {
        Self {
            store,
            scripts,
            scripts_place: NotesPlace::Local,
            team_key: "team".to_owned(),
            other_scripts: None,
            trust,
            trust_team_scripts: false,
            projects,
            client: None,
            stop: Stop::default(),
        }
    }

    /// The server whose running script `stop` stops (see [`stop_on_sigterm`]); [`serve`] also
    /// fires it when the client cancels the request or closes the input.
    pub fn with_stop(mut self, stop: Stop) -> Self {
        self.stop = stop;
        self
    }

    /// The server with its scripts read from `place` (see
    /// [`crate::backend::Selection::scripts_place`]), which the script tools report: a store that
    /// keeps every script in one place keeps them where `place` saves them, a
    /// [`crate::store::SplitScripts`] says per script ([`ScriptStore::origin`]).
    pub fn with_scripts_place(mut self, place: impl Into<NotesPlace>) -> Self {
        self.scripts_place = place.into();
        self
    }

    /// The server naming the team database `key` in this machine's script trust (`team` unless
    /// set, see [`crate::backend::Selection::scripts_trust_key`]).
    pub fn with_team_key(mut self, key: String) -> Self {
        self.team_key = key;
        self
    }

    /// The server with `store`, the scripts in the place scripts are not read from (the team
    /// database when they stay on this computer, the local file when they are `remote`):
    /// `script_save` does not trust a copy of a script waiting for approval there either.
    pub fn with_other_scripts(mut self, store: Box<dyn ScriptStore>) -> Self {
        self.other_scripts = Some(store);
        self
    }

    /// The server running every script read from the team database without asking first
    /// (`trust_team_scripts`, see [`crate::backend::Selection::trust_team_scripts`]).
    pub fn with_trust_team_scripts(mut self, trusted: bool) -> Self {
        self.trust_team_scripts = trusted;
        self
    }

    /// True when scripts read from `place` count as approved: the team database with
    /// `trust_team_scripts` on.
    fn team_trusted(&self, place: Place) -> bool {
        self.trust_team_scripts && place == Place::Remote
    }

    /// The trust name of `place`: `local`, or the team database's key.
    fn place_key(&self, place: Place) -> String {
        match place {
            Place::Local => crate::backend::LOCAL_KEY.to_owned(),
            Place::Remote => self.team_key.clone(),
        }
    }

    /// Where the other script store is: the place scripts are not read from (none with
    /// `remote+local`, which reads both).
    fn other_place(&self) -> Option<Place> {
        match self.scripts_place {
            NotesPlace::Local => Some(Place::Remote),
            NotesPlace::Remote => Some(Place::Local),
            NotesPlace::RemoteLocal => None,
        }
    }

    /// Where the script the script store shows as `id` is kept, and its id there (the id this
    /// machine's trust names it by).
    fn spot(&self, id: i64) -> (Place, i64) {
        self.scripts.origin(id).spot(id, self.scripts_place.writes())
    }

    /// Answers one parsed message; `None` for notifications and client answers.
    pub fn handle(&mut self, message: Value) -> Option<Value> {
        let null = Value::Null;

        let Some(object) = message.as_object() else {
            return Some(protocol::error(&null, INVALID_REQUEST, "Invalid Request"));
        };

        let id = object.get("id");
        let method = object.get("method").and_then(Value::as_str);
        let valid_id = id.is_none_or(|id| id.is_string() || id.is_number() || id.is_null());
        let reply_id = id.filter(|_| valid_id).cloned().unwrap_or(Value::Null);

        let Some(method) = method else {
            let is_answer = id.is_some() && (object.contains_key("result") || object.contains_key("error"));
            return (!is_answer).then(|| protocol::error(&reply_id, INVALID_REQUEST, "Invalid Request"));
        };

        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || !valid_id {
            return Some(protocol::error(&reply_id, INVALID_REQUEST, "Invalid Request"));
        }

        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));

        // A notification (no id) is never answered, whatever its method.
        let id = id?;

        let answer = match method {
            "initialize" => {
                self.client = params
                    .pointer("/clientInfo/name")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                protocol::result(id, protocol::initialize_result(&params))
            }
            "ping" => protocol::result(id, json!({})),
            "tools/list" => protocol::result(id, tools::definitions()),
            "tools/call" => self.tools_call(id, &params),
            _ => protocol::error(id, METHOD_NOT_FOUND, "Method not found"),
        };

        Some(answer)
    }

    fn tools_call(&mut self, id: &Value, params: &Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return protocol::error(id, INVALID_PARAMS, "tools/call needs a tool name");
        };

        let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

        match calls::call(self, name, arguments) {
            Some(reply) => protocol::result(id, tool_result(reply)),
            None => protocol::error(id, INVALID_PARAMS, &format!("unknown tool: {}", short(name))),
        }
    }
}

/// The `tools/call` result for `reply`: its text, `isError`, and `structuredContent` when the
/// reply has one.
fn tool_result(reply: calls::Reply) -> Value {
    let mut result = json!({
        "content": [{ "type": "text", "text": reply.text }],
        "isError": reply.is_error,
    });

    if let Some(structured) = reply.structured {
        result["structuredContent"] = structured;
    }

    result
}

/// Serves messages from `reader` until end of input or SIGTERM ([`stop_on_sigterm`]); fails only
/// when reading or writing fails. `reader` is read on a thread of its own ([`input`]).
pub fn serve<R: BufRead + Send + 'static, W: Write>(reader: R, mut writer: W, server: &mut Server) -> io::Result<()> {
    let watch = Watch::new(server.stop.clone());
    let (sender, lines) = mpsc::channel();
    input::spawn_reader(reader, watch.clone(), sender);

    loop {
        let line = match lines.recv_timeout(TERMINATE_POLL) {
            Ok(line) => line?,
            Err(RecvTimeoutError::Timeout) if server.stop.is_terminated() => return Ok(()),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };

        if server.stop.is_terminated() {
            return Ok(());
        }

        let reply = match line {
            Line::End => return Ok(()),
            Line::TooLong => Some(protocol::error(
                &Value::Null,
                INVALID_REQUEST,
                &format!("message exceeds {LINE_MAX} bytes"),
            )),
            Line::Text(line) if line.iter().all(u8::is_ascii_whitespace) => None,
            Line::Text(line) => match serde_json::from_slice::<Value>(&line) {
                Ok(message) => answer(server, &watch, message),
                Err(_) => Some(protocol::error(&Value::Null, PARSE_ERROR, "Parse error")),
            },
        };

        if let Some(reply) = reply {
            protocol::write_message(&mut writer, &reply)?;
        }
    }
}

/// The answer to `message`, none when the client cancelled it while it ran.
fn answer(server: &mut Server, watch: &Watch, message: Value) -> Option<Value> {
    let request = message.get("method").and_then(|_| message.get("id")).cloned();

    watch.begin(request.as_ref());
    let reply = server.handle(message);
    let cancelled = watch.finish();

    reply.filter(|_| !cancelled)
}

/// At most 64 characters of agent text, for error messages.
fn short(text: &str) -> String {
    text.chars().take(64).collect()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn structured_content_is_sent_only_when_the_reply_has_it() {
        let plain = calls::Reply {
            text: "saved".to_owned(),
            is_error: false,
            structured: None,
        };
        assert_eq!(
            tool_result(plain),
            json!({ "content": [{ "type": "text", "text": "saved" }], "isError": false })
        );

        let structured = calls::Reply {
            text: "saved".to_owned(),
            is_error: true,
            structured: Some(json!({ "untrusted": true })),
        };
        assert_eq!(
            tool_result(structured),
            json!({
                "content": [{ "type": "text", "text": "saved" }],
                "isError": true,
                "structuredContent": { "untrusted": true },
            })
        );
    }
}
