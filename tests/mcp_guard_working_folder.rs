//! With no chat root, the server's working folder counts as a repository of every guarded call
//! (version 2 of `guards/global.json`): an agent that works in a folder saving is off for cannot
//! save by naming an enabled project in `project_path`. Both must be on. A working folder that is
//! no project (the home folder) adds nothing, and a chat root replaces the working folder.

mod common;

use std::path::Path;

use common::guards::{CHAT, Guarded, tool};
use common::mcp::{canonical, text};
use common::opt_in::{approval_in, global, home, kind, note, refusal, script};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

/// The seven write tools, each naming `project` in `project_path`.
fn writes(project: &Path) -> Vec<(&'static str, Value)> {
    let mut calls = vec![
        ("note_save", note("Named")),
        ("note_update", json!({ "id": 1, "body": "changed" })),
        ("note_delete", json!({ "id": 1 })),
        (
            "entity_link",
            json!({ "from": "cache", "relation": "located_in", "to": "src" }),
        ),
        ("entity_delete", json!({ "entity": "cache" })),
        ("script_save", script("named")),
        ("script_delete", json!({ "name": "named" })),
    ];

    for (_, arguments) in &mut calls {
        arguments["project_path"] = json!(project.to_string_lossy());
    }

    calls
}

#[test]
fn naming_an_enabled_project_from_a_folder_that_is_off_is_refused_by_every_write_tool() {
    let guarded = Guarded::new();
    let on = kind(false, false, &[&guarded.repo]);
    global(&guarded, &on, &on);
    let mut server = guarded.server(None, &guarded.work());

    for (name, arguments) in writes(&guarded.repo) {
        let reply = tool(&mut server, name, arguments);
        assert_eq!(refusal(&reply).as_deref(), Some("repo-off"), "{name}");
    }

    // Nothing was saved, so nothing is trusted.
    let mut named = json!({});
    named["project_path"] = json!(guarded.repo.to_string_lossy());
    let list = tool(&mut server, "script_list", named);
    assert!(!text(&list).contains("named"), "{}", text(&list));
}

#[test]
fn both_the_working_folder_and_the_named_project_on_saves_and_trusts() {
    let guarded = Guarded::new();
    let on = kind(false, false, &[&guarded.repo, &canonical(&guarded.work())]);
    global(&guarded, &on, &on);
    let mut server = guarded.server(None, &guarded.work());
    let project = json!({ "project_path": guarded.repo.to_string_lossy() });

    let mut note = note("Both on");
    note["project_path"] = json!(guarded.repo.to_string_lossy());
    assert_eq!(refusal(&tool(&mut server, "note_save", note)), None);

    let mut script = script("both");
    script["project_path"] = json!(guarded.repo.to_string_lossy());
    assert_eq!(refusal(&tool(&mut server, "script_save", script)), None);
    assert_eq!(approval_in(&mut server, "both", &project), "local");
}

#[test]
fn a_working_folder_that_is_no_project_adds_nothing() {
    let guarded = Guarded::new();
    let on = kind(false, false, &[&guarded.repo]);
    global(&guarded, &on, &on);
    let mut server = guarded.server(None, &home(&guarded));
    let project = json!({ "project_path": guarded.repo.to_string_lossy() });

    let mut note = note("From home");
    note["project_path"] = json!(guarded.repo.to_string_lossy());
    assert_eq!(refusal(&tool(&mut server, "note_save", note)), None);

    let mut script = script("home");
    script["project_path"] = json!(guarded.repo.to_string_lossy());
    assert_eq!(refusal(&tool(&mut server, "script_save", script)), None);
    assert_eq!(approval_in(&mut server, "home", &project), "local");
}

#[test]
fn the_chat_root_replaces_the_working_folder() {
    let guarded = Guarded::new();
    let on = kind(false, false, &[&guarded.repo]);
    global(&guarded, &on, &on);

    let chat = json!({ "version": 1, "paused": false, "repoRoot": guarded.repo.to_string_lossy(),
                       "notes": { "on": true, "ask": false }, "scripts": { "on": true, "ask": false } });
    guarded.chat(CHAT, &chat.to_string());
    let mut server = guarded.server(Some(CHAT), &guarded.work());

    let mut note = note("Chat root");
    note["project_path"] = json!(guarded.repo.to_string_lossy());
    assert_eq!(refusal(&tool(&mut server, "note_save", note)), None);
}
