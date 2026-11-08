//! What a user reads: the team's notes and scripts through the commands the desktop runs, and
//! what is left on this computer, from the local file and the waiting lists directly.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::user::User;

/// One note of a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: i64,
    pub title: String,
    pub revision: i64,
    /// Whether a script is trusted to run here (`None` for a note).
    pub trusted: Option<bool>,
}

/// What `notes list` and `scripts list` showed.
pub struct Reading {
    pub rows: Vec<Row>,
    /// Whether the answer says the team database was down.
    pub offline: bool,
}

impl Reading {
    /// The titles (names), sorted.
    pub fn titles(&self) -> Vec<String> {
        let mut titles: Vec<String> = self.rows.iter().map(|row| row.title.clone()).collect();
        titles.sort();
        titles
    }

    /// The row titled `title`, which must be there exactly once.
    pub fn one(&self, title: &str) -> &Row {
        let found: Vec<&Row> = self.rows.iter().filter(|row| row.title == title).collect();
        assert_eq!(
            found.len(),
            1,
            "{title:?} is listed {} times in {:?}",
            found.len(),
            self.titles()
        );
        found[0]
    }
}

/// Where a list looks.
#[derive(Debug, Clone, Copy)]
pub enum Place {
    Team,
    Local,
}

impl Place {
    fn flag(self) -> &'static str {
        match self {
            Self::Team => "team",
            Self::Local => "local",
        }
    }
}

impl User {
    /// The notes of every repository this user sees in `place`.
    pub fn notes(&self, place: Place) -> Reading {
        let list = self.ok(
            &[
                "notes",
                "list",
                "--json",
                "--all-repos",
                "--limit",
                "200",
                "--place",
                place.flag(),
            ],
            "",
        );

        reading(&list, "notes", "title")
    }

    /// The scripts of every repository this user sees in `place`.
    pub fn scripts(&self, place: Place) -> Reading {
        let list = self.ok(
            &[
                "scripts",
                "list",
                "--json",
                "--all-repos",
                "--limit",
                "200",
                "--place",
                place.flag(),
            ],
            "",
        );

        reading(&list, "scripts", "name")
    }

    /// The titles of the team's notes this user reads.
    pub fn team_titles(&self) -> Vec<String> {
        let reading = self.notes(Place::Team);
        assert!(!reading.offline, "the team database was reachable");
        reading.titles()
    }

    /// The names of the team's scripts this user reads.
    pub fn team_script_names(&self) -> Vec<String> {
        let reading = self.scripts(Place::Team);
        assert!(!reading.offline, "the team database was reachable");
        reading.titles()
    }

    /// The body of team note `id`.
    pub fn body(&self, id: i64) -> String {
        let note = self.ok(&["notes", "get", &id.to_string(), "--json", "--all-repos"], "");
        note["note"]["body"].as_str().expect("a body").to_owned()
    }

    /// Polls `check` until it holds, for at most `limit`; the last failure otherwise.
    pub fn eventually<T>(&self, limit: Duration, mut check: impl FnMut() -> Result<T, String>) -> T {
        let until = Instant::now() + limit;

        loop {
            match check() {
                Ok(value) => return value,
                Err(why) if Instant::now() >= until => panic!("not within {limit:?}: {why}"),
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}

fn reading(list: &Value, key: &str, title: &str) -> Reading {
    let rows = list[key]
        .as_array()
        .unwrap_or_else(|| panic!("a list of {key}: {list}"))
        .iter()
        .map(|row| Row {
            id: row["id"].as_i64().expect("an id"),
            title: row[title].as_str().expect("a title").to_owned(),
            revision: row["revision"].as_i64().unwrap_or_default(),
            trusted: row["trusted"].as_bool(),
        })
        .collect();

    Reading {
        rows,
        offline: list["offline"] == true,
    }
}

/// The notes and scripts in `user`'s local file.
pub fn local_counts(user: &User) -> (usize, usize) {
    let path = user.local_database();

    if !path.is_file() {
        return (0, 0);
    }

    let conn = rusqlite::Connection::open(path).expect("the local file");
    let count = |table: &str| -> usize {
        let counted: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0))
            .expect("count");
        usize::try_from(counted).expect("a count")
    };

    (count("notes"), count("scripts"))
}

/// The titles of the notes in `user`'s local file, sorted.
pub fn local_titles(user: &User) -> Vec<String> {
    column(&user.local_database(), "SELECT title FROM notes ORDER BY title")
}

/// The `(title, body)` of the notes in `user`'s local file, sorted.
pub fn local_notes(user: &User) -> Vec<(String, String)> {
    let path = user.local_database();

    if !path.is_file() {
        return Vec::new();
    }

    let conn = rusqlite::Connection::open(path).expect("the local file");
    let mut statement = conn
        .prepare("SELECT title, body FROM notes ORDER BY title")
        .expect("select");
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("rows")
}

/// The names of the scripts in `user`'s local file, sorted.
pub fn local_script_names(user: &User) -> Vec<String> {
    column(&user.local_database(), "SELECT name FROM scripts ORDER BY name")
}

/// The `(name, body)` of the scripts in `user`'s local file, sorted.
pub fn local_scripts(user: &User) -> Vec<(String, String)> {
    let path = user.local_database();

    if !path.is_file() {
        return Vec::new();
    }

    let conn = rusqlite::Connection::open(path).expect("the local file");
    let mut statement = conn
        .prepare("SELECT name, body FROM scripts ORDER BY name")
        .expect("select");
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("rows")
}

/// The script versions held back from team trust on `user`'s computer, as `(place, id)`: `place` is
/// `local` or the team database's key, and `id` is none for a version held back by its name before
/// it was saved. Sorted; empty when nothing was ever held back.
pub fn held_back(user: &User) -> Vec<(String, Option<i64>)> {
    let path = user.local_database();

    if !path.is_file() {
        return Vec::new();
    }

    let conn = rusqlite::Connection::open(path).expect("the local file");
    let Ok(mut statement) = conn.prepare("SELECT place, script_id FROM script_held_back ORDER BY place, script_id")
    else {
        return Vec::new();
    };

    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("rows")
}

fn column(path: &Path, sql: &str) -> Vec<String> {
    if !path.is_file() {
        return Vec::new();
    }

    let conn = rusqlite::Connection::open(path).expect("the local file");
    let mut statement = conn.prepare(sql).expect("select");
    statement
        .query_map([], |row| row.get(0))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("rows")
}

/// What waits for the team database next to `user`'s local file: the notes and scripts saved
/// while it was down, and the queued changes to team notes.
pub fn waiting(user: &User) -> (usize, usize, usize) {
    let length = |file: &str, key: &str| -> usize {
        let Ok(text) = std::fs::read_to_string(user.file(file)) else {
            return 0;
        };

        let parsed: Value = serde_json::from_str(&text).expect("a waiting list is JSON");
        parsed[key].as_array().map_or(0, Vec::len)
    };

    (
        length("pending-sync.json", "notes"),
        length("pending-sync.json", "scripts"),
        length("pending-updates.json", "updates"),
    )
}

/// Waits for the next whole second, so a write made after it is later than one made before.
pub fn pass_a_second() {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = || SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_secs();
    let started = now();

    while now() <= started {
        std::thread::sleep(Duration::from_millis(50));
    }
}
