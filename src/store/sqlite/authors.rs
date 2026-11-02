//! Who wrote a note or a script (see [`crate::store::author`]) in the SQLite dialect: the four
//! author columns of the `notes` and `scripts` tables, which a database an older server made
//! (and a role that cannot add them) does not have.

use super::sql::{Row, Sql, args};
use crate::store::Authors;
use crate::store::StoreError;
use crate::store::clean::Writer;

/// The author columns of the table aliased `alias`, or four NULLs when the database has none.
pub(super) fn select(alias: &str, authors: bool) -> String {
    if !authors {
        return "NULL, NULL, NULL, NULL".to_owned();
    }

    format!("{alias}.added_by_name, {alias}.added_by_via, {alias}.updated_by_name, {alias}.updated_by_via")
}

/// The authors in the four columns of [`select`] that start at `first`.
pub(super) fn read(row: &Row, first: usize) -> Result<Authors, StoreError> {
    Ok(Authors::stored([
        row.get(first)?,
        row.get(first + 1)?,
        row.get(first + 2)?,
        row.get(first + 3)?,
    ]))
}

/// Stores who wrote row `id` of `table` (`notes` or `scripts`): the one who changed it last, and
/// when `created` the one who added it. An update leaves `added_by` as it is.
pub(super) fn set(db: &mut dyn Sql, table: &str, id: i64, writer: &Writer, created: bool) -> Result<(), StoreError> {
    let (updated_name, updated_via) = &writer.updated_by;

    if created {
        let (added_name, added_via) = &writer.added_by;

        db.execute(
            &format!(
                "UPDATE {table} SET added_by_name = ?2, added_by_via = ?3, updated_by_name = ?4, updated_by_via = ?5
                 WHERE id = ?1"
            ),
            args![
                id,
                added_name.as_deref(),
                added_via.as_deref(),
                updated_name.as_deref(),
                updated_via.as_deref()
            ],
        )?;

        return Ok(());
    }

    db.execute(
        &format!("UPDATE {table} SET updated_by_name = ?2, updated_by_via = ?3 WHERE id = ?1"),
        args![id, updated_name.as_deref(), updated_via.as_deref()],
    )?;

    Ok(())
}
