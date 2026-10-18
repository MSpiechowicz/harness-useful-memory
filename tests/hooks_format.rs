//! The team hook body rules against Harness Desktop's hook file fixtures (`fixtures/hooks`, copied
//! from the desktop repository's `business/tests/fixtures/hooks`): every `invalid-*` file is
//! invalid (a relative `project` aside: a valid file whose project is no repository key, so it
//! is refused), every `valid-*` file is accepted unless the team rules refuse it (a Harness family
//! hook, a project hook whose `project` is a local path), a credential makes a body invalid, and a
//! project hook is shared only for the repository its `project` names.

use std::path::Path;

use harness_memory::store::hooks::format::{BodyProblem, TeamBody, check_team_body};
use pretty_assertions::assert_eq;

const WIDGET: &str = "git:github.com/acme/widget";

/// `(file name, text)` of the fixtures whose name starts with `prefix`, sorted.
fn fixtures(prefix: &str) -> Vec<(String, String)> {
    let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hooks");
    let mut files: Vec<(String, String)> = std::fs::read_dir(&folder)
        .expect("fixtures")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(prefix) && name.ends_with(".json"))
        .map(|name| {
            let bytes = std::fs::read(folder.join(&name)).expect("read");
            (name, String::from_utf8_lossy(&bytes).into_owned())
        })
        .collect();

    files.sort();
    assert!(!files.is_empty(), "no {prefix} fixtures");
    files
}

#[test]
fn every_invalid_fixture_is_invalid() {
    for (name, text) in fixtures("invalid-") {
        let checked = check_team_body(&text);

        if name == "invalid-project-relative.json" {
            let refused = checked.expect("a valid file").repo_for(Some(WIDGET));
            assert!(matches!(refused, Err(BodyProblem::Refused(_))), "{name}: {refused:?}");
            continue;
        }

        assert!(matches!(checked, Err(BodyProblem::Invalid(_))), "{name}: {checked:?}");
    }
}

#[test]
fn every_valid_fixture_is_shared_unless_the_team_rules_refuse_it() {
    for (name, text) in fixtures("valid-") {
        let checked = check_team_body(&text);

        if name.starts_with("valid-seed-") {
            assert!(matches!(checked, Err(BodyProblem::Refused(_))), "{name}: {checked:?}");
            continue;
        }

        let body = checked.unwrap_or_else(|problem| panic!("{name}: {problem:?}"));
        assert!(text.contains(&format!("\"id\": \"{}\"", body.id)), "{name}");

        match &body.project {
            None => assert_eq!(body.repo_for(Some(WIDGET)), Ok(None), "{name}"),
            Some(local) => {
                assert!(local.starts_with('/'), "{name}: a desktop file holds a local path");
                assert!(
                    matches!(body.repo_for(Some(WIDGET)), Err(BodyProblem::Refused(_))),
                    "{name}"
                );
            }
        }
    }
}

#[test]
fn a_project_hook_is_shared_with_its_repository_key() {
    let (_, text) = fixtures("valid-project-scope").into_iter().next().expect("fixture");
    let shared = text.replace("\"/home/user/work/app\"", &format!("\"{WIDGET}\""));

    let body = check_team_body(&shared).expect("valid");
    assert_eq!(
        body,
        TeamBody {
            id: "1a2b3c4d5e6f70818293a4b5c6d7000b".to_owned(),
            project: Some(WIDGET.to_owned()),
        }
    );
    assert_eq!(body.repo_for(Some(WIDGET)), Ok(Some(WIDGET.to_owned())));
    assert!(matches!(
        body.repo_for(Some("git:github.com/acme/gadget")),
        Err(BodyProblem::Refused(_))
    ));
    assert!(matches!(body.repo_for(None), Err(BodyProblem::Refused(_))));
}

#[test]
fn a_body_with_a_credential_is_invalid() {
    let (_, text) = fixtures("valid-command-session-start")
        .into_iter()
        .next()
        .expect("fixture");
    let value: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let command = value["action"]["command"].as_str().expect("a command").to_owned();

    // Built at run time, so no token-shaped literal is in the repository.
    let token = "ghp_".to_owned() + &"a".repeat(36);
    let leaked = text.replace(
        &command,
        &format!("curl -H 'Authorization: token {token}' https://api.github.com"),
    );

    assert!(matches!(check_team_body(&leaked), Err(BodyProblem::Invalid(_))));
    assert!(check_team_body(&text).is_ok());
}

#[test]
fn a_project_hook_names_the_repository_it_is_shared_for() {
    let project = |key: &str| TeamBody {
        id: "1a2b3c4d5e6f70818293a4b5c6d7000b".to_owned(),
        project: Some(key.to_owned()),
    };

    assert_eq!(project(WIDGET).repo_for(Some(WIDGET)), Ok(Some(WIDGET.to_owned())));

    for (body, identity) in [
        (WIDGET, None),
        (WIDGET, Some("git:github.com/acme/gadget")),
        ("/home/user/work/widget", Some(WIDGET)),
        ("path:/home/user/notes", Some("path:/home/user/notes")),
        (WIDGET, Some("path:/home/user/notes")),
    ] {
        let refused = project(body).repo_for(identity);
        assert!(matches!(refused, Err(BodyProblem::Refused(_))), "{body} {identity:?}");
    }

    let global = TeamBody {
        project: None,
        ..project(WIDGET)
    };
    assert_eq!(global.repo_for(Some(WIDGET)), Ok(None));
    assert_eq!(global.repo_for(None), Ok(None));
}
