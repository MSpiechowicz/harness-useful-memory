//! Validation, cleaning and secret masking shared by every backend.
//!
//! Every text loses the characters that render as nothing or change how the rest renders: control
//! characters (a body keeps `\n` and `\t`) and invisible format characters (bidirectional
//! controls, zero-width characters, tag characters; see `display_safe`), so a note cannot hide or
//! reorder text on screen. Titles, entity names, relation labels and client names become one line
//! with single spaces. Each function then checks one limit of [`crate::limits`] and masks secrets
//! with `redact_text` (after cleaning, so an invisible character cannot split a secret).
//!
//! List lengths are checked before any item is looked at, and duplicates are dropped in linear
//! time, so an oversized list costs nothing.

use std::collections::HashSet;

use super::StoreError;
use crate::limits::{BODY_MAX, ENTITIES_PER_NOTE, ENTITY_MAX, RELATION_MAX, TAG_MAX, TAGS_MAX, TITLE_MAX};
use crate::support::redact::redact_text;
use crate::support::text::display_safe;

/// What `redact_text` puts in place of a secret.
const MASK: &str = "••••";

/// A note title (1 to [`TITLE_MAX`] characters, one line).
pub fn title(raw: &str) -> Result<String, StoreError> {
    bounded("title", &line(raw), TITLE_MAX).map(|title| redact_text(&title))
}

/// A note body (1 to [`BODY_MAX`] characters, lines kept).
pub fn body(raw: &str) -> Result<String, StoreError> {
    body_unmasked(raw).map(|body| redact_text(&body))
}

/// A note body like [`body`], but with secrets not yet masked: for text that is joined to more
/// text first, so a secret split across the parts is masked as a whole.
pub fn body_unmasked(raw: &str) -> Result<String, StoreError> {
    bounded("body", &block(raw), BODY_MAX)
}

/// Tags: at most [`TAGS_MAX`], each 1 to [`TAG_MAX`] characters of `[a-z0-9_-]` (lowercased,
/// duplicates dropped).
pub fn tags(raw: &[String]) -> Result<Vec<String>, StoreError> {
    if raw.len() > TAGS_MAX {
        return Err(StoreError::Invalid(format!("at most {TAGS_MAX} tags")));
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();

    for tag in raw {
        let tag = line(tag).to_lowercase();
        let allowed = tag
            .chars()
            .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || "_-".contains(character));

        if tag.is_empty() || tag.chars().count() > TAG_MAX || !allowed {
            return Err(StoreError::Invalid(format!(
                "each tag must be 1-{TAG_MAX} characters of a-z, 0-9, _ or -"
            )));
        }

        let tag = redact_text(&tag);
        if seen.insert(tag.clone()) {
            out.push(tag);
        }
    }

    Ok(out)
}

/// Entity names linked to one note: at most [`ENTITIES_PER_NOTE`], duplicates dropped.
pub fn entities(raw: &[String]) -> Result<Vec<String>, StoreError> {
    if raw.len() > ENTITIES_PER_NOTE {
        return Err(StoreError::Invalid(format!(
            "at most {ENTITIES_PER_NOTE} entities per note"
        )));
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();

    for name in raw {
        let name = entity(name)?;

        if seen.insert(normalize(&name)) {
            out.push(name);
        }
    }

    Ok(out)
}

/// An entity name (1 to [`ENTITY_MAX`] characters, one line).
pub fn entity(raw: &str) -> Result<String, StoreError> {
    bounded("entity", &line(raw), ENTITY_MAX).map(|name| redact_text(&name))
}

/// A relation label (1 to [`RELATION_MAX`] characters), lowercased with whitespace as `_`.
pub fn relation(raw: &str) -> Result<String, StoreError> {
    let label = line(raw).to_lowercase().replace(' ', "_");
    bounded("relation", &label, RELATION_MAX).map(|label| redact_text(&label))
}

/// The client name stored as a note's source (one line, masked, at most 64 characters). It is
/// masked before it is cut, so the cut cannot leave the visible start of a secret.
pub fn source(raw: Option<&str>) -> Option<String> {
    let name: String = redact_text(&line(raw?)).chars().take(64).collect();
    let name = name.trim_end();
    (!name.is_empty()).then(|| name.to_owned())
}

/// `body` and `append` (both [`body_unmasked`]) joined by a line break, then masked.
///
/// The line break splits the word that straddles the join, so a secret cut exactly there (`sk-ant-`
/// at the end of `body`, the rest at the start of `append`) would pass as two harmless halves. So
/// the two halves are also read as one word, and when that word holds a secret both halves are
/// replaced by the mask.
pub fn join_masked(body: &str, append: &str) -> String {
    let body_cut = body
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map_or(0, |(at, character)| at + character.len_utf8());
    let append_cut = append.find(char::is_whitespace).unwrap_or(append.len());
    let (body_rest, tail) = body.split_at(body_cut);
    let (head, append_rest) = append.split_at(append_cut);

    let word = format!("{tail}{head}");

    if redact_text(&word) == word {
        return redact_text(&format!("{body}\n{append}"));
    }

    redact_text(&format!("{body_rest}{MASK}\n{MASK}{append_rest}"))
}

/// The comparison form of a title or entity name: one line, lowercase.
pub fn normalize(text: &str) -> String {
    line(text).to_lowercase()
}

/// `text` on one line: whitespace (line breaks included) collapsed to single spaces, invisible
/// and control characters dropped, no leading or trailing space.
pub fn line(text: &str) -> String {
    let visible: String = text
        .chars()
        .filter_map(|character| {
            if character.is_whitespace() {
                Some(' ')
            } else {
                visible(character).then_some(character)
            }
        })
        .collect();

    visible.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` with its lines kept: every line break (`\r\n`, `\r`, vertical tab, form feed, U+0085,
/// U+2028, U+2029) becomes `\n`, tabs stay, other invisible and control characters are dropped,
/// and the whole is trimmed.
pub fn block(text: &str) -> String {
    let text = text.replace("\r\n", "\n");
    let kept: String = text
        .chars()
        .filter_map(|character| match character {
            '\n' | '\r' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}' => Some('\n'),
            '\t' => Some('\t'),
            other => visible(other).then_some(other),
        })
        .collect();

    kept.trim().to_owned()
}

/// True unless `character` is a control or invisible format character.
fn visible(character: char) -> bool {
    let mut buffer = [0u8; 4];
    display_safe(character.encode_utf8(&mut buffer))
}

/// `text` when it has 1 to `max` characters.
fn bounded(field: &str, text: &str, max: usize) -> Result<String, StoreError> {
    let length = text.chars().count();

    if length == 0 || length > max {
        return Err(StoreError::Invalid(format!(
            "{field} must be 1-{max} characters (got {length})"
        )));
    }

    Ok(text.to_owned())
}
