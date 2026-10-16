//! An in-process MCP server over byte buffers, and git repositories to start it in.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use harness_memory::project::ProjectResolver;
use harness_memory::store::SqliteStore;
use harness_memory::support::Env;
use harness_memory::{Server, serve};
use serde_json::{Value, json};
use tempfile::TempDir;

/// The path separator as a quoted reply field shows it under `~`: `/` on Unix, a JSON-escaped `\`
/// on Windows.
pub const SEP: &str = if cfg!(windows) { r"\\" } else { "/" };

/// `path` canonicalized the way the server keys and shows folders (no `\\?\` prefix on Windows).
pub fn canonical(path: &Path) -> PathBuf {
    dunce::canonicalize(path).expect("canonical")
}

/// `path` as a quoted reply field shows it: a JSON string, so Windows backslashes are doubled.
pub fn quoted(path: &Path) -> String {
    serde_json::to_string(&path.display().to_string()).expect("json")
}

/// An environment with `home` as the home folder and the real `PATH` (for git).
pub fn env(home: &Path) -> Env {
    Env::for_tests(home).with_var("PATH", std::env::var("PATH").unwrap_or_default())
}

/// A server on `dir/memory.sqlite3` whose working directory is `cwd` and whose home is `home`.
pub fn server(dir: &TempDir, home: &Path, cwd: &Path) -> Server {
    let store = SqliteStore::open(&dir.path().join("memory.sqlite3")).expect("store");
    Server::new(
        Box::new(store),
        ProjectResolver::new(env(home), Some(cwd.to_path_buf())),
    )
}

/// Serves `input` and parses every reply line.
pub fn run_raw(server: &mut Server, input: &[u8]) -> Vec<Value> {
    let mut output = Vec::new();
    serve(Cursor::new(input.to_vec()), &mut output, server).expect("serve");

    String::from_utf8(output)
        .expect("utf-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("each reply is JSON"))
        .collect()
}

pub fn run(server: &mut Server, messages: &[Value]) -> Vec<Value> {
    let input: String = messages.iter().map(|message| format!("{message}\n")).collect();
    run_raw(server, input.as_bytes())
}

pub fn call(id: i64, name: &str, arguments: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": arguments } })
}

pub fn text(reply: &Value) -> &str {
    reply["result"]["content"][0]["text"].as_str().expect("text content")
}

pub fn is_error(reply: &Value) -> bool {
    reply["result"]["isError"].as_bool().expect("isError")
}

/// Runs `git args` in `dir`, ignoring the system git config.
pub fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

/// A new repository `parent/name` (with a `src/deep` folder) whose `origin` is `remote`.
pub fn repo_with_remote(parent: &Path, name: &str, remote: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(dir.join("src/deep")).expect("mkdir");
    git(&dir, &["init", "-q"]);
    git(&dir, &["remote", "add", "origin", remote]);
    dir
}
