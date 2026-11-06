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

#[test]
fn a_name_git_could_not_decode_is_still_the_author() {
    assert_eq!(
        Author::stored(Some("Jos\u{fffd}".to_owned()), None),
        author("Jos\u{fffd}", None)
    );
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
            // Windows finds the home folder through USERPROFILE.
            .env("USERPROFILE", &self.home)
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

    /// `harness-memory <args>` in the repository: its exit code and the last line of stderr.
    fn status(&self, args: &[&str]) -> (Option<i32>, String) {
        let mut command = Command::new(BINARY);
        command
            .args(args)
            .current_dir(&self.repo)
            .env("HARNESS_MEMORY_DIR", self.data())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home);
        without_team_settings(&mut command);

        let output = command.output().expect("the executable runs");
        let last = String::from_utf8_lossy(&output.stderr)
            .lines()
            .last()
            .unwrap_or_default()
            .to_owned();

        (output.status.code(), last)
    }

    /// From now on the git `user.name` of the writer is `name` (none for `None`).
    fn be(&self, name: Option<&str>) {
        let config = self.home.join(".gitconfig");

        match name {
            Some(name) => std::fs::write(&config, format!("[user]\n\tname = {name}\n")).expect("gitconfig"),
            None => {
                let _ = std::fs::remove_file(&config);
            }
        }
    }

    /// An MCP server of client `omp` running in the repository.
    fn agent(&self) -> Client {
        let mut command = command(&self.data(), &self.repo);
        command.env("HOME", &self.home).env("USERPROFILE", &self.home);
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

/// The names and counts of an `authors` object: `(people, unknown)`.
fn who(listing: &Value) -> (Vec<(String, u64)>, u64) {
    let authors = &listing["authors"];
    let people = authors["people"]
        .as_array()
        .expect("people")
        .iter()
        .map(|person| {
            (
                person["name"].as_str().expect("name").to_owned(),
                person["count"].as_u64().expect("count"),
            )
        })
        .collect();

    (people, authors["unknown"].as_u64().expect("unknown"))
}

fn people(counts: &[(&str, u64)]) -> Vec<(String, u64)> {
    counts
        .iter()
        .map(|(name, count)| ((*name).to_owned(), *count))
        .collect()
}

/// The values of `field` of every object in the array `key` of `listing`.
fn names<'a>(listing: &'a Value, key: &str, field: &str) -> Vec<&'a str> {
    listing[key]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row[field].as_str().expect("text"))
        .collect()
}

#[test]
fn an_empty_database_lists_nobody() {
    let setup = setup(Some("Ada Lovelace"));

    for command in ["notes", "scripts"] {
        let listed = setup.cli(&[command, "list", "--json"], "");
        assert_eq!(listed["authors"], json!({ "people": [], "unknown": 0 }), "{command}");
    }

    let picked = setup.cli(&["notes", "list", "--json", "--author", "Ada Lovelace"], "");
    assert_eq!(
        picked["authors"],
        json!({ "people": [{ "name": "Ada Lovelace", "count": 0 }], "unknown": 0 }),
        "the name asked for is always listed"
    );
}

#[test]
fn notes_list_filters_sorts_and_counts_by_who_added_them() {
    let setup = setup(Some("Ada Lovelace"));
    let save = |title: &str, kind: &str| {
        setup.cli(
            &["notes", "save", "--json"],
            &json!({ "title": title, "body": "A body", "kind": kind }).to_string(),
        );
    };

    save("Ada fix", "fix");
    save("Ada note", "note");
    setup.be(Some("Grace Hopper"));
    save("Grace fix", "fix");
    setup.be(None);
    save("Nobody note", "note");

    let repo = setup.repo.to_string_lossy().into_owned();
    let list = |extra: &[&str]| {
        let args = [
            &["notes", "list", "--json", "--project", &repo, "--sort", "title"][..],
            extra,
        ]
        .concat();
        setup.cli(&args, "")
    };
    let everyone = people(&[("Ada Lovelace", 2), ("Grace Hopper", 1)]);

    let all = list(&[]);
    assert_eq!(all["total"], 4);
    assert_eq!(who(&all), (everyone.clone(), 1));

    let ada = list(&["--author", "Ada Lovelace"]);
    assert_eq!(names(&ada, "notes", "title"), ["Ada fix", "Ada note"]);
    assert_eq!(ada["total"], 2);
    assert_eq!(ada["scopes"], json!({ "repo": 2, "global": 0 }));
    assert_eq!(who(&ada), (everyone.clone(), 1), "the facet ignores the author");

    // The person is the git name, matched exactly.
    let lower = list(&["--author=ada lovelace"]);
    assert_eq!(lower["total"], 0);
    assert_eq!(
        who(&lower),
        (
            people(&[("Ada Lovelace", 2), ("Grace Hopper", 1), ("ada lovelace", 0)]),
            1
        )
    );

    let padded = list(&["--author", "  Grace Hopper "]);
    assert_eq!(names(&padded, "notes", "title"), ["Grace fix"], "a name is trimmed");

    let unknown = list(&["--author-unknown"]);
    assert_eq!(names(&unknown, "notes", "title"), ["Nobody note"]);
    assert_eq!(unknown["total"], 1);
    assert_eq!(who(&unknown), (everyone.clone(), 1));

    // The other filters narrow the facet, the author does not.
    let fixes = list(&["--kind", "fix", "--author", "Ada Lovelace"]);
    assert_eq!(names(&fixes, "notes", "title"), ["Ada fix"]);
    assert_eq!(who(&fixes), (people(&[("Ada Lovelace", 1), ("Grace Hopper", 1)]), 0));
    assert_eq!(
        fixes["kinds"]
            .as_array()
            .expect("kinds")
            .iter()
            .map(|kind| kind["count"].as_u64().expect("count"))
            .sum::<u64>(),
        2,
        "the counts per kind ignore the kind and follow the author"
    );

    // Everything is kept on this computer, so the team database has nobody to count.
    let team = list(&["--place", "team"]);
    assert_eq!(team["total"], 0);
    assert_eq!(who(&team), (Vec::new(), 0));
    assert_eq!(who(&list(&["--place", "local"])), (everyone.clone(), 1));

    // Sorted by the name, the notes nobody added come last whichever the order.
    let sorted = |order: &str| {
        let listed = setup.cli(
            &[
                "notes",
                "list",
                "--json",
                "--project",
                &repo,
                "--sort",
                "addedBy",
                "--order",
                order,
            ],
            "",
        );
        listed["notes"]
            .as_array()
            .expect("notes")
            .iter()
            .map(|note| note["addedBy"]["name"].as_str().map(str::to_owned))
            .collect::<Vec<_>>()
    };
    let ada = Some("Ada Lovelace".to_owned());
    let grace = Some("Grace Hopper".to_owned());
    assert_eq!(sorted("asc"), [ada.clone(), ada.clone(), grace.clone(), None]);
    assert_eq!(sorted("desc"), [grace, ada.clone(), ada, None]);
    let default = setup.cli(
        &["notes", "list", "--json", "--project", &repo, "--sort", "addedBy"],
        "",
    );
    assert_eq!(
        default["notes"][0]["addedBy"]["name"], "Ada Lovelace",
        "ascending by default"
    );

    // A name that is not usable, and both options together, are usage errors.
    for bad in [
        &["--author", ""][..],
        &["--author", "   "],
        &["--author", "Ada\tLovelace"],
        &["--author", &"x".repeat(81)],
        &["--author"],
        &["--author", "Ada", "--author", "Grace"],
        &["--author", "Ada", "--author-unknown"],
        &["--author-unknown", "--author-unknown"],
        &["--author-unknown=yes"],
    ] {
        let args = [&["notes", "list", "--json"][..], bad].concat();
        assert_eq!(setup.status(&args).0, Some(2), "{bad:?}");
    }
}

#[test]
fn scripts_list_counts_every_group_under_the_author() {
    let setup = setup(Some("Ada Lovelace"));
    let save = |name: &str, scope: &str| {
        setup.cli(
            &["scripts", "save", "--json"],
            &json!({ "name": name, "description": "Does it", "language": "sh", "body": "true", "scope": scope })
                .to_string(),
        );
    };

    save("ada-repo", "repo");
    save("ada-global", "global");
    setup.be(Some("Grace Hopper"));
    save("grace-repo", "repo");
    setup.be(None);
    save("nobody-repo", "repo");
    save("nobody-global", "global");

    let list = |extra: &[&str]| {
        let args = [&["scripts", "list", "--json"][..], extra].concat();
        setup.cli(&args, "")
    };
    let everyone = people(&[("Ada Lovelace", 2), ("Grace Hopper", 1)]);

    let all = list(&[]);
    assert_eq!(all["total"], 5);
    assert_eq!(who(&all), (everyone.clone(), 2));

    // Every count follows the author, only the facet does not.
    for (extra, listed, total) in [
        (&["--author", "Ada Lovelace"][..], vec!["ada-global", "ada-repo"], 2),
        (&["--author", "Grace Hopper"], vec!["grace-repo"], 1),
        (&["--author-unknown"], vec!["nobody-global", "nobody-repo"], 2),
        (&["--author", "ada lovelace"], vec![], 0),
    ] {
        let page = list(extra);
        assert_eq!(names(&page, "scripts", "name"), listed, "{extra:?}");
        assert_eq!(page["total"], total, "{extra:?}");

        let sum = |key: &str| -> u64 {
            page[key]
                .as_object()
                .expect(key)
                .values()
                .map(|n| n.as_u64().expect("n"))
                .sum()
        };
        for key in ["groups", "scopes", "review", "places"] {
            assert_eq!(sum(key), total, "{key} of {extra:?}");
        }

        let (names, unknown) = who(&page);
        assert_eq!(unknown, 2, "{extra:?}");
        assert!(names.starts_with(&everyone), "{extra:?}: {names:?}");
    }

    // The author combines with the other filters; the facet follows them.
    let global = list(&["--scope", "global", "--author", "Ada Lovelace"]);
    assert_eq!(names(&global, "scripts", "name"), ["ada-global"]);
    assert_eq!(who(&global), (people(&[("Ada Lovelace", 1)]), 1));

    let sorted = |order: &str| {
        let listed = list(&["--sort", "addedBy", "--order", order]);
        names(&listed, "scripts", "name")
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        sorted("asc"),
        ["ada-repo", "ada-global", "grace-repo", "nobody-repo", "nobody-global"]
    );
    assert_eq!(
        sorted("desc"),
        ["grace-repo", "ada-global", "ada-repo", "nobody-global", "nobody-repo"]
    );

    for bad in [
        &["--author", ""][..],
        &["--author", "Ada", "--author-unknown"],
        &["--author", "Ada\u{7}"],
    ] {
        let args = [&["scripts", "list", "--json"][..], bad].concat();
        assert_eq!(setup.status(&args).0, Some(2), "{bad:?}");
    }
}

#[test]
fn a_blob_in_an_author_column_never_fails_a_list() {
    let setup = setup(Some("Ada Lovelace"));

    for title in ["Aaa first", "Bbb second"] {
        setup.cli(
            &["notes", "save", "--json"],
            &json!({ "title": title, "body": "A body", "kind": "note" }).to_string(),
        );
    }

    let file = setup.data().join("memory.sqlite3");
    let raw = rusqlite::Connection::open(&file).expect("the database");
    let blobbed = raw
        .execute(
            "UPDATE notes SET added_by_name = X'0102', updated_by_via = X'FF' WHERE title = 'Bbb second'",
            [],
        )
        .expect("a blob");
    assert_eq!(blobbed, 1);

    let repo = setup.repo.to_string_lossy().into_owned();

    // A page off the blob row still counts it, and the page that holds it shows no author.
    for (offset, shown) in [("0", "Aaa first"), ("1", "Bbb second")] {
        let listed = setup.cli(
            &[
                "notes",
                "list",
                "--json",
                "--project",
                &repo,
                "--sort",
                "title",
                "--limit",
                "1",
                "--offset",
                offset,
            ],
            "",
        );
        assert_eq!(names(&listed, "notes", "title"), [shown]);
        assert_eq!(who(&listed), (people(&[("Ada Lovelace", 1)]), 1), "offset {offset}");
    }

    let by_name = setup.cli(
        &["notes", "list", "--json", "--project", &repo, "--sort", "addedBy"],
        "",
    );
    assert_eq!(names(&by_name, "notes", "title"), ["Aaa first", "Bbb second"]);
    assert_eq!(by_name["notes"][1]["addedBy"], Value::Null);

    // The commands that list for their own sake read the same rows.
    let repos = setup.cli(&["notes", "repos", "--json"], "");
    assert_eq!(repos["repos"][0]["count"], 2);
}

#[test]
fn a_git_name_that_is_not_utf8_keeps_who_wrote_notes_and_scripts() {
    let setup = setup(None);
    std::fs::write(setup.home.join(".gitconfig"), b"[user]\n\tname = Jos\xE9\n").expect("gitconfig");
    let name = "Jos\u{fffd}";

    let id = setup.cli(
        &["notes", "save", "--json"],
        r#"{ "title": "Build cache", "body": "Lives in target", "kind": "location" }"#,
    )["id"]
        .as_i64()
        .expect("id");
    assert_eq!(setup.note(id)["addedBy"], person(name, None));

    setup.cli(
        &["scripts", "save", "--json"],
        r#"{ "name": "build", "description": "Builds", "language": "bash", "body": "echo" }"#,
    );

    for section in ["notes", "scripts"] {
        let listing = setup.cli(&[section, "list", "--json", "--all-repos"], "");
        assert_eq!(who(&listing), (people(&[(name, 1)]), 0), "{section}");

        let found = setup.cli(&[section, "list", "--json", "--all-repos", "--author", name], "");
        assert_eq!(found["total"], 1, "{section}");

        let unknown = setup.cli(&[section, "list", "--json", "--all-repos", "--author-unknown"], "");
        assert_eq!(unknown["total"], 0, "{section}");
    }
}
