//! The notes' text for the embedder ([`crate::store::MemoryStore::notes_since`]) in PostgreSQL,
//! cut to the limits by the server like every other read.

use postgres::GenericClient;

use super::notes::note_columns;
use super::{backend, column};
use crate::store::{NoteText, StoreError};

pub(super) fn notes_since(
    client: &mut impl GenericClient,
    cursor: i64,
    limit: usize,
) -> Result<Vec<NoteText>, StoreError> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let sql = format!(
        "SELECT {} FROM notes n LEFT JOIN projects p ON p.id = n.project_id
         WHERE n.updated_at >= $1 ORDER BY n.updated_at, n.id LIMIT $2",
        note_columns()
    );

    client
        .query(&sql, &[&cursor, &limit])
        .map_err(backend)?
        .iter()
        .map(|row| {
            let tags: String = column(row, 6)?;

            Ok(NoteText {
                id: column(row, 0)?,
                project: column(row, 2)?,
                title: column(row, 4)?,
                body: column(row, 5)?,
                tags: tags.split_whitespace().map(str::to_owned).collect(),
                updated_at: column(row, 9)?,
            })
        })
        .collect()
}

pub(super) fn note_ids(client: &mut impl GenericClient) -> Result<Vec<i64>, StoreError> {
    client
        .query("SELECT id FROM notes ORDER BY id", &[])
        .map_err(backend)?
        .iter()
        .map(|row| column(row, 0))
        .collect()
}
