//! Copying between this computer and the team database for the scopes set to `remote+local`: a
//! reviewed push copies and removes only what the user ticked, a pull copies only the picked entries
//! and never writes the team database, and a plain push leaves those scopes alone.
//!
//! The cases run on every live backend whose test variable is set (PostgreSQL, libSQL, MongoDB
//! and MongoDB in the DocumentDB mode) and skip with a message otherwise.

mod common;
mod libsql_support;
mod mongo_support;
mod postgres_support;
mod sync_support;

use std::process::Output;

use serde_json::{Value, json};
use sync_support::{
    Kind, Place, REMOTE_LOCAL, User, World, local_notes, local_script_names, local_titles, pass_a_second, waiting,
};

/// The local id the preview lists for `label` (a title or a script name).
fn local_id(preview: &Value, label: &str) -> i64 {
    let key = if preview["changes"][0].get("title").is_some() {
        "title"
    } else {
        "name"
    };

    preview["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .find(|change| change[key] == label)
        .unwrap_or_else(|| panic!("no change for {label}: {preview}"))["localId"]
        .as_i64()
        .expect("a local id")
}

/// The entry of `only` the preview's change for `label` becomes: its id, the title or name and the
/// update time exactly as the preview printed them.
fn reviewed(preview: &Value, label: &str) -> Value {
    let key = if preview["changes"][0].get("title").is_some() {
        "title"
    } else {
        "name"
    };

    let change = preview["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .find(|change| change[key] == label)
        .unwrap_or_else(|| panic!("no change for {label}: {preview}"));

    json!({ "id": change["localId"], "name": label, "updatedAt": change["localUpdatedAt"] })
}

/// `(label, action, deleted)` of every change, sorted.
fn outcomes(pushed: &Value) -> Vec<(String, String, bool)> {
    let mut changes: Vec<(String, String, bool)> = pushed["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .map(|change| {
            let label = change
                .get("title")
                .or_else(|| change.get("name"))
                .and_then(Value::as_str);
            (
                label.expect("a label").to_owned(),
                change["action"].as_str().expect("action").to_owned(),
                change["deleted"].as_bool().expect("deleted"),
            )
        })
        .collect();
    changes.sort();
    changes
}

fn outcome(label: &str, action: &str, deleted: bool) -> (String, String, bool) {
    (label.to_owned(), action.to_owned(), deleted)
}

/// `{"only": ..., "overwrite": ...}`, as a review hands it to `--selection -`.
fn selection(only: &[Value], overwrite: &[i64]) -> String {
    json!({ "only": only, "overwrite": overwrite }).to_string()
}

fn script(user: &User, name: &str, body: &str) {
    let draft = json!({ "name": name, "description": "d", "language": "sh", "body": body, "scope": "repo" });
    user.ok(&["scripts", "save", "--json"], &draft.to_string());
}

/// Everything the team database holds of notes and scripts, as `reader` reads it.
fn team_state(reader: &User) -> Vec<(String, i64, String)> {
    let mut state = Vec::new();

    for row in reader.notes(Place::Team).rows {
        state.push((format!("note {}", row.title), row.revision, reader.body(row.id)));
    }

    for row in reader.scripts(Place::Team).rows {
        state.push((format!("script {}", row.title), row.revision, row.id.to_string()));
    }

    state.sort();
    state
}

fn exit_of(output: &Output) -> Option<i32> {
    output.status.code()
}

fn a_reviewed_push_copies_and_removes_only_what_was_ticked(kind: Kind) {
    let Some(world) = World::open(kind, "reviewed") else {
        return;
    };
    let ada = world.teammate("Ada").with_places(REMOTE_LOCAL);
    let bob = world.teammate("Bob");

    for title in ["Conflict kept", "Conflict ticked", "Conflict unlisted"] {
        bob.save_note(title, "the team's text", "repo");
    }
    pass_a_second();

    for title in ["Conflict kept", "Conflict ticked", "Conflict unlisted"] {
        ada.save_note(title, "Ada's newer text", "repo");
    }
    ada.save_note("Review one", "to copy", "repo");
    ada.save_note("Review two", "to copy as well", "repo");
    ada.save_note("Review global", "a global one", "global");

    // The review: every note is listed, conflicts marked.
    let preview = ada.ok(&["push", "--json", "--dry-run", "--remote-local"], "");
    assert_eq!(preview["notes"]["added"], 3, "{preview}");
    assert_eq!(preview["notes"]["overwritten"], 3, "{preview}");
    let entries = |labels: &[&str]| -> Vec<Value> { labels.iter().map(|label| reviewed(&preview, label)).collect() };
    let ticked = entries(&[
        "Review one",
        "Review two",
        "Review global",
        "Conflict kept",
        "Conflict ticked",
    ]);
    let overwrite = [local_id(&preview, "Conflict ticked")];

    // An agent saves a note before the user applies the review.
    ada.save_note("Saved after the review", "not reviewed", "repo");

    let apply = [
        "push",
        "--json",
        "--remote-local",
        "--delete-copied",
        "--selection",
        "-",
    ];
    let pushed = ada.ok(&apply, &selection(&ticked, &overwrite));

    assert_eq!(
        outcomes(&pushed),
        [
            outcome("Conflict kept", "keep", false),
            outcome("Conflict ticked", "overwrite", true),
            outcome("Review global", "add", true),
            outcome("Review one", "add", true),
            outcome("Review two", "add", true),
        ],
        "{pushed}"
    );
    assert_eq!(pushed["deleted"], 4, "{pushed}");
    assert_eq!(pushed["failed"], json!([]));

    let mut wanted = [
        "Conflict kept",
        "Conflict ticked",
        "Conflict unlisted",
        "Review global",
        "Review one",
        "Review two",
    ];
    wanted.sort_unstable();
    assert_eq!(bob.team_titles(), wanted, "nothing else reached the team");
    let body = |title: &str| bob.body(bob.notes(Place::Team).one(title).id);
    assert_eq!(
        body("Conflict ticked"),
        "Ada's newer text",
        "the ticked conflict was overwritten"
    );
    assert_eq!(
        body("Conflict kept"),
        "the team's text",
        "the unticked conflict keeps the team's text"
    );
    assert_eq!(body("Conflict unlisted"), "the team's text");
    assert_eq!(body("Review one"), "to copy");

    assert_eq!(
        local_titles(&ada),
        ["Conflict kept", "Conflict unlisted", "Saved after the review"],
        "the copied entries are gone from this computer, the others stay"
    );
    assert_eq!(
        waiting(&ada),
        (0, 0, 0),
        "nothing of this goes through the waiting list"
    );

    // An empty selection copies nothing.
    let before = team_state(&bob);
    let nothing = ada.ok(&apply, &selection(&[], &[]));
    assert_eq!(nothing["changes"], json!([]), "{nothing}");
    assert_eq!(nothing["deleted"], 0, "{nothing}");
    assert_eq!(team_state(&bob), before);
    assert_eq!(local_titles(&ada).len(), 3);

    // A plain push leaves the scopes set to remote+local alone.
    let plain = ada.ok(&["push", "--json"], "");
    assert_eq!(plain["changes"], json!([]), "{plain}");
    assert!(
        plain["nothing"]
            .as_str()
            .is_some_and(|said| said.contains("remote+local")),
        "{plain}"
    );
    assert_eq!(team_state(&bob), before);
    assert_eq!(local_titles(&ada).len(), 3);
}

fn a_reviewed_scripts_push_copies_and_removes_only_what_was_ticked(kind: Kind) {
    let Some(world) = World::open(kind, "reviewed-scripts") else {
        return;
    };
    let ada = world.teammate("Ada").with_places(REMOTE_LOCAL);
    let bob = world.teammate("Bob");

    script(&bob, "conflict-kept", "echo team");
    script(&bob, "conflict-ticked", "echo team");
    pass_a_second();

    script(&ada, "conflict-kept", "echo ada");
    script(&ada, "conflict-ticked", "echo ada");
    script(&ada, "review-one", "echo one");
    script(&ada, "review-two", "echo two");

    let preview = ada.ok(&["scripts", "push", "--json", "--dry-run", "--remote-local"], "");
    let entries = |labels: &[&str]| -> Vec<Value> { labels.iter().map(|label| reviewed(&preview, label)).collect() };
    let ticked = entries(&["review-one", "conflict-kept", "conflict-ticked"]);
    let overwrite = [local_id(&preview, "conflict-ticked")];

    script(&ada, "saved-after-the-review", "echo later");

    let apply = [
        "scripts",
        "push",
        "--json",
        "--remote-local",
        "--delete-copied",
        "--selection",
        "-",
    ];
    let pushed = ada.ok(&apply, &selection(&ticked, &overwrite));

    assert_eq!(
        outcomes(&pushed),
        [
            outcome("conflict-kept", "keep", false),
            outcome("conflict-ticked", "overwrite", true),
            outcome("review-one", "add", true),
        ],
        "{pushed}"
    );
    assert_eq!(
        bob.team_script_names(),
        ["conflict-kept", "conflict-ticked", "review-one"]
    );
    assert_eq!(
        local_script_names(&ada),
        ["conflict-kept", "review-two", "saved-after-the-review"]
    );

    let before = team_state(&bob);
    let nothing = ada.ok(&apply, &selection(&[], &[]));
    assert_eq!(nothing["changes"], json!([]), "{nothing}");
    assert_eq!(team_state(&bob), before);
    assert_eq!(local_script_names(&ada).len(), 3);
}

fn a_pull_copies_only_the_picked_ids_and_never_writes_the_team_database(kind: Kind) {
    let Some(world) = World::open(kind, "pull") else { return };
    let ada = world.teammate("Ada").with_places(REMOTE_LOCAL);
    let bob = world.teammate("Bob");

    bob.save_note("Pick me", "a repository note", "repo");
    bob.save_note("Pick me too", "a global note", "global");
    bob.save_note("Not picked", "nobody asked for it", "repo");
    bob.save_note("Hide me", "hidden on Ada's computer", "repo");
    bob.save_note("Same text", "identical on both", "repo");
    bob.save_note("Differs", "the team's version", "repo");

    ada.save_note("Same text", "identical on both", "repo");
    ada.save_note("Differs", "Ada's own version", "repo");

    let team = bob.notes(Place::Team);
    let id = |title: &str| team.one(title).id;
    ada.ok(
        &["notes", "hide", &id("Hide me").to_string(), "--all-repos", "--json"],
        "",
    );

    let before = team_state(&bob);
    let picked: Vec<String> = ["Pick me", "Pick me too", "Same text", "Differs", "Hide me"]
        .iter()
        .map(|title| id(title).to_string())
        .collect();
    let output = ada.run(&["pull", "--json", "--ids", "-"], &picked.join("\n"));
    let pulled: Value = serde_json::from_slice(&output.stdout).expect("one JSON object");

    assert_eq!(pulled["notes"]["added"], 2, "{pulled}");
    assert_eq!(pulled["notes"]["identical"], 1, "{pulled}");
    assert_eq!(pulled["notes"]["localDiffers"], 1, "{pulled}");
    assert_eq!(pulled["notes"]["failed"], 1, "{pulled}");
    assert_eq!(exit_of(&output), Some(1), "a refused entry is reported: {pulled}");
    let failed = pulled["failed"].as_array().expect("failed");
    assert_eq!(failed.len(), 1, "{pulled}");
    assert_eq!(failed[0]["teamId"], id("Hide me"), "{pulled}");

    assert_eq!(
        local_titles(&ada),
        ["Differs", "Pick me", "Pick me too", "Same text"],
        "only the picked, visible entries were copied"
    );
    let kept: Vec<(String, String)> = local_notes(&ada)
        .into_iter()
        .filter(|(title, _)| title == "Differs")
        .collect();
    assert_eq!(
        kept,
        [("Differs".to_owned(), "Ada's own version".to_owned())],
        "a note that differs is left alone"
    );

    assert_eq!(team_state(&bob), before, "the team database was not written");
    assert_eq!(waiting(&ada), (0, 0, 0), "a pulled note does not wait to go back");

    // Pulling the same ids again changes nothing here or there.
    let again = ada.run(&["pull", "--json", "--ids", "-"], &picked.join("\n"));
    let again: Value = serde_json::from_slice(&again.stdout).expect("one JSON object");
    assert_eq!(again["notes"]["added"], 0, "{again}");
    assert_eq!(local_titles(&ada).len(), 4);
    assert_eq!(team_state(&bob), before);
}

fn a_scripts_pull_copies_only_the_picked_ids_and_leaves_the_copies_untrusted(kind: Kind) {
    let Some(world) = World::open(kind, "pull-scripts") else {
        return;
    };
    let ada = world.teammate("Ada").with_places(REMOTE_LOCAL);
    let bob = world.teammate("Bob");

    script(&bob, "picked", "echo picked");
    script(&bob, "unpicked", "echo unpicked");
    script(&bob, "same", "echo same");
    script(&bob, "differs", "echo team");
    script(&ada, "same", "echo same");
    script(&ada, "differs", "echo ada");

    let team = bob.scripts(Place::Team);
    let before = team_state(&bob);
    let picked = [team.one("picked").id, team.one("same").id, team.one("differs").id];
    let ids: Vec<String> = picked.iter().map(i64::to_string).collect();

    let pulled = ada.ok(&["scripts", "pull", "--json", "--ids", "-"], &ids.join("\n"));
    assert_eq!(pulled["scripts"]["added"], 1, "{pulled}");
    assert_eq!(pulled["scripts"]["identical"], 1, "{pulled}");
    assert_eq!(pulled["scripts"]["localDiffers"], 1, "{pulled}");

    assert_eq!(local_script_names(&ada), ["differs", "picked", "same"]);
    let local = ada.scripts(Place::Local);
    assert_eq!(
        local.one("picked").trusted,
        Some(false),
        "a copy needs the user's approval before it runs"
    );
    assert_eq!(team_state(&bob), before, "the team database was not written");
    assert_eq!(bob.team_script_names(), ["differs", "picked", "same", "unpicked"]);
}

sync_cases!(
    a_reviewed_push_copies_and_removes_only_what_was_ticked,
    a_reviewed_scripts_push_copies_and_removes_only_what_was_ticked,
    a_pull_copies_only_the_picked_ids_and_never_writes_the_team_database,
    a_scripts_pull_copies_only_the_picked_ids_and_leaves_the_copies_untrusted,
);
