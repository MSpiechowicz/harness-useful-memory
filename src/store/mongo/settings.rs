//! The settings of the team: the `settings` document in `meta`, `{_id: "settings", english_notes}`.

use mongodb::bson::{Document, doc};
use mongodb::sync::Database;

use super::META;
use crate::store::extensions::ENGLISH_NOTES;

/// The `_id` of the settings document in `meta`.
const SETTINGS_ID: &str = "settings";

/// The English notes setting; on when the document or the field is missing, or not a boolean.
pub(super) fn english_notes(db: &Database) -> mongodb::error::Result<bool> {
    let settings = db
        .collection::<Document>(META)
        .find_one(doc! { "_id": SETTINGS_ID })
        .run()?;

    Ok(settings
        .and_then(|settings| settings.get_bool(ENGLISH_NOTES).ok())
        .unwrap_or(true))
}

pub(super) fn set_english_notes(db: &Database, on: bool) -> mongodb::error::Result<()> {
    db.collection::<Document>(META)
        .update_one(doc! { "_id": SETTINGS_ID }, doc! { "$set": { ENGLISH_NOTES: on } })
        .upsert(true)
        .run()?;

    Ok(())
}
