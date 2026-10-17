//! Notes page by page, for a person browsing them ([`MemoryStore::list_notes`]): filtered by a
//! substring, a kind and a linked entity, sorted by the time of the latest write, title, kind,
//! scope or place ([`NoteSort`]), counted per kind.
//!
//! The backends filter, sort, count and cut the page in the database and leave the bodies there.
//! The text matches the title, the body or the tags as a substring, ignoring case as the database
//! folds it: the SQLite-dialect stores fold ASCII letters only (SQLite's `lower()`), PostgreSQL
//! and MongoDB fold every letter. A sort by title compares the normalized titles
//! ([`clean::normalize`]) byte by byte, a sort by kind the stored kind names (`decision` before
//! `fix`); every sort breaks ties by note id in the same direction.
//!
//! [`MemoryStore::list_notes`]: crate::store::MemoryStore::list_notes

use std::cmp::Ordering;

use crate::limits::{LIST_PAGE_MAX, QUERY_MAX};
use crate::store::{Note, NoteKind, Provenance, Scope, StoreError, View, clean};

/// One page of notes to list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteListQuery {
    pub view: View,
    /// Keeps the notes whose title, body or tags contain it, ignoring case; blank keeps every
    /// note. At most [`QUERY_MAX`] characters.
    pub text: String,
    pub kind: Option<NoteKind>,
    /// Keeps the notes linked to the entity of this name (compared like entity names are).
    pub entity: Option<String>,
    pub sort: NoteSort,
    pub descending: bool,
    /// Rows skipped before the page.
    pub offset: usize,
    /// Rows on the page (clamped to 1 to [`LIST_PAGE_MAX`]).
    pub limit: usize,
}

/// The order of a [`NoteListQuery`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteSort {
    /// By the time of the latest write.
    Updated,
    /// By normalized title.
    Title,
    /// By the stored kind name.
    Kind,
    /// Global notes before repository notes (ascending).
    Scope,
    /// Notes kept on this computer before those in the team database (ascending). One store
    /// keeps its notes in one place, so only [`crate::store::SplitStore`] orders by it; the
    /// others order by id alone.
    Place,
}

/// A listed note: a [`Note`] without its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteRow {
    pub id: i64,
    pub scope: Scope,
    /// The repository key, for repository notes.
    pub project: Option<String>,
    pub kind: NoteKind,
    pub title: String,
    pub tags: Vec<String>,
    pub entities: Vec<String>,
    pub provenance: Provenance,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds.
    pub updated_at: i64,
    pub revision: i64,
    pub hits: i64,
}

impl From<Note> for NoteRow {
    fn from(note: Note) -> Self {
        Self {
            id: note.id,
            scope: note.scope,
            project: note.project,
            kind: note.kind,
            title: note.title,
            tags: note.tags,
            entities: note.entities,
            provenance: note.provenance,
            created_at: note.created_at,
            updated_at: note.updated_at,
            revision: note.revision,
            hits: note.hits,
        }
    }
}

/// One page of notes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotePage {
    /// The notes the text and the kind keep, on every page.
    pub total: usize,
    /// The notes the text keeps, per kind (every kind, in [`NoteKind::ALL`] order), whatever the
    /// kind asked for.
    pub kinds: Vec<(NoteKind, usize)>,
    pub rows: Vec<NoteRow>,
}

impl NotePage {
    /// A page of `rows` with the per-kind counts `counted` (a kind may come more than once, an
    /// unknown name counts as a plain note); the total follows from them and `kind`.
    pub(crate) fn new(
        counted: impl IntoIterator<Item = (NoteKind, usize)>,
        kind: Option<NoteKind>,
        rows: Vec<NoteRow>,
    ) -> Self {
        let mut kinds: Vec<(NoteKind, usize)> = NoteKind::ALL.iter().map(|kind| (*kind, 0)).collect();

        for (counted_kind, count) in counted {
            if let Some(entry) = kinds.iter_mut().find(|(known, _)| *known == counted_kind) {
                entry.1 += count;
            }
        }

        let total = kinds
            .iter()
            .filter(|(listed, _)| kind.is_none_or(|kind| kind == *listed))
            .map(|(_, count)| count)
            .sum();

        Self { total, kinds, rows }
    }
}

/// The text of `query`, trimmed; `None` when blank, an error when longer than [`QUERY_MAX`].
pub(crate) fn query_text(query: &NoteListQuery) -> Result<Option<&str>, StoreError> {
    let text = query.text.trim();

    if text.chars().count() > QUERY_MAX {
        return Err(StoreError::Invalid(format!(
            "query is longer than {QUERY_MAX} characters"
        )));
    }

    Ok((!text.is_empty()).then_some(text))
}

/// The normalized entity name of `query`; `None` when there is none or it is blank.
pub(crate) fn query_entity(query: &NoteListQuery) -> Option<String> {
    query
        .entity
        .as_deref()
        .map(clean::normalize)
        .filter(|entity| !entity.is_empty())
}

/// The rows of a page of `query`.
pub(crate) fn page_limit(query: &NoteListQuery) -> usize {
    query.limit.clamp(1, LIST_PAGE_MAX)
}

/// `ASC` or `DESC`.
pub(crate) fn direction(query: &NoteListQuery) -> &'static str {
    if query.descending { "DESC" } else { "ASC" }
}

/// How the rows of `query` are ordered: the stores' `ORDER BY`, for merging pages of two stores.
pub(crate) fn compare(query: &NoteListQuery, a: &NoteRow, b: &NoteRow) -> Ordering {
    let ascending = match query.sort {
        NoteSort::Updated => a.updated_at.cmp(&b.updated_at),
        NoteSort::Title => clean::normalize(&a.title).cmp(&clean::normalize(&b.title)),
        NoteSort::Kind => a.kind.as_str().cmp(b.kind.as_str()),
        NoteSort::Scope => (a.scope == Scope::Repo).cmp(&(b.scope == Scope::Repo)),
        NoteSort::Place => Ordering::Equal,
    }
    .then(a.id.cmp(&b.id));

    directed(query, ascending)
}

/// `ascending` turned round when `query` is descending.
pub(crate) fn directed(query: &NoteListQuery, ascending: Ordering) -> Ordering {
    if query.descending {
        ascending.reverse()
    } else {
        ascending
    }
}
