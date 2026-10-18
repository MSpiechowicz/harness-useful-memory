//! The team hook commands of the real executable: `hooks config --json` probes the settings,
//! `hooks list|get|put|delete --json` read and write the team database, refusals print
//! `{"error", "message"}` and exit 1, usage errors exit 2. The cases without a team database run
//! everywhere; the shared ones run on each team database whose test server is set (libSQL,
//! MongoDB, PostgreSQL), two machines sharing one.

mod common;
mod libsql_support;
mod mongo_support;
mod postgres_support;

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use common::hooks::{body, id};
use common::mcp::{git, repo_with_remote};
use common::stdio::{BINARY, without_team_settings};
use mongodb::bson::{Bson, Document, doc};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tempfile::TempDir;

const WIDGET: &str = "git:github.com/acme/widget";

/// A machine: its data folder, a folder outside any repository, two repositories with remotes and
/// one without, and the settings every command gets.
struct Machine {
    _dir: TempDir,
    data: PathBuf,
    work: PathBuf,
    widget: PathBuf,
    gadget: PathBuf,
    unshared: PathBuf,
    settings: Vec<(&'static str, String)>,
}

fn machine() -> Machine {
    let dir = TempDir::new().expect("temp");
    let data = dir.path().join("data");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&data).expect("mkdir");
    std::fs::create_dir_all(&work).expect("mkdir");

    let unshared = dir.path().join("notes");
    std::fs::create_dir_all(&unshared).expect("mkdir");
    git(&unshared, &["init", "-q"]);

    Machine {
        widget: repo_with_remote(dir.path(), "widget", "https://github.com/acme/widget.git"),
        gadget: repo_with_remote(dir.path(), "gadget", "https://github.com/acme/gadget.git"),
        unshared,
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

    /// The same machine sharing hooks through the team database `team`.
    fn on(self, team: &Team) -> Self {
        let mut shared = self
            .with("HARNESS_MEMORY_DATABASE_URL", &team.url)
            .with("HARNESS_MEMORY_HOOKS", "remote");

        if let Some(token) = &team.token {
            shared = shared.with("HARNESS_MEMORY_DATABASE_TOKEN", token);
        }

        shared
    }

    fn path(&self, folder: &std::path::Path) -> String {
        folder.to_string_lossy().into_owned()
    }

    /// `harness-memory <args>` with `stdin`, started outside any repository.
    fn run(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut command = Command::new(BINARY);
        command
            .args(args)
            .current_dir(&self.work)
            .env("HARNESS_MEMORY_DIR", &self.data)
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

    fn put(&self, project: Option<&std::path::Path>, extra: &[&str], hook: &str) -> Value {
        let project = project.map(|folder| self.path(folder));
        let mut args = vec!["hooks", "put", "--json"];

        if let Some(project) = &project {
            args.extend(["--project", project.as_str()]);
        }

        args.extend(extra);
        self.ok(&args, Some(hook))
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A team database under test, alive while this is.
struct Team {
    url: String,
    token: Option<String>,
    /// Writes `n` global rows this server never writes (ids that are no hook ids), created before
    /// any hook, behind the server's back. The `hooks` table must exist (after a first command).
    tamper: Box<dyn Fn(usize)>,
}

/// A row no hook command writes: its id is no hook id.
fn bad_id(n: usize) -> String {
    format!("bad-{n:03}")
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
            tamper: Box::new(move |n| {
                for index in 0..n {
                    db.batch(&format!(
                        "INSERT INTO hooks (id, repo, body, sha256, author, created_at, updated_at)
                         VALUES ('{}', NULL, '{{}}', 'x', 'x', 0, 0)",
                        bad_id(index)
                    ));
                }
            }),
        });
    }

    if let Some(db) = mongo_support::TestDb::new() {
        eprintln!("on MongoDB");
        case(&Team {
            url: db.url.clone(),
            token: None,
            tamper: Box::new(move |n| {
                let rows = (0..n).map(|index| {
                    doc! {
                        "_id": bad_id(index), "repo": Bson::Null, "body": "{}", "sha256": "x", "author": "x",
                        "created_at": 0_i64, "updated_at": 0_i64,
                    }
                });
                db.raw()
                    .collection::<Document>("hooks")
                    .insert_many(rows)
                    .run()
                    .expect("tamper");
            }),
        });
    }

    if let Some(db) = postgres_support::database() {
        eprintln!("on PostgreSQL");
        case(&Team {
            url: db.url().to_owned(),
            token: None,
            tamper: Box::new(move |n| {
                for index in 0..n {
                    db.admin()
                        .execute(
                            "INSERT INTO hooks (id, repo, body, sha256, author, created_at, updated_at)
                             VALUES ($1, NULL, '{}', 'x', 'x', 0, 0)",
                            &[&bad_id(index)],
                        )
                        .expect("tamper");
                }
            }),
        });
    }
}

/// A project hook `id(n)` for the repository `repo`.
fn project_hook(n: u32, repo: &str) -> String {
    body(&id(n), Some(repo), "Repo rules")
}

#[test]
fn config_says_whether_hooks_are_shared_and_trusted() {
    let local = json!({ "hooks": "local", "trustTeamHooks": false });
    assert_eq!(machine().ok(&["hooks", "config", "--json"], None), local);

    let no_team = machine()
        .with("HARNESS_MEMORY_HOOKS", "remote")
        .with("HARNESS_MEMORY_TRUST_TEAM_HOOKS", "1");
    assert_eq!(
        no_team.ok(&["hooks", "config", "--json"], None),
        local,
        "trust counts only while shared"
    );

    let not_shared = machine()
        .with("HARNESS_MEMORY_DATABASE_URL", "http://127.0.0.1:9")
        .with("HARNESS_MEMORY_TRUST_TEAM_HOOKS", "1");
    assert_eq!(not_shared.ok(&["hooks", "config", "--json"], None), local);

    // The probe reads the settings only: the database is not contacted.
    let team = machine()
        .with("HARNESS_MEMORY_DATABASE_URL", "http://127.0.0.1:9")
        .with("HARNESS_MEMORY_HOOKS", "remote")
        .with("HARNESS_MEMORY_TRUST_TEAM_HOOKS", "1");
    assert_eq!(
        team.ok(&["hooks", "config", "--json"], None),
        json!({ "hooks": "remote", "trustTeamHooks": true })
    );

    let dir = TempDir::new().expect("temp");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "database_url = \"http://127.0.0.1:9\"\nhooks = \"remote\"\ntrust_team_hooks = true\n",
    )
    .expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }

    let configured = machine().with("HARNESS_MEMORY_CONFIG", &config.to_string_lossy());
    assert_eq!(
        configured.ok(&["hooks", "config", "--json"], None),
        json!({ "hooks": "remote", "trustTeamHooks": true })
    );

    std::fs::write(&config, "hooks = \"team\"\n").expect("write");
    let output = configured.run(&["hooks", "config", "--json"], None);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).ends_with("hooks in the config file must be \"remote\" or \"local\"\n"));
}

#[test]
fn without_sharing_lists_are_empty_and_writes_are_refused() {
    let machine = machine();
    let widget = machine.path(&machine.widget);
    let unshared = machine.path(&machine.unshared);

    assert_eq!(
        machine.ok(&["hooks", "list", "--json"], None),
        json!({ "total": 0, "nextOffset": null, "project": null, "hooks": [] })
    );
    assert_eq!(
        machine.ok(&["hooks", "list", "--json", "--project", &widget], None),
        json!({ "total": 0, "nextOffset": null, "project": { "key": WIDGET, "shareable": true }, "hooks": [] })
    );

    let listed = machine.ok(&["hooks", "list", "--json", "--project", &unshared], None);
    assert_eq!(listed["project"]["shareable"], false);
    assert!(listed["project"]["key"].as_str().expect("key").starts_with("path:"));

    let hook = body(&id(1), None, "Global");
    let message = machine.refused(&["hooks", "put", "--json"], Some(&hook), "local");
    assert!(message.contains("hooks = \"remote\""), "{message}");
    machine.refused(&["hooks", "get", &id(1), "--json"], None, "local");
    machine.refused(&["hooks", "delete", &id(1), "--json"], None, "local");
}

#[test]
fn usage_errors_exit_2() {
    let machine = machine();
    let short_sha = "a".repeat(63);

    for args in [
        &["hooks"][..],
        &["hooks", "show"],
        &["hooks", "config"],
        &["hooks", "config", "--json", "extra"],
        &["hooks", "list"],
        &["hooks", "list", "--json", "--limit", "0"],
        &["hooks", "list", "--json", "--limit", "201"],
        &["hooks", "list", "--json", "--query", "x"],
        &["hooks", "list", "--json", "--project", " "],
        &["hooks", "get", "--json"],
        &["hooks", "get", "NOT-AN-ID", "--json"],
        &["hooks", "get", "00000000000000000000000000000001"],
        &["hooks", "put"],
        &["hooks", "put", "--json", "--expect-sha", &short_sha],
        &["hooks", "put", "--json", "extra"],
        &["hooks", "delete", "00000000000000000000000000000001"],
        &["hooks", "delete", "--json"],
        &[
            "hooks",
            "delete",
            "00000000000000000000000000000001",
            "--json",
            "--force",
        ],
    ] {
        let output = machine.run(args, None);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {}", stderr(&output));
        assert!(
            stderr(&output).starts_with("usage: harness-memory hooks config"),
            "{args:?}"
        );
    }
}

#[test]
fn two_machines_share_hooks_through_the_team_database() {
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
            &project_hook(2, WIDGET),
        );
        assert_eq!(
            (&project["repo"], &project["author"]),
            (&json!(WIDGET), &json!("Ada L."))
        );

        let listed = theirs.ok(&["hooks", "list", "--json", "--project", &their_widget], None);
        assert_eq!((&listed["total"], &listed["nextOffset"]), (&json!(2), &Value::Null));
        assert_eq!(listed["project"], json!({ "key": WIDGET, "shareable": true }));
        assert_eq!(listed["hooks"], json!([shared, project]));

        let paged = theirs.ok(
            &[
                "hooks",
                "list",
                "--json",
                "--project",
                &their_widget,
                "--offset",
                "1",
                "--limit",
                "1",
            ],
            None,
        );
        assert_eq!((&paged["total"], &paged["hooks"]), (&json!(2), &json!([project])));
        assert_eq!(paged["nextOffset"], Value::Null);

        let first = theirs.ok(
            &["hooks", "list", "--json", "--project", &their_widget, "--limit", "1"],
            None,
        );
        assert_eq!((&first["hooks"], &first["nextOffset"]), (&json!([shared]), &json!(1)));
        assert_eq!(theirs.ok(&["hooks", "list", "--json"], None)["hooks"], json!([shared]));
        assert_eq!(theirs.ok(&["hooks", "get", &id(2), "--json"], None), project);

        let taken = theirs.refused(&["hooks", "put", "--json"], Some(&global), "conflict");
        assert!(taken.contains("--expect-sha"), "{taken}");

        let sha = shared["sha256"].as_str().expect("sha256").to_owned();
        let renamed = body(&id(1), None, "Renamed");
        let updated = theirs.put(None, &["--expect-sha", &sha], &renamed);
        assert_eq!(
            (&updated["body"], &updated["author"]),
            (&json!(renamed), &json!("grace"))
        );
        assert_eq!(updated["createdAt"], shared["createdAt"]);

        mine.refused(
            &["hooks", "put", "--json", "--expect-sha", &sha],
            Some(&global),
            "conflict",
        );
        mine.refused(
            &["hooks", "put", "--json", "--expect-sha", &sha],
            Some(&body(&id(3), None, "Missing")),
            "notFound",
        );

        mine.refused(
            &["hooks", "delete", &id(1), "--expect-sha", &sha, "--json"],
            None,
            "conflict",
        );
        let current = updated["sha256"].as_str().expect("sha256");
        assert_eq!(
            mine.ok(&["hooks", "delete", &id(1), "--expect-sha", current, "--json"], None),
            json!({ "deleted": true })
        );
        assert_eq!(
            mine.ok(&["hooks", "delete", &id(1), "--json"], None),
            json!({ "deleted": false })
        );
        mine.refused(&["hooks", "get", &id(1), "--json"], None, "notFound");
        assert_eq!(theirs.ok(&["hooks", "list", "--json"], None)["total"], 0);
    });
}

#[test]
fn what_the_team_rules_keep_out_is_refused_or_invalid() {
    each_team(|team| {
        let machine = machine().on(team);

        let seeded = body(&id(1), None, "Seeded").replace(
            "\"updatedBy\": \"desktop\"",
            "\"updatedBy\": \"desktop\",\n  \"seed\": \"memory-search\"",
        );
        let message = machine.refused(&["hooks", "put", "--json"], Some(&seeded), "refused");
        assert!(message.contains("seed"), "{message}");

        let unshared = machine.path(&machine.unshared);
        let unshared_key = format!(
            "path:{}",
            std::fs::canonicalize(&machine.unshared).expect("real").display()
        );
        let message = machine.refused(
            &["hooks", "put", "--json", "--project", &unshared],
            Some(&project_hook(2, &unshared_key)),
            "refused",
        );
        assert!(message.contains("origin remote"), "{message}");

        let gadget = machine.path(&machine.gadget);
        machine.refused(
            &["hooks", "put", "--json", "--project", &gadget],
            Some(&project_hook(3, WIDGET)),
            "refused",
        );
        machine.refused(&["hooks", "put", "--json"], Some(&project_hook(4, WIDGET)), "refused");

        let local_path = body(&id(5), Some("/home/ada/work/widget"), "Local path");
        let widget = machine.path(&machine.widget);
        machine.refused(
            &["hooks", "put", "--json", "--project", &widget],
            Some(&local_path),
            "refused",
        );

        let broken = body(&id(6), None, "Broken").replace("\"sessionStart\"", "\"sometimes\"");
        let message = machine.refused(&["hooks", "put", "--json"], Some(&broken), "invalid");
        assert!(message.contains("unknown event"), "{message}");

        machine.refused(&["hooks", "put", "--json"], Some("not json"), "invalid");
        let huge = format!("{}{}", body(&id(7), None, "Huge"), " ".repeat(16 * 1024));
        machine.refused(&["hooks", "put", "--json"], Some(&huge), "invalid");
        machine.refused(
            &["hooks", "put", "--json", "--author", "a\u{202E}b"],
            Some(&body(&id(8), None, "Author")),
            "invalid",
        );

        assert_eq!(
            machine.ok(&["hooks", "list", "--json", "--project", &widget], None)["total"],
            0
        );
    });
}

#[test]
fn a_full_scope_takes_no_more_hooks() {
    each_team(|team| {
        let machine = machine().on(team);

        for n in 1..=64 {
            machine.put(None, &[], &body(&id(n), None, &format!("Global {n}")));
        }

        let message = machine.refused(&["hooks", "put", "--json"], Some(&body(&id(65), None, "More")), "limit");
        assert!(message.contains("64"), "{message}");

        let widget = machine.widget.clone();
        machine.put(Some(&widget), &[], &project_hook(66, WIDGET));
        assert_eq!(
            machine.ok(&["hooks", "list", "--json", "--limit", "200"], None)["total"],
            64
        );
    });
}

#[test]
fn paging_by_next_offset_passes_rows_left_out() {
    each_team(|team| {
        let machine = machine().on(team);
        let valid = machine.put(None, &[], &body(&id(1), None, "Valid"));

        // Created at 0, so all 32 sort before the valid hook.
        (team.tamper)(32);

        let mut offset = 0;
        let mut pages = 0;
        let mut seen: Vec<Value> = Vec::new();

        loop {
            let at = offset.to_string();
            let page = machine.ok(&["hooks", "list", "--json", "--offset", &at, "--limit", "10"], None);
            assert_eq!(page["total"], 33, "{page}");

            pages += 1;
            seen.extend(page["hooks"].as_array().expect("hooks").iter().cloned());

            match page["nextOffset"].as_u64() {
                Some(next) => offset = next,
                None => break,
            }
        }

        assert_eq!((pages, seen), (4, vec![valid]));
    });
}
