//! What the embedder and its clients say to each other: one JSON line per request and one per
//! reply, one request per connection, every request carrying `"v":1`.
//!
//! A reply is `{"ok":true,...}` or `{"ok":false,"error":"..."}`:
//!
//! - `ping` answers `{ok, model, ready, pending}`;
//! - `search {text, targets:[{store, projects}], limit, floor}` answers `{ok, model, hits:[{store,
//!   id, cosine}]}`, where `projects` lists repository keys and `null` for the global notes;
//! - `upsert {store, notes:[{id, project, title, body, tags}]}` and `forget {store, ids}` answer
//!   `{ok}`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The protocol version in every request.
pub const VERSION: u32 = 1;

/// The longest request line, in bytes.
pub const MAX_REQUEST: usize = 64 * 1024;

/// The most hits one search returns.
pub const MAX_HITS: usize = 200;

/// The notes of one store a search may return: those of the listed repositories (`None` is the
/// global notes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub store: String,
    pub projects: Vec<Option<String>>,
}

/// A note to index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteInput {
    pub id: i64,
    pub project: Option<String>,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// One request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    Ping,
    Search {
        text: String,
        targets: Vec<Target>,
        limit: usize,
        floor: f32,
    },
    Upsert {
        store: String,
        notes: Vec<NoteInput>,
    },
    Forget {
        store: String,
        ids: Vec<i64>,
    },
}

#[derive(Serialize)]
struct Framed<'a> {
    v: u32,
    #[serde(flatten)]
    request: &'a Request,
}

impl Request {
    /// The request line, without its newline.
    pub fn to_line(&self) -> String {
        serde_json::to_string(&Framed {
            v: VERSION,
            request: self,
        })
        .unwrap_or_default()
    }

    /// Reads a request line; the error is the reply text.
    pub fn parse(line: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(line).map_err(|_| "the request is not JSON".to_owned())?;

        if value.get("v").and_then(Value::as_u64) != Some(u64::from(VERSION)) {
            return Err(format!("unsupported protocol version (this embedder speaks {VERSION})"));
        }

        serde_json::from_value(value).map_err(|_| "unknown or malformed request".to_owned())
    }
}

/// One search result: a note of a store and its cosine to the query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorHit {
    pub store: String,
    pub id: i64,
    pub cosine: f32,
}

/// The reply line for a failure.
pub fn error_reply(message: &str) -> Value {
    json!({ "ok": false, "error": message })
}

/// What `ping` answers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Pong {
    /// The model that answers searches; `None` while none is loaded for the first time.
    pub model: Option<String>,
    /// Whether searches are answered.
    pub ready: bool,
    /// The model being switched to, while a switch runs.
    pub pending: Option<String>,
}

/// Any reply, read by a client.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub pending: Option<String>,
    #[serde(default)]
    pub hits: Vec<VectorHit>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_with_its_version() {
        let request = Request::Search {
            text: "bun install".to_owned(),
            targets: vec![Target {
                store: "local:ab".to_owned(),
                projects: vec![None, Some("git:github.com/a/b".to_owned())],
            }],
            limit: 20,
            floor: 0.3,
        };

        let line = request.to_line();
        assert!(line.contains("\"v\":1") && line.contains("\"op\":\"search\""), "{line}");
        assert_eq!(Request::parse(&line), Ok(request));
    }

    #[test]
    fn another_version_or_a_bad_line_is_refused() {
        assert!(Request::parse("{\"v\":2,\"op\":\"ping\"}").is_err());
        assert!(Request::parse("{\"op\":\"ping\"}").is_err());
        assert!(Request::parse("{\"v\":1,\"op\":\"shell\"}").is_err());
        assert!(Request::parse("nope").is_err());
        assert_eq!(Request::parse("{\"v\":1,\"op\":\"ping\"}"), Ok(Request::Ping));
    }
}
