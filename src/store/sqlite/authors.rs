//! Who wrote a note or a script (see [`crate::store::author`]) in the SQLite dialect: the four
//! author columns of the `notes` and `scripts` tables, which a database an older server made
//! (and a role that cannot add them) does not have.

use super::sql::{Row, Sql, args};
use crate::limits::TEAM_DOC_AUTHOR_MAX;
use crate::store::Authors;
use crate::store::StoreError;
use crate::store::clean::Writer;

/// The most bytes of a name read back: the limit's characters at up to four bytes each. The
/// database counts the bytes of the stored value, not its characters (`length` and `substr` count
/// characters, stop at a NUL, and take a lead byte and any run of continuation bytes for one), so
/// a value cannot hide its size.
const NAME_BYTES: usize = 4 * TEAM_DOC_AUTHOR_MAX;

/// The most bytes of a client name read back, as for a name (it keeps 64 characters).
const VIA_BYTES: usize = 4 * 64;

/// `column` whole when it is text of at most `bytes` bytes, else `NULL`: a value written by hand
/// may be a blob, a number or an enormous text, none of which a read may fail on or ship.
fn text_within(column: &str, bytes: usize) -> String {
    format!("CASE WHEN typeof({column}) = 'text' AND length(CAST({column} AS BLOB)) <= {bytes} THEN {column} END")
}

/// The name of whoever added the record of the table aliased `alias`, `NULL` when none is known
/// (not text, blank, or longer than any name this server writes), for the filters, the sort and
/// the facet. Only the length of a longer value is measured.
pub(super) fn added_by_key(alias: &str) -> String {
    format!(
        "nullif({}, '')",
        text_within(&format!("{alias}.added_by_name"), NAME_BYTES)
    )
}

/// The author columns of the table aliased `alias` (`NULL` unless text of a size this server
/// writes), or four NULLs when the database has none.
pub(super) fn select(alias: &str, authors: bool) -> String {
    if !authors {
        return "NULL, NULL, NULL, NULL".to_owned();
    }

    [
        ("added_by_name", NAME_BYTES),
        ("added_by_via", VIA_BYTES),
        ("updated_by_name", NAME_BYTES),
        ("updated_by_via", VIA_BYTES),
    ]
    .map(|(name, bytes)| text_within(&format!("{alias}.{name}"), bytes))
    .join(", ")
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
