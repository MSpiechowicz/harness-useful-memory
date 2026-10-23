//! The [`MemoryStore`](crate::store::MemoryStore) operations shared by the SQLite-dialect stores.
//!
//! A store provides its transactions ([`Engine`]); these functions clean and check the input
//! first, so a rejected call never opens a write transaction, then run the shared statements.

use std::time::Instant;

use super::sql::Sql;
use super::{graph, listing, notes, schema, search, settings};
use crate::store::extensions::{BACKFILL_BUDGET, BACKFILL_PAGE, unavailable_setting, warn_backfill, warn_unavailable};
use crate::store::listing::excluded_json;
use crate::store::{Edge, Forget, Hit, Link, Neighborhood, Note, NoteDraft, NotePatch, Saved, SearchQuery, clean};
use crate::store::{Forgotten, NoteListQuery, NotePage, StoreError, Target, View};

/// The transactions of one SQLite-dialect store.
pub(crate) trait Engine {
    /// Runs `op` in a write transaction that takes the write lock first (`BEGIN IMMEDIATE`),
    /// committed when `op` succeeds and rolled back when it fails.
    fn write<T>(&mut self, op: impl FnOnce(&mut dyn Sql) -> Result<T, StoreError>) -> Result<T, StoreError>;

    /// Runs `op` without a transaction (each statement on its own).
    fn read<T>(&mut self, op: impl FnOnce(&mut dyn Sql) -> Result<T, StoreError>) -> Result<T, StoreError>;

    /// True when the database has the additive schema extensions (see [`install`]): without them
    /// no statement may name `notes_fts_en`, `settings` or `notes.lang`.
    fn extensions(&self) -> bool;
}

pub(crate) fn remember(engine: &mut impl Engine, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError> {
    let draft = clean::draft(draft)?;
    let extensions = engine.extensions();
    engine.write(|db| notes::remember(db, target, &draft, extensions))
}

pub(crate) fn update(engine: &mut impl Engine, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError> {
    let patch = clean::patch(patch)?;
    let extensions = engine.extensions();
    engine.write(|db| notes::update(db, view, id, &patch, extensions))
}

/// A write: reading counts a hit on each note.
pub(crate) fn get(engine: &mut impl Engine, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
    let extensions = engine.extensions();
    engine.write(|db| notes::get(db, view, ids, true, extensions))
}

/// A read: counts no hit.
pub(crate) fn peek(engine: &mut impl Engine, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
    let extensions = engine.extensions();
    engine.read(|db| notes::get(db, view, ids, false, extensions))
}

pub(crate) fn list_notes(engine: &mut impl Engine, query: &NoteListQuery) -> Result<NotePage, StoreError> {
    let extensions = engine.extensions();
    engine.read(|db| listing::list(db, query, extensions))
}

pub(crate) fn search(engine: &mut impl Engine, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
    engine.read(|db| search::search(db, query))
}

pub(crate) fn similar(engine: &mut impl Engine, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
    engine.read(|db| search::similar(db, view, id, limit))
}

pub(crate) fn similar_text(
    engine: &mut impl Engine,
    view: &View,
    text: (&str, &str),
    limit: usize,
) -> Result<Vec<Hit>, StoreError> {
    engine.read(|db| search::resembling(db, view, text, None, limit))
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

pub(crate) fn forget_notes(engine: &mut impl Engine, view: &View, exclude: &[i64]) -> Result<Forgotten, StoreError> {
    let exclude = excluded_json(exclude)?;
    engine.write(|db| notes::forget_all(db, view, &exclude))
}

/// The note titled like `title` in the scope `target`, read exactly and without counting a hit.
pub(crate) fn find_by_title(
    engine: &mut impl Engine,
    target: &Target,
    title: &str,
) -> Result<Option<Note>, StoreError> {
    let title = clean::title(title)?;
    let extensions = engine.extensions();
    engine.read(|db| notes::find_by_title(db, target, &title, extensions))
}

/// Adds the schema extensions the database lacks (see [`schema::ensure_extensions`]) and reports
/// whether it has them now. A database that refuses them is used without, with one line on stderr.
pub(crate) fn install(engine: &mut impl Engine) -> bool {
    match engine.read(schema::extensions_present) {
        Ok(true) => return true,
        Ok(false) => {}
        Err(error) => {
            warn_unavailable(&error);
            return false;
        }
    }

    match engine.write(schema::ensure_extensions) {
        Ok(()) => true,
        Err(error) => {
            warn_unavailable(&error);
            false
        }
    }
}

/// Fills `lang` of the notes an older server wrote, a page at a time and for at most
/// [`BACKFILL_BUDGET`]; what is left waits for the next open. A failure is reported once.
pub(crate) fn backfill(engine: &mut impl Engine) {
    if !engine.extensions() {
        return;
    }

    let deadline = Instant::now() + BACKFILL_BUDGET;

    while Instant::now() < deadline {
        let step = match engine.read(notes::has_pending_lang) {
            Ok(true) => engine.write(|db| notes::fill_lang(db, BACKFILL_PAGE)),
            Ok(false) => return,
            Err(error) => Err(error),
        };

        match step {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) => return warn_backfill(&error),
        }
    }
}

/// The English notes setting; on without extensions.
pub(crate) fn english_notes(engine: &mut impl Engine) -> Result<bool, StoreError> {
    if !engine.extensions() {
        return Ok(true);
    }

    engine.read(settings::english_notes)
}

pub(crate) fn set_english_notes(engine: &mut impl Engine, on: bool) -> Result<(), StoreError> {
    if !engine.extensions() {
        return Err(unavailable_setting());
    }

    engine.write(|db| settings::set_english_notes(db, on))
}
