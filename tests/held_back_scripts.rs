//! Scripts held back from team trust: a script an agent saves while a save guard applies, or a
//! held script the user saves, needs the user's approval before it runs even while every team
//! script is trusted (`trust_team_scripts`), whatever else this computer remembers, until the
//! user approves it. A team database is stood in for by a second SQLite file.

mod common;

use common::guards::{CHAT, Guarded, TEAM_KEY, structured, tool};
use common::mcp::is_error;
use harness_memory::Server;
use harness_memory::push::Options;
use harness_memory::push_scripts::{TeamScripts, push_scripts};
use harness_memory::store::{
    Language, Provenance, Scope, ScriptDraft, ScriptStore, SqliteStore, Target, TrustOrigin, TrustStore,
    TrustedVersion, script_sha256,
};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

fn script(name: &str, body: &str) -> Value {
    json!({ "name": name, "description": "", "language": "sh", "body": body, "scope": "global" })
}

/// The approval `script_get` shows for script `name`.
fn approval(server: &mut Server, name: &str) -> String {
    let got = tool(server, "script_get", json!({ "name": name }));
    structured(&got)["script"]["approval"]
        .as_str()
        .unwrap_or_else(|| panic!("{got}"))
        .to_owned()
}

/// Saves a teammate's script straight into the team file.
fn teammate(guarded: &Guarded, name: &str) {
    let draft = ScriptDraft {
        name: name.to_owned(),
        description: String::new(),
        language: Language::Sh,
        body: format!("echo {name}"),
        timeout_secs: 10,
        provenance: Provenance::default(),
    };
    let mut team = SqliteStore::open(&guarded.team_file()).expect("team");
    team.save_script(&Target::Global, &draft).expect("teammate's script");
}

#[test]
fn a_guarded_save_to_the_team_asks_before_it_runs_while_team_scripts_are_trusted() {
    let guarded = Guarded::new();
    teammate(&guarded, "theirs");

    // Notes off in the chat: a guard applies to every save of the chat.
    guarded.rules(false, (false, false), (true, false));
    let mut chat = guarded.team_server(Some(CHAT), &guarded.repo);
    let saved = tool(&mut chat, "script_save", script("guarded", "echo guarded"));
    assert!(!is_error(&saved), "{saved}");

    assert_eq!(approval(&mut chat, "theirs"), "team");
    assert_eq!(approval(&mut chat, "guarded"), "needed");

    // With Harness Desktop running, script_run asks the user (no answer comes here).
    guarded.desktop_alive();
    let run = tool(&mut chat, "script_run", json!({ "name": "guarded" }));
    assert!(is_error(&run), "{run}");
    assert_eq!(structured(&run)["untrusted"], true, "{run}");
    assert_eq!(structured(&run)["pending"], "timeout", "{run}");

    // A later server without any guard still holds it back.
    std::fs::remove_file(guarded.data().join("guards/chats").join(format!("{CHAT}.json"))).expect("remove");
    let mut later = guarded.team_server(None, &guarded.repo);
    assert_eq!(approval(&mut later, "guarded"), "needed");

    // Saving the same code again without guards does not trust it either.
    let again = tool(&mut later, "script_save", script("guarded", "echo guarded"));
    assert!(!is_error(&again), "{again}");
    assert_eq!(approval(&mut later, "guarded"), "needed");

    // Without guards, an agent's save is trusted as before.
    let free = tool(&mut later, "script_save", script("free", "echo free"));
    assert!(!is_error(&free), "{free}");
    assert_eq!(approval(&mut later, "free"), "local");
}

#[test]
fn only_the_users_approval_releases_a_held_back_version() {
    let guarded = Guarded::new();
    guarded.rules(false, (false, false), (true, false));
    let mut chat = guarded.team_server(Some(CHAT), &guarded.repo);
    let body = "echo deploy";
    let saved = tool(&mut chat, "script_save", script("deploy", body));
    assert!(!is_error(&saved), "{saved}");
    let id = structured(&saved)["script"]["id"].as_i64().expect("id");
    assert_eq!(approval(&mut chat, "deploy"), "needed");

    let version = |body: &'static str, origin| TrustedVersion {
        place: TEAM_KEY,
        script_id: id,
        name: "deploy",
        scope: Scope::Global,
        project: None,
        language: Language::Sh,
        body,
        origin,
    };
    let sha256 = script_sha256(Language::Sh, body);
    let mut trust = SqliteStore::open(&guarded.data().join("memory.sqlite3")).expect("trust");

    // A trust that is not the user's approval leaves it held back.
    trust.trust(&version(body, TrustOrigin::Saved)).expect("saved");
    assert!(trust.is_held_back(TEAM_KEY, id, "deploy", &sha256).expect("read"));

    // The user approves it (as Harness Desktop or `harness-memory trust` do).
    trust.trust(&version(body, TrustOrigin::Approved)).expect("approved");
    assert!(!trust.is_held_back(TEAM_KEY, id, "deploy", &sha256).expect("read"));
    assert_eq!(approval(&mut chat, "deploy"), "approved");

    // Once that trust ages out (five newer versions trusted), team trust applies to it again.
    for newer in ["echo 1", "echo 2", "echo 3", "echo 4", "echo 5"] {
        trust.trust(&version(newer, TrustOrigin::Saved)).expect("newer");
    }

    assert_eq!(approval(&mut chat, "deploy"), "team");
}

#[test]
fn many_unrelated_unapproved_versions_do_not_release_a_held_back_one() {
    let dir = tempfile::tempdir().expect("temp");
    let path = dir.path().join("memory.sqlite3");
    let mut trust = SqliteStore::open(&path).expect("store");
    let sha256 = script_sha256(Language::Sh, "echo held");
    trust.hold_back(TEAM_KEY, Some(7), "held", &sha256).expect("held back");

    // 20 000 other versions reported as waiting, then one more through the store, which prunes
    // the list it keeps to its newest 10 000.
    {
        let mut conn = rusqlite::Connection::open(&path).expect("raw");
        let insert = conn.transaction().expect("transaction");

        for index in 0..20_000 {
            insert
                .execute(
                    "INSERT INTO script_seen_unapproved (sha256, seen_at) VALUES (?1, ?2)",
                    rusqlite::params![format!("{index:064x}"), index],
                )
                .expect("insert");
        }

        insert.commit().expect("commit");
    }

    trust.note_unapproved(&"f".repeat(64)).expect("pruned");
    let kept: i64 = rusqlite::Connection::open(&path)
        .expect("raw")
        .query_row("SELECT count(*) FROM script_seen_unapproved", [], |row| row.get(0))
        .expect("count");
    assert_eq!(kept, 10_000);

    assert!(trust.is_held_back(TEAM_KEY, 7, "held", &sha256).expect("read"));

    // Matched by id when renamed, by a row without an id by name, and per version and place.
    assert!(trust.is_held_back(TEAM_KEY, 7, "renamed", &sha256).expect("read"));
    assert!(!trust.is_held_back(TEAM_KEY, 8, "held", &sha256).expect("read"));
    trust.hold_back(TEAM_KEY, None, "named", &sha256).expect("by name");
    assert!(trust.is_held_back(TEAM_KEY, 9, "named", &sha256).expect("read"));
    assert!(!trust.is_held_back("local", 7, "held", &sha256).expect("read"));
    assert!(!trust.is_held_back(TEAM_KEY, 7, "held", &"0".repeat(64)).expect("read"));
}

#[test]
fn the_held_back_table_is_not_in_a_team_database() {
    let guarded = Guarded::new();
    guarded.rules(false, (false, false), (true, false));
    let mut chat = guarded.team_server(Some(CHAT), &guarded.repo);
    let saved = tool(&mut chat, "script_save", script("guarded", "echo guarded"));
    assert!(!is_error(&saved), "{saved}");

    let tables = |path: &std::path::Path| -> Vec<String> {
        let conn = rusqlite::Connection::open(path).expect("raw");
        let mut statement = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'script_held%'")
            .expect("prepare");
        statement
            .query_map([], |row| row.get(0))
            .expect("query")
            .map(|name| name.expect("name"))
            .collect()
    };

    assert_eq!(tables(&guarded.data().join("memory.sqlite3")), ["script_held_back"]);
    assert_eq!(tables(&guarded.team_file()), Vec::<String>::new());
}

#[test]
fn a_guarded_local_save_stays_held_back_when_copied_to_the_team() {
    let guarded = Guarded::new();
    guarded.rules(false, (false, false), (true, false));
    let mut chat = guarded.server(Some(CHAT), &guarded.repo);
    let saved = tool(&mut chat, "script_save", script("local-guarded", "echo local"));
    assert!(!is_error(&saved), "{saved}");
    drop(chat);

    let mut team = SqliteStore::open(&guarded.team_file()).expect("team");
    let mut trust = SqliteStore::open(&guarded.data().join("memory.sqlite3")).expect("trust");
    let pushed = push_scripts(
        &guarded.data().join("memory.sqlite3"),
        &mut TeamScripts {
            scripts: &mut team,
            key: TEAM_KEY,
            hidden: &[],
        },
        &mut trust,
        &Options::default(),
    )
    .expect("pushed");
    assert!(pushed.failed.is_empty(), "{pushed:?}");
    drop(team);

    let mut reader = guarded.team_server(None, &guarded.repo);
    assert_eq!(approval(&mut reader, "local-guarded"), "needed");
}
