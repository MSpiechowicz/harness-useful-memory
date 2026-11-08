//! An agent: the real `harness-memory mcp` over JSON-RPC on standard input.

use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::common::stdio::Client;

/// What one tool call answered.
pub struct Reply {
    pub text: String,
    pub is_error: bool,
    pub structured: Value,
    /// How long the call took.
    pub took: Duration,
}

impl Reply {
    /// Whether the reply says it comes from this computer because the team database is down.
    pub fn is_offline(&self) -> bool {
        self.text.contains("Offline: the team database is unreachable")
    }
}

/// One open chat's server.
pub struct Agent {
    client: Client,
}

impl Agent {
    pub fn start(command: Command) -> Self {
        Self {
            client: Client::start(command),
        }
    }

    /// Calls tool `name`, whether it succeeds or not.
    pub fn tool(&mut self, name: &str, arguments: Value) -> Reply {
        let started = Instant::now();
        let reply = self
            .client
            .request("tools/call", json!({ "name": name, "arguments": arguments }));
        let took = started.elapsed();

        Reply {
            text: reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            is_error: reply["result"]["isError"].as_bool().unwrap_or(true),
            structured: reply["result"]["structuredContent"].clone(),
            took,
        }
    }

    /// Calls tool `name`, which must succeed.
    pub fn ok(&mut self, name: &str, arguments: Value) -> Reply {
        let reply = self.tool(name, arguments);
        assert!(!reply.is_error, "{name} failed: {}", reply.text);
        reply
    }

    /// `note_save` of a repository note.
    pub fn save_note(&mut self, title: &str, body: &str) -> Reply {
        self.tool(
            "note_save",
            json!({ "title": title, "body": body, "kind": "decision", "tags": ["sync", "suite"] }),
        )
    }

    /// `note_save` of a global note.
    pub fn save_global_note(&mut self, title: &str, body: &str) -> Reply {
        self.tool(
            "note_save",
            json!({ "title": title, "body": body, "kind": "decision", "scope": "global", "tags": ["sync", "suite"] }),
        )
    }

    /// `script_save` of a repository script that prints its name.
    pub fn save_script(&mut self, name: &str) -> Reply {
        self.tool(
            "script_save",
            json!({
                "name": name,
                "description": "A script of the sync suite",
                "language": "sh",
                "body": format!("echo {name}"),
            }),
        )
    }

    /// Ends the chat.
    pub fn end(self) {
        self.client.finish();
    }
}
