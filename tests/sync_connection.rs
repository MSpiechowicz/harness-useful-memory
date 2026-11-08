//! Losing the connection to the team database and getting it back, with the real executable over
//! real connections that a proxy cuts: nothing an agent saved is lost, the calls after the first do
//! not wait for the dead connection, and everything reaches the team database once.
//!
//! The cases run on every live backend whose test variable is set (PostgreSQL, libSQL, MongoDB
//! and MongoDB in the DocumentDB mode) and skip with a message otherwise.

mod common;
mod libsql_support;
mod mongo_support;
mod postgres_support;
mod sync_support;

use std::time::Duration;

use sync_support::{Agent, Kind, Place, User, World, local_counts, waiting};

/// A call that does not wait for a connection.
const FAST: Duration = Duration::from_secs(2);

/// The most a call may take that meets the outage first (the connection that timed out).
const FIRST: Duration = Duration::from_secs(30);

fn restore(world: &World, users: &[&User]) {
    world.proxy.restore();

    for user in users {
        user.end_cooldown();
    }
}

/// Says that what was saved is on this computer only, for now.
fn assert_saved_here(reply: &sync_support::Reply) {
    assert!(!reply.is_error, "{}", reply.text);
    assert!(reply.is_offline(), "{}", reply.text);
}

/// Asserts the team database holds `titles` once each, as `reader` sees it, and that nothing is
/// left on `user`'s computer or waiting there.
fn assert_all_arrived(user: &User, reader: &User, titles: &[&str], scripts: &[&str]) {
    let mut wanted: Vec<String> = titles.iter().map(|title| (*title).to_owned()).collect();
    wanted.sort();
    assert_eq!(reader.team_titles(), wanted, "every note once, none twice");

    let mut wanted: Vec<String> = scripts.iter().map(|name| (*name).to_owned()).collect();
    wanted.sort();
    assert_eq!(reader.team_script_names(), wanted, "every script once, none twice");

    assert_eq!(waiting(user), (0, 0, 0), "nothing waits any more");
    assert_eq!(local_counts(user), (0, 0), "no copy is left on this computer");
}

fn an_open_chat_saves_through_a_connection_loss_and_everything_arrives_once(kind: Kind) {
    let Some(world) = World::open(kind, "lost") else { return };
    let (ada, bob) = (world.user("Ada"), world.teammate("Bob"));
    let mut chat = ada.agent();

    // The chat has live connections for notes and for scripts before the network goes.
    assert!(!chat.save_note("Saved online", "reached the team database").is_offline());
    assert!(!chat.save_script("online-script").is_offline());
    assert_eq!(bob.team_titles(), ["Saved online"]);

    world.proxy.cut();

    let note = chat.save_note("Saved in the outage", "the connection dropped");
    assert_saved_here(&note);
    assert!(note.took < FIRST, "{:?}", note.took);
    let script = chat.save_script("outage-script");
    assert_saved_here(&script);
    assert!(script.took < FIRST, "{:?}", script.took);

    // Later calls do not wait for the dead connection again.
    for round in 0..3 {
        let note = chat.save_note(&format!("Later {round}"), "no wait");
        assert_saved_here(&note);
        assert!(note.took < FAST, "{:?}", note.took);

        let found = chat.ok("note_search", serde_json::json!({ "query": "Later" }));
        assert!(
            found.is_offline() && found.took < FAST,
            "{:?} {}",
            found.took,
            found.text
        );
    }

    assert_eq!(waiting(&ada), (4, 1, 0));
    assert_eq!(local_counts(&ada), (4, 1));

    restore(&world, &[&ada]);
    let synced = ada.ok(&["sync", "--json"], "");
    assert_eq!(
        synced,
        serde_json::json!({ "online": true, "waiting": { "notes": 0, "scripts": 0, "updates": 0 } })
    );

    assert_all_arrived(
        &ada,
        &bob,
        &["Saved online", "Saved in the outage", "Later 0", "Later 1", "Later 2"],
        &["online-script", "outage-script"],
    );

    // Running it again copies nothing twice.
    ada.ok(&["sync", "--json"], "");
    chat.end();
    ada.agent().end();
    assert_eq!(bob.team_titles().len(), 5);
}

fn a_chat_started_during_the_outage_saves_here_and_the_next_server_copies_it(kind: Kind) {
    let Some(world) = World::open(kind, "started") else {
        return;
    };
    let (ada, bob) = (world.user("Ada"), world.teammate("Bob"));

    world.proxy.cut();
    let mut chat = ada.agent();

    let first = chat.save_note("Started down", "no connection ever");
    assert_saved_here(&first);
    assert_saved_here(&chat.save_script("down-script"));
    let later = chat.save_global_note("Global down", "a global note");
    assert_saved_here(&later);
    assert!(later.took < FAST, "{:?}", later.took);
    assert_eq!(waiting(&ada), (2, 1, 0));
    assert!(ada.is_cooling(), "the next process keeps away for a minute");
    chat.end();

    restore(&world, &[&ada]);

    // A server that starts when the database is back copies what waits at its first call.
    let mut next = ada.agent();
    let found = next.ok("note_search", serde_json::json!({ "query": "Started down" }));
    assert!(!found.is_offline(), "{}", found.text);
    next.end();

    assert_all_arrived(&ada, &bob, &["Started down", "Global down"], &["down-script"]);
}

fn without_the_fallback_a_lost_connection_names_the_fix_and_the_next_call_works(kind: Kind) {
    let Some(world) = World::open(kind, "strict") else {
        return;
    };
    let ada = world.user("Ada").without_fallback();
    let bob = world.teammate("Bob");
    let mut chat = ada.agent();

    chat.ok(
        "note_save",
        serde_json::json!({ "title": "Before", "body": "online", "kind": "note" }),
    );
    chat.save_script("strict-script");
    world.proxy.cut();

    let refused = chat.save_note("Refused", "the connection is down");
    assert!(refused.is_error, "{}", refused.text);
    assert!(
        refused.text.contains("offline_fallback") || refused.text.contains("HARNESS_MEMORY_OFFLINE_FALLBACK"),
        "the failure names the setting to change: {}",
        refused.text
    );
    assert!(!refused.is_offline(), "{}", refused.text);
    assert_eq!(waiting(&ada), (0, 0, 0), "nothing was kept on this computer");
    assert_eq!(local_counts(&ada), (0, 0));

    world.proxy.restore();

    // The same chat: the next call connects again.
    let saved = chat.save_note("After", "the connection is back");
    assert!(!saved.is_error && !saved.is_offline(), "{}", saved.text);
    let script = chat.save_script("after-script");
    assert!(!script.is_error, "{}", script.text);

    assert_eq!(bob.team_titles(), ["After", "Before"]);
    assert_eq!(bob.team_script_names(), ["after-script", "strict-script"]);
    assert_eq!(local_counts(&ada), (0, 0));
    chat.end();
}

fn several_servers_saving_offline_at_once_lose_nothing(kind: Kind) {
    const SERVERS: usize = 5;
    const NOTES_EACH: usize = 4;

    let Some(world) = World::open(kind, "parallel") else {
        return;
    };
    let (ada, bob) = (world.user("Ada"), world.teammate("Bob"));

    let chats: Vec<Agent> = (0..SERVERS).map(|_| ada.agent()).collect();
    world.proxy.cut();

    std::thread::scope(|scope| {
        for (server, mut chat) in chats.into_iter().enumerate() {
            scope.spawn(move || {
                for note in 0..NOTES_EACH {
                    let saved = chat.save_note(&format!("Server {server} note {note}"), "saved in parallel");
                    assert!(!saved.is_error, "{}", saved.text);
                }

                let saved = chat.save_script(&format!("server-{server}-script"));
                assert!(!saved.is_error, "{}", saved.text);
                chat.end();
            });
        }
    });

    assert_eq!(
        waiting(&ada),
        (SERVERS * NOTES_EACH, SERVERS, 0),
        "none fell off the waiting list"
    );
    assert_eq!(local_counts(&ada), (SERVERS * NOTES_EACH, SERVERS));

    restore(&world, &[&ada]);
    ada.ok(&["sync", "--json"], "");

    let titles: Vec<String> = (0..SERVERS)
        .flat_map(|server| (0..NOTES_EACH).map(move |note| format!("Server {server} note {note}")))
        .collect();
    let scripts: Vec<String> = (0..SERVERS).map(|server| format!("server-{server}-script")).collect();
    let titles: Vec<&str> = titles.iter().map(String::as_str).collect();
    let scripts: Vec<&str> = scripts.iter().map(String::as_str).collect();
    assert_all_arrived(&ada, &bob, &titles, &scripts);

    assert_eq!(bob.notes(Place::Team).rows.len(), SERVERS * NOTES_EACH);
}

fn without_the_fallback_a_refused_script_names_the_fix_and_a_new_server_works_once_the_minute_is_over(kind: Kind) {
    let Some(world) = World::open(kind, "strict-scripts") else {
        return;
    };
    let ada = world.user("Ada").without_fallback();
    let bob = world.teammate("Bob");
    let mut chat = ada.agent();

    chat.save_script("online-script");
    world.proxy.cut();

    let refused = chat.save_script("refused-script");
    assert!(refused.is_error, "{}", refused.text);
    assert!(refused.text.contains("offline fallback"), "{}", refused.text);
    assert_eq!(local_counts(&ada), (0, 0), "nothing was kept on this computer");
    chat.end();

    world.proxy.restore();
    ada.end_cooldown();

    let mut next = ada.agent();
    assert!(!next.save_script("after-script").is_error);
    assert!(!next.save_note("After", "the connection is back").is_error);
    next.end();

    assert_eq!(bob.team_script_names(), ["after-script", "online-script"]);
    assert_eq!(bob.team_titles(), ["After"]);
}

sync_cases!(
    an_open_chat_saves_through_a_connection_loss_and_everything_arrives_once,
    a_chat_started_during_the_outage_saves_here_and_the_next_server_copies_it,
    without_the_fallback_a_lost_connection_names_the_fix_and_the_next_call_works,
    without_the_fallback_a_refused_script_names_the_fix_and_a_new_server_works_once_the_minute_is_over,
    several_servers_saving_offline_at_once_lose_nothing,
);
