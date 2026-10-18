//! The collections, their indexes and the schema version (the `schema` document in `meta`).
//!
//! A migration runs under a lock document in `meta` (`migration_lock`, a lease of
//! [`LOCK_LEASE`] seconds, taken over once expired or invalid), so two servers starting at once do
//! not both migrate; the one that waits sees the version the other wrote.
//!
//! Versions: 1 the notes, entities and relations; 2 adds the scripts; 3 the team hooks.

use std::thread;
use std::time::{Duration, Instant};

use mongodb::IndexModel;
use mongodb::bson::{Document, doc, oid::ObjectId};
use mongodb::options::IndexOptions;
use mongodb::sync::Database;

use super::{ENTITIES, HOOKS, META, NOTES, RELATIONS, SCRIPTS, connection, error_code, int, now, number};
use crate::store::StoreError;

/// The schema version this server writes (independent of the SQLite schema version).
pub const SCHEMA_VERSION: i64 = 3;

/// The `_id` of the schema document in `meta`; its `version` field holds the version.
pub const SCHEMA_ID: &str = "schema";

/// The `_id` of the migration lock in `meta`.
const LOCK_ID: &str = "migration_lock";

/// Seconds a migration lock stays valid without its holder.
const LOCK_LEASE: i64 = 60;

/// How long a server waits for another server's migration.
const LOCK_WAIT: Duration = Duration::from_secs(90);

/// Pause between looks at another server's migration lock.
const LOCK_POLL: Duration = Duration::from_millis(100);

/// The state of a database after [`migrate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaState {
    /// At [`SCHEMA_VERSION`].
    Current,
    /// Written by a newer server; left untouched.
    Newer,
}

/// The stored schema version (0 for a new database).
pub fn version(db: &Database) -> mongodb::error::Result<i64> {
    let schema = db
        .collection::<Document>(META)
        .find_one(doc! { "_id": SCHEMA_ID })
        .projection(doc! { "version": number("$version") })
        .run()?;

    Ok(schema.as_ref().map_or(0, |schema| int(schema, "version")))
}

/// Brings the database to [`SCHEMA_VERSION`]; a newer database is reported, not changed.
pub(super) fn migrate(db: &Database) -> Result<SchemaState, StoreError> {
    let failed = |error: mongodb::error::Error| connection::backend(&error);

    if let Some(state) = settled(version(db).map_err(failed)?) {
        return Ok(state);
    }

    let owner = ObjectId::new();
    if !lock(db, owner)? {
        // Another server migrated while this one waited.
        return Ok(settled(version(db).map_err(failed)?).unwrap_or(SchemaState::Current));
    }

    let migrated = apply(db).map_err(failed);
    let released = db
        .collection::<Document>(META)
        .delete_one(doc! { "_id": LOCK_ID, "owner": owner })
        .run();

    let state = migrated?;
    released.map_err(failed)?;
    Ok(state)
}

/// The state when `version` needs no migration.
fn settled(version: i64) -> Option<SchemaState> {
    match version {
        version if version > SCHEMA_VERSION => Some(SchemaState::Newer),
        SCHEMA_VERSION => Some(SchemaState::Current),
        _ => None,
    }
}

/// Takes the migration lock for `owner`; false when another server finished the migration while
/// this one waited for the lock.
fn lock(db: &Database, owner: ObjectId) -> Result<bool, StoreError> {
    let meta = db.collection::<Document>(META);
    let failed = |error: mongodb::error::Error| connection::backend(&error);
    let deadline = Instant::now() + LOCK_WAIT;

    loop {
        let now = now();
        let inserted = meta
            .insert_one(doc! { "_id": LOCK_ID, "owner": owner, "expires_at": now + LOCK_LEASE })
            .run();

        match inserted {
            Ok(_) => return Ok(true),
            Err(error) if error_code(&error) == Some(11000) => {}
            Err(error) => return Err(failed(error)),
        }

        // A lock whose holder stopped before releasing it is taken over once it expires. A lock
        // no server could have written (an expiry that is not a number, or further away than a
        // lease and a lease of clock skew) counts as expired, so it cannot block migrations.
        let expired = doc! {
            "_id": LOCK_ID,
            "$or": [
                { "expires_at": { "$not": { "$type": "number" } } },
                { "expires_at": { "$lt": now } },
                { "expires_at": { "$gt": now + 2 * LOCK_LEASE } },
            ],
        };
        let taken = meta
            .find_one_and_update(
                expired,
                doc! { "$set": { "owner": owner, "expires_at": now + LOCK_LEASE } },
            )
            .projection(doc! { "_id": 1 })
            .run()
            .map_err(failed)?;

        if taken.is_some() {
            return Ok(true);
        }

        if version(db).map_err(failed)? >= SCHEMA_VERSION {
            return Ok(false);
        }

        if Instant::now() >= deadline {
            return Err(StoreError::Backend(
                "another server has been migrating the memory database for too long; try again".to_owned(),
            ));
        }

        thread::sleep(LOCK_POLL);
    }
}

/// Runs the missing migrations under the lock (re-reading the version first).
fn apply(db: &Database) -> mongodb::error::Result<SchemaState> {
    let current = version(db)?;

    if let Some(state) = settled(current) {
        return Ok(state);
    }

    if current < 1 {
        create_v1(db)?;
    }

    if current < 2 {
        create_v2(db)?;
    }

    if current < 3 {
        create_v3(db)?;
    }

    db.collection::<Document>(META)
        .update_one(
            doc! { "_id": SCHEMA_ID },
            doc! { "$set": { "version": SCHEMA_VERSION } },
        )
        .upsert(true)
        .run()?;

    Ok(SchemaState::Current)
}

/// Version 1: the collections and their indexes (creating an index creates its collection).
fn create_v1(db: &Database) -> mongodb::error::Result<()> {
    db.collection::<Document>(NOTES)
        .create_indexes([
            index(doc! { "project": 1, "title_norm": 1 }, "notes_title", true),
            index(doc! { "project": 1, "updated_at": -1 }, "notes_recent", false),
            index(doc! { "project": 1, "entities": 1 }, "notes_entities", false),
            index(doc! { "project": 1, "terms": 1 }, "notes_terms", false),
            IndexModel::builder()
                .keys(doc! { "title": "text", "body": "text", "tags": "text" })
                .options(
                    IndexOptions::builder()
                        .name("notes_text".to_owned())
                        .weights(doc! { "title": 5, "body": 1, "tags": 3 })
                        .default_language("none".to_owned())
                        .build(),
                )
                .build(),
        ])
        .run()?;

    db.collection::<Document>(ENTITIES)
        .create_index(index(doc! { "project": 1, "name_norm": 1 }, "entities_name", true))
        .run()?;

    db.collection::<Document>(RELATIONS)
        .create_indexes([
            index(
                doc! { "project": 1, "src": 1, "rel": 1, "dst": 1 },
                "relations_key",
                true,
            ),
            index(doc! { "project": 1, "dst": 1 }, "relations_dst", false),
            index(doc! { "note_id": 1 }, "relations_note", false),
        ])
        .run()?;

    Ok(())
}

/// Version 2: the scripts. `null` is a value of its own in a MongoDB index, so the unique
/// `(project, name)` index also keeps global script names unique (every script stores `project`,
/// `null` for global ones). Creating an existing index again is a no-op.
fn create_v2(db: &Database) -> mongodb::error::Result<()> {
    db.collection::<Document>(SCRIPTS)
        .create_index(index(doc! { "project": 1, "name": 1 }, "scripts_name", true))
        .run()?;

    Ok(())
}

/// Version 3: the team hooks, `_id` the hook id (so it is unique), listed per scope (`repo`,
/// `null` for global hooks) by `created_at`. Only adds a collection, so the documents of version
/// 2 are untouched.
fn create_v3(db: &Database) -> mongodb::error::Result<()> {
    db.collection::<Document>(HOOKS)
        .create_index(index(
            doc! { "repo": 1, "created_at": 1, "_id": 1 },
            "hooks_repo",
            false,
        ))
        .run()?;

    Ok(())
}

fn index(keys: Document, name: &str, unique: bool) -> IndexModel {
    IndexModel::builder()
        .keys(keys)
        .options(IndexOptions::builder().name(name.to_owned()).unique(unique).build())
        .build()
}
