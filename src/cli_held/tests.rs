use serde_json::{Value, json};

use super::*;
use crate::backend::Place;
use crate::project::ProjectResolver;
use crate::store::{Language, Provenance, ScriptDraft, ScriptStore, SqliteStore, Target};

/// The approval `script_get` shows for script `name`.
fn approval(server: &mut Server, name: &str) -> Value {
    let call = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                       "params": { "name": "script_get", "arguments": { "name": name } } });
    let reply = server.handle(call).expect("reply");
    reply["result"]["structuredContent"]["script"]["approval"].clone()
}

#[test]
fn a_held_script_waits_for_approval_while_every_team_script_is_trusted() {
    let dir = tempfile::tempdir().expect("temp");
    let path = dir.path().join("memory.sqlite3");
    let open = || SqliteStore::open(&path).expect("store");
    let projects = ProjectResolver::new(Env::for_tests(dir.path()), Some(dir.path().to_path_buf()));

    // One file stands in for the team database the scripts are read from.
    let server = Server::new(Box::new(open()), Box::new(open()), Box::new(open()), projects)
        .with_scripts_place(Place::Remote)
        .with_trust_team_scripts(true);
    let mut saver = Saver {
        server,
        secrets: Secrets::default(),
    };

    let arguments = json!({ "name": "held", "description": "", "language": "sh", "body": "echo held",
                            "scope": "global" });
    let item = HeldItem::new(
        "1b4e28ba-2fa1-4d3b-a3f5-ef19b5a7633b",
        "omp",
        HeldTool::ScriptSave,
        arguments,
        Value::Null,
    );
    let saved = saved_item(&mut saver, &item, None).expect("saved");
    assert_eq!(saved, json!({ "kind": "script", "name": "held" }));

    let teammate = ScriptDraft {
        name: "teammate".to_owned(),
        description: String::new(),
        language: Language::Sh,
        body: "echo team".to_owned(),
        timeout_secs: 10,
        provenance: Provenance::default(),
    };
    open().save_script(&Target::Global, &teammate).expect("team script");

    assert_eq!(approval(&mut saver.server, "held"), "needed");
    assert_eq!(approval(&mut saver.server, "teammate"), "team");
}
