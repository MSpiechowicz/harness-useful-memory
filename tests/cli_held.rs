//! The user-only commands of the save guards, on the real executable: `held list|save|skip` show,
//! save (as drafted or edited) and drop what an agent's chat held for review, a held script is
//! saved untrusted, a held update of a note deleted since is `gone`; and `notes save`, `notes
//! update` and `scripts save` save the user's own notes and scripts whatever the guards say.

mod common;

use std::io::Write;
use std::process::{Command, Output, Stdio};

use common::guards::{CHAT, Guarded, structured, tool};
use common::mcp::{is_error, text};
use common::stdio::{BINARY, without_team_settings};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

/// `harness-memory <args>` on the data folder, started outside any repository, with `input` on
/// standard input.
fn cli(guarded: &Guarded, args: &[&str], input: &str) -> Output {
    let mut command = Command::new(BINARY);
    command
        .args(args)
        .current_dir(guarded.work())
        .env("HARNESS_MEMORY_DIR", guarded.data())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    without_team_settings(&mut command);

    let mut child = command.spawn().expect("the executable runs");
    let mut stdin = child.stdin.take().expect("stdin");
    let _ = stdin.write_all(input.as_bytes());
    drop(stdin);
    child.wait_with_output().expect("the executable ends")
}

/// The parsed stdout of a successful `harness-memory <args>`.
fn ok(guarded: &Guarded, args: &[&str], input: &str) -> Value {
    let output = cli(guarded, args, input);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON")
}

/// The `error` object of a refused `harness-memory <args>` (exit 1).
fn refused(guarded: &Guarded, args: &[&str], input: &str) -> Value {
    let output = cli(guarded, args, input);
    assert_eq!(output.status.code(), Some(1), "{args:?}");
    serde_json::from_slice(&output.stdout).expect("JSON")
}

fn held_id(reply: &Value) -> String {
    assert!(!is_error(reply), "{reply}");
    structured(reply)["heldId"].as_str().expect("held").to_owned()
}

/// A chat that holds notes and scripts for review, with Harness Desktop running.
fn asking() -> Guarded {
    let guarded = Guarded::new();
    guarded.rules(false, (true, true), (true, true));
    guarded.desktop_alive();
    guarded
}

fn note(title: &str) -> Value {
    json!({ "title": title, "body": "Body of the note", "kind": "decision" })
}

#[test]
fn held_items_are_listed_saved_edited_and_skipped() {
    let guarded = asking();
    let mut chat = guarded.chat_server();
    let first = held_id(&tool(&mut chat, "note_save", note("First held")));
    let second = held_id(&tool(&mut chat, "note_save", note("Second held")));
    let third = held_id(&tool(
        &mut chat,
        "script_save",
        json!({ "name": "hello", "description": "d", "language": "sh", "body": "echo hi" }),
    ));

    let listed = ok(&guarded, &["held", "list", "--json"], "");
    let ids: Vec<&str> = listed["held"]
        .as_array()
        .expect("held")
        .iter()
        .map(|item| item["heldId"].as_str().expect("id"))
        .collect();
    assert_eq!(ids.len(), 3);
    assert!(ids.contains(&first.as_str()) && ids.contains(&second.as_str()) && ids.contains(&third.as_str()));
    assert_eq!(listed["held"][0]["chatKey"], CHAT);

    let other = "00000000-0000-4000-8000-000000000000";
    assert_eq!(
        ok(&guarded, &["held", "list", "--json", "--chat", other], ""),
        json!({ "held": [] })
    );
    assert_eq!(
        ok(&guarded, &["held", "list", "--json", "--chat", CHAT], "")["held"]
            .as_array()
            .map(Vec::len),
        Some(3)
    );

    let saved = ok(&guarded, &["held", "save", &first, "--json"], "");
    assert_eq!(saved["saved"]["kind"], "note");
    let first_id = saved["saved"]["id"].as_i64().expect("id");

    let edited = ok(
        &guarded,
        &["held", "save", &second, "--json"],
        r#"{ "title": "Edited title", "body": "Edited body" }"#,
    );
    let second_id = edited["saved"]["id"].as_i64().expect("id");

    assert_eq!(
        ok(&guarded, &["held", "skip", &third, "--json"], ""),
        json!({ "skipped": third })
    );
    assert_eq!(ok(&guarded, &["held", "list", "--json"], ""), json!({ "held": [] }));

    let gone = refused(&guarded, &["held", "skip", &third, "--json"], "");
    assert_eq!(gone["error"], "notFound");

    // Saved for real now, as the agent's client from the agent's repository.
    let mut reader = guarded.server(None, &guarded.repo);
    let shown = tool(&mut reader, "note_get", json!({ "ids": [first_id, second_id] }));
    let shown = text(&shown);
    assert!(
        shown.contains("First held") && shown.contains("[decision, repo"),
        "{shown}"
    );
    assert!(
        shown.contains("Edited title") && shown.contains("Edited body"),
        "{shown}"
    );
    assert!(!shown.contains("Second held"), "{shown}");
    assert!(shown.contains("omp"), "{shown}");

    let skipped = tool(&mut reader, "script_get", json!({ "name": "hello" }));
    assert!(is_error(&skipped), "{skipped}");

    let invalid = held_id(&tool(&mut chat, "note_save", note("Third held")));
    let bad = refused(
        &guarded,
        &["held", "save", &invalid, "--json"],
        r#"{ "project_path": "/" }"#,
    );
    assert_eq!(bad["error"], "invalid");
}

#[test]
fn a_held_script_is_saved_untrusted() {
    let guarded = asking();
    let mut chat = guarded.chat_server();
    let marker = guarded.dir.path().join("ran");
    let body = format!("touch '{}'", marker.display());
    let id = held_id(&tool(
        &mut chat,
        "script_save",
        json!({ "name": "touch-it", "description": "d", "language": "sh", "body": body, "scope": "global" }),
    ));

    let saved = ok(&guarded, &["held", "save", &id, "--json"], "");
    assert_eq!(saved, json!({ "saved": { "kind": "script", "name": "touch-it" } }));

    let mut agent = guarded.server(None, &guarded.work());
    let got = tool(&mut agent, "script_get", json!({ "name": "touch-it" }));
    assert_eq!(structured(&got)["script"]["approval"], "needed");

    // Harness Desktop is gone by now, so nothing asks and nothing runs.
    std::fs::remove_file(guarded.data().join("desktop.alive")).expect("desktop quits");
    let run = tool(&mut agent, "script_run", json!({ "name": "touch-it" }));
    assert!(is_error(&run), "{run}");
    assert!(!marker.exists());
}

#[test]
fn the_users_own_saves_ignore_every_guard() {
    let guarded = Guarded::new();
    let root = guarded.repo.to_string_lossy().into_owned();
    guarded.rules(true, (false, false), (false, false));
    guarded.global(
        &json!({ "version": 1, "notes": { "paused": true, "offRepos": [root] },
                 "scripts": { "paused": true, "offRepos": [root] } })
        .to_string(),
    );

    let saved = ok(
        &guarded,
        &["notes", "save", "--json", "--project", &root],
        r#"{ "title": "User note", "body": "Written by the user", "kind": "decision", "tags": ["ux"] }"#,
    );
    let id = saved["id"].as_i64().expect("id");
    assert_eq!(saved, json!({ "id": id, "title": "User note", "scope": "repo" }));

    let global = ok(
        &guarded,
        &["notes", "save", "--json"],
        r#"{ "title": "Global user note", "body": "Everywhere", "kind": "decision", "scope": "global" }"#,
    );
    assert_eq!(global["scope"], "global");

    let updated = ok(
        &guarded,
        &["notes", "update", &id.to_string(), "--json", "--project", &root],
        r#"{ "title": "User note, renamed" }"#,
    );
    assert_eq!(updated, json!({ "id": id, "title": "User note, renamed" }));

    let script = ok(
        &guarded,
        &["scripts", "save", "--json", "--project", &root],
        r#"{ "name": "greet", "description": "Greets", "language": "sh", "body": "echo hi", "timeoutSecs": 5 }"#,
    );
    assert_eq!(
        (
            &script["name"],
            &script["scope"],
            &script["approval"],
            &script["created"]
        ),
        (&json!("greet"), &json!("repo"), &json!("local"), &json!(true))
    );

    let missing = refused(
        &guarded,
        &["notes", "update", "999", "--json", "--project", &root],
        r#"{ "body": "x" }"#,
    );
    assert_eq!(missing["error"], "notFound");

    let bad = refused(
        &guarded,
        &["notes", "save", "--json"],
        r#"{ "title": "x", "project_path": "/" }"#,
    );
    assert_eq!(bad["error"], "invalid");

    // The agent's own writes are still refused.
    let mut agent = guarded.chat_server();
    let reply = tool(&mut agent, "note_save", note("Agent note"));
    assert_eq!(structured(&reply)["saved"], false);
}

#[test]
fn the_users_own_saves_show_the_client_they_name() {
    let guarded = Guarded::new();
    let root = guarded.repo.to_string_lossy().into_owned();

    let saved = ok(
        &guarded,
        &[
            "notes",
            "save",
            "--json",
            "--project",
            &root,
            "--client",
            "harness-desktop",
        ],
        r#"{ "title": "Desktop note", "body": "Saved from the app", "kind": "decision" }"#,
    );
    let id = saved["id"].as_i64().expect("id");

    let unnamed = ok(
        &guarded,
        &["notes", "save", "--json", "--project", &root],
        r#"{ "title": "Unnamed note", "body": "Saved from a terminal", "kind": "decision" }"#,
    );

    let authors = |guarded: &Guarded| {
        let listed = ok(guarded, &["notes", "list", "--json", "--project", &root], "");
        let rows = listed["notes"].as_array().expect("notes").clone();
        let author_of = |id: &Value| {
            rows.iter()
                .find(|row| &row["id"] == id)
                .map(|row| row["author"]["source"].clone())
                .expect("listed")
        };
        (author_of(&json!(id)), author_of(&unnamed["id"]))
    };
    assert_eq!(authors(&guarded), (json!("harness-desktop"), json!("unknown")));

    ok(
        &guarded,
        &[
            "notes",
            "update",
            &id.to_string(),
            "--json",
            "--project",
            &root,
            "--client=harness-desktop",
        ],
        r#"{ "body": "Edited in the app" }"#,
    );
    assert_eq!(authors(&guarded), (json!("harness-desktop"), json!("unknown")));

    let script = ok(
        &guarded,
        &[
            "scripts",
            "save",
            "--json",
            "--project",
            &root,
            "--client",
            "harness-desktop",
        ],
        r#"{ "name": "greet", "description": "Greets", "language": "sh", "body": "echo hi", "timeoutSecs": 5 }"#,
    );
    let shown = ok(
        &guarded,
        &[
            "scripts",
            "get",
            &script["id"].to_string(),
            "--json",
            "--project",
            &root,
        ],
        "",
    );
    assert_eq!(shown["script"]["author"]["source"], "harness-desktop", "{shown}");

    for bad in ["", "  ", "two\nlines", &"x".repeat(65)] {
        let refusal = refused(
            &guarded,
            &["notes", "save", "--json", "--client", bad],
            r#"{ "title": "x", "body": "y", "kind": "decision" }"#,
        );
        assert_eq!(refusal["error"], "invalid", "{bad:?}");
    }
}

#[test]
fn a_held_update_of_a_note_deleted_since_is_gone() {
    let guarded = Guarded::new();
    let mut chat = guarded.chat_server();
    let reply = tool(&mut chat, "note_save", note("Soon deleted"));
    assert!(!is_error(&reply), "{reply}");

    guarded.rules(false, (true, true), (true, false));
    guarded.desktop_alive();
    let held = held_id(&tool(
        &mut chat,
        "note_update",
        json!({ "id": 1, "body": "A newer body" }),
    ));

    // Deletes pass in Ask mode.
    assert!(!is_error(&tool(&mut chat, "note_delete", json!({ "id": 1 }))));

    let gone = refused(&guarded, &["held", "save", &held, "--json"], "");
    assert_eq!(gone["error"], "gone");
    assert!(gone["message"].as_str().expect("message").contains("note 1"));
}

/// The waiting item `held_id` as `held list` shows it.
fn waiting(guarded: &Guarded, held_id: &str) -> Value {
    listed(guarded)
        .into_iter()
        .find(|item| item["heldId"] == json!(held_id))
        .expect("the item waits")
}

#[test]
fn a_held_update_of_a_note_changed_since_is_shown_again_against_the_note_as_it_is_now() {
    let guarded = Guarded::new();
    let mut chat = guarded.chat_server();
    assert!(!is_error(&tool(&mut chat, "note_save", note("Changing note"))));

    guarded.rules(false, (true, true), (true, false));
    guarded.desktop_alive();
    let held = held_id(&tool(
        &mut chat,
        "note_update",
        json!({ "id": 1, "append": "Appended by the agent" }),
    ));
    let shown = waiting(&guarded, &held);
    assert_eq!(shown["current"]["body"], "Body of the note");
    let first = shown["revision"].as_str().expect("revision").to_owned();

    // The user changes the note before reviewing the update, which shows the note as it was.
    let root = guarded.repo.to_string_lossy().into_owned();
    ok(
        &guarded,
        &["notes", "update", "1", "--json", "--project", &root],
        r#"{ "body": "Rewritten by the user" }"#,
    );

    let changed = refused(&guarded, &["held", "save", &held, "--json", "--expect", &first], "");
    assert_eq!(changed["error"], "changed");
    assert!(changed["message"].as_str().expect("message").contains("note 1"));

    // The item keeps its id and what the agent asked, now shown against the note as it is.
    let again = waiting(&guarded, &held);
    assert_eq!(listed(&guarded).len(), 1);
    assert_eq!(again["current"]["body"], "Rewritten by the user");
    assert_eq!(again["current"]["title"], "Changing note");
    assert_eq!(again["arguments"], shown["arguments"]);
    let second = again["revision"].as_str().expect("revision").to_owned();
    assert_ne!(second, first);

    // What the user saw before is no longer what waits; what they see now saves.
    let stale = refused(&guarded, &["held", "save", &held, "--json", "--expect", &first], "");
    assert_eq!(stale["error"], "changed");
    assert_eq!(
        ok(&guarded, &["held", "save", &held, "--json", "--expect", &second], ""),
        json!({ "saved": { "kind": "note", "id": 1 } })
    );

    let mut reader = guarded.server(None, &guarded.repo);
    let read = tool(&mut reader, "note_get", json!({ "ids": [1] }));
    let read = text(&read);
    assert!(
        read.contains("Rewritten by the user") && read.contains("Appended by the agent"),
        "{read}"
    );

    // A change of the tags alone counts as well.
    guarded.rules(false, (true, false), (true, false));
    assert!(!is_error(&tool(&mut chat, "note_save", note("Tagged note"))));
    guarded.rules(false, (true, true), (true, false));
    let tagged = held_id(&tool(&mut chat, "note_update", json!({ "id": 2, "body": "New body" })));
    ok(
        &guarded,
        &["notes", "update", "2", "--json", "--project", &root],
        r#"{ "tags": ["retagged"] }"#,
    );
    assert_eq!(
        refused(&guarded, &["held", "save", &tagged, "--json"], "")["error"],
        "changed"
    );
    assert_eq!(waiting(&guarded, &tagged)["current"]["tags"], json!(["retagged"]));
    ok(&guarded, &["held", "save", &tagged, "--json"], "");
    assert_eq!(listed(&guarded), Vec::<Value>::new());
}

#[test]
fn a_skip_expecting_another_revision_is_changed_and_keeps_the_item() {
    let guarded = asking();
    let mut chat = guarded.chat_server();
    let id = held_id(&tool(&mut chat, "note_save", note("Skipped draft")));
    let shown = listed(&guarded)[0]["revision"].as_str().expect("revision").to_owned();

    let mut again = note("Skipped draft");
    again["body"] = json!("A draft the user did not see");
    assert_eq!(held_id(&tool(&mut chat, "note_save", again)), id);

    let changed = refused(&guarded, &["held", "skip", &id, "--json", "--expect", &shown], "");
    assert_eq!(changed["error"], "changed");
    assert_eq!(listed(&guarded).len(), 1, "the item still waits");

    let now = listed(&guarded)[0]["revision"].as_str().expect("revision").to_owned();
    assert_eq!(
        ok(&guarded, &["held", "skip", &id, "--json", "--expect", &now], ""),
        json!({ "skipped": id })
    );
    assert_eq!(listed(&guarded), Vec::<Value>::new());
    assert_eq!(
        refused(&guarded, &["held", "skip", &id, "--json", "--expect", &now], "")["error"],
        "notFound"
    );
}

/// The held items `held list` shows.
fn listed(guarded: &Guarded) -> Vec<Value> {
    ok(guarded, &["held", "list", "--json"], "")["held"]
        .as_array()
        .expect("held")
        .clone()
}

#[test]
fn a_save_expecting_another_revision_is_changed_and_keeps_the_item() {
    let guarded = asking();
    let mut chat = guarded.chat_server();
    let id = held_id(&tool(&mut chat, "note_save", note("Shown draft")));
    let shown = listed(&guarded)[0]["revision"].as_str().expect("revision").to_owned();

    // The agent holds the same note again while the user looks at the first draft.
    let mut again = note("Shown draft");
    again["body"] = json!("A draft the user did not see");
    assert_eq!(held_id(&tool(&mut chat, "note_save", again)), id);

    let changed = refused(&guarded, &["held", "save", &id, "--json", "--expect", &shown], "");
    assert_eq!(changed["error"], "changed");
    assert_eq!(listed(&guarded).len(), 1, "the item still waits");

    let now = listed(&guarded)[0]["revision"].as_str().expect("revision").to_owned();
    let saved = ok(&guarded, &["held", "save", &id, "--json", "--expect", &now], "");
    assert_eq!(saved["saved"]["kind"], "note");
    assert_eq!(listed(&guarded), Vec::<Value>::new());
}

#[test]
fn two_saves_of_one_item_save_it_once() {
    let guarded = asking();
    let mut chat = guarded.chat_server();
    let id = held_id(&tool(&mut chat, "note_save", note("Saved once")));

    let outputs: Vec<Output> = std::thread::scope(|scope| {
        let saves: Vec<_> = (0..2)
            .map(|_| scope.spawn(|| cli(&guarded, &["held", "save", &id, "--json"], "")))
            .collect();
        saves.into_iter().map(|save| save.join().expect("save")).collect()
    });

    let codes: Vec<Option<i32>> = outputs.iter().map(|output| output.status.code()).collect();
    assert!(codes.contains(&Some(0)) && codes.contains(&Some(1)), "{codes:?}");

    let refusal = outputs.iter().find(|output| !output.status.success()).expect("refused");
    let refusal: Value = serde_json::from_slice(&refusal.stdout).expect("JSON");
    assert_eq!(refusal["error"], "notFound");

    let notes = ok(
        &guarded,
        &["notes", "list", "--json", "--project", &guarded.repo.to_string_lossy()],
        "",
    );
    let titles: Vec<&Value> = notes["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .map(|note| &note["title"])
        .collect();
    assert_eq!(titles, [&json!("Saved once")]);
}

#[test]
fn a_hold_while_a_save_runs_is_kept_as_a_new_item() {
    let guarded = asking();
    let mut chat = guarded.chat_server();
    let id = held_id(&tool(&mut chat, "note_save", note("Same key")));

    // A save has claimed the item and is still saving it.
    let store = harness_memory::held::HeldStore::in_folder(guarded.data());
    let claim = store.claim(&id).expect("claim").expect("waiting");
    assert_eq!(claim.item.held_id, id);
    assert!(store.claim(&id).expect("claim").is_none(), "claimed once");

    let mut again = note("Same key");
    again["body"] = json!("Held during the save");
    let newer = held_id(&tool(&mut chat, "note_save", again));
    assert_ne!(newer, id);

    claim.finish().expect("saved");
    let waiting = listed(&guarded);
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0]["heldId"], json!(newer));
    assert_eq!(waiting[0]["arguments"]["body"], "Held during the save");
}

#[test]
fn a_held_update_saved_as_shown_keeps_only_what_was_held() {
    let guarded = Guarded::new();
    let mut chat = guarded.chat_server();
    assert!(!is_error(&tool(&mut chat, "note_save", note("Appended note"))));

    guarded.rules(false, (true, true), (true, false));
    guarded.desktop_alive();
    let held = held_id(&tool(
        &mut chat,
        "note_update",
        json!({ "id": 1, "append": "One more line", "scope": " Repo ", "stray": "dropped" }),
    ));

    let waiting = listed(&guarded);
    assert_eq!(
        waiting[0]["arguments"],
        json!({ "id": 1, "append": "One more line", "scope": "repo" })
    );

    let revision = waiting[0]["revision"].as_str().expect("revision").to_owned();
    ok(&guarded, &["held", "save", &held, "--json", "--expect", &revision], "");

    let mut reader = guarded.server(None, &guarded.repo);
    let shown = tool(&mut reader, "note_get", json!({ "ids": [1] }));
    let shown = text(&shown);
    assert!(
        shown.contains("Appended note") && shown.contains("Body of the note\n  One more line"),
        "{shown}"
    );
}

#[test]
fn a_held_note_the_user_edits_into_another_language_is_saved_with_a_warning() {
    let polish = "Zadanie synchronizacji czyta każdą zmienioną notatkę z bazy zespołu, a potem zapisuje nową \
                  wersję w lokalnej pamięci podręcznej, żeby następne wyszukiwanie znalazło ją od razu.";
    let guarded = asking();
    let mut chat = guarded.chat_server();
    let held = held_id(&tool(&mut chat, "note_save", note("Held note")));

    let draft = json!({ "title": "Synchronizacja notatek", "body": polish });
    let output = cli(&guarded, &["held", "save", &held, "--json"], &draft.to_string());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let saved: Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(saved["saved"]["warning"], "not-english", "{saved}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("this note is not in English"));

    let found = tool(&mut chat, "note_get", json!({ "ids": [saved["saved"]["id"]] }));
    assert!(text(&found).contains("Synchronizacja notatek"), "{found}");
}
