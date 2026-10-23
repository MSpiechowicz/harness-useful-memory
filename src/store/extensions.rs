//! What the additive schema extensions of every backend share.
//!
//! The extensions (English search objects, the `lang` of a note, the settings) are added to a
//! database without a new schema version, so an older server keeps using it. Each backend adds
//! what is missing on every open (`ensure_extensions`), then fills `lang` (and on MongoDB the
//! English terms) of the notes an older server wrote, a page at a time for a bounded time per
//! open ([`BACKFILL_BUDGET`]). A database that does not let this server add them (no DDL right) is
//! used without them, with one line on stderr.

use std::sync::Once;
use std::time::Duration;

use super::StoreError;

/// Notes filled per backfill write.
pub(crate) const BACKFILL_PAGE: usize = 500;

/// How long one open spends on backfilling; the rest waits for the next open.
pub(crate) const BACKFILL_BUDGET: Duration = Duration::from_millis(1500);

/// The settings key of the English notes setting (the key of its row in every backend).
pub(crate) const ENGLISH_NOTES: &str = "english_notes";

/// Says once per process, on stderr (stdout is the MCP stream), that the extensions are not
/// available and why.
pub(crate) fn warn_unavailable(error: &StoreError) {
    static WARNED: Once = Once::new();

    WARNED.call_once(|| {
        eprintln!(
            "harness-memory: cannot add the search extensions to the memory database ({error}), continuing without \
             them: English stemming and the notes language setting are off for this database"
        );
    });
}

/// Says once per process that a backfill step failed (it is tried again at the next open).
pub(crate) fn warn_backfill(error: &StoreError) {
    static WARNED: Once = Once::new();

    WARNED.call_once(|| eprintln!("harness-memory: cannot fill the notes language yet ({error}), trying again later"));
}

/// The error of a setting that needs the extensions when the database has none.
pub(crate) fn unavailable_setting() -> StoreError {
    StoreError::Backend(
        "this database has no settings storage: this server could not add its extensions (no permission to change \
         the schema)"
            .to_owned(),
    )
}

/// The English notes setting a stored text means: off for `0` and `false`, else on (also when
/// nothing is stored).
pub(crate) fn english_notes_from(stored: Option<&str>) -> bool {
    !matches!(stored.map(str::trim), Some("0" | "false"))
}
