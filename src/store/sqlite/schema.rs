//! The database schema and its version (`PRAGMA user_version`), shared by the local file and the
//! libSQL backend.

use rusqlite::{Connection, TransactionBehavior};

use super::sql::{Sql, args};
use crate::store::StoreError;

/// The schema version this server writes.
pub const SCHEMA_VERSION: i64 = 2;

/// The state of a database after [`migrate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaState {
    /// At [`SCHEMA_VERSION`].
    Current,
    /// Written by a newer server; left untouched.
    Newer,
}

/// Version 1. Global rows have a NULL `project_id`; the unique indexes use `ifnull(project_id, 0)`
/// (project ids start at 1) so a title or entity name is unique per scope. Public so tests can build
/// a version 1 database and check the migration.
pub const V1: &str = "
CREATE TABLE projects (
    id INTEGER PRIMARY KEY,
    key TEXT NOT NULL UNIQUE,
    root_path TEXT NOT NULL,
    remote TEXT,
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL
);

CREATE TABLE notes (
    id INTEGER PRIMARY KEY,
    project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
    kind TEXT NOT NULL
        CHECK (kind IN ('outcome', 'problem', 'fix', 'optimisation', 'location', 'decision', 'note')),
    title TEXT NOT NULL,
    title_norm TEXT NOT NULL,
    body TEXT NOT NULL,
    tags TEXT NOT NULL DEFAULT '',
    source TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1,
    hits INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX notes_title ON notes (ifnull(project_id, 0), title_norm);
CREATE INDEX notes_recent ON notes (project_id, updated_at DESC);

CREATE VIRTUAL TABLE notes_fts USING fts5(
    title, body, tags,
    content = 'notes', content_rowid = 'id',
    tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TRIGGER notes_fts_insert AFTER INSERT ON notes BEGIN
    INSERT INTO notes_fts (rowid, title, body, tags) VALUES (new.id, new.title, new.body, new.tags);
END;
CREATE TRIGGER notes_fts_delete AFTER DELETE ON notes BEGIN
    INSERT INTO notes_fts (notes_fts, rowid, title, body, tags)
        VALUES ('delete', old.id, old.title, old.body, old.tags);
END;
CREATE TRIGGER notes_fts_update AFTER UPDATE OF title, body, tags ON notes BEGIN
    INSERT INTO notes_fts (notes_fts, rowid, title, body, tags)
        VALUES ('delete', old.id, old.title, old.body, old.tags);
    INSERT INTO notes_fts (rowid, title, body, tags) VALUES (new.id, new.title, new.body, new.tags);
END;

CREATE TABLE entities (
    id INTEGER PRIMARY KEY,
    project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    name_norm TEXT NOT NULL,
    kind TEXT
);
CREATE UNIQUE INDEX entities_name ON entities (ifnull(project_id, 0), name_norm);

CREATE TABLE relations (
    id INTEGER PRIMARY KEY,
    project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
    src INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    rel TEXT NOT NULL,
    dst INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    note_id INTEGER REFERENCES notes(id) ON DELETE SET NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (src, rel, dst)
);
CREATE INDEX relations_dst ON relations (dst);
CREATE INDEX relations_note ON relations (note_id);

CREATE TABLE note_entities (
    note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    entity_id INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    PRIMARY KEY (note_id, entity_id)
) WITHOUT ROWID;
CREATE INDEX note_entities_entity ON note_entities (entity_id);
";

/// Version 2: where the latest write to each note came from (the client stays in `source`). Existing
/// repository notes take their repository's last seen root, the best guess there is.
const V2: &str = "
ALTER TABLE notes ADD COLUMN writer_root TEXT;
ALTER TABLE notes ADD COLUMN via_override INTEGER NOT NULL DEFAULT 0;
UPDATE notes SET writer_root = (SELECT p.root_path FROM projects p WHERE p.id = notes.project_id)
    WHERE project_id IS NOT NULL;
";

/// The migrations in order, each with the version it brings the database to.
const STEPS: [(i64, &str); 2] = [(1, V1), (2, V2)];

/// Brings the database to [`SCHEMA_VERSION`]; a newer database is reported, not changed.
pub fn migrate(conn: &mut Connection) -> rusqlite::Result<SchemaState> {
    match user_version(conn)? {
        version if version > SCHEMA_VERSION => return Ok(SchemaState::Newer),
        SCHEMA_VERSION => return Ok(SchemaState::Current),
        _ => {}
    }

    // Re-read under the write lock: another server may have migrated meanwhile.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version = user_version(&tx)?;

    if version > SCHEMA_VERSION {
        return Ok(SchemaState::Newer);
    }

    for (step, sql) in STEPS {
        if version < step {
            tx.execute_batch(sql)?;
        }
    }

    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;

    tx.commit()?;
    Ok(SchemaState::Current)
}

/// `PRAGMA user_version`.
pub fn user_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

/// `PRAGMA user_version` through [`Sql`].
pub(crate) fn version_of(db: &mut dyn Sql) -> Result<i64, StoreError> {
    db.one("PRAGMA user_version", args![], |row| row.get(0))
}

/// Runs the migrations a database at `version` lacks and records [`SCHEMA_VERSION`], inside the
/// caller's write transaction (the statements [`migrate`] runs on a local file).
pub(crate) fn upgrade(db: &mut dyn Sql, version: i64) -> Result<(), StoreError> {
    for (step, sql) in STEPS {
        if version < step {
            db.batch(sql)?;
        }
    }

    db.batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
}
