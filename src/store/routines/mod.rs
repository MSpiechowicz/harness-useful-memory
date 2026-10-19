//! The team routine contract: Harness Desktop routine files (version 1) a team shares through the
//! team database, the [`DocKind::Routines`] kind of team documents ([`crate::store::team_docs`],
//! which describes the rows, the scopes and how rows changed by hand are read back).
//!
//! The body follows the desktop's routine file format (`docs/routines-format.md` there), checked
//! by [`format`] (the schedule's cron expressions by [`cron`]), with the team rules of hooks: no
//! `seed`, and a project routine's `project` holds the repository key instead of a local path.
//!
//! This server never runs a routine: Harness Desktop reads them through `harness-memory routines`
//! ([`crate::cli_team_docs`]); a team routine arrives turned off there, and each person turns it
//! on. [`RoutineStore`] is the team document contract of routines, which every [`TeamDocStore`]
//! fulfils.

pub mod cron;
pub mod format;

use super::StoreError;
use super::team_docs::{DocDelete, DocDraft, DocKind, DocPage, DocPut, TeamDoc, TeamDocStore, checked_doc};

/// A routine to store: the checked body and where it applies.
pub type RoutineDraft = DocDraft;

/// A stored team routine.
pub type TeamRoutine = TeamDoc;

/// What [`RoutineStore::put_routine`] did.
pub type RoutinePut = DocPut;

/// What [`RoutineStore::delete_routine`] did.
pub type RoutineDelete = DocDelete;

/// One page of [`RoutineStore::list_routines`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutinePage {
    /// The rows of the listed scopes in the database (a row left out counts too).
    pub total: usize,
    pub routines: Vec<TeamRoutine>,
    /// The offset of the next page: after the last row read for this one (left out rows
    /// included), `None` when no rows follow.
    pub next_offset: Option<usize>,
}

impl From<DocPage> for RoutinePage {
    fn from(page: DocPage) -> Self {
        Self {
            total: page.total,
            routines: page.docs,
            next_offset: page.next_offset,
        }
    }
}

/// A team routine backend: the [`TeamDocStore`] methods on [`DocKind::Routines`].
pub trait RoutineStore: Send {
    /// One page (`offset`, at most `limit` rows) of the global routines and those of the
    /// repository key `repo`: global first, then by `created_at`, then by id.
    fn list_routines(&mut self, repo: Option<&str>, offset: usize, limit: usize) -> Result<RoutinePage, StoreError>;

    /// The routine `id`, whatever its scope.
    fn get_routine(&mut self, id: &str) -> Result<Option<TeamRoutine>, StoreError>;

    /// Inserts `draft` when `expect` is `None`, else replaces the routine with its id while its
    /// hash is `expect` (see [`TeamDocStore::put_doc`]).
    fn put_routine(&mut self, draft: &RoutineDraft, expect: Option<&str>) -> Result<RoutinePut, StoreError>;

    /// Deletes the routine `id`, only while its hash is `expect` when that is given.
    fn delete_routine(&mut self, id: &str, expect: Option<&str>) -> Result<RoutineDelete, StoreError>;

    /// Deletes every routine `author` shared (see [`TeamDocStore::delete_docs_by`]).
    fn delete_routines_by(&mut self, author: &str) -> Result<Vec<String>, StoreError>;
}

impl<T: TeamDocStore + ?Sized> RoutineStore for T {
    fn list_routines(&mut self, repo: Option<&str>, offset: usize, limit: usize) -> Result<RoutinePage, StoreError> {
        self.list_docs(DocKind::Routines, repo, offset, limit)
            .map(RoutinePage::from)
    }

    fn get_routine(&mut self, id: &str) -> Result<Option<TeamRoutine>, StoreError> {
        self.get_doc(DocKind::Routines, id)
    }

    fn put_routine(&mut self, draft: &RoutineDraft, expect: Option<&str>) -> Result<RoutinePut, StoreError> {
        self.put_doc(DocKind::Routines, draft, expect)
    }

    fn delete_routine(&mut self, id: &str, expect: Option<&str>) -> Result<RoutineDelete, StoreError> {
        self.delete_doc(DocKind::Routines, id, expect)
    }

    fn delete_routines_by(&mut self, author: &str) -> Result<Vec<String>, StoreError> {
        self.delete_docs_by(DocKind::Routines, author)
    }
}

/// `draft` ready to store as a routine (see [`checked_doc`]).
pub fn checked_routine(draft: &RoutineDraft) -> Result<RoutineDraft, StoreError> {
    checked_doc(DocKind::Routines, draft)
}
