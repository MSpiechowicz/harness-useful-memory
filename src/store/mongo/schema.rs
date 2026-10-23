//! The collections, their indexes and the schema version (the `schema` document in `meta`).
//!
//! A migration runs under a lock document in `meta` (`migration_lock`, a lease of
//! [`LOCK_LEASE`] seconds, taken over once expired or invalid), so two servers starting at once do
//! not both migrate; the one that waits sees the version the other wrote.
//!
//! Versions: 1 the notes, entities and relations; 2 adds the scripts; 3 the team hooks; 4 the team
//! routines.
//!
//! On Amazon DocumentDB version 1 has no text index (DocumentDB cannot build it, see
//! [`super::documentdb`]); the version number is the same, as nothing else differs and no read
//! needs the index there.

use std::thread;
use std::time::{Duration, Instant};

use mongodb::IndexModel;
use mongodb::bson::{Document, doc, oid::ObjectId};
use mongodb::error::ErrorKind;
use mongodb::options::IndexOptions;
use mongodb::sync::Database;

use super::{ENTITIES, META, NOTES, RELATIONS, SCRIPTS, connection, driver_error, error_code, int, now, number};
use crate::store::StoreError;
use crate::store::team_docs::DocKind;

/// The schema version this server writes (independent of the SQLite schema version).
pub const SCHEMA_VERSION: i64 = 4;

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

/// The stored schema version (0 for a new database). On Amazon DocumentDB (`documentdb`), whose
/// `find` takes no expression in its projection, it is read with an aggregation.
pub fn version(db: &Database, documentdb: bool) -> mongodb::error::Result<i64> {
    let meta = db.collection::<Document>(META);
    let filter = doc! { "_id": SCHEMA_ID };
    let projection = doc! { "version": number("$version") };

    let schema = if documentdb {
        let pipeline = vec![
            doc! { "$match": filter },
            doc! { "$limit": 1_i64 },
            doc! { "$project": projection },
        ];
        meta.aggregate(pipeline).run()?.next().transpose()?
    } else {
        meta.find_one(filter).projection(projection).run()?
    };

    Ok(schema.as_ref().map_or(0, |schema| int(schema, "version")))
}

/// Brings the database to [`SCHEMA_VERSION`]; a newer database is reported, not changed.
/// Amazon DocumentDB (`documentdb`) gets no text index.
pub(super) fn migrate(db: &Database, documentdb: bool) -> Result<SchemaState, StoreError> {
    let failed = |error: mongodb::error::Error| driver_error(&error, documentdb);

    if let Some(state) = settled(version(db, documentdb).map_err(failed)?) {
        return Ok(state);
    }

    let owner = ObjectId::new();
    if !lock(db, owner, documentdb)? {
        // Another server migrated while this one waited.
        return Ok(settled(version(db, documentdb).map_err(failed)?).unwrap_or(SchemaState::Current));
    }

    let migrated = apply(db, documentdb);
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
fn lock(db: &Database, owner: ObjectId, documentdb: bool) -> Result<bool, StoreError> {
    let meta = db.collection::<Document>(META);
    let failed = |error: mongodb::error::Error| driver_error(&error, documentdb);
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
        // lease and a lease of clock skew) counts as expired, so it cannot block migrations. The
        // number types are named one by one, each in a plain `$type` under `$nor`: Amazon
        // DocumentDB may not know the `number` alias or take a `$type` list under `$not`.
        let not_a_number: Vec<Document> = ["int", "long", "double", "decimal"]
            .into_iter()
            .map(|kind| doc! { "expires_at": { "$type": kind } })
            .collect();
        let expired = doc! {
            "_id": LOCK_ID,
            "$or": [
                { "$nor": not_a_number },
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

        if version(db, documentdb).map_err(failed)? >= SCHEMA_VERSION {
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
fn apply(db: &Database, documentdb: bool) -> Result<SchemaState, StoreError> {
    let failed = |error: mongodb::error::Error| driver_error(&error, documentdb);
    let current = version(db, documentdb).map_err(failed)?;

    if let Some(state) = settled(current) {
        return Ok(state);
    }

    if current < 1 {
        create_v1(db).map_err(failed)?;

        if !documentdb {
            create_text_index(db)?;
        }
    }

    if current < 2 {
        create_v2(db).map_err(failed)?;
    }

    if current < 3 {
        create_team_docs(db, DocKind::Hooks).map_err(failed)?;
    }

    if current < 4 {
        create_team_docs(db, DocKind::Routines).map_err(failed)?;
    }

    db.collection::<Document>(META)
        .update_one(
            doc! { "_id": SCHEMA_ID },
            doc! { "$set": { "version": SCHEMA_VERSION } },
        )
        .upsert(true)
        .run()
        .map_err(failed)?;

    Ok(SchemaState::Current)
}

/// Version 1: the collections and their indexes (creating an index creates its collection), all
/// but the text index ([`create_text_index`]).
fn create_v1(db: &Database) -> mongodb::error::Result<()> {
    db.collection::<Document>(NOTES)
        .create_indexes([
            index(doc! { "project": 1, "title_norm": 1 }, "notes_title", true),
            index(doc! { "project": 1, "updated_at": -1 }, "notes_recent", false),
            index(doc! { "project": 1, "entities": 1 }, "notes_entities", false),
            index(doc! { "project": 1, "terms": 1 }, "notes_terms", false),
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

/// Version 1 on MongoDB: the text index on `title`, `body` and `tags` (weights 5 : 1 : 3, language
/// `none`). MongoDB builds it on any database, so a failure means a server that only looks like
/// MongoDB, such as Amazon DocumentDB (no text index on an array field such as `tags`, nor with the
/// language `none`): the error says to pick that kind.
fn create_text_index(db: &Database) -> Result<(), StoreError> {
    let text = IndexModel::builder()
        .keys(doc! { "title": "text", "body": "text", "tags": "text" })
        .options(
            IndexOptions::builder()
                .name("notes_text".to_owned())
                .weights(doc! { "title": 5, "body": 1, "tags": 3 })
                .default_language("none".to_owned())
                .build(),
        )
        .build();

    match db.collection::<Document>(NOTES).create_index(text).run() {
        Ok(_) => Ok(()),
        Err(error) if matches!(error.kind.as_ref(), ErrorKind::Command(_)) => Err(StoreError::Backend(format!(
            "{}. The notes text index could not be created, which happens on Amazon DocumentDB: set \
                 database_kind = \"documentdb\" in the config file (or {}=documentdb) and try again",
            connection::message(&error),
            crate::backend::KIND_VAR
        ))),
        Err(error) => Err(connection::backend(&error)),
    }
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

/// Version 3 the team hooks, version 4 the team routines: one collection per [`DocKind`], `_id`
/// the document id (so it is unique), listed per scope (`repo`, `null` for global ones) by
/// `created_at` (index `hooks_repo`, `routines_repo`). Each only adds a collection, so the
/// documents of the version before are untouched, and creating an existing index again is a
/// no-op.
fn create_team_docs(db: &Database, kind: DocKind) -> mongodb::error::Result<()> {
    db.collection::<Document>(kind.table())
        .create_index(index(
            doc! { "repo": 1, "created_at": 1, "_id": 1 },
            &format!("{}_repo", kind.table()),
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

/// The index on the English stems of a note, the sibling of `notes_terms`.
const TERMS_EN_INDEX: &str = "notes_terms_en";

/// Adds the `terms_en` index when the database lacks it, without a schema version (an older server
/// ignores the index and the `terms_en`, `lang` and `en_rev` fields). The existing `notes_text`
/// index is not touched. Creating an index that exists with the same keys is a no-op for the
/// server, so servers starting together need no lock.
pub(super) fn ensure_extensions(db: &Database, documentdb: bool) -> Result<(), StoreError> {
    let failed = |error: mongodb::error::Error| driver_error(&error, documentdb);
    let notes = db.collection::<Document>(NOTES);

    let names = notes.list_index_names().run().map_err(failed)?;

    if names.iter().any(|name| name == TERMS_EN_INDEX) {
        return Ok(());
    }

    notes
        .create_index(index(doc! { "project": 1, "terms_en": 1 }, TERMS_EN_INDEX, false))
        .run()
        .map(drop)
        .map_err(failed)
}
