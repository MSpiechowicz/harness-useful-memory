//! The rules a team hook body follows: Harness Desktop's hook file format, version 1
//! (`docs/hooks-format.md` in the desktop repository, ported rule for rule), and the two team
//! rules.
//!
//! [`check_team_body`] checks everything the format lists except the project path, and refuses a
//! Harness family hook (`seed` set). [`TeamBody::repo_for`] then checks a project hook's
//! `project` against the repository it is shared for: it holds that repository's `git:` key, not a
//! local path, so a hook of a repository without an `origin` remote (a `path:` key) is never
//! shared. A body that breaks the format is [`BodyProblem::Invalid`], one the team rules keep out
//! is [`BodyProblem::Refused`].

use serde_json::{Map, Value};

use crate::limits::HOOK_BODY_MAX;
use crate::support::redact::has_credentials;
use crate::support::text::is_format_character;

/// Longest `name`, in characters.
const NAME_MAX: usize = 80;

/// Longest `description`, in characters.
const DESCRIPTION_MAX: usize = 500;

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

/// Why a body cannot be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyProblem {
    /// It breaks the hook file format (or holds a credential).
    Invalid(String),
    /// It is a valid hook the team rules keep out: a Harness family hook, or a project hook that
    /// does not name the repository it is shared for by its `git:` key.
    Refused(String),
}

impl BodyProblem {
    /// The reason, for a person.
    pub fn message(&self) -> String {
        match self {
            Self::Invalid(reason) => format!("the hook file is invalid: {reason}"),
            Self::Refused(reason) => reason.clone(),
        }
    }
}

/// What the team rules need of a body [`check_team_body`] accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamBody {
    pub id: String,
    /// The `project` of a project hook (`None`: a global hook).
    pub project: Option<String>,
}

impl TeamBody {
    /// The repository the hook is stored for when it is shared from the repository `identity`
    /// (its key, `None` without one): `None` for a global hook (whatever `identity` is), the key
    /// for a project hook whose `project` is that `git:` key. Refused otherwise.
    pub fn repo_for(&self, identity: Option<&str>) -> Result<Option<String>, BodyProblem> {
        let Some(project) = &self.project else {
            return Ok(None);
        };

        let refused = |reason: &str| Err(BodyProblem::Refused(reason.to_owned()));

        let Some(identity) = identity else {
            return refused("a project hook is shared for a repository: give its folder with --project");
        };

        if identity.starts_with("path:") || project.starts_with("path:") {
            return refused(
                "the repository has no origin remote, so its hooks are not shared (only a git: repository key \
                 leaves this computer)",
            );
        }

        if !project.starts_with("git:") {
            return refused("a team hook's project is the repository key (git:host/owner/repo), not a local path");
        }

        if project != identity {
            return refused("the hook's project is not the repository of --project");
        }

        Ok(Some(project.clone()))
    }
}

/// The hook in `body` when it is a valid version 1 hook file (its `project` aside, see
/// [`TeamBody::repo_for`]) that is no Harness family hook and holds no credential.
pub fn check_team_body(body: &str) -> Result<TeamBody, BodyProblem> {
    let invalid = |reason: String| BodyProblem::Invalid(reason);

    if body.len() > HOOK_BODY_MAX {
        return Err(invalid("the file is larger than 16 KiB".to_owned()));
    }

    let value: Value = serde_json::from_str(body).map_err(|_| invalid("the file is not JSON".to_owned()))?;

    let Value::Object(map) = value else {
        return Err(invalid("the file is not a JSON object".to_owned()));
    };

    let checked = check_map(&map).map_err(invalid)?;

    if has_credentials(body) {
        return Err(invalid(
            "it holds a credential: let the command read secrets from environment variables instead".to_owned(),
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
    match map.get("version") {
        None => return Err("version is missing".to_owned()),
        Some(version) if version.as_u64() != Some(1) => return Err("version must be 1".to_owned()),
        Some(_) => {}
    }

    let id = required_str(map, "id")?;
    if !super::is_hook_id(id) {
        return Err("id must be 32 lowercase hex characters".to_owned());
    }

    check_name(required_str(map, "name")?)?;

    if let Some(description) = optional_str(map, "description")? {
        check_description(description)?;
    }

    let project = check_scope(map)?;

    let event = required_str(map, "event")?;
    if !EVENTS.contains(&event) {
        return Err(format!("unknown event {event:?}"));
    }

    if let Some(matcher) = optional_str(map, "matcher")? {
        check_matcher(event, matcher)?;
    }

    let action = check_action(map.get("action"))?;
    check_combination(event, action)?;

    match map.get("enabled") {
        None => return Err("enabled is missing".to_owned()),
        Some(Value::Bool(_)) => {}
        Some(_) => return Err("enabled must be true or false".to_owned()),
    }

    check_requires(map.get("requires"))?;

    let seeded = match optional_str(map, "seed")? {
        None => false,
        Some(seed) if SEEDS.contains(&seed) => true,
        Some(seed) => return Err(format!("unknown seed {seed:?}")),
    };

    for key in ["createdAt", "updatedAt"] {
        check_timestamp(map, key)?;
    }

    for key in ["createdBy", "updatedBy"] {
        match required_str(map, key)? {
            "desktop" | "agent" => {}
            other => return Err(format!("{key} must be \"desktop\" or \"agent\", not {other:?}")),
        }
    }

    Ok(Checked {
        body: TeamBody {
            id: id.to_owned(),
            project,
        },
        seeded,
    })
}

/// 1 to [`NAME_MAX`] characters, not blank, no control or invisible characters.
fn check_name(name: &str) -> Result<(), String> {
    let chars = name.chars().count();

    if chars == 0 || name.trim().is_empty() {
        return Err("name is empty".to_owned());
    }

    if chars > NAME_MAX {
        return Err(format!("name is longer than {NAME_MAX} characters"));
    }

    if name.chars().any(char::is_control) {
        return Err("name contains a control character".to_owned());
    }

    if name.chars().any(is_format_character) {
        return Err("name contains an invisible character".to_owned());
    }

    Ok(())
}

/// At most [`DESCRIPTION_MAX`] characters, no invisible characters.
fn check_description(description: &str) -> Result<(), String> {
    if description.chars().count() > DESCRIPTION_MAX {
        return Err(format!("description is longer than {DESCRIPTION_MAX} characters"));
    }

    if description.chars().any(is_format_character) {
        return Err("description contains an invisible character".to_owned());
    }

    Ok(())
}

/// The `project` of a project hook (any string here), `None` for a global hook.
fn check_scope(map: &Map<String, Value>) -> Result<Option<String>, String> {
    let scope = required_str(map, "scope")?;
    let project = optional_str(map, "project")?;

    match (scope, project) {
        ("global", None) => Ok(None),
        ("global", Some(_)) => Err("a global hook has no project".to_owned()),
        ("project", None) => Err("a project hook needs a project".to_owned()),
        ("project", Some(project)) => Ok(Some(project.to_owned())),
        (other, _) => Err(format!("unknown scope {other:?}")),
    }
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

/// 1 to `max` characters.
fn check_length(field: &str, text: &str, max: usize) -> Result<(), String> {
    let chars = text.chars().count();

    if chars == 0 {
        return Err(format!("{field} is empty"));
    }

    if chars > max {
        return Err(format!("{field} is longer than {max} characters"));
    }

    Ok(())
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

fn check_timestamp(map: &Map<String, Value>, key: &str) -> Result<(), String> {
    if !is_timestamp(required_str(map, key)?) {
        return Err(format!("{key} must be an RFC 3339 UTC time like 2026-10-17T10:00:00Z"));
    }

    Ok(())
}

fn required_str<'a>(map: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    match map.get(key) {
        None => Err(format!("{key} is missing")),
        Some(Value::String(text)) => Ok(text),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

/// A string, or `None` when the key is absent or `null`.
fn optional_str<'a>(map: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(format!("{key} must be a string or null")),
    }
}

/// True for `YYYY-MM-DDTHH:MM:SS[.fraction]Z` with a valid month, day, hour, minute and second.
fn is_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();

    let Some(rest) = text.strip_suffix('Z') else {
        return false;
    };

    let separators = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')];

    if bytes.len() < 20 || separators.iter().any(|(index, separator)| bytes[*index] != *separator) {
        return false;
    }

    let number = |range: std::ops::Range<usize>| -> Option<u32> {
        let part = text.get(range)?;
        part.bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| part.parse().ok())?
    };

    let fraction_ok = match rest.get(19..) {
        Some("") => true,
        Some(fraction) => fraction
            .strip_prefix('.')
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())),
        None => false,
    };

    let parts = (
        number(0..4),
        number(5..7),
        number(8..10),
        number(11..13),
        number(14..16),
        number(17..19),
    );

    let (Some(_), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = parts else {
        return false;
    };

    fraction_ok && (1..=12).contains(&month) && (1..=31).contains(&day) && hour < 24 && minute < 60 && second < 61
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_follow_the_format() {
        for good in ["2026-10-17T10:00:00Z", "2026-10-17T10:00:00.5Z", "2026-12-31T23:59:60Z"] {
            assert!(is_timestamp(good), "{good}");
        }

        for bad in [
            "2026-10-17 10:00:00Z",
            "2026-10-17T10:00:00",
            "2026-10-17T10:00:00+02:00",
            "2026-13-17T10:00:00Z",
            "2026-10-17T24:00:00Z",
            "2026-10-17T10:00:00.Z",
            "2026-1O-17T10:00:00Z",
        ] {
            assert!(!is_timestamp(bad), "{bad}");
        }
    }
}
