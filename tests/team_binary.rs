//! The executable with a team database: settings that stop it, two machines sharing one
//! PostgreSQL memory, and `push` from the command line (those two need `postgres_support`'s
//! server and are skipped without it).

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

    let file = machine.config.join("harness-memory").join("config.toml");
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
