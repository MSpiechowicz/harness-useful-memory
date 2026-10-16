//! The MCP server: newline-delimited JSON-RPC 2.0 over stdio, read synchronously one bounded line
//! at a time.
//!
//! - A line longer than [`LINE_MAX`] is answered with an error and skipped up to its newline.
//! - Unparsable JSON gets `-32700`, a batch (array) or a malformed request `-32600`, an unknown
//!   method `-32601`; serving continues after each.
//! - Notifications are never answered; answers from the client are ignored.
//! - Tool validation and store failures are tool results with `isError: true`.
//! - Nothing a client sends is written to stderr.

mod blocks;
mod calls;
pub mod protocol;
mod reply;
pub mod tools;

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

use crate::limits::LINE_MAX;
use crate::project::ProjectResolver;
use crate::store::MemoryStore;
use protocol::{INVALID_PARAMS, INVALID_REQUEST, Line, METHOD_NOT_FOUND, PARSE_ERROR};

/// The state one server process keeps: the store, the project resolver and the client's name.
pub struct Server {
    store: Box<dyn MemoryStore>,
    projects: ProjectResolver,
    client: Option<String>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// A server over `store`, resolving repositories with `projects`.
    pub fn new(store: Box<dyn MemoryStore>, projects: ProjectResolver) -> Self {
        Self {
            store,
            projects,
            client: None,
        }
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
            Some(reply) => protocol::result(
                id,
                json!({
                    "content": [{ "type": "text", "text": reply.text }],
                    "isError": reply.is_error,
                }),
            ),
            None => protocol::error(id, INVALID_PARAMS, &format!("unknown tool: {}", short(name))),
        }
    }
}

/// Serves messages from `reader` until end of input; fails only when reading or writing fails.
pub fn serve<R: BufRead, W: Write>(mut reader: R, mut writer: W, server: &mut Server) -> io::Result<()> {
    let mut buffer = Vec::new();

    loop {
        let reply = match protocol::read_line(&mut reader, &mut buffer, LINE_MAX)? {
            Line::End => return Ok(()),
            Line::TooLong => Some(protocol::error(
                &Value::Null,
                INVALID_REQUEST,
                &format!("message exceeds {LINE_MAX} bytes"),
            )),
            Line::Text(line) if line.iter().all(u8::is_ascii_whitespace) => None,
            Line::Text(line) => match serde_json::from_slice::<Value>(&line) {
                Ok(message) => server.handle(message),
                Err(_) => Some(protocol::error(&Value::Null, PARSE_ERROR, "Parse error")),
            },
        };

        if let Some(reply) = reply {
            protocol::write_message(&mut writer, &reply)?;
        }
    }
}

/// At most 64 characters of agent text, for error messages.
fn short(text: &str) -> String {
    text.chars().take(64).collect()
}
