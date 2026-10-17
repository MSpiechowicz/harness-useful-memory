//! Trust belongs to one script, not to its code: saving the code of a script that waits for
//! approval (under another name, in another language, over the same name, or from the other
//! script store) leaves the copy waiting too, while code an agent writes itself runs (by design).
//! Approvals stay with the team database they were given for, run requests show only cleaned
//! client and writer names, and a client that hangs up while the user is asked runs nothing.

// Most of these tests run only on Unix, so on Windows their helpers and imports are unused.
#![cfg_attr(not(unix), allow(unused))]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::approval::{Fixture, approval, ok, refusal, tool, wait_for_request};
use common::mcp::text;
use common::stdio::{BINARY, without_team_settings};
use harness_memory::backend::Place;
use harness_memory::mcp::APPROVAL_WAIT_VAR;
use harness_memory::store::{Provenance, SqliteStore};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

fn save(name: &str, language: &str, body: &str) -> Value {
    json!({ "scope": "global", "name": name, "description": "", "language": language, "body": body })
}

#[test]
#[cfg(unix)]
fn saving_the_code_of_an_unapproved_script_does_not_trust_it() {
    let fixture = Fixture::new();
    let marker = fixture.dir.path().join("ran");
    let body = format!("touch '{}'", marker.display());
    let id = fixture.save_elsewhere("foreign", &body);
    let mut server = fixture.server(200);

    // Under another name: the copy waits for approval too, and the original is still refused.
    let copy = tool(&mut server, "script_save", save("mine", "sh", &body));
    assert_eq!(copy["result"]["isError"], false, "{}", text(&copy));
    assert_eq!(copy["result"]["structuredContent"]["script"]["approval"], "needed");
    assert!(
        text(&copy).ends_with(&format!(
            "\nit needs the user's approval before it runs: it is the code of script {id} foreign, which is not \
             approved on this machine"
        )),
        "{}",
        text(&copy)
    );
    refusal(&tool(&mut server, "script_run", json!({ "name": "mine" })));
    refusal(&tool(&mut server, "script_run", json!({ "name": "foreign" })));

    // In another language, and saved again over its own name.
    let other_language = ok(&mut server, "script_save", save("mine-bash", "bash", &body));
    assert_eq!(other_language["script"]["approval"], "needed");
    let again = ok(&mut server, "script_save", save("foreign", "sh", &body));
    assert_eq!((&again["script"]["id"], &again["created"]), (&json!(id), &json!(false)));
    assert_eq!(again["script"]["approval"], "needed");

    for name in ["mine-bash", "foreign"] {
        refusal(&tool(&mut server, "script_run", json!({ "name": name })));
    }
    assert!(!marker.exists(), "an unapproved copy ran");

    // Code the agent wrote itself is trusted (by design), also over the unapproved script.
    let fresh = ok(&mut server, "script_save", save("fresh", "sh", "printf fresh"));
    assert_eq!(fresh["script"]["approval"], "local");
    assert_eq!(
        ok(&mut server, "script_run", json!({ "name": "fresh" }))["stdout"],
        "fresh"
    );

    let replaced = ok(&mut server, "script_save", save("foreign", "sh", "printf replaced"));
    assert_eq!(replaced["script"]["approval"], "local");
    assert_eq!(
        ok(&mut server, "script_run", json!({ "name": "foreign" }))["stdout"],
        "replaced"
    );
}

#[test]
#[cfg(unix)]
fn a_copy_of_a_script_waiting_in_the_other_store_is_not_trusted() {
    let fixture = Fixture::new();
    let body = "printf team";
    let team = fixture.data().join("team.sqlite3");
    let id = fixture.save_into("team.sqlite3", "teammate", body, Provenance::default());
    let other = || Box::new(SqliteStore::open(&team).expect("team store"));

    // Scripts stay on this computer; the teammate's script waits in the team database.
    let mut server = fixture.server(200).with_other_scripts(other());
    let copy = tool(&mut server, "script_save", save("copied", "sh", body));
    assert_eq!(copy["result"]["structuredContent"]["script"]["approval"], "needed");
    assert!(
        text(&copy).contains(&format!("it is the code of script {id} teammate")),
        "{}",
        text(&copy)
    );
    refusal(&tool(&mut server, "script_run", json!({ "name": "copied" })));

    // With every team script trusted, nothing waits there.
    let fixture = Fixture::new();
    let team = fixture.data().join("team.sqlite3");
    fixture.save_into("team.sqlite3", "teammate", body, Provenance::default());
    let mut trusting = fixture
        .server(200)
        .with_other_scripts(Box::new(SqliteStore::open(&team).expect("team store")))
        .with_trust_team_scripts(true);
    let copy = ok(&mut trusting, "script_save", save("copied", "sh", body));
    assert_eq!(copy["script"]["approval"], "local");
}

#[test]
fn approvals_stay_with_their_team_database() {
    let fixture = Fixture::new();
    let first = "team:aaaaaaaaaaaaaaaa".to_owned();
    let second = "team:bbbbbbbbbbbbbbbb".to_owned();
    let on = |key: &str| {
        fixture
            .server(200)
            .with_scripts_place(Place::Remote)
            .with_team_key(key.to_owned())
    };

    let mut server = on(&first);
    let saved = ok(&mut server, "script_save", save("shared", "sh", "true"));
    assert_eq!(
        (&saved["script"]["approval"], &saved["script"]["place"]),
        (&json!("local"), &json!("team"))
    );

    // Another team database whose script has the same id and code is not approved by it.
    assert_eq!(approval(&mut on(&second), "shared"), "needed");
    assert_eq!(approval(&mut on(&first), "shared"), "local");
}

#[test]
#[cfg(unix)]
fn a_request_shows_cleaned_client_and_writer_names() {
    let fixture = Fixture::new();
    let writer = Provenance {
        root: Some(PathBuf::from(format!("/work/\u{202E}gpj\n{}", "r".repeat(600)))),
        source: Some(format!("claude\u{1B}[2J\u{200B}code{}", "s".repeat(100))),
        via_override: false,
    };
    fixture.save_into("memory.sqlite3", "foreign", "true", writer);

    let client = format!("evil\u{202E}\nclient{}", "c".repeat(100));
    let mut server = fixture.server_as(10_000, &client);
    fixture.desktop_alive(Duration::ZERO);

    let listed = ok(&mut server, "script_list", json!({}));
    let author = listed["scripts"][0]["author"].clone();
    let source = author["source"].as_str().expect("source");
    let root = author["root"].as_str().expect("root");
    assert!(
        source.starts_with("claude[2Jcode") && source.chars().count() == 64,
        "{source}"
    );
    assert!(root.starts_with("/work/gpj r") && root.chars().count() == 512, "{root}");

    let desktop = fixture.answer("allow");
    ok(&mut server, "script_run", json!({ "name": "foreign" }));
    let (request, _, _) = desktop.join().expect("desktop");

    let shown = request["client"].as_str().expect("client");
    assert!(
        shown.starts_with("evil clientccc") && shown.chars().count() == 64,
        "{shown}"
    );
    assert_eq!(request["script"]["author"], author);
}

#[test]
#[cfg(unix)]
fn a_client_that_hangs_up_while_the_user_is_asked_runs_nothing() {
    let fixture = Fixture::new();
    let marker = fixture.dir.path().join("ran");
    fixture.save_elsewhere("foreign", &format!("touch '{}'", marker.display()));
    fixture.desktop_alive(Duration::ZERO);

    let mut command = Command::new(BINARY);
    command
        .arg("mcp")
        .current_dir(fixture.dir.path().join("work"))
        .env("HARNESS_MEMORY_DIR", fixture.data())
        .env(APPROVAL_WAIT_VAR, "1500");
    without_team_settings(&mut command);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the server starts");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    for message in [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "tests" } } }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "script_run", "arguments": { "name": "foreign" } } }),
    ] {
        writeln!(stdin, "{message}").expect("write");
    }
    stdin.flush().expect("flush");

    let mut line = String::new();
    stdout.read_line(&mut line).expect("initialize answered");
    wait_for_request(&fixture.requests());

    // The client gives up: both pipes close while the server waits for the user.
    drop(stdin);
    drop(stdout);

    let started = Instant::now();
    while child.try_wait().expect("wait").is_none() {
        assert!(started.elapsed() < Duration::from_secs(10), "the server did not exit");
        std::thread::sleep(Duration::from_millis(50));
    }

    assert!(!marker.exists(), "the script ran without an answer");
    assert_eq!(fixture.leftovers(), Vec::<String>::new());
}
