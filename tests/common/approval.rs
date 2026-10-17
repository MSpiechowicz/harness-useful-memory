//! A data folder for the approval tests, a server on it, and Harness Desktop played by a thread
//! that answers the run requests.

// Most of these tests run only on Unix, so on Windows their helpers and imports are unused.
#![cfg_attr(not(unix), allow(unused))]

use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use harness_memory::Server;
use harness_memory::mcp::{ALIVE_FILE, APPROVAL_WAIT_VAR, REQUESTS_FOLDER};
use harness_memory::project::ProjectResolver;
use harness_memory::store::{Language, Provenance, ScriptDraft, ScriptStore, SqliteStore, Target};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::mcp::{call, env, is_error, run, text};

/// A temporary folder holding the data folder (`data`, as `HARNESS_MEMORY_DIR`), a home and a
/// working directory outside any repository.
pub struct Fixture {
    pub dir: TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = TempDir::new().expect("temp");
        for folder in ["data", "home", "work"] {
            std::fs::create_dir_all(dir.path().join(folder)).expect("mkdir");
        }

        Self { dir }
    }

    pub fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    /// The server's working directory, outside any repository.
    pub fn work(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    pub fn requests(&self) -> PathBuf {
        self.data().join(REQUESTS_FOLDER)
    }

    /// A server on `data/memory.sqlite3` that waits `wait_ms` for an answer, after `initialize`.
    pub fn server(&self, wait_ms: u64) -> Server {
        self.server_as(wait_ms, "tests")
    }

    /// [`Self::server`] for a client that calls itself `client`.
    pub fn server_as(&self, wait_ms: u64, client: &str) -> Server {
        self.server_over(wait_ms, client, Box::new(self.store()))
    }

    /// The store on `data/memory.sqlite3`.
    pub fn store(&self) -> SqliteStore {
        SqliteStore::open(&self.data().join("memory.sqlite3")).expect("store")
    }

    /// [`Self::server_as`] with its scripts in `scripts`.
    pub fn server_over(&self, wait_ms: u64, client: &str, scripts: Box<dyn ScriptStore>) -> Server {
        let env = env(&self.dir.path().join("home"))
            .with_var("HARNESS_MEMORY_DIR", self.data().to_string_lossy())
            .with_var(APPROVAL_WAIT_VAR, wait_ms.to_string());
        let projects = ProjectResolver::new(env, Some(self.work()));
        let mut server = Server::new(Box::new(self.store()), scripts, Box::new(self.store()), projects);

        run(
            &mut server,
            &[json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
                      "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": client } } })],
        );
        server
    }

    /// Saves global script `name` straight into the store, as a teammate's machine would; its id.
    pub fn save_elsewhere(&self, name: &str, body: &str) -> i64 {
        self.save_into("memory.sqlite3", name, body, Provenance::default())
    }

    /// Saves global sh script `name` straight into `data/<file>`, written by `provenance`; its id.
    pub fn save_into(&self, file: &str, name: &str, body: &str, provenance: Provenance) -> i64 {
        let mut store = SqliteStore::open(&self.data().join(file)).expect("store");
        let draft = ScriptDraft {
            name: name.to_owned(),
            description: "From a teammate".to_owned(),
            language: Language::Sh,
            body: body.to_owned(),
            timeout_secs: 10,
            provenance,
        };

        store
            .save_script(&Target::Global, &draft)
            .expect("saved")
            .script
            .summary
            .id
    }

    /// Touches `desktop.alive`, dated `age` ago.
    pub fn desktop_alive(&self, age: Duration) {
        let path = self.data().join(ALIVE_FILE);
        std::fs::write(&path, "").expect("alive");
        let file = std::fs::File::options().write(true).open(&path).expect("open");
        file.set_modified(SystemTime::now() - age).expect("mtime");
    }

    /// The files left in the requests folder.
    pub fn leftovers(&self) -> Vec<String> {
        match std::fs::read_dir(self.requests()) {
            Ok(entries) => entries
                .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Plays Harness Desktop: waits for one request, answers `decision`, and returns the request
    /// with the file's and the folder's Unix modes.
    pub fn answer(&self, decision: &'static str) -> JoinHandle<(Value, u32, u32)> {
        let requests = self.requests();

        thread::spawn(move || {
            let (path, request) = wait_for_request(&requests);
            let id = request["requestId"].as_str().expect("requestId").to_owned();
            let modes = (mode(&path), mode(&requests));

            let temporary = requests.join(format!(".{id}.tmp"));
            std::fs::write(&temporary, decision).expect("write decision");
            std::fs::rename(&temporary, requests.join(format!("{id}.decision"))).expect("rename");

            (request, modes.0, modes.1)
        })
    }
}

/// The first `*.json` request in `requests`, parsed, within 10 s.
pub fn wait_for_request(requests: &Path) -> (PathBuf, Value) {
    let started = Instant::now();

    while started.elapsed() < Duration::from_secs(10) {
        let found = std::fs::read_dir(requests)
            .into_iter()
            .flatten()
            .flatten()
            .find(|entry| entry.path().extension().is_some_and(|extension| extension == "json"));

        if let Some(entry) = found {
            let text = std::fs::read_to_string(entry.path()).expect("read request");
            return (entry.path(), serde_json::from_str(&text).expect("a request is JSON"));
        }

        thread::sleep(Duration::from_millis(20));
    }

    panic!("no request appeared in {}", requests.display());
}

#[cfg(unix)]
pub fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[cfg(not(unix))]
pub fn mode(_path: &Path) -> u32 {
    0
}

pub fn tool(server: &mut Server, name: &str, arguments: Value) -> Value {
    run(server, &[call(1, name, arguments)]).remove(0)
}

pub fn ok(server: &mut Server, name: &str, arguments: Value) -> Value {
    let reply = tool(server, name, arguments);
    assert!(!is_error(&reply), "{name}: {}", text(&reply));
    reply["result"]["structuredContent"].clone()
}

/// The `structuredContent` of a refusal, which must name no hash.
pub fn refusal(reply: &Value) -> Value {
    assert!(is_error(reply), "{reply}");
    let structured = reply["result"]["structuredContent"].clone();
    let keys: Vec<&str> = structured
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["untrusted", "id", "name", "pending"], "{reply}");
    assert!(!text(reply).contains("sha"), "{}", text(reply));
    structured
}

pub fn approval(server: &mut Server, name: &str) -> Value {
    ok(server, "script_get", json!({ "name": name }))["script"]["approval"].clone()
}
