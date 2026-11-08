//! Where new notes and scripts go in a scope set to `remote+local` (`remote_local_saves`): the team
//! database when the key is absent, this computer with `local`, for agents and for the user's own
//! saves alike, with `--place` winning either way. With the team database as the place the team's
//! entries are editable, and an outage keeps what was saved and changed until it is back.
//!
//! The cases run on every live backend whose test variable is set (PostgreSQL, libSQL, MongoDB
//! and MongoDB in the DocumentDB mode) and skip with a message otherwise.

mod common;
mod libsql_support;
mod mongo_support;
mod postgres_support;
mod sync_support;

use serde_json::{Value, json};
use sync_support::{
    Agent, Kind, Place, Places, REMOTE_LOCAL, REMOTE_LOCAL_DEFAULT, User, World, held_back, local_counts, local_notes,
    local_script_names, local_scripts, local_titles, waiting,
};

/// The chat the desktop reviews the saves of.
const CHAT: &str = "1b4e28ba-2fa1-4d3b-a3f5-ef19b5a7633b";

fn restore(world: &World, users: &[&User]) {
    world.proxy.restore();

    for user in users {
        user.end_cooldown();
    }
}

fn nothing_waits() -> Value {
    json!({ "online": true, "waiting": { "notes": 0, "scripts": 0, "updates": 0 } })
}

/// The id of the team note `title`, as `reader` reads it.
fn team_id(reader: &User, title: &str) -> i64 {
    reader.notes(Place::Team).one(title).id
}

/// The id of the note `title` kept on `reader`'s computer.
fn local_id(reader: &User, title: &str) -> i64 {
    reader.notes(Place::Local).one(title).id
}

fn script_draft(name: &str) -> String {
    json!({ "name": name, "description": "d", "language": "sh", "body": format!("echo {name}"), "scope": "repo" })
        .to_string()
}

fn script_with_body(name: &str, body: &str) -> String {
    json!({ "name": name, "description": "d", "language": "sh", "body": body, "scope": "repo" }).to_string()
}

fn note_draft(title: &str, scope: &str) -> String {
    json!({ "title": title, "body": format!("Body of {title}"), "kind": "decision", "scope": scope }).to_string()
}

fn agent_saves_go_to_the_team_when_the_key_is_absent_and_a_second_user_sees_them(kind: Kind) {
    let Some(world) = World::open(kind, "saves-default") else {
        return;
    };
    let ada = world.teammate("Ada").with_places(REMOTE_LOCAL_DEFAULT);
    let bob = world.teammate("Bob");

    let mut chat = ada.agent();
    assert!(!chat.save_note("Repository note", "saved by an agent").is_error);
    assert!(!chat.save_global_note("Global note", "saved by an agent").is_error);
    assert!(!chat.save_script("agent-script").is_error);
    chat.end();

    assert_eq!(bob.team_titles(), ["Global note", "Repository note"]);
    assert_eq!(bob.team_script_names(), ["agent-script"]);
    assert_eq!(local_counts(&ada), (0, 0), "nothing was kept on this computer");
    assert_eq!(waiting(&ada), (0, 0, 0));

    // Ada reads her own saves back from the team too.
    assert_eq!(ada.notes(Place::Team).titles(), ["Global note", "Repository note"]);
}

fn saves_stay_on_this_computer_when_the_key_says_local(kind: Kind) {
    let Some(world) = World::open(kind, "saves-local") else {
        return;
    };
    let ada = world.teammate("Ada").with_places(REMOTE_LOCAL);
    let bob = world.teammate("Bob");

    let mut chat = ada.agent();
    assert!(!chat.save_note("Repository note", "saved by an agent").is_error);
    assert!(!chat.save_script("agent-script").is_error);
    chat.end();

    assert_eq!(bob.team_titles(), Vec::<String>::new());
    assert_eq!(bob.team_script_names(), Vec::<String>::new());
    assert_eq!(local_titles(&ada), ["Repository note"]);
    assert_eq!(local_script_names(&ada), ["agent-script"]);
    assert_eq!(waiting(&ada), (0, 0, 0), "a scope that saves here waits for nothing");
}

fn the_team_entries_are_editable_with_remote_saves_and_read_only_with_local(kind: Kind) {
    let Some(world) = World::open(kind, "saves-edit") else {
        return;
    };
    let ada = world.teammate("Ada").with_places(REMOTE_LOCAL_DEFAULT);
    let cleo = world.teammate("Cleo").with_places(REMOTE_LOCAL);
    let bob = world.teammate("Bob");

    bob.save_note("Teammate note", "the team's text", "repo");
    let draft = script_draft("teammate-script");
    bob.ok(&["scripts", "save", "--json"], &draft);
    let teammate_note = team_id(&bob, "Teammate note");

    // With the team database as the place an agent changes a note it saved, and a teammate's.
    let mut chat = ada.agent();
    assert!(!chat.save_note("Agent note", "first").is_error);
    let agent_note = team_id(&bob, "Agent note");
    chat.ok("note_update", json!({ "id": agent_note, "append": "second" }));
    chat.ok(
        "note_update",
        json!({ "id": teammate_note, "append": "from Ada's agent" }),
    );
    chat.end();

    assert_eq!(bob.body(agent_note), "first\nsecond");
    assert_eq!(bob.body(teammate_note), "the team's text\nfrom Ada's agent");

    let mut chat = ada.agent();
    let scripts = bob.scripts(Place::Team);
    let script = scripts.one("teammate-script").id;
    chat.ok("script_delete", json!({ "id": script }));
    chat.end();
    assert_eq!(bob.team_script_names(), Vec::<String>::new());

    // With `local` the team's note and script are read-only, and an agent's own note stays editable.
    bob.ok(&["scripts", "save", "--json"], &script_draft("again-script"));
    let mut chat = cleo.agent();
    assert!(!chat.save_note("Cleo note", "mine").is_error);
    let refused = chat.tool("note_update", json!({ "id": teammate_note, "append": "refused" }));
    assert!(
        refused.is_error && refused.text.contains("read-only"),
        "{}",
        refused.text
    );

    let mine = local_id(&cleo, "Cleo note");
    chat.ok("note_update", json!({ "id": mine, "append": "kept here" }));

    let script = bob.scripts(Place::Team).one("again-script").id;
    let refused = chat.tool("script_delete", json!({ "id": script }));
    assert!(
        refused.is_error && refused.text.contains("read-only"),
        "{}",
        refused.text
    );
    chat.end();

    assert_eq!(bob.body(teammate_note), "the team's text\nfrom Ada's agent");
    assert_eq!(bob.team_script_names(), ["again-script"]);
    assert_eq!(cleo.body(mine), "mine\nkept here");
}

/// An agent of `user` that holds its saves for review, as the chat `CHAT` asks.
fn reviewing(world: &World, name: &str, places: Places) -> (User, Agent) {
    let user = world.teammate(name).with_places(places).in_chat(CHAT);
    user.rules_for_chat(CHAT, true);
    let agent = user.agent();
    (user, agent)
}

fn held(reply: &sync_support::Reply) -> String {
    assert!(!reply.is_error, "{}", reply.text);
    reply.structured["heldId"].as_str().expect("a held id").to_owned()
}

fn held_note(user: &User, chat: &mut Agent, title: &str) -> String {
    user.keep_desktop_alive();
    held(&chat.save_note(title, "held for review"))
}

fn a_held_save_follows_the_setting_and_a_place_overrides_it(kind: Kind) {
    let Some(world) = World::open(kind, "held-place") else {
        return;
    };
    let bob = world.teammate("Bob");
    let (mut ada, mut chat) = reviewing(&world, "Ada", REMOTE_LOCAL_DEFAULT);

    let by_default = held_note(&ada, &mut chat, "Held default");
    let to_local = held_note(&ada, &mut chat, "Held local");
    let to_remote = held_note(&ada, &mut chat, "Held remote");
    ada.keep_desktop_alive();
    let script = held(&chat.save_script("held-script"));
    let script_here = held(&chat.save_script("held-script-here"));

    let saved = ada.ok(&["held", "save", &by_default, "--json"], "");
    assert_eq!(saved["saved"]["kind"], "note", "{saved}");
    assert!(saved["saved"].get("place").is_none(), "{saved}");
    assert_eq!(bob.team_titles(), ["Held default"], "the key absent: the team database");

    let saved = ada.ok(&["held", "save", &to_local, "--json", "--place", "local"], "");
    assert_eq!(saved["saved"]["place"], "local", "{saved}");
    assert_eq!(local_titles(&ada), ["Held local"]);
    assert_eq!(bob.team_titles(), ["Held default"]);

    let saved = ada.ok(&["held", "save", &to_remote, "--json", "--place", "remote"], "");
    assert_eq!(saved["saved"]["place"], "remote", "{saved}");
    assert_eq!(bob.team_titles(), ["Held default", "Held remote"]);

    let saved = ada.ok(&["held", "save", &script, "--json"], "");
    assert_eq!(saved["saved"]["name"], "held-script", "{saved}");
    assert_eq!(bob.team_script_names(), ["held-script"]);

    let saved = ada.ok(&["held", "save", &script_here, "--json", "--place", "local"], "");
    assert_eq!(saved["saved"]["place"], "local", "{saved}");
    assert_eq!(bob.team_script_names(), ["held-script"]);
    assert_eq!(local_script_names(&ada), ["held-script-here"]);
    assert_eq!(waiting(&ada), (0, 0, 0), "a place a person picked waits for nothing");

    // With `local` set, a held save without a place stays on this computer.
    ada.set_places(REMOTE_LOCAL);
    let mut chat = ada.agent();
    let later = held_note(&ada, &mut chat, "Held later");
    let saved = ada.ok(&["held", "save", &later, "--json"], "");
    assert_eq!(saved["saved"]["kind"], "note", "{saved}");
    assert_eq!(local_titles(&ada), ["Held later", "Held local"]);
    assert_eq!(bob.team_titles(), ["Held default", "Held remote"]);
}

fn a_place_is_refused_for_a_held_item_that_has_none_to_pick_and_the_item_stays(kind: Kind) {
    let Some(world) = World::open(kind, "held-refused") else {
        return;
    };
    let bob = world.teammate("Bob");
    bob.save_note("Teammate note", "the team's text", "repo");
    let teammate_note = team_id(&bob, "Teammate note");

    // Repository notes are read from both places, global notes and scripts only from the team.
    let places = Places {
        repository_notes: "remote+local",
        global_notes: "remote",
        scripts: "remote",
        remote_local_saves: None,
    };
    let (ada, mut chat) = reviewing(&world, "Ada", places);

    ada.keep_desktop_alive();
    let global = held(&chat.save_global_note("Held global", "held for review"));
    ada.keep_desktop_alive();
    let script = held(&chat.save_script("held-script"));
    ada.keep_desktop_alive();
    let update = held(&chat.tool("note_update", json!({ "id": teammate_note, "append": "held change" })));
    let repository = held_note(&ada, &mut chat, "Held repository");

    let waiting_items = || -> usize {
        ada.ok(&["held", "list", "--json"], "")["held"]
            .as_array()
            .expect("held")
            .len()
    };
    assert_eq!(waiting_items(), 4);

    for (id, what) in [
        (&global, "a global note in a scope set to remote"),
        (&script, "a script while scripts are remote"),
        (&update, "a held note_update"),
    ] {
        for place in ["remote", "local"] {
            let said = ada.json(&["held", "save", id, "--json", "--place", place], "", 1);
            assert_eq!(said["error"], "invalid", "{what}: {said}");
        }
    }

    let said = ada.json(&["held", "save", &repository, "--json", "--place", "nowhere"], "", 1);
    assert_eq!(said["error"], "invalid", "{said}");

    assert_eq!(waiting_items(), 4, "every refused item is kept");
    assert_eq!(bob.team_titles(), ["Teammate note"], "and nothing was saved");
    assert_eq!(bob.team_script_names(), Vec::<String>::new());
    assert_eq!(local_titles(&ada), Vec::<String>::new());

    // The scope judged is the one the save will have: a draft moving the global note into the
    // repository, which is read from both places, lets a place be picked.
    let saved = ada.ok(
        &["held", "save", &global, "--json", "--place", "local"],
        r#"{ "scope": "repo" }"#,
    );
    assert_eq!(saved["saved"]["place"], "local", "{saved}");
    assert_eq!(local_titles(&ada), ["Held global"]);

    let saved = ada.ok(&["held", "save", &repository, "--json", "--place", "remote"], "");
    assert_eq!(saved["saved"]["place"], "remote", "{saved}");
    assert_eq!(bob.team_titles(), ["Held repository", "Teammate note"]);
    assert_eq!(waiting_items(), 2);
}

fn a_held_save_to_the_team_fails_visibly_while_it_is_down_and_one_without_a_place_falls_back(kind: Kind) {
    let Some(world) = World::open(kind, "held-down") else {
        return;
    };
    let user = world.user("Ada").with_places(REMOTE_LOCAL_DEFAULT).in_chat(CHAT);
    user.rules_for_chat(CHAT, true);
    let bob = world.teammate("Bob");

    let mut chat = user.agent();
    let note = held_note(&user, &mut chat, "Held note");
    user.keep_desktop_alive();
    let script = held(&chat.save_script("held-script"));
    let plain = held_note(&user, &mut chat, "Held plain");
    chat.end();

    world.proxy.cut();

    for (id, what) in [(&note, "a note"), (&script, "a script")] {
        let said = user.json(&["held", "save", id, "--json", "--place", "remote"], "", 1);
        assert_eq!(said["error"], "refused", "{what}: {said}");
        assert!(
            said["message"].as_str().expect("message").contains("nothing was saved"),
            "{said}"
        );
    }

    let kept = user.ok(&["held", "list", "--json"], "");
    assert_eq!(
        kept["held"].as_array().map(Vec::len),
        Some(3),
        "the items are kept: {kept}"
    );
    assert_eq!(local_counts(&user), (0, 0), "not even on this computer");
    assert_eq!(waiting(&user), (0, 0, 0));

    // Without a place the save falls back to this computer, waits for the team database, and goes
    // to it when it is back.
    let saved = user.ok(&["held", "save", &plain, "--json"], "");
    assert_eq!(saved["saved"]["kind"], "note", "{saved}");
    assert_eq!(local_titles(&user), ["Held plain"]);
    assert_eq!(waiting(&user), (1, 0, 0));

    restore(&world, &[&user]);
    assert_eq!(user.ok(&["sync", "--json"], ""), nothing_waits());
    assert_eq!(bob.team_titles(), ["Held plain"]);
    assert_eq!(local_counts(&user), (0, 0));

    // The kept items can be saved to the team now.
    user.ok(&["held", "save", &note, "--json", "--place", "remote"], "");
    user.ok(&["held", "save", &script, "--json", "--place", "remote"], "");
    assert_eq!(bob.team_titles(), ["Held note", "Held plain"]);
    assert_eq!(bob.team_script_names(), ["held-script"]);
}

fn an_outage_with_remote_saves_keeps_saves_and_changes_and_syncs_them_once(kind: Kind) {
    let Some(world) = World::open(kind, "saves-outage") else {
        return;
    };
    let ada = world.user("Ada").with_places(REMOTE_LOCAL_DEFAULT);
    let bob = world.teammate("Bob");

    bob.save_note("Team note", "the team's text", "repo");
    let team_note = team_id(&bob, "Team note");

    world.proxy.cut();
    let mut chat = ada.agent();
    let saved = chat.save_note("Offline note", "kept here for now");
    assert!(!saved.is_error && saved.is_offline(), "{}", saved.text);
    assert!(!chat.save_global_note("Offline global", "kept here too").is_error);
    assert!(!chat.save_script("offline-script").is_error);
    let queued = chat.ok("note_update", json!({ "id": team_note, "append": "queued line" }));
    assert!(queued.text.contains("queued"), "{}", queued.text);
    chat.end();

    assert_eq!(waiting(&ada), (2, 1, 1));
    assert_eq!(local_titles(&ada), ["Offline global", "Offline note"]);
    assert_eq!(
        ada.ok(&["sync", "--json"], ""),
        json!({ "online": false, "waiting": { "notes": 2, "scripts": 1, "updates": 1 } })
    );

    restore(&world, &[&ada]);
    assert_eq!(ada.ok(&["sync", "--json"], ""), nothing_waits());

    assert_eq!(bob.team_titles(), ["Offline global", "Offline note", "Team note"]);
    assert_eq!(bob.team_script_names(), ["offline-script"]);
    assert_eq!(bob.body(team_note), "the team's text\nqueued line");
    assert_eq!(waiting(&ada), (0, 0, 0), "no leftover");
    assert_eq!(local_counts(&ada), (0, 0), "and no copy kept here");

    // Nothing is copied or applied twice, by a sync or by the next server.
    ada.ok(&["sync", "--json"], "");
    ada.agent().end();
    assert_eq!(bob.team_titles(), ["Offline global", "Offline note", "Team note"]);
    assert_eq!(bob.body(team_note), "the team's text\nqueued line");
    assert_eq!(bob.team_script_names(), ["offline-script"]);
}

fn an_outage_with_local_saves_behaves_as_before(kind: Kind) {
    let Some(world) = World::open(kind, "saves-outage-local") else {
        return;
    };
    let ada = world.user("Ada").with_places(REMOTE_LOCAL);
    let bob = world.teammate("Bob");

    bob.save_note("Team note", "the team's text", "repo");
    let team_note = team_id(&bob, "Team note");

    world.proxy.cut();
    let mut chat = ada.agent();
    assert!(!chat.save_note("Kept here", "stays on this computer").is_error);
    assert!(!chat.save_script("kept-script").is_error);

    // The team's note cannot be read, and a scope that saves here keeps the team's notes read-only.
    let refused = chat.tool("note_update", json!({ "id": team_note, "append": "refused" }));
    assert!(refused.is_error, "{}", refused.text);
    chat.end();

    assert_eq!(waiting(&ada), (0, 0, 0), "nothing waits for the team database");

    restore(&world, &[&ada]);
    assert_eq!(ada.ok(&["sync", "--json"], ""), nothing_waits());
    assert_eq!(bob.team_titles(), ["Team note"]);
    assert_eq!(bob.body(team_note), "the team's text");
    assert_eq!(local_titles(&ada), ["Kept here"]);
    assert_eq!(local_script_names(&ada), ["kept-script"]);
}

fn a_note_a_person_keeps_here_is_never_copied_by_itself(kind: Kind) {
    let Some(world) = World::open(kind, "saves-kept") else {
        return;
    };
    let ada = world.user("Ada").with_places(REMOTE_LOCAL_DEFAULT);
    let bob = world.teammate("Bob");

    // Kept on purpose while the team database is up, and while it is down.
    let kept = ada.ok(
        &["notes", "save", "--json", "--place", "local"],
        &note_draft("Kept on purpose", "repo"),
    );
    assert_eq!(kept["place"], "local", "{kept}");
    ada.ok(
        &["scripts", "save", "--json", "--place", "local"],
        &script_draft("kept-script"),
    );

    world.proxy.cut();
    let kept = ada.ok(
        &["notes", "save", "--json", "--place", "local"],
        &note_draft("Kept while down", "repo"),
    );
    assert_eq!(kept["place"], "local", "{kept}");
    assert!(kept.get("notListed").is_none(), "{kept}");
    let mut chat = ada.agent();
    assert!(!chat.save_note("Agent while down", "an agent's save").is_error);
    chat.end();

    assert_eq!(waiting(&ada), (1, 0, 0), "only the agent's save waits");

    restore(&world, &[&ada]);
    assert_eq!(ada.ok(&["sync", "--json"], ""), nothing_waits());
    ada.agent().end();

    assert_eq!(bob.team_titles(), ["Agent while down"]);
    assert_eq!(bob.team_script_names(), Vec::<String>::new());
    assert_eq!(local_titles(&ada), ["Kept on purpose", "Kept while down"]);
    assert_eq!(local_script_names(&ada), ["kept-script"]);
}

fn a_place_overrides_the_setting_both_ways(kind: Kind) {
    let Some(world) = World::open(kind, "saves-place") else {
        return;
    };
    let bob = world.teammate("Bob");

    for (places, name, to_team, to_here) in [
        (
            REMOTE_LOCAL_DEFAULT,
            "remote setting",
            "Team from remote",
            "Here from remote",
        ),
        (REMOTE_LOCAL, "local setting", "Team from local", "Here from local"),
    ] {
        let ada = world.teammate("Ada").with_places(places);

        let saved = ada.ok(
            &["notes", "save", "--json", "--place", "remote"],
            &note_draft(to_team, "repo"),
        );
        assert_eq!(saved["place"], "remote", "{name}: {saved}");

        let saved = ada.ok(
            &["notes", "save", "--json", "--place", "local"],
            &note_draft(to_here, "global"),
        );
        assert_eq!(saved["place"], "local", "{name}: {saved}");

        let team = to_team.to_lowercase().replace(' ', "-");
        let here = to_here.to_lowercase().replace(' ', "-");
        let saved = ada.ok(
            &["scripts", "save", "--json", "--place", "remote"],
            &script_draft(&team),
        );
        assert_eq!(saved["place"], "remote", "{name}: {saved}");
        let saved = ada.ok(&["scripts", "save", "--json", "--place", "local"], &script_draft(&here));
        assert_eq!(saved["place"], "local", "{name}: {saved}");

        assert!(bob.team_titles().contains(&to_team.to_owned()), "{name}");
        assert!(!bob.team_titles().contains(&to_here.to_owned()), "{name}");
        assert!(bob.team_script_names().contains(&team), "{name}");
        assert!(!bob.team_script_names().contains(&here), "{name}");
        assert!(local_titles(&ada).contains(&to_here.to_owned()), "{name}");
        assert!(local_script_names(&ada).contains(&here), "{name}");
        assert_eq!(waiting(&ada), (0, 0, 0), "{name}");
    }
}

fn pair(name: &str, body: &str) -> (String, String) {
    (name.to_owned(), body.to_owned())
}

fn editing_a_script_kept_here_changes_the_local_row_and_the_team_gets_nothing(kind: Kind) {
    let Some(world) = World::open(kind, "saves-edit-local-script") else {
        return;
    };
    let ada = world.teammate("Ada").with_places(REMOTE_LOCAL_DEFAULT);
    let bob = world.teammate("Bob");

    ada.ok(
        &["scripts", "save", "--json", "--place", "local"],
        &script_with_body("mine", "echo one"),
    );

    // The desktop's edit names no place: the script is saved by its name.
    let edited = ada.ok(&["scripts", "save", "--json"], &script_with_body("mine", "echo two"));
    assert_eq!(edited["created"], false, "{edited}");
    assert!(edited.get("notListed").is_none(), "{edited}");
    assert_eq!(local_scripts(&ada), [pair("mine", "echo two")]);
    assert_eq!(bob.team_script_names(), Vec::<String>::new(), "nothing was published");

    // An agent's own save of the same name changes it too.
    let mut chat = ada.agent();
    assert!(!chat.save_script("mine").is_error);
    assert_eq!(local_scripts(&ada), [pair("mine", "echo mine")]);
    assert_eq!(bob.team_script_names(), Vec::<String>::new());

    // A name that is not here is new, and follows the setting.
    assert!(!chat.save_script("fresh").is_error);
    chat.end();
    assert_eq!(bob.team_script_names(), ["fresh"]);
    assert_eq!(local_scripts(&ada), [pair("mine", "echo mine")]);
    assert_eq!(waiting(&ada), (0, 0, 0));

    // A place a person picks still wins: the team gets its own script of that name.
    ada.ok(
        &["scripts", "save", "--json", "--place", "remote"],
        &script_with_body("mine", "echo shared"),
    );
    assert_eq!(bob.team_script_names(), ["fresh", "mine"]);
    assert_eq!(local_scripts(&ada), [pair("mine", "echo mine")]);
}

fn a_held_script_save_over_a_script_kept_here_changes_it_there(kind: Kind) {
    let Some(world) = World::open(kind, "held-edit-local-script") else {
        return;
    };
    let bob = world.teammate("Bob");
    let (ada, mut chat) = reviewing(&world, "Ada", REMOTE_LOCAL_DEFAULT);

    ada.ok(
        &["scripts", "save", "--json", "--place", "local"],
        &script_with_body("mine", "echo one"),
    );
    ada.keep_desktop_alive();
    let script = held(&chat.save_script("mine"));
    ada.keep_desktop_alive();
    let fresh = held(&chat.save_script("fresh"));

    let saved = ada.ok(&["held", "save", &script, "--json"], "");
    assert_eq!(saved["saved"]["name"], "mine", "{saved}");
    assert_eq!(local_scripts(&ada), [pair("mine", "echo mine")]);
    assert_eq!(bob.team_script_names(), Vec::<String>::new(), "nothing was published");

    // It was held back where it was saved, never by its name in the team database.
    assert!(
        held_back(&ada).iter().all(|(place, _)| place == "local"),
        "{:?}",
        held_back(&ada)
    );

    // A name that is not here goes to the team database, and is held back there.
    ada.ok(&["held", "save", &fresh, "--json"], "");
    assert_eq!(bob.team_script_names(), ["fresh"]);
    assert!(held_back(&ada).iter().any(|(place, _)| place != "local"));
}

fn a_held_script_is_held_back_in_the_place_picked_for_it(kind: Kind) {
    let Some(world) = World::open(kind, "held-back-place") else {
        return;
    };

    for (places, picked, name) in [
        (REMOTE_LOCAL, "remote", "to-the-team"),
        (REMOTE_LOCAL_DEFAULT, "local", "to-here"),
    ] {
        let (ada, mut chat) = reviewing(&world, "Ada", places);
        ada.keep_desktop_alive();
        let script = held(&chat.save_script(name));

        ada.ok(&["held", "save", &script, "--json", "--place", picked], "");

        let rows = held_back(&ada);
        assert!(!rows.is_empty(), "{name}");
        assert!(
            rows.iter().all(|(place, _)| (place == "local") == (picked == "local")),
            "{name}: held back where it was saved ({picked}), by name and by id: {rows:?}"
        );
    }
}

fn an_agents_save_over_a_note_kept_here_changes_it_there_online_and_offline(kind: Kind) {
    let Some(world) = World::open(kind, "saves-edit-local-note") else {
        return;
    };
    let ada = world.user("Ada").with_places(REMOTE_LOCAL_DEFAULT);
    let bob = world.teammate("Bob");

    ada.ok(
        &["notes", "save", "--json", "--place", "local"],
        &note_draft("Kept note", "repo"),
    );

    // Online: the note on this computer is updated, and the team gets nothing.
    let mut chat = ada.agent();
    assert!(!chat.save_note("Kept note", "online text").is_error);
    assert_eq!(local_notes(&ada), [pair("Kept note", "online text")]);
    assert_eq!(bob.team_titles(), Vec::<String>::new());

    // A title that is not here is new: it follows the setting.
    assert!(!chat.save_note("Fresh note", "for the team").is_error);
    chat.end();
    assert_eq!(bob.team_titles(), ["Fresh note"]);

    // Offline: the same, and what it changed is not listed as waiting.
    world.proxy.cut();
    let mut chat = ada.agent();
    let saved = chat.save_note("Kept note", "offline text");
    assert!(!saved.is_error && saved.is_offline(), "{}", saved.text);
    assert_eq!(local_notes(&ada), [pair("Kept note", "offline text")]);
    assert_eq!(waiting(&ada), (0, 0, 0));

    // A new title waits for the team database.
    assert!(!chat.save_note("Offline note", "kept for now").is_error);
    chat.end();
    assert_eq!(waiting(&ada), (1, 0, 0));

    restore(&world, &[&ada]);
    assert_eq!(ada.ok(&["sync", "--json"], ""), nothing_waits());
    ada.agent().end();

    assert_eq!(bob.team_titles(), ["Fresh note", "Offline note"]);
    assert_eq!(
        local_notes(&ada),
        [pair("Kept note", "offline text")],
        "still here, not moved"
    );
}

fn editing_a_script_kept_here_while_down_leaves_it_here_and_a_new_one_waits(kind: Kind) {
    let Some(world) = World::open(kind, "saves-edit-local-script-down") else {
        return;
    };
    let ada = world.user("Ada").with_places(REMOTE_LOCAL_DEFAULT);
    let bob = world.teammate("Bob");

    ada.ok(
        &["scripts", "save", "--json", "--place", "local"],
        &script_with_body("mine", "echo one"),
    );

    world.proxy.cut();
    let edited = ada.ok(&["scripts", "save", "--json"], &script_with_body("mine", "echo two"));
    assert_eq!(edited["created"], false, "{edited}");
    assert!(edited.get("notListed").is_none(), "{edited}");
    assert_eq!(local_scripts(&ada), [pair("mine", "echo two")]);
    assert_eq!(waiting(&ada), (0, 0, 0));

    let mut chat = ada.agent();
    assert!(!chat.save_script("mine").is_error);
    assert!(!chat.save_script("offline-script").is_error);
    chat.end();
    assert_eq!(waiting(&ada), (0, 1, 0), "only the new script waits");

    restore(&world, &[&ada]);
    assert_eq!(ada.ok(&["sync", "--json"], ""), nothing_waits());

    assert_eq!(bob.team_script_names(), ["offline-script"]);
    assert_eq!(
        local_scripts(&ada),
        [pair("mine", "echo mine")],
        "kept here, not copied or deleted"
    );
}

fn a_place_local_save_takes_an_entry_off_the_list_of_what_waits(kind: Kind) {
    let Some(world) = World::open(kind, "saves-place-unlists") else {
        return;
    };
    let ada = world.user("Ada").with_places(REMOTE_LOCAL_DEFAULT);
    let bob = world.teammate("Bob");

    world.proxy.cut();
    let mut chat = ada.agent();
    assert!(!chat.save_note("Waiting note", "agent text").is_error);
    assert!(!chat.save_script("waiting-script").is_error);
    chat.end();
    assert_eq!(waiting(&ada), (1, 1, 0));

    let kept = ada.ok(
        &["notes", "save", "--json", "--place", "local"],
        &note_draft("Waiting note", "repo"),
    );
    assert_eq!(kept["place"], "local", "{kept}");
    assert_eq!(waiting(&ada), (0, 1, 0), "the note no longer waits");

    ada.ok(
        &["scripts", "save", "--json", "--place", "local"],
        &script_with_body("waiting-script", "echo mine"),
    );
    assert_eq!(waiting(&ada), (0, 0, 0), "nor does the script");

    restore(&world, &[&ada]);
    assert_eq!(ada.ok(&["sync", "--json"], ""), nothing_waits());

    assert_eq!(bob.team_titles(), Vec::<String>::new());
    assert_eq!(bob.team_script_names(), Vec::<String>::new());
    assert_eq!(local_titles(&ada), ["Waiting note"]);
    assert_eq!(local_scripts(&ada), [pair("waiting-script", "echo mine")]);
}

fn a_held_save_to_this_computer_takes_the_waiting_row_off_the_list_before_the_team_is_back(kind: Kind) {
    let Some(world) = World::open(kind, "held-place-unlists") else {
        return;
    };
    let ada = world.user("Ada").with_places(REMOTE_LOCAL_DEFAULT).in_chat(CHAT);
    ada.rules_for_chat(CHAT, true);
    let bob = world.teammate("Bob");

    let mut chat = ada.agent();
    let note = held_note(&ada, &mut chat, "Waiting note");
    ada.keep_desktop_alive();
    let script = held(&chat.save_script("waiting-script"));
    chat.end();

    // An agent's saves wait for the team database while it is down.
    world.proxy.cut();
    ada.rules_for_chat(CHAT, false);
    let mut chat = ada.agent();
    assert!(!chat.save_note("Waiting note", "agent text").is_error);
    assert!(!chat.save_script("waiting-script").is_error);
    chat.end();
    assert_eq!(waiting(&ada), (1, 1, 0));

    // The team database is back, and nothing has asked it yet: the pick comes first, so the old
    // text is never copied there.
    restore(&world, &[&ada]);

    let saved = ada.ok(&["held", "save", &note, "--json", "--place", "local"], "");
    assert_eq!(saved["saved"]["place"], "local", "{saved}");
    let saved = ada.ok(&["held", "save", &script, "--json", "--place", "local"], "");
    assert_eq!(saved["saved"]["place"], "local", "{saved}");

    assert_eq!(bob.team_titles(), Vec::<String>::new());
    assert_eq!(bob.team_script_names(), Vec::<String>::new());
    assert_eq!(waiting(&ada), (0, 0, 0));
    assert_eq!(
        local_notes(&ada),
        [("Waiting note".to_owned(), "held for review".to_owned())]
    );
    assert_eq!(local_script_names(&ada), ["waiting-script"]);
}

sync_cases!(
    agent_saves_go_to_the_team_when_the_key_is_absent_and_a_second_user_sees_them,
    saves_stay_on_this_computer_when_the_key_says_local,
    the_team_entries_are_editable_with_remote_saves_and_read_only_with_local,
    a_held_save_follows_the_setting_and_a_place_overrides_it,
    a_place_is_refused_for_a_held_item_that_has_none_to_pick_and_the_item_stays,
    a_held_save_to_the_team_fails_visibly_while_it_is_down_and_one_without_a_place_falls_back,
    an_outage_with_remote_saves_keeps_saves_and_changes_and_syncs_them_once,
    an_outage_with_local_saves_behaves_as_before,
    a_note_a_person_keeps_here_is_never_copied_by_itself,
    a_place_overrides_the_setting_both_ways,
    editing_a_script_kept_here_changes_the_local_row_and_the_team_gets_nothing,
    a_held_script_save_over_a_script_kept_here_changes_it_there,
    a_held_script_is_held_back_in_the_place_picked_for_it,
    an_agents_save_over_a_note_kept_here_changes_it_there_online_and_offline,
    editing_a_script_kept_here_while_down_leaves_it_here_and_a_new_one_waits,
    a_place_local_save_takes_an_entry_off_the_list_of_what_waits,
    a_held_save_to_this_computer_takes_the_waiting_row_off_the_list_before_the_team_is_back,
);
