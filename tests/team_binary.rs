//! The executable with a team database: settings that stop it, two machines sharing one
//! PostgreSQL memory, `push` and `check` from the command line, and one scope kept on each
//! machine (the cases on a real database need `postgres_support`'s server and are skipped without
//! it).

// Most of these tests run only on Unix, so on Windows their helpers and imports are unused.
#![cfg_attr(not(unix), allow(unused))]

mod common;
mod postgres_support;
mod push_support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::mcp::repo_with_remote;
use common::stdio::{BINARY, Client, without_team_settings};
use harness_memory::backend::Backend;
use harness_memory::store::pending::{Entry, Kind, Pending};
use harness_memory::store::{
    Language, MemoryStore, NoteKind, Provenance, Reach, ScriptDraft, ScriptStore, SqliteStore, Target,
};
use postgres_support::database;
use serde_json::json;
use tempfile::TempDir;

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
    let config = root.path().join("config");
    std::fs::create_dir_all(&home).expect("home");

    let repo = repo_with_remote(
        &root.path().join("work"),
        "widget",
        "https://github.com/acme/widget.git",
    );

    Machine {
        memory: root.path().join("memory"),
        _root: root,
        home,
        config,
        repo,
    }
}

/// The executable with `args`, run on `machine` with the team database `url` (none: unset).
fn binary(machine: &Machine, args: &[&str], url: Option<&str>) -> Command {
    let mut command = Command::new(BINARY);
    command
        .args(args)
        .current_dir(&machine.repo)
        .env("HOME", &machine.home)
        .env("USERPROFILE", &machine.home)
        .env("HARNESS_MEMORY_DIR", &machine.memory)
        .env("XDG_CONFIG_HOME", &machine.config)
        .env("APPDATA", &machine.config);

    match url {
        Some(url) => command.env("HARNESS_MEMORY_DATABASE_URL", url),
        None => command.env_remove("HARNESS_MEMORY_DATABASE_URL"),
    };

    without_team_settings(&mut command);
    command
}

/// The config file the executable reads on this OS for `machine` (macOS ignores `XDG_CONFIG_HOME`
/// and `APPDATA` and uses `~/Library/Application Support`).
fn config_file(machine: &Machine) -> PathBuf {
    let dir = if cfg!(target_os = "macos") {
        machine.home.join("Library").join("Application Support")
    } else {
        machine.config.clone()
    };

    dir.join("harness-memory").join("config.toml")
}

fn run(mut command: Command) -> Output {
    command.stdin(Stdio::null()).output().expect("run")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn an_unreachable_team_database_keeps_the_server_up_and_fails_its_tools_without_the_password() {
    let machine = machine();
    let url = "postgres://alice:hunter2secret@127.0.0.1:1/memory?connect_timeout=2";
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"note_search","arguments":{"query":"x"}}}"#,
        "\n",
    );
    let mut command = binary(&machine, &["mcp"], Some(url));
    command.env("HARNESS_MEMORY_OFFLINE_FALLBACK", "0");
    let output = run_with_input(command, input);

    assert!(output.status.success(), "the server answers instead of exiting");
    let replies = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        replies.contains(r#""id":1"#) && replies.contains("serverInfo"),
        "{replies}"
    );
    assert!(
        replies.contains("cannot open the team database (PostgreSQL at postgres://alice@127.0.0.1:1/memory?…)"),
        "{replies}"
    );
    assert!(!replies.contains("hunter2secret"), "{replies}");
    assert!(!stderr(&output).contains("hunter2secret"), "{}", stderr(&output));
    assert!(!machine.memory.exists(), "no local database is made instead");
}

#[test]
fn an_unreachable_team_database_is_served_from_this_computer_by_default() {
    let machine = machine();
    let url = "postgres://alice:hunter2secret@127.0.0.1:1/memory?connect_timeout=2";
    let call = |id: u32, name: &str, arguments: &str| {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
        )
    };
    let input = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#.to_owned(),
        call(2, "note_save", r#"{"title":"offline note","body":"saved while the team database is down","kind":"note","scope":"global"}"#),
        call(3, "note_search", r#"{"query":"offline note"}"#),
    ]
    .join("\n")
        + "\n";

    let output = run_with_input(binary(&machine, &["mcp"], Some(url)), &input);
    let replies = String::from_utf8_lossy(&output.stdout).into_owned();

    assert!(output.status.success());
    assert!(replies.contains("created note"), "{replies}");
    assert!(replies.contains("offline note"), "{replies}");
    assert!(
        replies.contains("Offline: the team database is unreachable"),
        "{replies}"
    );
    assert!(!replies.contains(r#""isError":true"#), "{replies}");
    assert!(!replies.contains("hunter2secret"), "{replies}");
    assert!(!stderr(&output).contains("hunter2secret"), "{}", stderr(&output));
    assert!(
        machine.memory.join("pending-sync.json").is_file(),
        "the note waits to be copied"
    );
}

#[test]
fn an_agents_update_of_a_team_note_while_the_team_database_is_down_is_a_queued_success() {
    let machine = machine();
    let call = |id: u32, name: &str, arguments: &str| {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
        )
    };
    let input = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#.to_owned(),
        call(2, "note_update", r#"{"id":5,"append":"found while the team database was down","scope":"global"}"#),
        call(3, "note_delete", r#"{"id":5,"scope":"global"}"#),
    ]
    .join("\n")
        + "\n";

    let output = run_with_input(binary(&machine, &["mcp"], Some(NOBODY)), &input);
    let replies = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut lines = replies.lines();
    let (_initialize, update, delete) = (
        lines.next(),
        lines.next().expect("update"),
        lines.next().expect("delete"),
    );

    assert!(
        update.contains("note 5 queued: saved on this computer, applied when the remote database is back"),
        "{update}"
    );
    assert!(!update.contains(r#""isError":true"#), "a normal success: {update}");

    // A delete stays refused, and says why instead of "not found".
    assert!(delete.contains(r#""isError":true"#), "{delete}");
    assert!(
        delete.contains("unreachable") && delete.contains("nothing was changed"),
        "{delete}"
    );
    assert!(!delete.contains("not found"), "{delete}");
    assert!(machine.memory.join("pending-updates.json").is_file());
    assert!(!replies.contains("hunter2secret"), "{replies}");
}

#[test]
fn bad_team_settings_stop_the_server() {
    let machine = machine();

    let scheme = run(binary(&machine, &["mcp"], Some("mysql://u:hunter2@db/m")));
    assert!(!scheme.status.success());
    assert!(
        stderr(&scheme).contains("must start with postgres://"),
        "{}",
        stderr(&scheme)
    );
    assert!(!stderr(&scheme).contains("hunter2"));

    let search = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"note_search","arguments":{"query":"x"}}}"#,
        "\n",
    );
    let mut plaintext = binary(
        &machine,
        &["mcp"],
        Some("postgres://u:hunter2@db.example.com/m?sslmode=disable"),
    );
    plaintext.env("HARNESS_MEMORY_OFFLINE_FALLBACK", "0");
    let remote_plaintext = run_with_input(plaintext, search);
    let replies = String::from_utf8_lossy(&remote_plaintext.stdout).into_owned();
    assert!(replies.contains("sslmode=disable is only allowed"), "{replies}");
    assert!(!replies.contains("hunter2"), "{replies}");

    let file = config_file(&machine);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&file, "database_ulr = \"postgres://u:hunter2@db/m\"\n").expect("write");
    let typo = run(binary(&machine, &["mcp"], None));
    assert!(!typo.status.success());
    assert!(stderr(&typo).contains("database_ulr"), "{}", stderr(&typo));
    assert!(!stderr(&typo).contains("hunter2"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(&file, "database_url = \"postgres://u:hunter2@db/m\"\n").expect("write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o662)).expect("chmod");
        let writable = run(binary(&machine, &["mcp"], None));
        assert!(!writable.status.success());
        assert!(
            stderr(&writable).contains("can be changed by other users"),
            "{}",
            stderr(&writable)
        );
        assert!(!stderr(&writable).contains("hunter2"));
        assert!(!machine.memory.exists(), "the local database is not used instead");
    }
}

#[test]
fn push_needs_a_team_database() {
    let machine = machine();
    let output = run(binary(&machine, &["push"], None));

    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("HARNESS_MEMORY_DATABASE_URL"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn push_refuses_unknown_or_incomplete_options() {
    let machine = machine();
    let url = Some("postgres://u:hunter2@127.0.0.1:1/m");

    for args in [
        &["push", "--repo"][..],
        &["push", "--repo", ""],
        &["push", "--dry-run", "--dry-run"],
        &["push", "--repo", ".", "--repo=."],
        &["push", "--everything"],
        &["push", "--json", "--json"],
        &["push", "--dry-run=yes"],
        &["push", "--overwrite"],
        &["push", "--overwrite", "some"],
        &["push", "--overwrite", "12,x"],
        &["push", "--overwrite", "0"],
        &["push", "--overwrite=none", "--overwrite", "all"],
        &["push", "--scope", "both"],
        &["push", "--scope", "repo", "--scope", "global"],
    ] {
        let output = run(binary(&machine, args, url));
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("usage:"), "{args:?}: {}", stderr(&output));
    }
}

#[test]
fn push_refuses_options_that_do_not_go_together() {
    let machine = machine();
    let url = Some("postgres://u:hunter2@127.0.0.1:1/m");

    for (args, reason) in [
        (
            &["push", "--dry-run", "--delete-copied"][..],
            "does not go with --dry-run",
        ),
        (
            &["push", "--json", "--delete-copied", "--dry-run"],
            "does not go with --dry-run",
        ),
        (&["push", "--scope", "global", "--repo", "."], "does not go with --repo"),
        (&["push", "--scope", "global", "--no-global"], "leave nothing to push"),
    ] {
        let output = run(binary(&machine, args, url));
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains(reason), "{args:?}: {}", stderr(&output));
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn features_lists_what_this_build_can_do() {
    let machine = machine();
    let output = run(binary(&machine, &["features"], None));

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "notes-remote-local\nnotes-hide\npush-json\nscripts-remote-local\nscripts-hide\nscripts-push-json\nall-repos\nsave-guards\nsettings\nnotes-lang\nembedder\noffline-fallback\nnotes-useful\nnotes-authors\nnotes-author-filter\nrecall\nsave-opt-in\noffline-update-queue\nnote-expect-revision\nsync-command\npush-remote-local\npull-json\nscripts-pull-json\npush-selection\nsave-place\nremote-local-saves\nheld-save-place\n"
    );
}

/// The one JSON object `push --json` printed.
fn push_object(output: &Output) -> serde_json::Value {
    assert_eq!(output.status.code(), Some(0), "{}", stderr(output));
    serde_json::from_slice(&output.stdout).expect("one JSON object")
}

#[test]
fn push_json_says_when_there_is_nothing_to_push() {
    let machine = machine();
    let url = Some("postgres://u:hunter2@127.0.0.1:1/m?connect_timeout=1");

    for (routing, args, reason) in [
        (("local", "local"), &["push", "--json"][..], "both \"local\""),
        (
            ("remote+local", "remote+local"),
            &["push", "--json", "--dry-run"],
            "only --scope repo pushes them",
        ),
        (
            ("local", "remote"),
            &["push", "--json", "--scope", "repo"],
            "--scope repo names the repository notes, but they stay on this computer",
        ),
        (
            ("remote", "remote"),
            &["push", "--json"],
            "no notes are saved on this computer",
        ),
    ] {
        let output = run(routed(&machine, args, url, routing));
        let object = push_object(&output);
        let dry_run = args.contains(&"--dry-run");

        assert_eq!(object["dryRun"], json!(dry_run), "{args:?}");
        assert!(
            object["nothing"].as_str().is_some_and(|said| said.contains(reason)),
            "{args:?}: {object}"
        );
        assert_eq!(
            object["notes"],
            json!({ "added": 0, "updated": 0, "overwritten": 0, "kept": 0, "identical": 0, "teamNewer": 0, "failed": 0 })
        );
        assert_eq!(object["relations"], json!({ "added": 0, "unchanged": 0 }));
        assert_eq!(object["deleted"], json!(null));
        assert_eq!(object["changes"], json!([]));
        assert_eq!(object["failed"], json!([]));
    }

    // Without --json it stays a refusal.
    let output = run(routed(&machine, &["push"], url, ("remote+local", "remote+local")));
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("nothing to push"), "{}", stderr(&output));
}

/// git, which the server runs to find the repository, never sees the team database settings.
#[cfg(unix)]
#[test]
fn git_does_not_see_the_team_database_settings() {
    use std::os::unix::fs::PermissionsExt;

    let machine = machine();
    let bin = machine.home.join("fake-bin");
    let log = machine.home.join("git-env.log");
    std::fs::create_dir_all(&bin).expect("bin folder");

    let path = std::env::var_os("PATH").unwrap_or_default();
    let real_git = std::env::split_paths(&path)
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git is installed");

    let script = bin.join("git");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nenv >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real_git.display()
        ),
    )
    .expect("fake git");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let config = machine.home.join("team.toml");
    std::fs::write(&config, "").expect("config");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).expect("chmod");

    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&path));
    let mut command = binary(
        &machine,
        &["push", "--dry-run", "--repo", "."],
        Some("postgres://u:hunter2@127.0.0.1:1/m?connect_timeout=1"),
    );
    command
        .env("PATH", std::env::join_paths(paths).expect("PATH"))
        .env("HARNESS_MEMORY_DATABASE_TOKEN", "tok-secret")
        .env("HARNESS_MEMORY_CONFIG", &config);

    let output = run(command);
    assert!(!output.status.success(), "the team database is unreachable");

    let seen = std::fs::read_to_string(&log).expect("git ran");
    assert!(seen.contains("PATH="), "{seen}");
    for hidden in [
        "HARNESS_MEMORY_DATABASE_URL",
        "HARNESS_MEMORY_DATABASE_TOKEN",
        "HARNESS_MEMORY_CONFIG",
        "hunter2",
        "tok-secret",
    ] {
        assert!(!seen.contains(hidden), "git saw {hidden}");
    }
}

/// A server for `machine` on the team database `url`, with the handshake done.
fn team_client(machine: &Machine, url: &str) -> Client {
    Client::start(binary(machine, &["mcp"], Some(url)))
}

/// `trust_team_scripts` trusts only scripts read from the team database: with `scripts = "local"`
/// the team's scripts are still checked for copies, and a waiting one stays waiting there.
#[test]
fn trusting_team_scripts_does_not_trust_a_copy_while_scripts_stay_local() {
    let Some(db) = database() else { return };
    let machine = machine();
    let body = "printf team";

    let draft = ScriptDraft {
        name: "teammate".to_owned(),
        description: "From a teammate".to_owned(),
        language: Language::Sh,
        body: body.to_owned(),
        timeout_secs: 10,
        provenance: Provenance::default(),
    };
    db.open().save_script(&Target::Global, &draft).expect("team script");

    let mut command = binary(&machine, &["mcp"], Some(db.url()));
    command
        .env("HARNESS_MEMORY_SCRIPTS", "local")
        .env("HARNESS_MEMORY_TRUST_TEAM_SCRIPTS", "1");
    let mut client = Client::start(command);

    let arguments = json!({ "scope": "global", "name": "copied", "description": "", "language": "sh", "body": body });
    let reply = client.request("tools/call", json!({ "name": "script_save", "arguments": arguments }));
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    assert_eq!(
        reply["result"]["structuredContent"]["script"]["approval"], "needed",
        "{reply}"
    );
    assert!(client.finish().success());
}

#[test]
fn two_machines_share_one_team_memory() {
    let Some(db) = database() else { return };
    let (first, second) = (machine(), machine());

    let mut alice = team_client(&first, db.url());
    let created = alice.call(
        "note_save",
        json!({ "title": "Uploader location", "body": "src/upload.rs holds the uploader", "kind": "location" }),
    );
    assert!(created.starts_with("created note"), "{created}");
    assert!(alice.finish().success());

    let mut bob = team_client(&second, db.url());
    let found = bob.call("note_search", json!({ "query": "uploader" }));
    assert!(found.contains("Uploader location"), "{found}");
    assert!(bob.finish().success());

    for machine in [&first, &second] {
        assert!(!machine.memory.exists(), "nothing is kept locally");
    }
}

#[test]
fn push_from_the_command_line() {
    let Some(db) = database() else { return };
    let machine = machine();
    let local = push_support::local_memory();
    local.add_private_note();
    copy_into(&local.path, &machine.memory);

    // This checkout's repository only, without global notes.
    let mine = run(binary(
        &machine,
        &["push", "--dry-run", "--repo", ".", "--no-global"],
        Some(db.url()),
    ));
    assert!(mine.status.success(), "{}", stderr(&mine));
    let said = stdout(&mine);
    assert!(
        said.contains("dry run") && said.contains("notes 2 added, 0 updated, 0 overwritten, 0 skipped"),
        "{said}"
    );
    assert!(said.contains("relations 1 added, 0 unchanged"), "{said}");
    assert!(
        said.contains("git:github.com/acme/widget:\n  add \"Build cache\"\n  add \"Deploy steps\"\n"),
        "{said}"
    );
    assert!(!said.contains("Editor tabs") && !said.contains("Diary"), "{said}");
    assert!(!said.contains("path: keys"), "{said}");

    // Everything, by default; the path-keyed repository is pointed out.
    let dry = run(binary(&machine, &["push", "--dry-run"], Some(db.url())));
    assert!(dry.status.success(), "{}", stderr(&dry));
    let said = stdout(&dry);
    assert!(
        said.contains("notes 4 added, 0 updated, 0 overwritten, 0 skipped"),
        "{said}"
    );
    assert!(said.contains("relations 2 added, 0 unchanged"), "{said}");
    assert!(
        said.contains("path:/home/me/diary:\n  add \"Diary layout\"\n"),
        "{said}"
    );
    assert!(said.ends_with("global:\n  add \"Editor tabs\"\n"), "{said}");
    assert!(said.contains("1 repositories without a remote (path: keys)"), "{said}");

    let pushed = run(binary(&machine, &["push"], Some(db.url())));
    assert!(pushed.status.success(), "{}", stderr(&pushed));
    let said = stdout(&pushed);
    assert!(
        said.contains("notes 4 added, 0 updated, 0 overwritten, 0 skipped"),
        "{said}"
    );
    assert!(said.contains("relations 2 added, 0 unchanged"), "{said}");
    assert!(!said.contains("test@") && !said.contains(":test"), "{said}");

    let again = run(binary(&machine, &["push"], Some(db.url())));
    let said = stdout(&again);
    assert!(
        said.contains("notes 0 added, 0 updated, 0 overwritten, 4 skipped"),
        "{said}"
    );
    assert!(said.contains("relations 0 added, 2 unchanged"), "{said}");

    // A newer local text replaces the team's, and the push names the note.
    local.edit("Deploy steps", "Run make release", now() + 3600);
    copy_into(&local.path, &machine.memory);
    let overwrote = run(binary(&machine, &["push"], Some(db.url())));
    assert!(overwrote.status.success(), "{}", stderr(&overwrote));
    let said = stdout(&overwrote);
    assert!(
        said.contains("notes 0 added, 0 updated, 1 overwritten, 3 skipped"),
        "{said}"
    );
    assert!(
        said.contains("\"Deploy steps\" (git:github.com/acme/widget): overwrote the team's version"),
        "{said}"
    );
}

#[test]
fn push_stops_before_a_newer_team_database() {
    let Some(db) = database() else { return };
    let machine = machine();
    let local = push_support::local_memory();
    copy_into(&local.path, &machine.memory);

    drop(db.open());
    let mut admin = db.admin();
    admin
        .execute(
            "UPDATE harness_memory_meta SET value = value + 1 WHERE key = 'schema_version'",
            &[],
        )
        .expect("a newer version");

    let output = run(binary(&machine, &["push"], Some(db.url())));
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("set up by a newer harness-memory") && message.contains("Nothing was pushed"),
        "{message}"
    );
    assert_eq!(message.lines().count(), 1, "{message}");
    assert!(output.stdout.is_empty());

    let notes: i64 = admin
        .query_one("SELECT count(*) FROM notes", &[])
        .expect("count")
        .get(0);
    assert_eq!(notes, 0);
}

/// `binary` with the scopes routed: `(repository_notes, global_notes)` as the environment sets them.
fn routed(machine: &Machine, args: &[&str], url: Option<&str>, routing: (&str, &str)) -> Command {
    let mut command = binary(machine, args, url);
    command
        .env("HARNESS_MEMORY_REPOSITORY_NOTES", routing.0)
        .env("HARNESS_MEMORY_GLOBAL_NOTES", routing.1)
        // The `remote+local` cases here save on this computer.
        .env("HARNESS_MEMORY_REMOTE_LOCAL_SAVES", "local");
    command
}

#[test]
fn check_without_a_team_database_reports_the_local_file() {
    let machine = machine();
    let file = machine.memory.join("memory.sqlite3");

    let output = run(binary(&machine, &["check"], None));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "ok: local database at {} (not created yet)\nscripts: this computer\n",
            file.display()
        )
    );
    assert!(!file.exists(), "check does not create the database");

    std::fs::create_dir_all(&machine.memory).expect("memory folder");
    std::fs::write(&file, "").expect("a database file");
    let output = run(binary(&machine, &["check"], None));
    assert_eq!(
        stdout(&output),
        format!("ok: local database at {}\nscripts: this computer\n", file.display())
    );
}

#[test]
fn check_reports_an_unreachable_team_database_without_the_password() {
    let machine = machine();
    let url = "postgres://alice:hunter2secret@127.0.0.1:1/memory?connect_timeout=2";
    let output = run(binary(&machine, &["check"], Some(url)));

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty(), "{}", stdout(&output));
    let message = stderr(&output);
    assert!(
        message.starts_with(
            "harness-memory: cannot open the team database (PostgreSQL at postgres://alice@127.0.0.1:1/memory?…): "
        ),
        "{message}"
    );
    assert_eq!(message.lines().count(), 1, "{message}");
    assert!(!message.contains("hunter2secret"), "{message}");
    assert!(!machine.memory.exists());
}

#[test]
fn check_refuses_unknown_settings() {
    let machine = machine();
    let output = run(routed(
        &machine,
        &["check"],
        Some("postgres://u:hunter2@db/m"),
        ("team", "local"),
    ));

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "harness-memory: HARNESS_MEMORY_REPOSITORY_NOTES must be \"remote\", \"local\" or \"remote+local\"\n"
    );

    let extra = run(binary(&machine, &["check", "--verbose"], None));
    assert_eq!(extra.status.code(), Some(2));
    assert!(stderr(&extra).contains("| check |"), "{}", stderr(&extra));
}

#[test]
fn check_with_both_scopes_local_skips_the_team_database() {
    let machine = machine();
    let url = "postgres://alice:hunter2secret@127.0.0.1:1/memory";
    let mut command = routed(&machine, &["check"], Some(url), ("local", "local"));
    command.env("HARNESS_MEMORY_SCRIPTS", "local");
    let output = run(command);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("ok: local database at "),
        "{}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("is set, but repository_notes and global_notes are both \"local\""),
        "{}",
        stderr(&output)
    );
    assert!(!stderr(&output).contains("hunter2secret"));
}

#[test]
fn check_with_both_note_scopes_local_still_opens_the_team_database_for_scripts() {
    let machine = machine();
    let url = "postgres://alice:hunter2secret@127.0.0.1:1/memory?connect_timeout=2";
    let output = run(routed(&machine, &["check"], Some(url), ("local", "local")));

    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(output.stdout.is_empty(), "{}", stdout(&output));
    let message = stderr(&output);
    assert!(
        message.contains(
            "harness-memory: cannot open the team database (PostgreSQL at postgres://alice@127.0.0.1:1/memory?…): "
        ),
        "{message}"
    );
    assert!(!message.contains("hunter2secret"), "{message}");

    let Some(db) = database() else { return };
    let output = run(routed(&machine, &["check"], Some(db.url()), ("local", "local")));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let file = machine.memory.join("memory.sqlite3");
    assert_eq!(
        stdout(&output),
        format!(
            "ok: local database at {} (not created yet)\nscripts: team database\n",
            file.display()
        )
    );

    db.admin()
        .execute(
            "UPDATE harness_memory_meta SET value = value + 1 WHERE key = 'schema_version'",
            &[],
        )
        .expect("a newer version");
    let newer = run(routed(&machine, &["check"], Some(db.url()), ("local", "local")));
    assert_eq!(newer.status.code(), Some(0), "{}", stderr(&newer));
    assert!(
        stdout(&newer)
            .ends_with("\nscripts: team database; the team database is newer than this harness-memory; update it\n"),
        "{}",
        stdout(&newer)
    );
}

#[test]
fn check_opens_the_team_database() {
    let Some(db) = database() else { return };
    let machine = machine();

    let output = run(binary(&machine, &["check"], Some(db.url())));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let said = stdout(&output);
    assert!(said.starts_with("ok: PostgreSQL at postgres://"), "{said}");
    assert!(said.ends_with(" (schema 4)\nscripts: team database\n"), "{said}");
    assert_eq!(said.lines().count(), 2, "{said}");
    assert!(!said.contains(":test@"), "no password: {said}");

    let split = run(routed(&machine, &["check"], Some(db.url()), ("remote", "local")));
    assert_eq!(split.status.code(), Some(0), "{}", stderr(&split));
    let file = machine.memory.join("memory.sqlite3");
    assert!(
        stdout(&split).ends_with(&format!(
            " (schema 4); repository notes: team database, global notes: this computer; local database at {} (not \
             created yet)\nscripts: team database\n",
            file.display()
        )),
        "{}",
        stdout(&split)
    );

    let mut admin = db.admin();
    admin
        .execute(
            "UPDATE harness_memory_meta SET value = value + 1 WHERE key = 'schema_version'",
            &[],
        )
        .expect("a newer version");

    let newer = run(binary(&machine, &["check"], Some(db.url())));
    assert_eq!(newer.status.code(), Some(0), "{}", stderr(&newer));
    assert!(
        stdout(&newer).ends_with(
            " (schema newer than 4); the team database is newer than this harness-memory; update it\nscripts: \
                 team database\n"
        ),
        "{}",
        stdout(&newer)
    );
}

#[test]
fn push_refuses_when_routing_leaves_nothing_to_push() {
    let machine = machine();
    let url = Some("postgres://u:hunter2@127.0.0.1:1/m");

    for (routing, args, reason) in [
        (("local", "local"), &["push"][..], "both \"local\""),
        (
            ("local", "remote"),
            &["push", "--repo", "."],
            "--repo names a repository",
        ),
        (
            ("local", "remote"),
            &["push", "--no-global"],
            "--no-global leaves the global ones out",
        ),
    ] {
        let output = run(routed(&machine, args, url, routing));
        assert_eq!(output.status.code(), Some(2), "{args:?}: {}", stderr(&output));
        assert!(
            stderr(&output).contains("nothing to push") && stderr(&output).contains(reason),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(!stderr(&output).contains("hunter2"));
    }
}

#[test]
fn push_leaves_the_scope_kept_on_this_computer_out() {
    let Some(db) = database() else { return };
    let machine = machine();
    let local = push_support::local_memory();
    copy_into(&local.path, &machine.memory);

    let pushed = run(routed(&machine, &["push"], Some(db.url()), ("remote", "local")));
    assert!(pushed.status.success(), "{}", stderr(&pushed));
    let said = stdout(&pushed);
    assert!(
        said.contains("notes 2 added, 0 updated, 0 overwritten, 0 skipped"),
        "{said}"
    );
    assert!(said.contains("relations 1 added, 0 unchanged"), "{said}");
    assert!(
        said.contains("global notes stay on this computer (global_notes = \"local\") and were left out"),
        "{said}"
    );

    let mut admin = db.admin();
    let global: i64 = admin
        .query_one("SELECT count(*) FROM notes WHERE project_id IS NULL", &[])
        .expect("count")
        .get(0);
    assert_eq!(global, 0, "no global note reached the team database");
}

#[test]
fn a_split_server_shares_one_scope_and_keeps_the_other() {
    let Some(db) = database() else { return };
    let (first, second) = (machine(), machine());
    let start = |machine: &Machine| Client::start(routed(machine, &["mcp"], Some(db.url()), ("remote", "local")));

    let mut alice = start(&first);
    let shared = alice.call(
        "note_save",
        json!({ "title": "Uploader location", "body": "src/upload.rs holds the uploader", "kind": "location" }),
    );
    assert!(shared.starts_with("created note"), "{shared}");
    let private = alice.call(
        "note_save",
        json!({ "title": "Uploader habits", "body": "Alice debugs the uploader with prints", "kind": "note", "scope": "global" }),
    );
    assert!(private.starts_with("created note 1000000001"), "{private}");

    let found = alice.call("note_search", json!({ "query": "uploader" }));
    assert!(
        found.contains("Uploader location") && found.contains("Uploader habits"),
        "{found}"
    );
    let read = alice.call("note_get", json!({ "ids": [1, 1000000001] }));
    assert!(
        read.contains("Uploader location") && read.contains("Uploader habits"),
        "{read}"
    );
    assert!(alice.finish().success());
    assert!(
        first.memory.join("memory.sqlite3").exists(),
        "global notes are kept locally"
    );

    let mut bob = start(&second);
    let found = bob.call("note_search", json!({ "query": "uploader" }));
    assert!(found.contains("Uploader location"), "{found}");
    assert!(!found.contains("Uploader habits"), "{found}");
    assert!(bob.finish().success());
}

/// The executable with `args` on `machine`, `input` on its stdin.
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

/// The titles of the notes in `machine`'s local database, sorted.
fn local_titles(machine: &Machine) -> Vec<String> {
    let conn = rusqlite::Connection::open(machine.memory.join("memory.sqlite3")).expect("local");
    let mut statement = conn.prepare("SELECT title FROM notes ORDER BY title").expect("select");
    statement
        .query_map([], |row| row.get(0))
        .expect("titles")
        .collect::<Result<_, _>>()
        .expect("titles")
}

/// The change `object` lists for `title`.
fn change_titled<'a>(object: &'a serde_json::Value, title: &str) -> &'a serde_json::Value {
    object["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .find(|change| change["title"] == title)
        .unwrap_or_else(|| panic!("no change for {title}: {object}"))
}

#[test]
fn push_json_lists_every_note_and_deletes_the_copies() {
    let Some(db) = database() else { return };
    let machine = machine();
    let local = push_support::local_memory();
    copy_into(&local.path, &machine.memory);

    let listed = push_object(&run(binary(&machine, &["push", "--dry-run", "--json"], Some(db.url()))));
    let keys: Vec<&str> = listed.as_object().expect("object").keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "dryRun",
            "nothing",
            "notes",
            "relations",
            "entitiesCut",
            "deleted",
            "changes",
            "failed"
        ]
    );
    assert_eq!(listed["dryRun"], json!(true));
    assert_eq!(listed["nothing"], json!(null));
    assert_eq!(listed["notes"]["added"], json!(3));
    assert_eq!(listed["relations"], json!({ "added": 2, "unchanged": 0 }));

    let cache = change_titled(&listed, "Build cache");
    let change_keys: Vec<&str> = cache.as_object().expect("change").keys().map(String::as_str).collect();
    assert_eq!(
        change_keys,
        [
            "localId",
            "scope",
            "repo",
            "title",
            "action",
            "conflict",
            "teamId",
            "localUpdatedAt",
            "teamUpdatedAt",
            "deleted",
            "changedSince",
            "failed"
        ]
    );
    assert!(cache["localId"].as_i64().is_some_and(|id| id > 0), "{cache}");
    assert_eq!(cache["scope"], json!("repo"));
    assert_eq!(cache["repo"], json!("github.com/acme/widget"));
    assert_eq!(cache["action"], json!("add"));
    assert_eq!(cache["conflict"], json!(false));
    assert_eq!(cache["teamId"], json!(null));
    assert!(
        cache["localUpdatedAt"].as_str().is_some_and(|at| at.ends_with('Z')),
        "{cache}"
    );
    assert_eq!(cache["teamUpdatedAt"], json!(null));
    assert_eq!(cache["deleted"], json!(false));
    assert_eq!(cache["changedSince"], json!(false));
    assert_eq!(cache["failed"], json!(false));
    let tabs = change_titled(&listed, "Editor tabs");
    assert_eq!((&tabs["scope"], &tabs["repo"]), (&json!("global"), &json!(null)));

    let pushed = push_object(&run(binary(&machine, &["push", "--json"], Some(db.url()))));
    assert_eq!(pushed["dryRun"], json!(false));
    assert!(
        change_titled(&pushed, "Build cache")["teamId"].as_i64().is_some(),
        "{pushed}"
    );
    assert_eq!(
        local_titles(&machine).len(),
        3,
        "nothing deleted without --delete-copied"
    );

    // A newer local text is a conflict: kept with `none`, and the copies of the rest are deleted.
    local.edit("Deploy steps", "Run make release", now() + 3600);
    copy_into(&local.path, &machine.memory);
    let kept = push_object(&run(binary(
        &machine,
        &["push", "--json", "--overwrite", "none", "--delete-copied"],
        Some(db.url()),
    )));
    let deploy = change_titled(&kept, "Deploy steps");
    assert_eq!((&deploy["action"], &deploy["conflict"]), (&json!("keep"), &json!(true)));
    assert!(deploy["teamUpdatedAt"].as_str().is_some(), "{deploy}");
    assert_eq!(deploy["deleted"], json!(false));
    assert_eq!(change_titled(&kept, "Build cache")["action"], json!("identical"));
    assert_eq!(change_titled(&kept, "Build cache")["deleted"], json!(true));
    assert_eq!(kept["notes"]["kept"], json!(1));
    assert_eq!(kept["notes"]["identical"], json!(2));
    assert_eq!(kept["deleted"], json!(2));
    assert_eq!(local_titles(&machine), ["Deploy steps"]);

    // Ticked on stdin, it is overwritten and its copy goes too.
    let id = deploy["localId"].to_string();
    let command = binary(
        &machine,
        &["push", "--json", "--overwrite", "-", "--delete-copied"],
        Some(db.url()),
    );
    let overwrote = push_object(&run_with_input(command, &format!("{id}\n")));
    let deploy = change_titled(&overwrote, "Deploy steps");
    assert_eq!(
        (&deploy["action"], &deploy["conflict"]),
        (&json!("overwrite"), &json!(true))
    );
    assert_eq!(overwrote["deleted"], json!(1));
    assert!(local_titles(&machine).is_empty());
}

#[test]
fn push_takes_a_remote_local_scope_only_when_scope_names_it() {
    let Some(db) = database() else { return };
    let machine = machine();
    let local = push_support::local_memory();
    copy_into(&local.path, &machine.memory);
    let routing = ("remote+local", "remote");

    let plain = run(routed(&machine, &["push", "--dry-run"], Some(db.url()), routing));
    assert!(plain.status.success(), "{}", stderr(&plain));
    let said = stdout(&plain);
    assert!(said.contains("notes 1 added"), "{said}");
    assert!(
        said.contains("repository notes are \"remote+local\" (repository_notes) and were left out"),
        "{said}"
    );

    let named = push_object(&run(routed(
        &machine,
        &["push", "--json", "--scope", "repo"],
        Some(db.url()),
        routing,
    )));
    let scopes: Vec<&serde_json::Value> = named["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .map(|change| &change["scope"])
        .collect();
    assert_eq!(scopes, [&json!("repo"), &json!("repo")]);
    assert_eq!(named["notes"]["added"], json!(2));
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
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

/// [`binary`] with `scripts` routed as given.
fn scripts_routed(machine: &Machine, args: &[&str], url: Option<&str>, scripts: &str) -> Command {
    let mut command = binary(machine, args, url);
    command
        .env("HARNESS_MEMORY_SCRIPTS", scripts)
        .env("HARNESS_MEMORY_REMOTE_LOCAL_SAVES", "local");
    command
}

#[test]
fn scripts_push_json_says_when_there_is_nothing_to_push() {
    let machine = machine();
    let url = Some("postgres://u:hunter2@127.0.0.1:1/m?connect_timeout=1");

    for (scripts, args, reason) in [
        (
            "local",
            &["scripts", "push", "--json"][..],
            "scripts stay on this computer",
        ),
        (
            "remote+local",
            &["scripts", "push", "--json", "--dry-run"],
            "only --scope repo or --scope global pushes",
        ),
        (
            "remote",
            &["scripts", "push", "--json"],
            "no scripts are saved on this computer",
        ),
    ] {
        let output = run(scripts_routed(&machine, args, url, scripts));
        let object = push_object(&output);

        assert_eq!(object["dryRun"], json!(args.contains(&"--dry-run")), "{args:?}");
        assert!(
            object["nothing"].as_str().is_some_and(|said| said.contains(reason)),
            "{args:?}: {object}"
        );
        assert_eq!(
            object["scripts"],
            json!({ "added": 0, "updated": 0, "overwritten": 0, "kept": 0, "identical": 0, "teamNewer": 0, "failed": 0 })
        );
        assert_eq!(
            (&object["deleted"], &object["changes"], &object["failed"]),
            (&json!(null), &json!([]), &json!([]))
        );
    }

    let output = run(scripts_routed(&machine, &["scripts", "push"], url, "local"));
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("nothing to push"), "{}", stderr(&output));
}

#[test]
fn scripts_push_json_lists_every_script() {
    let Some(db) = database() else { return };
    let machine = machine();
    std::fs::create_dir_all(&machine.memory).expect("data folder");
    let mut local = SqliteStore::open(&machine.memory.join("memory.sqlite3")).expect("local file");
    let draft = ScriptDraft {
        name: "open-prs".to_owned(),
        description: "Lists open pull requests".to_owned(),
        language: Language::Sh,
        body: "gh pr list".to_owned(),
        timeout_secs: 30,
        provenance: Provenance::default(),
    };
    let local_id = local
        .save_script(&common::repo(push_support::REPO), &draft)
        .expect("saved")
        .script
        .summary
        .id;
    drop(local);

    let preview = push_object(&run(binary(
        &machine,
        &["scripts", "push", "--dry-run", "--json"],
        Some(db.url()),
    )));
    let keys: Vec<&str> = preview
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["dryRun", "nothing", "scripts", "deleted", "changes", "failed"]);
    assert_eq!(preview["scripts"]["added"], 1);

    let change = &preview["changes"][0];
    let change_keys: Vec<&str> = change.as_object().expect("change").keys().map(String::as_str).collect();
    assert_eq!(
        change_keys,
        [
            "localId",
            "scope",
            "repo",
            "name",
            "action",
            "conflict",
            "teamId",
            "localUpdatedAt",
            "teamUpdatedAt",
            "deleted",
            "changedSince",
            "failed",
            "trusted"
        ]
    );
    assert_eq!(
        (&change["localId"], &change["scope"], &change["repo"], &change["action"]),
        (
            &json!(local_id),
            &json!("repo"),
            &json!("github.com/acme/widget"),
            &json!("add")
        )
    );

    let pushed = push_object(&run(binary(
        &machine,
        &["scripts", "push", "--json", "--delete-copied"],
        Some(db.url()),
    )));
    assert_eq!(
        (&pushed["scripts"]["added"], &pushed["deleted"]),
        (&json!(1), &json!(1))
    );
    assert!(pushed["changes"][0]["teamId"].is_i64(), "{pushed}");

    let again = push_object(&run(binary(&machine, &["scripts", "push", "--json"], Some(db.url()))));
    assert_eq!(again["changes"], json!([]), "the copy was deleted here");
}

#[test]
fn check_says_when_scripts_are_read_from_both() {
    let Some(db) = database() else { return };
    let machine = machine();

    let output = run(scripts_routed(&machine, &["check"], Some(db.url()), "remote+local"));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).ends_with(" (schema 4)\nscripts: team database and this computer\n"),
        "{}",
        stdout(&output)
    );

    // With every note scope on this computer the team database is still opened for the scripts.
    let mut local_notes = routed(&machine, &["check"], Some(db.url()), ("local", "local"));
    local_notes.env("HARNESS_MEMORY_SCRIPTS", "remote+local");
    let output = run(local_notes);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).ends_with("\nscripts: team database and this computer\n"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_failed_check_starts_the_cooldown_with_its_reason() {
    let machine = machine();
    std::fs::create_dir_all(&machine.memory).expect("data folder");
    let url = "postgres://alice:hunter2secret@127.0.0.1:1/memory?connect_timeout=2";

    let output = run(binary(&machine, &["check"], Some(url)));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));

    let list = run(binary(&machine, &["notes", "list", "--json"], Some(url)));
    let shown: serde_json::Value = serde_json::from_str(&stdout(&list)).expect("json");
    assert_eq!(shown["offline"], true, "{shown}");

    let reason = shown["offlineReason"].as_str().expect("the reason");
    assert!(reason.contains("cannot open the team database"), "{reason}");
    assert!(!reason.contains("hunter2secret"), "no password: {reason}");
}

#[test]
fn a_green_check_ends_the_cooldown_for_the_lists_at_once() {
    let Some(db) = database() else { return };
    let machine = machine();
    std::fs::create_dir_all(&machine.memory).expect("data folder");

    let backend = Backend::Postgres {
        url: db.url().to_owned(),
    };
    Reach::beside(&machine.memory.join("memory.sqlite3"), &backend.team_key()).failed("it was down");

    let list = |machine: &Machine| -> serde_json::Value {
        let output = run(binary(machine, &["notes", "list", "--json"], Some(db.url())));
        serde_json::from_str(&stdout(&output)).expect("json")
    };

    let before = list(&machine);
    assert_eq!(before["offline"], true, "{before}");
    assert_eq!(before["offlineReason"], "it was down", "{before}");

    let output = run(binary(&machine, &["check"], Some(db.url())));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let after = list(&machine);
    assert!(after.get("offline").is_none(), "{after}");
    assert!(after.get("offlineReason").is_none(), "{after}");
}

/// A team database nobody listens to, for the commands that must not wait for one.
const NOBODY: &str = "postgres://alice:hunter2secret@127.0.0.1:1/memory?connect_timeout=2";

/// `harness-memory <args>` on `machine` with the team database unreachable and every note scope in it.
fn offline(machine: &Machine, args: &[&str], input: &str) -> Output {
    let command = routed(machine, args, Some(NOBODY), ("remote", "remote"));
    run_with_input(command, input)
}

fn sync_answer(machine: &Machine) -> serde_json::Value {
    let output = offline(machine, &["sync", "--json"], "");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    serde_json::from_slice(&output.stdout).expect("one JSON object")
}

#[test]
fn sync_with_nothing_waiting_makes_no_connection() {
    let machine = machine();

    let answer = sync_answer(&machine);

    assert_eq!(
        answer,
        json!({ "online": true, "waiting": { "notes": 0, "scripts": 0, "updates": 0 } })
    );
    assert!(
        !machine.memory.join("team-unreachable.json").exists(),
        "the team database was not asked"
    );
    assert!(
        !machine.memory.join("memory.sqlite3").exists(),
        "no local database was made either"
    );

    let usage = run(binary(&machine, &["sync"], None));
    assert_eq!(usage.status.code(), Some(2), "--json is required");
}

#[test]
fn a_change_to_a_team_note_while_the_team_database_is_down_is_queued_and_sync_counts_it() {
    let machine = machine();

    let output = offline(
        &machine,
        &["notes", "update", "5", "--json"],
        r#"{ "title": "A new title" }"#,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let answer: serde_json::Value = serde_json::from_slice(&output.stdout).expect("one JSON object");
    assert_eq!(
        answer,
        json!({
            "id": 5,
            "title": "A new title",
            "queued": true,
            "message": "saved on this computer, applied when the remote database is back",
        })
    );
    assert!(!stderr(&output).contains("hunter2secret"), "{}", stderr(&output));

    let queue = machine.memory.join("pending-updates.json");
    assert!(queue.is_file(), "the change waits in a file next to the database");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            std::fs::metadata(&queue).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
    }

    // The team database is still unreachable: nothing was applied, and sync says what waits.
    let answer = sync_answer(&machine);
    assert_eq!(answer["online"], false);
    assert_eq!(answer["waiting"], json!({ "notes": 0, "scripts": 0, "updates": 1 }));
}

#[test]
fn a_held_update_of_a_team_note_is_queued_against_the_revision_the_user_was_shown() {
    use harness_memory::held::{HeldItem, HeldStore, HeldTool};

    let machine = machine();
    let chat = "0b7a9b3e-3d57-4a63-9a2c-7b1d3b6f0c11";
    let item = HeldItem::new(
        chat,
        "omp",
        HeldTool::NoteUpdate,
        json!({ "id": 5, "append": "Appended by the agent", "scope": "global" }),
        json!({ "id": 5, "title": "Shown", "body": "Body", "kind": "note", "tags": [], "entities": [],
                "scope": "global", "revision": 3 }),
    );
    let store = HeldStore::in_folder(machine.memory.clone());
    let held = store.hold(item).expect("held");

    let output = offline(&machine, &["held", "save", &held, "--json"], "");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let answer: serde_json::Value = serde_json::from_slice(&output.stdout).expect("one JSON object");
    assert_eq!(
        answer,
        json!({ "saved": { "kind": "note", "id": 5, "queued": true,
                           "message": "saved on this computer, applied when the remote database is back" } })
    );
    assert!(store.list(None).expect("list").is_empty(), "the held item is done with");

    // Not `gone`: the note cannot be read, so it is not known to be deleted. The revision shown
    // is what the change is judged against when it is applied.
    let queue = std::fs::read_to_string(machine.memory.join("pending-updates.json")).expect("queue");
    let queue: serde_json::Value = serde_json::from_str(&queue).expect("JSON");
    assert_eq!(queue["updates"][0]["base_revision"], 3);
    assert_eq!(queue["updates"][0]["patch"]["append"], "Appended by the agent");
}

#[test]
fn a_held_update_without_a_current_note_is_queued_with_no_revision_while_the_team_database_is_down() {
    use harness_memory::held::{HeldItem, HeldStore, HeldTool};

    let machine = machine();
    let chat = "0b7a9b3e-3d57-4a63-9a2c-7b1d3b6f0c12";
    let item = HeldItem::new(
        chat,
        "omp",
        HeldTool::NoteUpdate,
        json!({ "id": 5, "append": "Appended by the agent", "scope": "global" }),
        serde_json::Value::Null,
    );
    let store = HeldStore::in_folder(machine.memory.clone());
    let held = store.hold(item).expect("held");

    let output = offline(&machine, &["held", "save", &held, "--json"], "");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let answer: serde_json::Value = serde_json::from_slice(&output.stdout).expect("one JSON object");
    assert_eq!(answer["saved"]["queued"], true, "{answer}");

    // No note was shown, so nothing is judged against a revision when it is applied.
    let queue = std::fs::read_to_string(machine.memory.join("pending-updates.json")).expect("queue");
    let queue: serde_json::Value = serde_json::from_str(&queue).expect("JSON");
    assert!(queue["updates"][0].get("base_revision").is_none(), "{queue}");
}

#[test]
fn an_edited_held_update_without_a_current_note_is_refused_while_the_team_database_is_down() {
    use harness_memory::held::{HeldItem, HeldStore, HeldTool};

    let machine = machine();
    let chat = "0b7a9b3e-3d57-4a63-9a2c-7b1d3b6f0c13";
    let item = HeldItem::new(
        chat,
        "omp",
        HeldTool::NoteUpdate,
        json!({ "id": 5, "append": "Appended by the agent", "scope": "global" }),
        serde_json::Value::Null,
    );
    let store = HeldStore::in_folder(machine.memory.clone());
    let held = store.hold(item).expect("held");

    // Queued with no revision, an edited draft would replace the title, body, kind and tags of the
    // shared note: it is refused and the item keeps waiting.
    let draft = r#"{ "title": "Edited", "body": "Edited body", "kind": "decision", "tags": ["x"] }"#;
    let output = offline(&machine, &["held", "save", &held, "--json"], draft);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let answer: serde_json::Value = serde_json::from_slice(&output.stdout).expect("one JSON object");
    assert_eq!(answer["error"], "refused", "{answer}");
    let message = answer["message"].as_str().expect("message");
    assert!(
        message.contains("Save it unedited") && message.contains("remote database is back"),
        "{message}"
    );
    assert!(!stdout(&output).contains("hunter2secret"), "{}", stdout(&output));
    assert!(!stderr(&output).contains("hunter2secret"), "{}", stderr(&output));
    assert!(
        !machine.memory.join("pending-updates.json").exists(),
        "nothing was queued"
    );
    assert_eq!(store.list(None).expect("list").len(), 1, "the held item is kept");

    // Unedited it still queues, and an empty draft is no edit.
    let output = offline(&machine, &["held", "save", &held, "--json"], "{}");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let answer: serde_json::Value = serde_json::from_slice(&output.stdout).expect("one JSON object");
    assert_eq!(answer["saved"]["queued"], true, "{answer}");
    assert!(store.list(None).expect("list").is_empty(), "the held item is done with");
}

/// An item held by an earlier version has a `current` with no `revision`: nothing the edited draft
/// could be judged against at replay, so it is refused like an item with no `current` at all.
#[test]
fn an_edited_held_update_whose_current_note_has_no_revision_is_refused_while_the_team_database_is_down() {
    use harness_memory::held::{HeldItem, HeldStore, HeldTool};

    let machine = machine();
    let chat = "0b7a9b3e-3d57-4a63-9a2c-7b1d3b6f0c15";
    let item = HeldItem::new(
        chat,
        "omp",
        HeldTool::NoteUpdate,
        json!({ "id": 5, "append": "Appended by the agent", "scope": "global" }),
        json!({ "id": 5, "title": "Shown", "body": "Body", "kind": "note", "tags": [], "entities": [],
                "scope": "global" }),
    );
    let store = HeldStore::in_folder(machine.memory.clone());
    let held = store.hold(item).expect("held");

    let output = offline(
        &machine,
        &["held", "save", &held, "--json"],
        r#"{ "body": "Edited body" }"#,
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let answer: serde_json::Value = serde_json::from_slice(&output.stdout).expect("one JSON object");
    assert_eq!(answer["error"], "refused", "{answer}");
    assert!(
        !machine.memory.join("pending-updates.json").exists(),
        "nothing was queued"
    );
    assert_eq!(store.list(None).expect("list").len(), 1, "the held item is kept");

    // Unedited it still queues, with no revision to judge it against.
    let output = offline(&machine, &["held", "save", &held, "--json"], "{}");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(store.list(None).expect("list").is_empty(), "the held item is done with");
}

#[test]
fn an_edited_held_update_with_a_current_note_is_still_queued_while_the_team_database_is_down() {
    use harness_memory::held::{HeldItem, HeldStore, HeldTool};

    let machine = machine();
    let chat = "0b7a9b3e-3d57-4a63-9a2c-7b1d3b6f0c14";
    let item = HeldItem::new(
        chat,
        "omp",
        HeldTool::NoteUpdate,
        json!({ "id": 5, "append": "Appended by the agent", "scope": "global" }),
        json!({ "id": 5, "title": "Shown", "body": "Body", "kind": "note", "tags": [], "entities": [],
                "scope": "global", "revision": 3 }),
    );
    let held = HeldStore::in_folder(machine.memory.clone()).hold(item).expect("held");

    let output = offline(
        &machine,
        &["held", "save", &held, "--json"],
        r#"{ "body": "Edited body" }"#,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let queue = std::fs::read_to_string(machine.memory.join("pending-updates.json")).expect("queue");
    let queue: serde_json::Value = serde_json::from_str(&queue).expect("JSON");
    assert_eq!(queue["updates"][0]["base_revision"], 3);
}

/// The waiting list of the offline saves cannot be read: what is saved here stays here, and the
/// answer says it was not listed to sync automatically.
#[test]
fn an_offline_save_says_when_it_could_not_be_listed_to_sync() {
    use harness_memory::held::{HeldItem, HeldStore, HeldTool};

    let machine = machine();
    std::fs::create_dir_all(&machine.memory).expect("data folder");
    let list = machine.memory.join("pending-sync.json");
    std::fs::write(&list, "not json").expect("a damaged list");

    let saved = |args: &[&str], input: &str| -> serde_json::Value {
        let mut command = routed(&machine, args, Some(NOBODY), ("remote", "remote"));
        command.env("HARNESS_MEMORY_SCRIPTS", "remote");
        let output = run_with_input(command, input);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {}", stderr(&output));

        serde_json::from_slice(&output.stdout).expect("one JSON object")
    };
    let message = "saved on this computer, but could not be listed to sync automatically";

    let note = saved(
        &["notes", "save", "--json"],
        r#"{ "title": "Saved offline", "body": "Kept here", "kind": "note", "scope": "global" }"#,
    );
    assert_eq!(
        (&note["notListed"], &note["message"]),
        (&json!(true), &json!(message)),
        "{note}"
    );

    let script = saved(
        &["scripts", "save", "--json"],
        r#"{ "name": "offline", "description": "d", "language": "sh", "body": "echo hi", "scope": "global" }"#,
    );
    assert_eq!(
        (&script["notListed"], &script["message"]),
        (&json!(true), &json!(message)),
        "{script}"
    );

    let item = HeldItem::new(
        "0b7a9b3e-3d57-4a63-9a2c-7b1d3b6f0c16",
        "omp",
        HeldTool::NoteSave,
        json!({ "title": "Held offline", "body": "Kept here", "kind": "note", "scope": "global" }),
        serde_json::Value::Null,
    );
    let held = HeldStore::in_folder(machine.memory.clone()).hold(item).expect("held");
    let answer = saved(&["held", "save", &held, "--json"], "");
    assert_eq!(answer["saved"]["notListed"], true, "{answer}");
    assert_eq!(answer["saved"]["message"], message, "{answer}");

    // Everything is saved on this computer, and the damaged list was not replaced.
    assert_eq!(local_titles(&machine), ["Held offline", "Saved offline"]);
    assert_eq!(std::fs::read_to_string(&list).expect("list"), "not json");

    // With a list that can be read, nothing is said.
    std::fs::remove_file(&list).expect("remove");
    let listed = saved(
        &["notes", "save", "--json"],
        r#"{ "title": "Listed offline", "body": "Kept here", "kind": "note", "scope": "global" }"#,
    );
    assert!(listed.get("notListed").is_none(), "{listed}");
    assert!(list.is_file(), "the note waits on the list");
}

#[test]
fn deleting_a_team_note_while_the_team_database_is_down_says_so() {
    let machine = machine();

    let output = offline(&machine, &["notes", "delete", "5"], "");
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("unreachable") && message.contains("nothing was changed"),
        "{message}"
    );
    assert!(!message.contains("not found"), "{message}");
}

/// A team database nobody listens on: enough for what never connects.
const UNREACHABLE: &str = "postgres://u:hunter2@127.0.0.1:1/m";

#[test]
fn push_selection_takes_only_stdin_and_does_not_go_with_overwrite() {
    let machine = machine();
    let url = Some(UNREACHABLE);

    for args in [
        &["push", "--selection"][..],
        &["push", "--selection", "12"],
        &["push", "--selection=1,2"],
        &["push", "--selection", "-", "--selection", "-"],
        &["scripts", "push", "--selection", "none"],
    ] {
        let output = run(binary(&machine, args, url));
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("usage:"), "{args:?}: {}", stderr(&output));
    }

    for args in [
        &["push", "--selection", "-", "--overwrite", "none"][..],
        &["push", "--overwrite", "all", "--selection", "-", "--json"],
        &["scripts", "push", "--selection", "-", "--overwrite", "1,2"],
    ] {
        let output = run(binary(&machine, args, url));
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            stderr(&output).contains("does not go with --overwrite"),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn push_selection_refuses_anything_but_an_object_with_both_lists_of_reviewed_entries() {
    let machine = machine();
    std::fs::create_dir_all(&machine.memory).expect("data folder");
    drop(SqliteStore::open(&machine.memory.join("memory.sqlite3")).expect("local file"));

    let many = format!("{{\"only\":[],\"overwrite\":[{}]}}", vec!["5"; 100_001].join(","));
    let huge = format!(
        "{{\"only\":[],\"overwrite\":[],\"x\":\"{}\"}}",
        " ".repeat(2 * 1024 * 1024)
    );

    for input in [
        "",
        "not json",
        "[1,2]",
        "{\"only\":[]}",
        "{\"overwrite\":[]}",
        "{\"only\":[],\"overwrite\":[],\"extra\":true}",
        // The bare ids of the older format are not what was reviewed.
        "{\"only\":[1],\"overwrite\":[]}",
        "{\"only\":[{\"id\":1,\"name\":\"a\"}],\"overwrite\":[]}",
        "{\"only\":[{\"id\":1,\"updatedAt\":\"2026-01-01T00:00:00Z\"}],\"overwrite\":[]}",
        "{\"only\":[{\"name\":\"a\",\"updatedAt\":\"2026-01-01T00:00:00Z\"}],\"overwrite\":[]}",
        "{\"only\":[{\"id\":1,\"name\":\"a\",\"updatedAt\":\"2026-01-01T00:00:00Z\",\"x\":1}],\"overwrite\":[]}",
        "{\"only\":[{\"id\":0,\"name\":\"a\",\"updatedAt\":\"2026-01-01T00:00:00Z\"}],\"overwrite\":[]}",
        "{\"only\":[{\"id\":\"1\",\"name\":\"a\",\"updatedAt\":\"2026-01-01T00:00:00Z\"}],\"overwrite\":[]}",
        "{\"only\":[{\"id\":1.5,\"name\":\"a\",\"updatedAt\":\"2026-01-01T00:00:00Z\"}],\"overwrite\":[]}",
        "{\"only\":[{\"id\":1,\"name\":7,\"updatedAt\":\"2026-01-01T00:00:00Z\"}],\"overwrite\":[]}",
        "{\"only\":[{\"id\":1,\"name\":\"a\",\"updatedAt\":null}],\"overwrite\":[]}",
        // The same id twice, with two answers about what it was.
        "{\"only\":[{\"id\":1,\"name\":\"a\",\"updatedAt\":\"2026-01-01T00:00:00Z\"},\
         {\"id\":1,\"name\":\"b\",\"updatedAt\":\"2026-01-01T00:00:00Z\"}],\"overwrite\":[]}",
        "{\"only\":[],\"overwrite\":[-4]}",
        "{\"only\":[],\"overwrite\":[\"1\"]}",
        many.as_str(),
        huge.as_str(),
    ] {
        for args in [
            &["push", "--selection", "-"][..],
            &["scripts", "push", "--selection", "-"],
        ] {
            let output = run_with_input(binary(&machine, args, Some(UNREACHABLE)), input);
            let shown: String = input.chars().take(60).collect();

            assert_eq!(output.status.code(), Some(2), "{args:?} {shown}: {}", stderr(&output));
            assert!(
                stderr(&output).contains("--selection - reads"),
                "{args:?} {shown}: {}",
                stderr(&output)
            );
            assert!(output.stdout.is_empty(), "{args:?} {shown}");
        }
    }
}

/// A reviewed selection listing `ids`, as `--selection -` reads it.
fn listing(ids: &[i64]) -> String {
    let only: Vec<serde_json::Value> = ids
        .iter()
        .map(|id| json!({ "id": id, "name": "a", "updatedAt": "2026-01-01T00:00:00Z" }))
        .collect();

    json!({ "only": only, "overwrite": [] }).to_string()
}

#[test]
fn push_selection_reports_every_listed_id_when_there_is_nothing_to_push_from() {
    let machine = machine();
    let url = Some("postgres://u:hunter2@127.0.0.1:1/m?connect_timeout=1");

    // No local database, or no scope taken: a listed id is never answered as "nothing to push".
    for (args, routing, scripts, noun) in [
        (
            &["push", "--selection", "-", "--json"][..],
            ("remote", "remote"),
            "local",
            "note",
        ),
        (
            &["push", "--selection", "-", "--json"],
            ("local", "local"),
            "local",
            "note",
        ),
        (
            &["scripts", "push", "--selection", "-", "--json"],
            ("remote", "remote"),
            "remote",
            "script",
        ),
        (
            &["scripts", "push", "--selection", "-", "--json"],
            ("remote", "remote"),
            "local",
            "script",
        ),
    ] {
        let command = |input: &str| {
            let mut command = routed(&machine, args, url, routing);
            command.env("HARNESS_MEMORY_SCRIPTS", scripts);
            run_with_input(command, input)
        };

        let output = command(&listing(&[3, 7]));
        let object: serde_json::Value = serde_json::from_slice(&output.stdout).expect("one JSON object");

        assert_eq!(output.status.code(), Some(1), "{args:?} {routing:?}: {object}");
        assert_eq!(object["nothing"], json!(null), "{args:?} {routing:?}: {object}");

        let ids: Vec<i64> = object["failed"]
            .as_array()
            .expect("failed")
            .iter()
            .map(|failure| failure["localId"].as_i64().expect("id"))
            .collect();
        assert_eq!(ids, [3, 7], "{args:?} {routing:?}: {object}");
        assert!(
            object["failed"][0]["message"]
                .as_str()
                .is_some_and(|said| said.contains(noun)),
            "{object}"
        );
        assert!(!stderr(&output).contains("hunter2"), "{}", stderr(&output));
        assert!(
            !machine.memory.join("memory.sqlite3").exists(),
            "no local database was made"
        );

        // An empty selection stays "nothing to push".
        let object = push_object(&command(&listing(&[])));
        assert!(object["nothing"].is_string(), "{args:?} {routing:?}: {object}");
        assert_eq!(object["failed"], json!([]), "{args:?} {routing:?}");
    }
}

/// A machine whose local file holds notes and scripts, some of them listed as waiting for the team
/// database [`UNREACHABLE`] (as the offline fallback leaves them), and that team's key.
struct Waiting {
    machine: Machine,
    team: String,
}

fn waiting_machine() -> Waiting {
    let machine = machine();
    std::fs::create_dir_all(&machine.memory).expect("data folder");

    let team = Backend::Postgres {
        url: UNREACHABLE.to_owned(),
    }
    .team_key();

    Waiting { machine, team }
}

impl Waiting {
    fn local(&self) -> SqliteStore {
        SqliteStore::open(&self.machine.memory.join("memory.sqlite3")).expect("local file")
    }

    fn pending(&self) -> Pending {
        Pending::beside(&self.machine.memory.join("memory.sqlite3"))
    }

    fn note(&self, target: &Target, title: &str) -> i64 {
        let draft = common::draft(title, "Saved here", NoteKind::Note);
        self.local().remember(target, &draft).expect("note").id
    }

    fn script(&self, target: &Target, name: &str) -> i64 {
        let draft = ScriptDraft {
            name: name.to_owned(),
            description: String::new(),
            language: Language::Sh,
            body: format!("printf {name}"),
            timeout_secs: 10,
            provenance: Provenance::default(),
        };

        self.local()
            .save_script(target, &draft)
            .expect("script")
            .script
            .summary
            .id
    }

    /// The entry the offline fallback lists for the local row `id`.
    fn entry(&self, team: &str, id: i64, scope: &str, name: &str) -> Entry {
        Entry {
            team: team.to_owned(),
            id,
            scope: scope.to_owned(),
            name: name.to_owned(),
        }
    }

    fn wait(&self, kind: Kind, id: i64, scope: &str, name: &str) -> Entry {
        let entry = self.entry(&self.team, id, scope, name);
        self.pending().add(kind, entry.clone()).expect("listed");
        entry
    }

    fn titles(&self) -> Vec<String> {
        local_titles(&self.machine)
    }

    fn names(&self) -> Vec<String> {
        let conn = rusqlite::Connection::open(self.machine.memory.join("memory.sqlite3")).expect("local");
        let mut statement = conn.prepare("SELECT name FROM scripts ORDER BY name").expect("select");
        statement
            .query_map([], |row| row.get(0))
            .expect("names")
            .collect::<Result<_, _>>()
            .expect("names")
    }

    fn discard(&self, noun: &str, args: &[&str]) -> Output {
        let mut all = vec![noun, "discard-local"];
        all.extend_from_slice(args);

        let mut command = binary(&self.machine, &all, Some(UNREACHABLE));
        command.env("HARNESS_MEMORY_SCRIPTS", "remote");
        run(command)
    }
}

const WIDGET_KEY: &str = "git:github.com/acme/widget";

#[test]
fn discard_local_leaves_the_notes_that_still_wait_for_the_team_database() {
    let waiting = waiting_machine();
    let widget = common::repo(push_support::REPO);

    let waits_global = waiting.note(&Target::Global, "Waits global");
    let waits_repo = waiting.note(&widget, "Waits repo");
    waiting.note(&Target::Global, "Free global");
    waiting.note(&widget, "Free repo");
    let other_team = waiting.note(&Target::Global, "Waits elsewhere");

    waiting.wait(Kind::Notes, waits_global, "global", "Waits global");
    waiting.wait(Kind::Notes, waits_repo, WIDGET_KEY, "Waits repo");
    let elsewhere = waiting.entry("team:other", other_team, "global", "Waits elsewhere");
    waiting.pending().add(Kind::Notes, elsewhere).expect("listed");

    // A scope's notes that wait stay, and so do they in the scope's `--all`.
    let scoped = push_object(&waiting.discard("notes", &["--all", "--yes", "--json", "--scope", "repo"]));
    assert_eq!(scoped, json!({ "deleted": 1, "waiting": 1 }));
    assert_eq!(
        waiting.titles(),
        ["Free global", "Waits elsewhere", "Waits global", "Waits repo"]
    );

    // Every other note goes, among them the one only another team database waits for.
    let all = push_object(&waiting.discard("notes", &["--all", "--yes", "--json"]));
    assert_eq!(all, json!({ "deleted": 2, "waiting": 2 }));
    assert_eq!(waiting.titles(), ["Waits global", "Waits repo"]);

    // Named, the same: the id that waits is reported, not deleted.
    let named = waiting.note(&Target::Global, "Named");
    let ids = format!("{waits_global},{named}");
    let one = push_object(&waiting.discard("notes", &[&ids, "--yes", "--json"]));
    assert_eq!(one, json!({ "deleted": 1, "waiting": 1 }));
    assert_eq!(waiting.titles(), ["Waits global", "Waits repo"]);

    let text = waiting.discard("notes", &[&waits_global.to_string(), "--yes"]);
    assert_eq!(text.status.code(), Some(0), "{}", stderr(&text));
    assert_eq!(
        stdout(&text),
        "deleted 0 notes from this computer, kept 1 that still wait for the team database\n"
    );
    assert_eq!(waiting.titles(), ["Waits global", "Waits repo"]);

    // Once a sync took it off the list, it can be discarded.
    waiting
        .pending()
        .remove(
            Kind::Notes,
            &[waiting.entry(&waiting.team, waits_global, "global", "Waits global")],
        )
        .expect("copied");
    let freed = push_object(&waiting.discard("notes", &[&waits_global.to_string(), "--yes", "--json"]));
    assert_eq!(freed, json!({ "deleted": 1, "waiting": 0 }));
    assert_eq!(waiting.titles(), ["Waits repo"]);
}

#[test]
fn discard_local_leaves_the_scripts_that_still_wait_for_the_team_database() {
    let waiting = waiting_machine();
    let widget = common::repo(push_support::REPO);

    let waits_global = waiting.script(&Target::Global, "waits-global");
    let waits_repo = waiting.script(&widget, "waits-repo");
    waiting.script(&Target::Global, "free-global");
    waiting.script(&widget, "free-repo");
    let other_team = waiting.script(&Target::Global, "waits-elsewhere");

    waiting.wait(Kind::Scripts, waits_global, "global", "waits-global");
    waiting.wait(Kind::Scripts, waits_repo, WIDGET_KEY, "waits-repo");
    let elsewhere = waiting.entry("team:other", other_team, "global", "waits-elsewhere");
    waiting.pending().add(Kind::Scripts, elsewhere).expect("listed");

    // A listed note does not hold a script back: the lists are kept apart.
    let note = waiting.note(&Target::Global, "A note");
    waiting.wait(Kind::Notes, waits_global, "global", "A note");
    waiting.wait(Kind::Notes, note, "global", "A note");

    let scoped = push_object(&waiting.discard("scripts", &["--all", "--yes", "--json", "--scope", "global"]));
    assert_eq!(scoped, json!({ "deleted": 2, "waiting": 1 }));
    assert_eq!(waiting.names(), ["free-repo", "waits-global", "waits-repo"]);

    let all = push_object(&waiting.discard("scripts", &["--all", "--yes", "--json"]));
    assert_eq!(all, json!({ "deleted": 1, "waiting": 2 }));
    assert_eq!(waiting.names(), ["waits-global", "waits-repo"]);

    let ids = format!("{waits_repo},{waits_global}");
    let named = push_object(&waiting.discard("scripts", &[&ids, "--yes", "--json"]));
    assert_eq!(named, json!({ "deleted": 0, "waiting": 2 }));

    let text = waiting.discard("scripts", &[&ids, "--yes"]);
    assert_eq!(
        stdout(&text),
        "deleted 0 scripts from this computer, kept 2 that still wait for the team database\n"
    );

    waiting
        .pending()
        .remove(
            Kind::Scripts,
            &[waiting.entry(&waiting.team, waits_repo, WIDGET_KEY, "waits-repo")],
        )
        .expect("copied");
    let freed = push_object(&waiting.discard("scripts", &[&ids, "--yes", "--json"]));
    assert_eq!(freed, json!({ "deleted": 1, "waiting": 1 }));
    assert_eq!(waiting.names(), ["waits-global"]);
}

impl Waiting {
    /// The list of notes saved offline, as the folder holds it.
    fn list(&self) -> PathBuf {
        self.machine.memory.join("pending-sync.json")
    }

    /// Replaces the list (which held `listed`) with one the program refuses to read, `how`.
    #[cfg(unix)]
    fn break_list(&self, how: &str, listed: &[u8]) {
        use std::os::unix::fs::PermissionsExt;

        let _ = std::fs::remove_file(self.list());

        match how {
            "malformed" => self.plant(b"not json"),
            "not a list" => self.plant(b"[1, 2]"),
            "oversized" => self.plant(&vec![b' '; 4 * 1024 * 1024 + 1]),
            "writable by others" => {
                std::fs::write(self.list(), listed).expect("list");
                std::fs::set_permissions(self.list(), std::fs::Permissions::from_mode(0o666)).expect("open");
            }
            "a link" => {
                let target = self.machine.memory.join("elsewhere.json");
                std::fs::write(&target, listed).expect("target");
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).expect("private");
                std::os::unix::fs::symlink(&target, self.list()).expect("link");
            }
            other => panic!("unknown way to break the list: {other}"),
        }
    }

    /// Replaces the list with `bytes`, private to its owner as the program writes it.
    #[cfg(unix)]
    fn plant(&self, bytes: &[u8]) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(self.list(), bytes).expect("planted list");
        std::fs::set_permissions(self.list(), std::fs::Permissions::from_mode(0o600)).expect("private");
    }
}

#[cfg(unix)]
#[test]
fn discard_local_deletes_nothing_when_the_list_of_waiting_entries_cannot_be_read() {
    for noun in ["notes", "scripts"] {
        let waiting = waiting_machine();
        let widget = common::repo(push_support::REPO);

        // One row waits for the team database, the other does not: only the list tells them apart.
        let (waits, free) = if noun == "notes" {
            (waiting.note(&Target::Global, "Waits"), waiting.note(&widget, "Free"))
        } else {
            (
                waiting.script(&Target::Global, "waits"),
                waiting.script(&widget, "free"),
            )
        };
        let kind = if noun == "notes" { Kind::Notes } else { Kind::Scripts };
        waiting.wait(kind, waits, "global", if noun == "notes" { "Waits" } else { "waits" });

        let rows = || {
            if noun == "notes" {
                waiting.titles()
            } else {
                waiting.names()
            }
        };
        let before = rows();
        let listed = std::fs::read(waiting.list()).expect("the list");

        for how in ["malformed", "not a list", "oversized", "writable by others", "a link"] {
            waiting.break_list(how, &listed);

            let free_id = free.to_string();
            let invocations: [&[&str]; 3] = [
                &["--all", "--yes", "--json"],
                &["--all", "--yes", "--scope", "repo"],
                &[&free_id, "--yes", "--json"],
            ];

            for args in invocations {
                let output = waiting.discard(noun, args);

                assert_eq!(
                    output.status.code(),
                    Some(1),
                    "{noun} {how} {args:?}: {}",
                    stderr(&output)
                );
                assert!(output.stdout.is_empty(), "{noun} {how} {args:?}: {}", stdout(&output));
                assert!(
                    stderr(&output).contains("nothing was deleted") && stderr(&output).contains("pending-sync.json"),
                    "{noun} {how} {args:?}: {}",
                    stderr(&output)
                );
                assert_eq!(rows(), before, "{noun} {how} {args:?}: nothing was deleted");
            }
        }

        // Once the list is readable again, only what does not wait goes.
        let _ = std::fs::remove_file(waiting.list());
        std::fs::remove_file(waiting.machine.memory.join("elsewhere.json")).ok();
        waiting.wait(kind, waits, "global", if noun == "notes" { "Waits" } else { "waits" });
        let all = push_object(&waiting.discard(noun, &["--all", "--yes", "--json"]));
        assert_eq!(all, json!({ "deleted": 1, "waiting": 1 }), "{noun}");
    }
}

#[test]
fn discard_local_without_a_list_deletes_what_is_asked_for() {
    let waiting = waiting_machine();
    waiting.note(&Target::Global, "One");
    waiting.note(&Target::Global, "Two");
    assert!(!waiting.list().exists());

    let all = push_object(&waiting.discard("notes", &["--all", "--yes", "--json"]));
    assert_eq!(all, json!({ "deleted": 2, "waiting": 0 }));
    assert!(waiting.titles().is_empty());
}
