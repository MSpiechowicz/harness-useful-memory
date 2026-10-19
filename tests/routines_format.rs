//! The team routine body rules against Harness Desktop's routine file fixtures
//! (`fixtures/routines`, copied from the desktop repository's `business/tests/fixtures/routines`,
//! with two files of its own explained in the folder's README): every `invalid-*` file is
//! invalid (two aside: valid files whose project is no repository key, so they are refused),
//! every `valid-*` file is accepted unless its `project` is a local path, a credential makes a
//! body invalid, and a project routine is shared only for the repository its `project` names.
//! The fixtures are picked up by name, so the folder may grow; the inline cases below check the
//! rules on their own.

use std::path::Path;

use harness_memory::store::routines::format::check_team_body;
use harness_memory::store::team_docs::{BodyProblem, DocKind, TeamBody};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

const APP: &str = "git:github.com/acme/app";

/// Invalid fixtures that are valid files whose `project` is not a repository key.
const REFUSED_INVALID: [&str; 2] = ["invalid-project-relative.json", "invalid-team-local-path.json"];

/// `(file name, text)` of the fixtures whose name starts with `prefix`, sorted.
fn fixtures(prefix: &str) -> Vec<(String, String)> {
    let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/routines");
    let Ok(entries) = std::fs::read_dir(&folder) else {
        eprintln!("no routine fixtures in {}", folder.display());
        return Vec::new();
    };

    let mut files: Vec<(String, String)> = entries
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(prefix) && name.ends_with(".json"))
        .map(|name| {
            let bytes = std::fs::read(folder.join(&name)).expect("read");
            (name, String::from_utf8_lossy(&bytes).into_owned())
        })
        .collect();

    files.sort();
    files
}

#[test]
fn every_invalid_fixture_is_invalid() {
    for (name, text) in fixtures("invalid-") {
        let checked = check_team_body(&text);

        if REFUSED_INVALID.contains(&name.as_str()) {
            let refused = checked
                .unwrap_or_else(|problem| panic!("{name}: {problem:?}"))
                .repo_in(DocKind::Routines, Some(APP));
            assert!(matches!(refused, Err(BodyProblem::Refused(_))), "{name}: {refused:?}");
            continue;
        }

        assert!(matches!(checked, Err(BodyProblem::Invalid(_))), "{name}: {checked:?}");
    }
}

#[test]
fn every_valid_fixture_is_shared_unless_its_project_is_a_local_path() {
    for (name, text) in fixtures("valid-") {
        let body = check_team_body(&text).unwrap_or_else(|problem| panic!("{name}: {problem:?}"));
        assert!(text.contains(&format!("\"id\": \"{}\"", body.id)), "{name}");

        match &body.project {
            None => assert_eq!(body.repo_in(DocKind::Routines, Some(APP)), Ok(None), "{name}"),
            Some(key) if key.starts_with("git:") => {
                assert_eq!(
                    body.repo_in(DocKind::Routines, Some(key)),
                    Ok(Some(key.clone())),
                    "{name}"
                );
            }
            Some(local) => {
                assert!(local.starts_with('/'), "{name}: a desktop file holds a local path");
                assert!(
                    matches!(body.repo_in(DocKind::Routines, Some(APP)), Err(BodyProblem::Refused(_))),
                    "{name}"
                );
            }
        }
    }
}

/// A valid global routine, `change` applied to its JSON.
fn routine(change: impl FnOnce(&mut Value)) -> String {
    let mut value = json!({
        "version": 1,
        "id": "3f2a9c1e7b4d4a60b8d5e1c2a9f00001",
        "name": "Morning review",
        "description": null,
        "scope": "global",
        "project": null,
        "when": { "type": "daily", "at": "09:00" },
        "action": { "type": "agent", "harness": "omp", "model": null, "prompt": "Summarize the open pull requests." },
        "enabled": true,
        "createdAt": "2026-10-17T10:00:00Z",
        "createdBy": "desktop",
        "updatedAt": "2026-10-17T10:00:00Z",
        "updatedBy": "agent",
    });

    change(&mut value);
    serde_json::to_string_pretty(&value).expect("JSON")
}

fn is_valid(change: impl FnOnce(&mut Value)) -> bool {
    check_team_body(&routine(change)).is_ok()
}

fn invalid_reason(change: impl FnOnce(&mut Value)) -> String {
    match check_team_body(&routine(change)) {
        Err(BodyProblem::Invalid(message)) => message,
        other => panic!("not invalid: {other:?}"),
    }
}

#[test]
fn every_schedule_kind_has_its_rules() {
    assert!(is_valid(|_| {}));

    for when in [
        json!({ "type": "weekdays", "at": "23:59" }),
        json!({ "type": "everyMinutes", "every": 1 }),
        json!({ "type": "everyMinutes", "every": 720 }),
        json!({ "type": "everyHours", "every": 24 }),
        json!({ "type": "cron", "expression": "*/15 9-17 * jan-jun,sep mon-fri" }),
        json!({ "type": "cron", "expression": "0 0 29 2 *" }),
        json!({ "type": "cron", "expression": "0 0 31 2 1" }),
        json!({ "type": "appStart", "later": true }),
    ] {
        assert!(is_valid(|value| value["when"] = when.clone()), "{when}");
    }

    for when in [
        json!({ "type": "daily" }),
        json!({ "type": "daily", "at": "9:00" }),
        json!({ "type": "weekdays", "at": "24:00" }),
        json!({ "type": "everyMinutes", "every": 0 }),
        json!({ "type": "everyMinutes", "every": 721 }),
        json!({ "type": "everyMinutes", "every": 1.5 }),
        json!({ "type": "everyMinutes", "every": "5" }),
        json!({ "type": "everyHours", "every": 25 }),
        json!({ "type": "cron", "expression": "0 9 * *" }),
        json!({ "type": "cron", "expression": "0 0 30 2 *" }),
        json!({ "type": "cron", "expression": "0 0 31 4,6,9,11 *" }),
        json!({ "type": "cron", "expression": format!("0 0 1 1 {}", "1,".repeat(48) + "1") }),
        json!({ "type": "monthly" }),
        json!("daily"),
    ] {
        assert!(!is_valid(|value| value["when"] = when.clone()), "{when}");
    }

    let never = invalid_reason(|value| value["when"] = json!({ "type": "cron", "expression": "0 0 31 2 *" }));
    assert!(never.starts_with("the routine file is invalid: "), "{never}");
    assert!(never.contains("never runs"), "{never}");
}

#[test]
fn every_action_kind_has_its_rules() {
    let action = |value: Value| move |routine: &mut Value| routine["action"] = value;

    for good in [
        json!({ "type": "agent", "harness": "omp", "prompt": "Hi", "approvalMode": "yolo", "maxMinutes": 240 }),
        json!({ "type": "agent", "harness": "pi", "prompt": "Hi", "trustOnce": true, "model": { "provider": "anthropic", "id": "claude" } }),
        json!({ "type": "agent", "harness": "pi", "prompt": "Line\n\tTwo\r\n", "trustOnce": false }),
        json!({ "type": "command", "command": "make check", "timeoutSecs": 3600 }),
        json!({ "type": "script", "name": "open-prs", "scope": "global" }),
    ] {
        assert!(is_valid(action(good.clone())), "{good}");
    }

    for bad in [
        json!({ "type": "agent", "harness": "opencode", "prompt": "Hi" }),
        json!({ "type": "agent", "harness": "pi", "prompt": "Hi", "approvalMode": "write" }),
        json!({ "type": "agent", "harness": "omp", "prompt": "Hi", "approvalMode": "always-ask" }),
        json!({ "type": "agent", "harness": "omp", "prompt": "Hi", "trustOnce": true }),
        json!({ "type": "agent", "harness": "omp", "prompt": "Hi", "maxMinutes": 241 }),
        json!({ "type": "agent", "harness": "omp", "prompt": "" }),
        json!({ "type": "agent", "harness": "omp", "prompt": "a".repeat(8193) }),
        json!({ "type": "agent", "harness": "omp", "prompt": "Hi\u{200B}" }),
        json!({ "type": "agent", "harness": "omp", "prompt": "Hi\u{0000}" }),
        json!({ "type": "agent", "harness": "omp", "prompt": "Hi", "model": { "provider": "", "id": "x" } }),
        json!({ "type": "agent", "harness": "omp", "prompt": "Hi", "model": "anthropic/claude" }),
        json!({ "type": "command", "command": "a".repeat(2049) }),
        json!({ "type": "command", "command": "echo \u{202E}", "timeoutSecs": 5 }),
        json!({ "type": "command", "command": "make", "timeoutSecs": 0 }),
        json!({ "type": "script", "name": "Open-PRs", "scope": "global" }),
        json!({ "type": "script", "name": "open-prs", "scope": "project" }),
        json!({ "type": "script", "name": "open-prs", "scope": "repo" }),
        json!({ "type": "notify" }),
    ] {
        assert!(!is_valid(action(bad.clone())), "{bad}");
    }

    let in_project = routine(|value| {
        value["scope"] = json!("project");
        value["project"] = json!(APP);
        value["action"] = json!({ "type": "script", "name": "open-prs", "scope": "repo" });
    });
    let body = check_team_body(&in_project).expect("a repo script in a project routine");
    assert_eq!(
        body,
        TeamBody {
            id: "3f2a9c1e7b4d4a60b8d5e1c2a9f00001".to_owned(),
            project: Some(APP.to_owned()),
        }
    );
}

#[test]
fn the_shared_rules_and_reserved_fields_apply() {
    for bad in [
        routine(|value| value["seed"] = json!("memory-save")),
        routine(|value| value["requires"] = json!(["harness-memory"])),
        routine(|value| value["version"] = json!(2)),
        routine(|value| value["name"] = json!("Review\u{202E}")),
        routine(|value| value["createdBy"] = json!("team")),
        routine(|value| value["project"] = json!(APP)),
        routine(|value| value["enabled"] = json!("yes")),
        format!("{}{}", routine(|_| {}), " ".repeat(16 * 1024)),
        "[]".to_owned(),
    ] {
        assert!(matches!(check_team_body(&bad), Err(BodyProblem::Invalid(_))), "{bad}");
    }

    assert!(
        check_team_body(&routine(|value| {
            value["seed"] = Value::Null;
            value["requires"] = Value::Null;
            value["futureField"] = json!({ "keep": true });
        }))
        .is_ok()
    );
}

#[test]
fn a_body_with_a_credential_is_invalid() {
    // Built at run time, so no token-shaped literal is in the repository.
    let token = "ghp_".to_owned() + &"a".repeat(36);

    for leaked in [
        routine(|value| {
            value["action"] = json!({ "type": "command", "command": format!("curl -H 'Authorization: token {token}' https://api.github.com") });
        }),
        routine(|value| value["action"]["prompt"] = json!(format!("Use the token {token} to read the issues."))),
    ] {
        let problem = check_team_body(&leaked).expect_err("a credential");
        assert!(matches!(problem, BodyProblem::Invalid(_)), "{problem:?}");
        assert!(problem.message().contains("credential"), "{problem:?}");
    }
}

#[test]
fn a_project_routine_names_the_repository_it_is_shared_for() {
    let project = |key: &str| TeamBody {
        id: "3f2a9c1e7b4d4a60b8d5e1c2a9f00001".to_owned(),
        project: Some(key.to_owned()),
    };

    assert_eq!(
        project(APP).repo_in(DocKind::Routines, Some(APP)),
        Ok(Some(APP.to_owned()))
    );

    for (body, identity) in [
        (APP, None),
        (APP, Some("git:github.com/acme/other")),
        ("/home/user/work/app", Some(APP)),
        ("path:/home/user/notes", Some("path:/home/user/notes")),
        (APP, Some("path:/home/user/notes")),
    ] {
        let refused = project(body).repo_in(DocKind::Routines, identity);
        assert!(matches!(refused, Err(BodyProblem::Refused(_))), "{body} {identity:?}");
    }

    let message = project(APP)
        .repo_in(DocKind::Routines, None)
        .expect_err("no identity")
        .message();
    assert_eq!(
        message,
        "a project routine is shared for a repository: give its folder with --project"
    );
}
