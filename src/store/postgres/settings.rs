//! The settings of the team, rows of `harness_memory_meta` next to the schema version (the table
//! exists from the first migration on, so no schema extension is needed).

use postgres::GenericClient;

use super::{backend, column};
use crate::store::StoreError;
use crate::store::extensions::ENGLISH_NOTES;

/// The English notes setting: a row with 0 (off) or 1 (on); on when there is no row.
pub(super) fn english_notes(client: &mut impl GenericClient) -> Result<bool, StoreError> {
    let row = client
        .query_opt(
            "SELECT value FROM harness_memory_meta WHERE key = $1",
            &[&ENGLISH_NOTES],
        )
        .map_err(backend)?;

    Ok(row.map(|row| column::<i64>(&row, 0)).transpose()? != Some(0))
}

pub(super) fn set_english_notes(client: &mut impl GenericClient, on: bool) -> Result<(), StoreError> {
    client
        .execute(
            "INSERT INTO harness_memory_meta (key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            &[&ENGLISH_NOTES, &i64::from(on)],
        )
        .map_err(backend)?;

    Ok(())
}
