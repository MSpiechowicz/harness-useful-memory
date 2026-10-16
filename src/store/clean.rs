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

use super::{Forget, Link, NoteDraft, NoteKind, NotePatch, Provenance, StoreError};
use crate::limits::{BODY_MAX, ENTITIES_PER_NOTE, ENTITY_MAX, RELATION_MAX, TAG_MAX, TAGS_MAX, TITLE_MAX};
use crate::support::redact::redact_text;
use crate::support::text::display_safe;

/// What `redact_text` puts in place of a secret.
const MASK: &str = "••••";

/// A [`NoteDraft`] cleaned and checked, ready to store. Backends clean before they open a write
/// transaction, so nothing slow runs while other writers wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanDraft {
    pub title: String,
    /// The comparison form of the title ([`normalize`]): unique per scope.
    pub title_norm: String,
    pub body: String,
    pub kind: NoteKind,
    /// The tags joined by single spaces.
    pub tags: String,
    pub entities: Vec<String>,
    pub writer: Writer,
}

/// A [`NotePatch`] cleaned and checked; `body` and `append` are not masked yet (see
/// [`patched_body`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanPatch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub append: Option<String>,
    pub kind: Option<NoteKind>,
    pub tags: Option<Vec<String>>,
    pub writer: Writer,
}

/// A [`Provenance`] as stored. An unknown writer is stored as such (no root and no source), so a
/// note never keeps the name of an earlier writer for a write it did not make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Writer {
    pub root: Option<String>,
    pub source: Option<String>,
    pub via_override: bool,
}

impl Writer {
    pub fn new(provenance: &Provenance) -> Self {
        Self {
            root: provenance.root.as_ref().map(|root| root.to_string_lossy().into_owned()),
            source: source(provenance.source.as_deref()),
            via_override: provenance.via_override,
        }
    }
}

/// `draft` cleaned and checked.
pub fn draft(draft: &NoteDraft) -> Result<CleanDraft, StoreError> {
    let title = title(&draft.title)?;

    Ok(CleanDraft {
        title_norm: normalize(&title),
        title,
        body: body(&draft.body)?,
        kind: draft.kind,
        tags: tags(&draft.tags)?.join(" "),
        entities: entities(&draft.entities)?,
        writer: Writer::new(&draft.provenance),
    })
}

/// `patch` cleaned and checked; fails when it changes nothing.
pub fn patch(patch: &NotePatch) -> Result<CleanPatch, StoreError> {
    let empty = patch.title.is_none()
        && patch.body.is_none()
        && patch.append.is_none()
        && patch.kind.is_none()
        && patch.tags.is_none();

    if empty {
        return Err(StoreError::Invalid(
            "nothing to change: give title, body, append, kind or tags".to_owned(),
        ));
    }

    Ok(CleanPatch {
        title: patch.title.as_deref().map(title).transpose()?,
        body: patch.body.as_deref().map(body_unmasked).transpose()?,
        append: patch.append.as_deref().map(body_unmasked).transpose()?,
        kind: patch.kind,
        tags: patch.tags.as_deref().map(tags).transpose()?,
        writer: Writer::new(&patch.provenance),
    })
}

/// The body a [`CleanPatch`] leaves on a note whose body is `current`: the patch body (or the
/// current one) with `append` joined, masked after the join so a secret split across the two is
/// masked as a whole (see [`join_masked`]), and at most [`BODY_MAX`] characters.
pub fn patched_body(current: &str, patch: &CleanPatch) -> Result<String, StoreError> {
    let body = patch.body.as_deref().unwrap_or(current);
    let body = match &patch.append {
        Some(append) => join_masked(body, append),
        None => redact_text(body),
    };

    let length = body.chars().count();
    if length > BODY_MAX {
        return Err(StoreError::Invalid(format!(
            "body would be {length} characters (most {BODY_MAX}); shorten it with body instead of append"
        )));
    }

    Ok(body)
}

/// `link` with its names and label cleaned and checked.
pub fn link(link: &Link) -> Result<Link, StoreError> {
    let from = entity(&link.from)?;
    let to = entity(&link.to)?;

    if normalize(&from) == normalize(&to) {
        return Err(StoreError::Invalid("an entity cannot be linked to itself".to_owned()));
    }

    Ok(Link {
        from,
        relation: relation(&link.relation)?,
        to,
        note_id: link.note_id,
    })
}

/// `what` with its names and label cleaned and checked.
pub fn forget(what: &Forget) -> Result<Forget, StoreError> {
    Ok(match what {
        Forget::Note(id) => Forget::Note(*id),
        Forget::Entity(name) => Forget::Entity(entity(name)?),
        Forget::Relation {
            from,
            relation: label,
            to,
        } => Forget::Relation {
            from: entity(from)?,
            relation: relation(label)?,
            to: entity(to)?,
        },
    })
}

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
