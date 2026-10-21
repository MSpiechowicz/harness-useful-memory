//! A repository turned off for agents' saves (`offRepos`) is matched by its project key as well as
//! its folder: another clone and a git worktree of it are turned off too, a repository with
//! another remote is not.

mod common;

use common::guards::{CHAT, Guarded, structured, tool};
use common::mcp::{canonical, git, is_error, repo_with_remote};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

fn note(title: &str) -> Value {
    json!({ "title": title, "body": "The cache lives in src/cache.rs", "kind": "location" })
}

/// The reason of a refusal, `None` for a save.
fn refusal(reply: &Value) -> Option<String> {
    assert!(!is_error(reply), "{reply}");
    structured(reply)["reason"].as_str().map(str::to_owned)
}

#[test]
fn clones_and_worktrees_of_a_repository_turned_off_are_off() {
    let guarded = Guarded::new();
    let root = guarded.repo.to_string_lossy().into_owned();
    guarded.global(
        &json!({ "version": 1, "notes": { "paused": false, "offRepos": [root] },
                 "scripts": { "paused": false, "offRepos": [] } })
        .to_string(),
    );
    guarded.rules(false, (true, false), (true, false));

    let parent = guarded.dir.path();
    let clone = canonical(&repo_with_remote(parent, "clone", "git@github.com:acme/widget.git"));
    let other = canonical(&repo_with_remote(parent, "other", "https://github.com/acme/gadget.git"));

    let identity = ["-c", "user.name=Tests", "-c", "user.email=tests@example.com"];
    git(
        &guarded.repo,
        &[&identity[..], &["commit", "-q", "--allow-empty", "-m", "start"]].concat(),
    );
    git(&guarded.repo, &["worktree", "add", "-q", "../worktree"]);
    let worktree = canonical(&parent.join("worktree"));

    for folder in [&clone, &worktree] {
        let mut server = guarded.server(Some(CHAT), folder);
        let reply = tool(&mut server, "note_save", note("From a copy"));
        assert_eq!(refusal(&reply).as_deref(), Some("repo-off"), "{}", folder.display());

        // Named through project_path from outside, and as the chat's own root, it is off too.
        let mut outside = guarded.server(Some(CHAT), &guarded.work());
        let mut named = note("Named copy");
        named["project_path"] = json!(folder.to_string_lossy());
        assert_eq!(
            refusal(&tool(&mut outside, "note_save", named)).as_deref(),
            Some("repo-off")
        );
    }

    let text = json!({ "version": 1, "paused": false, "repoRoot": worktree.to_string_lossy(),
                       "notes": { "on": true, "ask": false }, "scripts": { "on": true, "ask": false } });
    guarded.chat(CHAT, &text.to_string());
    let mut outside = guarded.server(Some(CHAT), &guarded.work());
    let mut global = note("Global from the worktree");
    global["scope"] = json!("global");
    assert_eq!(
        refusal(&tool(&mut outside, "note_save", global)).as_deref(),
        Some("repo-off")
    );

    // Another repository, and scripts (not turned off), still save.
    guarded.rules(false, (true, false), (true, false));
    let mut elsewhere = guarded.server(Some(CHAT), &other);
    assert_eq!(refusal(&tool(&mut elsewhere, "note_save", note("Other repo"))), None);

    let mut in_clone = guarded.server(Some(CHAT), &clone);
    let script = json!({ "name": "hi", "description": "", "language": "sh", "body": "echo hi" });
    assert_eq!(refusal(&tool(&mut in_clone, "script_save", script)), None);
}
