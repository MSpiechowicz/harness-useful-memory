//! The storage contract of the memory server.
//!
//! [`MemoryStore`] is what the MCP tools call; [`sqlite::SqliteStore`] is the local backend, and
//! [`postgres::PostgresStore`] and [`libsql::LibsqlStore`] are shared team databases. Every
//! implementation validates its input against [`crate::limits`] and masks secrets with
//! `redact_text` before anything is stored (the helpers in [`clean`] do both), so callers may pass
//! raw agent text.

pub mod clean;
pub mod libsql;
pub mod mongo;
pub mod postgres;
pub mod sqlite;

use std::path::PathBuf;

pub use libsql::LibsqlStore;
pub use mongo::MongoStore;
pub use postgres::PostgresStore;
pub use sqlite::SqliteStore;
pub(crate) use sqlite::prepare_new_file;

/// Where a note, an entity or a relation lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Tied to one repository.
    Repo,
    /// Shared by every repository.
    Global,
}

impl Scope {
    /// `repo` or `global`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::Global => "global",
        }
    }
}

/// The identity of a repository (see [`crate::project`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectKey {
    /// `git:<host/owner/repo>` or `path:<canonical directory>`.
    pub key: String,
    /// The canonical work-tree (or directory) path.
    pub root: PathBuf,
    /// The normalized `origin` remote (`host/owner/repo`, no credentials).
    pub remote: Option<String>,
}

/// The scope an operation writes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The given repository.
    Repo(ProjectKey),
    /// The global scope.
    Global,
}

/// What an operation may read: the current repository (when known) and/or the global scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// The repository whose notes are visible.
    pub repo: Option<ProjectKey>,
    /// Whether global notes are visible.
    pub global: bool,
}

impl View {
    /// The scope `target` writes to, plus the global scope.
    pub fn around(target: &Target) -> Self {
        match target {
            Target::Repo(key) => Self {
                repo: Some(key.clone()),
                global: true,
            },
            Target::Global => Self {
                repo: None,
                global: true,
            },
        }
    }
}

/// The kind of a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    Outcome,
    Problem,
    Fix,
    Optimisation,
    Location,
    Decision,
    Note,
}

impl NoteKind {
    /// Every kind, in the order the tool schemas list them.
    pub const ALL: [NoteKind; 7] = [
        Self::Outcome,
        Self::Problem,
        Self::Fix,
        Self::Optimisation,
        Self::Location,
        Self::Decision,
        Self::Note,
    ];

    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Outcome => "outcome",
            Self::Problem => "problem",
            Self::Fix => "fix",
            Self::Optimisation => "optimisation",
            Self::Location => "location",
            Self::Decision => "decision",
            Self::Note => "note",
        }
    }

    /// Parses a stored or agent-supplied name (`optimization` is accepted too).
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim().to_lowercase();

        if raw == "optimization" {
            return Some(Self::Optimisation);
        }

        Self::ALL.into_iter().find(|kind| kind.as_str() == raw)
    }
}

/// Where the latest write to a note came from. The repository key is only an identifier (any
/// folder can claim any `origin`), so replies show this next to each note for the reader to judge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    /// The canonical root of the checkout (or folder) the writer ran in, also when it named
    /// another repository with `project_path`; `None` when unknown or outside one.
    pub root: Option<PathBuf>,
    /// The client that wrote it (`clientInfo.name`).
    pub source: Option<String>,
    /// True when the agent named the repository with `project_path` and it is not the one the
    /// server runs in.
    pub via_override: bool,
}

/// A note to create (or to update when a note with the same title exists in the scope).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteDraft {
    pub title: String,
    pub body: String,
    pub kind: NoteKind,
    pub tags: Vec<String>,
    /// Entity names linked to the note (created in the note's scope when missing).
    pub entities: Vec<String>,
    pub provenance: Provenance,
}

/// Changes to an existing note; `None` keeps a field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotePatch {
    pub title: Option<String>,
    pub body: Option<String>,
    /// Text added to the end of the body (after `body` when both are given).
    pub append: Option<String>,
    pub kind: Option<NoteKind>,
    /// Replaces every tag.
    pub tags: Option<Vec<String>>,
    /// The writer, recorded on the note (an unknown one too: the note then names no writer).
    pub provenance: Provenance,
}

/// A stored note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: i64,
    pub scope: Scope,
    /// The repository key, for repository notes.
    pub project: Option<String>,
    pub kind: NoteKind,
    pub title: String,
    pub body: String,
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

/// The result of a write to a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub id: i64,
    /// True when the note was created, false when an existing one was updated.
    pub created: bool,
    pub revision: i64,
    pub scope: Scope,
    pub project: Option<String>,
}

/// A search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    /// Free text; empty lists the most recent notes.
    pub text: String,
    pub view: View,
    pub kind: Option<NoteKind>,
    /// Number of rows (clamped to [`crate::limits::SEARCH_LIMIT_MAX`]).
    pub limit: usize,
    /// When set, only repository notes last written from this checkout root without
    /// `project_path` ([`Provenance::root`] equal to it, not [`Provenance::via_override`]); global
    /// notes are left out too. Memory recall searches this way.
    pub checkout: Option<PathBuf>,
}

/// One search row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub id: i64,
    pub kind: NoteKind,
    pub scope: Scope,
    pub title: String,
    /// A short excerpt around the match (the start of the body without a query).
    pub snippet: String,
    /// Unix seconds.
    pub updated_at: i64,
    pub entities: Vec<String>,
    pub provenance: Provenance,
}

/// A relation between two entities (`from relation to`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub from: String,
    pub relation: String,
    pub to: String,
    /// A note explaining the relation.
    pub note_id: Option<i64>,
}

/// A stored relation as [`MemoryStore::neighbors`] returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: String,
    pub relation: String,
    pub to: String,
    pub scope: Scope,
    pub note_id: Option<i64>,
}

/// The graph around an entity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Neighborhood {
    /// Relations within the requested depth (at most the requested limit).
    pub edges: Vec<Edge>,
    /// Relations left out by the limit.
    pub more_edges: usize,
    /// Notes linked to the reached entities or to the relations: `(id, title)`.
    pub notes: Vec<(i64, String)>,
}

/// What [`MemoryStore::forget`] deletes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Forget {
    /// A note (visible in the view).
    Note(i64),
    /// An entity with its relations.
    Entity(String),
    /// One relation.
    Relation { from: String, relation: String, to: String },
}

/// Store failures; every message is safe to show to the agent.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The input breaks a limit or a rule.
    #[error("{0}")]
    Invalid(String),
    /// The note, entity or relation does not exist in the visible scopes.
    #[error("{0}")]
    NotFound(String),
    /// A repository note with this title exists but was last written from another checkout, or the
    /// write named the repository with `project_path`; nothing was saved (the id of that note).
    #[error(
        "note {0} already has this title and was written from another checkout, or this call named the \
         repository with project_path, so nothing was saved: read it with memory_get, then call memory_update \
         with id {0} to replace it, or use another title"
    )]
    Conflict(i64),
    /// The database was written by a newer server; nothing is read or written.
    #[error("memory database is newer than this server")]
    NewerSchema,
    /// The database failed (the message carries no stored text).
    #[error("memory database error: {0}")]
    Backend(String),
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        let message = match &error {
            rusqlite::Error::SqliteFailure(code, _) => format!("{:?}", code.code),
            other => other.to_string(),
        };

        Self::Backend(message.chars().take(200).collect())
    }
}

/// A project memory backend. Writes and reads take `&mut self`; one store serves one process.
pub trait MemoryStore: Send {
    /// Creates a note, or updates the note with the same title in the scope (its revision grows).
    /// A repository note last written from another root or through `project_path`, or one a write
    /// through `project_path` would replace, is not replaced: [`StoreError::Conflict`].
    fn remember(&mut self, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError>;

    /// Changes a note visible in `view`.
    fn update(&mut self, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError>;

    /// The notes among `ids` visible in `view` (in the order asked); counts a hit on each.
    fn get(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError>;

    /// Ranked notes: the repository first, then relevance, then the most recently updated.
    fn search(&mut self, query: &SearchQuery) -> Result<Vec<Hit>, StoreError>;

    /// Up to `limit` notes in `view` resembling note `id` (which is left out).
    fn similar(&mut self, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError>;

    /// Records `link` in the target scope, creating its entities when missing.
    fn link(&mut self, target: &Target, link: &Link) -> Result<Edge, StoreError>;

    /// The relations within `depth` hops of `entity` in `view`.
    fn neighbors(&mut self, view: &View, entity: &str, depth: u8, limit: usize) -> Result<Neighborhood, StoreError>;

    /// Deletes a note (visible in `view`), or an entity or relation in the scope `target`.
    /// Returns the number of deleted rows (relations included for an entity).
    fn forget(&mut self, view: &View, target: &Target, what: &Forget) -> Result<usize, StoreError>;

    /// The note in the scope `target` (not the global scope too) whose title compares equal to
    /// `title` the way [`MemoryStore::remember`] compares titles. `harness-memory push` uses it to
    /// decide between adding, updating and skipping a note.
    ///
    /// The default searches for the title and reads the best exact match, which counts a hit and
    /// can miss when more than [`crate::limits::SEARCH_LIMIT_MAX`] notes outrank it; backends
    /// override it with an exact lookup that counts no hit.
    fn find_by_title(&mut self, target: &Target, title: &str) -> Result<Option<Note>, StoreError> {
        let wanted = clean::normalize(&clean::title(title)?);
        let view = match target {
            Target::Repo(key) => View {
                repo: Some(key.clone()),
                global: false,
            },
            Target::Global => View {
                repo: None,
                global: true,
            },
        };

        let query = SearchQuery {
            text: title.chars().take(crate::limits::QUERY_MAX).collect(),
            view: view.clone(),
            kind: None,
            limit: crate::limits::SEARCH_LIMIT_MAX,
            checkout: None,
        };

        let found = self
            .search(&query)?
            .into_iter()
            .find(|hit| clean::normalize(&hit.title) == wanted);

        match found {
            Some(hit) => Ok(self.get(&view, &[hit.id])?.into_iter().next()),
            None => Ok(None),
        }
    }
}
