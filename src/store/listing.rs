//! Notes page by page, for a person browsing them ([`MemoryStore::list_notes`]): filtered by a
//! substring, a kind, a scope, a place and a linked entity, sorted by the time of the latest
//! write, title, kind, scope or place ([`NoteSort`]), counted per kind, per scope and per place.
//!
//! The backends filter, sort, count and cut the page in the database and leave the bodies there.
//! They count the notes the text and the entity keep per kind and scope ([`NotePage::new`] makes
//! the counts of the page from those), and list the page of the view narrowed to the scope asked
//! for ([`scoped`]). A backend keeps every note in one place and does not know which: the place is
//! filtered and counted above it, by [`crate::store::SplitStore`] (which reads only the store of
//! the place asked for) or by the caller of a lone store ([`NotePage::kept_in`]).
//! The text matches the title, the body or the tags as a substring, ignoring case as the database
//! folds it: the SQLite-dialect stores fold ASCII letters only (SQLite's `lower()`), PostgreSQL
//! and MongoDB fold every letter. A sort by title compares the normalized titles
//! ([`clean::normalize`]) byte by byte, a sort by kind the stored kind names (`decision` before
//! `fix`), a sort by usefulness the number of times agents found the note useful (a note without
//! counters counts as 0); every sort breaks ties by note id in the same direction.
//!
//! [`MemoryStore::list_notes`]: crate::store::MemoryStore::list_notes

use std::cmp::Ordering;

use crate::backend::Place;
use crate::limits::{HIDDEN_MAX, LIST_PAGE_MAX, QUERY_MAX};
use crate::store::{Note, NoteKind, NoteUsage, Provenance, Scope, StoreError, View, clean};
use crate::support::lang::Lang;

/// Which notes a listing keeps by their language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LangFilter {
    /// Notes looked at and found English.
    En,
    /// Notes looked at and found in another language.
    Other,
    /// Notes not looked at yet or not certain (no `lang`, or `unknown`).
    Unknown,
}

impl LangFilter {
    /// Parses `en`, `other` or `unknown`.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "en" => Some(Self::En),
            "other" => Some(Self::Other),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    /// True when a note of language `lang` (`None` when not looked at or not certain) is kept.
    pub fn keeps(filter: Option<Self>, lang: Option<Lang>) -> bool {
        match filter {
            None => true,
            Some(Self::En) => lang == Some(Lang::En),
            Some(Self::Other) => lang == Some(Lang::Other),
            Some(Self::Unknown) => lang.is_none(),
        }
    }

    /// The SQL condition on the `lang` column `column` keeping these notes (`1 = 1` for no
    /// filter). Without the schema extensions there is no column: no note was looked at.
    pub(crate) fn condition(filter: Option<Self>, column: &str, extensions: bool) -> String {
        let Some(filter) = filter else {
            return "1 = 1".to_owned();
        };

        if !extensions {
            return if filter == Self::Unknown { "1 = 1" } else { "1 = 0" }.to_owned();
        }

        match filter {
            Self::En => format!("{column} = 'en'"),
            Self::Other => format!("{column} = 'other'"),
            Self::Unknown => format!("({column} IS NULL OR {column} NOT IN ('en', 'other'))"),
        }
    }
}

/// Which notes a listing keeps by how useful agents found them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsefulFilter {
    /// Notes an agent found useful at least once.
    Ever,
    /// Notes shown to agents at least [`Self::SHOWN_MIN`] times and never found useful.
    Never,
}

impl UsefulFilter {
    /// How often a note has to be shown before never being useful says something about it.
    pub const SHOWN_MIN: i64 = 3;

    /// Parses `ever` or `never`.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "ever" => Some(Self::Ever),
            "never" => Some(Self::Never),
            _ => None,
        }
    }

    /// True when a note with `usage` (`None` when the database has no counters) is kept.
    pub fn keeps(filter: Option<Self>, usage: Option<NoteUsage>) -> bool {
        let Some(filter) = filter else {
            return true;
        };

        let Some(usage) = usage else {
            return false;
        };

        match filter {
            Self::Ever => usage.useful >= 1,
            Self::Never => usage.shown >= Self::SHOWN_MIN && usage.useful == 0,
        }
    }

    /// The SQL condition on the counter columns `shown` and `useful` keeping these notes (`1 = 1`
    /// for no filter). Without the columns no note qualifies.
    pub(crate) fn condition(filter: Option<Self>, shown: &str, useful: &str, usage: bool) -> String {
        let Some(filter) = filter else {
            return "1 = 1".to_owned();
        };

        if !usage {
            return "1 = 0".to_owned();
        }

        match filter {
            Self::Ever => format!("{useful} >= 1"),
            Self::Never => format!("({shown} >= {} AND {useful} = 0)", Self::SHOWN_MIN),
        }
    }
}

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
    /// Keeps the notes kept in this place (this computer's file or the team database); `None`
    /// keeps both. A store over one database leaves it to its caller (see the module docs).
    pub place: Option<Place>,
    /// Keeps the notes linked to the entity of this name (compared like entity names are).
    pub entity: Option<String>,
    /// Keeps the notes of this language (see [`LangFilter`]); `None` keeps every note. It narrows
    /// the page and every count.
    pub lang: Option<LangFilter>,
    /// Keeps the notes agents found useful, or never useful (see [`UsefulFilter`]); `None` keeps
    /// every note. It narrows the page and every count.
    pub useful: Option<UsefulFilter>,
    pub sort: NoteSort,
    pub descending: bool,
    /// Rows skipped before the page.
    pub offset: usize,
    /// Rows on the page (clamped to 1 to [`LIST_PAGE_MAX`]).
    pub limit: usize,
    /// Note ids left out of the page and the counts (the team notes hidden on this computer), at
    /// most [`HIDDEN_MAX`].
    pub exclude: Vec<i64>,
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
    /// By how often agents found the note useful.
    Useful,
    /// By the time an agent last found the note useful.
    LastUseful,
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
    /// The language, when a store looked at the note and was certain.
    pub lang: Option<Lang>,
    /// The usefulness counters, when the database has them.
    pub usage: Option<NoteUsage>,
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
            lang: note.lang,
            usage: note.usage,
        }
    }
}

/// One page of notes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotePage {
    /// The notes the text, the kind, the scope and the place keep, on every page.
    pub total: usize,
    /// The notes the text, the scope and the place keep, per kind (every kind, in
    /// [`NoteKind::ALL`] order), whatever the kind asked for.
    pub kinds: Vec<(NoteKind, usize)>,
    /// The notes the text, the kind and the place keep, per scope (repository, then global),
    /// whatever the scope asked for.
    pub scopes: Vec<(Scope, usize)>,
    /// The notes the text, the kind and the scope keep, per place (this computer, then the team
    /// database), whatever the place asked for; empty from a store over one database, which does
    /// not know where it is kept (see [`Self::kept_in`]).
    pub places: Vec<(Place, usize)>,
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
            places: Vec::new(),
            rows,
            counted: cells,
        }
    }

    /// This page of a store that keeps every note in `place`, for `query`: what it counts is kept
    /// there, and it lists and counts nothing else when `query` asks for the other place.
    pub(crate) fn kept_in(self, place: Place, query: &NoteListQuery) -> Self {
        let places = per_place([(place, self.total)]);

        if query.place.is_some_and(|asked| asked != place) {
            return Self {
                places,
                ..Self::new([], query, Vec::new())
            };
        }

        Self { places, ..self }
    }
}

/// The counts `counted` (a place may come more than once) per place: this computer, then the team
/// database.
pub(crate) fn per_place(counted: impl IntoIterator<Item = (Place, usize)>) -> Vec<(Place, usize)> {
    let counted: Vec<(Place, usize)> = counted.into_iter().collect();

    [Place::Local, Place::Remote]
        .into_iter()
        .map(|place| {
            let count = counted
                .iter()
                .filter(|(at, _)| *at == place)
                .map(|(_, count)| count)
                .sum();
            (place, count)
        })
        .collect()
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

/// `ids` as a JSON array (`[3,7]`), for `json_each`; an error when more than [`HIDDEN_MAX`].
pub(crate) fn excluded_json(ids: &[i64]) -> Result<String, StoreError> {
    let ids = checked_exclude(ids)?;
    let listed: Vec<String> = ids.iter().map(i64::to_string).collect();
    Ok(format!("[{}]", listed.join(",")))
}

/// `ids` when there are at most [`HIDDEN_MAX`] of them, else an error.
pub(crate) fn checked_exclude(ids: &[i64]) -> Result<&[i64], StoreError> {
    if ids.len() > HIDDEN_MAX {
        return Err(StoreError::Invalid(format!(
            "at most {HIDDEN_MAX} note ids can be left out"
        )));
    }

    Ok(ids)
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
        NoteSort::Useful => useful_of(a).cmp(&useful_of(b)),
        NoteSort::LastUseful => last_useful_of(a).cmp(&last_useful_of(b)),
    }
    .then(a.id.cmp(&b.id));

    directed(query, ascending)
}

/// How often `row` was found useful: 0 without counters, as the stores sort it.
fn useful_of(row: &NoteRow) -> i64 {
    row.usage.map_or(0, |usage| usage.useful)
}

/// When `row` was last found useful: 0 when never or without counters, as the stores sort it.
fn last_useful_of(row: &NoteRow) -> i64 {
    row.usage.and_then(|usage| usage.last_useful_at).unwrap_or(0)
}

/// `ascending` turned round when `query` is descending.
pub(crate) fn directed(query: &NoteListQuery, ascending: Ordering) -> Ordering {
    if query.descending {
        ascending.reverse()
    } else {
        ascending
    }
}
