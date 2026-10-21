//! The script commands of the real executable: `scripts list --json` lists what `script_list`
//! does, `scripts delete <id>` and `scripts delete --all --yes` delete as `script_delete` does
//! (`--all` without `--yes` exits 2), `trust <id>=<sha256>...` approves reviewed versions from anywhere, `trust <id>...` and
//! `trust --all` approve scripts as they are only from a terminal (exit 1 for a missing id, 2 for
//! bad usage or no terminal, the reason on the last line of stderr), and `check` reports
//! `trust_team_scripts`.

// Most of these tests run only on Unix, so on Windows their helpers and imports are unused.
#![cfg_attr(not(unix), allow(unused))]

mod common;
mod libsql_support;
mod mongo_support;
mod postgres_support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::mcp::repo_with_remote;
use common::repo;
use common::stdio::{BINARY, Client, command, without_team_settings};
use harness_memory::store::{Language, Provenance, ScriptDraft, ScriptStore, SqliteStore, Target};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tempfile::TempDir;

/// A data folder (`HARNESS_MEMORY_DIR`) and a working directory outside any repository.
fn folders() -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new().expect("temp");
    let data = dir.path().join("data");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&data).expect("mkdir");
    std::fs::create_dir_all(&work).expect("mkdir");
    (dir, data, work)
}

/// Saves script `name` into `target` straight into the store, as a teammate's machine would; its id.
fn save_elsewhere(data: &Path, target: &Target, name: &str) -> i64 {
    let mut store = SqliteStore::open(&data.join("memory.sqlite3")).expect("store");
    let draft = ScriptDraft {
        name: name.to_owned(),
        description: format!("The {name} script"),
        language: Language::Sh,
        body: format!("echo {name}"),
        timeout_secs: 10,
        provenance: Provenance::default(),
    };

    store.save_script(target, &draft).expect("saved").script.summary.id
}

/// `harness-memory <args>` on the local database in `data`, started in `cwd`.
fn cli(data: &Path, cwd: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(BINARY);
    command.args(args).current_dir(cwd).env("HARNESS_MEMORY_DIR", data);
    without_team_settings(&mut command);
    command.output().expect("the executable runs")
}

/// [`cli`] with a terminal as standard input, as a person at the terminal runs it.
#[cfg(unix)]
fn cli_in_terminal(data: &Path, cwd: &Path, args: &[&str]) -> Output {
    let terminal = nix::pty::openpty(None, None).expect("a pseudo-terminal");
    let mut command = Command::new(BINARY);
    command
        .args(args)
        .current_dir(cwd)
        .env("HARNESS_MEMORY_DIR", data)
        .stdin(Stdio::from(terminal.slave));
    without_team_settings(&mut command);

    let output = command.output().expect("the executable runs");
    drop(terminal.master);
    output
}

/// `<id>=<sha256>` for every script `scripts list --json <extra>` shows, in its order.
fn reviewed(data: &Path, cwd: &Path, extra: &[&str]) -> Vec<String> {
    listed(data, cwd, extra)
        .iter()
        .map(|script| format!("{}={}", script["id"], script["sha256"].as_str().expect("sha256")))
        .collect()
}

/// `trust <options> <picks>`.
fn trust_args(options: &[&str], picks: Vec<String>) -> Vec<String> {
    let mut args = vec!["trust".to_owned()];
    args.extend(options.iter().map(|option| (*option).to_owned()));
    args.extend(picks);
    args
}

fn strs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The whole reply of `scripts list --json <extra>`.
fn listing(data: &Path, cwd: &Path, extra: &[&str]) -> Value {
    let args: Vec<&str> = ["scripts", "list", "--json"].iter().chain(extra).copied().collect();
    let output = cli(data, cwd, &args);
    assert!(output.status.success(), "{}", stderr(&output));

    let parsed: Value = serde_json::from_str(&stdout(&output)).expect("JSON");
    let keys: Vec<&String> = parsed.as_object().expect("object").keys().collect();
    assert_eq!(keys, ["total", "groups", "scopes", "review", "places", "scripts"]);
    parsed
}

/// The `scripts` array of `scripts list --json <extra>`.
fn listed(data: &Path, cwd: &Path, extra: &[&str]) -> Vec<Value> {
    listing(data, cwd, extra)["scripts"]
        .as_array()
        .expect("scripts")
        .clone()
}

fn names(scripts: &[Value]) -> Vec<&str> {
    scripts
        .iter()
        .map(|script| script["name"].as_str().expect("name"))
        .collect()
}

fn approvals(scripts: &[Value]) -> Vec<(String, String)> {
    scripts
        .iter()
        .map(|script| {
            let field = |key: &str| script[key].as_str().expect(key).to_owned();
            (field("name"), field("approval"))
        })
        .collect()
}

#[test]
fn scripts_list_json_is_what_script_list_returns() {
    let (_dir, data, work) = folders();
    save_elsewhere(&data, &Target::Global, "beta");

    let mut client = Client::start(command(&data, &work));
    client.call(
        "script_save",
        json!({ "scope": "global", "name": "alpha", "description": "Mine", "language": "sh", "body": "echo a" }),
    );
    let mcp = client.request("tools/call", json!({ "name": "script_list", "arguments": {} }));
    assert!(client.finish().success());

    let scripts = listed(&data, &work, &[]);
    assert_eq!(
        Value::Array(scripts.clone()),
        mcp["result"]["structuredContent"]["scripts"]
    );
    assert_eq!(
        approvals(&scripts),
        [
            ("alpha".to_owned(), "local".to_owned()),
            ("beta".to_owned(), "needed".to_owned())
        ]
    );

    let untrusted = listed(&data, &work, &["--untrusted"]);
    assert_eq!(approvals(&untrusted), [("beta".to_owned(), "needed".to_owned())]);
}

#[test]
fn a_project_shows_its_repository_scripts() {
    let (dir, data, work) = folders();
    let root = repo_with_remote(dir.path(), "widget", "https://github.com/acme/widget.git");
    save_elsewhere(&data, &repo("github.com/acme/widget"), "local-only");
    save_elsewhere(&data, &Target::Global, "shared");

    let outside = listed(&data, &work, &[]);
    assert_eq!(approvals(&outside), [("shared".to_owned(), "needed".to_owned())]);

    let project = root.to_string_lossy().into_owned();
    let inside = listed(&data, &work, &["--project", &project]);
    assert_eq!(inside[0]["name"], "local-only");
    assert_eq!(inside[0]["repo"], "github.com/acme/widget");
    assert_eq!(inside.len(), 2);

    let untrusted = reviewed(&data, &work, &["--project", &project, "--untrusted"]);
    let args = trust_args(&["--project", &project], untrusted);
    let output = cli(&data, &work, &strs(&args));
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        listed(&data, &work, &["--project", &project, "--untrusted"]),
        Vec::<Value>::new()
    );
}

#[test]
fn trust_approves_the_ids_given_and_fails_for_a_missing_one() {
    let (_dir, data, work) = folders();
    let first = save_elsewhere(&data, &Target::Global, "first");
    let second = save_elsewhere(&data, &Target::Global, "second");
    let shas = reviewed(&data, &work, &[]);
    let missing = format!("999={}", "a".repeat(64));

    let output = cli(&data, &work, &["trust", &shas[0]]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("trusted script {first} first\n"));
    assert_eq!(
        approvals(&listed(&data, &work, &[])),
        [
            ("first".to_owned(), "approved".to_owned()),
            ("second".to_owned(), "needed".to_owned())
        ]
    );

    let output = cli(&data, &work, &["trust", &shas[0], &missing, &shas[1]]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout(&output),
        format!("script {first} first is already trusted\ntrusted script {second} second\n")
    );
    assert_eq!(stderr(&output), "harness-memory: script 999 not found\n");

    // An approved script runs for the agent with no desktop to ask.
    let mut client = Client::start(command(&data, &work));
    let ran = client.request(
        "tools/call",
        json!({ "name": "script_run", "arguments": { "name": "second" } }),
    );
    assert_eq!(ran["result"]["structuredContent"]["stdout"], "second\n", "{ran}");
    assert!(client.finish().success());
}

#[test]
#[cfg(unix)]
fn trust_all_approves_every_script_that_needs_it() {
    let (_dir, data, work) = folders();
    let first = save_elsewhere(&data, &Target::Global, "first");
    let second = save_elsewhere(&data, &Target::Global, "second");

    let output = cli_in_terminal(&data, &work, &["trust", "--all"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("trusted script {first} first\ntrusted script {second} second\n")
    );

    let again = cli_in_terminal(&data, &work, &["trust", "--all"]);
    assert!(again.status.success());
    assert_eq!(stdout(&again), "no scripts need approval\n");
    assert_eq!(listed(&data, &work, &["--untrusted"]), Vec::<Value>::new());
}

#[test]
fn bad_usage_exits_2() {
    let (_dir, data, work) = folders();

    for args in [
        &["trust"][..],
        &["trust", "--all", "3"],
        &["trust", "x"],
        &["trust", "3", "--project"],
        &["scripts"],
        &["scripts", "list"],
        &["scripts", "list", "--json", "--all"],
        &["scripts", "list", "--json", "--scope", "team"],
        &["scripts", "list", "--json", "--scope", "Global"],
        &["scripts", "list", "--json", "--review", "pending"],
        &["scripts", "list", "--json", "--review", "Needed"],
        &["scripts", "list", "--json", "--review"],
        &["scripts", "list", "--json", "--group", "repo", "--scope", "repo"],
        &[
            "scripts",
            "list",
            "--json",
            "--group",
            "needsReview",
            "--review",
            "needed",
        ],
        &["scripts", "list", "--json", "--untrusted", "--scope", "global"],
        &["scripts", "list", "--json", "--untrusted", "--review", "needed"],
        &["scripts", "delete"],
        &["scripts", "delete", "x"],
        &["scripts", "delete", "3", "--json"],
        &["scripts", "delete", "3", "--yes"],
        &["scripts", "delete", "--yes"],
        &["scripts", "delete", "--all", "--yes", "3"],
    ] {
        let output = cli(&data, &work, args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            stderr(&output).starts_with("usage: harness-memory"),
            "{}",
            stderr(&output)
        );
    }

    let relative = cli(&data, &work, &["scripts", "list", "--json", "--project", "widget"]);
    assert_eq!(relative.status.code(), Some(1));
    assert_eq!(
        stderr(&relative),
        "harness-memory: --project must be an absolute path\n"
    );
}

#[test]
fn trust_with_the_reviewed_sha_refuses_a_script_changed_since() {
    let (_dir, data, work) = folders();
    let first = save_elsewhere(&data, &Target::Global, "first");
    let second = save_elsewhere(&data, &Target::Global, "second");

    let shas: Vec<String> = listed(&data, &work, &[])
        .iter()
        .map(|script| script["sha256"].as_str().expect("sha256").to_owned())
        .collect();

    // A teammate changes `first` after the user reviewed it.
    let mut store = SqliteStore::open(&data.join("memory.sqlite3")).expect("store");
    let changed = ScriptDraft {
        name: "first".to_owned(),
        description: String::new(),
        language: Language::Sh,
        body: "echo changed".to_owned(),
        timeout_secs: 10,
        provenance: Provenance::default(),
    };
    store.save_script(&Target::Global, &changed).expect("changed");
    drop(store);

    let output = cli(
        &data,
        &work,
        &[
            "trust",
            &format!("{first}={}", shas[0]),
            &format!("{second}={}", shas[1].to_uppercase()),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), format!("trusted script {second} second\n"));
    assert_eq!(
        stderr(&output),
        format!("harness-memory: script {first} first changed since you reviewed it\n")
    );
    assert_eq!(
        approvals(&listed(&data, &work, &[])),
        [
            ("first".to_owned(), "needed".to_owned()),
            ("second".to_owned(), "approved".to_owned())
        ]
    );
}

#[test]
fn a_body_with_a_hidden_character_is_never_trusted() {
    let (_dir, data, work) = folders();
    let hidden = save_elsewhere(&data, &Target::Global, "hidden");
    let plain = save_elsewhere(&data, &Target::Global, "plain");

    // Written into the database by hand: the server refuses such a body.
    let conn = rusqlite::Connection::open(data.join("memory.sqlite3")).expect("raw");
    conn.execute(
        "UPDATE scripts SET body = ?1 WHERE id = ?2",
        rusqlite::params!["x=1 # note\rprint('CR-HIDDEN')", hidden],
    )
    .expect("update");
    drop(conn);

    let args = trust_args(&[], reviewed(&data, &work, &[]));
    let output = cli(&data, &work, &strs(&args));
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), format!("trusted script {plain} plain\n"));
    assert!(
        stderr(&output).starts_with(&format!(
            "harness-memory: script {hidden} hidden was not trusted, it can never run: the script body holds an \
             invisible or control character (U+000D on line 1)"
        )),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        approvals(&listed(&data, &work, &["--untrusted"])),
        [("hidden".to_owned(), "needed".to_owned())]
    );
}

#[test]
fn check_reports_team_scripts_trusted_automatically_only_with_a_team_database() {
    let (_dir, data, work) = folders();

    let plain = cli(&data, &work, &["check"]);
    assert!(plain.status.success(), "{}", stderr(&plain));
    assert!(!stdout(&plain).contains("team scripts"), "{}", stdout(&plain));

    // Without a team database there are no team scripts to trust.
    let mut command = Command::new(BINARY);
    command
        .arg("check")
        .current_dir(&work)
        .env("HARNESS_MEMORY_DIR", &data)
        .env("HARNESS_MEMORY_TRUST_TEAM_SCRIPTS", "1");
    without_team_settings(&mut command);
    let trusting = command.output().expect("runs");

    assert!(trusting.status.success(), "{}", stderr(&trusting));
    let lines: Vec<String> = stdout(&trusting).lines().map(str::to_owned).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[1], "scripts: this computer");
}

#[test]
fn plain_ids_and_all_need_a_terminal() {
    let (_dir, data, work) = folders();
    let id = save_elsewhere(&data, &Target::Global, "first");

    // An agent's shell tool has no terminal: it cannot approve what no one reviewed.
    for args in [&["trust", &id.to_string()][..], &["trust", "--all"]] {
        let output = cli(&data, &work, args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(stdout(&output), "");
        let last = stderr(&output).lines().last().unwrap_or_default().to_owned();
        assert!(
            last.starts_with("harness-memory: run this in your own terminal"),
            "{}",
            stderr(&output)
        );
    }

    let reviewed_and_not = trust_args(&[], vec![reviewed(&data, &work, &[]).remove(0), id.to_string()]);
    assert_eq!(cli(&data, &work, &strs(&reviewed_and_not)).status.code(), Some(2));
    assert_eq!(
        approvals(&listed(&data, &work, &[])),
        [("first".to_owned(), "needed".to_owned())]
    );

    #[cfg(unix)]
    {
        let output = cli_in_terminal(&data, &work, &["trust", &id.to_string()]);
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(stdout(&output), format!("trusted script {id} first\n"));
    }
}

#[test]
fn a_failed_trust_ends_stderr_with_why() {
    let (_dir, data, work) = folders();
    let odd = save_elsewhere(&data, &Target::Global, "odd");
    let fine = save_elsewhere(&data, &Target::Global, "fine");
    let picks = trust_args(&[], reviewed(&data, &work, &[]));

    // A teammate's row this server cannot run: reading it notes it on stderr.
    let conn = rusqlite::Connection::open(data.join("memory.sqlite3")).expect("raw");
    conn.execute_batch(&format!(
        "PRAGMA ignore_check_constraints = ON; UPDATE scripts SET language = 'ruby' WHERE id = {odd};"
    ))
    .expect("update");
    drop(conn);

    let missing = format!("999={}", "b".repeat(64));
    let mut args = picks;
    args.push(missing);
    let output = cli(&data, &work, &strs(&args));

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), format!("trusted script {fine} fine\n"));
    assert_eq!(
        stderr(&output).lines().collect::<Vec<_>>(),
        [
            format!("harness-memory: left out script {odd}: its language is not one this server runs"),
            format!("harness-memory: script {odd} not found"),
            "harness-memory: script 999 not found".to_owned(),
            "harness-memory: 2 scripts were not trusted".to_owned(),
        ]
    );
}

#[test]
fn a_python_body_declaring_another_encoding_is_never_trusted() {
    let (_dir, data, work) = folders();
    let id = save_elsewhere(&data, &Target::Global, "prs");

    // The auditor's body: UTF-7 makes Python run a line no reviewer sees.
    let body = "# -*- coding: utf-7 -*-\nprint(\"listing PRs\")\n# note +AAo-print(\"HIDDEN-RAN\")\n";
    let conn = rusqlite::Connection::open(data.join("memory.sqlite3")).expect("raw");
    conn.execute(
        "UPDATE scripts SET body = ?1, language = 'python' WHERE id = ?2",
        rusqlite::params![body, id],
    )
    .expect("update");
    drop(conn);

    let output = cli(&data, &work, &strs(&trust_args(&[], reviewed(&data, &work, &[]))));
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        format!(
            "harness-memory: script {id} prs was not trusted, it can never run: the script body declares the \
             encoding utf-7: Python would run other code than the text shown, so only utf-8 may be declared\n"
        )
    );
    assert_eq!(
        approvals(&listed(&data, &work, &[])),
        [("prs".to_owned(), "needed".to_owned())]
    );
}

#[test]
fn a_renamed_script_is_trusted_again_not_already_trusted() {
    let (_dir, data, work) = folders();
    let id = save_elsewhere(&data, &Target::Global, "first");
    let output = cli(&data, &work, &strs(&trust_args(&[], reviewed(&data, &work, &[]))));
    assert!(output.status.success(), "{}", stderr(&output));

    let conn = rusqlite::Connection::open(data.join("memory.sqlite3")).expect("raw");
    conn.execute("UPDATE scripts SET name = 'renamed' WHERE id = ?1", [id])
        .expect("rename");
    drop(conn);

    assert_eq!(
        approvals(&listed(&data, &work, &[])),
        [("renamed".to_owned(), "needed".to_owned())]
    );
    let again = cli(&data, &work, &strs(&trust_args(&[], reviewed(&data, &work, &[]))));
    assert_eq!(stdout(&again), format!("trusted script {id} renamed\n"));
    assert_eq!(
        approvals(&listed(&data, &work, &[])),
        [("renamed".to_owned(), "approved".to_owned())]
    );
}

#[test]
fn scripts_list_pages_groups_and_sorts() {
    let (dir, data, work) = folders();
    let root = repo_with_remote(dir.path(), "widget", "https://github.com/acme/widget.git");
    let project = root.to_string_lossy().into_owned();
    let at = |extra: &[&str]| -> Vec<String> {
        let mut args = vec!["--project".to_owned(), project.clone()];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        args
    };

    for name in ["delta", "alpha", "echo"] {
        save_elsewhere(&data, &Target::Global, name);
    }

    for name in ["bravo", "charlie"] {
        save_elsewhere(&data, &repo("github.com/acme/widget"), name);
    }

    let shas = reviewed(&data, &work, &strs(&at(&[])));
    let args = trust_args(&strs(&at(&[])), shas[..2].to_vec());
    assert!(
        cli(&data, &work, &strs(&args)).status.success(),
        "alpha and bravo trusted"
    );

    let all = listing(&data, &work, &strs(&at(&[])));
    assert_eq!(all["total"], 5);
    assert_eq!(all["groups"], json!({ "needsReview": 3, "repo": 1, "global": 1 }));
    let scripts = all["scripts"].as_array().expect("scripts");
    assert_eq!(names(scripts), ["alpha", "bravo", "charlie", "delta", "echo"]);

    let pending = listing(&data, &work, &strs(&at(&["--group", "needsReview"])));
    assert_eq!(
        names(pending["scripts"].as_array().expect("scripts")),
        ["charlie", "delta", "echo"]
    );
    assert_eq!(pending["total"], 3, "the total follows the group");
    assert_eq!(pending["groups"], all["groups"], "the counts do not");
    assert_eq!(
        names(&listed(&data, &work, &strs(&at(&["--group", "repo"])))),
        ["bravo"],
        "a repository script that needs approval is only in needsReview"
    );

    let page = listing(&data, &work, &strs(&at(&["--limit", "2", "--offset", "1"])));
    assert_eq!(
        names(page["scripts"].as_array().expect("scripts")),
        ["bravo", "charlie"]
    );
    assert_eq!(page["total"], 5);
    assert_eq!(
        listed(&data, &work, &strs(&at(&["--offset", "5"]))),
        Vec::<Value>::new()
    );
    assert_eq!(
        names(&listed(&data, &work, &strs(&at(&["--order", "desc", "--limit", "2"])))),
        ["echo", "delta"]
    );

    let matching = listing(&data, &work, &strs(&at(&["--query", "HA"])));
    assert_eq!(
        names(matching["scripts"].as_array().expect("scripts")),
        ["alpha", "charlie"]
    );
    assert_eq!(matching["total"], 2);
    assert_eq!(matching["groups"], json!({ "needsReview": 1, "repo": 0, "global": 1 }));

    let conn = rusqlite::Connection::open(data.join("memory.sqlite3")).expect("raw");
    conn.execute_batch(
        "UPDATE scripts SET updated_at = CASE name WHEN 'charlie' THEN 500 WHEN 'alpha' THEN 400 \
         WHEN 'echo' THEN 300 WHEN 'bravo' THEN 200 ELSE 100 END",
    )
    .expect("set the times");

    assert_eq!(
        names(&listed(&data, &work, &strs(&at(&["--sort", "updated"])))),
        ["charlie", "alpha", "echo", "bravo", "delta"],
        "newest first"
    );
    assert_eq!(
        names(&listed(
            &data,
            &work,
            &strs(&at(&["--sort", "updated", "--order", "asc"]))
        )),
        ["delta", "bravo", "echo", "alpha", "charlie"]
    );

    for bad in [
        &["--limit", "0"][..],
        &["--limit", "201"],
        &["--group", "trusted"],
        &["--untrusted", "--group", "repo"],
        &["--sort", "size"],
    ] {
        let args: Vec<&str> = ["scripts", "list", "--json"].iter().chain(bad).copied().collect();
        assert_eq!(cli(&data, &work, &args).status.code(), Some(2), "{bad:?}");
    }

    let long = "x".repeat(257);
    let output = cli(&data, &work, &["scripts", "list", "--json", "--query", &long]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).ends_with("harness-memory: query is longer than 256 characters\n"));
}

#[test]
fn scripts_list_filters_by_scope_and_review_together() {
    let (dir, data, work) = folders();
    let root = repo_with_remote(dir.path(), "widget", "https://github.com/acme/widget.git");
    let project = root.to_string_lossy().into_owned();
    let at = |extra: &[&str]| -> Value {
        let mut args = vec!["--project", project.as_str()];
        args.extend(extra);
        listing(&data, &work, &args)
    };

    for name in ["delta", "alpha", "echo"] {
        save_elsewhere(&data, &Target::Global, name);
    }

    for name in ["bravo", "charlie"] {
        save_elsewhere(&data, &repo("github.com/acme/widget"), name);
    }

    let shas = reviewed(&data, &work, &["--project", &project]);
    let args = trust_args(&["--project", &project], shas[..2].to_vec());
    assert!(
        cli(&data, &work, &strs(&args)).status.success(),
        "alpha (global) and bravo (repository) trusted"
    );

    let all = at(&[]);
    assert_eq!(all["total"], 5);
    assert_eq!(all["groups"], json!({ "needsReview": 3, "repo": 1, "global": 1 }));
    assert_eq!(all["scopes"], json!({ "repo": 2, "global": 3 }));
    assert_eq!(all["review"], json!({ "needed": 3, "trusted": 2 }));

    let global = at(&["--scope", "global"]);
    assert_eq!(
        names(global["scripts"].as_array().expect("scripts")),
        ["alpha", "delta", "echo"]
    );
    assert_eq!(global["total"], 3, "the total follows the scope");
    assert_eq!(global["scopes"], all["scopes"], "the scope counts ignore the scope");
    assert_eq!(
        global["review"],
        json!({ "needed": 2, "trusted": 1 }),
        "the review counts follow the scope"
    );
    assert_eq!(global["groups"], all["groups"], "the groups stay as they were");

    let needed = at(&["--review", "needed"]);
    assert_eq!(
        names(needed["scripts"].as_array().expect("scripts")),
        ["charlie", "delta", "echo"]
    );
    assert_eq!(needed["total"], 3);
    assert_eq!(
        needed["scopes"],
        json!({ "repo": 1, "global": 2 }),
        "the scope counts follow the review"
    );
    assert_eq!(needed["review"], all["review"], "the review counts ignore the review");

    let trusted_repo = at(&["--scope", "repo", "--review", "trusted"]);
    assert_eq!(names(trusted_repo["scripts"].as_array().expect("scripts")), ["bravo"]);
    assert_eq!(trusted_repo["total"], 1);
    assert_eq!(trusted_repo["scopes"], json!({ "repo": 1, "global": 1 }));
    assert_eq!(trusted_repo["review"], json!({ "needed": 1, "trusted": 1 }));

    let queried = at(&["--scope", "global", "--review", "needed", "--query", "LTA"]);
    assert_eq!(names(queried["scripts"].as_array().expect("scripts")), ["delta"]);
    assert_eq!(queried["total"], 1);
    assert_eq!(queried["scopes"], json!({ "repo": 0, "global": 1 }));
    assert_eq!(queried["review"], json!({ "needed": 1, "trusted": 0 }));
    assert_eq!(queried["groups"], json!({ "needsReview": 1, "repo": 0, "global": 0 }));

    let paged = at(&[
        "--review", "needed", "--sort", "name", "--order", "desc", "--offset", "1", "--limit", "1",
    ]);
    assert_eq!(names(paged["scripts"].as_array().expect("scripts")), ["delta"]);
    assert_eq!(paged["total"], 3);

    let grouped = at(&["--group", "repo"]);
    assert_eq!(names(grouped["scripts"].as_array().expect("scripts")), ["bravo"]);
    assert_eq!(grouped["total"], 1, "--group works as before");
    assert_eq!(grouped["scopes"], all["scopes"]);
    assert_eq!(grouped["review"], all["review"]);

    for bad in [
        &["--group", "global", "--scope", "global"][..],
        &["--group", "needsReview", "--review", "needed"],
        &["--untrusted", "--review", "needed"],
        &["--scope", "everywhere"],
        &["--review", "approved"],
    ] {
        let args: Vec<&str> = ["scripts", "list", "--json"].iter().chain(bad).copied().collect();
        let output = cli(&data, &work, &args);
        assert_eq!(output.status.code(), Some(2), "{bad:?}");
        assert!(
            stderr(&output).starts_with("usage: harness-memory"),
            "{}",
            stderr(&output)
        );
    }
}

#[test]
fn scripts_list_filters_by_place() {
    let (dir, data, work) = folders();
    let root = repo_with_remote(dir.path(), "widget", "https://github.com/acme/widget.git");
    let project = root.to_string_lossy().into_owned();
    let at = |extra: &[&str]| -> Value {
        let mut args = vec!["--project", project.as_str()];
        args.extend(extra);
        listing(&data, &work, &args)
    };

    for name in ["alpha", "delta"] {
        save_elsewhere(&data, &Target::Global, name);
    }

    save_elsewhere(&data, &repo("github.com/acme/widget"), "bravo");

    // Without a team database every script is kept on this computer.
    let all = at(&[]);
    assert_eq!(all["places"], json!({ "local": 3, "team": 0 }));

    let here = at(&["--place", "local", "--sort", "name"]);
    assert_eq!(
        names(here["scripts"].as_array().expect("scripts")),
        ["alpha", "bravo", "delta"]
    );
    assert_eq!(here["total"], 3);
    assert_eq!(here["places"], all["places"]);
    assert_eq!((&here["groups"], &here["scopes"]), (&all["groups"], &all["scopes"]));

    let paged = at(&["--place", "local", "--sort", "name", "--offset", "1", "--limit", "1"]);
    assert_eq!(names(paged["scripts"].as_array().expect("scripts")), ["bravo"]);
    assert_eq!(paged["total"], 3);

    let team = at(&["--place", "team"]);
    assert_eq!((&team["total"], &team["scripts"]), (&json!(0), &json!([])));
    assert_eq!(team["places"], all["places"], "the place counts ignore the place");
    assert_eq!(team["groups"], json!({ "needsReview": 0, "repo": 0, "global": 0 }));
    assert_eq!(team["scopes"], json!({ "repo": 0, "global": 0 }));
    assert_eq!(team["review"], json!({ "needed": 0, "trusted": 0 }));

    let global = at(&["--scope", "global", "--place", "local", "--query", "LTA"]);
    assert_eq!(names(global["scripts"].as_array().expect("scripts")), ["delta"]);
    assert_eq!(
        global["places"],
        json!({ "local": 1, "team": 0 }),
        "the place counts follow the scope and the query"
    );

    let grouped = at(&["--group", "repo", "--place", "local"]);
    assert_eq!(grouped["total"], 0, "nothing is trusted yet");
    assert_eq!(
        grouped["places"],
        json!({ "local": 0, "team": 0 }),
        "the place counts follow the group"
    );

    for bad in [
        &["--place", "remote"][..],
        &["--place", "Team"],
        &["--place"],
        &["--place", "local", "--place", "team"],
    ] {
        let args: Vec<&str> = ["scripts", "list", "--json"].iter().chain(bad).copied().collect();
        let output = cli(&data, &work, &args);
        assert_eq!(output.status.code(), Some(2), "{bad:?}");
        assert!(
            stderr(&output).starts_with("usage: harness-memory"),
            "{}",
            stderr(&output)
        );
    }
}

#[test]
fn scripts_delete_removes_one_or_every_listed_script() {
    let (dir, data, work) = folders();
    let widget = repo_with_remote(dir.path(), "widget", "https://github.com/acme/widget.git");
    let gadget = repo_with_remote(dir.path(), "gadget", "https://github.com/acme/gadget.git");
    let first = save_elsewhere(&data, &Target::Global, "first");
    save_elsewhere(&data, &Target::Global, "second");
    save_elsewhere(&data, &repo("github.com/acme/widget"), "local-only");
    save_elsewhere(&data, &repo("github.com/acme/gadget"), "theirs");

    let widget = widget.to_string_lossy().into_owned();
    let gadget = gadget.to_string_lossy().into_owned();

    let unconfirmed = cli(&data, &work, &["scripts", "delete", "--all", "--project", &widget]);
    assert_eq!(unconfirmed.status.code(), Some(2));
    assert!(
        stderr(&unconfirmed).trim_end().ends_with("add --yes to delete them"),
        "{}",
        stderr(&unconfirmed)
    );
    assert_eq!(listing(&data, &work, &["--project", &widget])["total"], 3);

    let id = first.to_string();
    let one = cli(&data, &work, &["scripts", "delete", &id, "--project", &widget]);
    assert!(one.status.success(), "{}", stderr(&one));
    assert_eq!(stdout(&one), format!("deleted script {first} first\n"));

    let again = cli(&data, &work, &["scripts", "delete", &id]);
    assert_eq!(again.status.code(), Some(1));
    assert_eq!(
        stderr(&again).lines().last(),
        Some(format!("harness-memory: script {first} not found").as_str())
    );

    let all = cli(
        &data,
        &work,
        &["scripts", "delete", "--all", "--yes", "--json", "--project", &widget],
    );
    assert!(all.status.success(), "{}", stderr(&all));
    let deleted: Value = serde_json::from_str(&stdout(&all)).expect("JSON");
    assert_eq!(deleted, json!({ "deleted": 2, "team": 0 }));
    assert_eq!(listing(&data, &work, &["--project", &widget])["total"], 0);
    assert_eq!(names(&listed(&data, &work, &["--project", &gadget])), ["theirs"]);

    // The deleted scripts waited for approval, so saving their code again trusts nothing.
    let mut client = Client::start(command(&data, &work));
    let saved = client.request(
        "tools/call",
        json!({ "name": "script_save", "arguments": { "scope": "global", "name": "copy", "description": "Mine", "language": "sh", "body": "echo second" } }),
    );
    assert!(client.finish().success());
    assert_eq!(
        saved["result"]["structuredContent"]["script"]["approval"], "needed",
        "{saved}"
    );

    let text = cli(
        &data,
        &work,
        &["scripts", "delete", "--all", "--yes", "--project", &gadget],
    );
    assert!(text.status.success(), "{}", stderr(&text));
    assert_eq!(stdout(&text), "deleted 2 scripts\n");
    assert_eq!(listing(&data, &work, &["--project", &gadget])["total"], 0);
}

#[test]
fn scripts_delete_all_handles_many_scripts() {
    let (_dir, data, work) = folders();
    let mut store = SqliteStore::open(&data.join("memory.sqlite3")).expect("store");

    for index in 0..1200 {
        let draft = ScriptDraft {
            name: format!("script-{index}"),
            description: String::new(),
            language: Language::Sh,
            body: format!("echo {index}"),
            timeout_secs: 10,
            provenance: Provenance::default(),
        };
        store.save_script(&Target::Global, &draft).expect("saved");
    }
    drop(store);

    let output = cli(&data, &work, &["scripts", "delete", "--all", "--yes", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let deleted: Value = serde_json::from_str(&stdout(&output)).expect("JSON");
    assert_eq!(deleted, json!({ "deleted": 1200, "team": 0 }));
    assert_eq!(listing(&data, &work, &[])["total"], 0);
}

#[test]
fn scripts_list_sorts_by_every_column_and_pages_in_that_order() {
    let (dir, data, work) = folders();
    let root = repo_with_remote(dir.path(), "widget", "https://github.com/acme/widget.git");
    let project = root.to_string_lossy().into_owned();

    for name in ["delta", "alpha", "echo"] {
        save_elsewhere(&data, &Target::Global, name);
    }

    for name in ["bravo", "charlie"] {
        save_elsewhere(&data, &repo("github.com/acme/widget"), name);
    }

    let conn = rusqlite::Connection::open(data.join("memory.sqlite3")).expect("raw");
    conn.execute_batch(
        "UPDATE scripts SET
           language = CASE name WHEN 'alpha' THEN 'python' WHEN 'charlie' THEN 'sh' WHEN 'echo' THEN 'node'
                      ELSE 'bash' END,
           run_count = CASE name WHEN 'alpha' THEN 5 WHEN 'echo' THEN 9 WHEN 'bravo' THEN 0 ELSE 2 END,
           last_run_at = CASE name WHEN 'alpha' THEN 300 WHEN 'echo' THEN 200 WHEN 'charlie' THEN 100 END",
    )
    .expect("raw update");
    drop(conn);

    let sorted = |sort: &str, order: &str, extra: &[&str]| -> Vec<String> {
        let mut args = vec!["--project", &project, "--sort", sort, "--order", order];
        args.extend(extra);
        names(&listed(&data, &work, &args))
            .into_iter()
            .map(str::to_owned)
            .collect()
    };

    for (sort, ascending) in [
        ("language", ["delta", "bravo", "echo", "alpha", "charlie"]),
        ("scope", ["delta", "alpha", "echo", "bravo", "charlie"]),
        ("place", ["delta", "alpha", "echo", "bravo", "charlie"]),
        ("runs", ["bravo", "delta", "charlie", "alpha", "echo"]),
    ] {
        assert_eq!(sorted(sort, "asc", &[]), ascending, "{sort}");

        let mut descending = ascending.to_vec();
        descending.reverse();
        assert_eq!(sorted(sort, "desc", &[]), descending, "{sort} desc");
    }

    assert_eq!(
        sorted("lastRun", "asc", &[]),
        ["charlie", "echo", "alpha", "delta", "bravo"],
        "scripts never run come last"
    );
    assert_eq!(
        sorted("lastRun", "desc", &[]),
        ["alpha", "echo", "charlie", "bravo", "delta"],
        "also when the order turns round"
    );
    assert_eq!(
        sorted("lastRun", "desc", &["--offset", "2", "--limit", "2"]),
        ["charlie", "bravo"]
    );
    assert_eq!(sorted("runs", "desc", &["--limit", "1"]), ["echo"]);

    let unknown = cli(&data, &work, &["scripts", "list", "--json", "--sort", "size"]);
    assert_eq!(unknown.status.code(), Some(2));
}

#[test]
fn scripts_get_shows_one_script_with_its_body() {
    let (_dir, data, work) = folders();
    let id = save_elsewhere(&data, &Target::Global, "beta");

    let output = cli(&data, &work, &["scripts", "get", &id.to_string(), "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let shown: Value = serde_json::from_str(&stdout(&output)).expect("JSON");
    let script = &shown["script"];
    assert_eq!(
        (&script["id"], &script["body"], &script["place"]),
        (&json!(id), &json!("echo beta"), &json!("local"))
    );
    assert_eq!((&script["readOnly"], &script["hidden"]), (&json!(false), &json!(false)));
    assert_eq!(script["approval"], "needed");

    let missing = cli(&data, &work, &["scripts", "get", "999", "--json"]);
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(
        stderr(&missing).lines().last(),
        Some("harness-memory: script 999 not found")
    );

    for bad in [&["scripts", "get", "1"][..], &["scripts", "get", "x", "--json"]] {
        assert_eq!(cli(&data, &work, bad).status.code(), Some(2), "{bad:?}");
    }
}

#[test]
fn remote_script_commands_need_a_team_database() {
    let (_dir, data, work) = folders();
    let id = save_elsewhere(&data, &Target::Global, "beta").to_string();

    // No team database: nothing to hide, discard or push, and no team counts in the list.
    let keys: Vec<String> = listing(&data, &work, &[])
        .as_object()
        .expect("object")
        .keys()
        .cloned()
        .collect();
    assert_eq!(keys, ["total", "groups", "scopes", "review", "places", "scripts"]);

    let hide = cli(&data, &work, &["scripts", "hide", &id]);
    assert_eq!(hide.status.code(), Some(1));
    assert!(stderr(&hide).contains("no team script to hide"), "{}", stderr(&hide));

    let unconfirmed = cli(&data, &work, &["scripts", "discard-local", "--all"]);
    assert_eq!(unconfirmed.status.code(), Some(2));

    let discard = cli(&data, &work, &["scripts", "discard-local", "--all", "--yes"]);
    assert_eq!(discard.status.code(), Some(1));
    assert!(
        stderr(&discard).contains("no team database is set"),
        "{}",
        stderr(&discard)
    );

    let push = cli(&data, &work, &["scripts", "push", "--json"]);
    assert_eq!(push.status.code(), Some(2));

    for bad in [
        &["scripts", "push", "--dry-run", "--delete-copied"][..],
        &["scripts", "push", "--overwrite", "some"],
        &["scripts", "hide"],
        &["scripts", "unhide", "x"],
    ] {
        assert_eq!(cli(&data, &work, bad).status.code(), Some(2), "{bad:?}");
    }
}

/// The URLs of the team test servers that are set, each with what keeps its database alive.
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

/// `harness-memory <args>` on the team database `url` with `scripts = <scripts>`.
fn on_team(data: &Path, cwd: &Path, url: &str, scripts: &str, args: &[&str]) -> Output {
    let mut command = Command::new(BINARY);
    command
        .args(args)
        .current_dir(cwd)
        .env("HARNESS_MEMORY_DIR", data)
        .env("HARNESS_MEMORY_DATABASE_URL", url)
        .env("HARNESS_MEMORY_SCRIPTS", scripts);
    without_team_settings(&mut command);
    command.output().expect("the executable runs")
}

/// The parsed stdout of a successful [`on_team`].
fn team_json(data: &Path, cwd: &Path, url: &str, scripts: &str, args: &[&str]) -> Value {
    let output = on_team(data, cwd, url, scripts, args);
    assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    serde_json::from_str(&stdout(&output)).expect("JSON")
}

/// Saves global script `name` into the team database `url`, as a teammate would; its id.
fn save_in_team(url: &str, name: &str) -> i64 {
    let backend = harness_memory::backend::backend_for(Some(url.to_owned()), None).expect("backend");
    let mut team = harness_memory::backend::open_scripts(&backend)
        .expect("a team database")
        .expect("open");
    let draft = ScriptDraft {
        name: name.to_owned(),
        description: format!("The team's {name}"),
        language: Language::Sh,
        body: format!("echo team {name}"),
        timeout_secs: 10,
        provenance: Provenance::default(),
    };

    team.save_script(&Target::Global, &draft)
        .expect("saved")
        .script
        .summary
        .id
}

#[test]
fn remote_local_scripts_list_hide_trust_and_delete_on_each_team_database() {
    for (url, _db) in team_urls() {
        remote_local_scripts(&url);
    }
}

fn remote_local_scripts(url: &str) {
    let (_dir, data, work) = folders();
    let team = save_in_team(url, "deploy");
    let raw = save_elsewhere(&data, &Target::Global, "deploy");
    let mine = raw + 1_000_000_000;
    let both = "remote+local";
    let run = |args: &[&str]| on_team(&data, &work, url, both, args);
    let json = |args: &[&str]| team_json(&data, &work, url, both, args);

    // Both are listed, this computer's first; the team's is read-only.
    let listing = json(&["scripts", "list", "--json"]);
    let rows: Vec<(Value, Value, Value)> = listing["scripts"]
        .as_array()
        .expect("scripts")
        .iter()
        .map(|row| (row["id"].clone(), row["place"].clone(), row["readOnly"].clone()))
        .collect();
    assert_eq!(
        rows,
        [
            (json!(mine), json!("local"), json!(false)),
            (json!(team), json!("team"), json!(true))
        ]
    );
    assert_eq!(
        (&listing["hiddenCount"], &listing["localCounts"]),
        (&json!(0), &json!({ "repo": 0, "global": 1 }))
    );
    assert_eq!(listing["places"], json!({ "local": 1, "team": 1 }));

    for (place, id) in [("team", team), ("local", mine)] {
        let one = json(&["scripts", "list", "--json", "--place", place]);
        assert_eq!(
            (&one["total"], &one["scripts"][0]["id"]),
            (&json!(1), &json!(id)),
            "{place}"
        );
        assert_eq!(one["scripts"].as_array().expect("scripts").len(), 1, "{place}");
        assert_eq!(one["places"], listing["places"], "{place}");
    }

    let sorted = json(&["scripts", "list", "--json", "--sort", "place", "--order", "desc"]);
    assert_eq!(sorted["scripts"][0]["id"], team);

    // A team script cannot be deleted from here, only hidden; this computer's cannot be hidden.
    let refused = run(&["scripts", "delete", &team.to_string()]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        stderr(&refused).contains(&format!("harness-memory scripts hide {team}")),
        "{}",
        stderr(&refused)
    );

    let local = run(&["scripts", "hide", &mine.to_string()]);
    assert_eq!(local.status.code(), Some(1));
    assert_eq!(
        stderr(&local).lines().last(),
        Some(format!("harness-memory: script {mine} is saved on this computer, delete it instead").as_str())
    );

    assert_eq!(
        json(&["scripts", "hide", &team.to_string(), "--json"]),
        json!({ "hidden": team })
    );
    let listing = json(&["scripts", "list", "--json"]);
    assert_eq!((&listing["total"], &listing["hiddenCount"]), (&json!(1), &json!(1)));
    assert_eq!(
        listing["places"],
        json!({ "local": 1, "team": 0 }),
        "the hidden script is not counted"
    );

    let hidden = json(&["scripts", "list", "--json", "--hidden"]);
    assert_eq!(hidden["total"], 1);
    assert_eq!(hidden["places"], json!({ "local": 0, "team": 1 }));
    assert_eq!(
        json(&["scripts", "list", "--json", "--hidden", "--place", "local"])["total"],
        0
    );
    assert_eq!(
        (&hidden["scripts"][0]["id"], &hidden["scripts"][0]["hidden"]),
        (&json!(team), &json!(true))
    );

    let missing = run(&["scripts", "get", &team.to_string(), "--json"]);
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(
        stderr(&missing).lines().last(),
        Some(format!("harness-memory: script {team} not found").as_str())
    );
    let shown = json(&["scripts", "get", &team.to_string(), "--json", "--hidden"]);
    assert_eq!(
        (&shown["script"]["body"], &shown["script"]["hidden"]),
        (&json!("echo team deploy"), &json!(true))
    );

    let unhid = run(&["scripts", "unhide", &team.to_string()]);
    assert_eq!(stdout(&unhid), format!("unhid script {team}\n"));

    // Approving reviewed versions by the ids the list shows.
    let shas: Vec<String> = json(&["scripts", "list", "--json"])["scripts"]
        .as_array()
        .expect("scripts")
        .iter()
        .map(|row| format!("{}={}", row["id"], row["sha256"].as_str().expect("sha256")))
        .collect();
    let trusted = run(&["trust", &shas[0], &shas[1]]);
    assert!(trusted.status.success(), "{}", stderr(&trusted));
    assert_eq!(
        stdout(&trusted),
        format!("trusted script {mine} deploy\ntrusted script {team} deploy\n")
    );
    let approvals: Vec<Value> = json(&["scripts", "list", "--json"])["scripts"]
        .as_array()
        .expect("scripts")
        .iter()
        .map(|row| row["approval"].clone())
        .collect();
    assert_eq!(approvals, [json!("approved"), json!("approved")]);

    // The same trust holds for each script read alone, under its own id.
    let local_only = team_json(&data, &work, url, "local", &["scripts", "list", "--json"]);
    assert_eq!(
        (&local_only["scripts"][0]["id"], &local_only["scripts"][0]["approval"]),
        (&json!(raw), &json!("approved"))
    );
    let team_only = team_json(&data, &work, url, "remote", &["scripts", "list", "--json"]);
    assert_eq!(team_only["scripts"][0]["approval"], "approved");

    // Read alone, each store keeps every script in one place.
    let here = team_json(
        &data,
        &work,
        url,
        "remote",
        &["scripts", "list", "--json", "--place", "local"],
    );
    assert_eq!(
        (&here["total"], &here["places"]),
        (&json!(0), &json!({ "local": 0, "team": 1 }))
    );
    let away = team_json(
        &data,
        &work,
        url,
        "local",
        &["scripts", "list", "--json", "--place", "team"],
    );
    assert_eq!(
        (&away["total"], &away["places"]),
        (&json!(0), &json!({ "local": 1, "team": 0 }))
    );

    // A plain push leaves remote+local scripts out.
    let pushed = json(&["scripts", "push", "--json", "--dry-run"]);
    assert!(pushed["nothing"].as_str().expect("why").contains("--scope"), "{pushed}");

    // Deleting every script leaves the team's, counted as kept.
    assert_eq!(
        json(&["scripts", "delete", "--all", "--yes", "--json"]),
        json!({ "deleted": 1, "team": 0, "kept": 1 })
    );
    assert_eq!(json(&["scripts", "list", "--json"])["total"], 1);
}

#[test]
fn remote_scripts_push_and_discard_what_stays_on_this_computer() {
    for (url, _db) in team_urls() {
        push_and_discard(&url);
    }
}

fn push_and_discard(url: &str) {
    let (_dir, data, work) = folders();
    let first = save_elsewhere(&data, &Target::Global, "first");
    let second = save_elsewhere(&data, &Target::Global, "second");
    save_in_team(url, "second");
    let json = |scripts: &str, args: &[&str]| team_json(&data, &work, url, scripts, args);

    // Read from the team database, the local ones are not listed but counted.
    let listing = json("remote", &["scripts", "list", "--json"]);
    assert_eq!(listing["localCounts"], json!({ "repo": 0, "global": 2 }));
    assert_eq!(listing["total"], 1);

    let preview = json("remote", &["scripts", "push", "--dry-run", "--json"]);
    let actions: Vec<(Value, Value)> = preview["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .map(|change| (change["localId"].clone(), change["action"].clone()))
        .collect();
    assert_eq!(
        actions,
        [(json!(first), json!("add")), (json!(second), json!("teamNewer"))]
    );

    let pushed = json("remote", &["scripts", "push", "--json", "--delete-copied"]);
    assert_eq!(pushed["scripts"]["added"], 1);
    assert_eq!(pushed["deleted"], 1);
    let marks: Vec<(Value, Value)> = pushed["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .map(|change| (change["deleted"].clone(), change["changedSince"].clone()))
        .collect();
    assert_eq!(marks, [(json!(true), json!(false)), (json!(false), json!(false))]);
    assert_eq!(json("remote", &["scripts", "list", "--json"])["total"], 2);

    // The one left (the team's is newer) is discarded by its local id.
    let refused = on_team(
        &data,
        &work,
        url,
        "remote+local",
        &["scripts", "discard-local", "--all", "--yes"],
    );
    assert_eq!(refused.status.code(), Some(1));
    assert!(stderr(&refused).contains("\"remote+local\""), "{}", stderr(&refused));

    let discarded = json(
        "remote",
        &["scripts", "discard-local", &second.to_string(), "--yes", "--json"],
    );
    assert_eq!(discarded, json!({ "deleted": 1 }));
    assert_eq!(
        json("remote", &["scripts", "list", "--json"])["localCounts"],
        json!({ "repo": 0, "global": 0 })
    );
}
