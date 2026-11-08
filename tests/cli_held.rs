//! The user-only commands of the save guards, on the real executable: `held list|save|skip` show,
//! save (as drafted or edited) and drop what an agent's chat held for review, a held script is
//! saved untrusted, a held update of a note deleted since is `gone`; and `notes save`, `notes
//! update` and `scripts save` save the user's own notes and scripts whatever the guards say.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use common::guards::{CHAT, Guarded, structured, tool};
use common::mcp::{is_error, text};
use common::stdio::{BINARY, without_team_settings};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

/// `harness-memory <args>` on the data folder, started outside any repository, with `input` on
/// standard input.
fn cli(guarded: &Guarded, args: &[&str], input: &str) -> Output {
    cli_at(guarded, None, args, input)
}

/// [`cli`] with `home` as the home folder, where git reads the user's name from.
fn cli_at(guarded: &Guarded, home: Option<&Path>, args: &[&str], input: &str) -> Output {
    let mut command = Command::new(BINARY);

    if let Some(home) = home {
        command.env("HOME", home);
    }

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
fn the_users_own_saves_work_when_saving_is_off_by_default() {
    let guarded = Guarded::new();
    let root = guarded.repo.to_string_lossy().into_owned();
    guarded.global(
        &json!({ "version": 2,
                 "notes": { "paused": false, "everyProject": false, "onRepos": [] },
                 "scripts": { "paused": true, "everyProject": false, "onRepos": [] } })
        .to_string(),
    );

    let saved = ok(
        &guarded,
        &["notes", "save", "--json", "--project", &root],
        r#"{ "title": "User note", "body": "Written by the user", "kind": "decision", "tags": ["ux"] }"#,
    );
    assert_eq!(saved["scope"], "repo");

    let script = ok(
        &guarded,
        &["scripts", "save", "--json", "--project", &root],
        r#"{ "name": "greet", "description": "Greets", "language": "sh", "body": "echo hi" }"#,
    );
    assert_eq!(script["created"], true);

    // The agent's own write is refused.
    let mut agent = guarded.server(None, &guarded.repo);
    let reply = tool(&mut agent, "note_save", note("Agent note"));
    assert_eq!(structured(&reply)["reason"], "repo-off");
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
    assert_eq!(
        shown["current"]["revision"], 1,
        "what the update is judged against once the note cannot be read"
    );
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

/// An update held while the team database could not be read has no `current`. Saved once the note
/// can be read, it is shown against the note as it is, like an update of a note that changed since.
#[test]
fn a_held_update_without_a_current_note_is_shown_against_the_note_as_it_is() {
    use harness_memory::held::{HeldItem, HeldStore, HeldTool};

    let guarded = Guarded::new();
    let mut chat = guarded.chat_server();
    let mut unread = note("Unread note");
    unread["scope"] = json!("global");
    assert!(!is_error(&tool(&mut chat, "note_save", unread)));

    let item = HeldItem::new(
        CHAT,
        "omp",
        HeldTool::NoteUpdate,
        json!({ "id": 1, "append": "Appended by the agent", "scope": "global" }),
        Value::Null,
    );
    let held = HeldStore::in_folder(guarded.data()).hold(item).expect("held");

    let changed = refused(&guarded, &["held", "save", &held, "--json"], "");
    assert_eq!(changed["error"], "changed");

    let again = waiting(&guarded, &held);
    assert_eq!(again["current"]["body"], "Body of the note", "the live note is shown");
    assert_eq!(again["current"]["revision"], 1);
    assert_eq!(listed(&guarded).len(), 1, "nothing was saved");

    // Reviewed against the real note, it saves.
    assert_eq!(
        ok(&guarded, &["held", "save", &held, "--json"], ""),
        json!({ "saved": { "kind": "note", "id": 1 } })
    );
}

/// An edited draft for an update held with no `current` is judged against the note once it can be
/// read: refused as `changed`, never saved over it unseen.
#[test]
fn an_edited_held_update_without_a_current_note_is_changed_when_the_note_can_be_read() {
    use harness_memory::held::{HeldItem, HeldStore, HeldTool};

    let guarded = Guarded::new();
    let mut chat = guarded.chat_server();
    let mut unread = note("Unread note");
    unread["scope"] = json!("global");
    assert!(!is_error(&tool(&mut chat, "note_save", unread)));

    let item = HeldItem::new(
        CHAT,
        "omp",
        HeldTool::NoteUpdate,
        json!({ "id": 1, "append": "Appended by the agent", "scope": "global" }),
        Value::Null,
    );
    let held = HeldStore::in_folder(guarded.data()).hold(item).expect("held");

    let draft = json!({ "title": "Edited", "body": "Edited body" }).to_string();
    let changed = refused(&guarded, &["held", "save", &held, "--json"], &draft);
    assert_eq!(changed["error"], "changed");

    let again = waiting(&guarded, &held);
    assert_eq!(again["current"]["body"], "Body of the note", "the live note is shown");
    assert_eq!(listed(&guarded).len(), 1, "nothing was saved");
}

#[test]
fn a_note_edited_expecting_a_revision_is_changed_only_when_it_is_still_at_it() {
    let guarded = Guarded::new();
    let root = guarded.repo.to_string_lossy().into_owned();
    let saved = ok(
        &guarded,
        &["notes", "save", "--json", "--project", &root],
        r#"{ "title": "Edited note", "body": "first text", "kind": "decision" }"#,
    );
    let note_id = saved["id"].as_i64().expect("id");
    let id = note_id.to_string();
    let edit = |expect: &str, body: &str| {
        let patch = json!({ "body": body }).to_string();
        cli(
            &guarded,
            &[
                "notes",
                "update",
                &id,
                "--json",
                "--project",
                &root,
                "--expect-revision",
                expect,
            ],
            &patch,
        )
    };

    // The editor read revision 1, and the note is still at it.
    let output = edit("1", "second text");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    // Another editor, which still holds revision 1, does not overwrite the second text.
    let output = edit("1", "overwritten text");
    assert_eq!(output.status.code(), Some(1));
    let answer: Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(answer["error"], "changed");
    assert_eq!(answer["revision"], 2, "the revision the note is at now");
    assert!(
        answer["message"]
            .as_str()
            .expect("message")
            .contains("nothing was changed")
    );

    let mut reader = guarded.server(None, &guarded.repo);
    let reply = tool(&mut reader, "note_get", json!({ "ids": [note_id] }));
    let read = text(&reply);
    assert!(
        read.contains("second text") && !read.contains("overwritten text"),
        "{read}"
    );

    // A revision that is not a positive number is a usage error, and `notes save` takes none.
    assert_eq!(edit("0", "x").status.code(), Some(2));
    assert_eq!(edit("two", "x").status.code(), Some(2));
    let output = cli(
        &guarded,
        &["notes", "save", "--json", "--expect-revision", "1"],
        r#"{ "title": "x", "body": "y", "kind": "decision" }"#,
    );
    assert_eq!(output.status.code(), Some(2));
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

/// The author of the note `id` saved in the repository of `guarded`, as the list of the store shows it.
fn authors_of(guarded: &Guarded, id: i64) -> harness_memory::store::Authors {
    use harness_memory::store::{MemoryStore, SqliteStore};

    let mut store = SqliteStore::open(&guarded.data().join("memory.sqlite3")).expect("store");
    store
        .peek(&common::both("github.com/acme/widget"), &[id])
        .expect("peek")
        .remove(0)
        .authors
}

fn named(name: &str, via: Option<&str>) -> Option<harness_memory::store::Author> {
    Some(harness_memory::store::Author {
        name: name.to_owned(),
        via: via.map(str::to_owned),
    })
}

#[test]
fn a_held_save_is_written_as_the_author_it_had_when_it_was_held() {
    let guarded = asking();
    std::fs::write(
        guarded.dir.path().join("home/.gitconfig"),
        "[user]\n\tname = Ada Lovelace\n\temail = ada@example.com\n",
    )
    .expect("gitconfig");

    let mut chat = guarded.chat_server();
    let held = held_id(&tool(&mut chat, "note_save", note("Held by Ada")));

    let files = guarded.held_files();
    assert_eq!(files[0]["author"], json!({ "name": "Ada Lovelace", "via": "omp" }));
    assert!(!files[0].to_string().contains("ada@example.com"));

    // Someone else saves it, on another account.
    let other = guarded.dir.path().join("other");
    std::fs::create_dir_all(&other).expect("mkdir");
    std::fs::write(other.join(".gitconfig"), "[user]\n\tname = Grace Hopper\n").expect("gitconfig");

    let output = cli_at(&guarded, Some(&other), &["held", "save", &held, "--json"], "");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let saved: Value = serde_json::from_slice(&output.stdout).expect("JSON");
    let id = saved["saved"]["id"].as_i64().expect("id");

    let authors = authors_of(&guarded, id);
    assert_eq!(authors.added_by, named("Ada Lovelace", Some("omp")));
    assert_eq!(authors.updated_by, named("Ada Lovelace", Some("omp")));
}

#[test]
fn a_held_save_with_no_author_is_written_with_none() {
    let guarded = asking();
    let mut chat = guarded.chat_server();
    let held = held_id(&tool(&mut chat, "note_save", note("Held by nobody")));

    let files = guarded.held_files();
    assert!(
        files[0].get("author").is_none(),
        "an item without an author names none: {}",
        files[0]
    );

    // Saved on an account that does have a name: the held item is not given it.
    let other = guarded.dir.path().join("other");
    std::fs::create_dir_all(&other).expect("mkdir");
    std::fs::write(other.join(".gitconfig"), "[user]\n\tname = Grace Hopper\n").expect("gitconfig");

    let output = cli_at(&guarded, Some(&other), &["held", "save", &held, "--json"], "");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let saved: Value = serde_json::from_slice(&output.stdout).expect("JSON");

    let authors = authors_of(&guarded, saved["saved"]["id"].as_i64().expect("id"));
    assert_eq!(authors, harness_memory::store::Authors::default());
}

/// Rewrites the `author` of the one held file; the revision stays valid, as the author is no part of it.
fn set_held_author(guarded: &Guarded, author: Value) {
    let chat = std::fs::read_dir(guarded.held())
        .expect("held folder")
        .map(|entry| entry.expect("entry").path())
        .find(|path| path.is_dir())
        .expect("a chat folder");
    let file = std::fs::read_dir(chat)
        .expect("chat folder")
        .map(|entry| entry.expect("entry").path())
        .find(|path| path.extension().is_some_and(|extension| extension == "json"))
        .expect("a held file");

    let mut item: Value = serde_json::from_str(&std::fs::read_to_string(&file).expect("read")).expect("JSON");
    item["author"] = author;
    std::fs::write(file, item.to_string()).expect("write");
}

#[test]
fn the_author_of_a_held_file_is_cleaned_again_when_the_save_is_approved() {
    let guarded = asking();
    let mut chat = guarded.chat_server();
    let held = held_id(&tool(&mut chat, "note_save", note("Held then edited")));

    set_held_author(&guarded, json!({ "name": "  Mallory  ", "via": "omp\u{1b}[31m" }));

    let output = cli(&guarded, &["held", "save", &held, "--json"], "");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let saved: Value = serde_json::from_slice(&output.stdout).expect("JSON");
    let id = saved["saved"]["id"].as_i64().expect("id");

    let stored = authors_of(&guarded, id).added_by.expect("an author");
    assert_eq!(stored.name, "Mallory");
    assert!(
        stored.via.as_deref().is_none_or(|via| !via.contains('\u{1b}')),
        "{stored:?}"
    );

    // A name with control characters is no name.
    let held = held_id(&tool(&mut chat, "note_save", note("Held then edited again")));
    set_held_author(&guarded, json!({ "name": "Eve\u{1b}[2J", "via": null }));

    let output = cli(&guarded, &["held", "save", &held, "--json"], "");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let saved: Value = serde_json::from_slice(&output.stdout).expect("JSON");

    assert_eq!(
        authors_of(&guarded, saved["saved"]["id"].as_i64().expect("id")),
        harness_memory::store::Authors::default()
    );
}

#[test]
fn the_list_shows_a_cleaned_author_and_never_a_hand_edited_one() {
    let guarded = asking();
    let mut chat = guarded.chat_server();
    held_id(&tool(&mut chat, "note_save", note("Held then edited")));

    set_held_author(&guarded, json!({ "name": "  Mallory  ", "via": "omp\u{1b}[31m" }));
    let listed = ok(&guarded, &["held", "list", "--json"], "");
    let author = &listed["held"][0]["author"];
    assert_eq!(author["name"], "Mallory", "{listed}");
    assert!(
        author["via"].as_str().is_none_or(|via| !via.contains('\u{1b}')),
        "{listed}"
    );

    set_held_author(&guarded, json!({ "name": "Eve\u{1b}[2J", "via": null }));
    let listed = ok(&guarded, &["held", "list", "--json"], "");
    assert!(listed["held"][0].get("author").is_none(), "{listed}");
    assert!(!listed.to_string().contains("Eve"), "{listed}");
}

#[cfg(unix)]
/// The title of every note `project` can see.
fn titles_in(guarded: &Guarded, project: &Path) -> String {
    let mut server = guarded.server(None, project);
    text(&tool(
        &mut server,
        "note_search",
        json!({ "query": "Body of the note" }),
    ))
    .to_owned()
}

#[cfg(unix)]
/// Rewrites the one held file with `change`; the revision stays valid for the fields it leaves out of the digest.
fn change_held(guarded: &Guarded, change: impl FnOnce(&mut Value)) {
    let chat = std::fs::read_dir(guarded.held())
        .expect("held folder")
        .map(|entry| entry.expect("entry").path())
        .find(|path| path.is_dir())
        .expect("a chat folder");
    let file = std::fs::read_dir(chat)
        .expect("chat folder")
        .map(|entry| entry.expect("entry").path())
        .find(|path| path.extension().is_some_and(|extension| extension == "json"))
        .expect("a held file");

    let mut item: Value = serde_json::from_str(&std::fs::read_to_string(&file).expect("read")).expect("JSON");
    change(&mut item);
    std::fs::write(file, item.to_string()).expect("write");
}

#[cfg(unix)]
/// A held note naming `project_path` and, for the global scope, `scope`.
fn held_via(guarded: &Guarded, link: &Path, title: &str, scope: &str) -> String {
    let mut chat = guarded.server(Some(CHAT), &guarded.repo);
    let mut arguments = note(title);
    arguments["project_path"] = json!(link.to_string_lossy());
    arguments["scope"] = json!(scope);
    held_id(&tool(&mut chat, "note_save", arguments))
}

#[cfg(unix)]
fn point(link: &Path, target: &Path) {
    let _ = std::fs::remove_file(link);
    std::os::unix::fs::symlink(target, link).expect("link");
}

/// Puts a link to `target` where `folder` is: a real folder is moved aside first.
#[cfg(unix)]
fn link_over(folder: &Path, target: &Path) {
    if !std::fs::symlink_metadata(folder).expect("folder").is_symlink() {
        std::fs::rename(folder, folder.with_extension("real")).expect("move aside");
    }

    point(folder, target);
}

#[cfg(unix)]
#[test]
fn a_held_save_whose_project_folder_moved_is_changed_and_writes_nothing() {
    use common::opt_in::{commit, worktree};

    let guarded = asking();
    let other = common::mcp::canonical(&common::mcp::repo_with_remote(
        guarded.dir.path(),
        "other",
        "https://github.com/acme/other.git",
    ));
    commit(&guarded.repo);
    let sibling = worktree(&guarded.repo, "widget-sibling");
    let folder = guarded.repo.join("src").join("deep");

    let id = held_via(&guarded, &folder, "Through a link", "repo");
    let item = waiting(&guarded, &id);
    assert_eq!(item["projectRoot"], json!(guarded.repo.to_string_lossy()));
    assert_eq!(item["projectPath"], json!(folder.to_string_lossy()), "kept canonical");

    // The folder is replaced by a link before the user approves: nothing is written, the item waits.
    link_over(&folder, &other);
    let moved = refused(&guarded, &["held", "save", &id, "--json"], "");
    assert_eq!(moved["error"], "changed", "{moved}");
    assert_eq!(moved["reason"], "project", "{moved}");
    assert_eq!(listed(&guarded).len(), 1);
    assert!(!titles_in(&guarded, &other).contains("Through a link"));
    assert!(!titles_in(&guarded, &guarded.repo).contains("Through a link"));

    // The folder gone is changed too, and is not saved from the folder the command runs in.
    std::fs::remove_file(&folder).expect("remove");
    let gone = refused(&guarded, &["held", "save", &id, "--json"], "");
    assert_eq!(gone["error"], "changed", "{gone}");
    assert_eq!(listed(&guarded).len(), 1);

    // A worktree of the same repository is the same project, so it still saves.
    point(&folder, &sibling);
    assert_eq!(
        ok(&guarded, &["held", "save", &id, "--json"], "")["saved"]["kind"],
        "note"
    );
    assert!(titles_in(&guarded, &guarded.repo).contains("Through a link"));
    assert_eq!(listed(&guarded), Vec::<Value>::new());
}

#[cfg(unix)]
#[test]
fn a_held_item_without_a_project_root_is_checked_against_its_repo_root_and_a_global_one_is_not() {
    let guarded = asking();
    let other = common::mcp::canonical(&common::mcp::repo_with_remote(
        guarded.dir.path(),
        "other",
        "https://github.com/acme/other.git",
    ));
    let folder = guarded.repo.join("src").join("deep");

    // An item held before `projectRoot` existed.
    let id = held_via(&guarded, &folder, "Older item", "repo");
    change_held(&guarded, |item| {
        item.as_object_mut().expect("object").remove("projectRoot");
    });
    link_over(&folder, &other);
    assert_eq!(
        refused(&guarded, &["held", "save", &id, "--json"], "")["error"],
        "changed"
    );
    assert_eq!(listed(&guarded).len(), 1);
    assert!(!titles_in(&guarded, &other).contains("Older item"));

    // A global note does not depend on the project, so the retargeted link changes nothing.
    ok(&guarded, &["held", "skip", &id, "--json"], "");
    let id = held_via(&guarded, &folder, "Everywhere", "global");
    point(&folder, &guarded.repo);
    point(&folder, &other);
    assert_eq!(
        ok(&guarded, &["held", "save", &id, "--json"], "")["saved"]["kind"],
        "note"
    );
}

#[cfg(unix)]
/// `harness-memory <args>` with the home folder the in-process servers have (where `~/` leads).
fn cli_home(guarded: &Guarded, args: &[&str], input: &str) -> Output {
    cli_at(guarded, Some(&guarded.dir.path().join("home")), args, input)
}

#[cfg(unix)]
/// The refusal of `harness-memory <args>` run in the guarded home folder.
fn refused_home(guarded: &Guarded, args: &[&str], input: &str) -> Value {
    let output = cli_home(guarded, args, input);
    assert_eq!(output.status.code(), Some(1), "{args:?}");
    serde_json::from_slice(&output.stdout).expect("JSON")
}

#[cfg(unix)]
/// A held global script named `name` for `project_path`.
fn held_script_via(guarded: &Guarded, project_path: &str, name: &str) -> String {
    let mut chat = guarded.server(Some(CHAT), &guarded.repo);
    let script = json!({ "name": name, "description": "Says hi", "language": "sh", "body": "echo hi",
                         "scope": "global", "project_path": project_path });
    held_id(&tool(&mut chat, "script_save", script))
}

#[cfg(unix)]
/// The names of the scripts every project can see in `project`.
fn scripts_in(guarded: &Guarded, project: &Path) -> String {
    let mut server = guarded.server(None, project);
    text(&tool(&mut server, "script_list", json!({}))).to_owned()
}

#[cfg(unix)]
#[test]
fn a_draft_that_turns_a_global_item_into_a_repository_one_is_checked_against_the_project() {
    let guarded = asking();
    let home = guarded.dir.path().join("home");
    let off = common::mcp::canonical(&common::mcp::repo_with_remote(
        &home,
        "off",
        "https://github.com/acme/off.git",
    ));
    let draft = r#"{ "scope": "repo" }"#;

    // `~/off` is no absolute path for the guard, which checked the working folder: nothing of it
    // is kept, so the edit cannot reach `off` once `held save` expands the `~`.
    let note_id = held_via(&guarded, Path::new("~/off"), "Tilde note", "global");
    let script_id = held_script_via(&guarded, "~/off", "tilde-script");
    assert_eq!(waiting(&guarded, &note_id)["projectPath"], Value::Null);
    assert_eq!(
        waiting(&guarded, &note_id)["projectRoot"],
        json!(guarded.repo.to_string_lossy())
    );

    for (id, what) in [(&note_id, "note"), (&script_id, "script")] {
        let moved = refused_home(&guarded, &["held", "save", id, "--json"], draft);
        assert_eq!(moved["error"], "changed", "{what}: {moved}");
        assert_eq!(moved["reason"], "project", "{what}: {moved}");
    }

    assert_eq!(listed(&guarded).len(), 2, "both wait");

    for project in [&off, &guarded.repo] {
        assert!(!titles_in(&guarded, project).contains("Tilde note"));
        assert!(!scripts_in(&guarded, project).contains("tilde-script"));
    }

    // Saved as the agent asked, to the global scope, it needs no project.
    for id in [&note_id, &script_id] {
        let saved = cli_home(&guarded, &["held", "save", id, "--json"], "");
        assert!(saved.status.success(), "{}", String::from_utf8_lossy(&saved.stderr));
    }
}

#[cfg(unix)]
#[test]
fn a_draft_that_turns_a_global_item_into_a_repository_one_is_checked_after_a_link_moved() {
    let guarded = asking();
    let other = common::mcp::canonical(&common::mcp::repo_with_remote(
        guarded.dir.path(),
        "other",
        "https://github.com/acme/other.git",
    ));
    let folder = guarded.repo.join("src").join("deep");
    let note_id = held_via(&guarded, &folder, "Moved note", "global");
    let script_id = held_script_via(&guarded, &folder.to_string_lossy(), "moved-script");

    link_over(&folder, &other);

    for (id, what) in [(&note_id, "note"), (&script_id, "script")] {
        let moved = refused(&guarded, &["held", "save", id, "--json"], r#"{ "scope": "repo" }"#);
        assert_eq!(moved["error"], "changed", "{what}: {moved}");
        assert_eq!(moved["reason"], "project", "{what}: {moved}");
    }

    assert_eq!(listed(&guarded).len(), 2);
    assert!(!titles_in(&guarded, &other).contains("Moved note"));
    assert!(!scripts_in(&guarded, &other).contains("moved-script"));
    assert!(!titles_in(&guarded, &guarded.repo).contains("Moved note"));
}

#[cfg(unix)]
#[test]
fn a_held_repository_save_without_a_draft_in_an_unchanged_project_still_saves() {
    let guarded = asking();
    let folder = guarded.repo.join("src").join("deep");
    let id = held_via(&guarded, &folder, "Unchanged", "repo");

    assert_eq!(
        ok(&guarded, &["held", "save", &id, "--json"], "")["saved"]["kind"],
        "note"
    );
    assert!(titles_in(&guarded, &guarded.repo).contains("Unchanged"));

    // An edit that keeps the repository scope in an unchanged project saves too.
    let id = held_via(&guarded, &folder, "Edited", "repo");
    let saved = ok(
        &guarded,
        &["held", "save", &id, "--json"],
        r#"{ "scope": "repo", "body": "Body of the note" }"#,
    );
    assert_eq!(saved["saved"]["kind"], "note");
}

#[cfg(unix)]
#[test]
fn a_held_item_for_a_folder_that_is_no_project_compares_nothing_and_saves() {
    let guarded = Guarded::new();
    let mut chat = guarded.chat_server();
    assert!(!is_error(&tool(
        &mut chat,
        "note_save",
        json!({
            "title": "Everywhere", "body": "Body of the note", "kind": "decision", "scope": "global"
        })
    )));

    guarded.rules(false, (true, true), (true, false));
    guarded.desktop_alive();
    let home = guarded.dir.path().join("home");
    let id = held_id(&tool(
        &mut chat,
        "note_update",
        json!({ "id": 1, "body": "Newer body", "project_path": home.to_string_lossy() }),
    ));

    let item = waiting(&guarded, &id);
    assert_eq!(item["projectRoot"], Value::Null, "{item}");
    assert!(item.as_object().expect("object").contains_key("projectRoot"), "{item}");

    let saved = ok(&guarded, &["held", "save", &id, "--json"], "");
    assert_eq!(saved["saved"]["kind"], "note", "{saved}");
}

/// A held repository-scope note or script for each way a call can name the project, as `(what,
/// held id, the folder the item is tied to, a draft that edits it)`.
#[cfg(unix)]
fn held_for_root(guarded: &Guarded, what: &str, sibling: &Path) -> (String, std::path::PathBuf, &'static str) {
    let mut chat = guarded.server(Some(CHAT), &guarded.repo);
    let script = |extra: Value| {
        let mut script = json!({ "name": "root-script", "description": "Says hi", "language": "sh",
                                 "body": "echo hi", "scope": "repo" });
        script
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        script
    };
    let root = json!(guarded.repo.to_string_lossy());

    match what {
        "note at the root" => {
            let id = held_id(&tool(
                &mut chat,
                "note_save",
                json!({ "title": what, "body": "Body of the note",
                "kind": "decision", "project_path": root }),
            ));
            (id, guarded.repo.clone(), r#"{ "body": "Body of the note, edited" }"#)
        }
        "note of the working folder" => {
            let id = held_id(&tool(&mut chat, "note_save", note(what)));
            (id, guarded.repo.clone(), r#"{ "body": "Body of the note, edited" }"#)
        }
        "note of a worktree" => {
            let id = held_via(guarded, sibling, what, "repo");
            (id, sibling.to_path_buf(), r#"{ "body": "Body of the note, edited" }"#)
        }
        "script at the root" => {
            let id = held_id(&tool(&mut chat, "script_save", script(json!({ "project_path": root }))));
            (id, guarded.repo.clone(), r#"{ "description": "Edited" }"#)
        }
        _ => {
            let id = held_id(&tool(&mut chat, "script_save", script(json!({}))));
            (id, guarded.repo.clone(), r#"{ "description": "Edited" }"#)
        }
    }
}

#[cfg(unix)]
#[test]
fn a_held_save_whose_project_root_became_a_link_is_changed_and_kept_with_or_without_a_draft() {
    use common::opt_in::{commit, worktree};

    let cases = [
        "note at the root",
        "note of the working folder",
        "note of a worktree",
        "script at the root",
        "script of the working folder",
    ];

    for what in cases {
        let guarded = asking();
        let other = common::mcp::canonical(&common::mcp::repo_with_remote(
            guarded.dir.path(),
            "other",
            "https://github.com/acme/other.git",
        ));
        commit(&guarded.repo);
        let sibling = worktree(&guarded.repo, "widget-sibling");
        let (id, tied_to, draft) = held_for_root(&guarded, what, &sibling);

        // The folder the item is tied to is itself replaced by a link to another repository.
        link_over(&tied_to, &other);

        for input in ["", draft] {
            let moved = refused(&guarded, &["held", "save", &id, "--json"], input);
            assert_eq!(moved["error"], "changed", "{what} {input}: {moved}");
            assert_eq!(moved["reason"], "project", "{what} {input}: {moved}");
        }

        assert_eq!(listed(&guarded).len(), 1, "{what}: kept");
        assert!(!titles_in(&guarded, &other).contains(what), "{what}");
        assert!(!scripts_in(&guarded, &other).contains("root-script"), "{what}");
    }
}

/// A held repository-scope note or script for the subfolder `folder` of the repository: named in
/// `project_path`, or not named, then the chat's server works in `folder`.
#[cfg(unix)]
fn held_for_subfolder(guarded: &Guarded, what: &str, named: bool, folder: &Path) -> String {
    let mut chat = guarded.server(Some(CHAT), if named { &guarded.repo } else { folder });
    let path = json!(folder.to_string_lossy());

    if what == "note" {
        let mut arguments = note("Subfolder note");
        if named {
            arguments["project_path"] = path;
        }

        return held_id(&tool(&mut chat, "note_save", arguments));
    }

    let mut arguments = json!({ "name": "root-script", "description": "Says hi", "language": "sh",
                                "body": "echo hi", "scope": "repo" });
    if named {
        arguments["project_path"] = path;
    }

    held_id(&tool(&mut chat, "script_save", arguments))
}

/// The root is compared without following a link at it, so a root swapped for a link to a worktree
/// of another repository that holds the same subfolder does not read as the same project.
#[cfg(unix)]
#[test]
fn a_held_save_for_a_subfolder_is_changed_when_the_root_became_a_link_to_another_repositorys_worktree() {
    use common::opt_in::{commit, worktree};

    for (what, named, draft) in [
        ("note", true, r#"{ "body": "Body of the note, edited" }"#),
        ("note", false, r#"{ "body": "Body of the note, edited" }"#),
        ("script", true, r#"{ "description": "Edited" }"#),
        ("script", false, r#"{ "description": "Edited" }"#),
    ] {
        let guarded = asking();
        let other = common::mcp::canonical(&common::mcp::repo_with_remote(
            guarded.dir.path(),
            "other",
            "https://github.com/acme/other.git",
        ));
        commit(&other);
        let foreign = worktree(&other, "other-worktree");
        let folder = guarded.repo.join("src").join("deep");
        std::fs::create_dir_all(foreign.join("src").join("deep")).expect("mkdir");

        let id = held_for_subfolder(&guarded, what, named, &folder);
        let held = waiting(&guarded, &id);
        assert_eq!(
            held["projectRoot"],
            json!(guarded.repo.to_string_lossy()),
            "{what} {named}"
        );
        assert_eq!(held["projectPath"], json!(folder.to_string_lossy()), "{what} {named}");

        // The root itself is replaced by a link to the worktree of another repository, which has
        // the same subfolder.
        link_over(&guarded.repo, &foreign);

        for input in ["", draft] {
            let moved = refused(&guarded, &["held", "save", &id, "--json"], input);
            assert_eq!(moved["error"], "changed", "{what} {named} {input}: {moved}");
            assert_eq!(moved["reason"], "project", "{what} {named} {input}: {moved}");
        }

        assert_eq!(listed(&guarded).len(), 1, "{what} {named}: kept");
        assert!(
            !titles_in(&guarded, &other).contains("Subfolder note"),
            "{what} {named}"
        );
        assert!(!scripts_in(&guarded, &other).contains("root-script"), "{what} {named}");
    }
}

#[cfg(unix)]
#[test]
fn a_held_update_of_a_global_note_saves_after_its_folder_is_gone() {
    let guarded = Guarded::new();
    let mut chat = guarded.chat_server();
    assert!(!is_error(&tool(
        &mut chat,
        "note_save",
        json!({ "title": "Everywhere", "body": "Body of the note", "kind": "decision", "scope": "global" })
    )));

    guarded.rules(false, (true, true), (true, true));
    guarded.desktop_alive();

    // The update names no scope: only the note it changes says it is global.
    let folder = guarded.repo.join("src").join("deep");
    let id = held_id(&tool(
        &mut chat,
        "note_update",
        json!({ "id": 1, "body": "Newer body", "project_path": folder.to_string_lossy() }),
    ));
    assert_eq!(waiting(&guarded, &id)["current"]["scope"], "global");

    std::fs::remove_dir(&folder).expect("remove the empty folder");

    let saved = ok(&guarded, &["held", "save", &id, "--json"], "");
    assert_eq!(saved["saved"]["kind"], "note", "{saved}");
}

#[test]
fn a_place_is_refused_for_a_held_item_when_nothing_is_routed_remote_and_local_and_the_item_stays() {
    // A note to change, saved before the chat asks to review.
    let guarded = Guarded::new();
    let mut chat = guarded.chat_server();
    assert!(!is_error(&tool(&mut chat, "note_save", note("Existing note"))));
    guarded.rules(false, (true, true), (true, true));
    guarded.desktop_alive();

    let saved = held_id(&tool(&mut chat, "note_save", note("Held note")));
    let script = held_id(&tool(
        &mut chat,
        "script_save",
        json!({ "name": "hello", "description": "d", "language": "sh", "body": "echo hi" }),
    ));
    let update = held_id(&tool(&mut chat, "note_update", json!({ "id": 1, "append": "more" })));

    // No team database is set, so everything is on this computer and there is no place to pick.
    for id in [&saved, &script, &update] {
        for place in ["remote", "local"] {
            let said = refused(&guarded, &["held", "save", id, "--json", "--place", place], "");
            assert_eq!(said["error"], "invalid", "{id} {place}: {said}");
        }
    }

    let bad = refused(&guarded, &["held", "save", &saved, "--json", "--place", "nowhere"], "");
    assert_eq!(bad["error"], "invalid", "{bad}");
    assert!(bad["message"].as_str().expect("message").contains("--place"), "{bad}");

    let listed = ok(&guarded, &["held", "list", "--json"], "");
    assert_eq!(
        listed["held"].as_array().map(Vec::len),
        Some(3),
        "every refused item is kept: {listed}"
    );

    // Without a place they save as before.
    let done = ok(&guarded, &["held", "save", &saved, "--json"], "");
    assert_eq!(done["saved"]["kind"], "note");
    assert!(done["saved"].get("place").is_none(), "{done}");
}
