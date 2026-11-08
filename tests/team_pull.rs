//! Copying between this computer and the team database for the scopes set to `remote+local`, from
//! the command line: `push --remote-local` and `scripts push --remote-local` take those scopes
//! only, and `pull` and `scripts pull` copy what the user picked from the team database to this
//! computer without ever writing to it. The cases on a real database need `postgres_support`'s
//! server and are skipped without it.

// Most of these tests run only on Unix, so on Windows their helpers and imports are unused.
#![cfg_attr(not(unix), allow(unused))]

mod common;
mod postgres_support;
mod push_support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::mcp::repo_with_remote;
use common::stdio::{BINARY, without_team_settings};
use common::{draft, repo, repo_key};
use harness_memory::store::{
    Author, Language, Link, MemoryStore, Note, NoteKind, NotePatch, Provenance, ScriptDraft, ScriptStore, SqliteStore,
    Target, View,
};
use postgres_support::{Database, database};
use serde_json::{Value, json};
use tempfile::TempDir;

const WIDGET: &str = push_support::REPO;
const OTHER: &str = "github.com/acme/other";

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

/// The executable with `args` on `machine`, the team database `url` and the notes routed as
/// `(repository_notes, global_notes)` and the scripts as `scripts`.
fn binary(machine: &Machine, args: &[&str], url: &str, routing: (&str, &str), scripts: &str) -> Command {
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
        .env("HARNESS_MEMORY_REPOSITORY_NOTES", routing.0)
        .env("HARNESS_MEMORY_GLOBAL_NOTES", routing.1)
        .env("HARNESS_MEMORY_SCRIPTS", scripts)
        // A pull is for `remote+local` scopes that save on this computer.
        .env("HARNESS_MEMORY_REMOTE_LOCAL_SAVES", "local");

    without_team_settings(&mut command);
    command
}

/// Both note scopes and the scripts set to `remote+local`.
const SHARED: (&str, &str) = ("remote+local", "remote+local");

fn shared(machine: &Machine, args: &[&str], url: &str) -> Command {
    binary(machine, args, url, SHARED, "remote+local")
}

fn run(mut command: Command) -> Output {
    command.stdin(Stdio::null()).output().expect("run")
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
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

fn now() -> i64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock");
    i64::try_from(elapsed.as_secs()).expect("seconds")
}

/// Copies the database file at `from` into the folder `memory` as its `memory.sqlite3`.
fn copy_into(from: &Path, memory: &Path) {
    std::fs::create_dir_all(memory).expect("memory folder");
    std::fs::copy(from, memory.join("memory.sqlite3")).expect("copy");
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

fn local_store(machine: &Machine) -> SqliteStore {
    std::fs::create_dir_all(&machine.memory).expect("data folder");
    SqliteStore::open(&machine.memory.join("memory.sqlite3")).expect("local file")
}

/// The local note titled `title` in `target`, if there is one.
fn local_note(machine: &Machine, target: &Target, title: &str) -> Option<Note> {
    local_store(machine).find_by_title(target, title).expect("find")
}

/// Everything in the team database a pull could change: counts, revisions, hit counters and update
/// times of the notes and scripts, and the graph.
fn snapshot(db: &Database) -> String {
    let mut admin = db.admin();
    let row = admin
        .query_one(
            "SELECT concat_ws('|',
                (SELECT count(*) FROM notes),
                (SELECT coalesce(sum(revision), 0) FROM notes),
                (SELECT coalesce(sum(hits), 0) FROM notes),
                (SELECT coalesce(max(updated_at), 0) FROM notes),
                (SELECT count(*) FROM relations),
                (SELECT count(*) FROM entities),
                (SELECT count(*) FROM projects),
                (SELECT count(*) FROM scripts),
                (SELECT coalesce(max(updated_at), 0) FROM scripts),
                (SELECT coalesce(sum(run_count), 0) FROM scripts))",
            &[],
        )
        .expect("snapshot");

    row.get(0)
}

/// A teammate's write: their git name, the client and a checkout of theirs.
fn writer(name: &str) -> Provenance {
    Provenance {
        root: Some(std::env::temp_dir().join("colleague").join("widget")),
        source: Some("omp".to_owned()),
        via_override: false,
        author: Some(Author {
            name: name.to_owned(),
            via: Some("omp".to_owned()),
        }),
        original_added_by: None,
    }
}

/// The ids the team notes of [`seed_team`] have.
struct Team {
    cache: i64,
    deploy: i64,
    lint: i64,
    other: i64,
    tabs: i64,
}

/// Two repository notes of the shared repository with a relation each, one note of another
/// repository and one global note. "Lint rules" was added by Ada and last changed by Grace.
fn seed_team(db: &Database) -> Team {
    let mut store = db.open();
    let widget = repo(WIDGET);

    let mut cache = draft("Build cache", "Lives in target/", NoteKind::Location);
    cache.tags = vec!["build".to_owned()];
    cache.entities = vec!["Cache".to_owned()];
    cache.provenance = writer("Ada Lovelace");
    let cache = store.remember(&widget, &cache).expect("cache").id;

    let located = Link {
        from: "Cache".to_owned(),
        relation: "located_in".to_owned(),
        to: "target/".to_owned(),
        note_id: Some(cache),
    };
    store.link(&widget, &located).expect("cache link");

    let mut deploy = draft("Deploy steps", "Run make deploy", NoteKind::Decision);
    deploy.provenance = writer("Ada Lovelace");
    let deploy = store.remember(&widget, &deploy).expect("deploy").id;

    let mut lint = draft("Lint rules", "Run clippy before pushing", NoteKind::Fix);
    lint.tags = vec!["lint".to_owned()];
    lint.entities = vec!["Clippy".to_owned(), "CI".to_owned()];
    lint.provenance = writer("Ada Lovelace");
    let lint = store.remember(&widget, &lint).expect("lint").id;

    let runs = Link {
        from: "Clippy".to_owned(),
        relation: "runs_on".to_owned(),
        to: "CI".to_owned(),
        note_id: Some(lint),
    };
    store.link(&widget, &runs).expect("lint link");

    let patch = NotePatch {
        tags: Some(vec!["lint".to_owned(), "ci".to_owned()]),
        provenance: writer("Grace Hopper"),
        ..NotePatch::default()
    };
    store.update(&View::around(&widget), lint, &patch).expect("lint update");

    let mut other = draft("Other repo note", "Belongs elsewhere", NoteKind::Note);
    other.provenance = writer("Ada Lovelace");
    let other = store.remember(&repo(OTHER), &other).expect("other").id;

    let mut tabs = draft("Editor tabs", "Four wide", NoteKind::Note);
    tabs.provenance = writer("Ada Lovelace");
    let tabs = store.remember(&Target::Global, &tabs).expect("tabs").id;

    Team {
        cache,
        deploy,
        lint,
        other,
        tabs,
    }
}

/// This computer's notes before a pull: "Build cache" as the team has it, "Deploy steps" with
/// another text, and a note of its own.
fn seed_local(machine: &Machine) {
    let mut store = local_store(machine);
    let widget = repo(WIDGET);

    store
        .remember(&widget, &draft("Build cache", "Lives in target/", NoteKind::Location))
        .expect("cache");
    store
        .remember(&widget, &draft("Deploy steps", "Run make release", NoteKind::Decision))
        .expect("deploy");
    store
        .remember(&widget, &draft("Mine", "Only here", NoteKind::Note))
        .expect("mine");
}

/// The change `object` lists for the team entry `id`.
fn change_of(object: &Value, id: i64) -> &Value {
    object["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .find(|change| change["teamId"] == id)
        .unwrap_or_else(|| panic!("no change for team id {id}: {object}"))
}

/// The team ids `object` lists a change for, sorted.
fn changed_ids(object: &Value) -> Vec<i64> {
    let mut ids: Vec<i64> = object["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .map(|change| change["teamId"].as_i64().expect("team id"))
        .collect();
    ids.sort_unstable();
    ids
}

fn sorted(mut ids: Vec<i64>) -> Vec<i64> {
    ids.sort_unstable();
    ids
}

#[test]
fn pull_dry_run_lists_what_this_computer_would_be_given_and_writes_nothing() {
    let Some(db) = database() else { return };
    let machine = machine();
    let team = seed_team(&db);
    seed_local(&machine);
    let before = snapshot(&db);

    let object = answer(&run(shared(&machine, &["pull", "--dry-run", "--json"], db.url())), 0);

    let keys: Vec<&str> = object.as_object().expect("object").keys().map(String::as_str).collect();
    assert_eq!(keys, ["dryRun", "nothing", "notes", "changes", "failed"]);
    assert_eq!((&object["dryRun"], &object["nothing"]), (&json!(true), &json!(null)));
    assert_eq!(
        object["notes"],
        json!({ "added": 3, "identical": 1, "localDiffers": 1, "failed": 0 })
    );
    assert_eq!(object["failed"], json!([]));
    assert_eq!(
        changed_ids(&object),
        sorted(vec![team.cache, team.deploy, team.lint, team.other, team.tabs])
    );

    let cache = change_of(&object, team.cache);
    let change_keys: Vec<&str> = cache.as_object().expect("change").keys().map(String::as_str).collect();
    assert_eq!(
        change_keys,
        [
            "teamId",
            "localId",
            "scope",
            "repo",
            "title",
            "action",
            "teamUpdatedAt",
            "localUpdatedAt",
            "failed"
        ]
    );
    assert_eq!(cache["action"], "identical");
    assert!(cache["localId"].as_i64().is_some_and(|id| id > 0), "{cache}");
    assert!(
        cache["localUpdatedAt"].as_str().is_some_and(|at| at.ends_with('Z')),
        "{cache}"
    );
    assert!(
        cache["teamUpdatedAt"].as_str().is_some_and(|at| at.ends_with('Z')),
        "{cache}"
    );
    assert_eq!(cache["scope"], "repo");
    assert_eq!(cache["repo"], WIDGET);
    assert_eq!(cache["failed"], false);

    let deploy = change_of(&object, team.deploy);
    assert_eq!(deploy["action"], "localDiffers");
    assert!(deploy["localId"].as_i64().is_some(), "{deploy}");

    for (id, scope, repo) in [
        (team.lint, "repo", json!(WIDGET)),
        (team.other, "repo", json!(OTHER)),
        (team.tabs, "global", json!(null)),
    ] {
        let change = change_of(&object, id);
        assert_eq!(change["action"], "add", "{change}");
        assert_eq!(change["localId"], json!(null), "{change}");
        assert_eq!(change["localUpdatedAt"], json!(null), "{change}");
        assert_eq!((&change["scope"], &change["repo"]), (&json!(scope), &repo), "{change}");
    }

    assert_eq!(local_titles(&machine), ["Build cache", "Deploy steps", "Mine"]);
    assert_eq!(snapshot(&db), before, "the team database is not changed");
    assert!(!machine.memory.join("pending-sync.json").exists());
}

#[test]
fn pull_takes_only_the_scopes_set_to_remote_local_and_what_repo_and_no_global_leave_in() {
    let Some(db) = database() else { return };
    let machine = machine();
    let team = seed_team(&db);

    let listed = |args: &[&str], routing: (&str, &str)| {
        let mut all = vec!["pull", "--dry-run", "--json"];
        all.extend_from_slice(args);
        let command = binary(&machine, &all, db.url(), routing, "remote+local");
        changed_ids(&answer(&run(command), 0))
    };

    assert_eq!(
        listed(&[], SHARED),
        sorted(vec![team.cache, team.deploy, team.lint, team.other, team.tabs])
    );
    assert_eq!(
        listed(&["--repo", "."], SHARED),
        sorted(vec![team.cache, team.deploy, team.lint, team.tabs]),
        "the current repository and the global notes"
    );
    assert_eq!(
        listed(&["--repo", "git:github.com/acme/other", "--no-global"], SHARED),
        vec![team.other]
    );
    assert_eq!(
        listed(&["--no-global"], SHARED),
        sorted(vec![team.cache, team.deploy, team.lint, team.other])
    );

    // A scope set to remote or local is never taken.
    assert_eq!(listed(&[], ("remote", "remote+local")), vec![team.tabs]);
    assert_eq!(listed(&[], ("local", "remote+local")), vec![team.tabs]);
    assert_eq!(
        listed(&[], ("remote+local", "remote")),
        sorted(vec![team.cache, team.deploy, team.lint, team.other])
    );
    assert_eq!(
        listed(&[], ("remote+local", "local")),
        sorted(vec![team.cache, team.deploy, team.lint, team.other])
    );
}

#[test]
fn pull_copies_only_the_ids_given_and_keeps_authors_provenance_and_relations() {
    let Some(db) = database() else { return };
    let machine = machine();
    let team = seed_team(&db);
    seed_local(&machine);
    let before = snapshot(&db);
    let input = format!("{}\n{}\n{}\n{}\n", team.lint, team.tabs, team.cache, team.deploy);

    let object = answer(
        &run_with_input(shared(&machine, &["pull", "--json", "--ids", "-"], db.url()), &input),
        0,
    );

    assert_eq!((&object["dryRun"], &object["nothing"]), (&json!(false), &json!(null)));
    assert_eq!(
        object["notes"],
        json!({ "added": 2, "identical": 1, "localDiffers": 1, "failed": 0 })
    );
    assert_eq!(object["failed"], json!([]));
    assert_eq!(
        changed_ids(&object),
        sorted(vec![team.lint, team.tabs, team.cache, team.deploy]),
        "the note nobody asked for is not listed"
    );

    // Only the notes asked for were copied, and the two this computer had were left as they are.
    assert_eq!(
        local_titles(&machine),
        ["Build cache", "Deploy steps", "Editor tabs", "Lint rules", "Mine"]
    );
    let deploy = local_note(&machine, &repo(WIDGET), "Deploy steps").expect("deploy");
    assert_eq!(deploy.body, "Run make release");
    assert_eq!(deploy.revision, 1);

    let lint = local_note(&machine, &repo(WIDGET), "Lint rules").expect("lint");
    assert_eq!(change_of(&object, team.lint)["localId"], json!(lint.id));
    assert_eq!(change_of(&object, team.lint)["action"], "add");
    assert_eq!(
        (lint.body.as_str(), lint.kind, lint.tags.clone()),
        (
            "Run clippy before pushing",
            NoteKind::Fix,
            vec!["lint".to_owned(), "ci".to_owned()]
        )
    );
    let mut entities = lint.entities.clone();
    entities.sort();
    assert_eq!(entities, ["CI", "Clippy"]);

    let added = lint.authors.added_by.expect("added by");
    let changed = lint.authors.updated_by.expect("updated by");
    assert_eq!(
        (added.name.as_str(), added.via.as_deref()),
        ("Ada Lovelace", Some("omp"))
    );
    assert_eq!(
        (changed.name.as_str(), changed.via.as_deref()),
        ("Grace Hopper", Some("omp"))
    );
    assert_eq!(lint.provenance.source.as_deref(), Some("omp"));
    assert_eq!(
        lint.provenance.root, None,
        "the teammate's checkout is not this computer's: the copy is a row of this checkout"
    );
    assert!(!lint.provenance.via_override);

    let tabs = local_note(&machine, &Target::Global, "Editor tabs").expect("tabs");
    assert_eq!(change_of(&object, team.tabs)["localId"], json!(tabs.id));
    assert_eq!(
        tabs.authors.added_by.map(|author| author.name),
        Some("Ada Lovelace".to_owned())
    );

    // The relation the team keeps for the note came along, attached to the copy.
    let mut local = local_store(&machine);
    let view = View::around(&repo(WIDGET));
    let around = local.neighbors(&view, "Clippy", 1, 10).expect("neighbors");
    assert_eq!(around.edges.len(), 1);
    let edge = &around.edges[0];
    assert_eq!(
        (
            edge.from.as_str(),
            edge.relation.as_str(),
            edge.to.as_str(),
            edge.note_id
        ),
        ("Clippy", "runs_on", "CI", Some(lint.id))
    );

    assert_eq!(snapshot(&db), before, "the team database is not changed");
    assert!(
        !machine.memory.join("pending-sync.json").exists(),
        "a pulled note is not waiting for the team database"
    );

    // Pulling the same ids again changes nothing.
    let ids_before = (lint.id, tabs.id);
    let again = answer(
        &run_with_input(shared(&machine, &["pull", "--json", "--ids", "-"], db.url()), &input),
        0,
    );
    assert_eq!(
        again["notes"],
        json!({ "added": 0, "identical": 3, "localDiffers": 1, "failed": 0 })
    );
    assert_eq!(
        local_titles(&machine),
        ["Build cache", "Deploy steps", "Editor tabs", "Lint rules", "Mine"]
    );
    let lint = local_note(&machine, &repo(WIDGET), "Lint rules").expect("lint");
    let tabs = local_note(&machine, &Target::Global, "Editor tabs").expect("tabs");
    assert_eq!((lint.id, tabs.id), ids_before);
    assert_eq!(lint.revision, 1, "an identical note is not written again");
    assert_eq!(snapshot(&db), before);
}

#[test]
fn a_note_pulled_from_the_team_can_be_saved_over_from_this_checkout() {
    let Some(db) = database() else { return };
    let machine = machine();
    let team = seed_team(&db);
    let input = format!("{}\n", team.lint);

    answer(
        &run_with_input(shared(&machine, &["pull", "--json", "--ids", "-"], db.url()), &input),
        0,
    );
    let pulled = local_note(&machine, &repo(WIDGET), "Lint rules").expect("pulled");
    assert_eq!(pulled.body, "Run clippy before pushing");

    // The same title saved from this checkout replaces the copy, as a save over any local note.
    let draft = json!({ "title": "Lint rules", "body": "Run clippy with -D warnings", "kind": "fix", "scope": "repo" });
    let mut command = shared(&machine, &["notes", "save", "--json"], db.url());
    command.env("HARNESS_MEMORY_REMOTE_LOCAL_SAVES", "local");
    let saved = answer(&run_with_input(command, &draft.to_string()), 0);
    assert_eq!(saved["title"], "Lint rules", "{saved}");

    let after = local_note(&machine, &repo(WIDGET), "Lint rules").expect("saved over");
    assert_eq!(after.id, pulled.id, "the same row was changed");
    assert_eq!(after.body, "Run clippy with -D warnings");
    assert_eq!(
        after.authors.added_by.map(|author| author.name),
        Some("Ada Lovelace".to_owned()),
        "the person who added the original stays"
    );
}

#[test]
fn pull_names_ids_by_argument_and_gives_each_copy_its_own_local_row() {
    let Some(db) = database() else { return };
    let machine = machine();
    let team = seed_team(&db);

    let ids = format!("{},{}", team.tabs, team.other);
    let object = answer(&run(shared(&machine, &["pull", "--json", "--ids", &ids], db.url())), 0);
    assert_eq!(object["notes"]["added"], 2);

    // The team's note stays, and the copy is a note of this computer with an id of its own.
    let tabs = local_note(&machine, &Target::Global, "Editor tabs").expect("copy");
    assert_ne!(tabs.id, team.tabs, "{tabs:?}");
    let other = local_note(&machine, &repo(OTHER), "Other repo note").expect("copy");
    assert_eq!(other.project.as_deref(), Some(repo_key(OTHER).key.as_str()));

    let mut team_store = db.open();
    let kept = team_store.peek(&View::every_repo(), &[team.tabs]).expect("team note");
    assert_eq!(kept.len(), 1);
}

#[test]
fn pull_refuses_hidden_unknown_and_out_of_scope_ids_and_copies_nothing_for_them() {
    let Some(db) = database() else { return };
    let machine = machine();
    let team = seed_team(&db);
    let before = snapshot(&db);

    let hidden = run(shared(
        &machine,
        &["notes", "hide", &team.tabs.to_string(), "--json"],
        db.url(),
    ));
    assert_eq!(hidden.status.code(), Some(0), "{}", stderr(&hidden));

    // A hidden note is never offered.
    let listed = answer(&run(shared(&machine, &["pull", "--dry-run", "--json"], db.url())), 0);
    assert!(!changed_ids(&listed).contains(&team.tabs), "{listed}");
    assert_eq!(listed["notes"]["added"], 4);

    // Not even when its id is given, and neither are ids that name no note or one out of scope.
    let missing = team.other + 1000;
    let ids = format!("{},{missing},{},{}", team.tabs, team.other, team.cache);
    let command = shared(&machine, &["pull", "--json", "--repo", ".", "--ids", &ids], db.url());
    let object = answer(&run(command), 1);

    assert_eq!(object["notes"]["added"], 1, "{object}");
    assert_eq!(object["notes"]["failed"], 3, "{object}");
    let failed = object["failed"].as_array().expect("failed");
    let failed_ids: Vec<i64> = failed
        .iter()
        .map(|failure| failure["teamId"].as_i64().expect("team id"))
        .collect();
    assert_eq!(sorted(failed_ids), sorted(vec![team.tabs, missing, team.other]));

    let message_of = |id: i64| {
        failed
            .iter()
            .find(|failure| failure["teamId"] == id)
            .and_then(|failure| failure["message"].as_str())
            .expect("message")
            .to_owned()
    };
    assert!(message_of(team.tabs).contains("hidden on this computer"), "{object}");
    assert!(message_of(missing).contains("not found"), "{object}");
    assert!(message_of(team.other).contains("--repo"), "{object}");

    assert_eq!(
        local_titles(&machine),
        ["Build cache"],
        "only the note that qualifies was copied"
    );
    assert_eq!(snapshot(&db), before);

    // A scope that is not remote+local does not take an id of its notes either.
    let ids = team.lint.to_string();
    let command = binary(
        &machine,
        &["pull", "--json", "--ids", &ids],
        db.url(),
        ("remote", "remote+local"),
        "remote+local",
    );
    let object = answer(&run(command), 1);
    assert_eq!(object["notes"]["failed"], 1, "{object}");
    assert!(
        object["failed"][0]["message"]
            .as_str()
            .is_some_and(|said| said.contains("repository notes are \"remote\"")),
        "{object}"
    );
    assert_eq!(local_titles(&machine), ["Build cache"]);
}

#[test]
fn pull_masks_secrets_as_a_local_save_does_and_a_repeated_pull_stays_harmless() {
    let Some(db) = database() else { return };
    let machine = machine();
    let mut store = db.open();

    let mut leaky = draft("Leaky note", "placeholder", NoteKind::Note);
    leaky.provenance = writer("Ada Lovelace");
    let team_id = store.remember(&Target::Global, &leaky).expect("leaky").id;

    // Written below the store, as an older server could have: the token is unmasked there.
    let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4).get(..36).expect("36 characters"));
    let body = format!("Deploy with GITHUB_TOKEN={token} set");
    let changed = db
        .admin()
        .execute("UPDATE notes SET body = $1 WHERE id = $2", &[&body, &team_id])
        .expect("raw write");
    assert_eq!(changed, 1);

    let ids = team_id.to_string();
    let object = answer(&run(shared(&machine, &["pull", "--json", "--ids", &ids], db.url())), 0);
    assert_eq!(object["notes"]["added"], 1, "{object}");

    let copy = local_note(&machine, &Target::Global, "Leaky note").expect("copy");
    assert!(!copy.body.contains(&token), "{}", copy.body);
    assert!(copy.body.contains("••••"), "{}", copy.body);

    let again = answer(&run(shared(&machine, &["pull", "--json", "--ids", &ids], db.url())), 0);
    assert_eq!(
        again["notes"],
        json!({ "added": 0, "identical": 1, "localDiffers": 0, "failed": 0 }),
        "the masked copy is the same note"
    );
    assert_eq!(local_titles(&machine), ["Leaky note"]);
}

#[test]
fn pull_reports_an_entry_this_computer_refuses_and_still_copies_the_others() {
    let Some(db) = database() else { return };
    let machine = machine();
    let mut store = db.open();

    let good = store
        .remember(&Target::Global, &draft("Good note", "Fine as it is", NoteKind::Note))
        .expect("good")
        .id;
    let bad = store
        .remember(&Target::Global, &draft("Too long soon", "Fine too", NoteKind::Note))
        .expect("bad")
        .id;

    // Below the store, a title past the limit every local save checks.
    let title = "t".repeat(130);
    db.admin()
        .execute("UPDATE notes SET title = $1 WHERE id = $2", &[&title, &bad])
        .expect("raw write");

    let ids = format!("{bad},{good}");
    let object = answer(&run(shared(&machine, &["pull", "--json", "--ids", &ids], db.url())), 1);

    assert_eq!(
        object["notes"],
        json!({ "added": 1, "identical": 0, "localDiffers": 0, "failed": 1 }),
        "{object}"
    );
    assert_eq!(change_of(&object, bad)["failed"], true);
    assert_eq!(change_of(&object, bad)["localId"], json!(null));
    assert_eq!(change_of(&object, good)["failed"], false);
    assert_eq!(object["failed"][0]["teamId"], bad);
    assert_eq!(local_titles(&machine), ["Good note"]);

    // The same for a script whose code this computer refuses.
    let mut scripts = db.open();
    let fine = scripts
        .save_script(&Target::Global, &script("fine-one", "printf fine"))
        .expect("fine")
        .script
        .summary
        .id;
    let hidden = scripts
        .save_script(&Target::Global, &script("zero-width", "printf zero"))
        .expect("zero width")
        .script
        .summary
        .id;
    let code = "printf zero\u{200b}";
    db.admin()
        .execute("UPDATE scripts SET body = $1 WHERE id = $2", &[&code, &hidden])
        .expect("raw write");

    let ids = format!("{hidden},{fine}");
    let command = shared(&machine, &["scripts", "pull", "--json", "--ids", &ids], db.url());
    let object = answer(&run(command), 1);

    assert_eq!(
        object["scripts"],
        json!({ "added": 1, "identical": 0, "localDiffers": 0, "failed": 1 }),
        "{object}"
    );
    assert_eq!(change_of(&object, hidden)["failed"], true);
    assert_eq!(local_script_names(&machine), ["fine-one"]);
}

#[test]
fn pull_lists_and_copies_across_pages_of_the_team_list() {
    let Some(db) = database() else { return };
    let machine = machine();
    let mut store = db.open();
    let mut ids: Vec<i64> = Vec::new();

    // More than two pages of the team list (200 notes each).
    for number in 0..430 {
        let title = format!("Team note {number:03}");
        let target = if number % 2 == 0 { repo(WIDGET) } else { Target::Global };
        let mut note = draft(&title, &format!("Body number {number}"), NoteKind::Note);
        note.entities = vec![format!("Topic {}", number % 7)];
        ids.push(store.remember(&target, &note).expect("team note").id);
    }

    let before = snapshot(&db);
    let listed = answer(&run(shared(&machine, &["pull", "--dry-run", "--json"], db.url())), 0);
    assert_eq!(listed["notes"]["added"], 430);
    assert_eq!(changed_ids(&listed), sorted(ids.clone()), "every note once");

    let input = ids.iter().map(i64::to_string).collect::<Vec<_>>().join("\n");
    let copied = answer(
        &run_with_input(shared(&machine, &["pull", "--json", "--ids", "-"], db.url()), &input),
        0,
    );
    assert_eq!(copied["notes"]["added"], 430);
    assert_eq!(local_titles(&machine).len(), 430);
    assert_eq!(snapshot(&db), before);

    let again = answer(&run(shared(&machine, &["pull", "--dry-run", "--json"], db.url())), 0);
    assert_eq!(again["notes"]["identical"], 430);
    assert_eq!(again["notes"]["added"], 0);
}

#[test]
fn pull_copies_nothing_by_default_and_refuses_what_it_cannot_read() {
    let machine = machine();
    // Nothing here is ever contacted: every case is refused before.
    let url = "postgres://alice:hunter2secret@127.0.0.1:1/memory?connect_timeout=1";

    for args in [
        &["pull", "--json"][..],
        &["scripts", "pull", "--json"],
        &["pull", "--dry-run"],
        &["pull", "--json", "--dry-run", "extra"],
        &["pull", "--json", "--dry-run", "--ids", "x"],
        &["pull", "--json", "--dry-run", "--ids", "0"],
        &["pull", "--json", "--dry-run", "--ids", ""],
        &["pull", "--json", "--dry-run", "--repo", " "],
        &["pull", "--json", "--dry-run", "--dry-run"],
        &["pull", "--json", "--dry-run", "--scope", "repo"],
        &["scripts", "pull", "--json", "--dry-run", "--ids", "1,x"],
        &["push", "--remote-local", "--scope", "repo"],
        &["scripts", "push", "--remote-local", "--scope", "global"],
        &["push", "--remote-local", "--remote-local"],
    ] {
        let output = run(shared(&machine, args, url));
        assert_eq!(output.status.code(), Some(2), "{args:?}: {}", stderr(&output));
        assert!(output.stdout.is_empty(), "{args:?}: {}", stdout(&output));
    }

    assert!(!machine.memory.exists(), "nothing was created");
}

#[test]
fn pull_says_why_there_is_nothing_to_take() {
    let machine = machine();
    let url = "postgres://alice:hunter2secret@127.0.0.1:1/memory?connect_timeout=1";

    for (routing, scripts, args, reason) in [
        (
            ("remote", "local"),
            "remote+local",
            &["pull", "--dry-run", "--json"][..],
            "pull only takes the scopes set to \"remote+local\"",
        ),
        (
            ("remote", "remote+local"),
            "remote+local",
            &["pull", "--dry-run", "--json", "--no-global"],
            "--no-global leaves the global notes out",
        ),
        (
            ("remote", "remote+local"),
            "remote+local",
            &["pull", "--dry-run", "--json", "--repo", "git:github.com/acme/widget"],
            "--repo names a repository, but repository notes are \"remote\"",
        ),
        (
            SHARED,
            "remote",
            &["scripts", "pull", "--dry-run", "--json"],
            "scripts are \"remote\", and pull only takes",
        ),
        (
            SHARED,
            "local",
            &["scripts", "pull", "--dry-run", "--json"],
            "scripts are \"local\", and pull only takes",
        ),
    ] {
        let output = run(binary(&machine, args, url, routing, scripts));
        let object = answer(&output, 0);

        assert_eq!(object["dryRun"], true, "{args:?}");
        assert!(
            object["nothing"].as_str().is_some_and(|said| said.contains(reason)),
            "{args:?}: {object}"
        );
        assert_eq!(object["changes"], json!([]), "{args:?}");
        assert_eq!(object["failed"], json!([]), "{args:?}");
    }

    // An empty list of ids on stdin copies nothing and says so.
    let command = shared(&machine, &["pull", "--json", "--ids", "-"], url);
    let object = answer(&run_with_input(command, "\n"), 0);
    assert_eq!(object["nothing"], "no ids were given");

    assert!(!machine.memory.exists(), "nothing was created");
}

#[test]
fn pull_fails_visibly_without_the_password_when_the_team_database_is_unreachable() {
    let machine = machine();
    let url = "postgres://alice:hunter2secret@127.0.0.1:1/memory?connect_timeout=1";

    for args in [
        &["pull", "--dry-run", "--json"][..],
        &["pull", "--json", "--ids", "5"],
        &["scripts", "pull", "--dry-run", "--json"],
        &["scripts", "pull", "--json", "--ids", "5"],
    ] {
        let output = run(shared(&machine, args, url));
        let object = answer(&output, 1);

        assert_eq!(object["error"], "failed", "{args:?}: {object}");
        let message = object["message"].as_str().expect("message");
        assert!(message.contains("cannot open the team database"), "{args:?}: {message}");
        assert!(!message.contains("hunter2secret"), "{args:?}: {message}");
        assert!(
            !stderr(&output).contains("hunter2secret"),
            "{args:?}: {}",
            stderr(&output)
        );
    }

    assert!(!machine.memory.join("memory.sqlite3").exists(), "nothing was copied");
}

#[test]
fn pull_without_a_team_database_exits_2() {
    let machine = machine();
    let mut command = Command::new(BINARY);
    command
        .args(["pull", "--dry-run", "--json"])
        .current_dir(&machine.repo)
        .env("HOME", &machine.home)
        .env("USERPROFILE", &machine.home)
        .env("HARNESS_MEMORY_DIR", &machine.memory)
        .env("XDG_CONFIG_HOME", &machine.config)
        .env("APPDATA", &machine.config);
    without_team_settings(&mut command);

    let output = run(command);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("no team database is set"),
        "{}",
        stderr(&output)
    );
}

fn script(name: &str, body: &str) -> ScriptDraft {
    ScriptDraft {
        name: name.to_owned(),
        description: format!("The {name} script"),
        language: Language::Sh,
        body: body.to_owned(),
        timeout_secs: 30,
        provenance: writer("Ada Lovelace"),
    }
}

/// The team scripts of [`seed_scripts`].
struct Scripts {
    prs: i64,
    lint: i64,
    shared: i64,
    differs: i64,
    other: i64,
}

/// Two scripts of the shared repository, a global one, one of another repository and one this
/// computer also has with another body.
fn seed_scripts(db: &Database) -> Scripts {
    let mut store = db.open();
    let widget = repo(WIDGET);

    let prs = store
        .save_script(&widget, &script("open-prs", "gh pr list"))
        .expect("prs")
        .script
        .summary
        .id;
    let lint = store
        .save_script(&Target::Global, &script("lint-all", "cargo clippy"))
        .expect("lint")
        .script
        .summary
        .id;
    let shared = store
        .save_script(&widget, &script("same-here", "printf same"))
        .expect("same")
        .script
        .summary
        .id;
    let differs = store
        .save_script(&widget, &script("differs-here", "printf team"))
        .expect("differs")
        .script
        .summary
        .id;
    let other = store
        .save_script(&repo(OTHER), &script("elsewhere", "printf other"))
        .expect("other")
        .script
        .summary
        .id;

    Scripts {
        prs,
        lint,
        shared,
        differs,
        other,
    }
}

fn seed_local_scripts(machine: &Machine) {
    let mut store = local_store(machine);
    let widget = repo(WIDGET);

    store
        .save_script(&widget, &script("same-here", "printf same"))
        .expect("same");
    store
        .save_script(&widget, &script("differs-here", "printf mine"))
        .expect("differs");
    store
        .save_script(&widget, &script("mine-only", "printf mine"))
        .expect("mine");
}

#[test]
fn scripts_pull_lists_copies_by_id_and_leaves_the_team_database_and_equal_scripts_alone() {
    let Some(db) = database() else { return };
    let machine = machine();
    let team = seed_scripts(&db);
    seed_local_scripts(&machine);
    let before = snapshot(&db);

    let listed = answer(
        &run(shared(&machine, &["scripts", "pull", "--dry-run", "--json"], db.url())),
        0,
    );
    let keys: Vec<&str> = listed.as_object().expect("object").keys().map(String::as_str).collect();
    assert_eq!(keys, ["dryRun", "nothing", "scripts", "changes", "failed"]);
    assert_eq!(
        listed["scripts"],
        json!({ "added": 3, "identical": 1, "localDiffers": 1, "failed": 0 })
    );
    assert_eq!(
        change_of(&listed, team.prs)["name"],
        "open-prs",
        "a script's change carries its name"
    );
    assert_eq!(change_of(&listed, team.shared)["action"], "identical");
    assert_eq!(change_of(&listed, team.differs)["action"], "localDiffers");
    assert_eq!(change_of(&listed, team.other)["repo"], OTHER);
    assert_eq!(change_of(&listed, team.lint)["scope"], "global");
    assert_eq!(local_script_names(&machine), ["differs-here", "mine-only", "same-here"]);
    assert_eq!(snapshot(&db), before);

    let ids = format!("{},{},{},{}", team.prs, team.shared, team.differs, team.lint);
    let copied = answer(
        &run(shared(
            &machine,
            &["scripts", "pull", "--json", "--ids", &ids],
            db.url(),
        )),
        0,
    );
    assert_eq!(
        copied["scripts"],
        json!({ "added": 2, "identical": 1, "localDiffers": 1, "failed": 0 })
    );
    assert_eq!(
        local_script_names(&machine),
        ["differs-here", "lint-all", "mine-only", "open-prs", "same-here"],
        "the script nobody asked for was not copied"
    );

    let mut local = local_store(&machine);
    let view = View::around(&repo(WIDGET));
    let prs = local
        .get_script(&view, &harness_memory::store::ScriptRef::Name("open-prs".to_owned()))
        .expect("read")
        .expect("copy");
    assert_eq!(change_of(&copied, team.prs)["localId"], json!(prs.summary.id));
    assert_eq!(prs.body, "gh pr list");
    assert_eq!(prs.summary.timeout_secs, 30);
    assert_eq!(prs.summary.description, "The open-prs script");
    assert_eq!(
        prs.summary.authors.added_by.map(|author| author.name),
        Some("Ada Lovelace".to_owned())
    );

    let differs = local
        .get_script(
            &view,
            &harness_memory::store::ScriptRef::Name("differs-here".to_owned()),
        )
        .expect("read")
        .expect("script");
    assert_eq!(differs.body, "printf mine", "a script that differs is never written");

    assert_eq!(snapshot(&db), before, "the team database is not changed");
    assert!(!machine.memory.join("pending-sync.json").exists());

    let again = answer(
        &run(shared(
            &machine,
            &["scripts", "pull", "--json", "--ids", &ids],
            db.url(),
        )),
        0,
    );
    assert_eq!(
        again["scripts"],
        json!({ "added": 0, "identical": 3, "localDiffers": 1, "failed": 0 })
    );
    assert_eq!(local_script_names(&machine).len(), 5);
}

#[test]
fn scripts_pull_never_carries_trust_and_refuses_hidden_scripts() {
    let Some(db) = database() else { return };
    let machine = machine();
    let team = seed_scripts(&db);

    let listing = |machine: &Machine| {
        let mut command = shared(machine, &["scripts", "list", "--json"], db.url());
        command.env("HARNESS_MEMORY_TRUST_TEAM_SCRIPTS", "1");
        answer(&run(command), 0)
    };
    let approval = |listed: &Value, name: &str, place: &str| -> Option<String> {
        listed["scripts"]
            .as_array()
            .expect("scripts")
            .iter()
            .find(|script| script["name"] == name && script["place"] == place)
            .map(|script| script["approval"].as_str().expect("approval").to_owned())
    };

    // The team runs its scripts without approval here, because they are the team's.
    assert_eq!(
        approval(&listing(&machine), "open-prs", "team").as_deref(),
        Some("team")
    );

    let ids = team.prs.to_string();
    let mut command = shared(&machine, &["scripts", "pull", "--json", "--ids", &ids], db.url());
    command.env("HARNESS_MEMORY_TRUST_TEAM_SCRIPTS", "1");
    assert_eq!(answer(&run(command), 0)["scripts"]["added"], 1);

    // The copy is a script of this computer: nothing trusts it, so it waits for approval.
    let listed = listing(&machine);
    assert_eq!(
        approval(&listed, "open-prs", "local").as_deref(),
        Some("needed"),
        "{listed}"
    );
    assert_eq!(approval(&listed, "open-prs", "team").as_deref(), Some("team"));

    // A hidden script is neither offered nor copied by id.
    let hide = run(shared(
        &machine,
        &["scripts", "hide", &team.lint.to_string(), "--json"],
        db.url(),
    ));
    assert_eq!(hide.status.code(), Some(0), "{}", stderr(&hide));

    let dry = answer(
        &run(shared(&machine, &["scripts", "pull", "--dry-run", "--json"], db.url())),
        0,
    );
    assert!(!changed_ids(&dry).contains(&team.lint), "{dry}");

    let ids = format!("{},{}", team.lint, team.other);
    let before = snapshot(&db);
    let object = answer(
        &run(shared(
            &machine,
            &["scripts", "pull", "--json", "--ids", &ids, "--repo", "."],
            db.url(),
        )),
        1,
    );
    assert_eq!(object["scripts"]["added"], 0, "{object}");
    assert_eq!(object["scripts"]["failed"], 2, "{object}");
    let messages: Vec<&str> = object["failed"]
        .as_array()
        .expect("failed")
        .iter()
        .map(|failure| failure["message"].as_str().expect("message"))
        .collect();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("hidden on this computer")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|message| message.contains("--repo")),
        "{messages:?}"
    );
    assert_eq!(local_script_names(&machine), ["open-prs"]);
    assert_eq!(snapshot(&db), before);
}

#[test]
fn push_remote_local_takes_the_remote_local_scopes_only_deletes_only_copied_rows_and_leaves_conflicts() {
    let Some(db) = database() else { return };
    let machine = machine();
    let local = push_support::local_memory();
    copy_into(&local.path, &machine.memory);

    let changed_scopes = |object: &Value| -> Vec<String> {
        let mut scopes: Vec<String> = object["changes"]
            .as_array()
            .expect("changes")
            .iter()
            .map(|change| change["scope"].as_str().expect("scope").to_owned())
            .collect();
        scopes.sort();
        scopes
    };
    let push = |routing: (&str, &str), extra: &[&str]| {
        let mut args = vec!["push", "--json", "--remote-local"];
        args.extend_from_slice(extra);
        let command = binary(&machine, &args, db.url(), routing, "remote+local");
        answer(&run(command), 0)
    };

    // Only the scopes set to remote+local, whatever the other one is set to.
    for routing in [("remote+local", "remote"), ("remote+local", "local")] {
        let object = push(routing, &["--dry-run"]);
        assert_eq!(changed_scopes(&object), ["repo", "repo"], "{routing:?}: {object}");
        assert_eq!(object["nothing"], json!(null));
    }

    let object = push(("remote", "remote+local"), &["--dry-run"]);
    assert_eq!(changed_scopes(&object), ["global"]);

    let object = push(SHARED, &["--dry-run"]);
    assert_eq!(changed_scopes(&object), ["global", "repo", "repo"]);
    let object = push(SHARED, &["--dry-run", "--no-global"]);
    assert_eq!(changed_scopes(&object), ["repo", "repo"]);

    // Plain push still leaves a remote+local scope out.
    let plain = answer(
        &run(binary(
            &machine,
            &["push", "--json", "--dry-run"],
            db.url(),
            ("remote+local", "remote"),
            "remote+local",
        )),
        0,
    );
    assert_eq!(changed_scopes(&plain), ["global"], "{plain}");

    // Pushing writes the repository notes to the team and nothing of the global scope.
    let pushed = push(("remote+local", "remote"), &[]);
    assert_eq!(pushed["notes"]["added"], 2);
    let mut admin = db.admin();
    let team_notes: i64 = admin
        .query_one("SELECT count(*) FROM notes", &[])
        .expect("count")
        .get(0);
    assert_eq!(team_notes, 2);
    assert_eq!(
        local_titles(&machine).len(),
        3,
        "nothing is deleted without --delete-copied"
    );

    // A newer local text is a conflict: it stays unticked and its local copy stays too.
    local.edit("Deploy steps", "Run make release", now() + 3600);
    copy_into(&local.path, &machine.memory);

    let kept = push(("remote+local", "remote"), &["--overwrite", "none", "--delete-copied"]);
    let deploy = kept["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .find(|change| change["title"] == "Deploy steps")
        .expect("deploy");
    assert_eq!(
        (&deploy["action"], &deploy["conflict"], &deploy["deleted"]),
        (&json!("keep"), &json!(true), &json!(false))
    );
    assert_eq!(kept["deleted"], 1, "only the copied row went: {kept}");
    assert_eq!(
        local_titles(&machine),
        ["Deploy steps", "Editor tabs"],
        "the conflict and the global note, which this run did not take, are still here"
    );
    let team_notes: i64 = admin
        .query_one("SELECT count(*) FROM notes", &[])
        .expect("count")
        .get(0);
    assert_eq!(team_notes, 2, "the global note was not pushed");
}

#[test]
fn push_remote_local_says_why_when_no_scope_is_remote_local() {
    let machine = machine();
    let url = "postgres://alice:hunter2secret@127.0.0.1:1/memory?connect_timeout=1";

    for (routing, args, reason) in [
        (
            ("remote", "local"),
            &["push", "--json", "--remote-local"][..],
            "--remote-local takes the scopes set to \"remote+local\", and none is taken",
        ),
        (
            ("local", "local"),
            &["push", "--json", "--remote-local", "--dry-run"],
            "repository notes are not \"remote+local\" (repository_notes)",
        ),
        (
            ("remote", "remote+local"),
            &["push", "--json", "--remote-local", "--no-global"],
            "--no-global leaves the global ones out",
        ),
        (
            ("remote", "remote+local"),
            &[
                "push",
                "--json",
                "--remote-local",
                "--repo",
                "git:github.com/acme/widget",
            ],
            "--repo names a repository, but repository notes are not \"remote+local\"",
        ),
    ] {
        let object = answer(&run(binary(&machine, args, url, routing, "local")), 0);

        assert!(
            object["nothing"].as_str().is_some_and(|said| said.contains(reason)),
            "{args:?}: {object}"
        );
        assert_eq!(object["changes"], json!([]), "{args:?}");
    }

    for (scripts, reason) in [
        (
            "remote",
            "--remote-local takes scripts set to \"remote+local\", and scripts are \"remote\"",
        ),
        ("local", "scripts stay on this computer"),
    ] {
        let command = binary(
            &machine,
            &["scripts", "push", "--json", "--remote-local"],
            url,
            SHARED,
            scripts,
        );
        let object = answer(&run(command), 0);

        assert!(
            object["nothing"].as_str().is_some_and(|said| said.contains(reason)),
            "{scripts}: {object}"
        );
    }

    // Without --json it stays a refusal, as push's other refusals are.
    let output = run(binary(
        &machine,
        &["push", "--remote-local"],
        url,
        ("remote", "local"),
        "local",
    ));
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("nothing to push"), "{}", stderr(&output));
}

#[test]
fn scripts_push_remote_local_takes_remote_local_scripts_only_deletes_copied_rows_and_leaves_conflicts() {
    let Some(db) = database() else { return };
    let machine = machine();
    let mut local = local_store(&machine);
    let widget = repo(WIDGET);

    for (name, body) in [("open-prs", "gh pr list"), ("deploy", "make deploy")] {
        local.save_script(&widget, &script(name, body)).expect("local script");
    }

    let push = |scripts: &str, extra: &[&str]| {
        let mut args = vec!["scripts", "push", "--json"];
        args.extend_from_slice(extra);
        answer(&run(binary(&machine, &args, db.url(), SHARED, scripts)), 0)
    };

    // Plain remote and local scripts are not taken by --remote-local.
    for scripts in ["remote", "local"] {
        let object = push(scripts, &["--remote-local", "--dry-run"]);
        assert!(object["nothing"].is_string(), "{scripts}: {object}");
        assert_eq!(object["changes"], json!([]));
    }

    // Without --scope or --remote-local a remote+local scope is left out, as before.
    let plain = push("remote+local", &["--dry-run"]);
    assert!(
        plain["nothing"]
            .as_str()
            .is_some_and(|said| said.contains("only --scope")),
        "{plain}"
    );

    let dry = push("remote+local", &["--remote-local", "--dry-run"]);
    assert_eq!(dry["scripts"]["added"], 2, "{dry}");

    let pushed = push("remote+local", &["--remote-local"]);
    assert_eq!(pushed["scripts"]["added"], 2);

    // A newer local version is a conflict: kept, and its local copy stays.
    {
        let conn = rusqlite::Connection::open(machine.memory.join("memory.sqlite3")).expect("raw local");
        conn.execute(
            "UPDATE scripts SET body = 'make release', updated_at = ?1 WHERE name = 'deploy'",
            [now() + 3600],
        )
        .expect("edit");
    }

    let kept = push(
        "remote+local",
        &["--remote-local", "--overwrite", "none", "--delete-copied"],
    );
    let deploy = kept["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .find(|change| change["name"] == "deploy")
        .expect("deploy");
    assert_eq!(
        (&deploy["action"], &deploy["conflict"], &deploy["deleted"]),
        (&json!("keep"), &json!(true), &json!(false))
    );
    assert_eq!(kept["deleted"], 1, "{kept}");
    assert_eq!(local_script_names(&machine), ["deploy"]);
}

/// The local id `object` lists a change for, by the note's title (`key` `title`) or the script's
/// name (`key` `name`).
fn local_id_of(object: &Value, key: &str, label: &str) -> i64 {
    object["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .find(|change| change[key] == label)
        .unwrap_or_else(|| panic!("no change for {label}: {object}"))["localId"]
        .as_i64()
        .expect("local id")
}

/// The `(label, action, deleted)` of every change of `object`, sorted by label.
fn outcomes(object: &Value, key: &str) -> Vec<(String, String, bool)> {
    let mut changes: Vec<(String, String, bool)> = object["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .map(|change| {
            (
                change[key].as_str().expect("label").to_owned(),
                change["action"].as_str().expect("action").to_owned(),
                change["deleted"].as_bool().expect("deleted"),
            )
        })
        .collect();
    changes.sort();
    changes
}

/// The entry of `only` for the change of `object` titled (`key` `title`) or named (`key` `name`)
/// `label`: its id, title or name and update time exactly as the preview printed them.
fn reviewed(object: &Value, key: &str, label: &str) -> Value {
    let change = object["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .find(|change| change[key] == label)
        .unwrap_or_else(|| panic!("no change for {label}: {object}"));

    json!({ "id": change["localId"], "name": label, "updatedAt": change["localUpdatedAt"] })
}

/// `{"only": ..., "overwrite": ...}`, as a review hands it to `--selection -`.
fn selection(only: &[Value], overwrite: &[i64]) -> String {
    json!({ "only": only, "overwrite": overwrite }).to_string()
}

/// An entry of `only` for the row `id` that the local file no longer has.
fn gone(id: i64) -> Value {
    json!({ "id": id, "name": "Long gone", "updatedAt": "2020-01-01T00:00:00Z" })
}

/// Runs `sql` on the local file of `machine`, below the store.
fn raw(machine: &Machine, sql: &str) {
    let conn = rusqlite::Connection::open(machine.memory.join("memory.sqlite3")).expect("raw local");
    conn.execute_batch(sql).expect("raw sql");
}

fn team_body(db: &Database, table: &str, column: &str, name: &str) -> Option<String> {
    db.admin()
        .query_opt(&format!("SELECT body FROM {table} WHERE {column} = $1"), &[&name])
        .expect("team row")
        .map(|row| row.get(0))
}

#[test]
fn push_selection_copies_and_deletes_only_the_notes_that_were_reviewed() {
    let Some(db) = database() else { return };
    let machine = machine();
    let local = push_support::local_memory();
    copy_into(&local.path, &machine.memory);

    // The review: the preview lists three notes and the user keeps two of them.
    let preview = answer(
        &run(shared(
            &machine,
            &["push", "--json", "--dry-run", "--remote-local"],
            db.url(),
        )),
        0,
    );
    let cache = reviewed(&preview, "title", "Build cache");
    let tabs = reviewed(&preview, "title", "Editor tabs");

    // An agent saves a note before the user applies the review.
    local_store(&machine)
        .remember(&repo(WIDGET), &draft("Saved later", "Not reviewed", NoteKind::Note))
        .expect("later note");

    let apply = shared(
        &machine,
        &[
            "push",
            "--json",
            "--remote-local",
            "--delete-copied",
            "--selection",
            "-",
        ],
        db.url(),
    );
    let pushed = answer(&run_with_input(apply, &selection(&[cache, tabs], &[])), 0);

    assert_eq!(
        outcomes(&pushed, "title"),
        [
            ("Build cache".to_owned(), "add".to_owned(), true),
            ("Editor tabs".to_owned(), "add".to_owned(), true)
        ],
        "{pushed}"
    );
    assert_eq!(pushed["deleted"], 2);
    assert_eq!(pushed["failed"], json!([]));
    assert_eq!(
        local_titles(&machine),
        ["Deploy steps", "Saved later"],
        "what was not named, reviewed or not, stays on this computer"
    );

    let mut admin = db.admin();
    let titles: Vec<String> = admin
        .query("SELECT title FROM notes ORDER BY title", &[])
        .expect("team notes")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(titles, ["Build cache", "Editor tabs"], "nothing else reached the team");
}

#[test]
fn push_selection_overwrites_only_the_conflicts_it_names_and_reports_an_id_that_is_gone() {
    let Some(db) = database() else { return };
    let machine = machine();
    let local = push_support::local_memory();
    copy_into(&local.path, &machine.memory);

    let first = answer(
        &run(shared(&machine, &["push", "--json", "--remote-local"], db.url())),
        0,
    );
    assert_eq!(first["notes"]["added"], 3);

    // Two newer local texts: both are conflicts.
    local.edit("Deploy steps", "Run make release", now() + 3600);
    local.edit("Build cache", "Lives in build/", now() + 3600);
    copy_into(&local.path, &machine.memory);

    let preview = answer(
        &run(shared(
            &machine,
            &["push", "--json", "--dry-run", "--remote-local"],
            db.url(),
        )),
        0,
    );
    let cache = reviewed(&preview, "title", "Build cache");
    let deploy = reviewed(&preview, "title", "Deploy steps");
    let tabs = reviewed(&preview, "title", "Editor tabs");
    let cache_id = local_id_of(&preview, "title", "Build cache");
    let deploy_id = local_id_of(&preview, "title", "Deploy steps");
    let missing = 987_654;

    let push = |only: &[Value], overwrite: &[i64], code: i32| {
        let command = shared(
            &machine,
            &[
                "push",
                "--json",
                "--remote-local",
                "--delete-copied",
                "--selection",
                "-",
            ],
            db.url(),
        );
        answer(&run_with_input(command, &selection(only, overwrite)), code)
    };

    // The conflict that is not in `overwrite` keeps the team's text and stays here, an id that no
    // longer exists is reported (exit 1) and the others go on.
    let pushed = push(&[cache.clone(), deploy, tabs, gone(missing)], &[deploy_id], 1);

    assert_eq!(
        outcomes(&pushed, "title"),
        [
            ("Build cache".to_owned(), "keep".to_owned(), false),
            ("Deploy steps".to_owned(), "overwrite".to_owned(), true),
            ("Editor tabs".to_owned(), "identical".to_owned(), true)
        ],
        "{pushed}"
    );
    assert_eq!(pushed["deleted"], 2);

    let failed = pushed["failed"].as_array().expect("failed");
    assert_eq!(failed.len(), 1, "{pushed}");
    assert_eq!(failed[0]["localId"], missing);
    assert!(
        failed[0]["message"]
            .as_str()
            .is_some_and(|said| said.contains("no longer")),
        "{pushed}"
    );

    assert_eq!(
        team_body(&db, "notes", "title", "Deploy steps").as_deref(),
        Some("Run make release")
    );
    assert_eq!(
        team_body(&db, "notes", "title", "Build cache").as_deref(),
        Some("Lives in target/")
    );
    assert_eq!(local_titles(&machine), ["Build cache"]);

    // An id in `overwrite` that `only` leaves out is not touched.
    let untouched = push(&[], &[cache_id], 0);
    assert_eq!(untouched["changes"], json!([]));
    assert_eq!(untouched["deleted"], 0);
    assert_eq!(local_titles(&machine), ["Build cache"]);
    assert_eq!(
        team_body(&db, "notes", "title", "Build cache").as_deref(),
        Some("Lives in target/")
    );

    let overwritten = push(&[cache], &[cache_id], 0);
    assert_eq!(
        outcomes(&overwritten, "title"),
        [("Build cache".to_owned(), "overwrite".to_owned(), true)]
    );
    assert_eq!(
        team_body(&db, "notes", "title", "Build cache").as_deref(),
        Some("Lives in build/")
    );
    assert!(local_titles(&machine).is_empty());
}

#[test]
fn scripts_push_selection_copies_and_deletes_only_the_scripts_that_were_reviewed() {
    let Some(db) = database() else { return };
    let machine = machine();
    let mut local = local_store(&machine);
    let widget = repo(WIDGET);

    for (name, body) in [("open-prs", "gh pr list"), ("deploy", "make deploy")] {
        local.save_script(&widget, &script(name, body)).expect("local script");
    }

    let first = answer(
        &run(shared(
            &machine,
            &["scripts", "push", "--json", "--remote-local"],
            db.url(),
        )),
        0,
    );
    assert_eq!(first["scripts"]["added"], 2);

    // A newer local version is a conflict, and an agent saves a script after the review.
    {
        let conn = rusqlite::Connection::open(machine.memory.join("memory.sqlite3")).expect("raw local");
        conn.execute(
            "UPDATE scripts SET body = 'make release', updated_at = ?1 WHERE name = 'deploy'",
            [now() + 3600],
        )
        .expect("edit");
    }

    let preview = answer(
        &run(shared(
            &machine,
            &["scripts", "push", "--json", "--dry-run", "--remote-local"],
            db.url(),
        )),
        0,
    );
    let prs = reviewed(&preview, "name", "open-prs");
    let deploy = reviewed(&preview, "name", "deploy");
    let missing = 987_654;

    local
        .save_script(&widget, &script("saved-later", "printf later"))
        .expect("later script");

    let push = |only: &[Value], overwrite: &[i64], code: i32| {
        let command = shared(
            &machine,
            &[
                "scripts",
                "push",
                "--json",
                "--remote-local",
                "--delete-copied",
                "--selection",
                "-",
            ],
            db.url(),
        );
        answer(&run_with_input(command, &selection(only, overwrite)), code)
    };

    let kept = push(&[prs, deploy.clone(), gone(missing)], &[], 1);
    assert_eq!(
        outcomes(&kept, "name"),
        [
            ("deploy".to_owned(), "keep".to_owned(), false),
            ("open-prs".to_owned(), "identical".to_owned(), true)
        ],
        "{kept}"
    );
    assert_eq!(kept["deleted"], 1);
    assert_eq!(kept["failed"].as_array().expect("failed").len(), 1, "{kept}");
    assert_eq!(kept["failed"][0]["localId"], missing);
    assert_eq!(local_script_names(&machine), ["deploy", "saved-later"]);
    assert_eq!(
        team_body(&db, "scripts", "name", "deploy").as_deref(),
        Some("make deploy")
    );

    let deploy_id = local_id_of(&preview, "name", "deploy");
    let overwritten = push(&[deploy], &[deploy_id], 0);
    assert_eq!(
        outcomes(&overwritten, "name"),
        [("deploy".to_owned(), "overwrite".to_owned(), true)]
    );
    assert_eq!(
        team_body(&db, "scripts", "name", "deploy").as_deref(),
        Some("make release")
    );
    assert_eq!(
        local_script_names(&machine),
        ["saved-later"],
        "the script saved after the review is neither copied nor deleted"
    );
    assert_eq!(team_body(&db, "scripts", "name", "saved-later"), None);
}

/// The messages of the `failed` list of `pushed` by local id, sorted by id.
fn failed_by_id(pushed: &Value) -> Vec<(i64, String)> {
    let mut failed: Vec<(i64, String)> = pushed["failed"]
        .as_array()
        .expect("failed")
        .iter()
        .map(|failure| {
            (
                failure["localId"].as_i64().expect("local id"),
                failure["message"].as_str().expect("message").to_owned(),
            )
        })
        .collect();
    failed.sort();
    failed
}

#[test]
fn push_selection_leaves_a_note_that_changed_since_the_review_neither_pushed_nor_deleted() {
    let Some(db) = database() else { return };
    let machine = machine();
    let local = push_support::local_memory();
    copy_into(&local.path, &machine.memory);

    // An agent's note that will be deleted after the review, so its id is the highest one.
    let widget = repo(WIDGET);
    local_store(&machine)
        .remember(&widget, &draft("Doomed", "Reviewed once", NoteKind::Note))
        .expect("doomed note");

    let preview = answer(
        &run(shared(
            &machine,
            &["push", "--json", "--dry-run", "--remote-local"],
            db.url(),
        )),
        0,
    );
    let entries: Vec<Value> = ["Build cache", "Deploy steps", "Doomed", "Editor tabs"]
        .iter()
        .map(|title| reviewed(&preview, "title", title))
        .collect();
    let doomed = local_id_of(&preview, "title", "Doomed");

    // After the review: one title changes, one body changes (its update time moves on), and the
    // reviewed note with the highest id is deleted while a new note takes over its id.
    raw(
        &machine,
        "UPDATE notes SET title = 'Renamed cache' WHERE title = 'Build cache'",
    );
    raw(
        &machine,
        "UPDATE notes SET body = 'Run make something else', updated_at = updated_at + 60 WHERE title = 'Deploy steps'",
    );
    {
        let mut store = local_store(&machine);
        store
            .forget(
                &View::around(&widget),
                &widget,
                &harness_memory::store::Forget::Note(doomed),
            )
            .expect("delete doomed");
        let reused = store
            .remember(&widget, &draft("Private diary", "Not for the team", NoteKind::Note))
            .expect("reusing note");
        assert_eq!(reused.id, doomed, "the new note took over the deleted note's id");
    }

    let apply = shared(
        &machine,
        &[
            "push",
            "--json",
            "--remote-local",
            "--delete-copied",
            "--selection",
            "-",
        ],
        db.url(),
    );
    let pushed = answer(&run_with_input(apply, &selection(&entries, &[])), 1);

    // Only the note that is still what was reviewed goes, and goes from here.
    assert_eq!(
        outcomes(&pushed, "title"),
        [("Editor tabs".to_owned(), "add".to_owned(), true)],
        "{pushed}"
    );
    assert_eq!(pushed["deleted"], 1, "{pushed}");

    let failed = failed_by_id(&pushed);
    let ids: Vec<i64> = failed.iter().map(|(id, _)| *id).collect();
    let mut changed = vec![
        local_id_of(&preview, "title", "Build cache"),
        local_id_of(&preview, "title", "Deploy steps"),
        doomed,
    ];
    changed.sort_unstable();
    assert_eq!(ids, changed, "{pushed}");
    assert!(
        failed
            .iter()
            .all(|(_, message)| message.contains("changed since it was reviewed")),
        "{pushed}"
    );

    assert_eq!(
        local_titles(&machine),
        ["Deploy steps", "Private diary", "Renamed cache"],
        "what changed since the review stays on this computer"
    );

    let mut admin = db.admin();
    let titles: Vec<String> = admin
        .query("SELECT title FROM notes ORDER BY title", &[])
        .expect("team notes")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(titles, ["Editor tabs"], "nothing that changed reached the team");
}

#[test]
fn push_selection_takes_the_update_time_only_as_the_preview_printed_it() {
    let Some(db) = database() else { return };
    let machine = machine();
    let local = push_support::local_memory();
    copy_into(&local.path, &machine.memory);

    let preview = answer(
        &run(shared(
            &machine,
            &["push", "--json", "--dry-run", "--remote-local"],
            db.url(),
        )),
        0,
    );
    let mut cache = reviewed(&preview, "title", "Build cache");
    let printed = cache["updatedAt"].as_str().expect("time").to_owned();
    cache["updatedAt"] = json!(printed.replace('Z', "+00:00"));

    let apply = shared(
        &machine,
        &[
            "push",
            "--json",
            "--remote-local",
            "--delete-copied",
            "--selection",
            "-",
        ],
        db.url(),
    );
    let pushed = answer(&run_with_input(apply, &selection(&[cache], &[])), 1);

    assert_eq!(pushed["changes"], json!([]), "{pushed}");
    assert_eq!(pushed["deleted"], 0, "{pushed}");
    assert_eq!(failed_by_id(&pushed).len(), 1, "{pushed}");
    assert_eq!(local_titles(&machine).len(), 3);
}

#[test]
fn scripts_push_selection_leaves_a_script_that_changed_since_the_review_neither_pushed_nor_deleted() {
    let Some(db) = database() else { return };
    let machine = machine();
    let mut local = local_store(&machine);
    let widget = repo(WIDGET);

    for (name, body) in [
        ("open-prs", "gh pr list"),
        ("deploy", "make deploy"),
        ("lint", "make lint"),
        ("doomed", "echo reviewed once"),
    ] {
        local.save_script(&widget, &script(name, body)).expect("local script");
    }

    let preview = answer(
        &run(shared(
            &machine,
            &["scripts", "push", "--json", "--dry-run", "--remote-local"],
            db.url(),
        )),
        0,
    );
    let entries: Vec<Value> = ["open-prs", "deploy", "doomed", "lint"]
        .iter()
        .map(|name| reviewed(&preview, "name", name))
        .collect();
    let doomed = local_id_of(&preview, "name", "doomed");

    // After the review: one name changes, one body changes (its update time moves on), and the
    // reviewed script with the highest id is deleted while a new script takes over its id.
    raw(
        &machine,
        "UPDATE scripts SET name = 'renamed-prs' WHERE name = 'open-prs'",
    );
    raw(
        &machine,
        "UPDATE scripts SET body = 'make something else', updated_at = updated_at + 60 WHERE name = 'deploy'",
    );
    raw(&machine, "DELETE FROM scripts WHERE name = 'doomed'");
    let reused = local
        .save_script(&widget, &script("private-tool", "echo not for the team"))
        .expect("reusing script")
        .script
        .summary
        .id;
    assert_eq!(reused, doomed, "the new script took over the deleted script's id");

    let apply = shared(
        &machine,
        &[
            "scripts",
            "push",
            "--json",
            "--remote-local",
            "--delete-copied",
            "--selection",
            "-",
        ],
        db.url(),
    );
    let pushed = answer(&run_with_input(apply, &selection(&entries, &[])), 1);

    assert_eq!(
        outcomes(&pushed, "name"),
        [("lint".to_owned(), "add".to_owned(), true)],
        "{pushed}"
    );
    assert_eq!(pushed["deleted"], 1, "{pushed}");

    let failed = failed_by_id(&pushed);
    let mut changed = vec![
        local_id_of(&preview, "name", "open-prs"),
        local_id_of(&preview, "name", "deploy"),
        doomed,
    ];
    changed.sort_unstable();
    assert_eq!(
        failed.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        changed,
        "{pushed}"
    );
    assert!(
        failed
            .iter()
            .all(|(_, message)| message.contains("changed since it was reviewed")),
        "{pushed}"
    );

    assert_eq!(
        local_script_names(&machine),
        ["deploy", "private-tool", "renamed-prs"],
        "what changed since the review stays on this computer"
    );

    for name in ["deploy", "private-tool", "renamed-prs", "open-prs", "doomed"] {
        assert_eq!(
            team_body(&db, "scripts", "name", name),
            None,
            "{name} did not reach the team"
        );
    }
    assert_eq!(team_body(&db, "scripts", "name", "lint").as_deref(), Some("make lint"));
}

#[test]
fn features_lists_the_copy_commands() {
    let machine = machine();
    let mut command = Command::new(BINARY);
    command
        .arg("features")
        .env("HOME", &machine.home)
        .env("USERPROFILE", &machine.home);
    without_team_settings(&mut command);

    let output = run(command);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let listed = stdout(&output);
    let features: Vec<&str> = listed.lines().collect();

    for feature in ["push-remote-local", "pull-json", "scripts-pull-json", "push-selection"] {
        assert!(features.contains(&feature), "{feature} is missing: {listed}");
    }
}
