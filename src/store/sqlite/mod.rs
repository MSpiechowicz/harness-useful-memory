//! [`MemoryStore`] over one SQLite file (WAL, FTS5 full-text index, entity/relation graph).
//!
//! Several servers (one per running agent) share the file: every write runs in an `IMMEDIATE`
//! transaction and waits up to 5 s for the lock, so writers queue instead of failing with BUSY.
//! Input is cleaned and checked before that transaction opens, so a rejected call never holds it.

mod graph;
mod notes;
pub mod schema;
mod search;

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::{Edge, Forget, Hit, Link, MemoryStore, Neighborhood, Note, NoteDraft, NotePatch, ProjectKey};
use super::{Saved, SearchQuery, StoreError, Target, View};
use schema::SchemaState;

/// How long a statement waits for another server's lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Pause between attempts to initialize a database another server holds.
const BUSY_RETRY: Duration = Duration::from_millis(20);

/// The local backend. See the module docs.
pub struct SqliteStore {
    conn: Connection,
    newer: bool,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("newer", &self.newer)
            .finish_non_exhaustive()
    }
}

impl SqliteStore {
    /// Opens (creating when missing) the database at `path` and migrates it. On Unix a created
    /// parent folder is private (0700) and the file is 0600. A database written by a newer server
    /// opens, but every operation then fails with [`StoreError::NewerSchema`].
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        prepare_file(path)
            .map_err(|error| StoreError::Backend(format!("cannot create the database: {}", error.kind())))?;

        let mut conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;")?;

        // Switching to WAL and the first migration can report BUSY without waiting while another
        // server opens the same new file, so they are retried until the busy timeout.
        let deadline = Instant::now() + BUSY_TIMEOUT;
        let state = loop {
            match initialize(&mut conn) {
                Err(error) if is_busy(&error) && Instant::now() < deadline => thread::sleep(BUSY_RETRY),
                other => break other?,
            }
        };

        Ok(Self {
            conn,
            newer: state == SchemaState::Newer,
        })
    }

    /// True when the database was written by a newer server.
    pub fn is_newer(&self) -> bool {
        self.newer
    }

    fn conn(&mut self) -> Result<&mut Connection, StoreError> {
        if self.newer {
            return Err(StoreError::NewerSchema);
        }

        Ok(&mut self.conn)
    }

    fn write(&mut self) -> Result<Transaction<'_>, StoreError> {
        let conn = self.conn()?;
        Ok(conn.transaction_with_behavior(TransactionBehavior::Immediate)?)
    }
}

impl MemoryStore for SqliteStore {
    fn remember(&mut self, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError> {
        let draft = notes::prepare(draft)?;
        let tx = self.write()?;
        let saved = notes::remember(&tx, target, &draft)?;
        tx.commit()?;
        Ok(saved)
    }

    fn update(&mut self, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError> {
        let patch = notes::prepare_patch(patch)?;
        let tx = self.write()?;
        let saved = notes::update(&tx, view, id, &patch)?;
        tx.commit()?;
        Ok(saved)
    }

    fn get(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        let tx = self.write()?;
        let found = notes::get(&tx, view, ids)?;
        tx.commit()?;
        Ok(found)
    }

    fn search(&mut self, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
        search::search(self.conn()?, query)
    }

    fn similar(&mut self, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
        search::similar(self.conn()?, view, id, limit)
    }

    fn link(&mut self, target: &Target, link: &Link) -> Result<Edge, StoreError> {
        let link = graph::prepare(link)?;
        let tx = self.write()?;
        let edge = graph::link(&tx, target, &link)?;
        tx.commit()?;
        Ok(edge)
    }

    fn neighbors(&mut self, view: &View, entity: &str, depth: u8, limit: usize) -> Result<Neighborhood, StoreError> {
        graph::neighbors(self.conn()?, view, entity, depth, limit)
    }

    fn forget(&mut self, view: &View, target: &Target, what: &Forget) -> Result<usize, StoreError> {
        let what = graph::prepare_forget(what)?;
        let tx = self.write()?;
        let deleted = match &what {
            Forget::Note(id) => notes::forget(&tx, view, *id)?,
            Forget::Entity(name) => graph::forget_entity(&tx, target, name)?,
            Forget::Relation { from, relation, to } => graph::forget_relation(&tx, target, from, relation, to)?,
        };
        tx.commit()?;
        Ok(deleted)
    }
}

/// Switches to WAL and migrates.
fn initialize(conn: &mut Connection) -> rusqlite::Result<SchemaState> {
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    schema::migrate(conn)
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

/// Creates the parent folder (0700) and the file (0600) before SQLite opens it.
pub(crate) fn prepare_file(path: &Path) -> std::io::Result<()> {
    prepare_dir(path)?;

    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);

    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);

    options.open(path).map(drop)
}

/// Creates `path` (and its parent folder) as a new, empty, private file (0600 on Unix); fails when
/// anything, a link included, already has that name.
pub(crate) fn prepare_new_file(path: &Path) -> std::io::Result<()> {
    prepare_dir(path)?;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);

    options.open(path).map(drop)
}

/// Creates the parent folder of the database file `path` (0700 on Unix) when missing.
fn prepare_dir(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) else {
        return Ok(());
    };

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);

    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);

    builder.create(parent)
}

/// The current time in Unix seconds.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The SQL parameters of a view: the repository's project id (NULL when the repository has no
/// rows yet or is not visible) and whether global rows are visible.
fn view_params(conn: &Connection, view: &View) -> Result<(Option<i64>, bool), StoreError> {
    let repo = match &view.repo {
        Some(key) => project_id(conn, key)?,
        None => None,
    };

    Ok((repo, view.global))
}

/// The project id of `key`, if it has rows.
fn project_id(conn: &Connection, key: &ProjectKey) -> Result<Option<i64>, StoreError> {
    let id = conn
        .query_row("SELECT id FROM projects WHERE key = ?1", [&key.key], |row| row.get(0))
        .optional()?;

    Ok(id)
}

/// The project id a write to `target` uses (`None` for global), registering the repository.
fn target_project(tx: &Transaction<'_>, target: &Target) -> Result<Option<i64>, StoreError> {
    let Target::Repo(key) = target else {
        return Ok(None);
    };

    let id = tx.query_row(
        "INSERT INTO projects (key, root_path, remote, created_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT (key) DO UPDATE SET
             root_path = excluded.root_path, remote = excluded.remote, last_seen_at = excluded.last_seen_at
         RETURNING id",
        params![key.key, key.root.to_string_lossy(), key.remote, now()],
        |row| row.get(0),
    )?;

    Ok(Some(id))
}

/// The project id of an existing scope for reads and deletes (`Ok(None)` inside means global;
/// the outer `None` means the repository has no rows).
fn target_project_existing(conn: &Connection, target: &Target) -> Result<Option<Option<i64>>, StoreError> {
    match target {
        Target::Global => Ok(Some(None)),
        Target::Repo(key) => Ok(project_id(conn, key)?.map(Some)),
    }
}
