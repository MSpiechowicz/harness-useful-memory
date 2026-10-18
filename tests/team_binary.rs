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
fn an_unreachable_team_database_stops_the_server_without_showing_the_password() {
    let machine = machine();
    let url = "postgres://alice:hunter2secret@127.0.0.1:1/memory?connect_timeout=2";
    let output = run(binary(&machine, &["mcp"], Some(url)));

    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(
        message.contains("cannot open the team database (PostgreSQL at postgres://alice@127.0.0.1:1/memory?…)"),
        "{message}"
    );
    assert!(!message.contains("hunter2secret"), "{message}");
    assert!(output.stdout.is_empty());
    assert!(!machine.memory.exists(), "no local database is made instead");
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

    let remote_plaintext = run(binary(
        &machine,
        &["mcp"],
        Some("postgres://u:hunter2@db.example.com/m?sslmode=disable"),
    ));
    assert!(!remote_plaintext.status.success());
    assert!(
        stderr(&remote_plaintext).contains("sslmode=disable is only allowed"),
        "{}",
        stderr(&remote_plaintext)
    );

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
    ] {
        let output = run(binary(&machine, args, url));
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("usage:"), "{args:?}: {}", stderr(&output));
    }
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

#[test]
fn two_machines_share_one_team_memory() {
    let Some(db) = database() else { return };
    let (first, second) = (machine(), machine());

    let mut alice = team_client(&first, db.url());
    let created = alice.call(
        "memory_remember",
        json!({ "title": "Uploader location", "body": "src/upload.rs holds the uploader", "kind": "location" }),
    );
    assert!(created.starts_with("created note"), "{created}");
    assert!(alice.finish().success());

    let mut bob = team_client(&second, db.url());
    let found = bob.call("memory_search", json!({ "query": "uploader" }));
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
        .env("HARNESS_MEMORY_GLOBAL_NOTES", routing.1);
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
        "harness-memory: HARNESS_MEMORY_REPOSITORY_NOTES must be \"remote\" or \"local\"\n"
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
    assert!(said.ends_with(" (schema 3)\nscripts: team database\n"), "{said}");
    assert_eq!(said.lines().count(), 2, "{said}");
    assert!(!said.contains(":test@"), "no password: {said}");

    let split = run(routed(&machine, &["check"], Some(db.url()), ("remote", "local")));
    assert_eq!(split.status.code(), Some(0), "{}", stderr(&split));
    let file = machine.memory.join("memory.sqlite3");
    assert!(
        stdout(&split).ends_with(&format!(
            " (schema 3); repository notes: team database, global notes: this computer; local database at {} (not \
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
            " (schema newer than 2); the team database is newer than this harness-memory; update it\nscripts: \
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
        "memory_remember",
        json!({ "title": "Uploader location", "body": "src/upload.rs holds the uploader", "kind": "location" }),
    );
    assert!(shared.starts_with("created note"), "{shared}");
    let private = alice.call(
        "memory_remember",
        json!({ "title": "Uploader habits", "body": "Alice debugs the uploader with prints", "kind": "note", "scope": "global" }),
    );
    assert!(private.starts_with("created note 1000000001"), "{private}");

    let found = alice.call("memory_search", json!({ "query": "uploader" }));
    assert!(
        found.contains("Uploader location") && found.contains("Uploader habits"),
        "{found}"
    );
    let read = alice.call("memory_get", json!({ "ids": [1, 1000000001] }));
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
    let found = bob.call("memory_search", json!({ "query": "uploader" }));
    assert!(found.contains("Uploader location"), "{found}");
    assert!(!found.contains("Uploader habits"), "{found}");
    assert!(bob.finish().success());
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
