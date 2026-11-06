//! A chat or an agent whose project path goes through a link (a linked parent such as
//! `~/Projekty -> /data/projects`) is in the repository the user turned on: the entry is the
//! canonical target, and the chat's root, the working folder and the `project_path` of a call
//! all resolve to it. Harness Desktop tests its side (`linked_parent.rs` in
//! `business/src/ecosystem/memory_guards/tests` there): the UI's root and the hooks' state.

#![cfg(unix)]

mod common;

use std::os::unix::fs::symlink;
use std::path::PathBuf;

use common::guards::{CHAT, Guarded, tool};
use common::mcp::{canonical, repo_with_remote};
use common::opt_in::{approval_in, global, kind, note, refusal, script};
use pretty_assertions::assert_eq;
use serde_json::json;

/// The project `real/proj` that the user turned on, and the path to it through the link
/// `linked -> real`.
struct Linked {
    guarded: Guarded,
    project: PathBuf,
    through_link: PathBuf,
}

impl Linked {
    fn new() -> Self {
        let guarded = Guarded::new();
        let base = canonical(guarded.dir.path());
        let real = base.join("real");
        std::fs::create_dir_all(&real).expect("mkdir");

        let project = canonical(&repo_with_remote(&real, "proj", "https://github.com/acme/proj.git"));
        symlink(&real, base.join("linked")).expect("link");
        let through_link = base.join("linked").join("proj");

        let on = kind(false, false, &[&project]);
        global(&guarded, &on, &on);

        Self {
            guarded,
            project,
            through_link,
        }
    }
}

#[test]
fn a_working_folder_and_a_project_path_through_a_linked_parent_are_on() {
    let linked = Linked::new();
    let mut server = linked.guarded.server(None, &linked.through_link);

    // The working folder is on, and so is a project path spelled through the link.
    let mut by_cwd = note("By the working folder");
    by_cwd["project_path"] = json!(linked.through_link.to_string_lossy());
    assert_eq!(refusal(&tool(&mut server, "note_save", by_cwd)), None);

    let mut saved = script("through-link");
    saved["project_path"] = json!(linked.through_link.join("src").to_string_lossy());
    assert_eq!(refusal(&tool(&mut server, "script_save", saved)), None);

    // The script is trusted in the project.
    let canonical_path = json!({ "project_path": linked.project.to_string_lossy() });
    assert_eq!(approval_in(&mut server, "through-link", &canonical_path), "local");

    // Nothing else is on: a sibling of the project is refused.
    let mut sibling = note("A sibling");
    sibling["project_path"] = json!(linked.guarded.repo.to_string_lossy());
    assert_eq!(
        refusal(&tool(&mut server, "note_save", sibling)).as_deref(),
        Some("repo-off")
    );
}

#[test]
fn a_chat_rooted_through_a_linked_parent_is_on() {
    let linked = Linked::new();
    let root = json!({ "version": 1, "paused": false, "repoRoot": linked.through_link.to_string_lossy(),
                       "notes": { "on": true, "ask": false }, "scripts": { "on": true, "ask": false } });
    linked.guarded.chat(CHAT, &root.to_string());
    let mut server = linked.guarded.server(Some(CHAT), &linked.through_link);

    let mut saved = note("By the chat root");
    saved["project_path"] = json!(linked.through_link.to_string_lossy());
    assert_eq!(refusal(&tool(&mut server, "note_save", saved)), None);

    // Without a project path the chat's own root decides.
    assert_eq!(refusal(&tool(&mut server, "note_save", note("Root alone"))), None);
}
