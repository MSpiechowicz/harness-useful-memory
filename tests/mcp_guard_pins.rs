//! An entry of `onRepos` is on only while the repository still has the project key it had when the
//! user turned it on (`repoKeys`). A remote that changed afterwards (a `git remote set-url`, a
//! fresh clone at the same path, an upstream change that landed in `.git/config`) would send the
//! writes to the notes of another project, such as one the user left off, so the repository is
//! refused until the user turns it on again. `everyProject` needs no pin, an entry with no pin
//! names nothing, and a file from before the pins is read as having none.

mod common;

use std::path::{Path, PathBuf};

use common::guards::{CHAT, Guarded, tool};
use common::mcp::{canonical, git, repo_with_remote};
use common::opt_in::{commit, global, home, kind, note, off, refusal, script, with_pins, worktree};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

const OFF_URL: &str = "https://example.invalid/acme/off.git";
const WIDGET_KEY: &str = "git:github.com/acme/widget";
const OFF_KEY: &str = "git:example.invalid/acme/off";

fn saves(guarded: &Guarded, folder: &Path, title: &str) -> Option<String> {
    let mut server = guarded.server(None, folder);
    let mut saved = note(title);
    saved["project_path"] = json!(folder.to_string_lossy());
    refusal(&tool(&mut server, "note_save", saved))
}

/// A repository `off` with a remote [`OFF_URL`] that holds a note and a script.
fn off_repository(guarded: &Guarded) -> PathBuf {
    let off = canonical(&repo_with_remote(guarded.dir.path(), "off", OFF_URL));
    let on = kind(false, false, &[&off]);
    global(guarded, &on, &on);

    let mut server = guarded.server(None, &off);
    let mut saved = note("Seeded");
    saved["project_path"] = json!(off.to_string_lossy());
    assert_eq!(refusal(&tool(&mut server, "note_save", saved)), None);

    let mut saved = script("named");
    saved["project_path"] = json!(off.to_string_lossy());
    assert_eq!(refusal(&tool(&mut server, "script_save", saved)), None);
    off
}

/// What `off` holds as the user would read it.
fn held_by_off(guarded: &Guarded, off: &Path) -> Vec<String> {
    let mut reader = guarded.server(None, &guarded.repo);
    let at = json!(off.to_string_lossy());
    let reads = [
        ("note_get", json!({ "ids": [1], "scope": "repo", "project_path": at })),
        ("script_list", json!({ "scope": "repo", "project_path": at })),
    ];

    reads
        .into_iter()
        .map(|(name, arguments)| common::mcp::text(&tool(&mut reader, name, arguments)).to_owned())
        .collect()
}

/// The write tools naming `folder`, called by a server that lets the chat's repository save.
fn attack(guarded: &Guarded, folder: &Path) -> Vec<Option<String>> {
    let root = json!({ "version": 1, "paused": false, "repoRoot": guarded.repo.to_string_lossy(),
                       "notes": { "on": true, "ask": false }, "scripts": { "on": true, "ask": false } });
    guarded.chat(CHAT, &root.to_string());
    let mut server = guarded.server(Some(CHAT), &guarded.repo);
    let at = json!(folder.to_string_lossy());

    let mut saved_note = note("Through the entry");
    saved_note["project_path"] = at.clone();
    let mut saved_script = script("named");
    saved_script["project_path"] = at.clone();

    let calls: Vec<(&str, Value)> = vec![
        ("note_save", saved_note),
        (
            "note_update",
            json!({ "id": 1, "body": "OVERWRITTEN through the entry", "project_path": at }),
        ),
        ("note_delete", json!({ "id": 1, "project_path": at })),
        ("script_save", saved_script),
        ("script_delete", json!({ "name": "named", "project_path": at })),
    ];

    calls
        .into_iter()
        .map(|(name, arguments)| refusal(&tool(&mut server, name, arguments)))
        .collect()
}

#[test]
fn a_remote_that_changed_after_the_entry_was_turned_on_stops_saving_until_it_is_turned_on_again() {
    let guarded = Guarded::new();
    let repo = guarded.repo.clone();
    global(&guarded, &kind(false, false, &[&repo]), &kind(false, false, &[&repo]));
    assert_eq!(saves(&guarded, &repo, "Before"), None);

    git(
        &repo,
        &["remote", "set-url", "origin", "https://example.invalid/acme/other.git"],
    );
    assert_eq!(saves(&guarded, &repo, "After").as_deref(), Some("repo-off"));

    // A folder inside it is the same project, and so is a server started in it.
    let deep = repo.join("src/deep");
    assert_eq!(saves(&guarded, &deep, "Deep").as_deref(), Some("repo-off"));

    // The user turns it on again: the entry gets the key it has now.
    global(&guarded, &kind(false, false, &[&repo]), &kind(false, false, &[&repo]));
    assert_eq!(saves(&guarded, &repo, "Again"), None);
}

#[test]
fn a_nested_clone_that_is_not_a_submodule_cannot_be_pointed_at_a_repository_that_is_off() {
    let guarded = Guarded::new();
    let off = off_repository(&guarded);
    let before = held_by_off(&guarded, &off);

    // A nested clone that nothing tracks, turned on while its remote was another repository.
    let nested = canonical(&repo_with_remote(
        &guarded.repo,
        "nested",
        "https://example.invalid/acme/lib.git",
    ));
    global(
        &guarded,
        &kind(false, false, &[&guarded.repo, &nested]),
        &kind(false, false, &[&guarded.repo, &nested]),
    );
    assert_eq!(saves(&guarded, &nested, "Before the change"), None);

    // The sync flow: its `origin` is rewritten to the repository that is off.
    git(&nested, &["remote", "set-url", "origin", OFF_URL]);
    assert_eq!(attack(&guarded, &nested), vec![Some("repo-off".to_owned()); 5]);
    assert_eq!(held_by_off(&guarded, &off), before);

    // The fresh-clone flow: the folder is cloned afresh from the repository that is off.
    std::fs::remove_dir_all(&nested).expect("remove");
    repo_with_remote(&guarded.repo, "nested", OFF_URL);
    assert_eq!(attack(&guarded, &nested), vec![Some("repo-off".to_owned()); 5]);
    assert_eq!(held_by_off(&guarded, &off), before);
}

#[test]
fn a_chat_root_whose_project_changed_stops_the_call_that_names_another_listed_repository() {
    let guarded = Guarded::new();
    let other = canonical(&repo_with_remote(
        guarded.dir.path(),
        "other",
        "https://example.invalid/acme/other.git",
    ));
    global(
        &guarded,
        &kind(false, false, &[&guarded.repo, &other]),
        &kind(false, false, &[&guarded.repo, &other]),
    );

    let root = json!({ "version": 1, "paused": false, "repoRoot": guarded.repo.to_string_lossy(),
                       "notes": { "on": true, "ask": false }, "scripts": { "on": true, "ask": false } });
    guarded.chat(CHAT, &root.to_string());
    let call = |title: &str| {
        let mut server = guarded.server(Some(CHAT), &guarded.repo);
        let mut saved = note(title);
        saved["project_path"] = json!(other.to_string_lossy());
        refusal(&tool(&mut server, "note_save", saved))
    };
    assert_eq!(call("Both unchanged"), None);

    // The call's own repository changed: refused.
    git(&other, &["remote", "set-url", "origin", OFF_URL]);
    assert_eq!(call("Named repository changed").as_deref(), Some("repo-off"));

    // The chat's root changed instead: refused as well.
    git(
        &other,
        &["remote", "set-url", "origin", "https://example.invalid/acme/other.git"],
    );
    assert_eq!(call("Back"), None);
    git(&guarded.repo, &["remote", "set-url", "origin", OFF_URL]);
    assert_eq!(call("Chat root changed").as_deref(), Some("repo-off"));
}

#[test]
fn a_worktree_of_a_pinned_repository_saves_with_that_key() {
    let guarded = Guarded::new();
    commit(&guarded.repo);
    let linked = worktree(&guarded.repo, "linked");
    global(&guarded, &kind(false, false, &[&guarded.repo]), &off());

    let pinned = with_pins(
        &kind(false, false, &[&guarded.repo]),
        &[(&guarded.repo, Some(WIDGET_KEY))],
    );
    global(&guarded, &pinned, &off());
    assert_eq!(saves(&guarded, &linked, "Worktree"), None);

    // Pinned to another key: the worktree is refused with the repository.
    let pinned = with_pins(&kind(false, false, &[&guarded.repo]), &[(&guarded.repo, Some(OFF_KEY))]);
    global(&guarded, &pinned, &off());
    assert_eq!(saves(&guarded, &linked, "Worktree").as_deref(), Some("repo-off"));
    assert_eq!(saves(&guarded, &guarded.repo, "Main").as_deref(), Some("repo-off"));
}

#[test]
fn a_repository_with_no_remote_and_its_worktree_match_the_path_key_they_were_turned_on_with() {
    let guarded = Guarded::new();
    std::fs::create_dir_all(guarded.dir.path().join("plain")).expect("mkdir");
    let plain = canonical(&guarded.dir.path().join("plain"));
    git(&plain, &["init", "-q"]);
    commit(&plain);
    let linked = worktree(&plain, "plain-linked");

    global(&guarded, &kind(false, false, &[&plain]), &off());
    assert_eq!(saves(&guarded, &plain, "Main"), None);
    assert_eq!(saves(&guarded, &linked, "Worktree"), None);

    // A remote that appeared is another key.
    git(&plain, &["remote", "add", "origin", OFF_URL]);
    assert_eq!(saves(&guarded, &plain, "Remote added").as_deref(), Some("repo-off"));
}

#[test]
fn a_null_pin_is_for_the_chat_root_with_no_project_and_only_for_it() {
    let guarded = Guarded::new();
    let home = home(&guarded);

    // A chat started at the home folder: its root has no project key.
    let root = json!({ "version": 1, "paused": false, "repoRoot": home.to_string_lossy(),
                       "notes": { "on": true, "ask": false }, "scripts": { "on": true, "ask": false } });
    guarded.chat(CHAT, &root.to_string());
    let from_home = || {
        let mut server = guarded.server(Some(CHAT), &home);
        refusal(&tool(
            &mut server,
            "note_save",
            common::opt_in::global_note("From home"),
        ))
    };

    // Turned on with the key `null` it had.
    let on = kind(false, false, &[&home]);
    global(&guarded, &on, &on);
    assert_eq!(from_home(), None);

    // Pinned with a key, or with none: refused.
    for pins in [vec![(home.as_path(), Some(WIDGET_KEY))], vec![]] {
        let pinned = with_pins(&on, &pins);
        global(&guarded, &pinned, &pinned);
        assert_eq!(from_home().as_deref(), Some("repo-off"), "{pins:?}");
    }

    // A repository pinned `null` is refused too: it has a key.
    let pinned = with_pins(&kind(false, false, &[&guarded.repo]), &[(&guarded.repo, None)]);
    global(&guarded, &pinned, &pinned);
    assert_eq!(
        saves(&guarded, &guarded.repo, "Pinned null").as_deref(),
        Some("repo-off")
    );
}

#[test]
fn an_entry_with_no_pin_names_nothing_and_every_project_needs_none() {
    let guarded = Guarded::new();
    let repo = guarded.repo.clone();

    // A file from before the pins has no `repoKeys` at all: it is read, and nothing is on.
    let plain = kind(false, false, &[&repo]);
    guarded.global(&json!({ "version": 2, "notes": plain, "scripts": plain }).to_string());
    assert_eq!(saves(&guarded, &repo, "No pin").as_deref(), Some("repo-off"));

    // An object with other entries only: still no pin for this one.
    let other = with_pins(&plain, &[(Path::new("/elsewhere"), Some(WIDGET_KEY))]);
    global(&guarded, &other, &other);
    assert_eq!(saves(&guarded, &repo, "Other pin").as_deref(), Some("repo-off"));

    // Every project saves without pins, whatever the keys are.
    let every = kind(false, true, &[]);
    guarded.global(&json!({ "version": 2, "notes": every, "scripts": every }).to_string());
    assert_eq!(saves(&guarded, &repo, "Every project"), None);
    git(&repo, &["remote", "set-url", "origin", OFF_URL]);
    assert_eq!(saves(&guarded, &repo, "Every project changed"), None);

    // A pin that is not text or null is not the shape of the file: it cannot be read.
    let mut broken = plain.clone();
    broken["repoKeys"] = json!({ repo.to_string_lossy(): 3 });
    guarded.global(&json!({ "version": 2, "notes": broken, "scripts": broken }).to_string());
    assert_eq!(
        saves(&guarded, &repo, "Unreadable").as_deref(),
        Some("guard-unreadable")
    );
}
