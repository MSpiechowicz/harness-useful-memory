//! The [`MemoryStore`](crate::store::MemoryStore) operations shared by the SQLite-dialect stores.
//!
//! A store provides its transactions ([`Engine`]); these functions clean and check the input
//! first, so a rejected call never opens a write transaction, then run the shared statements.

use super::sql::Sql;
use super::{graph, listing, notes, search};
use crate::store::{Edge, Forget, Hit, Link, Neighborhood, Note, NoteDraft, NotePatch, Saved, SearchQuery, clean};
use crate::store::{Forgotten, NoteListQuery, NotePage, StoreError, Target, View};

/// The transactions of one SQLite-dialect store.
pub(crate) trait Engine {
    /// Runs `op` in a write transaction that takes the write lock first (`BEGIN IMMEDIATE`),
    /// committed when `op` succeeds and rolled back when it fails.
    fn write<T>(&mut self, op: impl FnOnce(&mut dyn Sql) -> Result<T, StoreError>) -> Result<T, StoreError>;

    /// Runs `op` without a transaction (each statement on its own).
    fn read<T>(&mut self, op: impl FnOnce(&mut dyn Sql) -> Result<T, StoreError>) -> Result<T, StoreError>;
}

pub(crate) fn remember(engine: &mut impl Engine, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError> {
    let draft = clean::draft(draft)?;
    engine.write(|db| notes::remember(db, target, &draft))
}

pub(crate) fn update(engine: &mut impl Engine, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError> {
    let patch = clean::patch(patch)?;
    engine.write(|db| notes::update(db, view, id, &patch))
}

/// A write: reading counts a hit on each note.
pub(crate) fn get(engine: &mut impl Engine, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
    engine.write(|db| notes::get(db, view, ids, true))
}

/// A read: counts no hit.
pub(crate) fn peek(engine: &mut impl Engine, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
    engine.read(|db| notes::get(db, view, ids, false))
}

pub(crate) fn list_notes(engine: &mut impl Engine, query: &NoteListQuery) -> Result<NotePage, StoreError> {
    engine.read(|db| listing::list(db, query))
}

pub(crate) fn search(engine: &mut impl Engine, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
    engine.read(|db| search::search(db, query))
}

pub(crate) fn similar(engine: &mut impl Engine, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
    engine.read(|db| search::similar(db, view, id, limit))
}

pub(crate) fn link(engine: &mut impl Engine, target: &Target, link: &Link) -> Result<Edge, StoreError> {
    let link = clean::link(link)?;
    engine.write(|db| graph::link(db, target, &link))
}

pub(crate) fn neighbors(
    engine: &mut impl Engine,
    view: &View,
    entity: &str,
    depth: u8,
    limit: usize,
) -> Result<Neighborhood, StoreError> {
    engine.read(|db| graph::neighbors(db, view, entity, depth, limit))
}

pub(crate) fn forget(
    engine: &mut impl Engine,
    view: &View,
    target: &Target,
    what: &Forget,
) -> Result<usize, StoreError> {
    let what = clean::forget(what)?;

    engine.write(|db| match &what {
        Forget::Note(id) => notes::forget(db, view, *id),
        Forget::Entity(name) => graph::forget_entity(db, target, name),
        Forget::Relation { from, relation, to } => graph::forget_relation(db, target, from, relation, to),
    })
}

pub(crate) fn forget_notes(engine: &mut impl Engine, view: &View) -> Result<Forgotten, StoreError> {
    engine.write(|db| notes::forget_all(db, view))
}

/// The note titled like `title` in the scope `target`, read exactly and without counting a hit.
pub(crate) fn find_by_title(
    engine: &mut impl Engine,
    target: &Target,
    title: &str,
) -> Result<Option<Note>, StoreError> {
    let title = clean::title(title)?;
    engine.read(|db| notes::find_by_title(db, target, &title))
}
