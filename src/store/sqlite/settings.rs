//! The `settings` table of the SQLite-dialect stores (added with the schema extensions, see
//! [`super::schema::ensure_extensions`]).

use super::sql::{Sql, args};
use crate::store::StoreError;
use crate::store::extensions::{ENGLISH_NOTES, english_notes_from};

/// The English notes setting; on when nothing is stored.
pub(super) fn english_notes(db: &mut dyn Sql) -> Result<bool, StoreError> {
    let stored: Option<String> = db.value("SELECT value FROM settings WHERE key = ?1", args![ENGLISH_NOTES])?;

    Ok(english_notes_from(stored.as_deref()))
}

pub(super) fn set_english_notes(db: &mut dyn Sql, on: bool) -> Result<(), StoreError> {
    db.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        args![ENGLISH_NOTES, if on { "1" } else { "0" }],
    )?;

    Ok(())
}
