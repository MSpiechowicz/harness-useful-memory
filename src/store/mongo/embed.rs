//! The notes' text for the embedder ([`crate::store::MemoryStore::notes_since`]) in MongoDB and
//! Amazon DocumentDB, read with the same bounded fields as every other note read.

use mongodb::bson::doc;

use super::notes::stored;
use super::{Ctx, Fail, NOTES, id_only, int, note_fields};
use crate::store::NoteText;

pub(super) fn notes_since(ctx: &mut Ctx<'_>, cursor: i64, limit: usize) -> Result<Vec<NoteText>, Fail> {
    let found = ctx.find(
        NOTES,
        doc! { "updated_at": { "$gte": cursor } },
        doc! { "updated_at": 1, "_id": 1 },
        i64::try_from(limit).unwrap_or(i64::MAX),
        note_fields(),
    )?;

    Ok(found
        .iter()
        .map(|document| {
            let note = stored(document).note;

            NoteText {
                id: note.id,
                project: note.project,
                title: note.title,
                body: note.body,
                tags: note.tags,
                updated_at: note.updated_at,
            }
        })
        .collect())
}

pub(super) fn note_ids(ctx: &mut Ctx<'_>) -> Result<Vec<i64>, Fail> {
    let found = ctx.find(NOTES, doc! {}, doc! { "_id": 1 }, 0, id_only())?;
    Ok(found.iter().map(|document| int(document, "_id")).collect())
}
