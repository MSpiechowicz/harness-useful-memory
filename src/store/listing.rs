//! Notes page by page, for a person browsing them ([`MemoryStore::list_notes`]): filtered by a
//! substring, a kind, a scope and a linked entity, sorted by the time of the latest write, title,
//! kind, scope or place ([`NoteSort`]), counted per kind and per scope.
//!
//! The backends filter, sort, count and cut the page in the database and leave the bodies there.
//! They count the notes the text and the entity keep per kind and scope ([`NotePage::new`] makes
//! the counts of the page from those), and list the page of the view narrowed to the scope asked
//! for ([`scoped`]).
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
    /// Keeps the notes of this scope (of those the view shows); `None` keeps both.
    pub scope: Option<Scope>,
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
    /// The notes the text, the kind and the scope keep, on every page.
    pub total: usize,
    /// The notes the text and the scope keep, per kind (every kind, in [`NoteKind::ALL`] order),
    /// whatever the kind asked for.
    pub kinds: Vec<(NoteKind, usize)>,
    /// The notes the text and the kind keep, per scope (repository, then global), whatever the
    /// scope asked for.
    pub scopes: Vec<(Scope, usize)>,
    pub rows: Vec<NoteRow>,
    /// The notes the text keeps, per kind and scope: what the counts above come from (a
    /// [`crate::store::SplitStore`] adds those of its two stores).
    pub(crate) counted: Vec<(NoteKind, Scope, usize)>,
}

impl NotePage {
    /// A page of `rows` of `query` with the counts per kind and scope `counted` (a pair may come
    /// more than once, an unknown kind name counts as a plain note); the counts and the total
    /// follow from them and the kind and the scope of `query`.
    pub(crate) fn new(
        counted: impl IntoIterator<Item = (NoteKind, Scope, usize)>,
        query: &NoteListQuery,
        rows: Vec<NoteRow>,
    ) -> Self {
        let mut cells: Vec<(NoteKind, Scope, usize)> = Vec::new();

        for (kind, scope, count) in counted {
            match cells.iter_mut().find(|(known, at, _)| *known == kind && *at == scope) {
                Some(cell) => cell.2 += count,
                None => cells.push((kind, scope, count)),
            }
        }

        let kind_kept = |kind: NoteKind| query.kind.is_none_or(|asked| asked == kind);
        let scope_kept = |scope: Scope| query.scope.is_none_or(|asked| asked == scope);

        let count = |keeps: &dyn Fn(NoteKind, Scope) -> bool| -> usize {
            cells
                .iter()
                .filter(|(kind, scope, _)| keeps(*kind, *scope))
                .map(|(_, _, count)| count)
                .sum()
        };

        let kinds = NoteKind::ALL
            .iter()
            .map(|listed| (*listed, count(&|kind, scope| kind == *listed && scope_kept(scope))))
            .collect();
        let scopes = [Scope::Repo, Scope::Global]
            .into_iter()
            .map(|listed| (listed, count(&|kind, scope| scope == listed && kind_kept(kind))))
            .collect();
        let total = count(&|kind, scope| kind_kept(kind) && scope_kept(scope));

        Self {
            total,
            kinds,
            scopes,
            rows,
            counted: cells,
        }
    }
}

/// The view parameters `repo` and `global` (the repository and whether global notes are shown)
/// narrowed to the scope of `query`: what its page lists. The counts take the whole view.
pub(crate) fn scoped<T>(query: &NoteListQuery, repo: Option<T>, global: bool) -> (Option<T>, bool) {
    match query.scope {
        None => (repo, global),
        Some(Scope::Repo) => (repo, false),
        Some(Scope::Global) => (None, global),
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
