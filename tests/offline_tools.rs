//! Agents' tool calls while the team database is unreachable (the offline fallback): an update of a
//! team note cannot read the note it changes, and is held or refused for what the call gives alone.

mod common;

use common::guards::{CHAT, Guarded, structured, tool};
use common::mcp::{is_error, run, text};
use harness_memory::Server;
use harness_memory::backend::{NotesPlace, Secrets};
use harness_memory::project::ProjectResolver;
use harness_memory::store::{Lazy, MemoryStore, ScriptStore, SplitScripts, SplitStore, SqliteStore, TrustStore};
use serde_json::{Value, json};

const POLISH: &str = "Zadanie synchronizacji czyta każdą zmienioną notatkę z bazy zespołu, a potem zapisuje nową \
                      wersję w lokalnej pamięci podręcznej, żeby następne wyszukiwanie znalazło ją od razu.";

const ENGLISH: &str = "The sync job reads every changed note from the team database, then writes the new revision \
                       to the local cache so the next search finds it without another round trip.";

/// A server of chat [`CHAT`] whose team database cannot be opened, with every note scope in it and
/// the offline fallback on.
fn offline_server(guarded: &Guarded) -> Server {
    let local_file = guarded.data().join("memory.sqlite3");
    let local = || SqliteStore::open(&local_file).expect("local file");
    let team: Lazy<dyn MemoryStore> = Lazy::new(|| Err("cannot open the team database".to_owned()));

    let store = SplitStore::routed(
        Box::new(team),
        Box::new(local()),
        NotesPlace::Remote,
        NotesPlace::Remote,
        None,
    )
    .with_offline_fallback(&local_file, "team:tests".to_owned(), Secrets::default());

    let projects = ProjectResolver::new(guarded.env(Some(CHAT)), Some(guarded.repo.clone()));
    let mut server = Server::new(Box::new(store), Box::new(local()), Box::new(local()), projects);

    run(
        &mut server,
        &[json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
                  "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "omp" } } })],
    );

    server
}

#[test]
fn a_non_english_update_of_a_team_note_is_refused_while_the_team_database_is_down() {
    let guarded = Guarded::new();
    guarded.rules(false, (true, false), (true, false));
    SqliteStore::open(&guarded.data().join("memory.sqlite3"))
        .expect("local file")
        .set_english_notes(true)
        .expect("on");
    let mut server = offline_server(&guarded);

    let refused = tool(
        &mut server,
        "note_update",
        json!({ "id": 5, "append": POLISH, "scope": "global" }),
    );
    assert_eq!(
        structured(&refused),
        json!({ "saved": false, "reason": "not-english" }),
        "{refused}"
    );
    assert!(
        !guarded.data().join("pending-updates.json").exists(),
        "nothing was queued"
    );

    // In English the same call is queued, as it was.
    let queued = tool(
        &mut server,
        "note_update",
        json!({ "id": 5, "append": ENGLISH, "scope": "global" }),
    );
    assert!(!is_error(&queued), "{queued}");
    assert!(text(&queued).contains("queued"), "{queued}");
    assert!(guarded.data().join("pending-updates.json").is_file());
}

#[test]
fn an_update_of_a_team_note_is_held_without_a_current_note_while_the_team_database_is_down() {
    let guarded = Guarded::new();
    guarded.rules(false, (true, true), (true, false));
    guarded.desktop_alive();
    let mut server = offline_server(&guarded);

    let held = tool(
        &mut server,
        "note_update",
        json!({ "id": 5, "append": "Found while offline", "scope": "global" }),
    );
    assert!(!is_error(&held), "{held}");
    assert_eq!(structured(&held)["held"], true, "{held}");

    let files = guarded.held_files();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["current"], Value::Null, "the note could not be read");
    assert_eq!(files[0]["arguments"]["id"], 5);

    // A note of this computer that is not there is still the tool's own error.
    let missing = tool(
        &mut server,
        "note_update",
        json!({ "id": 1_000_000_099_i64, "append": "x", "scope": "global" }),
    );
    assert!(is_error(&missing), "{missing}");
    assert_eq!(guarded.held_files().len(), 1);
}

/// [`offline_server`] with scripts routed to the team database too, which cannot be opened either.
fn offline_scripts_server(guarded: &Guarded) -> Server {
    let local_file = guarded.data().join("memory.sqlite3");
    let local = || SqliteStore::open(&local_file).expect("local file");
    let team: Lazy<dyn ScriptStore> = Lazy::new(|| Err("cannot open the team database".to_owned()));

    let scripts = SplitScripts::new(Box::new(team), Box::new(local()), NotesPlace::Remote, None).with_offline_fallback(
        &local_file,
        Box::new(local()) as Box<dyn TrustStore>,
        "team:tests".to_owned(),
        Secrets::default(),
    );

    let notes: Lazy<dyn MemoryStore> = Lazy::new(|| Err("cannot open the team database".to_owned()));
    let store = SplitStore::routed(
        Box::new(notes),
        Box::new(local()),
        NotesPlace::Remote,
        NotesPlace::Remote,
        None,
    )
    .with_offline_fallback(&local_file, "team:tests".to_owned(), Secrets::default());

    let projects = ProjectResolver::new(guarded.env(Some(CHAT)), Some(guarded.repo.clone()));
    let mut server = Server::new(Box::new(store), Box::new(scripts), Box::new(local()), projects);

    run(
        &mut server,
        &[json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
                  "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "omp" } } })],
    );

    server
}

/// The waiting list of the offline saves cannot be read: what is saved here stays here, and the
/// reply says it was not listed to sync automatically.
#[test]
fn a_note_or_script_saved_offline_says_when_it_could_not_be_listed_to_sync() {
    let guarded = Guarded::new();
    guarded.rules(false, (true, false), (true, false));
    std::fs::write(guarded.data().join("pending-sync.json"), "not json").expect("a damaged list");
    let mut server = offline_scripts_server(&guarded);

    let note = tool(
        &mut server,
        "note_save",
        json!({ "title": "Saved while offline", "body": "Kept on this computer", "kind": "note", "scope": "global" }),
    );
    assert!(!is_error(&note), "{note}");
    assert!(
        text(&note).contains("not listed: saved on this computer, but could not be listed to sync automatically"),
        "{note}"
    );

    let script = tool(
        &mut server,
        "script_save",
        json!({ "name": "offline-script", "description": "d", "language": "sh", "body": "echo hi", "scope": "global" }),
    );
    assert!(!is_error(&script), "{script}");
    assert!(
        text(&script).contains("not listed: saved on this computer, but could not be listed to sync automatically"),
        "{script}"
    );
    assert_eq!(structured(&script)["notListed"], true, "{script}");

    // The damaged list was left as it was.
    assert_eq!(
        std::fs::read_to_string(guarded.data().join("pending-sync.json")).expect("list"),
        "not json"
    );

    // With a list that can be read, nothing is said.
    std::fs::remove_file(guarded.data().join("pending-sync.json")).expect("remove");
    let listed = tool(
        &mut server,
        "note_save",
        json!({ "title": "Listed note", "body": "Kept on this computer", "kind": "note", "scope": "global" }),
    );
    assert!(!is_error(&listed), "{listed}");
    assert!(!text(&listed).contains("not listed"), "{listed}");
}
