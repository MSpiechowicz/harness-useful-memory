//! The English notes setting through the MCP tools: an agent's clearly non-English `note_save` or
//! `note_update` is refused before the save guards, with nothing saved or held, while short,
//! mixed and code-heavy notes pass, and the setting off lets everything through. The instructions,
//! the tool descriptions and `note_get` follow the setting.

mod common;

use common::guards::{Guarded, structured, tool};
use common::mcp::{call, env, is_error, run, server, text};
use harness_memory::Server;
use harness_memory::backend::NotesPlace;
use harness_memory::project::ProjectResolver;
use harness_memory::store::{
    HiddenNotes, MemoryStore, NoteDraft, NoteKind, Provenance, SplitStore, SqliteStore, Target,
};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tempfile::TempDir;

const ENGLISH: &str = "The sync job reads every changed note from the team database, then writes the new revision \
                       to the local cache so the next search finds it without another round trip.";

const POLISH: &str = "Zadanie synchronizacji czyta każdą zmienioną notatkę z bazy zespołu, a potem zapisuje nową \
                      wersję w lokalnej pamięci podręcznej, żeby następne wyszukiwanie znalazło ją od razu.";

const REFUSED_SAVE: &str = "This note is not in English, and this store keeps notes in English. Rewrite the title \
                            and body in English and call note_save again. Nothing was saved.";

fn global_note(title: &str, body: &str) -> Value {
    json!({ "title": title, "body": body, "kind": "note", "scope": "global" })
}

/// The memory file of `guarded`, to change the setting or save behind the server.
fn memory_file(guarded: &Guarded) -> SqliteStore {
    SqliteStore::open(&guarded.data().join("memory.sqlite3")).expect("store")
}

fn found(server: &mut Server, query: &str) -> bool {
    let reply = tool(server, "note_search", json!({ "query": query, "scope": "global" }));
    !text(&reply).starts_with("no matching notes")
}

/// Asserts `reply` is the refusal `message` with the reason `not-english`, a normal result.
fn assert_refused(reply: &Value, message: &str) {
    assert!(!is_error(reply), "{reply}");
    assert_eq!(text(reply), message);
    assert_eq!(structured(reply), json!({ "saved": false, "reason": "not-english" }));
}

fn saved(reply: &Value) {
    assert!(!is_error(reply), "{reply}");
    assert!(structured(reply).get("saved").is_none(), "{reply}");
}

#[test]
fn a_polish_note_is_refused_and_nothing_is_saved() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);

    let refused = tool(&mut server, "note_save", global_note("Synchronizacja notatek", POLISH));
    assert_refused(&refused, REFUSED_SAVE);
    assert!(!found(&mut server, "synchronizacja"));

    saved(&tool(&mut server, "note_save", global_note("Sync job", ENGLISH)));
    assert!(found(&mut server, "sync"));
}

#[test]
fn short_mixed_and_code_heavy_notes_are_saved() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);

    let code = "Run `cargo test --locked` then check src/store/sqlite/schema.rs and notes_fts_en::rebuild, see \
                https://example.com/docs/fts5.html for the option list.\n\
                ```rust\nfn ensure_extensions(db: &mut dyn Sql) -> Result<(), StoreError> { Ok(()) }\n```";
    let mixed = "The cache key is built by the function in src/cache.rs, ale nie ma jej w dokumentacji, więc \
                 sprawdź kod.";

    for (title, body) in [
        ("Wersja", "Zapisz plik"),
        ("Where the schema lives", code),
        ("Cache key", mixed),
    ] {
        saved(&tool(&mut server, "note_save", global_note(title, body)));
    }
}

#[test]
fn the_setting_off_lets_a_polish_note_in() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);
    memory_file(&guarded).set_english_notes(false).expect("off");

    saved(&tool(
        &mut server,
        "note_save",
        global_note("Synchronizacja notatek", POLISH),
    ));
    assert!(found(&mut server, "synchronizacja"));

    memory_file(&guarded).set_english_notes(true).expect("on");

    let refused = tool(&mut server, "note_save", global_note("Inna notatka", POLISH));
    assert_refused(&refused, REFUSED_SAVE);
}

#[test]
fn an_update_to_polish_is_refused_and_the_note_stays() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);
    saved(&tool(&mut server, "note_save", global_note("Sync job", ENGLISH)));

    let refused = tool(&mut server, "note_update", json!({ "id": 1, "body": POLISH }));
    assert_refused(
        &refused,
        "This note is not in English, and this store keeps notes in English. Rewrite the title and body in English \
         and call note_update again. Nothing was saved.",
    );

    let appended = tool(&mut server, "note_update", json!({ "id": 1, "append": POLISH }));
    assert_eq!(structured(&appended)["reason"], "not-english", "{appended}");

    let note = tool(&mut server, "note_get", json!({ "ids": [1] }));
    assert!(text(&note).contains("round trip"), "{note}");
    assert!(!text(&note).contains("Zadanie"), "{note}");

    // Tags alone change neither the title nor the body, so the check leaves them be.
    saved(&tool(&mut server, "note_update", json!({ "id": 1, "tags": ["sync"] })));
    saved(&tool(&mut server, "note_update", json!({ "id": 1, "body": ENGLISH })));
}

#[test]
fn the_refusal_comes_before_the_review_and_holds_nothing() {
    let guarded = Guarded::new();
    guarded.rules(false, (true, true), (true, false));
    guarded.desktop_alive();
    let mut server = guarded.chat_server();

    let refused = tool(&mut server, "note_save", global_note("Synchronizacja notatek", POLISH));
    assert_refused(&refused, REFUSED_SAVE);
    assert_eq!(guarded.held_files(), Vec::<Value>::new());

    // An English note under the same rule is held, as before.
    let held = tool(&mut server, "note_save", global_note("Sync job", ENGLISH));
    assert_eq!(structured(&held)["held"], true, "{held}");
    assert_eq!(guarded.held_files().len(), 1);
}

#[test]
fn note_get_asks_for_a_rewrite_of_a_polish_note_only_while_the_setting_is_on() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);

    // Notes an agent saved while the setting was off, or that a user saved.
    let mut file = memory_file(&guarded);
    file.set_english_notes(false).expect("off");

    for (title, body) in [("Notatka synchronizacji", POLISH), ("Sync job", ENGLISH)] {
        let draft = NoteDraft {
            title: title.to_owned(),
            body: body.to_owned(),
            kind: NoteKind::Note,
            tags: Vec::new(),
            entities: Vec::new(),
            provenance: Provenance::default(),
        };
        file.remember(&Target::Global, &draft).expect("save");
    }

    let hint = "This note is not in English: rewrite it in English with note_update (id 1).";

    let off = tool(&mut server, "note_get", json!({ "ids": [1, 2] }));
    assert!(!text(&off).contains("rewrite it in English"), "{off}");

    file.set_english_notes(true).expect("on");

    let on = tool(&mut server, "note_get", json!({ "ids": [1, 2] }));
    let lines: Vec<&str> = text(&on).lines().collect();
    assert_eq!(lines.iter().filter(|line| **line == hint).count(), 1, "{on}");
    assert!(!text(&on).contains("(id 2)"), "{on}");

    // The hint closes the block of its note, after the indented body.
    let at = lines.iter().position(|line| *line == hint).expect("hint");
    assert!(lines[at - 1].starts_with("  "), "{on}");
}

/// The `initialize` and `tools/list` replies of a server over `dir`.
fn handshake(dir: &TempDir) -> (String, String) {
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("project");
    let mut server = server(dir, &dir.path().join("home"), &project);

    let replies = run(
        &mut server,
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "omp" } } }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        ],
    );

    let instructions = replies[0]["result"]["instructions"].as_str().expect("instructions");
    let tools: Vec<String> = replies[1]["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter(|tool| ["note_save", "note_search"].contains(&tool["name"].as_str().unwrap_or_default()))
        .map(|tool| tool["description"].as_str().expect("description").to_owned())
        .collect();

    (instructions.to_owned(), tools.join("\n"))
}

#[test]
fn the_instructions_and_tool_descriptions_follow_the_setting() {
    let dir = TempDir::new().expect("temp");
    let (on_instructions, on_tools) = handshake(&dir);
    assert!(on_instructions.contains("English keywords"), "{on_instructions}");
    assert!(on_tools.contains("English keywords"), "{on_tools}");
    assert!(on_instructions.chars().count() <= 2000);
    assert!(on_instructions.contains("note_feedback"), "{on_instructions}");

    SqliteStore::open(&dir.path().join("memory.sqlite3"))
        .expect("store")
        .set_english_notes(false)
        .expect("off");

    let (off_instructions, off_tools) = handshake(&dir);
    assert!(!off_instructions.contains("English"), "{off_instructions}");
    assert!(off_instructions.contains("note_feedback"), "{off_instructions}");
    assert!(!off_tools.contains("English"), "{off_tools}");

    // Synonym tags are asked for either way.
    assert!(off_instructions.contains("synonyms"), "{off_instructions}");
    assert!(off_tools.contains("synonyms"), "{off_tools}");
}

#[test]
fn a_tool_call_without_a_handshake_still_works() {
    let dir = TempDir::new().expect("temp");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("project");
    let mut server = server(&dir, &dir.path().join("home"), &project);

    let replies = run(
        &mut server,
        &[call(1, "note_save", global_note("Synchronizacja notatek", POLISH))],
    );
    assert_refused(&replies[0], REFUSED_SAVE);
}

/// The hint asks for a `note_update`, which a team note of a `remote+local` scope refuses: the
/// caller is not told to do what cannot work, while a team note it may change gets the hint.
#[test]
fn note_get_asks_for_no_rewrite_of_a_team_note_this_computer_cannot_change() {
    let hint = "This note is not in English: rewrite it in English with note_update (id 1).";
    let dir = TempDir::new().expect("temp dir");

    let mut team = SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team file");
    let draft = NoteDraft {
        title: "Notatka synchronizacji".to_owned(),
        body: POLISH.to_owned(),
        kind: NoteKind::Note,
        tags: Vec::new(),
        entities: Vec::new(),
        provenance: Provenance::default(),
    };
    team.remember(&Target::Global, &draft).expect("save");

    let get = |place: NotesPlace| {
        let team = SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team file");
        let local = || SqliteStore::open(&dir.path().join("memory.sqlite3")).expect("local file");
        let hidden = HiddenNotes::new(dir.path().join("memory.sqlite3"), "team");

        let mut server = Server::new(
            Box::new(SplitStore::routed(
                Box::new(team),
                Box::new(local()),
                place,
                place,
                Some(hidden),
            )),
            Box::new(local()),
            Box::new(local()),
            ProjectResolver::new(env(dir.path()), Some(dir.path().to_path_buf())),
        );

        text(&tool(&mut server, "note_get", json!({ "ids": [1] }))).to_owned()
    };

    let read_only = get(NotesPlace::RemoteLocal);
    assert!(read_only.contains("Notatka synchronizacji"), "{read_only}");
    assert!(!read_only.contains("rewrite it in English"), "{read_only}");

    let writable = get(NotesPlace::Remote);
    assert!(writable.lines().any(|line| line == hint), "{writable}");
}
