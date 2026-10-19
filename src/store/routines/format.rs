//! The rules a team routine body follows: Harness Desktop's routine file format, version 1
//! (`docs/routines-format.md` in the desktop repository, ported rule for rule), and the team rules
//! of hooks.
//!
//! [`check_team_body`] checks everything the format lists except the project path: the rules
//! every file kind shares ([`crate::store::team_docs::fields`]), `when` (the schedule, a cron
//! expression through [`super::cron`]), `action` (an agent prompt, a command, or a saved script),
//! and the reserved `seed` and `requires`, which a routine never sets (routines have no Harness
//! family catalogue), so a body with either is invalid. A body holding a credential is invalid
//! too. [`TeamBody::repo_in`] with [`DocKind::Routines`] then checks a project routine's
//! `project` against the repository it is shared for, as for hooks.

use serde_json::{Map, Value};

use super::cron;
use crate::store::is_valid_name;
use crate::store::team_docs::fields::{
    check_enabled, check_id, check_labels, check_length, check_scope, check_version, check_written, object,
    optional_str, required_str,
};
use crate::store::team_docs::{BodyProblem, DocKind, TeamBody};
use crate::support::redact::has_credentials;
use crate::support::text::{display_safe, is_format_character};

/// Longest cron `expression`, in characters.
const CRON_MAX: usize = 100;

/// Largest `every` of an `everyMinutes` schedule (12 hours).
const EVERY_MINUTES_MAX: u64 = 720;

/// Largest `every` of an `everyHours` schedule.
const EVERY_HOURS_MAX: u64 = 24;

/// Longest agent `prompt`, in characters.
const PROMPT_MAX: usize = 8192;

/// Longest model `provider` or `id`, in characters.
const MODEL_PART_MAX: usize = 200;

/// Largest agent `maxMinutes`.
const MAX_MINUTES_MAX: u64 = 240;

/// Longest `command`, in characters.
const COMMAND_MAX: usize = 2048;

/// Largest command `timeoutSecs`.
const TIMEOUT_MAX: u64 = 3600;

/// The harnesses an agent routine runs in.
const HARNESSES: [&str; 2] = ["omp", "pi"];

/// The approval modes of an OMP agent routine (the desktop's `ApprovalMode`).
const APPROVAL_MODES: [&str; 3] = ["alwaysAsk", "write", "yolo"];

/// The routine in `body` when it is a valid version 1 routine file (its `project` aside, see
/// [`TeamBody::repo_in`]) that holds no credential.
pub fn check_team_body(body: &str) -> Result<TeamBody, BodyProblem> {
    let invalid = |reason: &str| BodyProblem::invalid(DocKind::Routines, reason);

    let map = object(body).map_err(|reason| invalid(&reason))?;
    let checked = check_map(&map).map_err(|reason| invalid(&reason))?;

    if has_credentials(body) {
        return Err(invalid(
            "it holds a credential: let the command or the agent read secrets from environment variables instead",
        ));
    }

    Ok(checked)
}

/// Every rule of the format, but `project` only as a string.
fn check_map(map: &Map<String, Value>) -> Result<TeamBody, String> {
    check_version(map)?;
    let id = check_id(map)?;
    check_labels(map)?;
    let project = check_scope(map, DocKind::Routines)?;
    check_when(object_field(map, "when")?)?;
    check_action(object_field(map, "action")?, project.is_some())?;
    check_enabled(map)?;

    for reserved in ["seed", "requires"] {
        if map.get(reserved).is_some_and(|value| !value.is_null()) {
            return Err(format!("{reserved} is reserved: a routine has none"));
        }
    }

    check_written(map)?;

    Ok(TeamBody {
        id: id.to_owned(),
        project,
    })
}

/// The object at `key`.
fn object_field<'a>(map: &'a Map<String, Value>, key: &str) -> Result<&'a Map<String, Value>, String> {
    match map.get(key) {
        None => Err(format!("{key} is missing")),
        Some(Value::Object(object)) => Ok(object),
        Some(_) => Err(format!("{key} must be an object")),
    }
}

/// The schedule: `daily` or `weekdays` at a time, `everyMinutes` or `everyHours`, a `cron`
/// expression that can match, or `appStart`.
fn check_when(when: &Map<String, Value>) -> Result<(), String> {
    match field_str(when, "when", "type")? {
        "daily" | "weekdays" => {
            if !is_clock_time(field_str(when, "when", "at")?) {
                return Err("when.at must be a time from 00:00 to 23:59, like 09:30".to_owned());
            }
        }
        "everyMinutes" => check_whole(when, "when.every", EVERY_MINUTES_MAX, false)?,
        "everyHours" => check_whole(when, "when.every", EVERY_HOURS_MAX, false)?,
        "cron" => {
            let expression = field_str(when, "when", "expression")?;

            check_length("when.expression", expression, CRON_MAX)?;
            cron::check(expression)?;
        }
        "appStart" => {}
        other => return Err(format!("unknown when type {other:?}")),
    }

    Ok(())
}

/// True for `HH:MM`, 00:00 to 23:59, two digits each.
fn is_clock_time(text: &str) -> bool {
    let bytes = text.as_bytes();

    if bytes.len() != 5 || bytes[2] != b':' {
        return false;
    }

    let two = |at: usize| -> Option<u32> {
        let part = text.get(at..at + 2)?;
        part.bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| part.parse().ok())?
    };

    matches!((two(0), two(3)), (Some(hour), Some(minute)) if hour < 24 && minute < 60)
}

/// What the routine does: ask an agent, run a command, or run a saved script (a `repo` script
/// only in a project routine).
fn check_action(action: &Map<String, Value>, in_project: bool) -> Result<(), String> {
    match field_str(action, "action", "type")? {
        "agent" => check_agent(action),
        "command" => {
            check_runnable(action, "command", COMMAND_MAX)?;
            check_whole(action, "action.timeoutSecs", TIMEOUT_MAX, true)
        }
        "script" => {
            if !is_valid_name(field_str(action, "action", "name")?) {
                return Err(
                    "action.name must be a script name: a-z or 0-9 first, then a-z, 0-9, _ or -, at most 64".to_owned(),
                );
            }

            match field_str(action, "action", "scope")? {
                "global" => Ok(()),
                "repo" if in_project => Ok(()),
                "repo" => Err("a global routine has no repository to run a repo script from".to_owned()),
                other => Err(format!("action.scope must be \"repo\" or \"global\", not {other:?}")),
            }
        }
        other => Err(format!("unknown action type {other:?}")),
    }
}

/// An agent action: `harness`, `model`, `prompt`, `approvalMode` (OMP only), `trustOnce` (PI
/// only) and `maxMinutes`.
fn check_agent(action: &Map<String, Value>) -> Result<(), String> {
    let harness = field_str(action, "action", "harness")?;

    if !HARNESSES.contains(&harness) {
        return Err(format!("action.harness must be \"omp\" or \"pi\", not {harness:?}"));
    }

    check_model(action.get("model"))?;
    check_runnable(action, "prompt", PROMPT_MAX)?;

    let mode = optional_str(action, "approvalMode").map_err(|reason| format!("action.{reason}"))?;

    match mode {
        None => {}
        Some(mode) if !APPROVAL_MODES.contains(&mode) => {
            return Err(format!("unknown action.approvalMode {mode:?}"));
        }
        Some(_) if harness != "omp" => return Err("only an omp agent routine has an approvalMode".to_owned()),
        Some(_) => {}
    }

    match action.get("trustOnce") {
        None | Some(Value::Null | Value::Bool(false)) => {}
        Some(Value::Bool(true)) if harness == "pi" => {}
        Some(Value::Bool(true)) => return Err("only a pi agent routine has trustOnce".to_owned()),
        Some(_) => return Err("action.trustOnce must be true or false".to_owned()),
    }

    check_whole(action, "action.maxMinutes", MAX_MINUTES_MAX, true)
}

/// `null`, or `{"provider", "id"}`, each 1 to [`MODEL_PART_MAX`] characters without control or
/// invisible characters.
fn check_model(model: Option<&Value>) -> Result<(), String> {
    let model = match model {
        None | Some(Value::Null) => return Ok(()),
        Some(Value::Object(model)) => model,
        Some(_) => return Err("action.model must be an object or null".to_owned()),
    };

    for key in ["provider", "id"] {
        let text = field_str(model, "action.model", key)?;
        let field = format!("action.model.{key}");

        check_length(&field, text, MODEL_PART_MAX)?;

        if !display_safe(text) {
            return Err(format!("{field} contains a control or invisible character"));
        }
    }

    Ok(())
}

/// The prompt or command `key`: 1 to `max` characters, no invisible characters and no control
/// characters but tab, line feed and carriage return, so what a person approves is what runs.
fn check_runnable(action: &Map<String, Value>, key: &str, max: usize) -> Result<(), String> {
    let text = field_str(action, "action", key)?;
    let field = format!("action.{key}");

    check_length(&field, text, max)?;

    let hidden = |character: char| {
        let control = character.is_control() && !matches!(character, '\t' | '\n' | '\r');
        control || is_format_character(character)
    };

    if text.chars().any(hidden) {
        return Err(format!("{field} contains a control or invisible character"));
    }

    Ok(())
}

/// The string `key` of the object at `path`.
fn field_str<'a>(map: &'a Map<String, Value>, path: &str, key: &str) -> Result<&'a str, String> {
    required_str(map, key).map_err(|reason| format!("{path}.{reason}"))
}

/// The whole number at `field` (`path.key`, the key read from `map`) from 1 to `max`; absent or
/// `null` (the default) is fine when `optional` is set.
fn check_whole(map: &Map<String, Value>, field: &str, max: u64, optional: bool) -> Result<(), String> {
    let key = field.rsplit('.').next().unwrap_or(field);

    match map.get(key) {
        None | Some(Value::Null) if optional => Ok(()),
        Some(value) if value.as_u64().is_some_and(|number| (1..=max).contains(&number)) => Ok(()),
        _ => Err(format!("{field} must be a whole number from 1 to {max}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_times_are_hours_and_minutes() {
        for good in ["00:00", "09:30", "23:59"] {
            assert!(is_clock_time(good), "{good}");
        }

        for bad in ["24:00", "9:30", "09:60", "09-30", "09:3a", "09:30:00", ""] {
            assert!(!is_clock_time(bad), "{bad}");
        }
    }
}
