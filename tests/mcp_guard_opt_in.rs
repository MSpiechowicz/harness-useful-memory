//! `guards/global.json` of version 2: saving is off until the user turns it on, per kind, for
//! every project or for the repositories listed (`onRepos`). A linked git worktree counts as its
//! main repository, another clone and a `.git` file that only claims a worktree do not. Reads,
//! recall and `script_run` stay open, user-only saves bypass the guards, and an unknown version
//! refuses.

mod common;

use std::path::Path;

use common::guards::{CHAT, Guarded, private_dir, private_file, structured, tool};
use common::mcp::{canonical, is_error, repo_with_remote, text};
use common::opt_in::{approval, commit, global, global_note, home, kind, note, off, refusal, script, worktree};
use pretty_assertions::assert_eq;
use serde_json::json;

const NOTES_OFF: &str = "Saving notes is off for this project. The user can turn it on in Harness Desktop.";

const NOTES_NOTICE: &str = "Saving notes is off for this project. The user can turn it on in Harness Desktop. Reading \
                            works. Do not try to save notes.";

const SCRIPTS_NOTICE: &str = "Saving scripts is off for this project. The user can turn it on in Harness Desktop. \
                              Reading works. Do not try to save scripts.";

#[test]
fn without_a_global_file_saving_is_allowed() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);

    assert_eq!(refusal(&tool(&mut server, "note_save", note("No file"))), None);
    assert_eq!(refusal(&tool(&mut server, "script_save", script("hello"))), None);
}

/// Writes the marker Harness Desktop writes before `guards/global.json`.
fn marker(guarded: &Guarded) {
    let guards = guarded.data().join("guards");
    private_dir(&guards);
    private_file(&guards.join("opt-in"), r#"{"version":1}"#);
}

#[test]
fn a_missing_global_file_refuses_once_the_desktop_left_its_marker() {
    let guarded = Guarded::new();
    marker(&guarded);

    // The marker alone refuses every write, with a chat and without one, and trusts nothing.
    for session in [None, Some(CHAT)] {
        if session.is_some() {
            guarded.rules(false, (true, false), (true, false));
        }

        let mut server = guarded.server(session, &guarded.repo);

        for (name, arguments) in [("note_save", note("Gone")), ("script_save", script("gone"))] {
            let reply = tool(&mut server, name, arguments);
            assert_eq!(
                refusal(&reply).as_deref(),
                Some("guard-unreadable"),
                "{name} {session:?}"
            );
        }

        let list = tool(&mut server, "script_list", json!({}));
        assert!(!text(&list).contains("gone"), "{}", text(&list));
    }

    // The file written again, the desktop's rules apply.
    let on = kind(false, true, &[]);
    global(&guarded, &on, &on);
    let mut server = guarded.server(None, &guarded.repo);
    assert_eq!(refusal(&tool(&mut server, "note_save", note("Back"))), None);
}

#[test]
fn a_marker_of_any_kind_counts_and_no_marker_keeps_saving() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);

    // An older desktop, or none: no marker and no file saves.
    assert_eq!(refusal(&tool(&mut server, "note_save", note("Before"))), None);

    // A folder where the marker belongs is still a marker.
    private_dir(&guarded.data().join("guards").join("opt-in"));
    assert_eq!(
        refusal(&tool(&mut server, "note_save", note("After"))).as_deref(),
        Some("guard-unreadable")
    );
}

#[test]
fn saving_is_off_by_default_and_every_write_is_refused_but_reads_and_runs_work() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);

    // What exists before the user's rules.
    assert_eq!(refusal(&tool(&mut server, "note_save", note("Cache location"))), None);
    assert_eq!(refusal(&tool(&mut server, "script_save", script("hello"))), None);

    global(&guarded, &off(), &off());

    let refused = tool(&mut server, "note_save", note("Another note"));
    assert_eq!(refusal(&refused).as_deref(), Some("repo-off"));
    assert_eq!(
        text(&refused),
        format!("{NOTES_OFF} Nothing was saved. Do not retry. Continue without saving.")
    );

    for (name, arguments) in [
        ("note_update", json!({ "id": 1, "body": "changed" })),
        ("note_delete", json!({ "id": 1 })),
        (
            "entity_link",
            json!({ "from": "cache", "relation": "located_in", "to": "src" }),
        ),
        ("entity_delete", json!({ "entity": "cache" })),
        ("script_save", script("other")),
        ("script_delete", json!({ "name": "hello" })),
    ] {
        let reply = tool(&mut server, name, arguments);
        assert_eq!(refusal(&reply).as_deref(), Some("repo-off"), "{name}");
    }

    let deleted = tool(&mut server, "note_delete", json!({ "id": 1 }));
    assert_eq!(
        text(&deleted),
        format!("{NOTES_OFF} Nothing was deleted. Do not retry. Continue with your task.")
    );

    // Nothing changed, and reads say why saving is off.
    let search = tool(&mut server, "note_search", json!({ "query": "cache" }));
    assert!(!is_error(&search), "{search}");
    assert!(text(&search).contains("Cache location"), "{}", text(&search));
    assert!(text(&search).ends_with(NOTES_NOTICE), "{}", text(&search));

    let list = tool(&mut server, "script_list", json!({}));
    assert!(!is_error(&list), "{list}");
    assert!(text(&list).contains("hello"), "{}", text(&list));
    assert!(text(&list).ends_with(SCRIPTS_NOTICE), "{}", text(&list));
    assert_eq!(
        structured(&list)["saving"],
        json!({ "allowed": false, "reason": "repo-off" })
    );
    assert!(!text(&list).contains("other"), "{}", text(&list));

    // `sh` runs the script on Unix, and it stays trusted.
    #[cfg(unix)]
    {
        let ran = tool(&mut server, "script_run", json!({ "name": "hello" }));
        assert!(!is_error(&ran), "{ran}");
        assert_eq!(structured(&ran)["stdout"], "hi\n");
    }
}

#[test]
fn a_repository_is_turned_on_for_one_kind_only() {
    let guarded = Guarded::new();
    global(&guarded, &kind(false, false, &[&guarded.repo]), &off());
    let mut server = guarded.server(None, &guarded.repo);

    assert_eq!(refusal(&tool(&mut server, "note_save", note("Allowed"))), None);
    assert_eq!(
        refusal(&tool(&mut server, "script_save", script("hello"))).as_deref(),
        Some("repo-off")
    );

    let search = tool(&mut server, "note_search", json!({ "query": "cache" }));
    assert!(!text(&search).contains("Saving notes"), "{}", text(&search));
    let list = tool(&mut server, "script_list", json!({}));
    assert!(text(&list).ends_with(SCRIPTS_NOTICE), "{}", text(&list));

    // Scripts on and notes off: the script saves, but nothing an agent saves is trusted.
    global(&guarded, &off(), &kind(false, false, &[&guarded.repo]));
    assert_eq!(refusal(&tool(&mut server, "script_save", script("hello"))), None);
    assert_eq!(approval(&mut server, "hello"), "needed");
    assert_eq!(
        refusal(&tool(&mut server, "note_save", note("Refused"))).as_deref(),
        Some("repo-off")
    );

    // Both on: scripts are trusted as before.
    let on = kind(false, false, &[&guarded.repo]);
    global(&guarded, &on, &on);
    let mut again = script("again");
    again["body"] = json!("echo again");
    assert_eq!(refusal(&tool(&mut server, "script_save", again)), None);
    assert_eq!(approval(&mut server, "again"), "local");
}

#[test]
fn a_worktree_is_its_main_repository_and_a_clone_or_a_forged_file_is_not() {
    let guarded = Guarded::new();
    let parent = guarded.dir.path();
    commit(&guarded.repo);
    let linked = worktree(&guarded.repo, "linked");

    let clone = canonical(&repo_with_remote(parent, "clone", "https://github.com/acme/widget.git"));

    let other = canonical(&repo_with_remote(parent, "other", "https://github.com/acme/gadget.git"));
    commit(&other);
    let other_linked = worktree(&other, "other-linked");

    // A plain folder whose `.git` file claims the enabled repository's worktree.
    let forged = parent.join("forged");
    std::fs::create_dir_all(&forged).expect("mkdir");
    let admin = guarded.repo.join(".git/worktrees/linked");
    std::fs::write(forged.join(".git"), format!("gitdir: {}\n", admin.display())).expect("forge");
    let forged = canonical(&forged);

    global(&guarded, &kind(false, false, &[&guarded.repo]), &off());

    let saves = |folder: &Path, title: &str| {
        let mut server = guarded.server(None, folder);
        refusal(&tool(&mut server, "note_save", note(title)))
    };

    assert_eq!(saves(&guarded.repo, "Main"), None);
    assert_eq!(saves(&linked, "Linked"), None);
    assert_eq!(saves(&clone, "Clone").as_deref(), Some("repo-off"));
    assert_eq!(saves(&other, "Other").as_deref(), Some("repo-off"));
    assert_eq!(saves(&other_linked, "Other linked").as_deref(), Some("repo-off"));
    assert_eq!(saves(&forged, "Forged").as_deref(), Some("repo-off"));

    // Named by project_path from outside (the home folder, which is no project), the same.
    let mut outside = guarded.server(None, &home(&guarded));

    for (folder, allowed) in [
        (&linked, true),
        (&clone, false),
        (&forged, false),
        (&other_linked, false),
    ] {
        let mut named = note("Named");
        named["project_path"] = json!(folder.to_string_lossy());
        let reason = refusal(&tool(&mut outside, "note_save", named));
        assert_eq!(reason.is_none(), allowed, "{}", folder.display());
    }

    // A worktree listed turns on its main repository, and a clone listed is on by itself.
    global(&guarded, &kind(false, false, &[&linked]), &off());
    assert_eq!(saves(&guarded.repo, "Main by worktree"), None);
    assert_eq!(saves(&clone, "Clone not listed").as_deref(), Some("repo-off"));

    global(&guarded, &kind(false, false, &[&clone]), &off());
    assert_eq!(saves(&clone, "Clone listed"), None);
    assert_eq!(saves(&guarded.repo, "Main not listed").as_deref(), Some("repo-off"));

    // A relative `gitdir`, and Windows line ends, still find the main repository.
    global(&guarded, &kind(false, false, &[&guarded.repo]), &off());
    let relative = Path::new("..").join("widget/.git/worktrees/linked");
    std::fs::write(linked.join(".git"), format!("gitdir: {}\r\n", relative.display())).expect("rewrite");
    assert_eq!(saves(&linked, "Relative"), None);
}

#[test]
fn a_worktree_claim_that_does_not_check_out_maps_to_nothing() {
    let guarded = Guarded::new();
    commit(&guarded.repo);
    let linked = worktree(&guarded.repo, "linked");
    global(&guarded, &kind(false, false, &[&guarded.repo]), &off());

    let mut server = guarded.server(None, &linked);
    assert_eq!(refusal(&tool(&mut server, "note_save", note("Linked"))), None);

    // A back-link that names another file, and an admin folder without `commondir`.
    let admin = guarded.repo.join(".git/worktrees/linked");
    let back = std::fs::read_to_string(admin.join("gitdir")).expect("back-link");

    std::fs::write(admin.join("gitdir"), "/nowhere/.git\n").expect("break");
    assert_eq!(
        refusal(&tool(&mut server, "note_save", note("Broken back-link"))).as_deref(),
        Some("repo-off")
    );
    std::fs::write(admin.join("gitdir"), back).expect("restore");
    assert_eq!(refusal(&tool(&mut server, "note_save", note("Restored"))), None);

    std::fs::write(admin.join("commondir"), "x".repeat(10_000)).expect("big");
    assert_eq!(
        refusal(&tool(&mut server, "note_save", note("Large commondir"))).as_deref(),
        Some("repo-off")
    );
}

#[test]
fn every_project_allows_saving_with_or_without_a_repository() {
    let guarded = Guarded::new();
    let on = kind(false, true, &[]);
    global(&guarded, &on, &on);

    for folder in [guarded.repo.clone(), guarded.work(), home(&guarded)] {
        let mut server = guarded.server(None, &folder);
        assert_eq!(refusal(&tool(&mut server, "note_save", global_note("Anywhere"))), None);
        let mut hello = script("hello");
        hello["scope"] = json!("global");
        assert_eq!(refusal(&tool(&mut server, "script_save", hello)), None);
        assert_eq!(approval(&mut server, "hello"), "local");
    }

    let mut chat = guarded.chat_server();
    assert_eq!(refusal(&tool(&mut chat, "note_save", note("In a chat"))), None);
}

#[test]
fn a_global_note_and_a_call_without_a_repository_need_every_project() {
    let guarded = Guarded::new();
    global(
        &guarded,
        &kind(false, false, &[&guarded.repo]),
        &kind(false, false, &[&guarded.repo]),
    );

    // On for the repository, so a global note from it saves too.
    let mut in_repo = guarded.server(None, &guarded.repo);
    assert_eq!(
        refusal(&tool(&mut in_repo, "note_save", global_note("From the repo"))),
        None
    );

    // From a folder that is not on, a global one is refused.
    let mut in_work = guarded.server(None, &guarded.work());
    assert_eq!(
        refusal(&tool(&mut in_work, "note_save", global_note("From elsewhere"))).as_deref(),
        Some("repo-off")
    );

    // Naming the enabled repository from there is refused too: the working folder is off.
    let mut named = global_note("Named");
    named["project_path"] = json!(guarded.repo.to_string_lossy());
    assert_eq!(
        refusal(&tool(&mut in_work, "note_save", named.clone())).as_deref(),
        Some("repo-off")
    );

    // No repository at all (the home folder is no project), naming the enabled repository saves.
    let mut nowhere = guarded.server(None, &home(&guarded));
    assert_eq!(refusal(&tool(&mut nowhere, "note_save", named)), None);
    assert_eq!(
        refusal(&tool(&mut nowhere, "note_save", global_note("From nowhere"))).as_deref(),
        Some("repo-off")
    );
    assert_eq!(
        refusal(&tool(
            &mut nowhere,
            "script_save",
            json!({ "scope": "global", "name": "x", "description": "d",
                                                          "language": "sh", "body": "true" })
        ))
        .as_deref(),
        Some("repo-off")
    );

    let search = tool(&mut nowhere, "note_search", json!({ "query": "cache" }));
    assert!(text(&search).ends_with(NOTES_NOTICE), "{}", text(&search));
}

#[test]
fn every_repository_of_the_call_must_be_on_and_the_chat_rules_still_apply() {
    let guarded = Guarded::new();
    let other = canonical(&repo_with_remote(
        guarded.dir.path(),
        "other",
        "https://github.com/acme/gadget.git",
    ));
    global(&guarded, &kind(false, false, &[&guarded.repo]), &off());

    let chat = |root: &Path| {
        let text = json!({ "version": 1, "paused": false, "repoRoot": root.to_string_lossy(),
                           "notes": { "on": true, "ask": false }, "scripts": { "on": true, "ask": false } });
        guarded.chat(CHAT, &text.to_string());
    };

    // The chat's root is off while the call names an enabled project.
    chat(&other);
    let mut server = guarded.server(Some(CHAT), &guarded.repo);
    assert_eq!(
        refusal(&tool(&mut server, "note_save", note("Chat root off"))).as_deref(),
        Some("repo-off")
    );

    let mut outside = guarded.server(Some(CHAT), &guarded.work());
    let mut named = note("Named");
    named["project_path"] = json!(guarded.repo.to_string_lossy());
    assert_eq!(
        refusal(&tool(&mut outside, "note_save", named.clone())).as_deref(),
        Some("repo-off")
    );

    // The chat's root is on, the working folder named by no project_path is not.
    chat(&guarded.repo);
    assert_eq!(refusal(&tool(&mut outside, "note_save", named)), None);
    assert_eq!(
        refusal(&tool(&mut outside, "note_save", note("Working folder off"))).as_deref(),
        Some("repo-off")
    );
    assert_eq!(refusal(&tool(&mut server, "note_save", note("Both on"))), None);

    // The chat's own rules still apply on top.
    guarded.rules(false, (false, false), (true, false));
    assert_eq!(
        refusal(&tool(&mut server, "note_save", note("Chat off"))).as_deref(),
        Some("chat-off")
    );
}

#[test]
fn a_pause_beats_every_project() {
    let guarded = Guarded::new();
    global(&guarded, &kind(true, true, &[]), &kind(false, true, &[]));
    let mut server = guarded.server(None, &guarded.repo);

    let refused = tool(&mut server, "note_save", note("Paused"));
    assert_eq!(refusal(&refused).as_deref(), Some("global-paused"));
    assert_eq!(
        text(&refused),
        "The user paused saving notes everywhere. Nothing was saved. Do not retry. Continue without saving."
    );
    assert_eq!(refusal(&tool(&mut server, "script_save", script("hello"))), None);

    let search = tool(&mut server, "note_search", json!({ "query": "x" }));
    assert!(
        text(&search).contains("Saving notes is paused everywhere"),
        "{}",
        text(&search)
    );
}

/// The desktop reads at most this much of the file: a larger one it could not bring to version 2.
const DESKTOP_FILE_MAX: usize = 256 * 1024;

#[test]
fn a_file_larger_than_the_desktop_reads_refuses_whatever_its_version() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);
    let padding = "x".repeat(DESKTOP_FILE_MAX);
    let old = json!({ "paused": false, "offRepos": [format!("/{padding}")] });

    guarded.global(&json!({ "version": 1, "notes": old, "scripts": old }).to_string());

    let reply = tool(&mut server, "note_save", note("Too large"));
    assert_eq!(refusal(&reply).as_deref(), Some("guard-unreadable"));
}

#[test]
fn an_unknown_version_or_a_wrong_shape_refuses() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);
    let ok = json!({ "paused": false, "everyProject": true, "onRepos": [] });

    for broken in [
        json!({ "version": 3, "notes": ok, "scripts": ok }),
        json!({ "version": 2, "notes": ok }),
        json!({ "version": 2, "notes": { "paused": false, "onRepos": [] }, "scripts": ok }),
        json!({ "version": 2, "notes": { "paused": false, "everyProject": true, "onRepos": [], "offRepos": [] },
                "scripts": ok }),
        json!({ "version": 2, "notes": { "paused": false, "offRepos": [] }, "scripts": ok }),
    ] {
        guarded.global(&broken.to_string());

        let reply = tool(&mut server, "note_save", note("Broken"));
        assert_eq!(refusal(&reply).as_deref(), Some("guard-unreadable"), "{broken}");

        let reply = tool(&mut server, "script_save", script("hello"));
        assert_eq!(refusal(&reply).as_deref(), Some("guard-unreadable"), "{broken}");

        let search = tool(&mut server, "note_search", json!({ "query": "x" }));
        assert!(
            text(&search).ends_with("Saving notes is blocked: the save settings could not be read. Reading works."),
            "{}",
            text(&search)
        );
    }

    // Fixed, it saves again.
    guarded.global(&json!({ "version": 2, "notes": ok, "scripts": ok }).to_string());
    assert_eq!(refusal(&tool(&mut server, "note_save", note("Fixed"))), None);
}
