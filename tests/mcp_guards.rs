//! The save guards an agent meets through the MCP tools: what the user turned off or paused (per
//! chat, per repository, everywhere) refuses every write of that kind, deletes and links
//! included; "Ask before saving" holds a save for the user's review in Harness Desktop and saves
//! nothing; the replies are final results, not errors; and a guard file that cannot be read
//! refuses (fail closed). Reading and `script_run` are never guarded.

mod common;

use common::guards::{CHAT, Guarded, structured, tool};
use common::mcp::{is_error, text};
use harness_memory::Server;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

const CHAT_OFF: &str =
    "The user turned off saving notes for this chat. Nothing was saved. Do not retry. Continue without saving.";

const HELD: &str = "Waiting for the user's review in Harness Desktop. Nothing is saved yet. Do not save it again or \
                    retry, the user decides. Continue with your task.";

fn note(title: &str) -> Value {
    json!({ "title": title, "body": "The cache lives in src/cache.rs", "kind": "location" })
}

fn script(name: &str) -> Value {
    json!({ "name": name, "description": "Says hi", "language": "sh", "body": "echo hi" })
}

/// The reason of a refusal, which is a normal result (not an error).
fn reason(reply: &Value) -> String {
    assert!(!is_error(reply), "{reply}");
    let refused = structured(reply);
    assert_eq!(refused["saved"], false, "{reply}");
    refused["reason"].as_str().expect("reason").to_owned()
}

/// The held id of a held reply.
fn held_id(reply: &Value) -> String {
    assert!(!is_error(reply), "{reply}");
    assert_eq!(text(reply), HELD);
    let held = structured(reply);
    assert_eq!(held["held"], true, "{reply}");
    held["heldId"].as_str().expect("heldId").to_owned()
}

/// The titles `note_search` finds for `query`.
fn found(server: &mut Server, query: &str) -> bool {
    let reply = tool(server, "note_search", json!({ "query": query }));
    !text(&reply).starts_with("no matching notes")
}

fn saved(reply: &Value) {
    assert!(!is_error(reply), "{reply}");
    assert!(structured(reply).get("saved").is_none(), "{reply}");
    assert!(structured(reply).get("held").is_none(), "{reply}");
}

#[test]
fn notes_off_in_the_chat_refuse_every_note_write_and_nothing_changes() {
    let guarded = Guarded::new();
    let mut server = guarded.chat_server();
    saved(&tool(&mut server, "note_save", note("Cache location")));
    saved(&tool(&mut server, "script_save", script("hello")));

    guarded.rules(false, (false, false), (true, false));

    let refused = tool(&mut server, "note_save", note("Another note"));
    assert_eq!(reason(&refused), "chat-off");
    assert_eq!(text(&refused), CHAT_OFF);

    for (name, arguments) in [
        ("note_update", json!({ "id": 1, "body": "changed" })),
        ("note_delete", json!({ "id": 1 })),
        (
            "entity_link",
            json!({ "from": "cache", "relation": "located_in", "to": "src" }),
        ),
        ("entity_delete", json!({ "entity": "cache" })),
    ] {
        assert_eq!(reason(&tool(&mut server, name, arguments)), "chat-off", "{name}");
    }

    assert_eq!(
        text(&tool(&mut server, "note_delete", json!({ "id": 1 }))),
        "The user turned off saving notes for this chat. Nothing was deleted. Do not retry. Continue with your task."
    );

    assert!(found(&mut server, "cache"));
    assert!(!found(&mut server, "Another"));
    let kept = tool(&mut server, "note_get", json!({ "ids": [1] }));
    assert!(
        text(&kept).contains("The cache lives in src/cache.rs"),
        "{}",
        text(&kept)
    );

    // Scripts stay on: saving and deleting them works.
    saved(&tool(&mut server, "script_save", script("other")));
    saved(&tool(&mut server, "script_delete", json!({ "name": "other" })));
}

#[test]
fn a_paused_chat_refuses_notes_and_scripts_but_still_reads_and_runs() {
    let guarded = Guarded::new();
    let mut server = guarded.chat_server();
    saved(&tool(&mut server, "script_save", script("hello")));
    guarded.rules(true, (true, false), (true, false));

    for (name, arguments) in [
        ("note_save", note("Paused")),
        ("script_save", script("hello")),
        ("script_delete", json!({ "name": "hello" })),
    ] {
        assert_eq!(reason(&tool(&mut server, name, arguments)), "chat-paused", "{name}");
    }

    assert_eq!(
        text(&tool(&mut server, "script_save", script("x"))),
        "The user paused saving to Harness Memory for this chat. Nothing was saved. Do not retry. Continue without \
         saving."
    );

    // `sh` runs the script on Unix.
    #[cfg(unix)]
    {
        let ran = tool(&mut server, "script_run", json!({ "name": "hello" }));
        assert!(!is_error(&ran), "{ran}");
        assert_eq!(structured(&ran)["stdout"], "hi\n");
    }

    assert!(!is_error(&tool(&mut server, "script_list", json!({}))));
    assert!(!is_error(&tool(&mut server, "note_search", json!({ "query": "x" }))));
}

#[test]
fn a_repository_turned_off_refuses_its_chats_writes_global_ones_included() {
    let guarded = Guarded::new();
    let root = guarded.repo.to_string_lossy().into_owned();
    guarded.global(
        &json!({ "version": 1, "notes": { "paused": false, "offRepos": [root] },
                             "scripts": { "paused": false, "offRepos": [] } })
        .to_string(),
    );

    // No chat at all: the repository rule applies by itself.
    let mut in_repo = guarded.server(None, &guarded.repo.join("src/deep"));
    let refused = tool(&mut in_repo, "note_save", note("Repo note"));
    assert_eq!(reason(&refused), "repo-off");
    assert_eq!(
        text(&refused),
        "The user turned off saving notes for this project. Nothing was saved. Do not retry. Continue without \
         saving."
    );

    let mut global = note("Global note");
    global["scope"] = json!("global");
    assert_eq!(reason(&tool(&mut in_repo, "note_save", global.clone())), "repo-off");

    // Named with project_path from elsewhere, the repository is the chat's too.
    guarded.rules(false, (true, false), (true, false));
    let mut elsewhere = guarded.server(Some(CHAT), &guarded.work());
    let mut named = note("Named");
    named["project_path"] = json!(root);
    assert_eq!(reason(&tool(&mut elsewhere, "note_save", named)), "repo-off");

    // A server that runs inside the repository (one shared by chats of several projects starts in
    // the first's folder) does not turn off a call that names another project.
    let mut other = note("Other project");
    other["project_path"] = json!(guarded.work().to_string_lossy());
    saved(&tool(&mut in_repo, "note_save", other));

    // Outside it, global notes save, and the repository's scripts are not turned off.
    saved(&tool(&mut elsewhere, "note_save", global));
    saved(&tool(&mut in_repo, "script_save", script("hello")));
    assert!(found(&mut elsewhere, "Global"));
    assert!(!found(&mut in_repo, "Repo"));
}

#[test]
fn a_global_pause_refuses_its_kind_everywhere() {
    let guarded = Guarded::new();
    guarded.global(
        r#"{ "version": 1, "notes": { "paused": false, "offRepos": [] },
                        "scripts": { "paused": true, "offRepos": [] } }"#,
    );
    let mut server = guarded.server(None, &guarded.work());

    let refused = tool(
        &mut server,
        "script_save",
        json!({ "scope": "global", "name": "x", "description": "d",
                                                             "language": "sh", "body": "true" }),
    );
    assert_eq!(reason(&refused), "global-paused");
    assert_eq!(
        text(&refused),
        "The user paused saving scripts everywhere. Nothing was saved. Do not retry. Continue without saving."
    );

    let mut global = note("Still saved");
    global["scope"] = json!("global");
    saved(&tool(&mut server, "note_save", global));
}

#[test]
fn without_a_session_only_the_repository_and_global_guards_apply() {
    let guarded = Guarded::new();
    guarded.rules(true, (false, false), (false, false));
    let mut server = guarded.server(None, &guarded.repo);

    saved(&tool(&mut server, "note_save", note("No chat")));
    saved(&tool(&mut server, "script_save", script("hello")));

    guarded.global(
        r#"{ "version": 1, "notes": { "paused": true, "offRepos": [] },
                        "scripts": { "paused": false, "offRepos": [] } }"#,
    );
    assert_eq!(reason(&tool(&mut server, "note_save", note("Paused"))), "global-paused");
}

#[test]
fn a_session_without_a_chat_file_refuses() {
    let guarded = Guarded::new();
    let mut server = guarded.server(Some(CHAT), &guarded.repo);

    assert_eq!(
        reason(&tool(&mut server, "note_save", note("No rules yet"))),
        "guard-unreadable"
    );
    assert_eq!(
        reason(&tool(&mut server, "script_delete", json!({ "name": "x" }))),
        "guard-unreadable"
    );
    assert!(!found(&mut server, "rules"));

    guarded.rules(false, (true, false), (true, false));
    saved(&tool(&mut server, "note_save", note("Rules written")));
}

#[test]
fn unreadable_guards_refuse() {
    let guarded = Guarded::new();

    let mut invalid = guarded.server(Some("NOT-A-UUID"), &guarded.repo);
    let refused = tool(&mut invalid, "note_save", note("x"));
    assert_eq!(reason(&refused), "guard-unreadable");
    assert_eq!(
        reason(&tool(&mut invalid, "script_delete", json!({ "name": "x" }))),
        "guard-unreadable"
    );

    let mut server = guarded.chat_server();

    for broken in [
        "not json",
        r#"{ "version": 2, "paused": false, "notes": { "on": true, "ask": false }, "scripts": { "on": true, "ask": false } }"#,
        r#"{ "version": 1, "paused": false, "notes": { "on": true } }"#,
    ] {
        guarded.chat(CHAT, broken);
        assert_eq!(
            reason(&tool(&mut server, "note_save", note("x"))),
            "guard-unreadable",
            "{broken}"
        );
    }

    #[cfg(unix)]
    {
        let chats = guarded.data().join("guards/chats");
        let real = guarded.dir.path().join("rules.json");
        std::fs::write(
            &real,
            r#"{ "version": 1, "paused": false, "notes": { "on": true, "ask": false },
                                   "scripts": { "on": true, "ask": false } }"#,
        )
        .expect("write");
        std::fs::remove_file(chats.join(format!("{CHAT}.json"))).expect("remove");
        std::os::unix::fs::symlink(&real, chats.join(format!("{CHAT}.json"))).expect("link");
        assert_eq!(reason(&tool(&mut server, "note_save", note("x"))), "guard-unreadable");
    }

    // A broken global file refuses every chat, and calls without one.
    guarded.rules(false, (true, false), (true, false));
    guarded.global("{}");
    assert_eq!(reason(&tool(&mut server, "note_save", note("x"))), "guard-unreadable");
    let mut no_chat = guarded.server(None, &guarded.work());
    assert_eq!(
        reason(&tool(&mut no_chat, "script_save", script("x"))),
        "guard-unreadable"
    );

    assert!(!found(&mut server, "x"));
}

#[test]
fn ask_without_harness_desktop_refuses() {
    let guarded = Guarded::new();
    guarded.rules(false, (true, true), (true, false));
    let mut server = guarded.chat_server();

    let refused = tool(&mut server, "note_save", note("Asked"));
    assert_eq!(reason(&refused), "no-desktop");
    assert_eq!(
        text(&refused),
        "The user reviews notes before they are saved, and Harness Desktop is not running. Nothing was saved. Do not \
         retry. Continue without saving."
    );
    assert_eq!(guarded.held_files(), Vec::<Value>::new());
}

#[test]
fn ask_holds_saves_for_review_and_saves_nothing() {
    let guarded = Guarded::new();
    let mut server = guarded.chat_server();
    saved(&tool(&mut server, "note_save", note("Cache location")));
    saved(&tool(&mut server, "script_save", script("hello")));
    guarded.rules(false, (true, true), (true, true));
    guarded.desktop_alive();

    let held = held_id(&tool(&mut server, "note_save", note("Held note")));
    assert!(!found(&mut server, "Held"));

    let update = held_id(&tool(
        &mut server,
        "note_update",
        json!({ "id": 1, "body": "Moved to src/store" }),
    ));
    let mut replaced = script("hello");
    replaced["body"] = json!("echo bye");
    let script_held = held_id(&tool(&mut server, "script_save", replaced));

    let listed = tool(&mut server, "script_get", json!({ "name": "hello" }));
    assert_eq!(structured(&listed)["script"]["body"], "echo hi");
    let note_now = tool(&mut server, "note_get", json!({ "ids": [1] }));
    assert!(text(&note_now).contains("The cache lives in src/cache.rs"));

    let mut files = guarded.held_files();
    files.sort_by_key(|file| file["tool"].as_str().map(str::to_owned));
    let ids: Vec<&str> = files.iter().map(|file| file["heldId"].as_str().expect("id")).collect();
    assert_eq!(ids, [held.as_str(), update.as_str(), script_held.as_str()]);

    let note_file = &files[0];
    assert_eq!(
        (
            &note_file["version"],
            &note_file["chatKey"],
            &note_file["client"],
            &note_file["kind"]
        ),
        (&json!(1), &json!(CHAT), &json!("omp"), &json!("note"))
    );
    assert_eq!(note_file["arguments"]["title"], "Held note");
    assert_eq!(note_file["current"], Value::Null);
    assert_eq!(note_file["repoRoot"], json!(guarded.repo.to_string_lossy()));
    assert_eq!(note_file["projectPath"], json!(guarded.repo.to_string_lossy()));

    assert_eq!(files[1]["current"]["title"], "Cache location");
    assert_eq!(files[1]["current"]["body"], "The cache lives in src/cache.rs");
    assert_eq!(files[2]["current"]["body"], "echo hi");
    assert_eq!(files[2]["current"]["name"], "hello");

    // Deletes and links pass in Ask mode.
    saved(&tool(
        &mut server,
        "entity_link",
        json!({ "from": "cache", "relation": "located_in", "to": "src" }),
    ));
    saved(&tool(&mut server, "note_delete", json!({ "id": 1 })));

    // An update of a note that is not there is the tool's own error, and nothing waits.
    let missing = tool(&mut server, "note_update", json!({ "id": 99, "body": "x" }));
    assert!(is_error(&missing), "{missing}");
    assert_eq!(guarded.held_files().len(), 3);
}

#[test]
fn holding_the_same_title_again_replaces_it_under_its_held_id() {
    let guarded = Guarded::new();
    guarded.rules(false, (true, true), (true, false));
    guarded.desktop_alive();
    let mut server = guarded.chat_server();

    let first = held_id(&tool(&mut server, "note_save", note("Same title")));
    let mut again = note("same title ");
    again["body"] = json!("The second draft");
    let second = held_id(&tool(&mut server, "note_save", again));
    let other = held_id(&tool(&mut server, "note_save", note("Other title")));

    assert_eq!(first, second);
    assert_ne!(first, other);

    let files = guarded.held_files();
    assert_eq!(files.len(), 2);
    let replaced = files.iter().find(|file| file["heldId"] == json!(first)).expect("held");
    assert_eq!(replaced["arguments"]["body"], "The second draft");
}

#[test]
fn a_chat_holds_at_most_fifty_items() {
    let guarded = Guarded::new();
    guarded.rules(false, (true, true), (true, false));
    guarded.desktop_alive();
    let mut server = guarded.chat_server();

    for index in 0..50 {
        held_id(&tool(&mut server, "note_save", note(&format!("Note {index}"))));
    }

    let refused = tool(&mut server, "note_save", note("One too many"));
    assert_eq!(reason(&refused), "held-limit");
    assert_eq!(
        text(&refused),
        "Too many saves are waiting for the user's review in this chat. Nothing was saved. Do not retry. Continue \
         without saving."
    );
    assert_eq!(guarded.held_files().len(), 50);

    // A held item replaced keeps the count.
    held_id(&tool(&mut server, "note_save", note("Note 7")));
}

/// The global guard file turning `kind` (`notes` or `scripts`) off for the repositories `off`.
fn off_repos(guarded: &Guarded, kind: &str, off: &[String]) {
    let mut global = json!({ "version": 1, "notes": { "paused": false, "offRepos": [] },
                             "scripts": { "paused": false, "offRepos": [] } });
    global[kind]["offRepos"] = json!(off);
    guarded.global(&global.to_string());
}

/// The file of [`CHAT`] with every kind on, naming `repo_root` as the chat's project root.
fn chat_rooted(guarded: &Guarded, repo_root: Value) {
    let text = json!({ "version": 1, "paused": false, "repoRoot": repo_root,
                       "notes": { "on": true, "ask": false }, "scripts": { "on": true, "ask": false } });
    guarded.chat(CHAT, &text.to_string());
}

fn global_note(title: &str) -> Value {
    let mut global = note(title);
    global["scope"] = json!("global");
    global
}

#[test]
fn the_chat_files_repository_counts_wherever_the_server_runs() {
    let guarded = Guarded::new();
    off_repos(&guarded, "notes", &[guarded.repo.to_string_lossy().into_owned()]);

    // The MCP child runs outside the repository and the call names none.
    let mut server = guarded.server(Some(CHAT), &guarded.work());
    chat_rooted(&guarded, json!(guarded.repo.to_string_lossy()));
    assert_eq!(
        reason(&tool(&mut server, "note_save", global_note("From the repo"))),
        "repo-off"
    );

    // Without the chat's root nothing tells the repository.
    chat_rooted(&guarded, Value::Null);
    saved(&tool(&mut server, "note_save", global_note("From elsewhere")));

    // A root that is not absolute is unreadable.
    chat_rooted(&guarded, json!("widget"));
    assert_eq!(
        reason(&tool(&mut server, "note_save", global_note("Relative"))),
        "guard-unreadable"
    );
}

#[test]
fn a_repository_at_the_home_folder_can_be_turned_off() {
    let guarded = Guarded::new();
    let home = guarded.dir.path().join("home");
    let canonical_home = common::mcp::canonical(&home);

    // Written in another form than the chat's root: they are compared as canonical paths.
    off_repos(&guarded, "notes", &[home.join(".").to_string_lossy().into_owned()]);
    chat_rooted(&guarded, json!(canonical_home.to_string_lossy()));

    let mut server = guarded.server(Some(CHAT), &home);
    assert_eq!(
        reason(&tool(&mut server, "note_save", global_note("At home"))),
        "repo-off"
    );
    let search = tool(&mut server, "note_search", json!({ "query": "home" }));
    assert!(!text(&search).contains("At home"), "{}", text(&search));
}

/// The approval `script_get` shows for script `name`.
fn approval(server: &mut Server, name: &str) -> String {
    let got = tool(server, "script_get", json!({ "name": name }));
    structured(&got)["script"]["approval"]
        .as_str()
        .expect("approval")
        .to_owned()
}

#[test]
fn a_guarded_chat_saves_scripts_untrusted_and_running_them_asks() {
    let guarded = Guarded::new();
    let marker = guarded.dir.path().join("ran");
    let mut touching = script("touch-it");
    touching["body"] = json!(format!("touch '{}'", marker.display()));

    // Notes off in the chat: scripts still save, but nothing the agent saves is trusted.
    guarded.rules(false, (false, false), (true, false));
    let mut server = guarded.chat_server();
    let reply = tool(&mut server, "script_save", touching.clone());
    saved(&reply);
    assert!(
        text(&reply).contains("it needs the user's approval before it runs: the user's save settings"),
        "{}",
        text(&reply)
    );
    assert_eq!(approval(&mut server, "touch-it"), "needed");

    // Without Harness Desktop no one can approve it, so it does not run.
    let run = tool(&mut server, "script_run", json!({ "name": "touch-it" }));
    assert!(is_error(&run), "{run}");
    assert!(!marker.exists());

    // Scripts paused everywhere count too, for a note-only chat as for a chat with no session.
    std::fs::remove_file(guarded.data().join("guards/chats").join(format!("{CHAT}.json"))).expect("remove");
    guarded.global(
        r#"{ "version": 1, "notes": { "paused": true, "offRepos": [] },
                        "scripts": { "paused": false, "offRepos": [] } }"#,
    );
    let mut no_chat = guarded.server(None, &guarded.repo);
    saved(&tool(&mut no_chat, "script_save", script("other")));
    assert_eq!(approval(&mut no_chat, "other"), "needed");

    // A repository turned off for notes only counts as well.
    off_repos(&guarded, "notes", &[guarded.repo.to_string_lossy().into_owned()]);
    saved(&tool(&mut no_chat, "script_save", script("third")));
    assert_eq!(approval(&mut no_chat, "third"), "needed");
}

#[test]
fn without_guards_a_saved_script_is_trusted_as_before() {
    let guarded = Guarded::new();
    let mut server = guarded.chat_server();

    saved(&tool(&mut server, "script_save", script("hello")));
    assert_eq!(approval(&mut server, "hello"), "local");

    // A chat file with everything on and no Ask applies no guard either.
    guarded.rules(false, (true, false), (true, false));
    saved(&tool(&mut server, "script_save", script("again")));
    assert_eq!(approval(&mut server, "again"), "local");

    #[cfg(unix)]
    {
        let ran = tool(&mut server, "script_run", json!({ "name": "again" }));
        assert_eq!(structured(&ran)["stdout"], "hi\n", "{ran}");
    }
}

#[test]
fn held_arguments_are_kept_in_normal_form() {
    let guarded = Guarded::new();
    guarded.rules(false, (true, true), (true, true));
    guarded.desktop_alive();
    let mut server = guarded.chat_server();

    let sent = json!({ "title": "Normal form", "body": "Body", "kind": " Decision ", "scope": " Global ",
                       "tags": ["UX", "ux"], "entities": ["cache", "cache"], "unknown": "dropped" });
    let note_id = held_id(&tool(&mut server, "note_save", sent));

    let sent = json!({ "name": " greet ", "description": " Greets ", "language": " SH ", "body": "echo hi",
                       "scope": "GLOBAL" });
    let script_id = held_id(&tool(&mut server, "script_save", sent));

    let files = guarded.held_files();
    let file = |id: &str| {
        files
            .iter()
            .find(|file| file["heldId"] == json!(id))
            .expect("held")
            .clone()
    };

    assert_eq!(
        file(&note_id)["arguments"],
        json!({ "title": "Normal form", "body": "Body", "kind": "decision", "tags": ["ux"],
                "entities": ["cache"], "scope": "global" })
    );
    assert_eq!(
        file(&script_id)["arguments"],
        json!({ "name": "greet", "description": "Greets", "language": "sh", "body": "echo hi",
                "timeout_secs": 60, "scope": "global" })
    );
}

#[test]
fn holding_again_changes_the_revision() {
    let guarded = Guarded::new();
    guarded.rules(false, (true, true), (true, false));
    guarded.desktop_alive();
    let mut server = guarded.chat_server();

    let first = held_id(&tool(&mut server, "note_save", note("Revised")));
    let revision = |guarded: &Guarded| {
        guarded.held_files()[0]["revision"]
            .as_str()
            .expect("revision")
            .to_owned()
    };
    let before = revision(&guarded);
    assert_eq!(before.len(), 64);
    assert!(before.chars().all(|character| character.is_ascii_hexdigit()));

    let mut again = note("Revised");
    again["body"] = json!("A second draft");
    assert_eq!(held_id(&tool(&mut server, "note_save", again)), first);
    assert_ne!(revision(&guarded), before);
}

#[test]
#[cfg(unix)]
fn scripts_do_not_see_the_chat() {
    use common::stdio::{Client, command};

    let guarded = Guarded::new();
    guarded.rules(false, (true, false), (true, false));
    let mut process = command(&guarded.data(), &guarded.repo);
    process.env("HARNESS_MEMORY_SESSION", CHAT);
    let mut client = Client::start(process);

    let saved = client.call(
        "script_save",
        json!({ "name": "show-env", "description": "", "language": "sh", "body": "env" }),
    );
    assert!(saved.contains("show-env"), "{saved}");

    let ran = client.request(
        "tools/call",
        json!({ "name": "script_run", "arguments": { "name": "show-env" } }),
    );
    let stdout = ran["result"]["structuredContent"]["stdout"].as_str().expect("stdout");
    assert!(stdout.contains("PATH="), "{ran}");
    assert!(!stdout.contains("HARNESS_MEMORY_SESSION"), "{stdout}");
    assert!(client.finish().success());
}
