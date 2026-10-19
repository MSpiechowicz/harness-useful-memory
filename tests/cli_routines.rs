//! The team routine commands of the real executable, the `hooks` commands with the key
//! `routines`: `routines config --json` probes the settings, `routines list|get|put|delete
//! --json` read and write the team database, `routines delete --mine` removes the routines the
//! user shared, refusals print `{"error", "message"}` and exit 1, usage errors exit 2. The cases
//! without a team database run everywhere; the shared ones run on each team database whose test
//! server is set (libSQL, MongoDB, PostgreSQL), two machines sharing one.

mod common;
mod libsql_support;
mod mongo_support;
mod postgres_support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::mcp::repo_with_remote;
use common::routines::{body, id};
use common::stdio::{BINARY, without_team_settings};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tempfile::TempDir;

const WIDGET: &str = "git:github.com/acme/widget";

/// A machine: its data folder, a folder outside any repository, a repository with a remote, and
/// the settings every command gets.
struct Machine {
    _dir: TempDir,
    data: PathBuf,
    work: PathBuf,
    widget: PathBuf,
    settings: Vec<(&'static str, String)>,
}

fn machine() -> Machine {
    let dir = TempDir::new().expect("temp");
    let data = dir.path().join("data");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&data).expect("mkdir");
    std::fs::create_dir_all(&work).expect("mkdir");

    Machine {
        widget: repo_with_remote(dir.path(), "widget", "https://github.com/acme/widget.git"),
        _dir: dir,
        data,
        work,
        settings: vec![("USER", "ada".to_owned())],
    }
}

impl Machine {
    /// The same machine with `name` set to `value`.
    fn with(mut self, name: &'static str, value: &str) -> Self {
        self.settings.push((name, value.to_owned()));
        self
    }

    /// The same machine with neither `USER` nor `USERNAME` set.
    fn without_user(mut self) -> Self {
        self.settings.retain(|(name, _)| *name != "USER" && *name != "USERNAME");
        self
    }

    /// The same machine sharing routines through the team database `team`.
    fn on(self, team: &Team) -> Self {
        let mut shared = self
            .with("HARNESS_MEMORY_DATABASE_URL", &team.url)
            .with("HARNESS_MEMORY_ROUTINES", "remote");

        if let Some(token) = &team.token {
            shared = shared.with("HARNESS_MEMORY_DATABASE_TOKEN", token);
        }

        shared
    }

    fn path(&self, folder: &Path) -> String {
        folder.to_string_lossy().into_owned()
    }

    /// `harness-memory <args>` with `stdin`, started outside any repository.
    fn run(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut command = Command::new(BINARY);
        command
            .args(args)
            .current_dir(&self.work)
            .env("HARNESS_MEMORY_DIR", &self.data)
            .env_remove("HARNESS_MEMORY_ROUTINES")
            .env_remove("HARNESS_MEMORY_TRUST_TEAM_ROUTINES")
            .env_remove("USER")
            .env_remove("USERNAME")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        for (name, value) in &self.settings {
            command.env(name, value);
        }

        without_team_settings(&mut command);

        let mut child = command.spawn().expect("the executable runs");
        let mut input = child.stdin.take().expect("stdin");
        input.write_all(stdin.unwrap_or_default().as_bytes()).ok();
        drop(input);

        child.wait_with_output().expect("the executable ends")
    }

    /// The JSON of a successful `harness-memory <args>`.
    fn ok(&self, args: &[&str], stdin: Option<&str>) -> Value {
        let output = self.run(args, stdin);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {}", stderr(&output));
        serde_json::from_str(&stdout(&output)).expect("JSON")
    }

    /// The message of a refusal with `code`: exit 1, `{"error", "message"}` on stdout, the
    /// message as the last line of stderr.
    fn refused(&self, args: &[&str], stdin: Option<&str>, code: &str) -> String {
        let output = self.run(args, stdin);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {}", stderr(&output));

        let said: Value = serde_json::from_str(&stdout(&output)).expect("JSON");
        assert_eq!(said["error"], code, "{args:?}: {said}");

        let message = said["message"].as_str().expect("message").to_owned();
        let last = stderr(&output).lines().last().unwrap_or_default().to_owned();
        assert_eq!(last, format!("harness-memory: {message}"));
        message
    }

    fn put(&self, project: Option<&Path>, extra: &[&str], routine: &str) -> Value {
        let project = project.map(|folder| self.path(folder));
        let mut args = vec!["routines", "put", "--json"];

        if let Some(project) = &project {
            args.extend(["--project", project.as_str()]);
        }

        args.extend(extra);
        self.ok(&args, Some(routine))
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A team database under test, alive while `_keep` is.
struct Team {
    url: String,
    token: Option<String>,
    _keep: Box<dyn std::any::Any>,
}

/// Runs `case` on each team database whose test server is set.
fn each_team(case: fn(&Team)) {
    if let Some(db) = libsql_support::Database::libsql() {
        let libsql_support::Database::Libsql(server) = &db else {
            unreachable!("a libSQL database");
        };

        let (url, token) = (server.url.clone(), server.token.clone());
        eprintln!("on libSQL");
        case(&Team {
            url,
            token,
            _keep: Box::new(db),
        });
    }

    if let Some(db) = mongo_support::TestDb::new() {
        eprintln!("on MongoDB");
        case(&Team {
            url: db.url.clone(),
            token: None,
            _keep: Box::new(db),
        });
    }

    if let Some(db) = postgres_support::database() {
        eprintln!("on PostgreSQL");
        case(&Team {
            url: db.url().to_owned(),
            token: None,
            _keep: Box::new(db),
        });
    }
}

/// A project routine `id(n)` for the repository `repo`.
fn project_routine(n: u32, repo: &str) -> String {
    body(&id(n), Some(repo), "Repo triage")
}

#[test]
fn config_says_whether_routines_are_shared_and_trusted() {
    let local = json!({ "routines": "local", "trustTeamRoutines": false });
    assert_eq!(machine().ok(&["routines", "config", "--json"], None), local);

    let no_team = machine()
        .with("HARNESS_MEMORY_ROUTINES", "remote")
        .with("HARNESS_MEMORY_TRUST_TEAM_ROUTINES", "1");
    assert_eq!(
        no_team.ok(&["routines", "config", "--json"], None),
        local,
        "trust counts only while shared"
    );

    // The probe reads the settings only: the database is not contacted.
    let team = machine()
        .with("HARNESS_MEMORY_DATABASE_URL", "http://127.0.0.1:9")
        .with("HARNESS_MEMORY_ROUTINES", "remote")
        .with("HARNESS_MEMORY_TRUST_TEAM_ROUTINES", "1");
    assert_eq!(
        team.ok(&["routines", "config", "--json"], None),
        json!({ "routines": "remote", "trustTeamRoutines": true })
    );
    assert_eq!(
        team.ok(&["hooks", "config", "--json"], None),
        json!({ "hooks": "local", "trustTeamHooks": false }),
        "routines leave the hooks alone"
    );

    let dir = TempDir::new().expect("temp");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "database_url = \"http://127.0.0.1:9\"\nroutines = \"remote\"\ntrust_team_routines = true\n",
    )
    .expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }

    let configured = machine().with("HARNESS_MEMORY_CONFIG", &config.to_string_lossy());
    assert_eq!(
        configured.ok(&["routines", "config", "--json"], None),
        json!({ "routines": "remote", "trustTeamRoutines": true })
    );

    std::fs::write(&config, "routines = \"team\"\n").expect("write");
    let output = configured.run(&["routines", "config", "--json"], None);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).ends_with("routines in the config file must be \"remote\" or \"local\"\n"));
}

#[test]
fn without_sharing_lists_are_empty_and_writes_are_refused() {
    let machine = machine();
    let widget = machine.path(&machine.widget);

    assert_eq!(
        machine.ok(&["routines", "list", "--json"], None),
        json!({ "total": 0, "nextOffset": null, "project": null, "routines": [] })
    );
    assert_eq!(
        machine.ok(&["routines", "list", "--json", "--project", &widget], None),
        json!({ "total": 0, "nextOffset": null, "project": { "key": WIDGET, "shareable": true }, "routines": [] })
    );

    let routine = body(&id(1), None, "Global");
    let message = machine.refused(&["routines", "put", "--json"], Some(&routine), "local");
    assert_eq!(
        message,
        "team routines are not shared: set routines = \"remote\" in the config file, next to a team database_url"
    );
    machine.refused(&["routines", "get", &id(1), "--json"], None, "local");
    machine.refused(&["routines", "delete", &id(1), "--json"], None, "local");
    machine.refused(&["routines", "delete", "--mine", "--json"], None, "local");
    machine.refused(
        &["routines", "delete", "--mine", "--author", "grace", "--json"],
        None,
        "local",
    );
}

#[test]
fn usage_errors_exit_2() {
    let machine = machine();
    let short_sha = "a".repeat(63);
    let one = "00000000000000000000000000000001";

    for args in [
        &["routines"][..],
        &["routines", "show"],
        &["routines", "config"],
        &["routines", "config", "--json", "extra"],
        &["routines", "list"],
        &["routines", "list", "--json", "--limit", "0"],
        &["routines", "list", "--json", "--limit", "201"],
        &["routines", "get", "NOT-AN-ID", "--json"],
        &["routines", "get", one],
        &["routines", "put"],
        &["routines", "put", "--json", "--expect-sha", &short_sha],
        &["routines", "delete", one],
        &["routines", "delete", "--json"],
        &["routines", "delete", one, "--json", "--force"],
        &["routines", "delete", one, "--json", "--author", "ada"],
        &["routines", "delete", "--mine"],
        &["routines", "delete", "--mine", one, "--json"],
        &[
            "routines",
            "delete",
            "--mine",
            "--expect-sha",
            &"a".repeat(64),
            "--json",
        ],
        &["routines", "delete", "--mine", "--mine", "--json"],
        &["routines", "delete", "--mine", "--author", "--json"],
    ] {
        let output = machine.run(args, None);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {}", stderr(&output));
        assert!(
            stderr(&output).starts_with("usage: harness-memory routines config"),
            "{args:?}"
        );
    }

    let hooks = machine.run(&["hooks", "delete", "--mine", "--json"], None);
    assert_eq!(hooks.status.code(), Some(2), "hooks have no --mine");
    assert!(stderr(&hooks).starts_with("usage: harness-memory hooks config"));
}

#[test]
fn two_machines_share_routines_through_the_team_database() {
    each_team(|team| {
        let mine = machine().on(team);
        let theirs = machine().with("USER", "grace").on(team);
        let their_widget = theirs.path(&theirs.widget);

        let global = body(&id(1), None, "Global");
        let shared = mine.put(None, &[], &global);
        assert_eq!(shared["id"], id(1));
        assert_eq!(shared["repo"], Value::Null);
        assert_eq!(shared["author"], "ada");
        assert_eq!(shared["body"], global);
        assert_eq!(shared["createdAt"], shared["updatedAt"]);

        let project = mine.put(
            Some(&mine.widget.join("src")),
            &["--author", " Ada L. "],
            &project_routine(2, WIDGET),
        );
        assert_eq!(
            (&project["repo"], &project["author"]),
            (&json!(WIDGET), &json!("Ada L."))
        );

        let listed = theirs.ok(&["routines", "list", "--json", "--project", &their_widget], None);
        assert_eq!((&listed["total"], &listed["nextOffset"]), (&json!(2), &Value::Null));
        assert_eq!(listed["project"], json!({ "key": WIDGET, "shareable": true }));
        assert_eq!(listed["routines"], json!([shared, project]));
        assert_eq!(theirs.ok(&["routines", "get", &id(2), "--json"], None), project);
        assert_eq!(
            theirs.ok(&["hooks", "list", "--json"], None)["total"],
            0,
            "routines are no hooks"
        );

        let taken = theirs.refused(&["routines", "put", "--json"], Some(&global), "conflict");
        assert!(taken.contains("--expect-sha"), "{taken}");

        let sha = shared["sha256"].as_str().expect("sha256").to_owned();
        let renamed = body(&id(1), None, "Renamed");
        let updated = theirs.put(None, &["--expect-sha", &sha], &renamed);
        assert_eq!(
            (&updated["body"], &updated["author"]),
            (&json!(renamed), &json!("grace"))
        );

        mine.refused(
            &["routines", "delete", &id(1), "--expect-sha", &sha, "--json"],
            None,
            "conflict",
        );
        let current = updated["sha256"].as_str().expect("sha256");
        assert_eq!(
            mine.ok(&["routines", "delete", &id(1), "--expect-sha", current, "--json"], None),
            json!({ "deleted": true })
        );
        assert_eq!(
            mine.ok(&["routines", "delete", &id(1), "--json"], None),
            json!({ "deleted": false })
        );
        mine.refused(&["routines", "get", &id(1), "--json"], None, "notFound");
    });
}

#[test]
fn deleting_mine_keeps_the_other_authors_routines() {
    each_team(|team| {
        let mine = machine().on(team);
        let theirs = machine().with("USER", "grace").on(team);

        mine.put(None, &[], &body(&id(3), None, "Mine"));
        mine.put(Some(&mine.widget), &[], &project_routine(1, WIDGET));
        mine.put(
            None,
            &["--author", "Ada at work"],
            &body(&id(4), None, "Mine elsewhere"),
        );
        let kept = theirs.put(None, &[], &body(&id(2), None, "Theirs"));

        assert_eq!(
            mine.ok(&["routines", "delete", "--mine", "--json"], None),
            json!({ "deleted": 2, "ids": [id(1), id(3)] })
        );
        assert_eq!(
            mine.ok(&["routines", "delete", "--mine", "--json"], None),
            json!({ "deleted": 0, "ids": [] })
        );

        let listed = theirs.ok(&["routines", "list", "--json", "--limit", "200"], None);
        assert_eq!(listed["total"], 2);
        assert_eq!(listed["routines"][0], kept);

        assert_eq!(
            mine.ok(
                &["routines", "delete", "--mine", "--author", " Ada at work ", "--json"],
                None
            ),
            json!({ "deleted": 1, "ids": [id(4)] })
        );
        mine.refused(
            &["routines", "delete", "--mine", "--author", "a\u{202E}b", "--json"],
            None,
            "invalid",
        );
        assert_eq!(
            theirs.ok(&["routines", "list", "--json"], None)["routines"],
            json!([kept])
        );
    });
}

#[test]
fn deleting_mine_without_a_user_name_is_refused() {
    each_team(|team| {
        let nameless = machine().without_user().on(team);

        let unknown = nameless.put(None, &[], &body(&id(5), None, "Nobody's"));
        assert_eq!(unknown["author"], "unknown");

        let message = nameless.refused(&["routines", "delete", "--mine", "--json"], None, "refused");
        assert!(message.contains("--author"), "{message}");
        assert_eq!(nameless.ok(&["routines", "get", &id(5), "--json"], None), unknown);

        assert_eq!(
            nameless.ok(&["routines", "delete", "--mine", "--author", "unknown", "--json"], None),
            json!({ "deleted": 1, "ids": [id(5)] })
        );
    });
}

#[test]
fn what_the_team_rules_keep_out_is_refused_or_invalid() {
    each_team(|team| {
        let machine = machine().on(team);
        let widget = machine.path(&machine.widget);

        let seeded = body(&id(1), None, "Seeded").replace(
            "\"updatedBy\": \"desktop\"",
            "\"updatedBy\": \"desktop\",\n  \"seed\": \"memory-search\"",
        );
        let message = machine.refused(&["routines", "put", "--json"], Some(&seeded), "invalid");
        assert!(message.contains("seed"), "{message}");

        let local_path = body(&id(2), Some("/home/ada/work/widget"), "Local path");
        machine.refused(
            &["routines", "put", "--json", "--project", &widget],
            Some(&local_path),
            "refused",
        );
        machine.refused(
            &["routines", "put", "--json"],
            Some(&project_routine(3, WIDGET)),
            "refused",
        );

        let never = body(&id(4), None, "Never").replace(
            "{\n    \"type\": \"weekdays\",\n    \"at\": \"09:15\"\n  }",
            "{ \"type\": \"cron\", \"expression\": \"0 9 30 2 *\" }",
        );
        assert!(never.contains("0 9 30 2 *"), "{never}");
        let message = machine.refused(&["routines", "put", "--json"], Some(&never), "invalid");
        assert!(message.starts_with("the routine file is invalid: "), "{message}");
        assert!(message.contains("never runs"), "{message}");

        let hook = common::hooks::body(&id(5), None, "A hook");
        machine.refused(&["routines", "put", "--json"], Some(&hook), "invalid");

        let huge = format!("{}{}", body(&id(6), None, "Huge"), " ".repeat(16 * 1024));
        let message = machine.refused(&["routines", "put", "--json"], Some(&huge), "invalid");
        assert_eq!(message, "the routine file is invalid: it is larger than 16384 bytes");

        assert_eq!(
            machine.ok(&["routines", "list", "--json", "--project", &widget], None)["total"],
            0
        );
    });
}

#[test]
fn a_full_scope_takes_no_more_routines() {
    each_team(|team| {
        let machine = machine().on(team);

        for n in 1..=64 {
            machine.put(None, &[], &body(&id(n), None, &format!("Global {n}")));
        }

        let message = machine.refused(
            &["routines", "put", "--json"],
            Some(&body(&id(65), None, "More")),
            "limit",
        );
        assert_eq!(
            message,
            "global holds 64 team routines already: remove one from the team first"
        );

        let widget = machine.widget.clone();
        machine.put(Some(&widget), &[], &project_routine(66, WIDGET));
    });
}
