//! Who may let a script run: no sequence of agent calls runs a script this machine has not
//! approved; Harness Desktop asks the user (allow, deny, no answer) through the request files next
//! to the database; without it the run is refused at once, never naming the trust hash; and
//! `trust_team_scripts` runs every team script. Also the characters a body may hold.

// Most of these tests run only on Unix, so on Windows their helpers and imports are unused.
#![cfg_attr(not(unix), allow(unused))]

mod common;

use std::time::{Duration, Instant};

use common::approval::{Fixture, approval, ok, refusal, tool};
use common::mcp::{is_error, run, text};
use harness_memory::backend::Place;
use harness_memory::mcp::ALIVE_FILE;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

#[test]
#[cfg(unix)]
fn no_sequence_of_agent_calls_runs_an_unapproved_script() {
    let fixture = Fixture::new();
    let marker = fixture.dir.path().join("ran");
    let id = fixture.save_elsewhere("foreign", &format!("touch '{}'", marker.display()));
    let mut server = fixture.server(200);

    let schema = &run(
        &mut server,
        &[json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })],
    )[0];
    let run_tool = schema["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == "script_run")
        .expect("script_run")
        .clone();
    assert!(run_tool["inputSchema"]["properties"].get("sha").is_none());
    assert_eq!(run_tool["inputSchema"]["additionalProperties"], false);

    let got = ok(&mut server, "script_get", json!({ "name": "foreign" }));
    let sha = got["script"]["sha256"].clone();
    assert_eq!(
        (&got["script"]["trusted"], &got["script"]["approval"]),
        (&json!(false), &json!("needed"))
    );

    let first = tool(&mut server, "script_run", json!({ "name": "foreign" }));
    assert_eq!(
        refusal(&first),
        json!({ "untrusted": true, "id": id, "name": "foreign", "pending": false })
    );
    assert_eq!(
        text(&first),
        format!(
            "script foreign is not approved on this machine (written by a teammate, changed, renamed or moved in \
             the team database). Ask the user to review and approve it in Harness Desktop → Memory (or the user \
             can run `harness-memory trust {id}` in a terminal), then call script_run again."
        )
    );

    for arguments in [
        json!({ "name": "foreign", "sha": sha }),
        json!({ "id": id, "sha256": sha }),
        json!({ "name": "foreign", "approve": true }),
        json!({ "name": "foreign", "trusted": true }),
    ] {
        let reply = tool(&mut server, "script_run", arguments.clone());
        assert!(is_error(&reply), "{arguments}");
        assert!(
            text(&reply).starts_with("invalid arguments: unknown field"),
            "{}",
            text(&reply)
        );
    }

    for (name, arguments) in [
        ("script_get", json!({ "id": id, "sha": sha })),
        ("script_list", json!({ "trusted": true })),
        ("script_delete", json!({ "id": id, "force": true })),
    ] {
        let reply = tool(&mut server, name, arguments);
        assert!(
            text(&reply).starts_with("invalid arguments: unknown field"),
            "{}",
            text(&reply)
        );
    }

    for arguments in [json!({ "id": id }), json!({ "name": "foreign", "scope": "global" })] {
        refusal(&tool(&mut server, "script_run", arguments));
    }

    assert!(!marker.exists(), "the script ran");
    assert_eq!(approval(&mut server, "foreign"), "needed");
    assert_eq!(fixture.leftovers(), Vec::<String>::new());
}

#[test]
#[cfg(unix)]
fn the_user_allows_a_run_in_harness_desktop_and_the_version_is_approved() {
    let fixture = Fixture::new();
    let id = fixture.save_elsewhere("foreign", "printf '%s' \"$1\"");
    let mut server = fixture.server(10_000);
    fixture.desktop_alive(Duration::ZERO);

    let desktop = fixture.answer("allow");
    let ran = ok(&mut server, "script_run", json!({ "name": "foreign", "args": ["a b"] }));
    let (request, file_mode, folder_mode) = desktop.join().expect("desktop");

    assert_eq!(ran["stdout"], "a b");
    assert_eq!((file_mode, folder_mode), (0o600, 0o700));

    let request_id = request["requestId"].as_str().expect("id");
    assert_eq!(request_id.len(), 32);
    assert!(
        request_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    let keys: Vec<&str> = request
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "version",
            "requestId",
            "createdAt",
            "client",
            "script",
            "body",
            "language",
            "args",
            "cwd",
            "stdin",
            "previous"
        ]
    );
    assert_eq!(request["version"], 1);
    assert_eq!(request["client"], "tests");
    assert_eq!(request["body"], "printf '%s' \"$1\"");
    assert_eq!(request["language"], "sh");
    assert_eq!(request["args"], json!(["a b"]));
    assert_eq!(request["previous"], Value::Null);
    assert_eq!(
        (
            &request["script"]["id"],
            &request["script"]["place"],
            &request["script"]["approval"]
        ),
        (&json!(id), &json!("local"), &json!("needed"))
    );

    assert_eq!(fixture.leftovers(), Vec::<String>::new());
    assert_eq!(approval(&mut server, "foreign"), "approved");

    // Approved now: it runs without asking, the desktop gone.
    std::fs::remove_file(fixture.data().join(ALIVE_FILE)).expect("desktop quits");
    assert_eq!(ok(&mut server, "script_run", json!({ "id": id }))["exitCode"], 0);
}

#[test]
#[cfg(unix)]
fn the_user_declines_a_run() {
    let fixture = Fixture::new();
    let marker = fixture.dir.path().join("ran");
    let id = fixture.save_elsewhere("foreign", &format!("touch '{}'", marker.display()));
    let mut server = fixture.server(10_000);
    fixture.desktop_alive(Duration::ZERO);

    let desktop = fixture.answer("deny");
    let reply = tool(&mut server, "script_run", json!({ "name": "foreign" }));
    desktop.join().expect("desktop");

    assert_eq!(
        refusal(&reply),
        json!({ "untrusted": true, "id": id, "name": "foreign", "pending": "declined" })
    );
    assert_eq!(text(&reply), "the user declined to run script foreign");
    assert!(!marker.exists());
    assert_eq!(fixture.leftovers(), Vec::<String>::new());
    assert_eq!(approval(&mut server, "foreign"), "needed");
}

#[test]
fn no_answer_times_out_and_leaves_no_files() {
    let fixture = Fixture::new();
    let id = fixture.save_elsewhere("foreign", "true");
    let mut server = fixture.server(300);
    fixture.desktop_alive(Duration::ZERO);

    let started = Instant::now();
    let reply = tool(&mut server, "script_run", json!({ "name": "foreign" }));

    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(
        refusal(&reply),
        json!({ "untrusted": true, "id": id, "name": "foreign", "pending": "timeout" })
    );
    assert_eq!(
        text(&reply),
        "no answer from the user in Harness Desktop; ask the user to approve script foreign in Harness Desktop → \
         Memory, then call script_run again"
    );
    assert!(fixture.requests().is_dir());
    assert_eq!(fixture.leftovers(), Vec::<String>::new());
}

#[test]
fn a_stale_heartbeat_means_no_desktop_and_an_immediate_refusal() {
    let fixture = Fixture::new();
    fixture.save_elsewhere("foreign", "true");
    let mut server = fixture.server(10_000);
    fixture.desktop_alive(Duration::from_secs(60));

    let started = Instant::now();
    let reply = tool(&mut server, "script_run", json!({ "name": "foreign" }));

    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(refusal(&reply)["pending"], false);
    assert!(!fixture.requests().exists(), "nothing was asked");
}

#[test]
#[cfg(unix)]
fn the_request_shows_the_version_approved_before_a_teammate_changed_it() {
    let fixture = Fixture::new();
    let mut server = fixture.server(10_000);
    let saved = ok(
        &mut server,
        "script_save",
        json!({ "scope": "global", "name": "shared", "description": "", "language": "sh", "body": "echo one" }),
    );
    assert_eq!(saved["script"]["approval"], "local");

    let id = fixture.save_elsewhere("shared", "echo two");
    assert_eq!(Some(id), saved["script"]["id"].as_i64());
    fixture.desktop_alive(Duration::ZERO);

    let desktop = fixture.answer("allow");
    assert_eq!(
        ok(&mut server, "script_run", json!({ "name": "shared" }))["stdout"],
        "two\n"
    );
    let (request, _, _) = desktop.join().expect("desktop");

    assert_eq!(request["body"], "echo two");
    assert_eq!(request["previous"]["body"], "echo one");
    assert_eq!(request["previous"]["language"], "sh");
    assert_eq!(request["previous"]["origin"], "saved");
    let trusted_at = request["previous"]["trustedAt"].as_str().expect("trustedAt");
    assert!(trusted_at.len() == 20 && trusted_at.ends_with('Z'), "{trusted_at}");
}

#[test]
#[cfg(unix)]
fn trust_team_scripts_runs_every_team_script() {
    let fixture = Fixture::new();
    fixture.save_elsewhere("team", "echo team");

    let mut trusting = fixture
        .server(200)
        .with_scripts_place(Place::Remote)
        .with_trust_team_scripts(true);
    let listed = ok(&mut trusting, "script_list", json!({}));
    assert_eq!(
        (&listed["scripts"][0]["trusted"], &listed["scripts"][0]["approval"]),
        (&json!(true), &json!("team"))
    );
    assert_eq!(
        ok(&mut trusting, "script_run", json!({ "name": "team" }))["stdout"],
        "team\n"
    );
    assert_eq!(approval(&mut trusting, "team"), "team", "nothing is recorded");

    // Scripts on this computer are not team scripts; without the setting team scripts need approval.
    let mut local = fixture.server(200).with_trust_team_scripts(true);
    assert_eq!(approval(&mut local, "team"), "needed");
    let mut cautious = fixture.server(200).with_scripts_place(Place::Remote);
    refusal(&tool(&mut cautious, "script_run", json!({ "name": "team" })));
}

#[test]
#[cfg(unix)]
fn emoji_bodies_are_accepted_and_bidi_controls_refused() {
    let fixture = Fixture::new();
    let mut server = fixture.server(200);
    let body = "printf '%s' '\u{1F469}\u{200D}\u{1F4BB} \u{2764}\u{FE0F}'";

    ok(
        &mut server,
        "script_save",
        json!({ "scope": "global", "name": "emoji", "description": "", "language": "sh", "body": body }),
    );
    let ran = ok(&mut server, "script_run", json!({ "name": "emoji" }));
    assert_eq!(ran["stdout"], "\u{1F469}\u{200D}\u{1F4BB} \u{2764}\u{FE0F}");

    for hidden in [
        '\u{202E}', '\u{2067}', '\u{200F}', '\u{061C}', '\u{200B}', '\u{200C}', '\u{2060}', '\u{AD}',
    ] {
        let reply = tool(
            &mut server,
            "script_save",
            json!({ "scope": "global", "name": "bidi", "description": "", "language": "sh",
                    "body": format!("echo a{hidden}b") }),
        );
        assert!(is_error(&reply), "U+{:04X}", u32::from(hidden));
        assert!(
            text(&reply).contains("invisible or control character"),
            "{}",
            text(&reply)
        );
    }
}
