//! Note usefulness through the MCP tools: `note_search` counts the notes its reply shows, except
//! for Harness Desktop's own background searches, and `note_feedback` counts the notes an agent
//! says helped. Feedback is not a save, so it works while saving is off, paused or reviewed, and
//! it touches nothing of a note but its counters.

mod common;

use std::path::{Path, PathBuf};

use common::guards::{Guarded, structured, tool};
use common::mcp::{call, env, is_error, run, server, text};
use common::{both, global_only};
use harness_memory::Server;
use harness_memory::backend::NotesPlace;
use harness_memory::limits::SEARCH_LIMIT_MAX;
use harness_memory::project::ProjectResolver;
use harness_memory::store::{HiddenNotes, MemoryStore, Note, NoteUsage, SplitStore, SqliteStore};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tempfile::TempDir;

/// The view of the repository of [`Guarded`].
const WIDGET: &str = "github.com/acme/widget";

fn global_note(title: &str) -> Value {
    json!({ "title": title, "body": format!("{title} is described here"), "kind": "note", "scope": "global" })
}

/// The memory file of `guarded`.
fn memory_file(guarded: &Guarded) -> PathBuf {
    guarded.data().join("memory.sqlite3")
}

/// Global note `id` as the file `file` holds it, read without counting a hit.
fn note_in(file: &Path, id: i64) -> Note {
    SqliteStore::open(file)
        .expect("store")
        .peek(&global_only(), &[id])
        .expect("peek")
        .remove(0)
}

fn usage_in(file: &Path, id: i64) -> NoteUsage {
    note_in(file, id).usage.expect("the store has the counters")
}

/// `(shown, useful)` of global note `id` in `file`.
fn counters_in(file: &Path, id: i64) -> (i64, i64) {
    let usage = usage_in(file, id);
    (usage.shown, usage.useful)
}

/// The ids of the notes a search or list reply shows, in order.
fn ids_in(reply: &Value) -> Vec<i64> {
    text(reply)
        .lines()
        .filter_map(|line| line.strip_prefix('#'))
        .filter_map(|rest| rest.split(' ').next()?.parse().ok())
        .collect()
}

fn feedback(server: &mut Server, ids: &[i64]) -> Value {
    tool(
        server,
        "note_feedback",
        json!({ "ids": ids, "reason": "it named the cache key", "scope": "global" }),
    )
}

/// Makes the database of `file` refuse to count `column` (`shown` or `useful`).
fn break_counter(file: &Path, column: &str) {
    rusqlite::Connection::open(file)
        .expect("open")
        .execute_batch(&format!(
            "CREATE TRIGGER broken_{column} BEFORE UPDATE OF {column} ON notes \
             BEGIN SELECT RAISE(ABORT, 'the counter is broken'); END;"
        ))
        .expect("trigger");
}

/// A server for the files in `dir` that `initialize`d as `client`.
fn client_server(dir: &TempDir, client: &str) -> Server {
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("project");
    let mut server = server(dir, &dir.path().join("home"), &project);

    run(
        &mut server,
        &[json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
                  "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": client } } })],
    );

    server
}

#[test]
fn feedback_marks_the_visible_notes_useful_and_reports_the_rest() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);
    let file = memory_file(&guarded);

    for title in ["Build cache", "Retry policy"] {
        let saved = tool(&mut server, "note_save", global_note(title));
        assert!(!is_error(&saved), "{saved}");
    }

    let before = note_in(&file, 1);
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_secs() as i64;

    // A repeated id counts once, and a note that is not there is reported.
    let both = feedback(&mut server, &[1, 2, 9, 1]);
    assert!(!is_error(&both), "{both}");
    assert_eq!(text(&both), "marked 2 notes useful: #1, #2\nnot found: 9");

    let usage = usage_in(&file, 1);
    assert_eq!((usage.shown, usage.useful), (0, 1));
    assert!(usage.last_useful_at.expect("last useful") >= started);
    assert_eq!(counters_in(&file, 2), (0, 1));

    // Nothing else of the note changed: not its text, update time, revision nor hits.
    let after = note_in(&file, 1);
    assert_eq!(
        (&after.title, &after.body, after.updated_at, after.revision, after.hits),
        (
            &before.title,
            &before.body,
            before.updated_at,
            before.revision,
            before.hits
        )
    );

    let one = feedback(&mut server, &[1]);
    assert_eq!(text(&one), "marked 1 note useful: #1");
    assert_eq!(counters_in(&file, 1), (0, 1), "a note counts once per session");

    let nobody = feedback(&mut server, &[]);
    assert!(!is_error(&nobody), "{nobody}");
    assert_eq!(text(&nobody), "marked no notes useful");

    let missing = feedback(&mut server, &[7, 9]);
    assert!(!is_error(&missing), "{missing}");
    assert_eq!(text(&missing), "marked no notes useful\nnot found: 7, 9");
    assert_eq!((counters_in(&file, 1), counters_in(&file, 2)), ((0, 1), (0, 1)));
}

#[test]
fn feedback_takes_a_reason_in_any_language_and_keeps_none() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);
    tool(&mut server, "note_save", global_note("Build cache"));

    let reason = "podał klucz pamięci podręcznej, unikalny-powód-92817";
    let reply = tool(
        &mut server,
        "note_feedback",
        json!({ "ids": [1], "reason": reason, "scope": "global" }),
    );
    assert_eq!(text(&reply), "marked 1 note useful: #1");

    // The reason is not kept: no file of the data folder holds it.
    for entry in std::fs::read_dir(guarded.data()).expect("data") {
        let path = entry.expect("entry").path();

        if path.is_file() {
            let bytes = std::fs::read(&path).expect("read");
            assert!(
                !bytes
                    .windows("unikalny-powód-92817".len())
                    .any(|window| window == "unikalny-powód-92817".as_bytes()),
                "{}",
                path.display()
            );
        }
    }
}

#[test]
fn feedback_without_a_reason_succeeds_and_counts() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);
    tool(&mut server, "note_save", global_note("Build cache"));
    let file = memory_file(&guarded);

    let reply = tool(&mut server, "note_feedback", json!({ "ids": [1], "scope": "global" }));
    assert!(!is_error(&reply), "{reply}");
    assert_eq!(text(&reply), "marked 1 note useful: #1");
    assert_eq!(counters_in(&file, 1).1, 1);

    let reply = tool(&mut server, "note_feedback", json!({ "ids": [] }));
    assert!(!is_error(&reply), "{reply}");
    assert_eq!(text(&reply), "marked no notes useful");
}

#[test]
fn feedback_refuses_malformed_arguments_and_too_many_ids() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);

    let ids: Vec<i64> = (1..=SEARCH_LIMIT_MAX as i64).collect();
    let most = feedback(&mut server, &ids);
    assert!(!is_error(&most), "{most}");
    assert!(
        text(&most).starts_with("marked no notes useful\nnot found: 1, 2"),
        "{most}"
    );

    let too_many: Vec<i64> = (1..=SEARCH_LIMIT_MAX as i64 + 1).collect();
    let refused = feedback(&mut server, &too_many);
    assert!(is_error(&refused), "{refused}");
    assert!(text(&refused).contains("at most 20"), "{refused}");

    let long = "x".repeat(201);
    for arguments in [
        json!({}),
        json!({ "reason": "x" }),
        json!({ "ids": "1", "reason": "x" }),
        json!({ "ids": [1], "reason": "x", "extra": true }),
        json!({ "ids": [1], "reason": long }),
        json!({ "ids": [1], "reason": "x", "scope": "everywhere" }),
    ] {
        let reply = tool(&mut server, "note_feedback", arguments.clone());
        assert!(is_error(&reply), "{arguments}: {reply}");
    }
}

#[test]
fn feedback_does_not_find_a_note_outside_the_scope_asked() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);

    let repo_note = json!({ "title": "Repo only", "body": "lives in the widget repository", "kind": "note",
                            "scope": "repo", "project_path": guarded.repo.to_string_lossy() });
    assert!(!is_error(&tool(&mut server, "note_save", repo_note)));

    let global = feedback(&mut server, &[1]);
    assert_eq!(text(&global), "marked no notes useful\nnot found: 1");

    let both_scopes = tool(
        &mut server,
        "note_feedback",
        json!({ "ids": [1], "reason": "x", "project_path": guarded.repo.to_string_lossy() }),
    );
    assert_eq!(text(&both_scopes), "marked 1 note useful: #1");

    let page = SqliteStore::open(&memory_file(&guarded))
        .expect("store")
        .peek(&both(WIDGET), &[1])
        .expect("peek");
    assert_eq!(page[0].usage.map(|usage| usage.useful), Some(1));
}

#[test]
fn feedback_works_while_saving_is_off_paused_or_reviewed() {
    let rules = [
        ("paused", true, (true, false)),
        ("off", false, (false, false)),
        ("ask", false, (true, true)),
    ];

    for (name, paused, notes) in rules {
        let guarded = Guarded::new();
        let file = memory_file(&guarded);
        let mut saver = guarded.server(None, &guarded.repo);
        tool(&mut saver, "note_save", global_note("Build cache"));

        guarded.rules(paused, notes, (true, false));
        guarded.desktop_alive();
        let mut server = guarded.chat_server();

        // The reads say that saving is limited; the feedback is not a save and says nothing.
        let search = tool(
            &mut server,
            "note_search",
            json!({ "query": "cache", "scope": "global" }),
        );
        assert!(
            text(&search).contains("Saving") || text(&search).contains("review"),
            "{name}: {search}"
        );

        let reply = feedback(&mut server, &[1]);
        assert!(!is_error(&reply), "{name}: {reply}");
        assert_eq!(text(&reply), "marked 1 note useful: #1", "{name}");
        assert_eq!(structured(&reply), Value::Null, "{name}: {reply}");
        assert_eq!(guarded.held_files(), Vec::<Value>::new(), "{name}");

        // The search above counted the note as shown as well.
        assert_eq!(counters_in(&file, 1), (1, 1), "{name}");
    }
}

#[test]
fn feedback_is_not_held_when_the_desktop_is_away_either() {
    let guarded = Guarded::new();
    let mut saver = guarded.server(None, &guarded.repo);
    tool(&mut saver, "note_save", global_note("Build cache"));

    // A review rule with no Harness Desktop to review refuses a save.
    guarded.rules(false, (true, true), (true, false));
    let mut server = guarded.chat_server();

    let save = tool(&mut server, "note_save", global_note("Another note"));
    assert_eq!(structured(&save)["saved"], false, "{save}");

    let reply = feedback(&mut server, &[1]);
    assert_eq!(text(&reply), "marked 1 note useful: #1");
    assert_eq!(structured(&reply), Value::Null, "{reply}");
}

#[test]
fn a_search_counts_each_note_it_shows_once() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);
    let file = memory_file(&guarded);

    for title in ["Build cache", "Cache keys", "Unrelated"] {
        tool(&mut server, "note_save", global_note(title));
    }

    let first = tool(
        &mut server,
        "note_search",
        json!({ "query": "cache", "scope": "global" }),
    );
    let mut shown = ids_in(&first);
    shown.sort_unstable();
    assert_eq!(shown, [1, 2], "{first}");
    assert_eq!([1, 2, 3].map(|id| counters_in(&file, id)), [(1, 0), (1, 0), (0, 0)]);

    tool(
        &mut server,
        "note_search",
        json!({ "query": "cache", "scope": "global" }),
    );
    assert_eq!(counters_in(&file, 1), (1, 0), "a note counts once per session");

    // A search that finds nothing counts nothing, and reading a note whole is not being shown it.
    let none = tool(
        &mut server,
        "note_search",
        json!({ "query": "zzzz", "scope": "global" }),
    );
    assert!(text(&none).starts_with("no matching notes"), "{none}");
    tool(&mut server, "note_get", json!({ "ids": [3] }));
    assert_eq!(counters_in(&file, 3), (0, 0));

    // The search changes nothing else of a note: the revision and the update time stay.
    let note = note_in(&file, 1);
    assert_eq!((note.revision, note.hits), (1, 0));
}

#[test]
fn a_search_does_not_count_the_notes_its_reply_cap_cuts() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);
    let file = memory_file(&guarded);
    let count = SEARCH_LIMIT_MAX as i64;

    for index in 1..=count {
        let note = json!({ "title": format!("Cache note {index:02}"), "kind": "note", "scope": "global",
                           "body": format!("cachekey {}", "keeps the built output between runs ".repeat(12)) });
        assert!(!is_error(&tool(&mut server, "note_save", note)));
    }

    let reply = tool(
        &mut server,
        "note_search",
        json!({ "query": "cachekey", "scope": "global", "limit": count }),
    );
    let rendered = ids_in(&reply);
    let omitted = text(&reply)
        .lines()
        .last()
        .and_then(|line| line.strip_prefix("… ("))
        .and_then(|rest| rest.strip_suffix(" more)"))
        .and_then(|number| number.parse::<usize>().ok())
        .unwrap_or_else(|| panic!("the reply is cut: {reply}"));

    assert_eq!(rendered.len() + omitted, SEARCH_LIMIT_MAX, "{reply}");
    assert!(!rendered.is_empty());

    for id in 1..=count {
        let expected = i64::from(rendered.contains(&id));
        assert_eq!(counters_in(&file, id), (expected, 0), "note {id}");
    }
}

#[test]
fn a_search_by_harness_desktop_counts_nothing() {
    let dir = TempDir::new().expect("temp");
    let file = dir.path().join("memory.sqlite3");

    let mut desktop = client_server(&dir, "harness-desktop");
    tool(&mut desktop, "note_save", global_note("Build cache"));

    let found = tool(
        &mut desktop,
        "note_search",
        json!({ "query": "cache", "scope": "global" }),
    );
    assert_eq!(ids_in(&found), [1], "{found}");
    assert_eq!(counters_in(&file, 1), (0, 0));

    // The same search by an agent counts, on the same database.
    let mut agent = client_server(&dir, "omp");
    tool(
        &mut agent,
        "note_search",
        json!({ "query": "cache", "scope": "global" }),
    );
    assert_eq!(counters_in(&file, 1), (1, 0));

    // Feedback is the agent's word, whoever carries it.
    let reply = feedback(&mut desktop, &[1]);
    assert_eq!(text(&reply), "marked 1 note useful: #1");
}

#[test]
fn a_counter_that_cannot_be_written_never_fails_a_call() {
    let guarded = Guarded::new();
    let mut server = guarded.server(None, &guarded.repo);
    let file = memory_file(&guarded);
    tool(&mut server, "note_save", global_note("Build cache"));

    break_counter(&file, "shown");
    break_counter(&file, "useful");

    let search = tool(
        &mut server,
        "note_search",
        json!({ "query": "cache", "scope": "global" }),
    );
    assert!(!is_error(&search), "{search}");
    assert_eq!(ids_in(&search), [1], "{search}");

    let reply = feedback(&mut server, &[1]);
    assert!(!is_error(&reply), "{reply}");
    assert_eq!(counters_in(&file, 1), (0, 0));
}

/// A server whose notes are split between a team file and the local file, the team notes
/// read-only (`remote+local`), and the files.
fn split_server(dir: &TempDir) -> (Server, PathBuf, PathBuf) {
    split_server_hiding_in(dir, &dir.path().join("memory.sqlite3"))
}

/// As [`split_server`], the list of the team notes hidden on this computer kept in `hidden_file`.
fn split_server_hiding_in(dir: &TempDir, hidden_file: &Path) -> (Server, PathBuf, PathBuf) {
    let team_file = dir.path().join("team.sqlite3");
    let local_file = dir.path().join("memory.sqlite3");
    let local = || SqliteStore::open(&local_file).expect("local file");

    let split = SplitStore::routed(
        Box::new(SqliteStore::open(&team_file).expect("team file")),
        Box::new(local()),
        NotesPlace::RemoteLocal,
        NotesPlace::RemoteLocal,
        Some(HiddenNotes::new(hidden_file.to_path_buf(), "team")),
    );

    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("project");
    let server = Server::new(
        Box::new(split),
        Box::new(local()),
        Box::new(local()),
        ProjectResolver::new(env(dir.path()), Some(project)),
    );

    (server, team_file, local_file)
}

/// Saves global note `title` in the team database, as a teammate would.
fn teammate_saves(team_file: &Path, title: &str) {
    let mut team = SqliteStore::open(team_file).expect("team");
    let draft = common::draft(
        title,
        "the team's build is cached",
        harness_memory::store::NoteKind::Note,
    );
    team.remember(&harness_memory::store::Target::Global, &draft)
        .expect("save");
}

#[test]
fn counters_follow_a_read_only_team_note_and_leave_a_hidden_one() {
    let dir = TempDir::new().expect("temp");
    let (mut server, team_file, local_file) = split_server(&dir);

    teammate_saves(&team_file, "Team cache");
    teammate_saves(&team_file, "Team hidden cache");

    // This computer hides the second team note.
    let hidden = note_in(&team_file, 2);
    assert!(
        HiddenNotes::new(local_file.clone(), "team")
            .hide(&hidden)
            .expect("hide")
    );

    // A note saved here is counted in the local file, a team note in the team database.
    tool(&mut server, "note_save", global_note("Local cache"));

    let search = tool(
        &mut server,
        "note_search",
        json!({ "query": "cache", "scope": "global" }),
    );
    let ids = ids_in(&search);
    assert_eq!(ids.len(), 2, "{search}");
    assert!(ids.contains(&1), "{search}");
    let local_id = *ids.iter().find(|id| **id != 1).expect("the local note");

    assert_eq!(counters_in(&team_file, 1), (1, 0), "a read-only team note is counted");
    assert_eq!(counters_in(&team_file, 2), (0, 0), "a hidden team note is not shown");
    assert_eq!(counters_in(&local_file, 1), (1, 0));

    let reply = tool(
        &mut server,
        "note_feedback",
        json!({ "ids": [1, 2, local_id], "reason": "x", "scope": "global" }),
    );
    assert_eq!(
        text(&reply),
        format!("marked 2 notes useful: #1, #{local_id}\nnot found: 2")
    );
    assert_eq!(counters_in(&team_file, 1), (1, 1));
    assert_eq!(counters_in(&team_file, 2), (0, 0), "a hidden team note is not found");
    assert_eq!(counters_in(&local_file, 1), (1, 1));

    // The team note itself is not changed.
    let note = note_in(&team_file, 1);
    assert_eq!((note.revision, note.hits), (1, 0));
}

#[test]
fn a_team_database_that_cannot_count_does_not_stop_the_local_counts() {
    let dir = TempDir::new().expect("temp");
    let (mut server, team_file, local_file) = split_server(&dir);

    teammate_saves(&team_file, "Team cache");
    tool(&mut server, "note_save", global_note("Local cache"));
    break_counter(&team_file, "shown");
    break_counter(&team_file, "useful");

    let search = tool(
        &mut server,
        "note_search",
        json!({ "query": "cache", "scope": "global" }),
    );
    let ids = ids_in(&search);
    assert_eq!(ids.len(), 2, "{search}");
    assert_eq!(counters_in(&team_file, 1), (0, 0));
    assert_eq!(counters_in(&local_file, 1), (1, 0));

    let reply = feedback(&mut server, &ids);
    assert!(!is_error(&reply), "{reply}");
    assert_eq!(counters_in(&team_file, 1), (0, 0));
    assert_eq!(counters_in(&local_file, 1), (1, 1));
}

#[test]
fn feedback_has_its_own_tool_definition() {
    let dir = TempDir::new().expect("temp");
    let mut server = client_server(&dir, "omp");

    let listed = run(
        &mut server,
        &[json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })],
    );
    let tools = listed[0]["result"]["tools"].as_array().expect("tools");
    let feedback = tools
        .iter()
        .find(|tool| tool["name"] == "note_feedback")
        .expect("note_feedback");
    let schema = &feedback["inputSchema"];

    assert_eq!(schema["required"], json!(["ids"]));
    assert_eq!(schema["properties"]["ids"]["minItems"], 0);
    assert_eq!(schema["properties"]["ids"]["maxItems"], SEARCH_LIMIT_MAX);
    assert_eq!(schema["properties"]["reason"]["maxLength"], 200);
    assert_eq!(schema["additionalProperties"], false);

    // A call by id is answered like any other tool.
    let reply = run(
        &mut server,
        &[call(2, "note_feedback", json!({ "ids": [], "reason": "none helped" }))],
    );
    assert_eq!(text(&reply[0]), "marked no notes useful");
}

/// A writer that notes how many times the first note was counted shown whenever it is flushed,
/// that is, when a reply reaches the client.
struct Probe {
    file: PathBuf,
    flushed_at: Vec<i64>,
}

impl std::io::Write for Probe {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flushed_at.push(counters_in(&self.file, 1).0);
        Ok(())
    }
}

fn search_line(id: i64) -> String {
    format!(
        "{}\n",
        call(id, "note_search", json!({ "query": "cache", "scope": "global" }))
    )
}

#[test]
fn a_counter_is_written_after_its_reply_is_flushed() {
    let dir = TempDir::new().expect("temp");
    let file = dir.path().join("memory.sqlite3");
    let mut server = client_server(&dir, "omp");
    tool(&mut server, "note_save", global_note("Build cache"));

    let mut probe = Probe {
        file: file.clone(),
        flushed_at: Vec::new(),
    };
    harness_memory::serve(
        std::io::Cursor::new(search_line(5).into_bytes()),
        &mut probe,
        &mut server,
    )
    .expect("serve");

    // The reply went out before the note was counted, and the count came right after it.
    assert_eq!(probe.flushed_at, [0]);
    assert_eq!(counters_in(&file, 1), (1, 0));
}

#[test]
fn a_search_does_not_wait_for_a_write_lock_another_process_holds() {
    let dir = TempDir::new().expect("temp");
    let file = dir.path().join("memory.sqlite3");
    let mut server = client_server(&dir, "omp");
    tool(&mut server, "note_save", global_note("Build cache"));

    // Another process holds the write lock for the whole search.
    let holder = rusqlite::Connection::open(&file).expect("open");
    holder.execute_batch("BEGIN IMMEDIATE").expect("lock");

    let started = std::time::Instant::now();
    let replies = common::mcp::run_raw(&mut server, search_line(7).as_bytes());
    let took = started.elapsed();

    assert_eq!(ids_in(&replies[0]), [1], "{}", replies[0]);
    assert!(!is_error(&replies[0]));
    // Far below the 5 s the database waits for a lock otherwise.
    assert!(took < std::time::Duration::from_millis(1500), "took {took:?}");

    holder.execute_batch("COMMIT").expect("unlock");
    assert_eq!(counters_in(&file, 1), (0, 0), "the count is dropped, not waited for");

    // The same for feedback.
    holder.execute_batch("BEGIN IMMEDIATE").expect("lock");
    let started = std::time::Instant::now();
    let reply = feedback(&mut server, &[1]);
    assert_eq!(text(&reply), "marked 1 note useful: #1");
    assert!(started.elapsed() < std::time::Duration::from_millis(1500));
    holder.execute_batch("COMMIT").expect("unlock");
}

#[test]
fn a_note_is_counted_once_per_session() {
    let dir = TempDir::new().expect("temp");
    let file = dir.path().join("memory.sqlite3");
    let mut first = client_server(&dir, "omp");
    tool(&mut first, "note_save", global_note("Build cache"));

    for _ in 0..3 {
        let found = tool(
            &mut first,
            "note_search",
            json!({ "query": "cache", "scope": "global" }),
        );
        assert_eq!(ids_in(&found), [1], "{found}");
    }

    for _ in 0..2 {
        // The reply is the same each time: only the counting is deduplicated.
        assert_eq!(text(&feedback(&mut first, &[1])), "marked 1 note useful: #1");
    }

    assert_eq!(counters_in(&file, 1), (1, 1));

    // Another session counts again.
    let mut second = client_server(&dir, "omp");
    tool(
        &mut second,
        "note_search",
        json!({ "query": "cache", "scope": "global" }),
    );
    feedback(&mut second, &[1]);
    assert_eq!(counters_in(&file, 1), (2, 2));
}

#[test]
fn a_hidden_list_that_cannot_be_read_counts_no_team_note() {
    let dir = TempDir::new().expect("temp");
    let garbage = dir.path().join("hidden.sqlite3");
    std::fs::write(
        &garbage,
        "not a database, just text that is long enough to have a header",
    )
    .expect("write");
    let (mut server, team_file, local_file) = split_server_hiding_in(&dir, &garbage);

    teammate_saves(&team_file, "Team cache");
    tool(&mut server, "note_save", global_note("Local cache"));

    let search = tool(
        &mut server,
        "note_search",
        json!({ "query": "cache", "scope": "global" }),
    );
    let ids = ids_in(&search);
    assert_eq!(ids.len(), 2, "{search}");
    let local_id = *ids.iter().find(|id| **id != 1).expect("the local note");

    let reply = feedback(&mut server, &[1, local_id]);
    assert!(!is_error(&reply), "{reply}");
    assert_eq!(
        text(&reply),
        format!("marked 1 note useful: #{local_id}\nnot checked: 1")
    );
    assert_eq!(counters_in(&team_file, 1).1, 0, "no team note is counted useful");
    assert_eq!(counters_in(&local_file, 1).1, 1);
}

#[test]
fn a_search_counts_no_team_note_while_the_hidden_list_cannot_be_read() {
    let dir = TempDir::new().expect("temp");
    let garbage = dir.path().join("hidden.sqlite3");
    std::fs::write(
        &garbage,
        "not a database, just text that is long enough to have a header",
    )
    .expect("write");
    let (mut server, team_file, local_file) = split_server_hiding_in(&dir, &garbage);

    teammate_saves(&team_file, "Team cache");
    tool(&mut server, "note_save", global_note("Local cache"));

    // The search itself is unchanged: it shows both notes.
    let search = tool(
        &mut server,
        "note_search",
        json!({ "query": "cache", "scope": "global" }),
    );
    assert_eq!(ids_in(&search).len(), 2, "{search}");

    // A note that may be hidden is not counted as shown, a note of this computer is.
    assert_eq!(counters_in(&team_file, 1), (0, 0));
    assert_eq!(counters_in(&local_file, 1), (1, 0));
}

#[test]
fn a_note_of_a_cancelled_search_is_counted_by_the_next_search_that_shows_it() {
    let dir = TempDir::new().expect("temp");
    let file = dir.path().join("memory.sqlite3");
    let mut server = client_server(&dir, "omp");
    tool(&mut server, "note_save", global_note("Build cache"));

    // The client cancels request 41 before it runs: it is never answered nor counted.
    let cancel = json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 41 } });
    let input = format!("{cancel}\n{}{}", search_line(41), search_line(42));
    let replies = common::mcp::run_raw(&mut server, input.as_bytes());

    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0]["id"], json!(42));
    assert_eq!(ids_in(&replies[0]), [1], "{}", replies[0]);
    assert_eq!(
        counters_in(&file, 1),
        (1, 0),
        "counted once, by the search that was answered"
    );

    // And not again by a later one.
    tool(
        &mut server,
        "note_search",
        json!({ "query": "cache", "scope": "global" }),
    );
    assert_eq!(counters_in(&file, 1), (1, 0));
}

#[test]
fn feedback_with_the_team_database_down_succeeds_and_lists_what_it_could_not_check() {
    let dir = TempDir::new().expect("temp");
    let (mut server, team_file, local_file) = split_server(&dir);

    teammate_saves(&team_file, "Team cache");
    tool(&mut server, "note_save", global_note("Local cache"));

    let search = tool(
        &mut server,
        "note_search",
        json!({ "query": "cache", "scope": "global" }),
    );
    let ids = ids_in(&search);
    assert_eq!(ids.len(), 2, "{search}");
    let local_id = *ids.iter().find(|id| **id != 1).expect("the local note");

    // The team database stops answering.
    rusqlite::Connection::open(&team_file)
        .expect("open")
        .execute_batch("DROP TABLE notes")
        .expect("drop");

    let reply = tool(
        &mut server,
        "note_feedback",
        json!({ "ids": [1, local_id, local_id + 50], "reason": "x", "scope": "global" }),
    );
    assert!(!is_error(&reply), "{reply}");
    assert_eq!(
        text(&reply),
        format!(
            "marked 1 note useful: #{local_id}\nnot found: {}\nnot checked: 1",
            local_id + 50
        )
    );
    assert_eq!(counters_in(&local_file, 1), (1, 1));
}
