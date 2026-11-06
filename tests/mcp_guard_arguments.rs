//! `arguments` of `tools/call` must be a JSON object. A derived struct also parses a JSON array
//! as positional fields, which a guard that reads `project_path` by name does not see: an array
//! naming project `O` in its last position would be checked against the chat root alone and write
//! in `O` while saving is off there. The answer is invalid params, whatever the tool.

mod common;

use std::path::Path;

use common::guards::{CHAT, Guarded, tool};
use common::mcp::{call, canonical, repo_with_remote, run, text};
use common::opt_in::{global, kind, note, refusal, script};
use harness_memory::Server;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

const INVALID_PARAMS: i64 = -32602;

/// A chat or a working folder on project `E`, and a project `O` that saving is off for, which
/// holds a note, a relation and a script the user saved before turning `O` off.
struct Setup {
    guarded: Guarded,
    on: std::path::PathBuf,
    off: std::path::PathBuf,
}

impl Setup {
    fn new() -> Self {
        let guarded = Guarded::new();
        let off = canonical(&repo_with_remote(
            guarded.dir.path(),
            "other",
            "https://github.com/acme/other.git",
        ));
        let on = guarded.repo.clone();

        // The user had `O` on and saved in it.
        let both = kind(false, false, &[&on, &off]);
        global(&guarded, &both, &both);
        let mut seed = guarded.server(None, &off);
        let at = json!(off.to_string_lossy());

        let mut saved = note("Seeded");
        saved["project_path"] = at.clone();
        assert_eq!(refusal(&tool(&mut seed, "note_save", saved)), None);

        let link = json!({ "from": "cache", "relation": "located_in", "to": "src", "project_path": at });
        tool(&mut seed, "entity_link", link);

        let mut script = script("named");
        script["project_path"] = at;
        assert_eq!(refusal(&tool(&mut seed, "script_save", script)), None);

        // Then turned it off.
        let only = kind(false, false, &[&on]);
        global(&guarded, &only, &only);

        Self { guarded, on, off }
    }

    /// A server for a chat rooted in the project that is on, or for none (started in it).
    fn server(&self, chat: bool) -> Server {
        if !chat {
            return self.guarded.server(None, &self.on);
        }

        let root = json!({ "version": 1, "paused": false, "repoRoot": self.on.to_string_lossy(),
                           "notes": { "on": true, "ask": false }, "scripts": { "on": true, "ask": false } });
        self.guarded.chat(CHAT, &root.to_string());
        self.guarded.server(Some(CHAT), &self.on)
    }

    /// What `O` holds: its notes, relations and scripts, read as the user would.
    fn held_by_off(&self) -> Vec<String> {
        let mut reader = self.guarded.server(None, &self.guarded.work());
        let at = json!(self.off.to_string_lossy());
        let reads = [
            (
                "note_search",
                json!({ "query": "cache", "scope": "repo", "project_path": at }),
            ),
            ("entity_neighbors", json!({ "entity": "cache", "project_path": at })),
            ("script_list", json!({ "scope": "repo", "project_path": at })),
        ];

        reads
            .into_iter()
            .map(|(name, arguments)| text(&tool(&mut reader, name, arguments)).to_owned())
            .collect()
    }
}

/// The seven write tools, each as an array of positional fields naming `project` last.
fn arrays(project: &Path) -> Vec<(&'static str, Value)> {
    let at = json!(project.to_string_lossy());
    let body = "The cache lives in src/cache.rs";
    let sh = "echo hi";

    vec![
        ("note_save", json!(["Array", body, "location", null, null, "repo", at])),
        (
            "note_update",
            json!([1, null, "OVERWRITTEN", null, null, null, null, at]),
        ),
        ("note_delete", json!([1, null, at])),
        ("entity_link", json!(["aa", "located_in", "bb", null, null, at])),
        ("entity_delete", json!(["cache", null, null, null, null, at])),
        ("script_save", json!([null, "array", "Says hi", "sh", sh, null, at])),
        ("script_delete", json!([null, "named", null, at])),
    ]
}

fn invalid_params(reply: &Value) -> bool {
    reply["error"]["code"] == json!(INVALID_PARAMS) && reply.get("result").is_none()
}

#[test]
fn an_array_naming_a_project_that_is_off_changes_nothing_in_any_write_tool() {
    for chat in [true, false] {
        let setup = Setup::new();
        let before = setup.held_by_off();
        assert!(
            before[0].contains("Seeded") && before[2].contains("named"),
            "{before:?}"
        );

        let mut server = setup.server(chat);

        for (name, arguments) in arrays(&setup.off) {
            let reply = tool(&mut server, name, arguments);
            assert!(invalid_params(&reply), "{name} chat={chat}: {reply}");
        }

        assert_eq!(setup.held_by_off(), before, "chat={chat}");
    }
}

#[test]
fn a_script_saved_by_an_array_is_not_there_to_run() {
    let setup = Setup::new();
    let marker = setup.guarded.dir.path().join("ran");
    let mut server = setup.server(true);

    let save = json!([
        null,
        "array",
        "Says hi",
        "sh",
        format!("touch {}", marker.display()),
        null,
        setup.off.to_string_lossy()
    ]);
    assert!(invalid_params(&tool(&mut server, "script_save", save)));

    let run_it = tool(
        &mut server,
        "script_run",
        json!({ "name": "array", "project_path": setup.off.to_string_lossy() }),
    );
    assert!(common::mcp::is_error(&run_it), "{run_it}");
    assert!(!marker.exists());
}

#[test]
fn a_read_tool_gets_the_same_error() {
    let setup = Setup::new();
    let mut server = setup.server(true);
    let at = json!(setup.off.to_string_lossy());

    let reads = [
        ("note_search", json!(["cache", "repo", at])),
        ("script_list", json!([at])),
        ("script_get", json!(["named", at])),
        ("entity_neighbors", json!(["cache", at])),
    ];

    for (name, arguments) in reads {
        let reply = tool(&mut server, name, arguments);
        assert!(invalid_params(&reply), "{name}: {reply}");
    }
}

#[test]
fn a_string_a_number_and_a_boolean_are_refused_too() {
    let setup = Setup::new();
    let mut server = setup.server(true);

    for arguments in [json!("note_save"), json!(1), json!(true), json!(false), json!(2.5)] {
        for name in ["note_save", "note_search"] {
            let reply = tool(&mut server, name, arguments.clone());
            assert!(invalid_params(&reply), "{name} {arguments}: {reply}");
        }
    }
}

#[test]
fn a_call_with_no_arguments_or_null_still_works_for_a_tool_that_needs_none() {
    let setup = Setup::new();
    let mut server = setup.server(true);

    let none = json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "script_list" } });
    let null = call(6, "script_list", Value::Null);

    for message in [none, null] {
        let reply = run(&mut server, &[message]).remove(0);
        assert!(reply.get("error").is_none(), "{reply}");
        assert!(!common::mcp::is_error(&reply), "{reply}");
    }
}

#[test]
fn the_object_form_still_works() {
    let setup = Setup::new();
    let mut server = setup.server(true);

    let mut saved = note("Object form");
    saved["project_path"] = json!(setup.on.to_string_lossy());
    assert_eq!(refusal(&tool(&mut server, "note_save", saved)), None);

    // The same call aimed at the project that is off is still refused by the guard.
    let mut aimed = note("Object form off");
    aimed["project_path"] = json!(setup.off.to_string_lossy());
    assert_eq!(
        refusal(&tool(&mut server, "note_save", aimed)).as_deref(),
        Some("repo-off")
    );
}
