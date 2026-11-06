//! `harness-memory project-key --json`: the project key of folders on standard input, the way the
//! MCP server decides it. Harness Desktop asks it when the user turns a project on. It is a
//! command for a person: the MCP server has no tool for it.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use common::guards::Guarded;
use common::mcp::canonical;
use common::opt_in::{commit, home, worktree};
use common::stdio::{BINARY, without_team_settings};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

fn project_key(guarded: &Guarded, args: &[&str], input: &str) -> Output {
    let mut command = Command::new(BINARY);
    command
        .arg("project-key")
        .args(args)
        .current_dir(guarded.work())
        .env("HOME", guarded.dir.path().join("home"))
        .env("HARNESS_MEMORY_DIR", guarded.data())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    without_team_settings(&mut command);

    let mut child = command.spawn().expect("the executable runs");
    let mut stdin = child.stdin.take().expect("stdin");
    let _ = stdin.write_all(input.as_bytes());
    drop(stdin);
    child.wait_with_output().expect("the executable ends")
}

fn answer(guarded: &Guarded, paths: &Value) -> Vec<Value> {
    let output = project_key(guarded, &["--json"], &paths.to_string());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    serde_json::from_slice(&output.stdout).expect("a JSON array")
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[test]
fn it_answers_each_folder_with_its_root_and_key_in_order() {
    let guarded = Guarded::new();
    commit(&guarded.repo);
    let linked = worktree(&guarded.repo, "linked");
    let deep = guarded.repo.join("src/deep");
    let work = canonical(&guarded.work());
    let nowhere = guarded.work().join("missing");
    let key = "git:github.com/acme/widget";

    let paths = json!([
        text(&guarded.repo),
        text(&deep),
        text(&linked),
        text(&home(&guarded)),
        text(&work),
        text(&nowhere),
        "relative/folder",
    ]);
    let got = answer(&guarded, &paths);
    assert_eq!(got.len(), 7);

    assert_eq!(
        got[0],
        json!({ "path": text(&guarded.repo), "root": text(&guarded.repo), "key": key })
    );
    assert_eq!(
        got[1],
        json!({ "path": text(&deep), "root": text(&guarded.repo), "key": key })
    );
    assert_eq!(
        got[2],
        json!({ "path": text(&linked), "root": text(&linked), "key": key })
    );

    // The home folder has no project; a folder outside a repository is keyed by its path.
    let home = home(&guarded);
    assert_eq!(got[3], json!({ "path": text(&home), "root": text(&home), "key": null }));
    assert_eq!(
        got[4],
        json!({ "path": text(&work), "root": text(&work), "key": format!("path:{}", work.display()) })
    );

    // An unusable path says why and does not stop the others.
    assert_eq!(got[5]["path"], text(&nowhere));
    assert!(
        got[5]["error"].as_str().is_some_and(|error| !error.is_empty()),
        "{}",
        got[5]
    );
    assert!(got[5].get("key").is_none());
    assert_eq!(got[6]["error"], "project_path must be an absolute path");
}

#[test]
fn input_past_a_bound_or_of_another_shape_exits_one_with_a_reason() {
    let guarded = Guarded::new();
    let many = json!(vec![text(&guarded.repo); 513]).to_string();

    for input in [many.as_str(), "{}", "[1]", "not json", ""] {
        let output = project_key(&guarded, &["--json"], input);
        assert_eq!(output.status.code(), Some(1), "{input:.40}");

        let reply: Value = serde_json::from_slice(&output.stdout).expect("JSON");
        assert_eq!(reply["error"], "invalid");
        assert!(!output.stderr.is_empty());
    }

    // 512 paths are within the bound, and a path past the length bound is one error among answers.
    assert_eq!(answer(&guarded, &json!(vec![text(&guarded.repo); 512])).len(), 512);

    let long = format!("/{}", "a".repeat(4096));
    let got = answer(&guarded, &json!([long, text(&guarded.repo)]));
    assert_eq!(got[0]["error"], "a path is longer than 4096 bytes");
    assert_eq!(got[1]["key"], "git:github.com/acme/widget");
}

#[test]
fn a_missing_flag_or_an_extra_argument_is_a_usage_error() {
    let guarded = Guarded::new();

    for args in [&[][..], &["--json", "extra"], &["--other"]] {
        assert_eq!(project_key(&guarded, args, "[]").status.code(), Some(2), "{args:?}");
    }
}
