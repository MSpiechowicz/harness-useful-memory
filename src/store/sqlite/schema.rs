//! The database schema and its version (`PRAGMA user_version`), shared by the local file and the
//! libSQL backend.

use rusqlite::{Connection, TransactionBehavior};

use super::sql::{Sql, args};
use crate::store::StoreError;

/// The schema version this server writes.
pub const SCHEMA_VERSION: i64 = 5;

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
/// repository notes take their repository's last seen root, the best guess there is. Public so tests
/// can build a version 2 database and check the migration.
pub const V2: &str = "
ALTER TABLE notes ADD COLUMN writer_root TEXT;
ALTER TABLE notes ADD COLUMN via_override INTEGER NOT NULL DEFAULT 0;
UPDATE notes SET writer_root = (SELECT p.root_path FROM projects p WHERE p.id = notes.project_id)
    WHERE project_id IS NOT NULL;
";

/// Version 3: scripts, unique by name per scope like note titles (`ifnull(project_id, 0)`), with
/// the provenance columns of notes and their run statistics; the script versions this machine
/// trusts, by script (where it is kept and its id there) and hash, with the name, scope and
/// repository key it was trusted as, its body (for the next review's diff) and whether it was
/// saved or approved here; and the hashes of the versions reported here as waiting for approval
/// (both used in the local file only; a libSQL team database gets the tables but never fills
/// them). Public so tests can build a version 3 database and check the migration.
pub const V3: &str = "
CREATE TABLE scripts (
    id INTEGER PRIMARY KEY,
    project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    language TEXT NOT NULL CHECK (language IN ('sh', 'bash', 'python', 'node', 'bun')),
    body TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    timeout_secs INTEGER NOT NULL,
    source TEXT,
    writer_root TEXT,
    via_override INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    run_count INTEGER NOT NULL DEFAULT 0,
    last_run_at INTEGER,
    last_exit_code INTEGER
);
CREATE UNIQUE INDEX scripts_name ON scripts (ifnull(project_id, 0), name);

CREATE TABLE script_trust (
    place TEXT NOT NULL,
    script_id INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    trusted_at TEXT NOT NULL,
    name TEXT NOT NULL,
    scope TEXT NOT NULL CHECK (scope IN ('repo', 'global')),
    project TEXT,
    language TEXT NOT NULL,
    body TEXT NOT NULL,
    origin TEXT NOT NULL CHECK (origin IN ('saved', 'approved')),
    PRIMARY KEY (place, script_id, sha256)
);
CREATE INDEX script_trust_script ON script_trust (place, script_id, trusted_at);

CREATE TABLE script_seen_unapproved (
    sha256 TEXT PRIMARY KEY,
    seen_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX script_seen_unapproved_age ON script_seen_unapproved (seen_at);
";

/// Version 4: the hooks a team shares (see [`crate::store::hooks`]), unique by id, with the
/// repository key they apply to (`NULL` for global hooks; a key, not a `projects` row, so no
/// local path goes with it), listed per scope by `created_at` (used in a libSQL team database
/// only; the local file gets the table but never fills it). Only adds a table, so the rows of
/// version 3 are untouched. Public so tests can build a version 4 database and check the
/// migration.
pub const V4: &str = "
CREATE TABLE hooks (
    id TEXT PRIMARY KEY,
    repo TEXT,
    body TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    author TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX hooks_repo ON hooks (repo, created_at, id);
";

/// Version 5: the routines a team shares (see [`crate::store::routines`]), with the columns and
/// index of `hooks` (used in a libSQL team database only, like `hooks`). Only adds a table, so
/// the rows of version 4 are untouched. `IF NOT EXISTS`, so a database whose version was set back
/// by hand without dropping the table still migrates.
const V5: &str = "
CREATE TABLE IF NOT EXISTS routines (
    id TEXT PRIMARY KEY,
    repo TEXT,
    body TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    author TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS routines_repo ON routines (repo, created_at, id);
";

/// The migrations in order, each with the version it brings the database to.
const STEPS: [(i64, &str); 5] = [(1, V1), (2, V2), (3, V3), (4, V4), (5, V5)];

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
