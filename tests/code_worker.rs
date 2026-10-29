//! The worker process of the code index: a repository that exhausts a worker's memory, outlasts its deadline
//! or crashes it is answered with an error while the server lives on, and the worker never outlives its
//! request, nor its server. The hooks that make a worker misbehave exist in debug builds only.

#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use common::mcp::{call, canonical, env, is_error, repo_with_remote, run, text};
use common::stdio::{BINARY, Client};
use harness_memory::Server;
use harness_memory::project::ProjectResolver;
use harness_memory::store::SqliteStore;
use harness_memory::support::runner::Stop;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tempfile::TempDir;

const LIB: &str = "pub fn compute_total(items: &[i32]) -> i32 {\n    items.iter().sum()\n}\n";

const LIMITS: &str = "This repository could not be indexed within the code index limits.";

const STOPPED: &str = "The code index worker was stopped before it finished. Try again.";

struct Machine {
    dir: TempDir,
    repo: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let dir = TempDir::new().expect("temp");

        for folder in ["data", "home"] {
            std::fs::create_dir_all(dir.path().join(folder)).expect("mkdir");
        }

        let repo = canonical(&repo_with_remote(
            dir.path(),
            "widget",
            "https://github.com/acme/widget.git",
        ));

        std::fs::write(repo.join("src/lib.rs"), LIB).expect("lib");

        Self { dir, repo }
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn pid_file(&self) -> PathBuf {
        self.data().join("code-worker.pid")
    }

    /// A server with the extra environment `vars`, whose cancellation is `stop`.
    fn server_with(&self, vars: &[(&str, &str)], stop: &Stop) -> Server {
        let open = || SqliteStore::open(&self.data().join("memory.sqlite3")).expect("store");
        let mut env = env(&self.home())
            .with_var("HARNESS_MEMORY_DIR", self.data().to_string_lossy())
            .with_var("HARNESS_MEMORY_CODE_WORKER_BIN", BINARY)
            // the grace past the budget is twelve seconds in a release build
            .with_var("HARNESS_MEMORY_CODE_GRACE_MS", "1500");

        for (name, value) in vars {
            env = env.with_var(name, *value);
        }

        let mut server = Server::new(
            Box::new(open()),
            Box::new(open()),
            Box::new(open()),
            ProjectResolver::new(env, Some(self.repo.clone())),
        )
        .with_stop(stop.clone());

        run(
            &mut server,
            &[json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
                      "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "omp" } } })],
        );

        server
    }

    fn server(&self, vars: &[(&str, &str)]) -> Server {
        self.server_with(vars, &Stop::default())
    }

    fn symbol(&self, server: &mut Server) -> Value {
        run(
            server,
            &[call(
                1,
                "code_symbol",
                json!({ "project_path": self.repo.to_string_lossy(), "symbol": "compute_total" }),
            )],
        )
        .remove(0)
    }

    /// Changes a file of the repository (its size, so its stamp).
    fn touch(&self) {
        let mut source = std::fs::read_to_string(self.repo.join("src/lib.rs")).expect("read");

        source.push_str("// touched\n");
        std::fs::write(self.repo.join("src/lib.rs"), source).expect("write");
    }

    fn database(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.data().join("code.sqlite3")).expect("code.sqlite3")
    }

    /// (kind, status, error) of the `builds` rows.
    fn builds(&self) -> Vec<(String, String, Option<String>)> {
        let conn = self.database();
        let mut statement = conn
            .prepare("SELECT kind, status, error FROM builds ORDER BY id")
            .expect("prepare");

        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("query")
            .map(|row| row.expect("row"))
            .collect()
    }

    fn failed_mark(&self) -> Option<String> {
        self.database()
            .query_row("SELECT failed FROM repos", [], |row| row.get(0))
            .expect("repos row")
    }

    /// (generation, validated generation) of the repository.
    fn generations(&self) -> (i64, i64) {
        self.database()
            .query_row("SELECT generation, validated_generation FROM repos", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("repos row")
    }

    fn status(&self) -> Value {
        let files: Vec<PathBuf> = std::fs::read_dir(self.data().join("code-builds"))
            .expect("status folder")
            .map(|entry| entry.expect("entry").path())
            .collect();

        assert_eq!(files.len(), 1, "{files:?}");

        serde_json::from_slice(&std::fs::read(&files[0]).expect("status file")).expect("JSON")
    }

    /// The process id the misbehaving worker wrote.
    fn worker_pid(&self) -> u32 {
        std::fs::read_to_string(self.pid_file())
            .expect("the worker started")
            .trim()
            .parse()
            .expect("pid")
    }

    /// Waits for a worker to have written its process id.
    fn wait_for_worker(&self) -> u32 {
        let until = Instant::now() + Duration::from_secs(20);

        while Instant::now() < until {
            if let Some(pid) = std::fs::read_to_string(self.pid_file())
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                return pid;
            }

            std::thread::sleep(Duration::from_millis(10));
        }

        panic!("no worker started");
    }
}

/// True while the process `pid` runs (a zombie is gone).
fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.trim_start().starts_with('Z')),
        Err(_) => Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success()),
    }
}

fn gone_within(pid: u32, limit: Duration) -> bool {
    let until = Instant::now() + limit;

    while Instant::now() < until {
        if !alive(pid) {
            return true;
        }

        std::thread::sleep(Duration::from_millis(20));
    }

    !alive(pid)
}

fn listed(server: &mut Server) -> usize {
    let reply = run(server, &[json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })]).remove(0);

    reply["result"]["tools"].as_array().expect("tools").len()
}

/// A repository on which the worker misbehaves as `hook` says: the server answers the limits error, stays
/// alive, marks the repository, ends the build as an error, fails fast while the files stay the same,
/// retries when a file changes, and indexes it once the worker behaves.
fn a_broken_worker_is_answered_and_the_repository_marked(hook: &str, extra: &[(&str, &str)]) {
    let machine = Machine::new();
    let mut vars = vec![("HARNESS_MEMORY_CODE_WORKER_TEST", hook)];

    vars.extend_from_slice(extra);

    let mut server = machine.server(&vars);
    let reply = machine.symbol(&mut server);

    assert!(is_error(&reply), "{reply}");
    assert_eq!(text(&reply), LIMITS);
    assert!(listed(&mut server) > 0, "the server answers on");
    assert!(
        gone_within(machine.worker_pid(), Duration::from_secs(2)),
        "the worker is gone"
    );

    // the build it started ended as an error, in the table and in the status file
    let builds = machine.builds();

    assert_eq!(builds.len(), 1, "{builds:?}");
    assert_eq!((builds[0].0.as_str(), builds[0].1.as_str()), ("full", "error"));
    assert_eq!(builds[0].2.as_deref(), Some(LIMITS));

    let status = machine.status();

    assert_eq!(status["phase"], "error");
    assert_eq!(status["error"], LIMITS);
    assert!(machine.failed_mark().is_some());

    // the same files: answered at once, no worker
    std::fs::remove_file(machine.pid_file()).expect("pid file");

    let again = machine.symbol(&mut server);

    assert_eq!(text(&again), LIMITS);
    assert!(!machine.pid_file().exists(), "no worker was started");

    // a changed file is a new attempt (which breaks the same way here)
    machine.touch();

    let retried = machine.symbol(&mut server);

    assert_eq!(text(&retried), LIMITS);
    assert!(machine.pid_file().exists(), "a worker was started again");

    // once the worker behaves, another change indexes the repository and takes the mark off
    machine.touch();

    let mut fine = machine.server(&[]);
    let indexed = machine.symbol(&mut fine);

    assert!(!is_error(&indexed), "{indexed}");
    assert!(text(&indexed).contains("src/lib.rs"), "{}", text(&indexed));
    assert!(machine.failed_mark().is_none());
    assert_eq!(machine.generations().0, machine.generations().1);
}

#[test]
fn a_worker_past_its_memory_limit_is_answered_with_the_limits_error() {
    a_broken_worker_is_answered_and_the_repository_marked("alloc", &[("HARNESS_MEMORY_CODE_WORKER_MEMORY_MB", "256")]);
}

#[test]
fn a_worker_that_aborts_is_answered_with_the_limits_error() {
    a_broken_worker_is_answered_and_the_repository_marked("abort", &[]);
}

#[test]
fn a_worker_past_its_deadline_is_killed_and_answered_with_the_limits_error() {
    let started = Instant::now();

    a_broken_worker_is_answered_and_the_repository_marked("spin", &[("HARNESS_MEMORY_CODE_BUDGET_MS", "1000")]);

    // every spinning attempt waits for its budget and the grace, twice
    assert!(started.elapsed() < Duration::from_secs(40), "{:?}", started.elapsed());
}

#[test]
fn a_cancelled_call_kills_its_worker_and_does_not_mark_the_repository() {
    let machine = Machine::new();
    let stop = Stop::default();
    let mut server = machine.server_with(&[("HARNESS_MEMORY_CODE_WORKER_TEST", "spin")], &stop);
    let cancelled = std::thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let pid = machine.wait_for_worker();

            stop.cancel();

            pid
        });
        let reply = machine.symbol(&mut server);
        let pid = waiter.join().expect("waiter");

        (reply, pid)
    });
    let (reply, pid) = cancelled;

    assert!(is_error(&reply), "{reply}");
    assert_eq!(text(&reply), "cancelled");
    assert!(!alive(pid), "no worker is left running");

    let builds = machine.builds();

    assert_eq!(builds.len(), 1, "{builds:?}");
    assert_eq!(builds[0].1, "error");
    assert_eq!(machine.status()["phase"], "error");
    assert!(machine.failed_mark().is_none(), "a cancel is not a failure");

    // the next call goes on
    stop.begin();

    let mut fine = machine.server(&[]);
    let indexed = machine.symbol(&mut fine);

    assert!(!is_error(&indexed), "{indexed}");
}

/// A server process whose worker spins.
fn spinning_server(machine: &Machine) -> Client {
    let mut command = Command::new(BINARY);

    command
        .arg("mcp")
        .current_dir(machine.dir.path())
        .env("HOME", machine.home())
        .env("XDG_CONFIG_HOME", machine.home().join("config"))
        .env("HARNESS_MEMORY_DIR", machine.data())
        .env("HARNESS_MEMORY_CODE_INDEX", "1")
        .env("HARNESS_MEMORY_CODE_GRACE_MS", "1500")
        .env("HARNESS_MEMORY_CODE_WORKER_TEST", "spin");

    let mut client = Client::start(command);

    client.send(&json!({ "jsonrpc": "2.0", "id": 90, "method": "tools/call",
        "params": { "name": "code_symbol", "arguments": {
            "project_path": machine.repo.to_string_lossy(), "symbol": "compute_total" } } }));

    client
}

/// A server process started under `limit` (a `ulimit -t` option and value) whose worker spins past the
/// budget: the server's grace is a minute, so only the processor limit can end the worker.
fn cpu_limited_spinning_server(machine: &Machine, limit: &str) -> Client {
    let mut command = Command::new("sh");

    command
        .arg("-c")
        .arg(format!("ulimit {limit} && exec \"$0\" mcp"))
        .arg(BINARY)
        .current_dir(machine.dir.path())
        .env("HOME", machine.home())
        .env("XDG_CONFIG_HOME", machine.home().join("config"))
        .env("HARNESS_MEMORY_DIR", machine.data())
        .env("HARNESS_MEMORY_CODE_INDEX", "1")
        .env("HARNESS_MEMORY_CODE_BUDGET_MS", "1000")
        .env("HARNESS_MEMORY_CODE_GRACE_MS", "60000")
        .env("HARNESS_MEMORY_CODE_WORKER_TEST", "spin");

    Client::start(command)
}

/// A worker that burns processor time past its limit is the repository's doing, whether the system ended it
/// with SIGXCPU (the user's soft limit, with a hard one above it) or with SIGKILL (a hard limit as low as the
/// soft one): it is answered with the limits error and the repository is marked.
fn a_worker_ended_by_the_processor_limit_marks_the_repository(limit: &str) {
    let machine = Machine::new();
    let mut client = cpu_limited_spinning_server(&machine, limit);
    let started = Instant::now();
    let reply = client.request(
        "tools/call",
        json!({ "name": "code_symbol", "arguments": {
            "project_path": machine.repo.to_string_lossy(), "symbol": "compute_total" } }),
    );

    assert_eq!(reply["result"]["isError"], true, "{reply}");
    assert_eq!(reply["result"]["content"][0]["text"], LIMITS);
    assert!(started.elapsed() < Duration::from_secs(40), "{:?}", started.elapsed());
    assert!(machine.failed_mark().is_some(), "the repository was not marked");
}

#[test]
fn a_worker_killed_by_a_hard_processor_limit_as_low_as_the_soft_one_is_blamed() {
    a_worker_ended_by_the_processor_limit_marks_the_repository("-t 8");
}

#[test]
fn a_worker_ended_by_a_soft_processor_limit_is_blamed() {
    a_worker_ended_by_the_processor_limit_marks_the_repository("-S -t 8");
}

#[cfg(target_os = "linux")]
#[test]
fn the_hard_processor_limit_of_a_worker_is_five_seconds_above_the_soft_one() {
    let machine = Machine::new();
    let worker = Direct::start(&machine, &machine.repo, 60_000, Some("spin"));

    machine.wait_for_worker();

    let limit = proc_field(worker.pid(), "limits", "Max cpu time");
    let mut columns = limit
        .split_whitespace()
        .skip(3)
        .map(|n| n.parse::<u64>().expect("seconds"));
    let (soft, hard) = (columns.next().expect("soft"), columns.next().expect("hard"));

    assert_eq!(hard, soft + 5, "{limit}");
}

#[test]
fn sigterm_to_a_server_waiting_for_its_worker_ends_the_worker_too() {
    let machine = Machine::new();
    let mut client = spinning_server(&machine);
    let worker = machine.wait_for_worker();

    assert!(alive(worker));

    let sent = Command::new("kill")
        .args(["-TERM", &client.pid().to_string()])
        .status()
        .expect("kill");

    assert!(sent.success());
    assert!(
        client.exit_within(Duration::from_secs(4)).is_some(),
        "the server was still busy after SIGTERM"
    );
    assert!(
        gone_within(worker, Duration::from_secs(3)),
        "the worker outlived the server"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_server_that_is_killed_takes_its_worker_with_it() {
    let machine = Machine::new();
    let client = spinning_server(&machine);
    let worker = machine.wait_for_worker();

    assert!(alive(worker));

    let sent = Command::new("kill")
        .args(["-KILL", &client.pid().to_string()])
        .status()
        .expect("kill");

    assert!(sent.success());
    assert!(
        gone_within(worker, Duration::from_secs(5)),
        "the worker outlived the server"
    );
    drop(client);
}

#[cfg(target_os = "linux")]
#[test]
fn the_status_file_of_a_build_that_died_with_its_server_ends_as_an_error_when_another_repository_is_asked() {
    let machine = Machine::new();
    let client = spinning_server(&machine);
    let worker = machine.wait_for_worker();
    let file = harness_memory::code::status::path_of(&machine.data(), &machine.repo);
    let phase = |path: &std::path::Path| -> Value {
        serde_json::from_slice::<Value>(&std::fs::read(path).expect("status file")).expect("JSON")["phase"].clone()
    };

    assert_eq!(phase(&file), "building");

    // the server and its worker die in the middle of the build: the file was written a moment ago
    let sent = Command::new("kill")
        .args(["-KILL", &client.pid().to_string()])
        .status()
        .expect("kill");

    assert!(sent.success());
    assert!(gone_within(worker, Duration::from_secs(5)));
    drop(client);
    assert_eq!(phase(&file), "building", "nothing has noticed yet");

    // another session indexes another repository: it finds the dead build
    let other = canonical(&repo_with_remote(
        machine.dir.path(),
        "other",
        "https://github.com/acme/other.git",
    ));

    std::fs::write(other.join("src/lib.rs"), LIB).expect("lib");

    let mut server = machine.server(&[]);
    let reply = run(
        &mut server,
        &[call(
            3,
            "code_symbol",
            json!({ "project_path": other.to_string_lossy(), "symbol": "compute_total" }),
        )],
    )
    .remove(0);

    assert!(!is_error(&reply), "{reply}");

    let ended = serde_json::from_slice::<Value>(&std::fs::read(&file).expect("status file")).expect("JSON");

    assert_eq!(ended["phase"], "error", "{ended}");
    assert_eq!(ended["error"], "the build was interrupted");
}

#[test]
fn only_a_generation_a_worker_validated_is_loaded_in_the_server() {
    let machine = Machine::new();
    let mut server = machine.server(&[]);
    let first = machine.symbol(&mut server);

    assert!(!is_error(&first), "{first}");

    let (generation, validated) = machine.generations();

    assert_eq!(generation, validated, "the worker validated what the server loaded");

    // a generation nobody validated (a worker that died before it wrote): a server that has to load it
    // asks a worker first, and a worker that breaks leaves it unloaded
    machine
        .database()
        .execute("UPDATE repos SET validated_generation = -1", [])
        .expect("forget");

    let mut broken = machine.server(&[("HARNESS_MEMORY_CODE_WORKER_TEST", "abort")]);
    let refused = machine.symbol(&mut broken);

    assert_eq!(text(&refused), LIMITS);

    // with a worker that behaves the same generation is validated and answered
    machine.touch();

    let mut fine = machine.server(&[]);
    let answered = machine.symbol(&mut fine);

    assert!(!is_error(&answered), "{answered}");
    assert_eq!(machine.generations().0, machine.generations().1);
}

#[test]
fn the_worker_records_how_big_the_graph_it_validated_is_and_a_graph_past_the_cap_is_not_served() {
    let machine = Machine::new();
    let mut server = machine.server(&[]);
    let bytes = |machine: &Machine| -> i64 {
        machine
            .database()
            .query_row("SELECT validated_bytes FROM repos", [], |row| row.get(0))
            .expect("validated_bytes")
    };

    assert!(!is_error(&machine.symbol(&mut server)));
    assert!(bytes(&machine) > 0, "the worker recorded the size of the graph");

    // a graph a worker measured as too big for the server: the server answers an error and loads nothing,
    // even though its cache holds nothing of the repository (a new server)
    machine
        .database()
        .execute("UPDATE repos SET validated_bytes = 5000000000", [])
        .expect("measured");

    let mut fresh = machine.server(&[("HARNESS_MEMORY_CODE_WORKER_TEST", "abort")]);
    let refused = machine.symbol(&mut fresh);

    assert!(is_error(&refused), "{refused}");
    assert_eq!(
        text(&refused),
        "The code graph of this repository is too large to serve."
    );

    // it is the measured size that decides: a smaller one is served again (no worker is needed)
    machine
        .database()
        .execute("UPDATE repos SET validated_bytes = 1000", [])
        .expect("measured");

    let answered = machine.symbol(&mut fresh);

    assert!(!is_error(&answered), "{answered}");
}

#[test]
fn a_repository_the_index_holds_at_a_validated_generation_needs_no_worker() {
    let machine = Machine::new();
    let mut server = machine.server(&[]);

    assert!(!is_error(&machine.symbol(&mut server)));

    // a server that would break any worker it started: it still answers, because none is needed
    let mut second = machine.server(&[("HARNESS_MEMORY_CODE_WORKER_TEST", "abort")]);
    let reply = machine.symbol(&mut second);

    assert!(!is_error(&reply), "{reply}");
    assert!(!machine.pid_file().exists(), "no worker was started");
}

#[test]
fn the_in_process_mode_of_the_unit_tests_starts_no_worker() {
    let machine = Machine::new();

    // a hook that would break any worker, and no executable to start: the server indexes by itself
    let mut server = machine.server(&[
        ("HARNESS_MEMORY_CODE_INPROCESS", "1"),
        ("HARNESS_MEMORY_CODE_WORKER_TEST", "abort"),
        ("HARNESS_MEMORY_CODE_WORKER_BIN", "/nonexistent/harness-memory"),
    ]);
    let reply = machine.symbol(&mut server);

    assert!(!is_error(&reply), "{reply}");
    assert!(text(&reply).contains("src/lib.rs"), "{}", text(&reply));
    assert!(!machine.pid_file().exists(), "no worker was started");
    assert_eq!(machine.generations().1, -1, "nothing was validated");
}

#[test]
fn two_sessions_that_start_workers_for_one_new_repository_share_one_build() {
    let machine = Machine::new();

    for index in 0..300 {
        std::fs::write(
            machine.repo.join(format!("src/deep/g{index}.rs")),
            format!("pub fn generated_{index}() -> i32 {{ {index} }}\n"),
        )
        .expect("write");
    }

    let replies: Vec<Value> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    let mut server = machine.server(&[]);

                    machine.symbol(&mut server)
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("session"))
            .collect()
    });

    for reply in &replies {
        assert!(!is_error(reply), "{reply}");
        assert!(text(reply).contains("src/lib.rs"), "{}", text(reply));
    }

    let builds = machine.builds();

    assert_eq!(builds.len(), 1, "one build, not one per session: {builds:?}");
    assert_eq!((builds[0].0.as_str(), builds[0].1.as_str()), ("full", "ok"));
    assert_eq!(machine.status()["phase"], "ready");
    assert_eq!(machine.generations().0, machine.generations().1);
}

#[test]
fn a_worker_that_dies_after_it_validated_the_generation_still_serves_the_graph() {
    let machine = Machine::new();
    let mut server = machine.server(&[("HARNESS_MEMORY_CODE_WORKER_TEST", "late-abort")]);
    let reply = machine.symbol(&mut server);

    assert!(machine.pid_file().exists(), "the worker ran and aborted");
    assert!(!is_error(&reply), "{reply}");
    assert!(text(&reply).contains("src/lib.rs"), "{}", text(&reply));
    assert!(
        machine.failed_mark().is_none(),
        "nothing is held against the repository"
    );
    assert_eq!(machine.generations().0, machine.generations().1);

    // and the next call needs no worker
    std::fs::remove_file(machine.pid_file()).expect("pid file");

    let again = machine.symbol(&mut server);

    assert!(!is_error(&again), "{again}");
    assert!(!machine.pid_file().exists(), "no worker was started");

    // the same for a repository that is already indexed and changes: the new generation is the one served
    machine.touch();
    std::fs::write(
        machine.repo.join("src/fresh.rs"),
        "pub fn only_in_the_new_generation() {}\n",
    )
    .expect("new file");

    let (before, _) = machine.generations();
    let changed = run(
        &mut server,
        &[call(
            2,
            "code_symbol",
            json!({ "project_path": machine.repo.to_string_lossy(), "symbol": "only_in_the_new_generation" }),
        )],
    )
    .remove(0);

    assert!(!is_error(&changed), "{changed}");
    assert!(text(&changed).contains("src/fresh.rs"), "{}", text(&changed));
    assert!(machine.generations().0 > before);
    assert_eq!(machine.generations().0, machine.generations().1);
}

#[test]
fn a_worker_killed_from_outside_does_not_mark_the_repository() {
    let machine = Machine::new();
    let mut server = machine.server(&[("HARNESS_MEMORY_CODE_WORKER_TEST", "kill")]);
    let reply = machine.symbol(&mut server);

    assert!(is_error(&reply), "{reply}");
    assert_eq!(text(&reply), STOPPED);
    assert!(
        machine.failed_mark().is_none(),
        "the system's SIGKILL says nothing about the repository"
    );

    // the build it started ended as an error, in the table and in the status file
    let builds = machine.builds();

    assert_eq!(builds.len(), 1, "{builds:?}");
    assert_eq!(builds[0].1, "error");
    assert_eq!(machine.status()["phase"], "error");

    // the very next call tries again, with no change of a file
    std::fs::remove_file(machine.pid_file()).expect("pid file");

    let mut fine = machine.server(&[]);
    let indexed = machine.symbol(&mut fine);

    assert!(!is_error(&indexed), "{indexed}");
}

#[test]
fn a_failed_mark_made_under_one_memory_limit_is_tried_again_under_another() {
    let machine = Machine::new();
    let mut small = machine.server(&[
        ("HARNESS_MEMORY_CODE_WORKER_TEST", "alloc"),
        ("HARNESS_MEMORY_CODE_WORKER_MEMORY_MB", "256"),
    ]);

    assert_eq!(text(&machine.symbol(&mut small)), LIMITS);
    assert!(machine.failed_mark().is_some());

    // the same files and the same limit: refused at once, no worker (the hook would break it again)
    std::fs::remove_file(machine.pid_file()).expect("pid file");

    let mut same = machine.server(&[
        ("HARNESS_MEMORY_CODE_WORKER_TEST", "abort"),
        ("HARNESS_MEMORY_CODE_WORKER_MEMORY_MB", "256"),
    ]);

    assert_eq!(text(&machine.symbol(&mut same)), LIMITS);
    assert!(!machine.pid_file().exists(), "no worker was started");

    // a larger limit is a new attempt, and it works
    let mut large = machine.server(&[("HARNESS_MEMORY_CODE_WORKER_MEMORY_MB", "2048")]);
    let indexed = machine.symbol(&mut large);

    assert!(!is_error(&indexed), "{indexed}");
    assert!(machine.failed_mark().is_none());
}

#[test]
fn a_failed_mark_older_than_a_day_is_tried_again() {
    let machine = Machine::new();
    let mut server = machine.server(&[("HARNESS_MEMORY_CODE_WORKER_TEST", "abort")]);

    assert_eq!(text(&machine.symbol(&mut server)), LIMITS);

    std::fs::remove_file(machine.pid_file()).expect("pid file");
    assert_eq!(text(&machine.symbol(&mut server)), LIMITS);
    assert!(!machine.pid_file().exists(), "marked: no worker");

    // the mark is `<walk>|<limit>|<version>|<unix seconds>`: make it two days old
    let mark = machine.failed_mark().expect("mark");
    let (head, _) = mark.rsplit_once('|').expect("time field");
    let old = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        - 2 * 24 * 3600;

    machine
        .database()
        .execute("UPDATE repos SET failed = ?1", [format!("{head}|{old}")])
        .expect("age the mark");

    assert_eq!(text(&machine.symbol(&mut server)), LIMITS);
    assert!(
        machine.pid_file().exists(),
        "an old mark does not hold: a worker was started"
    );
}

#[test]
fn a_mark_from_another_version_of_harness_memory_does_not_hold() {
    let machine = Machine::new();
    let mut server = machine.server(&[("HARNESS_MEMORY_CODE_WORKER_TEST", "abort")]);

    assert_eq!(text(&machine.symbol(&mut server)), LIMITS);

    let mark = machine.failed_mark().expect("mark");
    let parts: Vec<&str> = mark.split('|').collect();

    machine
        .database()
        .execute(
            "UPDATE repos SET failed = ?1",
            [format!("{}|{}|0.0.1|{}", parts[0], parts[1], parts[3])],
        )
        .expect("other version");
    std::fs::remove_file(machine.pid_file()).expect("pid file");

    assert_eq!(text(&machine.symbol(&mut server)), LIMITS);
    assert!(machine.pid_file().exists(), "a worker was started again");
}

/// A `code-worker` started by the test itself, as the server starts it, with its request already written
/// and its standard input still open.
struct Direct {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
}

impl Direct {
    fn start(machine: &Machine, root: &std::path::Path, budget_ms: u64, hook: Option<&str>) -> Self {
        Self::start_after(machine, root, budget_ms, hook, "")
    }

    /// Starts the worker from a shell that first runs `before` (a `ulimit`, say).
    fn start_after(
        machine: &Machine,
        root: &std::path::Path,
        budget_ms: u64,
        hook: Option<&str>,
        before: &str,
    ) -> Self {
        let mut command = Command::new("/bin/sh");

        command
            .arg("-c")
            .arg(format!("{before}exec \"$0\" code-worker"))
            .arg(BINARY)
            .env_clear()
            .env("HOME", machine.home())
            .env("HARNESS_MEMORY_DIR", machine.data())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        if let Some(hook) = hook {
            command.env("HARNESS_MEMORY_CODE_WORKER_TEST", hook);
        }

        let mut child = command.spawn().expect("worker");
        let mut stdin = child.stdin.take().expect("stdin");
        let request = json!({ "root": root.to_string_lossy(), "budget_ms": budget_ms });

        std::io::Write::write_all(&mut stdin, format!("{request}\n").as_bytes()).expect("request");

        Self {
            child,
            stdin: Some(stdin),
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// What the worker printed, once it ended.
    fn report(mut self) -> Value {
        let mut printed = String::new();

        std::io::Read::read_to_string(&mut self.child.stdout.take().expect("stdout"), &mut printed).expect("read");
        self.child.wait().expect("wait");

        serde_json::from_str(printed.lines().last().expect("a report line")).expect("report JSON")
    }
}

impl Drop for Direct {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_worker_refuses_a_folder_that_is_not_a_git_repository_whatever_the_server_asked() {
    let machine = Machine::new();
    let plain = machine.dir.path().join("plain");

    std::fs::create_dir_all(&plain).expect("mkdir");
    std::fs::write(plain.join("lib.rs"), LIB).expect("lib");

    let report = Direct::start(&machine, &plain, 10_000, None).report();

    assert_eq!(report["ok"], false, "{report}");
    assert!(
        report["error"]
            .as_str()
            .expect("error")
            .contains("not a git repository"),
        "{report}"
    );
    assert!(!machine.data().join("code.sqlite3").exists(), "nothing was indexed");

    // a hidden folder of the home folder, even as a git repository
    let hidden = machine.home().join(".config/tool");

    std::fs::create_dir_all(hidden.join(".git")).expect("mkdir");

    let report = Direct::start(&machine, &hidden, 10_000, None).report();

    assert_eq!(report["ok"], false, "{report}");
    assert!(
        report["error"].as_str().expect("error").contains("hidden folder"),
        "{report}"
    );

    // and the folder above the home folder
    let report = Direct::start(&machine, machine.dir.path(), 10_000, None).report();

    assert_eq!(report["ok"], false, "{report}");
    assert!(
        report["error"].as_str().expect("error").contains("system folder"),
        "{report}"
    );

    // a git repository is indexed
    let report = Direct::start(&machine, &machine.repo, 10_000, None).report();

    assert_eq!(report["ok"], true, "{report}");
}

#[cfg(target_os = "linux")]
fn proc_field(pid: u32, file: &str, key: &str) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/{file}"))
        .expect("proc file")
        .lines()
        .find(|line| line.starts_with(key))
        .unwrap_or_else(|| panic!("no {key} in /proc/{pid}/{file}"))
        .to_owned()
}

#[cfg(target_os = "linux")]
#[test]
fn a_worker_asked_for_a_very_long_run_is_held_to_the_servers_budget() {
    let machine = Machine::new();
    let worker = Direct::start(&machine, &machine.repo, u64::MAX, Some("spin"));

    machine.wait_for_worker();

    // the processor limit is the budget (at most two minutes) and ten seconds
    let limit = proc_field(worker.pid(), "limits", "Max cpu time");
    let seconds: u64 = limit
        .split_whitespace()
        .nth(3)
        .expect("soft limit")
        .parse()
        .expect("seconds");

    assert!(seconds <= 130, "{limit}");
}

#[cfg(target_os = "linux")]
#[test]
fn a_lower_soft_limit_of_the_user_is_not_raised_by_the_worker() {
    let machine = Machine::new();
    let worker = Direct::start_after(&machine, &machine.repo, 60_000, Some("spin"), "ulimit -S -t 5; ");

    machine.wait_for_worker();

    let limit = proc_field(worker.pid(), "limits", "Max cpu time");
    let soft: u64 = limit.split_whitespace().nth(3).expect("soft").parse().expect("seconds");

    // the worker's own limit would be 70 seconds
    assert_eq!(soft, 5, "{limit}");
}

#[cfg(target_os = "linux")]
#[test]
fn a_worker_dumps_no_core_and_leaves_no_process_image_to_read() {
    use std::os::unix::fs::MetadataExt;

    let machine = Machine::new();
    let worker = Direct::start(&machine, &machine.repo, 60_000, Some("spin"));

    machine.wait_for_worker();

    let limit = proc_field(worker.pid(), "limits", "Max core file size");

    assert!(
        limit.split_whitespace().skip(4).take(2).all(|field| field == "0"),
        "{limit}"
    );

    // the files of a process that is not dumpable belong to root in /proc
    let owner = std::fs::metadata(format!("/proc/{}/environ", worker.pid()))
        .expect("environ")
        .uid();

    assert_eq!(owner, 0, "the worker is dumpable");
}

#[cfg(target_os = "linux")]
#[test]
fn a_worker_is_stopped_by_sigterm_though_its_server_blocks_it() {
    let machine = Machine::new();
    let client = spinning_server(&machine);
    let worker = machine.wait_for_worker();
    let blocked = proc_field(worker, "status", "SigBlk");
    let mask = u64::from_str_radix(blocked.split_whitespace().nth(1).expect("mask"), 16).expect("hex");

    // SIGINT is signal 2 and SIGTERM signal 15
    assert_eq!(mask & ((1 << 1) | (1 << 14)), 0, "{blocked}");

    drop(client);
}

#[test]
fn a_worker_whose_server_closed_its_input_stops() {
    let machine = Machine::new();
    let mut worker = Direct::start(&machine, &machine.repo, 60_000, Some("spin"));

    machine.wait_for_worker();
    assert!(alive(worker.pid()));

    // the end of the pipe is how a worker knows its server is gone (the only way on Windows)
    drop(worker.stdin.take());

    assert!(gone_within(worker.pid(), Duration::from_secs(15)), "the worker went on");
}
