//! The note commands of the real executable: `notes list --json` pages the notes of `--project`'s
//! repository and the global scope, `notes get <id> --json` shows one with its body and the notes
//! linked through the graph, `notes delete <id>` deletes it (exit 1 and `note <id> not found` when
//! it is not visible) and `notes delete --all --yes` every note the list shows (exit 2 without
//! `--yes`), on the local database, on each team database whose test server is set, and
//! with one scope kept on this computer.

mod common;
mod libsql_support;
mod mongo_support;
mod postgres_support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::mcp::repo_with_remote;
use common::stdio::{BINARY, Client, command, without_team_settings};
use harness_memory::store::{MemoryStore, NoteDraft, NoteKind, Provenance, SqliteStore, Target};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tempfile::TempDir;

/// A data folder, a folder outside any repository, and two repositories with remotes.
struct Folders {
    _dir: TempDir,
    data: PathBuf,
    work: PathBuf,
    widget: PathBuf,
    gadget: PathBuf,
    /// The team database settings (`HARNESS_MEMORY_*` variables) every command gets.
    team: Vec<(&'static str, String)>,
}

fn folders() -> Folders {
    let dir = TempDir::new().expect("temp");
    let data = dir.path().join("data");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&data).expect("mkdir");
    std::fs::create_dir_all(&work).expect("mkdir");

    Folders {
        widget: repo_with_remote(dir.path(), "widget", "https://github.com/acme/widget.git"),
        gadget: repo_with_remote(dir.path(), "gadget", "https://github.com/acme/gadget.git"),
        _dir: dir,
        data,
        work,
        team: Vec::new(),
    }
}

impl Folders {
    /// The same folders on the team database `url`, with `routing` (repository and global notes)
    /// when given.
    fn on_team(mut self, url: &str, routing: Option<(&str, &str)>) -> Self {
        self.team.push(("HARNESS_MEMORY_DATABASE_URL", url.to_owned()));

        if let Some((repository, global)) = routing {
            self.team
                .push(("HARNESS_MEMORY_REPOSITORY_NOTES", repository.to_owned()));
            self.team.push(("HARNESS_MEMORY_GLOBAL_NOTES", global.to_owned()));
        }

        self
    }

    fn with_team(&self, command: &mut Command) {
        for (name, value) in &self.team {
            command.env(name, value);
        }
    }

    /// An MCP server running in `cwd`.
    fn server(&self, cwd: &Path) -> Client {
        let mut command = command(&self.data, cwd);
        self.with_team(&mut command);
        Client::start(command)
    }

    /// `harness-memory <args>`, started outside any repository.
    fn cli(&self, args: &[&str]) -> Output {
        let mut command = Command::new(BINARY);
        command
            .args(args)
            .current_dir(&self.work)
            .env("HARNESS_MEMORY_DIR", &self.data);
        self.with_team(&mut command);
        without_team_settings(&mut command);
        command.output().expect("the executable runs")
    }

    /// The parsed stdout of a successful `harness-memory <args>`.
    fn json(&self, args: &[&str]) -> Value {
        let output = self.cli(args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        serde_json::from_str(&stdout(&output)).expect("JSON")
    }

    /// `notes list --json --project <widget> <extra>`.
    fn widget_list(&self, extra: &[&str]) -> Value {
        let widget = self.widget.to_string_lossy().into_owned();
        let args: Vec<&str> = ["notes", "list", "--json", "--project", &widget]
            .iter()
            .chain(extra)
            .copied()
            .collect();
        self.json(&args)
    }

    fn widget_project(&self) -> String {
        self.widget.to_string_lossy().into_owned()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Remembers a note through the server and returns its id.
fn remember(server: &mut Client, arguments: Value) -> i64 {
    let text = server.call("memory_remember", arguments);
    let id = text
        .lines()
        .find_map(|line| line.strip_prefix("created note "))
        .and_then(|rest| rest.split_whitespace().next())
        .expect("a created note");
    id.parse().expect("an id")
}

fn titles(listing: &Value) -> Vec<&str> {
    listing["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .map(|note| note["title"].as_str().expect("title"))
        .collect()
}

/// The widget notes `Flaky test` (problem) and `Raise the timeout` (fix), a gadget note and a
/// global decision, written in that order; their ids.
fn seed(folders: &Folders) -> [i64; 4] {
    let mut widget = folders.server(&folders.widget);
    let flaky = remember(
        &mut widget,
        json!({ "title": "Flaky test", "body": "Times out on CI", "kind": "problem", "tags": ["ci"], "entities": ["runner.rs"] }),
    );
    let fix = remember(
        &mut widget,
        json!({ "title": "Raise the timeout", "body": "To 30 s", "kind": "fix", "entities": ["timeout.toml"] }),
    );
    let tabs = remember(
        &mut widget,
        json!({ "title": "Editor tabs", "body": "Four wide", "kind": "decision", "scope": "global" }),
    );
    assert!(widget.finish().success());

    let mut gadget = folders.server(&folders.gadget);
    let other = remember(
        &mut gadget,
        json!({ "title": "Gadget only", "body": "Not for widget", "kind": "note" }),
    );
    assert!(gadget.finish().success());

    [flaky, fix, other, tabs]
}

/// Checks list, get and delete on notes [`seed`] wrote; `place` is where each scope is kept.
fn lists_gets_and_deletes(folders: &Folders, repo_place: &str, global_place: &str) {
    let [flaky, fix, other, tabs] = seed(folders);

    let global = folders.json(&["notes", "list", "--json"]);
    assert_eq!(
        titles(&global),
        ["Editor tabs"],
        "without --project only the global notes"
    );

    assert_eq!(global["scopes"], json!({ "repo": 0, "global": 1 }));

    let listing = folders.widget_list(&["--sort", "title"]);
    let keys: Vec<&String> = listing.as_object().expect("object").keys().collect();
    assert_eq!(keys, ["total", "kinds", "scopes", "notes"]);
    assert_eq!(listing["scopes"], json!({ "repo": 2, "global": 1 }));
    assert_eq!(listing["total"], 3);
    assert_eq!(
        titles(&listing),
        ["Editor tabs", "Flaky test", "Raise the timeout"],
        "the gadget note is hidden"
    );
    assert_eq!(
        listing["kinds"],
        json!([
            { "kind": "outcome", "count": 0 },
            { "kind": "problem", "count": 1 },
            { "kind": "fix", "count": 1 },
            { "kind": "optimisation", "count": 0 },
            { "kind": "location", "count": 0 },
            { "kind": "decision", "count": 1 },
            { "kind": "note", "count": 0 },
        ])
    );

    let notes = listing["notes"].as_array().expect("notes");
    let flaky_row = &notes[1];
    let keys: Vec<&String> = flaky_row.as_object().expect("object").keys().collect();
    assert_eq!(
        keys,
        [
            "id",
            "kind",
            "title",
            "scope",
            "repo",
            "place",
            "tags",
            "entities",
            "author",
            "createdAt",
            "updatedAt",
            "revision",
            "hits"
        ]
    );
    assert_eq!(flaky_row["id"], flaky);
    assert_eq!(flaky_row["kind"], "problem");
    assert_eq!(flaky_row["scope"], "repo");
    assert_eq!(flaky_row["repo"], "github.com/acme/widget");
    assert_eq!(flaky_row["place"], repo_place);
    assert_eq!(flaky_row["tags"], json!(["ci"]));
    assert_eq!(flaky_row["entities"], json!(["runner.rs"]));
    assert_eq!(flaky_row["author"]["source"], "tests");
    assert!(flaky_row["author"]["root"].as_str().expect("root").ends_with("widget"));
    assert!(flaky_row["createdAt"].as_str().expect("time").ends_with('Z'));
    assert_eq!(
        (flaky_row["revision"].clone(), flaky_row["hits"].clone()),
        (json!(1), json!(0))
    );
    assert_eq!(notes[0]["id"], tabs);
    assert_eq!(notes[0]["scope"], "global");
    assert_eq!(notes[0]["repo"], Value::Null);
    assert_eq!(notes[0]["place"], global_place);

    assert_eq!(
        titles(&folders.widget_list(&["--sort", "scope"])),
        ["Editor tabs", "Flaky test", "Raise the timeout"],
        "global notes first, then by id"
    );
    assert_eq!(
        titles(&folders.widget_list(&["--sort", "kind", "--order", "desc"])),
        ["Flaky test", "Raise the timeout", "Editor tabs"],
        "problem, fix, decision"
    );
    let local_first = if global_place == "local" && repo_place == "team" {
        ["Editor tabs", "Flaky test", "Raise the timeout"]
    } else {
        ["Flaky test", "Raise the timeout", "Editor tabs"]
    };
    assert_eq!(titles(&folders.widget_list(&["--sort", "place"])), local_first);

    let newest = folders.widget_list(&["--limit", "1"]);
    assert_eq!(newest["total"], 3);
    assert_eq!(newest["notes"].as_array().expect("notes").len(), 1);

    let fixes = folders.widget_list(&["--kind", "fix", "--query", "TIMEOUT"]);
    assert_eq!(titles(&fixes), ["Raise the timeout"]);
    assert_eq!(fixes["total"], 1);
    assert_eq!(fixes["scopes"], json!({ "repo": 1, "global": 0 }));

    filters_combine(folders);

    let project = folders.widget_project();
    let shown = folders.json(&["notes", "get", &fix.to_string(), "--json", "--project", &project]);
    assert_eq!(shown["note"]["body"], "To 30 s");
    assert_eq!(shown["note"]["title"], "Raise the timeout");
    assert_eq!(shown["note"]["place"], repo_place);
    assert_eq!(shown["links"], json!([]));

    let fix_id = fix.to_string();
    let get_fix = ["notes", "get", &fix_id, "--json", "--project", &project];
    assert_eq!(shown["note"]["hits"], 0);
    assert_eq!(
        folders.json(&get_fix)["note"]["hits"],
        0,
        "showing a note counts no hit"
    );

    let mut widget = folders.server(&folders.widget);
    widget.call("memory_get", json!({ "ids": [fix] }));
    assert!(widget.finish().success());
    assert_eq!(folders.json(&get_fix)["note"]["hits"], 1, "memory_get counts a hit");

    for args in [
        vec!["notes", "get", &other.to_string(), "--json", "--project", &project],
        vec!["notes", "get", "999999", "--json"],
        vec!["notes", "delete", &other.to_string(), "--project", &project],
        vec!["notes", "get", &flaky.to_string(), "--json"],
    ] {
        let output = folders.cli(&args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(
            stderr(&output).lines().last(),
            Some(format!("harness-memory: note {} not found", args[2]).as_str()),
            "{args:?}"
        );
    }

    let output = folders.cli(&["notes", "delete", &flaky.to_string(), "--project", &project]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("deleted note {flaky} Flaky test\n"));
    assert_eq!(folders.widget_list(&[])["total"], 2);
}

/// The count of `kind` in the `kinds` of `listing`.
fn kind_count(listing: &Value, kind: &str) -> u64 {
    listing["kinds"]
        .as_array()
        .expect("kinds")
        .iter()
        .find(|entry| entry["kind"] == kind)
        .and_then(|entry| entry["count"].as_u64())
        .expect("every kind is counted")
}

/// Checks `--scope` with `--kind` and `--query` on the widget notes [`seed`] wrote: the counts per
/// kind follow the query and the scope, the counts per scope the query and the kind, the total
/// every filter.
fn filters_combine(folders: &Folders) {
    let repo_side = folders.widget_list(&["--scope", "repo", "--sort", "title"]);
    assert_eq!(titles(&repo_side), ["Flaky test", "Raise the timeout"]);
    assert_eq!(repo_side["total"], 2);
    assert_eq!(
        repo_side["scopes"],
        json!({ "repo": 2, "global": 1 }),
        "the scope counts ignore the scope"
    );
    assert_eq!(
        (
            kind_count(&repo_side, "problem"),
            kind_count(&repo_side, "fix"),
            kind_count(&repo_side, "decision")
        ),
        (1, 1, 0),
        "the kind counts follow the scope"
    );

    let global_decisions = folders.widget_list(&["--scope=global", "--kind", "decision"]);
    assert_eq!(titles(&global_decisions), ["Editor tabs"]);
    assert_eq!(global_decisions["total"], 1);
    assert_eq!(global_decisions["scopes"], json!({ "repo": 0, "global": 1 }));
    assert_eq!(kind_count(&global_decisions, "decision"), 1);
    assert_eq!(kind_count(&global_decisions, "fix"), 0);

    let none = folders.widget_list(&["--scope", "global", "--kind", "fix"]);
    assert_eq!((none["total"].clone(), titles(&none).len()), (json!(0), 0));
    assert_eq!(none["scopes"], json!({ "repo": 1, "global": 0 }));
    assert_eq!(kind_count(&none, "decision"), 1);

    let timeout = folders.widget_list(&["--scope", "repo", "--kind", "fix", "--query", "timeout"]);
    assert_eq!(titles(&timeout), ["Raise the timeout"]);
    assert_eq!(timeout["total"], 1);
    assert_eq!(timeout["scopes"], json!({ "repo": 1, "global": 0 }));
    assert_eq!(
        (kind_count(&timeout, "fix"), kind_count(&timeout, "problem")),
        (1, 0),
        "the kind counts follow the query"
    );

    let paged = folders.widget_list(&["--scope", "repo", "--sort", "title", "--offset", "1", "--limit", "1"]);
    assert_eq!(titles(&paged), ["Raise the timeout"]);
    assert_eq!(paged["total"], 2);

    let global = folders.json(&["notes", "list", "--json", "--scope", "repo"]);
    assert_eq!(global["total"], 0, "without --project no repository notes");
    assert_eq!(global["scopes"], json!({ "repo": 0, "global": 1 }));
}

/// Checks `notes delete --all` on notes [`seed`] wrote; `repo_team` and `global_team` say whether
/// the team database keeps that scope.
fn deletes_every_listed_note(folders: &Folders, repo_team: bool, global_team: bool) {
    seed(folders);
    let project = folders.widget_project();
    let gadget = folders.gadget.to_string_lossy().into_owned();

    let unconfirmed = folders.cli(&["notes", "delete", "--all", "--project", &project]);
    assert_eq!(unconfirmed.status.code(), Some(2));
    assert!(
        stderr(&unconfirmed).trim_end().ends_with("add --yes to delete them"),
        "{}",
        stderr(&unconfirmed)
    );
    assert_eq!(folders.widget_list(&[])["total"], 3, "nothing is deleted without --yes");

    let deleted = folders.json(&["notes", "delete", "--all", "--yes", "--json", "--project", &project]);
    let team = 2 * usize::from(repo_team) + usize::from(global_team);
    assert_eq!(deleted, json!({ "deleted": 3, "team": team }));
    assert_eq!(folders.widget_list(&[])["total"], 0);
    assert_eq!(
        folders.json(&["notes", "list", "--json"])["total"],
        0,
        "the global note went too"
    );

    let left = folders.json(&["notes", "list", "--json", "--project", &gadget]);
    assert_eq!(titles(&left), ["Gadget only"], "another repository's notes stay");

    let output = folders.cli(&["notes", "delete", "--all", "--yes", "--project", &gadget]);
    assert!(output.status.success(), "{}", stderr(&output));
    let expected = if repo_team {
        "deleted 1 note, 1 from the team database\n"
    } else {
        "deleted 1 note\n"
    };
    assert_eq!(stdout(&output), expected);
}

#[test]
fn notes_on_the_local_database() {
    lists_gets_and_deletes(&folders(), "local", "local");
    deletes_every_listed_note(&folders(), false, false);
}

#[test]
fn delete_all_takes_every_page_of_many_notes() {
    let folders = folders();
    let mut store = SqliteStore::open(&folders.data.join("memory.sqlite3")).expect("store");

    for index in 0..1200 {
        let draft = NoteDraft {
            title: format!("Note {index}"),
            body: "Body".to_owned(),
            kind: NoteKind::Note,
            tags: Vec::new(),
            entities: vec![format!("file-{}.rs", index % 50)],
            provenance: Provenance::default(),
        };
        store.remember(&Target::Global, &draft).expect("remember");
    }
    drop(store);

    let deleted = folders.json(&["notes", "delete", "--all", "--yes", "--json"]);
    assert_eq!(deleted, json!({ "deleted": 1200, "team": 0 }));
    assert_eq!(folders.json(&["notes", "list", "--json"])["total"], 0);
}

#[test]
fn notes_on_postgres_and_split_by_scope() {
    let Some(db) = postgres_support::database() else { return };
    lists_gets_and_deletes(&folders().on_team(db.url(), None), "team", "team");

    let Some(db) = postgres_support::database() else { return };
    deletes_every_listed_note(&folders().on_team(db.url(), None), true, true);

    let Some(db) = postgres_support::database() else { return };
    deletes_every_listed_note(&folders().on_team(db.url(), Some(("local", "remote"))), false, true);

    let Some(db) = postgres_support::database() else { return };
    let split = folders().on_team(db.url(), Some(("local", "remote")));
    lists_gets_and_deletes(&split, "local", "team");

    let listing = split.widget_list(&[]);
    let ids: Vec<i64> = listing["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .filter(|note| note["scope"] == "repo")
        .map(|note| note["id"].as_i64().expect("id"))
        .collect();
    assert!(
        ids.iter().all(|id| *id >= 1_000_000_000),
        "local ids are shown above the offset: {ids:?}"
    );
}

#[test]
fn notes_on_libsql() {
    let Some(db) = libsql_support::Database::libsql() else {
        return;
    };
    let libsql_support::Database::Libsql(server) = &db else {
        return;
    };
    lists_gets_and_deletes(&folders().on_team(&server.url, None), "team", "team");
    drop(db);

    let Some(db) = libsql_support::Database::libsql() else {
        return;
    };
    let libsql_support::Database::Libsql(server) = &db else {
        return;
    };
    deletes_every_listed_note(&folders().on_team(&server.url, None), true, true);
}

#[test]
fn notes_on_mongo_with_global_notes_local() {
    let Some(db) = mongo_support::TestDb::new() else { return };
    lists_gets_and_deletes(&folders().on_team(&db.url, None), "team", "team");

    let Some(db) = mongo_support::TestDb::new() else { return };
    lists_gets_and_deletes(&folders().on_team(&db.url, Some(("remote", "local"))), "team", "local");

    let Some(db) = mongo_support::TestDb::new() else { return };
    deletes_every_listed_note(&folders().on_team(&db.url, Some(("remote", "local"))), true, false);
}

#[test]
fn get_lists_the_notes_linked_through_relations() {
    let folders = folders();
    let mut server = folders.server(&folders.widget);
    let auth = remember(
        &mut server,
        json!({ "title": "Auth flow", "body": "Tokens refresh", "kind": "note", "entities": ["auth.rs"] }),
    );
    let pool = remember(
        &mut server,
        json!({ "title": "Pool size", "body": "Ten", "kind": "decision", "entities": ["db.rs"] }),
    );
    let cache = remember(
        &mut server,
        json!({ "title": "Cache layer", "body": "In front", "kind": "location", "entities": ["cache.rs", "auth.rs"] }),
    );
    let lone = remember(
        &mut server,
        json!({ "title": "Unlinked", "body": "Alone", "kind": "note", "entities": ["misc.rs"] }),
    );
    server.call(
        "memory_link",
        json!({ "from": "auth.rs", "relation": "depends_on", "to": "db.rs" }),
    );
    server.call(
        "memory_link",
        json!({ "from": "cache.rs", "relation": "wraps", "to": "auth.rs" }),
    );
    assert!(server.finish().success());

    let project = folders.widget_project();
    let shown = folders.json(&["notes", "get", &auth.to_string(), "--json", "--project", &project]);
    let mut links = shown["links"].as_array().expect("links").clone();
    links.sort_by_key(|link| link["id"].as_i64());

    assert_eq!(
        links,
        [
            json!({ "id": pool, "title": "Pool size", "kind": "decision", "relation": "depends_on", "direction": "out" }),
            json!({ "id": cache, "title": "Cache layer", "kind": "location", "relation": "wraps", "direction": "in" }),
        ]
    );
    assert_eq!(shown["note"]["entities"], json!(["auth.rs"]));
    assert_eq!(shown["note"]["hits"], 0, "no hit counted yet");

    let again = folders.json(&["notes", "get", &auth.to_string(), "--json", "--project", &project]);
    assert_eq!(again["note"]["hits"], 0, "showing a note counts no hit");

    let alone = folders.json(&["notes", "get", &lone.to_string(), "--json", "--project", &project]);
    assert_eq!(alone["links"], json!([]));

    let from_pool = folders.json(&["notes", "get", &pool.to_string(), "--json", "--project", &project]);
    let linked: Vec<(i64, &str)> = from_pool["links"]
        .as_array()
        .expect("links")
        .iter()
        .map(|link| {
            (
                link["id"].as_i64().expect("id"),
                link["direction"].as_str().expect("direction"),
            )
        })
        .collect();
    assert_eq!(linked.len(), 2, "both notes on auth.rs: {linked:?}");
    assert!(linked.iter().all(|(_, direction)| *direction == "in"));
}

#[test]
fn authors_are_shown_display_safe() {
    let folders = folders();
    let mut server = folders.server(&folders.widget);
    let id = remember(
        &mut server,
        json!({ "title": "Written oddly", "body": "Body", "kind": "note" }),
    );
    let unknown = remember(
        &mut server,
        json!({ "title": "No writer", "body": "Body", "kind": "note" }),
    );
    assert!(server.finish().success());

    // A teammate's tool can store anything behind the server's back.
    let conn = rusqlite::Connection::open(folders.data.join("memory.sqlite3")).expect("raw");
    let source = format!("agent\u{202e}\u{7}{}", "x".repeat(100));
    let root = format!("/w\n{}", "r".repeat(600));
    conn.execute(
        "UPDATE notes SET source = ?2, writer_root = ?3 WHERE id = ?1",
        rusqlite::params![id, source, root],
    )
    .expect("raw update");
    conn.execute(
        "UPDATE notes SET source = ?2, writer_root = NULL WHERE id = ?1",
        rusqlite::params![unknown, "\u{200b}"],
    )
    .expect("raw update");

    let listing = folders.widget_list(&["--sort", "title"]);
    let notes = listing["notes"].as_array().expect("notes");

    assert_eq!(
        notes[1]["author"],
        json!({ "source": format!("agent{}", "x".repeat(59)), "root": format!("/w {}", "r".repeat(509)) })
    );
    assert_eq!(notes[0]["author"], json!({ "source": "unknown", "root": null }));
}

#[test]
fn bad_usage_exits_2() {
    let folders = folders();

    for args in [
        &["notes"][..],
        &["notes", "list"],
        &["notes", "list", "--json", "--limit", "0"],
        &["notes", "list", "--json", "--limit", "201"],
        &["notes", "list", "--json", "--kind", "idea"],
        &["notes", "list", "--json", "--sort", "hits"],
        &["notes", "list", "--json", "--sort", "Kind"],
        &["notes", "list", "--json", "--order", "up"],
        &["notes", "list", "--json", "--scope", "team"],
        &["notes", "list", "--json", "--scope", "Repo"],
        &["notes", "list", "--json", "--scope", ""],
        &["notes", "list", "--json", "--scope"],
        &["notes", "list", "--json", "--scope", "repo", "--scope", "global"],
        &["notes", "get", "1"],
        &["notes", "get", "x", "--json"],
        &["notes", "delete"],
        &["notes", "delete", "1", "--json"],
        &["notes", "delete", "1", "--yes"],
        &["notes", "delete", "--yes"],
        &["notes", "delete", "--all", "--yes", "1"],
        &["notes", "show", "1"],
    ] {
        let output = folders.cli(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            stderr(&output).starts_with("usage: harness-memory notes"),
            "{}",
            stderr(&output)
        );
    }

    let long = "x".repeat(257);
    let output = folders.cli(&["notes", "list", "--json", "--query", &long]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output).lines().last(),
        Some("harness-memory: query is longer than 256 characters")
    );
}
