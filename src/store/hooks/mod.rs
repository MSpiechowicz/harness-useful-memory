//! The team hook contract: Harness Desktop hook files (version 1) a team shares through the team
//! database, the [`DocKind::Hooks`] kind of team documents ([`crate::store::team_docs`], which
//! describes the rows, the scopes and how rows changed by hand are read back).
//!
//! The body follows the desktop's file format with two team rules, checked by [`format`]: no
//! `seed` (Harness family hooks are never shared), and a project hook's `project` holds the
//! repository key instead of a local path. So only the repository identity leaves the machine.
//!
//! This server never runs a hook: Harness Desktop reads them through `harness-memory hooks`
//! ([`crate::cli_team_docs`]) and decides on its own whether one runs. [`HookStore`] is the team
//! document contract of hooks, which every [`TeamDocStore`] fulfils.

pub mod format;

use super::StoreError;
use super::team_docs::{DocDelete, DocDraft, DocKind, DocPage, DocPut, TeamDoc, TeamDocStore, checked_doc};

pub use super::team_docs::{DOC_REPO_MAX as HOOK_REPO_MAX, check_author, doc_sha256 as hook_sha256, is_sha256};

/// A hook to store: the checked body and where it applies.
pub type HookDraft = DocDraft;

/// A stored team hook.
pub type TeamHook = TeamDoc;

/// What [`HookStore::put_hook`] did.
pub type HookPut = DocPut;

/// What [`HookStore::delete_hook`] did.
pub type HookDelete = DocDelete;

/// One page of [`HookStore::list_hooks`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookPage {
    /// The rows of the listed scopes in the database (a row left out counts too).
    pub total: usize,
    pub hooks: Vec<TeamHook>,
    /// The offset of the next page: after the last row read for this one (left out rows
    /// included), `None` when no rows follow.
    pub next_offset: Option<usize>,
}

impl From<DocPage> for HookPage {
    fn from(page: DocPage) -> Self {
        Self {
            total: page.total,
            hooks: page.docs,
            next_offset: page.next_offset,
        }
    }
}

/// A team hook backend: the [`TeamDocStore`] methods on [`DocKind::Hooks`].
pub trait HookStore: Send {
    /// One page (`offset`, at most `limit` rows) of the global hooks and those of the repository
    /// key `repo`: global first, then by `created_at`, then by id.
    fn list_hooks(&mut self, repo: Option<&str>, offset: usize, limit: usize) -> Result<HookPage, StoreError>;

    /// The hook `id`, whatever its scope.
    fn get_hook(&mut self, id: &str) -> Result<Option<TeamHook>, StoreError>;

    /// Inserts `draft` when `expect` is `None`, else replaces the hook with its id while its
    /// [`hook_sha256`] is `expect` (see [`TeamDocStore::put_doc`]).
    fn put_hook(&mut self, draft: &HookDraft, expect: Option<&str>) -> Result<HookPut, StoreError>;

    /// Deletes the hook `id`, only while its [`hook_sha256`] is `expect` when that is given.
    fn delete_hook(&mut self, id: &str, expect: Option<&str>) -> Result<HookDelete, StoreError>;
}

impl<T: TeamDocStore + ?Sized> HookStore for T {
    fn list_hooks(&mut self, repo: Option<&str>, offset: usize, limit: usize) -> Result<HookPage, StoreError> {
        self.list_docs(DocKind::Hooks, repo, offset, limit).map(HookPage::from)
    }

    fn get_hook(&mut self, id: &str) -> Result<Option<TeamHook>, StoreError> {
        self.get_doc(DocKind::Hooks, id)
    }

    fn put_hook(&mut self, draft: &HookDraft, expect: Option<&str>) -> Result<HookPut, StoreError> {
        self.put_doc(DocKind::Hooks, draft, expect)
    }

    fn delete_hook(&mut self, id: &str, expect: Option<&str>) -> Result<HookDelete, StoreError> {
        self.delete_doc(DocKind::Hooks, id, expect)
    }
}

/// True for a hook id: 32 lowercase hex characters.
pub fn is_hook_id(text: &str) -> bool {
    super::team_docs::is_doc_id(text)
}

/// `draft` ready to store as a hook (see [`checked_doc`]).
pub fn checked_hook(draft: &HookDraft) -> Result<HookDraft, StoreError> {
    checked_doc(DocKind::Hooks, draft)
}
