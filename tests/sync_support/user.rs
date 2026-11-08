//! One simulated person: a home of their own with a git name, a local memory folder, and a clone of
//! the shared repository, running the real `harness-memory` as an agent's MCP server and as the
//! commands Harness Desktop runs. Two users share one team database and one project (the same
//! `origin`), and nothing else.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::live::Live;
use super::mcp::Agent;
use crate::common::mcp::repo_with_remote;
use crate::common::stdio::{BINARY, without_team_settings};

/// Where a kind of entry is kept, as the settings name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Places {
    pub repository_notes: &'static str,
    pub global_notes: &'static str,
    pub scripts: &'static str,
    /// `remote_local_saves`; `None` leaves it out, as a person who never set it.
    pub remote_local_saves: Option<&'static str>,
}

/// Everything in the team database.
pub const REMOTE: Places = Places {
    repository_notes: "remote",
    global_notes: "remote",
    scripts: "remote",
    remote_local_saves: None,
};

/// Everything read from the team database and saved on this computer (`remote_local_saves`
/// `local`).
pub const REMOTE_LOCAL: Places = Places {
    repository_notes: "remote+local",
    global_notes: "remote+local",
    scripts: "remote+local",
    remote_local_saves: Some("local"),
};

/// Everything read from both and saved to the team database: `remote+local` without
/// `remote_local_saves`, which is what a person who never set it has.
pub const REMOTE_LOCAL_DEFAULT: Places = Places {
    repository_notes: "remote+local",
    global_notes: "remote+local",
    scripts: "remote+local",
    remote_local_saves: None,
};

/// The repository the users share, by its `origin`, named per case.
pub fn origin(case: &str) -> String {
    format!("https://github.com/acme/sync-{case}.git")
}

/// One person on one computer.
pub struct User {
    _root: TempDir,
    pub name: String,
    pub repo: PathBuf,
    pub memory: PathBuf,
    home: PathBuf,
    config: PathBuf,
    settings: Vec<(&'static str, String)>,
    places: Places,
    fallback: bool,
    session: Option<String>,
}

impl User {
    /// `name` with a home, a git name and a clone of `origin`, pointed at `live` (through
    /// `proxy`, a `host:port`, when given).
    pub fn new(name: &str, origin: &str, live: &Live, proxy: Option<&str>) -> Self {
        let root = TempDir::new().expect("temp");
        let home = root.path().join("home");
        std::fs::create_dir_all(&home).expect("home");

        let identity = format!(
            "[user]\n\tname = {name}\n\temail = {}@example.com\n",
            name.to_lowercase()
        );
        std::fs::write(home.join(".gitconfig"), identity).expect("git config");

        let repo = repo_with_remote(&root.path().join("work"), "widget", origin);

        Self {
            name: name.to_owned(),
            repo,
            memory: root.path().join("memory"),
            config: root.path().join("config"),
            home,
            settings: live.settings(proxy),
            places: REMOTE,
            fallback: true,
            session: None,
            _root: root,
        }
    }

    pub fn with_places(mut self, places: Places) -> Self {
        self.places = places;
        self
    }

    /// Changes where this user's notes and scripts are kept (what the user does in the settings).
    pub fn set_places(&mut self, places: Places) {
        self.places = places;
    }

    /// Turns the offline fallback off (`HARNESS_MEMORY_OFFLINE_FALLBACK=0`).
    pub fn without_fallback(mut self) -> Self {
        self.fallback = false;
        self
    }

    /// Starts agents as the chat `key` (a lowercase UUID), whose save rules [`Self::rules_for_chat`]
    /// writes. The user's own commands run outside any chat, as the desktop's do.
    pub fn in_chat(mut self, key: &str) -> Self {
        self.session = Some(key.to_owned());
        self
    }

    /// The executable with `args` in this user's folders and settings.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BINARY);

        command
            .args(args)
            .current_dir(&self.repo)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("APPDATA", &self.config)
            .env("GIT_CONFIG_GLOBAL", self.home.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HARNESS_MEMORY_DIR", &self.memory)
            .env("HARNESS_MEMORY_REPOSITORY_NOTES", self.places.repository_notes)
            .env("HARNESS_MEMORY_GLOBAL_NOTES", self.places.global_notes)
            .env("HARNESS_MEMORY_SCRIPTS", self.places.scripts)
            .env("HARNESS_MEMORY_OFFLINE_FALLBACK", if self.fallback { "1" } else { "0" });

        if let Some(saves) = self.places.remote_local_saves {
            command.env("HARNESS_MEMORY_REMOTE_LOCAL_SAVES", saves);
        }

        for (name, value) in &self.settings {
            command.env(name, value);
        }

        without_team_settings(&mut command);
        command
    }

    /// An agent's `harness-memory mcp` in this user's repository, after the handshake.
    pub fn agent(&self) -> Agent {
        let mut command = self.command(&["mcp"]);

        if let Some(session) = &self.session {
            command.env("HARNESS_MEMORY_SESSION", session);
        }

        Agent::start(command)
    }

    /// Runs `args` with `input` on standard input and waits for the end.
    pub fn run(&self, args: &[&str], input: &str) -> Output {
        let mut command = self.command(args);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().expect("the executable runs");
        let mut stdin = child.stdin.take().expect("stdin");
        let _ignored = stdin.write_all(input.as_bytes());
        drop(stdin);

        wait_within(child, Duration::from_secs(120), args)
    }

    /// The JSON object `args` print, which must exit with `code`.
    pub fn json(&self, args: &[&str], input: &str, code: i32) -> Value {
        let output = self.run(args, input);

        assert_eq!(
            output.status.code(),
            Some(code),
            "{args:?}: stdout {} stderr {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "{args:?} printed one JSON object: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    /// The JSON of a command that succeeds.
    pub fn ok(&self, args: &[&str], input: &str) -> Value {
        self.json(args, input, 0)
    }

    /// Saves a note as this user, from the command line (what the desktop's button runs).
    pub fn save_note(&self, title: &str, body: &str, scope: &str) -> Value {
        let note = json!({ "title": title, "body": body, "kind": "decision", "scope": scope });
        self.ok(&["notes", "save", "--json"], &note.to_string())
    }

    /// The reach file that makes servers and commands keep away from an unreachable team
    /// database for a minute. Removing it is what the end of the minute does.
    pub fn end_cooldown(&self) {
        let _ = std::fs::remove_file(self.memory.join("team-unreachable.json"));
    }

    /// Whether the cooldown file is there.
    pub fn is_cooling(&self) -> bool {
        self.memory.join("team-unreachable.json").exists()
    }

    /// The path of a file in this user's memory folder.
    pub fn file(&self, name: &str) -> PathBuf {
        self.memory.join(name)
    }

    /// The local database file.
    pub fn local_database(&self) -> PathBuf {
        self.file("memory.sqlite3")
    }

    /// Touches `desktop.alive` now: the desktop counts as running for the next 15 seconds.
    pub fn keep_desktop_alive(&self) {
        write_private(&self.memory.join("desktop.alive"), "");
    }

    /// Writes the save rules of chat `key`: notes and scripts on, and Ask when `ask`. With Ask the
    /// desktop has to be running, which `desktop.alive` says.
    pub fn rules_for_chat(&self, key: &str, ask: bool) {
        let guards = self.memory.join("guards");
        let chats = guards.join("chats");

        for folder in [&self.memory, &guards, &chats] {
            private_dir(folder);
        }

        let rules = json!({
            "version": 1,
            "paused": false,
            "notes": { "on": true, "ask": ask },
            "scripts": { "on": true, "ask": ask },
        });
        write_private(&chats.join(format!("{key}.json")), &rules.to_string());
        write_private(&self.memory.join("desktop.alive"), "");
    }
}

/// Waits for `child` and gives its output, or fails the case when it hangs.
fn wait_within(child: Child, limit: Duration, args: &[&str]) -> Output {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ignored = sender.send(child.wait_with_output());
    });

    match receiver.recv_timeout(limit) {
        Ok(output) => output.expect("the executable ends"),
        Err(_) => panic!("{args:?} did not end within {limit:?}"),
    }
}

/// A folder only this user may use, as the desktop makes the guards' folders.
pub fn private_dir(path: &Path) {
    std::fs::create_dir_all(path).expect("folder");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    }
}

/// A file only this user may change.
pub fn write_private(path: &Path, text: &str) {
    std::fs::write(path, text).expect("write");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}
