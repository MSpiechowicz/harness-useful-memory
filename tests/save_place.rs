//! `notes save --place` and `scripts save --place`: where a note or script the user adds goes when
//! its scope is set to `remote+local`. The cases on a real team database need `postgres_support`'s
//! server and are skipped without it; the ones on a team database that cannot be reached, on
//! routing that has no place to pick, and on the MCP tools need none.

// Most of these tests run only on Unix, so on Windows their helpers and imports are unused.
#![cfg_attr(not(unix), allow(unused))]

mod common;
mod postgres_support;

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use common::mcp::repo_with_remote;
use common::stdio::{BINARY, Client, command, without_team_settings};
use postgres_support::{Database, database};
use serde_json::{Value, json};
use tempfile::TempDir;

/// A team database no server listens at, with a password that must never be printed.
const NOBODY: &str = "postgres://postgres:hunter2@127.0.0.1:1/postgres";

/// A machine of its own: home, data and config folders, and a clone of the shared repository.
struct Machine {
    _root: TempDir,
    home: PathBuf,
    memory: PathBuf,
    config: PathBuf,
    repo: PathBuf,
}

fn machine() -> Machine {
    let root = TempDir::new().expect("temp");
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).expect("home");

    let repo = repo_with_remote(
        &root.path().join("work"),
        "widget",
        "https://github.com/acme/widget.git",
    );

    Machine {
        memory: root.path().join("memory"),
        config: root.path().join("config"),
        _root: root,
        home,
        repo,
    }
}

/// How the notes of each scope and the scripts are routed, as the settings write it.
#[derive(Clone, Copy)]
struct Routing {
    repository: &'static str,
    global: &'static str,
    scripts: &'static str,
    /// `remote_local_saves`; empty leaves it out, as a person who never set it.
    saves: &'static str,
}

/// Every kind set to `remote+local`, saving to this computer (`remote_local_saves` `local`).
const SHARED: Routing = Routing {
    repository: "remote+local",
    global: "remote+local",
    scripts: "remote+local",
    saves: "local",
};

/// The executable with `args` on `machine`, the team database `url` and `routing`.
fn binary(machine: &Machine, args: &[&str], url: &str, routing: Routing) -> Command {
    let mut command = Command::new(BINARY);
    command
        .args(args)
        .current_dir(&machine.repo)
        .env("HOME", &machine.home)
        .env("USERPROFILE", &machine.home)
        .env("HARNESS_MEMORY_DIR", &machine.memory)
        .env("XDG_CONFIG_HOME", &machine.config)
        .env("APPDATA", &machine.config)
        .env("HARNESS_MEMORY_DATABASE_URL", url)
        .env("HARNESS_MEMORY_REPOSITORY_NOTES", routing.repository)
        .env("HARNESS_MEMORY_GLOBAL_NOTES", routing.global)
        .env("HARNESS_MEMORY_SCRIPTS", routing.scripts);

    if !routing.saves.is_empty() {
        command.env("HARNESS_MEMORY_REMOTE_LOCAL_SAVES", routing.saves);
    }

    without_team_settings(&mut command);
    command
}

/// The folder of `machine`'s repository, as the commands that list or pick entries take it.
fn project(machine: &Machine) -> &str {
    machine.repo.to_str().expect("utf-8 path")
}

/// `command` with `input` on its stdin.
fn run_with_input(mut command: Command, input: &str) -> Output {
    use std::io::Write as _;

    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("run")
}

fn run(mut command: Command) -> Output {
    command.stdin(Stdio::null()).output().expect("run")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The one JSON object a command printed, which exited with `code`.
fn answer(output: &Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {} stderr: {}",
        stdout(output),
        stderr(output)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("one JSON object: {}", stdout(output)))
}

/// `notes save` of the note `title` in `scope`, with the `--place` arguments `extra`.
fn save_note(machine: &Machine, url: &str, routing: Routing, scope: &str, title: &str, extra: &[&str]) -> Output {
    let mut args = vec!["notes", "save", "--json"];
    args.extend_from_slice(extra);

    let note = json!({ "title": title, "body": format!("Body of {title}"), "kind": "decision", "scope": scope });
    run_with_input(binary(machine, &args, url, routing), &note.to_string())
}

/// `scripts save` of the script `name` in `scope`, with the arguments `extra`.
fn save_script(machine: &Machine, url: &str, routing: Routing, scope: &str, name: &str, extra: &[&str]) -> Output {
    let mut args = vec!["scripts", "save", "--json"];
    args.extend_from_slice(extra);

    let script = json!({
        "name": name,
        "description": "A script",
        "language": "sh",
        "body": format!("printf {name}"),
        "scope": scope,
    });
    run_with_input(binary(machine, &args, url, routing), &script.to_string())
}

/// The titles of the notes in `machine`'s local database, sorted.
fn local_titles(machine: &Machine) -> Vec<String> {
    let path = machine.memory.join("memory.sqlite3");

    if !path.is_file() {
        return Vec::new();
    }

    let conn = rusqlite::Connection::open(path).expect("local");
    let mut statement = conn.prepare("SELECT title FROM notes ORDER BY title").expect("select");
    statement
        .query_map([], |row| row.get(0))
        .expect("titles")
        .collect::<Result<_, _>>()
        .expect("titles")
}

/// The names of the scripts in `machine`'s local database, sorted.
fn local_script_names(machine: &Machine) -> Vec<String> {
    let path = machine.memory.join("memory.sqlite3");

    if !path.is_file() {
        return Vec::new();
    }

    let conn = rusqlite::Connection::open(path).expect("local");
    let mut statement = conn.prepare("SELECT name FROM scripts ORDER BY name").expect("select");
    statement
        .query_map([], |row| row.get(0))
        .expect("names")
        .collect::<Result<_, _>>()
        .expect("names")
}

/// The titles of the team notes in `db`, sorted.
fn team_titles(db: &Database) -> Vec<String> {
    let mut admin = db.admin();
    admin
        .query("SELECT title FROM notes ORDER BY title", &[])
        .expect("select")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// The names of the team scripts in `db`, sorted.
fn team_script_names(db: &Database) -> Vec<String> {
    let mut admin = db.admin();
    admin
        .query("SELECT name FROM scripts ORDER BY name", &[])
        .expect("select")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// The titles `notes list --json` shows `machine` (on the team database `url`, routed `routing`).
fn listed_titles(machine: &Machine, url: &str, routing: Routing) -> Vec<String> {
    let listing = answer(
        &run(binary(
            machine,
            &["notes", "list", "--json", "--project", project(machine)],
            url,
            routing,
        )),
        0,
    );

    listing["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .map(|note| note["title"].as_str().expect("title").to_owned())
        .collect()
}

/// The names `scripts list --json` shows `machine`.
fn listed_names(machine: &Machine, url: &str, routing: Routing) -> Vec<String> {
    let listing = answer(
        &run(binary(
            machine,
            &["scripts", "list", "--json", "--project", project(machine)],
            url,
            routing,
        )),
        0,
    );

    listing["scripts"]
        .as_array()
        .expect("scripts")
        .iter()
        .map(|script| script["name"].as_str().expect("name").to_owned())
        .collect()
}

/// True when anything is waiting to be copied to the team database from `machine`.
fn has_waiting(machine: &Machine) -> bool {
    machine.memory.join("pending-sync.json").exists()
}

#[test]
fn a_note_goes_where_place_says_and_a_teammate_sees_the_remote_one() {
    let Some(db) = database() else { return };
    let (first, second) = (machine(), machine());

    let remote = answer(
        &save_note(&first, db.url(), SHARED, "repo", "Remote one", &["--place", "remote"]),
        0,
    );
    assert_eq!(remote["title"], "Remote one");
    assert_eq!(remote["scope"], "repo");
    assert_eq!(remote["place"], "remote");
    assert!(
        remote["id"].as_i64().expect("id") < 1_000_000_000,
        "a team id: {remote}"
    );

    let local = answer(
        &save_note(&first, db.url(), SHARED, "repo", "Local one", &["--place", "local"]),
        0,
    );
    assert_eq!(local["place"], "local");
    assert!(
        local["id"].as_i64().expect("id") >= 1_000_000_000,
        "a local id: {local}"
    );

    // Without --place the note goes to this computer as before, and the answer is as it was.
    let omitted = answer(&save_note(&first, db.url(), SHARED, "global", "Omitted one", &[]), 0);
    assert!(omitted.get("place").is_none(), "{omitted}");
    assert!(omitted["id"].as_i64().expect("id") >= 1_000_000_000);

    let global = answer(
        &save_note(
            &first,
            db.url(),
            SHARED,
            "global",
            "Remote global",
            &["--place", "remote"],
        ),
        0,
    );
    assert_eq!(
        (global["scope"].clone(), global["place"].clone()),
        (json!("global"), json!("remote"))
    );

    assert_eq!(team_titles(&db), ["Remote global", "Remote one"]);
    assert_eq!(local_titles(&first), ["Local one", "Omitted one"]);
    assert!(!has_waiting(&first), "a save the user placed waits for nothing");

    // A second user, who has none of the first one's local notes, sees the remote notes.
    let mut seen = listed_titles(&second, db.url(), SHARED);
    seen.sort();
    assert_eq!(seen, ["Remote global", "Remote one"]);
    assert_eq!(local_titles(&second), Vec::<String>::new());
}

#[test]
fn a_remote_note_is_saved_with_its_author_and_the_rules_of_a_team_save() {
    let Some(db) = database() else { return };
    let machine = machine();

    let saved = answer(
        &save_note(
            &machine,
            db.url(),
            SHARED,
            "repo",
            "Authored",
            &["--place", "remote", "--client", "harness-desktop"],
        ),
        0,
    );
    let id = saved["id"].to_string();

    let shown = answer(
        &run(binary(
            &machine,
            &["notes", "get", &id, "--json", "--project", project(&machine)],
            db.url(),
            SHARED,
        )),
        0,
    );
    assert_eq!(shown["note"]["title"], "Authored");
    assert_eq!(shown["note"]["author"]["source"], "harness-desktop", "{shown}");

    // The team save's masking is the store's: a secret in the body is masked as it is there.
    let token = ["gh", "p_", &"a".repeat(36)].concat();
    let secret = json!({ "title": "Leaky", "body": format!("token {token}"), "kind": "decision" });
    let saved = run_with_input(
        binary(
            &machine,
            &["notes", "save", "--json", "--place", "remote"],
            db.url(),
            SHARED,
        ),
        &secret.to_string(),
    );
    answer(&saved, 0);

    let body: String = db
        .admin()
        .query_one("SELECT body FROM notes WHERE title = 'Leaky'", &[])
        .expect("the note")
        .get(0);
    assert!(!body.contains(&token), "the token is masked: {body}");

    // And its limits: a title past the limit is refused, saving nothing.
    let long = json!({ "title": "t".repeat(1000), "body": "Body", "kind": "decision" });
    let refused = run_with_input(
        binary(
            &machine,
            &["notes", "save", "--json", "--place", "remote"],
            db.url(),
            SHARED,
        ),
        &long.to_string(),
    );
    assert_eq!(answer(&refused, 1)["error"], "refused");
    assert_eq!(team_titles(&db), ["Authored", "Leaky"]);
}

#[test]
fn a_script_goes_where_place_says_and_is_trusted_as_any_team_save() {
    let Some(db) = database() else { return };
    let (first, second) = (machine(), machine());

    let remote = answer(
        &save_script(&first, db.url(), SHARED, "global", "remote-one", &["--place", "remote"]),
        0,
    );
    assert_eq!(remote["name"], "remote-one");
    assert_eq!(remote["place"], "remote");
    assert_eq!(remote["created"], true);
    assert!(
        remote["id"].as_i64().expect("id") < 1_000_000_000,
        "a team id: {remote}"
    );

    let local = answer(
        &save_script(&first, db.url(), SHARED, "global", "local-one", &["--place", "local"]),
        0,
    );
    assert_eq!(local["place"], "local");
    assert!(local["id"].as_i64().expect("id") >= 1_000_000_000);

    let omitted = answer(&save_script(&first, db.url(), SHARED, "global", "omitted-one", &[]), 0);
    assert!(omitted.get("place").is_none(), "{omitted}");
    assert_eq!(omitted["approval"], local["approval"]);

    assert_eq!(team_script_names(&db), ["remote-one"]);
    assert_eq!(local_script_names(&first), ["local-one", "omitted-one"]);
    assert!(!has_waiting(&first));
    assert_eq!(listed_names(&second, db.url(), SHARED), ["remote-one"]);

    // The same trust as a save to a scripts = "remote" setting gives: trusted under the team database.
    let alone = machine();
    let plain = Routing {
        scripts: "remote",
        ..SHARED
    };
    let reference = answer(&save_script(&alone, db.url(), plain, "global", "plain-one", &[]), 0);
    assert_eq!(remote["approval"], reference["approval"], "{remote} {reference}");
}

#[test]
fn place_is_refused_unless_the_scope_is_routed_remote_and_local() {
    let Some(db) = database() else { return };
    let machine = machine();

    let mixed = Routing {
        repository: "remote+local",
        global: "remote",
        scripts: "local",
        saves: "local",
    };

    for (scope, routed) in [("global", "remote"), ("repo", "local")] {
        let routing = Routing {
            repository: if scope == "repo" { routed } else { "remote+local" },
            global: if scope == "global" { routed } else { "remote+local" },
            scripts: "remote+local",
            saves: "local",
        };

        for place in ["remote", "local"] {
            let title = format!("Refused {scope} {place}");
            let output = save_note(&machine, db.url(), routing, scope, &title, &["--place", place]);
            let said = answer(&output, 1);

            assert_eq!(said["error"], "invalid", "{said}");
            assert!(
                said["message"].as_str().expect("message").contains("remote+local"),
                "{said}"
            );
        }
    }

    // The scope of the entry decides: the repository notes of `mixed` take a place, the global ones not.
    let global = save_note(
        &machine,
        db.url(),
        mixed,
        "global",
        "Refused mixed",
        &["--place", "remote"],
    );
    assert_eq!(answer(&global, 1)["error"], "invalid");

    let repo = save_note(
        &machine,
        db.url(),
        mixed,
        "repo",
        "Placed mixed",
        &["--place", "remote"],
    );
    assert_eq!(answer(&repo, 0)["place"], "remote");

    for scripts in ["remote", "local"] {
        let routing = Routing { scripts, ..SHARED };

        for place in ["remote", "local"] {
            let name = format!("refused-{scripts}-{place}");
            let said = answer(
                &save_script(&machine, db.url(), routing, "global", &name, &["--place", place]),
                1,
            );

            assert_eq!(said["error"], "invalid", "{said}");
        }
    }

    // Nothing was saved anywhere by the refusals.
    assert_eq!(team_titles(&db), ["Placed mixed"]);
    assert_eq!(local_titles(&machine), Vec::<String>::new());
    assert_eq!(team_script_names(&db), Vec::<String>::new());
    assert_eq!(local_script_names(&machine), Vec::<String>::new());
}

#[test]
fn place_is_refused_without_a_team_database_and_for_a_value_that_is_not_a_place() {
    let machine = machine();
    let mut command = Command::new(BINARY);
    command
        .args(["notes", "save", "--json", "--place", "remote"])
        .current_dir(&machine.repo)
        .env("HOME", &machine.home)
        .env("HARNESS_MEMORY_DIR", &machine.memory);
    without_team_settings(&mut command);

    let note = json!({ "title": "Alone", "body": "Body", "kind": "decision" }).to_string();
    let said = answer(&run_with_input(command, &note), 1);
    assert_eq!(said["error"], "invalid", "{said}");
    assert_eq!(local_titles(&machine), Vec::<String>::new());

    let bad = save_note(&machine, NOBODY, SHARED, "repo", "Bad place", &["--place", "nowhere"]);
    assert_eq!(answer(&bad, 1)["error"], "invalid");

    let bad = save_script(&machine, NOBODY, SHARED, "repo", "bad-place", &["--place", "team"]);
    assert_eq!(answer(&bad, 1)["error"], "invalid");

    // `notes update` has no place to pick.
    let patch = json!({ "title": "x" }).to_string();
    let update = run_with_input(
        binary(
            &machine,
            &["notes", "update", "1", "--json", "--place", "remote"],
            NOBODY,
            SHARED,
        ),
        &patch,
    );
    assert_eq!(update.status.code(), Some(2), "{}", stderr(&update));
}

#[test]
fn a_remote_save_while_the_team_database_is_down_fails_and_keeps_nothing() {
    let machine = machine();

    let note = save_note(&machine, NOBODY, SHARED, "repo", "Not yet", &["--place", "remote"]);
    let said = answer(&note, 1);
    assert_eq!(said["error"], "refused", "{said}");

    let script = save_script(&machine, NOBODY, SHARED, "repo", "not-yet", &["--place", "remote"]);
    let said_script = answer(&script, 1);
    assert_eq!(said_script["error"], "refused", "{said_script}");

    for output in [&note, &script] {
        let text = format!("{}{}", stdout(output), stderr(output));
        assert!(!text.contains("hunter2"), "the password is scrubbed: {text}");
    }

    assert!(
        said["message"].as_str().expect("message").contains("nothing was saved"),
        "{said}"
    );
    assert_eq!(
        local_titles(&machine),
        Vec::<String>::new(),
        "no fallback to this computer"
    );
    assert_eq!(local_script_names(&machine), Vec::<String>::new());
    assert!(!has_waiting(&machine), "and nothing is listed as waiting");

    // Saving here is still possible: --place local, and a save without one, as before.
    let local = answer(
        &save_note(&machine, NOBODY, SHARED, "repo", "Here", &["--place", "local"]),
        0,
    );
    assert_eq!(local["place"], "local");

    let omitted = answer(&save_script(&machine, NOBODY, SHARED, "repo", "here", &[]), 0);
    assert_eq!(omitted["name"], "here");
    assert_eq!(local_titles(&machine), ["Here"]);
    assert_eq!(local_script_names(&machine), ["here"]);
    assert!(!has_waiting(&machine), "a scope set to remote+local waits for nothing");
}

#[test]
fn a_title_hidden_on_this_computer_is_refused_for_the_remote() {
    let Some(db) = database() else { return };
    let machine = machine();

    let team = answer(
        &save_note(&machine, db.url(), SHARED, "repo", "Taken", &["--place", "remote"]),
        0,
    );
    let id = team["id"].to_string();
    answer(
        &run(binary(
            &machine,
            &["notes", "hide", &id, "--json", "--project", project(&machine)],
            db.url(),
            SHARED,
        )),
        0,
    );

    let again = save_note(&machine, db.url(), SHARED, "repo", "Taken", &["--place", "remote"]);
    let said = answer(&again, 1);
    assert_eq!(said["error"], "refused", "{said}");
    assert!(
        said["message"].as_str().expect("message").contains("hidden team note"),
        "{said}"
    );

    // This computer is not the team database: the same title is still saved here.
    let local = answer(
        &save_note(&machine, db.url(), SHARED, "repo", "Taken", &["--place", "local"]),
        0,
    );
    assert_eq!(local["place"], "local");
    assert_eq!(team_titles(&db), ["Taken"]);

    let script = answer(
        &save_script(&machine, db.url(), SHARED, "global", "taken", &["--place", "remote"]),
        0,
    );
    let id = script["id"].to_string();
    answer(
        &run(binary(
            &machine,
            &["scripts", "hide", &id, "--json", "--project", project(&machine)],
            db.url(),
            SHARED,
        )),
        0,
    );

    let again = save_script(&machine, db.url(), SHARED, "global", "taken", &["--place", "remote"]);
    let said = answer(&again, 1);
    assert_eq!(said["error"], "refused", "{said}");
    assert!(
        said["message"]
            .as_str()
            .expect("message")
            .contains("hidden team script"),
        "{said}"
    );
    assert_eq!(team_script_names(&db), ["taken"]);
}

#[test]
fn a_same_title_note_from_another_checkout_is_answered_as_a_conflict() {
    let Some(db) = database() else { return };
    let (first, second) = (machine(), machine());

    let saved = answer(
        &save_note(&first, db.url(), SHARED, "repo", "Shared title", &["--place", "remote"]),
        0,
    );
    let id = saved["id"].as_i64().expect("id");

    let conflict = save_note(
        &second,
        db.url(),
        SHARED,
        "repo",
        "Shared title",
        &["--place", "remote"],
    );
    let said = answer(&conflict, 1);
    assert_eq!(said["error"], "refused", "{said}");

    let message = said["message"].as_str().expect("message");
    assert!(
        message.contains(&format!("note {id} already has this title")),
        "{message}"
    );
    assert!(message.contains("another checkout"), "{message}");

    // The same checkout replaces its own note, as a save to the team database does.
    let again = answer(
        &save_note(&first, db.url(), SHARED, "repo", "Shared title", &["--place", "remote"]),
        0,
    );
    assert_eq!(again["id"].as_i64(), Some(id));
    assert_eq!(team_titles(&db), ["Shared title"]);
}

#[test]
fn the_mcp_tools_have_no_place_argument() {
    let machine = machine();
    let mut client = Client::start(command(&machine.memory, &machine.repo));

    let listed = client.request("tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().expect("tools");

    for name in ["note_save", "script_save"] {
        let tool = tools.iter().find(|tool| tool["name"] == name).expect("the tool");
        let properties = tool["inputSchema"]["properties"].as_object().expect("properties");
        assert!(!properties.contains_key("place"), "{name} takes no place");
    }

    // A script call with an argument the tool does not have is refused.
    let script = client.request(
        "tools/call",
        json!({ "name": "script_save", "arguments": {
            "name": "placed", "description": "", "language": "sh", "body": "true", "place": "remote",
        } }),
    );
    assert_eq!(script["result"]["isError"], true, "{script}");
    assert!(client.finish().success());

    assert_eq!(local_script_names(&machine), Vec::<String>::new());
}

#[test]
fn a_place_an_agent_sends_over_mcp_changes_nothing() {
    let Some(db) = database() else { return };
    let machine = machine();
    let mut client = Client::start(binary(&machine, &["mcp"], db.url(), SHARED));

    // Whatever the note tool does with the extra argument, the note goes where the routing says: here.
    client.request(
        "tools/call",
        json!({ "name": "note_save", "arguments": {
            "title": "Placed", "body": "Body", "kind": "decision", "place": "remote",
        } }),
    );
    assert!(client.finish().success());

    assert_eq!(team_titles(&db), Vec::<String>::new());
    assert_eq!(local_titles(&machine), ["Placed"]);
}

#[test]
fn features_tell_that_a_place_can_be_picked_and_where_remote_local_saves() {
    let machine = machine();
    let output = run(binary(&machine, &["features"], NOBODY, SHARED));
    let features = stdout(&output);

    for feature in ["save-place", "remote-local-saves", "held-save-place"] {
        assert!(features.lines().any(|line| line == feature), "{feature}: {features}");
    }
}
