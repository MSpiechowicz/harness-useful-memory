//! The rules a team hook body follows: Harness Desktop's hook file format, version 1
//! (`docs/hooks-format.md` in the desktop repository, ported rule for rule), and the two team
//! rules.
//!
//! [`check_team_body`] checks everything the format lists except the project path, and refuses a
//! Harness family hook (`seed` set). [`TeamBody::repo_for`] then checks a project hook's
//! `project` against the repository it is shared for: it holds that repository's `git:` key, not a
//! local path, so a hook of a repository without an `origin` remote (a `path:` key) is never
//! shared. A body that breaks the format is [`BodyProblem::Invalid`], one the team rules keep out
//! is [`BodyProblem::Refused`]. The rules hooks share with routines are in
//! [`crate::store::team_docs::fields`].

use serde_json::{Map, Value};

use crate::store::team_docs::DocKind;
use crate::store::team_docs::fields::{
    check_enabled, check_id, check_labels, check_length, check_scope, check_version, check_written, object,
    optional_str, required_str,
};
pub use crate::store::team_docs::{BodyProblem, TeamBody};
use crate::support::redact::has_credentials;

/// Longest `addContext` or `followUp` text, in characters.
const TEXT_MAX: usize = 4096;

/// Longest `command`, in characters.
const COMMAND_MAX: usize = 2048;

/// Longest `matcher`, in characters.
const MATCHER_MAX: usize = 200;

/// Largest `timeoutSecs`.
const TIMEOUT_MAX: u64 = 30;

/// The events, as the file names them.
const EVENTS: [&str; 5] = ["sessionStart", "promptSubmit", "toolEnd", "turnEnd", "sessionEnd"];

/// The requirements a hook may list.
const REQUIREMENTS: [&str; 2] = ["harness-memory", "harness-dashboard"];

/// The Harness family hooks.
const SEEDS: [&str; 3] = ["memory-search", "memory-save", "dashboard-limits"];

impl TeamBody {
    /// The repository the hook is stored for when it is shared from the repository `identity`
    /// (see [`TeamBody::repo_in`]).
    pub fn repo_for(&self, identity: Option<&str>) -> Result<Option<String>, BodyProblem> {
        self.repo_in(DocKind::Hooks, identity)
    }
}

/// The hook in `body` when it is a valid version 1 hook file (its `project` aside, see
/// [`TeamBody::repo_for`]) that is no Harness family hook and holds no credential.
pub fn check_team_body(body: &str) -> Result<TeamBody, BodyProblem> {
    let invalid = |reason: &str| BodyProblem::invalid(DocKind::Hooks, reason);

    let map = object(body).map_err(|reason| invalid(&reason))?;
    let checked = check_map(&map).map_err(|reason| invalid(&reason))?;

    if has_credentials(body) {
        return Err(invalid(
            "it holds a credential: let the command read secrets from environment variables instead",
        ));
    }

    if checked.seeded {
        return Err(BodyProblem::Refused(
            "Harness family hooks (seed) are never shared: every computer adds its own".to_owned(),
        ));
    }

    Ok(checked.body)
}

/// A map [`check_map`] accepted.
struct Checked {
    body: TeamBody,
    seeded: bool,
}

/// Every rule of the format, in the desktop's order, but `project` only as a string.
fn check_map(map: &Map<String, Value>) -> Result<Checked, String> {
    check_version(map)?;
    let id = check_id(map)?;
    check_labels(map)?;
    let project = check_scope(map, DocKind::Hooks)?;

    let event = required_str(map, "event")?;
    if !EVENTS.contains(&event) {
        return Err(format!("unknown event {event:?}"));
    }

    if let Some(matcher) = optional_str(map, "matcher")? {
        check_matcher(event, matcher)?;
    }

    let action = check_action(map.get("action"))?;
    check_combination(event, action)?;
    check_enabled(map)?;
    check_requires(map.get("requires"))?;

    let seeded = match optional_str(map, "seed")? {
        None => false,
        Some(seed) if SEEDS.contains(&seed) => true,
        Some(seed) => return Err(format!("unknown seed {seed:?}")),
    };

    check_written(map)?;

    Ok(Checked {
        body: TeamBody {
            id: id.to_owned(),
            project,
        },
        seeded,
    })
}

/// `name *( "|" name )`, `name = 1*[A-Za-z0-9_.:-]`, at most [`MATCHER_MAX`], `toolEnd` only.
fn check_matcher(event: &str, matcher: &str) -> Result<(), String> {
    if event != "toolEnd" {
        return Err("only a toolEnd hook has a matcher".to_owned());
    }

    if matcher.chars().count() > MATCHER_MAX {
        return Err(format!("matcher is longer than {MATCHER_MAX} characters"));
    }

    let allowed = |character: char| character.is_ascii_alphanumeric() || "_.:-".contains(character);
    let valid = matcher
        .split('|')
        .all(|name| !name.is_empty() && name.chars().all(allowed));

    if !valid {
        return Err("matcher must be tool names joined by |".to_owned());
    }

    Ok(())
}

/// The action's type (`addContext`, `followUp` or `command`) once its fields are valid.
fn check_action(action: Option<&Value>) -> Result<&str, String> {
    let Some(action) = action else {
        return Err("action is missing".to_owned());
    };

    let Value::Object(action) = action else {
        return Err("action must be an object".to_owned());
    };

    let kind = action
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "action.type is missing".to_owned())?;

    let field = |name: &str, max: usize| -> Result<(), String> {
        let text = action
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("action.{name} is missing"))?;

        check_length(&format!("action.{name}"), text, max)
    };

    match kind {
        "addContext" | "followUp" => field("text", TEXT_MAX)?,
        "command" => {
            field("command", COMMAND_MAX)?;

            match action.get("timeoutSecs") {
                None | Some(Value::Null) => {}
                Some(value) if value.as_u64().is_some_and(|secs| (1..=TIMEOUT_MAX).contains(&secs)) => {}
                Some(_) => {
                    return Err(format!(
                        "action.timeoutSecs must be a whole number from 1 to {TIMEOUT_MAX}"
                    ));
                }
            }
        }
        other => return Err(format!("unknown action type {other:?}")),
    }

    Ok(kind)
}

/// Which actions an event takes (the table in the format doc).
fn check_combination(event: &str, action: &str) -> Result<(), String> {
    let allowed = match action {
        "addContext" => matches!(event, "sessionStart" | "promptSubmit"),
        "followUp" => matches!(event, "toolEnd" | "turnEnd"),
        _ => true,
    };

    if !allowed {
        return Err(format!("a {event} hook cannot use this action"));
    }

    Ok(())
}

fn check_requires(value: Option<&Value>) -> Result<(), String> {
    let items = match value {
        None | Some(Value::Null) => return Ok(()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err("requires must be a list".to_owned()),
    };

    let mut seen: Vec<&str> = Vec::new();

    for item in items {
        let Some(name) = item.as_str() else {
            return Err("requires must list strings".to_owned());
        };

        if !REQUIREMENTS.contains(&name) {
            return Err(format!("unknown requirement {name:?}"));
        }

        if seen.contains(&name) {
            return Err("requires lists a requirement twice".to_owned());
        }

        seen.push(name);
    }

    Ok(())
}
