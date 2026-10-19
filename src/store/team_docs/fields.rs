//! The rules every Harness Desktop file kind shares (hooks and routines, version 1): the body
//! size, `version`, `id`, `name`, `description`, `scope` with `project`, `enabled`, the
//! timestamps and who wrote it, each a port of the desktop's rule. Each check returns the reason
//! a file breaks it, for [`super::BodyProblem::invalid`].

use serde_json::{Map, Value};

use super::{DocKind, is_doc_id};
use crate::limits::TEAM_DOC_BODY_MAX;
use crate::support::text::is_format_character;

/// Longest `name`, in characters.
const NAME_MAX: usize = 80;

/// Longest `description`, in characters.
const DESCRIPTION_MAX: usize = 500;

/// The JSON object in `body`, at most [`TEAM_DOC_BODY_MAX`] bytes.
pub(crate) fn object(body: &str) -> Result<Map<String, Value>, String> {
    if body.len() > TEAM_DOC_BODY_MAX {
        return Err("the file is larger than 16 KiB".to_owned());
    }

    let value: Value = serde_json::from_str(body).map_err(|_| "the file is not JSON".to_owned())?;

    let Value::Object(map) = value else {
        return Err("the file is not a JSON object".to_owned());
    };

    Ok(map)
}

/// `version` is 1.
pub(crate) fn check_version(map: &Map<String, Value>) -> Result<(), String> {
    match map.get("version") {
        None => Err("version is missing".to_owned()),
        Some(version) if version.as_u64() != Some(1) => Err("version must be 1".to_owned()),
        Some(_) => Ok(()),
    }
}

/// `id`, 32 lowercase hex characters.
pub(crate) fn check_id(map: &Map<String, Value>) -> Result<&str, String> {
    let id = required_str(map, "id")?;

    if !is_doc_id(id) {
        return Err("id must be 32 lowercase hex characters".to_owned());
    }

    Ok(id)
}

/// `name` (required) and `description` (optional).
pub(crate) fn check_labels(map: &Map<String, Value>) -> Result<(), String> {
    check_name(required_str(map, "name")?)?;

    if let Some(description) = optional_str(map, "description")? {
        check_description(description)?;
    }

    Ok(())
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

/// The `project` of a project `kind` document (any string here), `None` for a global one.
pub(crate) fn check_scope(map: &Map<String, Value>, kind: DocKind) -> Result<Option<String>, String> {
    let noun = kind.noun();
    let scope = required_str(map, "scope")?;
    let project = optional_str(map, "project")?;

    match (scope, project) {
        ("global", None) => Ok(None),
        ("global", Some(_)) => Err(format!("a global {noun} has no project")),
        ("project", None) => Err(format!("a project {noun} needs a project")),
        ("project", Some(project)) => Ok(Some(project.to_owned())),
        (other, _) => Err(format!("unknown scope {other:?}")),
    }
}

/// `enabled` is `true` or `false`.
pub(crate) fn check_enabled(map: &Map<String, Value>) -> Result<(), String> {
    match map.get("enabled") {
        None => Err("enabled is missing".to_owned()),
        Some(Value::Bool(_)) => Ok(()),
        Some(_) => Err("enabled must be true or false".to_owned()),
    }
}

/// `createdAt` and `updatedAt` are RFC 3339 UTC times, `createdBy` and `updatedBy` `desktop` or
/// `agent`.
pub(crate) fn check_written(map: &Map<String, Value>) -> Result<(), String> {
    for key in ["createdAt", "updatedAt"] {
        if !is_timestamp(required_str(map, key)?) {
            return Err(format!("{key} must be an RFC 3339 UTC time like 2026-10-17T10:00:00Z"));
        }
    }

    for key in ["createdBy", "updatedBy"] {
        match required_str(map, key)? {
            "desktop" | "agent" => {}
            other => return Err(format!("{key} must be \"desktop\" or \"agent\", not {other:?}")),
        }
    }

    Ok(())
}

/// 1 to `max` characters.
pub(crate) fn check_length(field: &str, text: &str, max: usize) -> Result<(), String> {
    let chars = text.chars().count();

    if chars == 0 {
        return Err(format!("{field} is empty"));
    }

    if chars > max {
        return Err(format!("{field} is longer than {max} characters"));
    }

    Ok(())
}

pub(crate) fn required_str<'a>(map: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    match map.get(key) {
        None => Err(format!("{key} is missing")),
        Some(Value::String(text)) => Ok(text),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

/// A string, or `None` when the key is absent or `null`.
pub(crate) fn optional_str<'a>(map: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
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
