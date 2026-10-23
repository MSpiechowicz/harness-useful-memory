//! The English fields of the notes an older server wrote (see [`super::notes`]): `terms_en`,
//! `lang` and `en_rev`, the revision they describe. An older server inserts a note without them
//! and edits a note without refreshing them (it raises `revision` only), so a note needs them when
//! its `en_rev` is not its `revision`.
//!
//! The pass reads only `_id`, `revision` and `en_rev` of every note, a page at a time, and
//! rewrites the fields of the stale ones in place, guarded by the revision they were read at. It
//! stops after [`BACKFILL_BUDGET`]; the rest waits for the next open.

use std::time::Instant;

use mongodb::bson::{Document, doc};
use mongodb::sync::Database;

use super::notes::english_fields;
use super::search::terms;
use super::{Ctx, Fail, NOTES, int, note_fields, number, strings, text};
use crate::store::extensions::{BACKFILL_BUDGET, BACKFILL_PAGE, warn_backfill};

/// Fills the English fields of the stale notes. A failure is reported once and ends the pass.
pub(super) fn fill(db: &Database, documentdb: bool) {
    let mut ctx = Ctx::new(db, None, documentdb);

    if let Err(fail) = pass(&mut ctx) {
        warn_backfill(&fail.into_store(documentdb));
    }
}

fn pass(ctx: &mut Ctx<'_>) -> Result<(), Fail> {
    let deadline = Instant::now() + BACKFILL_BUDGET;
    let page = i64::try_from(BACKFILL_PAGE).unwrap_or(i64::MAX);
    let mut after = 0_i64;

    while Instant::now() < deadline {
        let scanned = ctx.find(
            NOTES,
            doc! { "_id": { "$gt": after } },
            doc! { "_id": 1 },
            page,
            doc! { "_id": number("$_id"), "revision": number("$revision"), "en_rev": number("$en_rev") },
        )?;

        let stale: Vec<i64> = scanned
            .iter()
            .filter(|note| int(note, "revision") != int(note, "en_rev"))
            .map(|note| int(note, "_id"))
            .collect();

        if !stale.is_empty() {
            refresh(ctx, &stale)?;
        }

        let Some(last) = scanned.last() else {
            return Ok(());
        };

        after = int(last, "_id");

        if scanned.len() < BACKFILL_PAGE {
            return Ok(());
        }
    }

    Ok(())
}

/// Rewrites the English fields of the notes `ids` at the revision they were read at; a note
/// changed meanwhile is left for the next pass.
fn refresh(ctx: &mut Ctx<'_>, ids: &[i64]) -> Result<(), Fail> {
    let notes = ctx.find(NOTES, doc! { "_id": { "$in": ids } }, Document::new(), 0, note_fields())?;

    for note in &notes {
        let title = text(note, "title").unwrap_or_default();
        let body = text(note, "body").unwrap_or_default();
        let revision = int(note, "revision");
        let words = terms(&title, &body, &strings(note, "tags"));

        ctx.update_one(
            NOTES,
            doc! { "_id": int(note, "_id"), "revision": revision },
            doc! { "$set": english_fields(&title, &body, &words, revision) },
        )?;
    }

    Ok(())
}
