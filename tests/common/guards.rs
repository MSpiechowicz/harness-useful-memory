//! A data folder with the save guard files Harness Desktop writes, a repository, and in-process
//! servers for a chat (`HARNESS_MEMORY_SESSION`) or none.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use harness_memory::Server;
use harness_memory::backend::Place;
use harness_memory::mcp::{ALIVE_FILE, GUARDS_FOLDER};
use harness_memory::project::ProjectResolver;
use harness_memory::store::SqliteStore;
use harness_memory::support::Env;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::mcp::{call, canonical, env, repo_with_remote, run};

/// The trust name of the team database [`Guarded::team_server`] reads scripts from.
pub const TEAM_KEY: &str = "team:tests";

/// The chat most tests run as.
pub const CHAT: &str = "1b4e28ba-2fa1-4d3b-a3f5-ef19b5a7633b";

/// A temporary folder with the data folder (`data`), a home, a working folder outside any
/// repository (`work`) and a repository with a remote (`widget`).
pub struct Guarded {
    pub dir: TempDir,
    pub repo: PathBuf,
}

impl Guarded {
    pub fn new() -> Self {
        let dir = TempDir::new().expect("temp");

        for folder in ["data", "home", "work"] {
            private_dir(&dir.path().join(folder));
        }

        let repo = canonical(&repo_with_remote(
            dir.path(),
            "widget",
            "https://github.com/acme/widget.git",
        ));
        Self { dir, repo }
    }

    pub fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    pub fn work(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    pub fn held(&self) -> PathBuf {
        self.data().join("held")
    }

    /// A server for chat `session` (none when `None`) started in `cwd`, after `initialize` as
    /// client `omp`.
    pub fn server(&self, session: Option<&str>, cwd: &Path) -> Server {
        let open = || SqliteStore::open(&self.data().join("memory.sqlite3")).expect("store");
        let projects = ProjectResolver::new(self.env(session), Some(cwd.to_path_buf()));
        initialized(Server::new(
            Box::new(open()),
            Box::new(open()),
            Box::new(open()),
            projects,
        ))
    }

    /// A server for chat `session` started in `cwd` whose scripts are in a team database (the
    /// SQLite file [`Self::team_file`] standing in for one) with `trust_team_scripts` on; notes
    /// and script trust stay in the local file. `script_run` waits 300 ms for an answer.
    pub fn team_server(&self, session: Option<&str>, cwd: &Path) -> Server {
        let local = || SqliteStore::open(&self.data().join("memory.sqlite3")).expect("store");
        let team = SqliteStore::open(&self.team_file()).expect("team");
        let env = self.env(session).with_var("HARNESS_MEMORY_APPROVAL_WAIT_MS", "300");
        let projects = ProjectResolver::new(env, Some(cwd.to_path_buf()));
        let server = Server::new(Box::new(local()), Box::new(team), Box::new(local()), projects)
            .with_scripts_place(Place::Remote)
            .with_team_key(TEAM_KEY.to_owned())
            .with_trust_team_scripts(true);
        initialized(server)
    }

    /// The SQLite file standing in for the team database of [`Self::team_server`].
    pub fn team_file(&self) -> PathBuf {
        self.data().join("team.sqlite3")
    }

    fn env(&self, session: Option<&str>) -> Env {
        let mut env = env(&self.dir.path().join("home")).with_var("HARNESS_MEMORY_DIR", self.data().to_string_lossy());

        if let Some(session) = session {
            env = env.with_var("HARNESS_MEMORY_SESSION", session);
        }

        env
    }

    /// A server for [`CHAT`] in the repository, after writing the chat's file with every kind on
    /// and no Ask when it has none.
    pub fn chat_server(&self) -> Server {
        let file = self
            .data()
            .join(GUARDS_FOLDER)
            .join("chats")
            .join(format!("{CHAT}.json"));

        if !file.exists() {
            self.rules(false, (true, false), (true, false));
        }

        self.server(Some(CHAT), &self.repo)
    }

    /// Writes `guards/global.json`.
    pub fn global(&self, text: &str) {
        let guards = self.data().join(GUARDS_FOLDER);
        private_dir(&guards);
        private_file(&guards.join("global.json"), text);
    }

    /// Writes the file of chat `key`.
    pub fn chat(&self, key: &str, text: &str) {
        let chats = self.data().join(GUARDS_FOLDER).join("chats");
        private_dir(&self.data().join(GUARDS_FOLDER));
        private_dir(&chats);
        private_file(&chats.join(format!("{key}.json")), text);
    }

    /// Writes the file of [`CHAT`] with these rules.
    pub fn rules(&self, paused: bool, notes: (bool, bool), scripts: (bool, bool)) {
        let text = json!({
            "version": 1,
            "paused": paused,
            "notes": { "on": notes.0, "ask": notes.1 },
            "scripts": { "on": scripts.0, "ask": scripts.1 },
        });
        self.chat(CHAT, &text.to_string());
    }

    /// Touches `desktop.alive` now.
    pub fn desktop_alive(&self) {
        let path = self.data().join(ALIVE_FILE);
        std::fs::write(&path, "").expect("alive");
        let file = std::fs::File::options().write(true).open(&path).expect("open");
        file.set_modified(SystemTime::now()).expect("mtime");
    }

    /// The parsed held files of every chat (`held/<chatKey>/<heldId>.json`).
    pub fn held_files(&self) -> Vec<Value> {
        let Ok(chats) = std::fs::read_dir(self.held()) else {
            return Vec::new();
        };

        chats
            .map(|chat| chat.expect("entry").path())
            .filter(|chat| chat.is_dir())
            .flat_map(|chat| std::fs::read_dir(chat).expect("chat folder"))
            .map(|entry| entry.expect("entry").path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
            .map(|path| serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("JSON"))
            .collect()
    }
}

/// `server` after `initialize` as client `omp`.
fn initialized(mut server: Server) -> Server {
    run(
        &mut server,
        &[json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
                  "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "omp" } } })],
    );
    server
}

/// Calls tool `name` once.
pub fn tool(server: &mut Server, name: &str, arguments: Value) -> Value {
    run(server, &[call(1, name, arguments)]).remove(0)
}

/// The `structuredContent` of a reply.
pub fn structured(reply: &Value) -> Value {
    reply["result"]["structuredContent"].clone()
}

/// A folder that only this user may use.
pub fn private_dir(path: &Path) {
    std::fs::create_dir_all(path).expect("mkdir");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    }
}

/// A file that only this user may change.
pub fn private_file(path: &Path, text: &str) {
    std::fs::write(path, text).expect("write");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}
