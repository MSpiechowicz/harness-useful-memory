//! Who wrote a note or a script (see [`crate::store::author`]) in PostgreSQL: the four author
//! columns of the `notes` and `scripts` tables, which a database an older server made (and a role
//! that cannot add them) does not have. Not `written_by`, which is the database role.

use postgres::{Row, Transaction};

use super::{SOURCE_MAX, backend, column, cut};
use crate::limits::TEAM_DOC_AUTHOR_MAX;
use crate::store::clean::Writer;
use crate::store::{Authors, StoreError};

/// The four author columns of the table aliased `alias` (cut to their limits), or four NULLs when
/// the database has none.
pub(super) fn select(alias: &str, authors: bool) -> String {
    if !authors {
        return "NULL::text, NULL::text, NULL::text, NULL::text".to_owned();
    }

    [
        ("added_by_name", TEAM_DOC_AUTHOR_MAX),
        ("added_by_via", SOURCE_MAX),
        ("updated_by_name", TEAM_DOC_AUTHOR_MAX),
        ("updated_by_via", SOURCE_MAX),
    ]
    .map(|(name, max)| cut(&format!("{alias}.{name}"), max))
    .join(", ")
}

/// The authors in the four columns of [`select`] that start at `first`.
pub(super) fn read(row: &Row, first: usize) -> Result<Authors, StoreError> {
    Ok(Authors::stored([
        column(row, first)?,
        column(row, first + 1)?,
        column(row, first + 2)?,
        column(row, first + 3)?,
    ]))
}

/// Stores who wrote row `id` of `table` (`notes` or `scripts`): the one who changed it last, and
/// when `created` the one who added it. An update leaves `added_by` as it is.
pub(super) fn set(
    tx: &mut Transaction<'_>,
    table: &str,
    id: i64,
    writer: &Writer,
    created: bool,
) -> Result<(), StoreError> {
    let (updated_name, updated_via) = &writer.updated_by;

    if created {
        let (added_name, added_via) = &writer.added_by;

        tx.execute(
            &format!(
                "UPDATE {table} SET added_by_name = $2, added_by_via = $3, updated_by_name = $4, updated_by_via = $5
                 WHERE id = $1"
            ),
            &[&id, added_name, added_via, updated_name, updated_via],
        )
        .map_err(backend)?;

        return Ok(());
    }

    tx.execute(
        &format!("UPDATE {table} SET updated_by_name = $2, updated_by_via = $3 WHERE id = $1"),
        &[&id, updated_name, updated_via],
    )
    .map_err(backend)?;

    Ok(())
}
