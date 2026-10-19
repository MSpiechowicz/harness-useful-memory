//! The executable and Amazon DocumentDB: `harness-memory kinds`, and `check` with
//! `database_kind = "documentdb"` (the cases on a real database run the DocumentDB mode against
//! `mongo_support`'s MongoDB server and are skipped without it).

mod common;
mod mongo_support;

use std::path::Path;
use std::process::{Command, Output, Stdio};

use common::stdio::{BINARY, without_team_settings};
use mongo_support::TestDb;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

/// The executable with `args` in a fresh home whose local database folder is `<home>/memory`.
fn binary(home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(BINARY);
    command
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("HARNESS_MEMORY_DIR", home.join("memory"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("APPDATA", home.join("config"));
    command
}

fn run(mut command: Command) -> Output {
    without_team_settings(&mut command);
    command.stdin(Stdio::null()).output().expect("run")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn kinds_lists_the_supported_team_database_kinds() {
    let home = TempDir::new().expect("temp");
    let output = run(binary(home.path(), &["kinds"]));

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "postgres\nlibsql\nmongodb\ndocumentdb\n");
    assert!(output.stderr.is_empty(), "{}", stderr(&output));

    let extra = run(binary(home.path(), &["kinds", "--json"]));
    assert_eq!(extra.status.code(), Some(2));
    assert!(stderr(&extra).contains("| kinds |"), "{}", stderr(&extra));
}

#[test]
fn check_names_an_unreachable_documentdb_cluster_and_points_at_the_vpn() {
    let home = TempDir::new().expect("temp");
    let mut command = binary(home.path(), &["check"]);
    command
        .env(
            "HARNESS_MEMORY_DATABASE_URL",
            "mongodb://alice:hunter2secret@127.0.0.1:1/team?directConnection=true&tls=false&serverSelectionTimeoutMS=300",
        )
        .env("HARNESS_MEMORY_DATABASE_KIND", "documentdb");
    let output = run(command);

    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.starts_with(
            "harness-memory: cannot open the team database (Amazon DocumentDB at mongodb://alice@127.0.0.1:1/team?…): "
        ),
        "{message}"
    );
    assert!(
        message.trim_end().ends_with(
            "Amazon DocumentDB only accepts connections from inside its AWS network: connect to your VPN and try \
             again."
        ),
        "{message}"
    );
    assert!(!message.contains("hunter2secret"), "{message}");
}

#[test]
fn check_refuses_an_unknown_kind() {
    let home = TempDir::new().expect("temp");
    let mut command = binary(home.path(), &["check"]);
    command
        .env("HARNESS_MEMORY_DATABASE_URL", "mongodb://u:hunter2@db.example.com/m")
        .env("HARNESS_MEMORY_DATABASE_KIND", "atlas");
    let output = run(command);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "harness-memory: HARNESS_MEMORY_DATABASE_KIND must be \"mongodb\" or \"documentdb\"\n"
    );
}

#[test]
fn check_opens_the_team_database_as_documentdb_from_the_variable_or_the_config_file() {
    let Some(db) = TestDb::new() else { return };
    let home = TempDir::new().expect("temp");

    let mut by_variable = binary(home.path(), &["check"]);
    by_variable
        .env("HARNESS_MEMORY_DATABASE_URL", &db.url)
        .env("HARNESS_MEMORY_DATABASE_KIND", "DocumentDB");
    let output = run(by_variable);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let said = stdout(&output);
    assert!(said.starts_with("ok: Amazon DocumentDB at mongodb://"), "{said}");
    assert!(said.ends_with(" (schema 4)\nscripts: team database\n"), "{said}");
    assert!(
        !db.index_names("notes").contains(&"notes_text".to_owned()),
        "the database was set up without a text index"
    );

    let config = home.path().join("config").join("harness-memory");
    std::fs::create_dir_all(&config).expect("mkdir");
    let file = config.join("config.toml");
    std::fs::write(
        &file,
        format!("database_url = \"{}\"\ndatabase_kind = \"documentdb\"\n", db.url),
    )
    .expect("write");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }

    let mut by_file = binary(home.path(), &["check"]);
    by_file.env("HARNESS_MEMORY_CONFIG", &file);
    let output = run(by_file);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("ok: Amazon DocumentDB at mongodb://"),
        "{}",
        stdout(&output)
    );

    let mut as_mongodb = binary(home.path(), &["check"]);
    as_mongodb
        .env("HARNESS_MEMORY_CONFIG", &file)
        .env("HARNESS_MEMORY_DATABASE_URL", &db.url);
    let output = run(as_mongodb);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("ok: MongoDB at mongodb://"),
        "the file's kind does not go with the variable's URL: {}",
        stdout(&output)
    );
}
