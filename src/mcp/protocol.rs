//! JSON-RPC 2.0 framing over newline-delimited JSON, and the `initialize` handshake.

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

use super::tools::INSTRUCTIONS;

/// Protocol versions this server speaks; a client asking for another gets the first.
pub const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// `serverInfo.name`.
pub const SERVER_NAME: &str = "harness-memory";

/// JSON-RPC error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;

/// One input line.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    /// A complete line without its newline.
    Text(Vec<u8>),
    /// A line longer than the limit; it was skipped up to its newline.
    TooLong,
    /// End of input.
    End,
}

/// Reads one line, keeping at most `max` bytes. A longer line is consumed up to and including its
/// newline and reported as [`Line::TooLong`], so reading resumes at the next message.
pub fn read_line(reader: &mut impl BufRead, buffer: &mut Vec<u8>, max: usize) -> io::Result<Line> {
    buffer.clear();
    let mut too_long = false;
    let mut read_any = false;

    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };

        if available.is_empty() {
            return Ok(match (read_any, too_long) {
                (false, _) => Line::End,
                (true, true) => Line::TooLong,
                (true, false) => Line::Text(std::mem::take(buffer)),
            });
        }

        read_any = true;
        let newline = available.iter().position(|&byte| byte == b'\n');
        let chunk = &available[..newline.unwrap_or(available.len())];

        if !too_long && buffer.len() + chunk.len() <= max {
            buffer.extend_from_slice(chunk);
        } else {
            too_long = true;
            buffer.clear();
        }

        let consumed = newline.map_or(available.len(), |at| at + 1);
        reader.consume(consumed);

        if newline.is_some() {
            if too_long {
                return Ok(Line::TooLong);
            }

            if buffer.last() == Some(&b'\r') {
                buffer.pop();
            }

            return Ok(Line::Text(std::mem::take(buffer)));
        }
    }
}

/// Writes one message and flushes it.
pub fn write_message(writer: &mut impl Write, message: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, message)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// A success response.
pub fn result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// An error response.
pub fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The `initialize` result: the client's protocol version when supported, else the newest.
pub fn initialize_result(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .filter(|asked| PROTOCOL_VERSIONS.contains(asked))
        .unwrap_or(PROTOCOL_VERSIONS[0]);

    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": SERVER_NAME, "version": env!("HARNESS_MEMORY_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}
