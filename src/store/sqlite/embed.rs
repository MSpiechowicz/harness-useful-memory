//! The notes' text for the embedder ([`crate::store::MemoryStore::notes_since`]) in the SQLite
//! dialect, shared by the local file and the libSQL backend.

use super::engine::Engine;
use super::sql::args;
use crate::store::{NoteText, StoreError};

pub(crate) fn notes_since(engine: &mut impl Engine, cursor: i64, limit: usize) -> Result<Vec<NoteText>, StoreError> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);

    engine.read(|db| {
        db.query(
            "SELECT n.id, p.key, n.title, n.body, n.tags, n.updated_at
             FROM notes n LEFT JOIN projects p ON p.id = n.project_id
             WHERE n.updated_at >= ?1 ORDER BY n.updated_at, n.id LIMIT ?2",
            args![cursor, limit],
        )?
        .iter()
        .map(|row| {
            let tags: String = row.get(4)?;

            Ok(NoteText {
                id: row.get(0)?,
                project: row.get(1)?,
                title: row.get(2)?,
                body: row.get(3)?,
                tags: tags.split_whitespace().map(str::to_owned).collect(),
                updated_at: row.get(5)?,
            })
        })
        .collect()
    })
}

pub(crate) fn note_ids(engine: &mut impl Engine) -> Result<Vec<i64>, StoreError> {
    engine.read(|db| db.column("SELECT id FROM notes ORDER BY id", &[]))
}
