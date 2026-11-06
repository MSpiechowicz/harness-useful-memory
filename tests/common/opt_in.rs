//! Helpers of the opt-in guard tests: the rules of `guards/global.json` of version 2, the saves
//! and scripts they are tried with, and the answer of a save.

use std::path::{Path, PathBuf};

use harness_memory::Server;
use harness_memory::project::ProjectResolver;
use serde_json::{Value, json};

use super::guards::{Guarded, structured, tool};
use super::mcp::{canonical, git, is_error};

pub fn note(title: &str) -> Value {
    json!({ "title": title, "body": "The cache lives in src/cache.rs", "kind": "location" })
}

pub fn global_note(title: &str) -> Value {
    let mut note = note(title);
    note["scope"] = json!("global");
    note
}

pub fn script(name: &str) -> Value {
    json!({ "name": name, "description": "Says hi", "language": "sh", "body": "echo hi" })
}

/// The rules of one kind in `guards/global.json`.
pub fn kind(paused: bool, every_project: bool, on_repos: &[&Path]) -> Value {
    let on_repos: Vec<String> = on_repos
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    json!({ "paused": paused, "everyProject": every_project, "onRepos": on_repos })
}

pub fn off() -> Value {
    kind(false, false, &[])
}

/// Writes `guards/global.json` of version 2. A kind from [`kind`] is pinned: each entry gets the
/// project key its folder has now (the way Harness Desktop pins it when the user turns it on), and
/// none when the folder cannot be resolved. A kind that has its own `repoKeys` is written as it is.
pub fn global(guarded: &Guarded, notes: &Value, scripts: &Value) {
    let (notes, scripts) = (pinned(guarded, notes), pinned(guarded, scripts));
    guarded.global(&json!({ "version": 2, "notes": notes, "scripts": scripts }).to_string());
}

/// `kind` with the `repoKeys` of its entries now (see [`global`]); as it is when it has them.
pub fn pinned(guarded: &Guarded, kind: &Value) -> Value {
    let mut kind = kind.clone();

    if kind.get("repoKeys").is_some() {
        return kind;
    }

    let mut resolver = ProjectResolver::new(guarded.env(None), None);
    let mut keys = serde_json::Map::new();

    for entry in kind["onRepos"].as_array().cloned().unwrap_or_default() {
        let Some(path) = entry.as_str() else { continue };

        if let Ok(project) = resolver.resolve(Some(path)) {
            keys.insert(path.to_owned(), json!(project.map(|project| project.key)));
        }
    }

    kind["repoKeys"] = Value::Object(keys);
    kind
}

/// `kind` with these pins instead: each entry path and the key it had when it was turned on.
pub fn with_pins(kind: &Value, pins: &[(&Path, Option<&str>)]) -> Value {
    let keys: serde_json::Map<String, Value> = pins
        .iter()
        .map(|(path, key)| (path.to_string_lossy().into_owned(), json!(key)))
        .collect();
    let mut kind = kind.clone();
    kind["repoKeys"] = Value::Object(keys);
    kind
}

/// The reason of a refusal, `None` for a save.
pub fn refusal(reply: &Value) -> Option<String> {
    assert!(!is_error(reply), "{reply}");
    let structured = structured(reply);

    if structured.get("saved") == Some(&json!(false)) {
        return structured["reason"].as_str().map(str::to_owned);
    }

    assert!(structured.get("held").is_none(), "{reply}");
    None
}

/// The approval `script_get` shows for script `name`.
pub fn approval(server: &mut Server, name: &str) -> String {
    approval_in(server, name, &json!({}))
}

/// The approval `script_get` shows for script `name`, with `extra` added to its arguments (for a
/// `project_path`).
pub fn approval_in(server: &mut Server, name: &str, extra: &Value) -> String {
    let mut arguments = json!({ "name": name });
    arguments["project_path"] = extra["project_path"].clone();
    let got = tool(server, "script_get", arguments);
    structured(&got)["script"]["approval"]
        .as_str()
        .expect("approval")
        .to_owned()
}

/// A commit in `repo`, so it can have worktrees.
pub fn commit(repo: &Path) {
    let identity = ["-c", "user.name=Tests", "-c", "user.email=tests@example.com"];
    git(
        repo,
        &[&identity[..], &["commit", "-q", "--allow-empty", "-m", "start"]].concat(),
    );
}

/// A linked worktree `name` next to `repo`.
pub fn worktree(repo: &Path, name: &str) -> PathBuf {
    git(repo, &["worktree", "add", "-q", &format!("../{name}")]);
    canonical(&repo.parent().expect("parent").join(name))
}

/// The home folder [`Guarded`] gives servers: no repository.
pub fn home(guarded: &Guarded) -> PathBuf {
    canonical(&guarded.dir.path().join("home"))
}
