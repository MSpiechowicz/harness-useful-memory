//! Who added a note or a script and who changed it last: the git `user.name` of the person (never
//! the email), found in the folder of the writer or else in the global configuration, and shown as
//! `addedBy` and `updatedBy` by the note and script commands, with the MCP client as `via` for an
//! agent's write and none for the user's own.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::mcp::repo_with_remote;
use common::stdio::{BINARY, Client, command, without_team_settings};
use harness_memory::store::{Author, Authors};
use harness_memory::support::Env;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tempfile::TempDir;

/// A home folder whose global git configuration names `name` and an email (none for `None`).
fn home_of(parent: &Path, folder: &str, name: Option<&str>) -> PathBuf {
    let home = parent.join(folder);
    std::fs::create_dir_all(&home).expect("mkdir");

    if let Some(name) = name {
        std::fs::write(
            home.join(".gitconfig"),
            format!(
                "[user]\n\tname = {name}\n\temail = {}@example.com\n",
                name.replace(' ', ".")
            ),
        )
        .expect("gitconfig");
    }

    home
}

fn author(name: &str, via: Option<&str>) -> Option<Author> {
    Some(Author {
        name: name.to_owned(),
        via: via.map(str::to_owned),
    })
}

fn env_of(home: &Path) -> Env {
    common::mcp::env(home)
}

#[test]
fn the_name_is_the_git_user_name_asked_in_the_writers_folder() {
    let dir = TempDir::new().expect("temp");
    let home = home_of(dir.path(), "ada", Some("Ada Lovelace"));
    let repo = repo_with_remote(dir.path(), "widget", "https://github.com/acme/widget.git");
    let env = env_of(&home);

    assert_eq!(
        Author::resolve(&env, Some(&repo), Some("claude-code")),
        author("Ada Lovelace", Some("claude-code"))
    );

    // The user's own saves have no client, and neither do Harness Desktop's.
    assert_eq!(Author::resolve(&env, Some(&repo), None), author("Ada Lovelace", None));
    assert_eq!(
        Author::resolve(&env, Some(&repo), Some("harness-desktop")),
        author("Ada Lovelace", None)
    );

    // Without a folder the global configuration still applies.
    assert_eq!(Author::resolve(&env, None, None), author("Ada Lovelace", None));

    // The repository's own configuration wins where it names someone.
    let other = repo_with_remote(dir.path(), "gadget", "https://github.com/acme/gadget.git");
    common::mcp::git(&other, &["config", "user.name", "Grace Hopper"]);
    assert_eq!(Author::resolve(&env, Some(&other), None), author("Grace Hopper", None));
}

#[test]
fn without_a_usable_name_there_is_no_author() {
    let dir = TempDir::new().expect("temp");
    let nobody = home_of(dir.path(), "nobody", None);
    assert_eq!(
        Author::resolve(&env_of(&nobody), Some(dir.path()), Some("claude-code")),
        None
    );

    let long = home_of(dir.path(), "long", Some(&"x".repeat(81)));
    assert_eq!(Author::resolve(&env_of(&long), Some(dir.path()), None), None);

    let eighty = home_of(dir.path(), "eighty", Some(&"y".repeat(80)));
    assert_eq!(
        Author::resolve(&env_of(&eighty), Some(dir.path()), None).map(|author| author.name.len()),
        Some(80)
    );

    // A control character in the name, written the way git escapes it.
    let tab = home_of(dir.path(), "tab", Some("Ada\\tLovelace"));
    assert_eq!(Author::resolve(&env_of(&tab), Some(dir.path()), None), None);
}

#[test]
fn a_stored_author_is_cleaned_again_when_read() {
    assert_eq!(
        Author::stored(Some("  Ada  ".to_owned()), Some("omp".to_owned())),
        author("Ada", Some("omp"))
    );
    assert_eq!(Author::stored(None, Some("omp".to_owned())), None);
    assert_eq!(Author::stored(Some("Ada\u{1b}[31m".to_owned()), None), None);
    assert_eq!(Authors::default().added_by, None);
}

/// The folders of the commands below: a data folder, the home the git configuration is read from,
/// and a repository.
struct Setup {
    dir: TempDir,
    home: PathBuf,
    repo: PathBuf,
}

fn setup(name: Option<&str>) -> Setup {
    let dir = TempDir::new().expect("temp");
    let home = home_of(dir.path(), "home", name);
    let repo = repo_with_remote(dir.path(), "widget", "https://github.com/acme/widget.git");
    std::fs::create_dir_all(dir.path().join("data")).expect("mkdir");

    Setup { dir, home, repo }
}

impl Setup {
    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    /// `harness-memory <args>` in the repository with `input` on standard input.
    fn cli(&self, args: &[&str], input: &str) -> Value {
        use std::io::Write;

        let mut command = Command::new(BINARY);
        command
            .args(args)
            .current_dir(&self.repo)
            .env("HARNESS_MEMORY_DIR", self.data())
            .env("HOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        without_team_settings(&mut command);

        let mut child = command.spawn().expect("the executable runs");
        let mut stdin = child.stdin.take().expect("stdin");
        let _ = stdin.write_all(input.as_bytes());
        drop(stdin);

        let output: Output = child.wait_with_output().expect("the executable ends");
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let shown = String::from_utf8(output.stdout).expect("utf-8");
        assert!(!shown.contains("@example.com"), "an email was printed: {shown}");

        serde_json::from_str(&shown).expect("JSON")
    }

    /// An MCP server of client `omp` running in the repository.
    fn agent(&self) -> Client {
        let mut command = command(&self.data(), &self.repo);
        command.env("HOME", &self.home);
        Client::start(command)
    }

    fn note(&self, id: i64) -> Value {
        let repo = self.repo.to_string_lossy().into_owned();
        self.cli(&["notes", "get", &id.to_string(), "--json", "--project", &repo], "")["note"].clone()
    }
}

fn person(name: &str, via: Option<&str>) -> Value {
    json!({ "name": name, "via": via })
}

#[test]
fn notes_show_who_added_and_who_changed_them_last() {
    let setup = setup(Some("Ada Lovelace"));
    let body = r#"{ "title": "Build cache", "body": "Lives in target", "kind": "location" }"#;

    // The user's own save from the command line, and the desktop app's.
    let plain = setup.cli(&["notes", "save", "--json"], body)["id"]
        .as_i64()
        .expect("id");
    let note = setup.note(plain);
    assert_eq!(note["addedBy"], person("Ada Lovelace", None));
    assert_eq!(note["updatedBy"], person("Ada Lovelace", None));
    assert_eq!(
        note["author"]["source"], "unknown",
        "the existing author object is unchanged"
    );

    let desktop = setup.cli(
        &["notes", "save", "--json", "--client", "harness-desktop"],
        r#"{ "title": "From the app", "body": "Saved in the app", "kind": "note" }"#,
    )["id"]
        .as_i64()
        .expect("id");
    assert_eq!(setup.note(desktop)["addedBy"], person("Ada Lovelace", None));
    assert_eq!(setup.note(desktop)["author"]["source"], "harness-desktop");

    // An agent's save through MCP names its client.
    let mut agent = setup.agent();
    let saved = agent.call(
        "note_save",
        json!({ "title": "Release steps", "body": "Tag then publish", "kind": "decision", "project_path": setup.repo }),
    );
    let id: i64 = saved
        .strip_prefix("created note ")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|digits| digits.parse().ok())
        .unwrap_or_else(|| panic!("a created note: {saved}"));
    assert_eq!(setup.note(id)["addedBy"], person("Ada Lovelace", Some("tests")));
    assert_eq!(setup.note(id)["updatedBy"], person("Ada Lovelace", Some("tests")));

    // The user changes the agent's note: it stays added by the agent's client.
    setup.cli(
        &[
            "notes",
            "update",
            &id.to_string(),
            "--json",
            "--client",
            "harness-desktop",
        ],
        r#"{ "body": "Tag, then publish" }"#,
    );
    assert_eq!(setup.note(id)["addedBy"], person("Ada Lovelace", Some("tests")));
    assert_eq!(setup.note(id)["updatedBy"], person("Ada Lovelace", None));

    // And the list shows both too.
    let repo = setup.repo.to_string_lossy().into_owned();
    let listed = setup.cli(&["notes", "list", "--json", "--project", &repo], "");
    let row = listed["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .find(|row| row["id"] == id)
        .expect("listed");
    assert_eq!(row["addedBy"], person("Ada Lovelace", Some("tests")));
    assert_eq!(row["updatedBy"], person("Ada Lovelace", None));
    drop(agent);
}

#[test]
fn notes_have_null_authors_without_a_git_name() {
    let setup = setup(None);
    let id = setup.cli(
        &["notes", "save", "--json"],
        r#"{ "title": "Build cache", "body": "Lives in target", "kind": "location" }"#,
    )["id"]
        .as_i64()
        .expect("id");

    let note = setup.note(id);
    assert_eq!(note["addedBy"], Value::Null);
    assert_eq!(note["updatedBy"], Value::Null);
}

#[test]
fn scripts_show_who_added_and_who_changed_them_last() {
    let setup = setup(Some("Ada Lovelace"));
    setup.cli(
        &["scripts", "save", "--json"],
        r#"{ "name": "lint", "description": "Lints", "language": "sh", "body": "just lint" }"#,
    );
    setup.cli(
        &["scripts", "save", "--json", "--client", "harness-desktop"],
        r#"{ "name": "lint", "description": "Lints the code", "language": "sh", "body": "just lint --all" }"#,
    );

    let listed = setup.cli(&["scripts", "list", "--json"], "");
    let script = &listed["scripts"][0];

    assert_eq!(script["name"], "lint");
    assert_eq!(script["addedBy"], person("Ada Lovelace", None));
    assert_eq!(script["updatedBy"], person("Ada Lovelace", None));
    assert_eq!(script["author"]["source"], "harness-desktop");
}
