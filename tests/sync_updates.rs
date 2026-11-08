//! Changes to team notes: made while the team database is down they are queued and applied when it
//! is back (once, without overwriting a teammate), an editor that read an older revision is
//! refused, and what the user approves from the review of a chat is saved the same way.
//!
//! The cases run on every live backend whose test variable is set (PostgreSQL, libSQL, MongoDB
//! and MongoDB in the DocumentDB mode) and skip with a message otherwise.

mod common;
mod libsql_support;
mod mongo_support;
mod postgres_support;
mod sync_support;

use serde_json::json;
use sync_support::{Kind, Place, User, World, local_counts, local_notes, local_titles, pass_a_second, waiting};

/// A chat the desktop reviews the saves of.
const CHAT: &str = "1b4e28ba-2fa1-4d3b-a3f5-ef19b5a7633b";

fn restore(world: &World, users: &[&User]) {
    world.proxy.restore();

    for user in users {
        user.end_cooldown();
    }
}

/// The id of the team note `title`, as `reader` reads it.
fn team_id(reader: &User, title: &str) -> i64 {
    reader.notes(Place::Team).one(title).id
}

fn count(text: &str, part: &str) -> usize {
    text.matches(part).count()
}

fn queued_changes_are_applied_after_the_restore_once_and_in_order(kind: Kind) {
    let Some(world) = World::open(kind, "queue") else {
        return;
    };
    let (ada, bob) = (world.user("Ada"), world.teammate("Bob"));

    for (title, body) in [
        ("Append target", "start"),
        ("Replace target", "old body"),
        ("Chain", "v1"),
    ] {
        bob.save_note(title, body, "repo");
    }
    let (append, replace, chain) = (
        team_id(&bob, "Append target"),
        team_id(&bob, "Replace target"),
        team_id(&bob, "Chain"),
    );

    world.proxy.cut();
    let mut chat = ada.agent();

    let queued = chat.ok(
        "note_update",
        json!({ "id": append, "append": "plus the appended line" }),
    );
    assert!(queued.text.contains("queued"), "{}", queued.text);
    let queued = chat.ok(
        "note_update",
        json!({ "id": replace, "title": "Replaced title", "body": "new body" }),
    );
    assert!(queued.text.contains("queued"), "{}", queued.text);

    // One person's changes in a row do not fall out with each other.
    chat.ok("note_update", json!({ "id": chain, "body": "v2" }));
    chat.ok("note_update", json!({ "id": chain, "body": "v3" }));
    chat.end();

    assert_eq!(waiting(&ada), (0, 0, 4));
    let offline = ada.ok(&["sync", "--json"], "");
    assert_eq!(
        offline,
        json!({ "online": false, "waiting": { "notes": 0, "scripts": 0, "updates": 4 } })
    );
    assert_eq!(bob.body(append), "start", "the team's note is not changed yet");

    restore(&world, &[&ada]);
    let synced = ada.ok(&["sync", "--json"], "");
    assert_eq!(
        synced,
        json!({ "online": true, "waiting": { "notes": 0, "scripts": 0, "updates": 0 } })
    );

    assert_eq!(bob.team_titles(), ["Append target", "Chain", "Replaced title"]);
    assert_eq!(bob.body(append), "start\nplus the appended line");
    assert_eq!(bob.body(replace), "new body");
    assert_eq!(bob.body(chain), "v3");
    assert_eq!(
        local_counts(&ada),
        (0, 0),
        "no change fell out as a note on this computer"
    );

    // Nothing is applied a second time, by a sync or by the next server.
    ada.ok(&["sync", "--json"], "");
    let mut next = ada.agent();
    next.ok("note_search", json!({ "query": "Chain" }));
    next.end();
    assert_eq!(count(&bob.body(append), "plus the appended line"), 1);
    assert_eq!(bob.body(chain), "v3");
}

fn a_teammates_change_wins_and_the_offline_text_is_kept_as_a_note_here(kind: Kind) {
    let Some(world) = World::open(kind, "contested") else {
        return;
    };
    let (ada, bob) = (world.user("Ada"), world.teammate("Bob"));

    bob.save_note("Contested", "the base text", "repo");
    let contested = team_id(&bob, "Contested");

    world.proxy.cut();
    let mut chat = ada.agent();
    let queued = chat.ok("note_update", json!({ "id": contested, "body": "Ada's offline text" }));
    assert!(queued.text.contains("queued"), "{}", queued.text);
    chat.end();

    // Bob changes the note after Ada's change was made.
    pass_a_second();
    bob.ok(
        &["notes", "update", &contested.to_string(), "--json"],
        r#"{ "body": "Bob's text" }"#,
    );

    restore(&world, &[&ada]);
    ada.ok(&["sync", "--json"], "");

    assert_eq!(bob.body(contested), "Bob's text", "the teammate's change stays");
    assert_eq!(waiting(&ada), (0, 0, 0), "the queue is empty");

    let kept = local_notes(&ada);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(kept[0].0.contains("offline change"), "{kept:?}");
    assert_eq!(kept[0].1, "Ada's offline text");
    assert_eq!(bob.team_titles(), ["Contested"]);
}

fn a_note_deleted_meanwhile_keeps_the_offline_text_here(kind: Kind) {
    let Some(world) = World::open(kind, "deleted") else {
        return;
    };
    let (ada, bob) = (world.user("Ada"), world.teammate("Bob"));

    bob.save_note("Short lived", "soon gone", "repo");
    let doomed = team_id(&bob, "Short lived");

    world.proxy.cut();
    let mut chat = ada.agent();
    chat.ok("note_update", json!({ "id": doomed, "append": "Ada's finding" }));
    chat.end();

    let deleted = bob.run(&["notes", "delete", &doomed.to_string(), "--all-repos"], "");
    assert_eq!(
        deleted.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&deleted.stderr)
    );

    restore(&world, &[&ada]);
    ada.ok(&["sync", "--json"], "");

    assert!(bob.team_titles().is_empty(), "the note stays deleted");
    assert_eq!(waiting(&ada), (0, 0, 0));

    let kept = local_notes(&ada);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(kept[0].1.contains("Ada's finding"), "{kept:?}");
}

fn deleting_a_team_note_while_the_database_is_down_is_refused_and_changes_nothing(kind: Kind) {
    let Some(world) = World::open(kind, "no-delete") else {
        return;
    };
    let (ada, bob) = (world.user("Ada"), world.teammate("Bob"));

    bob.save_note("Keep me", "the team needs it", "repo");
    let kept = team_id(&bob, "Keep me");

    world.proxy.cut();

    let output = ada.run(&["notes", "delete", &kept.to_string(), "--all-repos"], "");
    assert_eq!(output.status.code(), Some(1));
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("unreachable") && said.contains("nothing was changed"),
        "{said}"
    );
    assert!(!said.contains("not found"), "{said}");

    let mut chat = ada.agent();
    let refused = chat.tool("note_delete", json!({ "id": kept }));
    assert!(refused.is_error, "{}", refused.text);
    assert!(refused.text.contains("nothing was changed"), "{}", refused.text);
    chat.end();

    restore(&world, &[&ada]);
    ada.ok(&["sync", "--json"], "");

    assert_eq!(bob.team_titles(), ["Keep me"]);
    assert_eq!(waiting(&ada), (0, 0, 0));
}

fn an_editor_that_read_an_older_revision_is_refused_and_a_current_one_is_saved(kind: Kind) {
    let Some(world) = World::open(kind, "stale") else {
        return;
    };
    let (ada, bob) = (world.teammate("Ada"), world.teammate("Bob"));

    ada.save_note("Edited together", "first text", "repo");
    let note = ada.notes(Place::Team).one("Edited together").clone();
    let id = note.id.to_string();

    // Ada opened the note at this revision: her save goes through.
    let revision = note.revision.to_string();
    let saved = ada.ok(
        &["notes", "update", &id, "--json", "--expect-revision", &revision],
        r#"{ "body": "Ada's edit" }"#,
    );
    assert_eq!(saved["id"], note.id);
    let after = ada.notes(Place::Team).one("Edited together").revision;
    assert_eq!(after, note.revision + 1);
    assert_eq!(bob.body(note.id), "Ada's edit");

    // Bob still holds the older revision: refused, and nothing is changed.
    let refused = bob.json(
        &["notes", "update", &id, "--json", "--expect-revision", &revision],
        r#"{ "body": "Bob's late edit" }"#,
        1,
    );
    assert_eq!(refused["error"], "changed", "{refused}");
    assert_eq!(refused["revision"], after, "{refused}");
    assert_eq!(ada.body(note.id), "Ada's edit");
    assert_eq!(ada.notes(Place::Team).one("Edited together").revision, after);

    // Reading the revision again lets Bob save.
    let current = after.to_string();
    bob.ok(
        &["notes", "update", &id, "--json", "--expect-revision", &current],
        r#"{ "body": "Bob's edit" }"#,
    );
    assert_eq!(ada.body(note.id), "Bob's edit");
}

fn a_held_save_approved_during_an_outage_reaches_the_remote_after_the_restore(kind: Kind) {
    let Some(world) = World::open(kind, "held-save") else {
        return;
    };
    let ada = world.user("Ada").in_chat(CHAT);
    let bob = world.teammate("Bob");
    ada.rules_for_chat(CHAT, true);

    let mut chat = ada.agent();
    let mut held = Vec::new();

    for (title, body) in [
        ("Held as drafted", "the agent's text"),
        ("Held then edited", "the agent's draft"),
    ] {
        let reply = chat.save_note(title, body);
        assert_eq!(reply.structured["held"], true, "{}", reply.text);
        held.push(reply.structured["heldId"].as_str().expect("a held id").to_owned());
    }

    let script = chat.save_script("held-script");
    assert_eq!(script.structured["held"], true, "{}", script.text);
    let held_script = script.structured["heldId"].as_str().expect("a held id").to_owned();
    assert!(
        bob.team_titles().is_empty(),
        "nothing is saved before the user approves"
    );

    world.proxy.cut();

    let saved = ada.ok(&["held", "save", &held[0], "--json"], "");
    assert_eq!(saved["saved"]["kind"], "note", "{saved}");
    let edited = ada.ok(
        &["held", "save", &held[1], "--json"],
        r#"{ "title": "Edited draft", "body": "the user's text" }"#,
    );
    assert_eq!(edited["saved"]["kind"], "note", "{edited}");
    let saved = ada.ok(&["held", "save", &held_script, "--json"], "");
    assert_eq!(saved["saved"]["kind"], "script", "{saved}");

    assert_eq!(waiting(&ada), (2, 1, 0));
    assert_eq!(ada.ok(&["held", "list", "--json"], ""), json!({ "held": [] }));

    restore(&world, &[&ada]);
    ada.ok(&["sync", "--json"], "");

    assert_eq!(bob.team_titles(), ["Edited draft", "Held as drafted"]);
    let edited = bob.notes(Place::Team).one("Edited draft").id;
    assert_eq!(
        bob.body(edited),
        "the user's text",
        "what the user edited, not what the agent drafted"
    );
    assert_eq!(bob.team_script_names(), ["held-script"]);
    assert_eq!(waiting(&ada), (0, 0, 0));
    assert_eq!(local_counts(&ada), (0, 0));
    chat.end();
}

fn a_held_update_approved_during_an_outage_is_queued_and_applied_once(kind: Kind) {
    let Some(world) = World::open(kind, "held-update") else {
        return;
    };
    let ada = world.user("Ada").in_chat(CHAT);
    let bob = world.teammate("Bob");
    ada.rules_for_chat(CHAT, true);

    bob.save_note("Held target", "the team's text", "repo");
    let target = team_id(&bob, "Held target");

    let mut chat = ada.agent();
    let reply = chat.tool("note_update", json!({ "id": target, "append": "plus the held line" }));
    assert_eq!(reply.structured["held"], true, "{}", reply.text);
    let held = reply.structured["heldId"].as_str().expect("a held id").to_owned();

    world.proxy.cut();
    let saved = ada.ok(&["held", "save", &held, "--json"], "");
    assert_eq!(saved["saved"]["queued"], true, "{saved}");
    assert_eq!(waiting(&ada), (0, 0, 1));
    assert_eq!(local_titles(&ada), Vec::<String>::new());

    restore(&world, &[&ada]);
    ada.ok(&["sync", "--json"], "");
    ada.ok(&["sync", "--json"], "");

    assert_eq!(bob.body(target), "the team's text\nplus the held line");
    assert_eq!(waiting(&ada), (0, 0, 0));
    chat.end();
}

sync_cases!(
    queued_changes_are_applied_after_the_restore_once_and_in_order,
    a_teammates_change_wins_and_the_offline_text_is_kept_as_a_note_here,
    a_note_deleted_meanwhile_keeps_the_offline_text_here,
    deleting_a_team_note_while_the_database_is_down_is_refused_and_changes_nothing,
    an_editor_that_read_an_older_revision_is_refused_and_a_current_one_is_saved,
    a_held_save_approved_during_an_outage_reaches_the_remote_after_the_restore,
    a_held_update_approved_during_an_outage_is_queued_and_applied_once,
);
