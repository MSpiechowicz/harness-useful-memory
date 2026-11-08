//! `sync --json` (what Harness Desktop runs when it sees the team database again), `discard-local`
//! next to what still waits for the team database, and two people saving at once.
//!
//! The cases run on every live backend whose test variable is set (PostgreSQL, libSQL, MongoDB
//! and MongoDB in the DocumentDB mode) and skip with a message otherwise.

mod common;
mod libsql_support;
mod mongo_support;
mod postgres_support;
mod sync_support;

use serde_json::json;
use sync_support::{
    Agent, Kind, REMOTE, REMOTE_LOCAL, User, World, local_counts, local_script_names, local_titles, waiting,
};

fn nothing_waits() -> serde_json::Value {
    json!({ "online": true, "waiting": { "notes": 0, "scripts": 0, "updates": 0 } })
}

fn restore(world: &World, users: &[&User]) {
    world.proxy.restore();

    for user in users {
        user.end_cooldown();
    }
}

fn sync_reports_waiting_work_while_offline_and_drains_it_after_the_restore(kind: Kind) {
    let Some(world) = World::open(kind, "sync-drain") else {
        return;
    };
    let (ada, bob) = (world.user("Ada"), world.teammate("Bob"));

    bob.save_note("Team note", "the team's text", "repo");
    let team_note = bob.notes(sync_support::Place::Team).one("Team note").id;

    world.proxy.cut();
    let mut chat = ada.agent();
    assert!(!chat.save_note("Offline note", "kept here").is_error);
    assert!(!chat.save_script("offline-script").is_error);
    assert!(
        !chat
            .tool("note_update", json!({ "id": team_note, "append": "queued line" }))
            .is_error
    );
    chat.end();

    // One try finds the database still down: everything is reported, nothing is lost.
    let offline = ada.ok(&["sync", "--json"], "");
    assert_eq!(
        offline,
        json!({ "online": false, "waiting": { "notes": 1, "scripts": 1, "updates": 1 } })
    );
    assert_eq!(waiting(&ada), (1, 1, 1));

    restore(&world, &[&ada]);
    assert_eq!(ada.ok(&["sync", "--json"], ""), nothing_waits());

    assert_eq!(bob.team_titles(), ["Offline note", "Team note"]);
    assert_eq!(bob.team_script_names(), ["offline-script"]);
    assert_eq!(bob.body(team_note), "the team's text\nqueued line");
    assert_eq!(waiting(&ada), (0, 0, 0));
    assert_eq!(local_counts(&ada), (0, 0));
}

fn sync_with_nothing_waiting_makes_no_connection(kind: Kind) {
    let Some(world) = World::open(kind, "sync-quiet") else {
        return;
    };
    let ada = world.user("Ada");

    // Nothing was ever saved offline: the team database is not asked.
    assert_eq!(ada.ok(&["sync", "--json"], ""), nothing_waits());
    assert_eq!(world.proxy.connections(), 0, "no connection was made");

    // Not even when it is down, and the next process is not sent away for it.
    world.proxy.cut();
    assert_eq!(ada.ok(&["sync", "--json"], ""), nothing_waits());
    assert!(!ada.is_cooling());
    world.proxy.restore();

    // After a drain, the lists are empty and a sync is quiet again.
    let mut chat = ada.agent();
    chat.save_note("Online note", "reaches the team at once");
    chat.end();
    let connections = world.proxy.connections();
    assert!(connections > 0, "the save did use the network");

    assert_eq!(ada.ok(&["sync", "--json"], ""), nothing_waits());
    assert_eq!(
        world.proxy.connections(),
        connections,
        "a sync with nothing waiting makes no connection"
    );
}

fn discard_local_keeps_what_still_waits_for_the_team_database(kind: Kind) {
    let Some(world) = World::open(kind, "discard") else {
        return;
    };
    let bob = world.teammate("Bob");
    let mut ada = world.user("Ada").with_places(REMOTE_LOCAL);

    // Entries kept on this computer on purpose, until the scopes were set to remote.
    ada.save_note("Kept here", "saved while the scope was local", "repo");
    let draft =
        json!({ "name": "kept-script", "description": "d", "language": "sh", "body": "echo kept", "scope": "repo" });
    ada.ok(&["scripts", "save", "--json"], &draft.to_string());
    ada.set_places(REMOTE);

    world.proxy.cut();
    let mut chat = ada.agent();
    assert!(!chat.save_note("Waiting note", "waits for the team").is_error);
    assert!(!chat.save_script("waiting-script").is_error);
    chat.end();
    assert_eq!(waiting(&ada), (1, 1, 0));
    assert_eq!(local_counts(&ada), (2, 2));

    let notes = ada.ok(&["notes", "discard-local", "--all", "--yes", "--json"], "");
    assert_eq!(notes, json!({ "deleted": 1, "waiting": 1 }));
    let scripts = ada.ok(&["scripts", "discard-local", "--all", "--yes", "--json"], "");
    assert_eq!(scripts, json!({ "deleted": 1, "waiting": 1 }));

    assert_eq!(local_titles(&ada), ["Waiting note"]);
    assert_eq!(local_script_names(&ada), ["waiting-script"]);
    assert_eq!(waiting(&ada), (1, 1, 0), "the waiting list is untouched");

    restore(&world, &[&ada]);
    assert_eq!(ada.ok(&["sync", "--json"], ""), nothing_waits());

    assert_eq!(
        bob.team_titles(),
        ["Waiting note"],
        "what waited arrived, what was discarded did not"
    );
    assert_eq!(bob.team_script_names(), ["waiting-script"]);
    assert_eq!(local_counts(&ada), (0, 0));
}

/// Saves `notes` notes and `scripts` scripts named after `who`, as one chat does.
fn save_all(mut chat: Agent, who: &str, notes: usize, scripts: usize) {
    for note in 0..notes {
        let saved = chat.save_note(&format!("{who} note {note}"), "saved at the same time");
        assert!(!saved.is_error, "{}", saved.text);
    }

    for script in 0..scripts {
        let saved = chat.save_script(&script_name(who, script));
        assert!(!saved.is_error, "{}", saved.text);
    }

    chat.end();
}

/// The name of script `number` of the chat `who`.
fn script_name(who: &str, number: usize) -> String {
    format!("{}-script-{number}", who.to_lowercase().replace(' ', "-"))
}

fn titles_of(who: &[&str], notes: usize) -> Vec<String> {
    let mut titles: Vec<String> = who
        .iter()
        .flat_map(|who| (0..notes).map(move |note| format!("{who} note {note}")))
        .collect();
    titles.sort();
    titles
}

fn names_of(who: &[&str], scripts: usize) -> Vec<String> {
    let mut names: Vec<String> = who
        .iter()
        .flat_map(|who| (0..scripts).map(move |script| script_name(who, script)))
        .collect();
    names.sort();
    names
}

fn two_users_saving_at_once_lose_nothing(kind: Kind) {
    let Some(world) = World::open(kind, "at-once") else {
        return;
    };
    let (ada, bob) = (world.teammate("Ada"), world.teammate("Bob"));
    let reader = world.teammate("Cleo");

    // Two chats of each person save at the same time as the other person's.
    let chats = [
        ("Ada first", &ada),
        ("Ada second", &ada),
        ("Bob first", &bob),
        ("Bob second", &bob),
    ];

    std::thread::scope(|scope| {
        for (who, user) in chats {
            let chat = user.agent();
            scope.spawn(move || save_all(chat, who, 4, 2));
        }
    });

    let who = ["Ada first", "Ada second", "Bob first", "Bob second"];
    assert_eq!(reader.team_titles(), titles_of(&who, 4));
    assert_eq!(reader.team_script_names(), names_of(&who, 2));
}

fn two_users_draining_their_offline_saves_at_once_lose_nothing_and_copy_once(kind: Kind) {
    let Some(world) = World::open(kind, "drain-at-once") else {
        return;
    };
    let (ada, bob) = (world.user("Ada"), world.user("Bob"));
    let reader = world.teammate("Cleo");

    world.proxy.cut();
    std::thread::scope(|scope| {
        for (user, who) in [(&ada, "Ada"), (&bob, "Bob")] {
            let chat = user.agent();
            scope.spawn(move || save_all(chat, who, 5, 2));
        }
    });
    assert_eq!(waiting(&ada), (5, 2, 0));
    assert_eq!(waiting(&bob), (5, 2, 0));

    restore(&world, &[&ada, &bob]);

    std::thread::scope(|scope| {
        for user in [&ada, &bob] {
            // Each person's desktop and next chat settle at the same time.
            scope.spawn(move || assert_eq!(user.ok(&["sync", "--json"], ""), nothing_waits()));
            scope.spawn(move || user.agent().end());
        }
    });

    assert_eq!(reader.team_titles(), titles_of(&["Ada", "Bob"], 5));
    assert_eq!(reader.team_script_names(), names_of(&["Ada", "Bob"], 2));

    for user in [&ada, &bob] {
        assert_eq!(waiting(user), (0, 0, 0));
        assert_eq!(local_counts(user), (0, 0));
    }
}

sync_cases!(
    sync_reports_waiting_work_while_offline_and_drains_it_after_the_restore,
    sync_with_nothing_waiting_makes_no_connection,
    discard_local_keeps_what_still_waits_for_the_team_database,
    two_users_saving_at_once_lose_nothing,
    two_users_draining_their_offline_saves_at_once_lose_nothing_and_copy_once,
);
