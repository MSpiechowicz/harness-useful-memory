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

    /// Routes repository and global notes as given from now on.
    fn route(&mut self, repository: &str, global: &str) {
        self.team.retain(|(name, _)| !name.ends_with("_NOTES"));
        self.team
            .push(("HARNESS_MEMORY_REPOSITORY_NOTES", repository.to_owned()));
        self.team.push(("HARNESS_MEMORY_GLOBAL_NOTES", global.to_owned()));
    }

    /// `harness-memory <args>` with `input` on stdin.
    fn cli_with_input(&self, args: &[&str], input: &str) -> Output {
        use std::io::Write;

        let mut command = Command::new(BINARY);
        command
            .args(args)
            .current_dir(&self.work)
            .env("HARNESS_MEMORY_DIR", &self.data)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        self.with_team(&mut command);
        without_team_settings(&mut command);

        let mut child = command.spawn().expect("the executable runs");
        let mut stdin = child.stdin.take().expect("stdin");

        // A command that stops reading early closes the pipe, so the rest of `input` is dropped.
        let _ = stdin.write_all(input.as_bytes());
        drop(stdin);
        child.wait_with_output().expect("the executable ends")
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
    let text = server.call("note_save", arguments);
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

    if folders.team.is_empty() {
        assert_eq!(keys, ["total", "kinds", "scopes", "places", "notes"]);
    } else {
        assert_eq!(
            keys,
            [
                "total",
                "kinds",
                "scopes",
                "places",
                "notes",
                "hiddenCount",
                "localCounts"
            ],
            "a team database adds the hidden and local counts"
        );
        assert_eq!(listing["hiddenCount"], 0);
    }

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
            "repoKey",
            "place",
            "readOnly",
            "hidden",
            "tags",
            "entities",
            "author",
            "createdAt",
            "updatedAt",
            "revision",
            "hits",
            "lang"
        ]
    );
    assert_eq!(flaky_row["id"], flaky);
    assert_eq!(flaky_row["kind"], "problem");
    assert_eq!(flaky_row["scope"], "repo");
    assert_eq!(flaky_row["repo"], "github.com/acme/widget");
    assert_eq!(flaky_row["place"], repo_place);
    assert_eq!(
        (flaky_row["readOnly"].clone(), flaky_row["hidden"].clone()),
        (json!(false), json!(false))
    );
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
    places_filter(folders, repo_place, global_place);

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
    widget.call("note_get", json!({ "ids": [fix] }));
    assert!(widget.finish().success());
    assert_eq!(folders.json(&get_fix)["note"]["hits"], 1, "note_get counts a hit");

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

/// Checks `--place` on the widget notes [`seed`] wrote, `repo_place` and `global_place` being where
/// each scope is kept: the page and the total follow the place, the counts per place every other
/// filter. A lone store asked for the place it does not keep lists nothing.
fn places_filter(folders: &Folders, repo_place: &str, global_place: &str) {
    let of = |place: &str| -> Vec<&str> {
        [
            ("Editor tabs", global_place),
            ("Flaky test", repo_place),
            ("Raise the timeout", repo_place),
        ]
        .into_iter()
        .filter(|(_, at)| *at == place)
        .map(|(title, _)| title)
        .collect()
    };
    let counts = json!({ "local": of("local").len(), "team": of("team").len() });

    assert_eq!(folders.widget_list(&[])["places"], counts);

    for place in ["local", "team"] {
        let listing = folders.widget_list(&["--place", place, "--sort", "title"]);
        assert_eq!(titles(&listing), of(place), "{place}");
        assert_eq!(
            listing["total"],
            of(place).len(),
            "{place}: the total follows the place"
        );
        assert_eq!(listing["places"], counts, "{place}: the place counts ignore the place");
        assert!(
            listing["notes"]
                .as_array()
                .expect("notes")
                .iter()
                .all(|note| note["place"] == place),
            "{place}"
        );

        let repo_count = if repo_place == place { 2 } else { 0 };
        let global_count = usize::from(global_place == place);
        assert_eq!(
            listing["scopes"],
            json!({ "repo": repo_count, "global": global_count }),
            "{place}: the scope counts follow the place"
        );
        assert_eq!(kind_count(&listing, "fix"), repo_count / 2, "{place}");

        let paged = folders.widget_list(&["--place", place, "--sort", "title", "--offset", "1", "--limit", "1"]);
        assert_eq!(titles(&paged), of(place).get(1..2).unwrap_or_default(), "{place}");
        assert_eq!(paged["total"], of(place).len(), "{place}");
    }

    let fixes = folders.widget_list(&["--kind", "fix", "--place", repo_place]);
    assert_eq!(titles(&fixes), ["Raise the timeout"]);
    let local_fixes = usize::from(repo_place == "local");
    assert_eq!(
        fixes["places"],
        json!({ "local": local_fixes, "team": 1 - local_fixes }),
        "the place counts follow the kind"
    );

    let global = folders.json(&["notes", "list", "--json", "--place", global_place]);
    assert_eq!(titles(&global), ["Editor tabs"]);
    let local_global = usize::from(global_place == "local");
    assert_eq!(
        global["places"],
        json!({ "local": local_global, "team": 1 - local_global })
    );
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
fn every_repository_is_read_by_key_or_all_at_once() {
    let folders = folders();
    let [flaky, fix, other, tabs] = seed(&folders);
    let cli = |args: &[&str]| {
        let output = folders.cli(args);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {}", stderr(&output));
        serde_json::from_str::<Value>(&stdout(&output)).expect("json")
    };

    let all = cli(&["notes", "list", "--json", "--all-repos"]);
    assert_eq!(all["total"], 4);
    assert_eq!(all["scopes"], json!({ "repo": 3, "global": 1 }));

    let repos = cli(&["notes", "repos", "--json"]);
    assert_eq!(
        repos,
        json!({
            "repos": [
                { "key": "git:github.com/acme/gadget", "label": "github.com/acme/gadget", "count": 1 },
                { "key": "git:github.com/acme/widget", "label": "github.com/acme/widget", "count": 2 },
            ],
            "global": 1,
        })
    );

    let widget = cli(&[
        "notes",
        "list",
        "--json",
        "--repo",
        "git:github.com/acme/widget",
        "--scope",
        "repo",
    ]);
    assert_eq!(widget["total"], 2);
    assert_eq!(titles(&widget).len(), 2);
    assert_eq!(widget["notes"][0]["repoKey"], "git:github.com/acme/widget");

    // A note of another repository is read and deleted by its id when every repository is in view.
    let note = cli(&["notes", "get", &other.to_string(), "--json", "--all-repos"]);
    assert_eq!(note["note"]["title"], "Gadget only");
    assert_eq!(
        folders
            .cli(&["notes", "get", &other.to_string(), "--json"])
            .status
            .code(),
        Some(1)
    );
    assert_eq!(
        folders
            .cli(&["notes", "delete", &fix.to_string(), "--all-repos"])
            .status
            .code(),
        Some(0)
    );
    assert_eq!(cli(&["notes", "list", "--json", "--all-repos"])["total"], 3);

    // The choices exclude each other, and every repository is never deleted at once.
    for bad in [
        &["notes", "list", "--json", "--all-repos", "--project", "/w"][..],
        &["notes", "list", "--json", "--repo", "git:a/b/c", "--all-repos"],
        &["notes", "delete", "--all", "--yes", "--all-repos"],
        // Every repository is not a key: `*` is what the server uses inside, and a key starts with git: or path:.
        &["notes", "list", "--json", "--repo", "*"],
        &["notes", "delete", "--all", "--yes", "--repo", "*"],
        &["scripts", "delete", "--all", "--yes", "--repo", "*"],
        &["trust", "--all", "--repo", "*"],
        &["notes", "delete", "7", "--scope", "repo"],
    ] {
        assert_eq!(folders.cli(bad).status.code(), Some(2), "{bad:?}");
    }

    // Deleting all of one repository by its key leaves the global notes, with `--scope repo`.
    let deleted = cli(&[
        "notes",
        "delete",
        "--all",
        "--yes",
        "--json",
        "--repo",
        "git:github.com/acme/gadget",
        "--scope",
        "repo",
    ]);
    assert_eq!(deleted["deleted"], 1);
    let rest = cli(&["notes", "list", "--json", "--all-repos"]);
    assert_eq!(rest["scopes"], json!({ "repo": 1, "global": 1 }));

    let _ = (flaky, tabs, other);
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
        "entity_link",
        json!({ "from": "auth.rs", "relation": "depends_on", "to": "db.rs" }),
    );
    server.call(
        "entity_link",
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

    // Read among every repository, a note has the links of the repository it lives in.
    let among_all = folders.json(&["notes", "get", &auth.to_string(), "--json", "--all-repos"]);
    let mut among_all_links = among_all["links"].as_array().expect("links").clone();
    among_all_links.sort_by_key(|link| link["id"].as_i64());
    assert_eq!(among_all_links, links);

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
        &["notes", "list", "--json", "--place", "remote"],
        &["notes", "list", "--json", "--place", "Local"],
        &["notes", "list", "--json", "--place"],
        &["notes", "list", "--json", "--place", "local", "--place", "local"],
        &["notes", "list", "--json", "--lang", "polish"],
        &["notes", "list", "--json", "--lang", "EN"],
        &["notes", "list", "--json", "--lang"],
        &["notes", "list", "--json", "--lang", "en", "--lang", "en"],
        &["settings"],
        &["settings", "get"],
        &["settings", "set", "english_notes=true"],
        &["settings", "set", "english_notes=maybe", "--json"],
        &["settings", "set", "english_notes", "--json"],
        &["settings", "set", "other=true", "--json"],
        &["settings", "get", "extra", "--json"],
        &["notes", "get", "1"],
        &["notes", "get", "x", "--json"],
        &["notes", "delete"],
        &["notes", "delete", "1", "--json"],
        &["notes", "delete", "1", "--yes"],
        &["notes", "delete", "--yes"],
        &["notes", "delete", "--all", "--yes", "1"],
        &["notes", "show", "1"],
        &["notes", "hide"],
        &["notes", "hide", "x"],
        &["notes", "unhide", "1", "2"],
        &["notes", "discard-local", "--yes"],
        &["notes", "discard-local", "--all", "3", "--yes"],
        &["notes", "discard-local", "3,x", "--yes"],
        &["notes", "discard-local", "--all", "--yes", "--scope", "team"],
    ] {
        let output = folders.cli(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            stderr(&output).starts_with("usage: harness-memory"),
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

/// The URLs of the team test servers that are set.
fn team_urls() -> Vec<(String, Box<dyn std::any::Any>)> {
    let mut urls: Vec<(String, Box<dyn std::any::Any>)> = Vec::new();

    if let Some(db) = postgres_support::database() {
        urls.push((db.url().to_owned(), Box::new(db)));
    }

    if let Some(db) = mongo_support::TestDb::new() {
        urls.push((db.url.clone(), Box::new(db)));
    }

    if let Some(db) = libsql_support::Database::libsql()
        && let libsql_support::Database::Libsql(server) = &db
    {
        urls.push((server.url.clone(), Box::new(db)));
    }

    urls
}

/// The row of `listing` titled `title`.
fn row<'a>(listing: &'a Value, title: &str) -> &'a Value {
    listing["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .find(|note| note["title"] == title)
        .unwrap_or_else(|| panic!("no {title} in {listing}"))
}

/// The last line of stderr and the exit code of `output`.
fn failure(output: &Output) -> (Option<i32>, String) {
    let text = stderr(output);
    (output.status.code(), text.lines().last().unwrap_or_default().to_owned())
}

#[test]
fn remote_local_notes_are_read_only_and_team_notes_hide_on_this_computer() {
    for (url, _db) in team_urls() {
        reads_both_hides_and_keeps(&url);
    }
}

fn reads_both_hides_and_keeps(url: &str) {
    let mut folders = folders().on_team(url, Some(("remote", "remote")));
    let project = folders.widget_project();

    let mut widget = folders.server(&folders.widget);
    let shared = remember(
        &mut widget,
        json!({ "title": "Team deploy", "body": "deploy with make targets", "kind": "decision" }),
    );
    let rule = remember(
        &mut widget,
        json!({ "title": "Team tabs", "body": "Four wide", "kind": "note", "scope": "global" }),
    );
    assert!(widget.finish().success());

    folders.route("remote+local", "remote");
    let mut widget = folders.server(&folders.widget);
    let saved = widget.call(
        "note_save",
        json!({ "title": "Deploy via make", "body": "deploy with make targets locally", "kind": "fix" }),
    );
    assert!(
        saved
            .lines()
            .any(|line| line.contains(&format!("#{shared} ")) && line.ends_with(" (team, read-only)")),
        "{saved}"
    );

    let refused = widget.request(
        "tools/call",
        json!({ "name": "note_update", "arguments": { "id": shared, "append": "more" } }),
    );
    assert_eq!(refused["result"]["isError"], true);
    let text = refused["result"]["content"][0]["text"].as_str().expect("text");
    assert!(text.contains(&format!("harness-memory notes hide {shared}")), "{text}");
    widget.call("note_update", json!({ "id": rule, "append": "Always" }));
    assert!(widget.finish().success());

    let listing = folders.widget_list(&[]);
    assert_eq!(listing["total"], 3);
    assert_eq!(listing["hiddenCount"], 0);
    assert_eq!(listing["localCounts"], json!({ "repo": 1, "global": 0 }));

    let team_row = row(&listing, "Team deploy");
    assert_eq!(
        (
            team_row["place"].clone(),
            team_row["readOnly"].clone(),
            team_row["hidden"].clone()
        ),
        (json!("team"), json!(true), json!(false))
    );
    let mine = row(&listing, "Deploy via make");
    assert_eq!(
        (mine["place"].clone(), mine["readOnly"].clone()),
        (json!("local"), json!(false))
    );
    let mine = mine["id"].as_i64().expect("id");
    assert!(mine >= 1_000_000_000);
    assert_eq!(
        row(&listing, "Team tabs")["readOnly"],
        false,
        "a remote scope stays editable"
    );

    let shared_id = shared.to_string();
    let hidden = folders.json(&["notes", "hide", &shared_id, "--project", &project, "--json"]);
    assert_eq!(hidden, json!({ "hidden": shared }));
    let again = folders.cli(&["notes", "hide", &shared_id, "--project", &project]);
    assert_eq!(
        stdout(&again),
        format!("hid note {shared} Team deploy\n"),
        "{}",
        stderr(&again)
    );

    let listing = folders.widget_list(&[]);
    assert_eq!(
        (listing["total"].clone(), listing["hiddenCount"].clone()),
        (json!(2), json!(1))
    );
    assert!(titles(&listing).iter().all(|title| *title != "Team deploy"));

    let only_hidden = folders.widget_list(&["--hidden"]);
    assert_eq!(titles(&only_hidden), ["Team deploy"]);
    assert_eq!(only_hidden["total"], 1);
    assert_eq!(
        only_hidden["places"],
        json!({ "local": 0, "team": 1 }),
        "hidden notes are team notes"
    );

    let hidden_here = folders.widget_list(&["--hidden", "--place", "local"]);
    assert_eq!(
        (hidden_here["total"].clone(), titles(&hidden_here).len()),
        (json!(0), 0)
    );
    assert_eq!(hidden_here["places"], json!({ "local": 0, "team": 1 }));

    let team_side = folders.widget_list(&["--place", "team"]);
    assert_eq!(titles(&team_side), ["Team tabs"], "the hidden team note stays out");
    assert_eq!(team_side["total"], 1);
    assert_eq!(team_side["places"], json!({ "local": 1, "team": 1 }));
    assert_eq!(team_side["hiddenCount"], 1);
    assert_eq!(
        (
            row(&only_hidden, "Team deploy")["hidden"].clone(),
            row(&only_hidden, "Team deploy")["readOnly"].clone()
        ),
        (json!(true), json!(true))
    );

    let get = ["notes", "get", &shared_id, "--json", "--project", &project];
    assert_eq!(
        failure(&folders.cli(&get)),
        (Some(1), format!("harness-memory: note {shared} not found"))
    );
    let shown = folders.json(&[&get[..], &["--hidden"]].concat());
    assert_eq!(
        (shown["note"]["hidden"].clone(), shown["note"]["body"].clone()),
        (json!(true), json!("deploy with make targets"))
    );

    let mine_id = mine.to_string();
    assert_eq!(
        failure(&folders.cli(&["notes", "hide", &mine_id, "--project", &project])),
        (
            Some(1),
            format!("harness-memory: note {mine} is saved on this computer, delete it instead")
        )
    );
    assert_eq!(
        failure(&folders.cli(&["notes", "hide", "999999", "--project", &project])),
        (Some(1), "harness-memory: note 999999 not found".to_owned())
    );

    let unhidden = folders.json(&["notes", "unhide", &shared_id, "--json"]);
    assert_eq!(unhidden, json!({ "unhidden": shared }));
    assert_eq!(
        failure(&folders.cli(&["notes", "unhide", &shared_id])),
        (Some(1), format!("harness-memory: note {shared} not found"))
    );
    assert_eq!(folders.widget_list(&[])["hiddenCount"], 0);

    let deleted = folders.json(&["notes", "delete", "--all", "--yes", "--json", "--project", &project]);
    assert_eq!(
        deleted,
        json!({ "deleted": 2, "team": 1, "kept": 1 }),
        "the team's repository note stays"
    );
    assert_eq!(titles(&folders.widget_list(&[])), ["Team deploy"]);

    discards_what_nothing_reads(&mut folders);
}

/// Checks `notes discard-local` on notes left in the local file of `folders` (on a team
/// database).
fn discards_what_nothing_reads(folders: &mut Folders) {
    let mut store = SqliteStore::open(&folders.data.join("memory.sqlite3")).expect("local file");
    let key = harness_memory::store::ProjectKey {
        key: "git:github.com/acme/gadget".to_owned(),
        root: folders.gadget.clone(),
        remote: Some("github.com/acme/gadget".to_owned()),
    };
    let note = |title: &str| NoteDraft {
        title: title.to_owned(),
        body: "Left here".to_owned(),
        kind: NoteKind::Note,
        tags: Vec::new(),
        entities: Vec::new(),
        provenance: Provenance::default(),
    };
    let gadget = store
        .remember(&Target::Repo(key), &note("Gadget leftover"))
        .expect("note")
        .id;
    let global = store
        .remember(&Target::Global, &note("Global leftover"))
        .expect("note")
        .id;
    let other = store
        .remember(&Target::Global, &note("Other leftover"))
        .expect("note")
        .id;
    drop(store);

    let refused = folders.cli(&["notes", "discard-local", "--all", "--yes", "--scope", "repo"]);
    assert_eq!(
        failure(&refused).0,
        Some(1),
        "repository notes are read from this computer"
    );
    assert!(
        failure(&refused).1.contains("repository_notes = \"remote+local\""),
        "{}",
        stderr(&refused)
    );

    let unconfirmed = folders.cli(&["notes", "discard-local", "--all"]);
    assert_eq!(unconfirmed.status.code(), Some(2));

    let ids = format!("{global},{gadget}");
    let one = folders.json(&["notes", "discard-local", &ids, "--yes", "--json"]);
    assert_eq!(one, json!({ "deleted": 1 }), "only the global scope is routed remote");

    folders.route("remote", "remote");
    let piped = folders.cli_with_input(
        &["notes", "discard-local", "-", "--yes", "--json", "--scope", "repo"],
        &format!("{gadget}\n{other}"),
    );
    assert!(piped.status.success(), "{}", stderr(&piped));
    assert_eq!(
        stdout(&piped),
        "{\"deleted\":1}\n",
        "the global note is not in --scope repo"
    );

    let all = folders.cli(&["notes", "discard-local", "--all", "--yes"]);
    assert_eq!(stdout(&all), "deleted 1 note from this computer\n", "{}", stderr(&all));
    assert_eq!(
        folders.widget_list(&[])["localCounts"],
        json!({ "repo": 0, "global": 0 })
    );
}

#[test]
fn hiding_and_discarding_need_a_team_database() {
    let folders = folders();

    for args in [&["notes", "hide", "1"][..], &["notes", "unhide", "1", "--json"]] {
        let (code, last) = failure(&folders.cli(args));
        assert_eq!(code, Some(1), "{args:?}");
        assert!(last.contains("no team notes are read here"), "{last}");
    }

    let (code, last) = failure(&folders.cli(&["notes", "discard-local", "--all", "--yes"]));
    assert_eq!(code, Some(1));
    assert!(last.contains("no team database is set"), "{last}");

    // Ids piped in are capped in count and in bytes.
    let discard = ["notes", "discard-local", "-", "--yes"];
    let many = vec!["7"; 100_001].join(",");
    let output = folders.cli_with_input(&discard, &many);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("at most 100000 ids"), "{}", stderr(&output));

    let huge = " ".repeat(2 * 1024 * 1024 + 1);
    let output = folders.cli_with_input(&discard, &huge);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("more than 2 MiB"), "{}", stderr(&output));

    let hidden = folders.json(&["notes", "list", "--json", "--hidden"]);
    assert_eq!(
        (hidden["total"].clone(), hidden["notes"].clone()),
        (json!(0), json!([]))
    );
    assert!(hidden.get("hiddenCount").is_none());
}

#[test]
fn the_hidden_count_leaves_out_rows_of_notes_the_team_no_longer_has() {
    for (url, _db) in team_urls() {
        stale_rows_leave_the_hidden_count(&url);
    }
}

fn stale_rows_leave_the_hidden_count(url: &str) {
    let folders = folders().on_team(url, Some(("remote", "remote")));
    let project = folders.widget_project();

    let mut widget = folders.server(&folders.widget);
    let reused = remember(
        &mut widget,
        json!({ "title": "Reused id", "body": "A newer note", "kind": "note" }),
    );
    assert!(widget.finish().success());

    let id = reused.to_string();
    folders.json(&["notes", "hide", &id, "--project", &project, "--json"]);
    assert_eq!(folders.widget_list(&[])["hiddenCount"], 1);

    // The team database gave the hidden id to another note (SQLite and libSQL reuse ids).
    let raw = rusqlite::Connection::open(folders.data.join("memory.sqlite3")).expect("raw local");
    raw.execute(
        "UPDATE hidden_team_notes SET team_created_at = team_created_at - 60",
        [],
    )
    .expect("older row");

    // Within the window after the last prune (the commands above pruned) the row still counts,
    // and the note stays left out: a poll every few seconds reads the team database no more.
    let within = folders.widget_list(&[]);
    assert_eq!(
        (within["total"].clone(), within["hiddenCount"].clone()),
        (json!(0), json!(1)),
        "{within}"
    );

    raw.execute("UPDATE hidden_team_notes_meta SET pruned_at = pruned_at - 301", [])
        .expect("window passed");

    let listing = folders.widget_list(&[]);
    assert_eq!(
        (listing["total"].clone(), listing["hiddenCount"].clone()),
        (json!(1), json!(0)),
        "{listing}"
    );
    assert_eq!(folders.widget_list(&["--hidden"])["total"], 0);
}

#[cfg(unix)]
#[test]
fn delete_all_fails_while_the_hidden_list_cannot_be_read() {
    for (url, _db) in team_urls() {
        keeps_hidden_notes_when_unreadable(&url);
    }
}

#[cfg(unix)]
fn keeps_hidden_notes_when_unreadable(url: &str) {
    use std::os::unix::fs::PermissionsExt;

    let folders = folders().on_team(url, Some(("remote", "remote")));
    let project = folders.widget_project();

    let mut widget = folders.server(&folders.widget);
    let hidden = remember(
        &mut widget,
        json!({ "title": "Hidden here", "body": "A teammate's note", "kind": "note" }),
    );
    remember(
        &mut widget,
        json!({ "title": "Shown here", "body": "Another note", "kind": "note" }),
    );
    assert!(widget.finish().success());

    let id = hidden.to_string();
    folders.json(&["notes", "hide", &id, "--project", &project, "--json"]);

    let local = folders.data.join("memory.sqlite3");
    let mode = |bits| std::fs::set_permissions(&local, std::fs::Permissions::from_mode(bits)).expect("chmod");
    mode(0o000);

    if std::fs::File::open(&local).is_ok() {
        mode(0o600);
        eprintln!("skipped: the file stays readable (running as root?)");
        return;
    }

    let output = folders.cli(&["notes", "delete", "--all", "--yes", "--json", "--project", &project]);
    mode(0o600);

    let (code, last) = failure(&output);
    assert_eq!(code, Some(1), "{}", stdout(&output));
    assert!(
        last.contains("cannot read the team notes hidden on this computer"),
        "{last}"
    );

    let shown = folders.widget_list(&[]);
    assert_eq!(titles(&shown), ["Shown here"]);
    assert_eq!(shown["hiddenCount"], 1, "the hidden team note is still there");
}

const ENGLISH_BODY: &str = "The sync job reads every changed note from the team database, then writes the new revision \
                            to the local cache so the next search finds it without another round trip.";

const POLISH_BODY: &str = "Zadanie synchronizacji czyta każdą zmienioną notatkę z bazy zespołu, a potem zapisuje nową \
                           wersję w lokalnej pamięci podręcznej, żeby następne wyszukiwanie znalazło ją od razu.";

/// `notes save --json --project <widget>` of a global note.
fn user_saves(folders: &Folders, title: &str, body: &str) -> (Value, String) {
    let note = json!({ "title": title, "body": body, "kind": "note", "scope": "global" });
    let output = folders.cli_with_input(
        &["notes", "save", "--json", "--project", &folders.widget_project()],
        &note.to_string(),
    );

    assert!(output.status.success(), "{}", stderr(&output));
    (serde_json::from_str(&stdout(&output)).expect("JSON"), stderr(&output))
}

fn languages(listing: &Value) -> Vec<(&str, Value)> {
    listing["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .map(|note| (note["title"].as_str().expect("title"), note["lang"].clone()))
        .collect()
}

/// A user's note that is not in English is saved with a warning, never refused, and lists with its
/// `lang`; `--lang` keeps one language; the setting off silences the warning.
fn user_saves_and_lists_by_language(folders: &Folders) {
    assert_eq!(
        folders.json(&["settings", "get", "--json"]),
        json!({ "english_notes": true })
    );

    let (polish, warned) = user_saves(folders, "Synchronizacja notatek", POLISH_BODY);
    assert_eq!(polish["warning"], "not-english", "{polish}");
    assert!(
        warned.contains("harness-memory: this note is not in English"),
        "{warned}"
    );

    let (english, quiet) = user_saves(folders, "Sync job", ENGLISH_BODY);
    assert!(english.get("warning").is_none(), "{english}");
    assert!(!quiet.contains("not in English"), "{quiet}");

    let (short, quiet) = user_saves(folders, "ok", "x");
    assert!(short.get("warning").is_none() && !quiet.contains("not in English"));

    let listing = folders.widget_list(&["--sort", "title"]);
    assert_eq!(
        languages(&listing),
        [
            ("ok", Value::Null),
            ("Sync job", json!("en")),
            ("Synchronizacja notatek", json!("other")),
        ]
    );

    for (lang, expected) in [
        ("en", "Sync job"),
        ("other", "Synchronizacja notatek"),
        ("unknown", "ok"),
    ] {
        let kept = folders.widget_list(&["--lang", lang]);
        assert_eq!(titles(&kept), [expected], "--lang {lang}");
        assert_eq!(kept["total"], 1);
        assert_eq!(kind_count(&kept, "note"), 1);
    }

    let id = polish["id"].to_string();
    let got = folders.json(&["notes", "get", &id, "--json", "--project", &folders.widget_project()]);
    assert_eq!(got["note"]["lang"], "other");

    let off = folders.json(&["settings", "set", "english_notes=false", "--json"]);
    assert_eq!(off, json!({ "english_notes": false }));
    assert_eq!(
        folders.json(&["settings", "get", "--json"]),
        json!({ "english_notes": false })
    );

    let (silent, quiet) = user_saves(folders, "Inna notatka", POLISH_BODY);
    assert!(silent.get("warning").is_none() && !quiet.contains("not in English"));

    // The list still tells the language.
    assert_eq!(folders.widget_list(&["--lang", "other"])["total"], 2);

    let on = folders.json(&["settings", "set", "english_notes=true", "--json"]);
    assert_eq!(on, json!({ "english_notes": true }));
}

#[test]
fn user_notes_in_another_language_warn_and_list_by_language() {
    user_saves_and_lists_by_language(&folders());

    for (url, _kept) in team_urls() {
        user_saves_and_lists_by_language(&folders().on_team(&url, None));
    }
}

#[test]
fn the_setting_is_kept_where_notes_are_saved() {
    let split = |url: &str| folders().on_team(url, Some(("local", "remote")));

    for (url, _kept) in team_urls() {
        let folders = split(&url);
        folders.json(&["settings", "set", "english_notes=false", "--json"]);

        // A split store keeps it in the team database (global notes go there) and the local file.
        let mut local = SqliteStore::open(&folders.data.join("memory.sqlite3")).expect("local");
        assert!(!local.english_notes().expect("local setting"));

        let again = split(&url);
        assert_eq!(
            again.json(&["settings", "get", "--json"]),
            json!({ "english_notes": false })
        );
    }
}

#[test]
fn the_language_filter_pages_over_many_notes() {
    let folders = folders();
    let mut store = SqliteStore::open(&folders.data.join("memory.sqlite3")).expect("store");

    for index in 0..230 {
        let polish = index % 10 != 0;
        let draft = NoteDraft {
            title: format!("Note {index}"),
            body: if polish { POLISH_BODY } else { ENGLISH_BODY }.to_owned(),
            kind: NoteKind::Note,
            tags: Vec::new(),
            entities: Vec::new(),
            provenance: Provenance::default(),
        };
        store.remember(&Target::Global, &draft).expect("remember");
    }
    drop(store);

    let first = folders.json(&["notes", "list", "--json", "--lang", "other", "--limit", "200"]);
    let second = folders.json(&[
        "notes", "list", "--json", "--lang", "other", "--offset", "200", "--limit", "200",
    ]);
    assert_eq!(
        (first["total"].clone(), second["total"].clone()),
        (json!(207), json!(207))
    );
    assert_eq!(
        (
            first["notes"].as_array().map(Vec::len),
            second["notes"].as_array().map(Vec::len)
        ),
        (Some(200), Some(7))
    );

    let english = folders.json(&["notes", "list", "--json", "--lang", "en", "--limit", "200"]);
    assert_eq!(english["total"], 23);
    assert_eq!(
        folders.json(&["notes", "list", "--json", "--lang", "unknown"])["total"],
        0
    );
}
