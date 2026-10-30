//! The SQLite file behind the code index: its schema and the writes of a refresh.
//!
//! One file (`code.sqlite3`, next to the notes database) holds many repositories, keyed by `repos.id`.
//! The schema is versioned with `PRAGMA user_version`. A file of an older version is dropped and rebuilt,
//! which makes the next refresh of each repository a full build. A file of a newer version (written by a
//! newer harness-memory) is left untouched and refused, so an older binary never wipes it.

use std::collections::HashMap;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use super::extract::Extract;
use super::graph::{MAX_REFS, MAX_SYMS};
use super::packages::Package;
use super::walk::{Entry, TOO_LARGE};

/// The schema version in `PRAGMA user_version`. A change of what the extractors write (not only of the
/// tables) counts too: files whose stamp did not move are not read again, so the index is rebuilt.
/// 2: receiver types (`binds`, `symbols.ret`, `refs.recv`) and constant mentions.
/// 3: `repos.used_at` (repositories unused for 30 days are dropped), per-file caps on what is written.
/// 4: per-repository totals and the too-large mark on `repos` (a repository past the caps stops growing).
/// 5: bindings count in the per-repository totals too (`repos.binds`), and every stored string is at most
/// 256 bytes.
/// 6: the worker process: `repos.validated_generation` (the generation a worker loaded and ranked within
/// its limits), `repos.failed` (the walk at which a worker broke) and `builds.pid`.
/// 7: the value of a string literal is not kept in a signature (`const KEY = "…"`).
/// 8: no string literal is kept in any signature, and a string reference that names no identifier is not
/// stored; `repos.validated_bytes` (the size of the graph a worker validated).
/// 9: string literals in type positions are shown again, values stay blanked.
/// 10: the literal scan follows comments, parenthesised return types, Python lambda parameters and a `/` that
/// may open a regular expression (a literal after one used to be kept).
/// 11: `prop` references, a function kept in a property of an object literal (`{ key: fn }`).
/// 12: `word` references, a short plain-word string literal inside a definition (`"doctor"`, `"git status"`).
/// 13: `base` references (a type extends or implements another: Python base classes, TS `extends` and
/// `implements`, Rust `impl Trait for Type`), and the name a Python `with` statement binds (`as client`).
/// 14: `refs.handler`, a mention of a constant written where something is handled (a `case` label, an operand
/// of an equality test, the pattern of an `if let`): a shared constant bridges `code_path` only to those.
pub const SCHEMA_VERSION: i64 = 14;

/// The refusal of a file written by a newer harness-memory.
pub const NEWER: &str = "the code index was written by a newer harness-memory";

/// How long a statement waits for another process's write lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(60);

/// How long one attempt to take the write lock waits (callers that can do something else try again).
const WRITE_SLICE: Duration = Duration::from_millis(250);

/// How long a build lets go of the write lock between two batches, so a session that waits for it (its
/// busy handler polls every 100 ms at most) gets a turn.
const YIELD: Duration = Duration::from_millis(120);

/// A build lets go of the lock like that once it held it this long, so a short build is not slowed down and
/// a waiter gets in well within [`PATIENCE`].
const HOLD_MAX: Duration = Duration::from_millis(1_500);

/// The default of [`Store::patience`].
const PATIENCE: Duration = Duration::from_secs(2);

/// A repository is dropped from the file when nothing used it for this many seconds.
pub const UNUSED_SECONDS: i64 = 30 * 24 * 3600;

/// `repos.used_at` is written at most this often per repository.
pub const USED_EVERY_SECONDS: i64 = 24 * 3600;

/// Pause between attempts to initialise a file another process is initialising.
const BUSY_RETRY: Duration = Duration::from_millis(20);

/// The `files.lang` of a manifest (no symbols).
pub const PACKAGE_LANG: &str = "pkg";

const TABLES: &[&str] = &[
    "builds", "packages", "imports", "binds", "refs", "symbols", "files", "repos",
];

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS repos(
  id INTEGER PRIMARY KEY, root TEXT NOT NULL UNIQUE, key TEXT,
  generation INTEGER NOT NULL DEFAULT 0, packages_hash TEXT NOT NULL DEFAULT '', indexed_at INTEGER NOT NULL DEFAULT 0,
  used_at INTEGER NOT NULL DEFAULT 0,
  syms INTEGER NOT NULL DEFAULT 0, refs INTEGER NOT NULL DEFAULT 0, imports INTEGER NOT NULL DEFAULT 0,
  binds INTEGER NOT NULL DEFAULT 0, too_large TEXT,
  validated_generation INTEGER NOT NULL DEFAULT -1, failed TEXT, validated_bytes INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS files(
  id INTEGER PRIMARY KEY, repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  path TEXT NOT NULL, hash TEXT NOT NULL, mtime_ns INTEGER NOT NULL, size INTEGER NOT NULL, lang TEXT NOT NULL,
  UNIQUE(repo_id, path));
CREATE TABLE IF NOT EXISTS symbols(
  id INTEGER PRIMARY KEY, repo_id INTEGER NOT NULL, file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  name TEXT NOT NULL, kind TEXT NOT NULL, parent TEXT, line INTEGER NOT NULL, end_line INTEGER NOT NULL,
  sig TEXT NOT NULL, ret TEXT, test INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS refs(
  id INTEGER PRIMARY KEY, repo_id INTEGER NOT NULL, file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  from_symbol INTEGER REFERENCES symbols(id) ON DELETE CASCADE,
  name TEXT NOT NULL, kind TEXT NOT NULL, qual TEXT, recv TEXT, line INTEGER NOT NULL,
  handler INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS binds(
  repo_id INTEGER NOT NULL, file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  scope_symbol INTEGER, name TEXT NOT NULL, type TEXT NOT NULL, kind TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS imports(
  repo_id INTEGER NOT NULL, file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  local TEXT NOT NULL, orig TEXT NOT NULL, source TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS packages(
  repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  kind TEXT NOT NULL, name TEXT NOT NULL, dir TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS builds(
  id INTEGER PRIMARY KEY, repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  kind TEXT NOT NULL, status TEXT NOT NULL, started_at INTEGER NOT NULL, finished_at INTEGER,
  files_total INTEGER NOT NULL DEFAULT 0, files_done INTEGER NOT NULL DEFAULT 0, error TEXT,
  pid INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS symbols_name ON symbols(repo_id, name);
CREATE INDEX IF NOT EXISTS symbols_file ON symbols(file_id);
CREATE INDEX IF NOT EXISTS refs_file ON refs(file_id);
CREATE INDEX IF NOT EXISTS binds_file ON binds(file_id);
CREATE INDEX IF NOT EXISTS imports_file ON imports(file_id);
CREATE INDEX IF NOT EXISTS packages_repo ON packages(repo_id);
";

/// The current time in Unix seconds.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
}

/// The first 32 hex digits of the SHA-256 of `bytes` (stable across Rust versions and platforms).
pub fn digest_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A database error as a message (SQLite messages hold no file contents).
pub fn db(error: rusqlite::Error) -> String {
    format!("code index database: {error}")
}

/// The stamp of an indexed file.
#[derive(Debug, Clone)]
pub struct Stamp {
    pub id: i64,
    pub hash: String,
    pub mtime_ns: i64,
    pub size: i64,
    pub lang: String,
}

impl Stamp {
    /// True when the file on disk is not the one that was indexed (new, or another modification time or size).
    pub fn differs(&self, entry: &Entry) -> bool {
        self.mtime_ns != entry.mtime_ns || self.size != entry.size
    }
}

/// The most rows of each kind one repository may hold. The graph refuses to load more, so a refresh stops
/// writing at them instead of growing the file without bound.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub syms: usize,
    pub refs: usize,
    pub imports: usize,
    pub binds: usize,
}

/// The most bindings the graph loads for one repository (`graph/infer.rs` refuses more).
pub const MAX_BINDS: usize = 1_000_000;

impl Default for Caps {
    fn default() -> Self {
        Self {
            syms: MAX_SYMS,
            refs: MAX_REFS,
            imports: MAX_REFS,
            binds: MAX_BINDS,
        }
    }
}

/// The rows a repository (or one file) holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Held {
    pub syms: usize,
    pub refs: usize,
    pub imports: usize,
    pub binds: usize,
}

/// What the index holds for one repository.
#[derive(Debug)]
pub struct Known {
    pub repo: i64,
    pub generation: i64,
    /// The walk (see `refresh::fingerprint`) at which a refresh went past the [`Caps`]: while the files on
    /// disk are the same, the repository is still too large and nothing is read.
    pub too_large: Option<String>,
    /// The walk at which a worker process broke on the repository (killed past its limits, crashed, or
    /// ran out of time): while the files on disk are the same, later calls fail at once without a worker.
    pub failed: Option<String>,
    /// The generation a worker loaded and ranked within its limits, or -1: only a graph of that
    /// generation is loaded in the server.
    pub validated: i64,
    /// Unix seconds of the last call that used the repository.
    pub used_at: i64,
    pub packages_hash: String,
    pub files: HashMap<String, Stamp>,
}

/// The connection to `code.sqlite3`.
pub struct Store {
    conn: Connection,
    /// How long a refresh waits for another session's write lock before it answers from what that
    /// session has committed.
    patience: Duration,
}

impl Store {
    /// Opens (creating when missing) the file at `path`: on Unix the folder is 0700 and the file 0600.
    pub fn open(path: &Path) -> Result<Self, String> {
        crate::store::sqlite::prepare_file(path)
            .map_err(|error| format!("cannot create the code index: {}", error.kind()))?;

        let conn = Connection::open(path).map_err(db)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(db)?;

        // Switching to WAL and the first schema write can report BUSY without waiting while another
        // process initialises the same new file, so they are retried until the busy timeout.
        let deadline = Instant::now() + BUSY_TIMEOUT;

        loop {
            match initialise(&conn) {
                Err(error) if is_busy(&error) && Instant::now() < deadline => thread::sleep(BUSY_RETRY),
                Ok(true) => break,
                Ok(false) => return Err(NEWER.to_string()),
                Err(error) => return Err(db(error)),
            }
        }

        Ok(Self {
            conn,
            patience: PATIENCE,
        })
    }

    /// How long a refresh waits for the write lock of another session (see [`Store::write_for`]) before it
    /// serves the last committed state instead.
    pub fn patience(&self) -> Duration {
        self.patience
    }

    /// Changes [`Store::patience`].
    pub fn set_patience(&mut self, patience: Duration) {
        self.patience = patience;
    }

    /// The connection, for reads.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// What the index holds for the repository at `root`; `None` when it has no row yet. The generation and
    /// the files come from one snapshot, so a writer that commits meanwhile does not mix its files with the
    /// old generation.
    pub fn known(&self, root: &str) -> Result<Option<Known>, String> {
        self.snapshot(|conn| known_in(conn, root))
    }

    /// The repository's id, current generation and validated generation, when it has a row.
    pub fn state(&self, root: &str) -> Result<Option<(i64, i64, i64)>, String> {
        self.conn
            .query_row(
                "SELECT id, generation, validated_generation FROM repos WHERE root = ?1",
                [root],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(db)
    }

    /// Runs `read` on one consistent snapshot (a writer that commits meanwhile does not show).
    pub fn snapshot<T>(&self, read: impl FnOnce(&Connection) -> Result<T, String>) -> Result<T, String> {
        let txn = Txn::begin(&self.conn, false)?;
        let value = read(&self.conn)?;

        txn.finish()?;

        Ok(value)
    }

    /// Starts a write transaction, waiting for the write lock for a long time.
    pub fn write(&self) -> Result<Txn<'_>, String> {
        self.write_for(BUSY_TIMEOUT)?
            .ok_or_else(|| "code index database: database is locked".to_string())
    }

    /// Starts a write transaction, waiting at most `wait` for the write lock: `None` when another process
    /// still holds it.
    pub fn write_for(&self, wait: Duration) -> Result<Option<Txn<'_>>, String> {
        let deadline = Instant::now() + wait;

        loop {
            if let Some(txn) = self.try_write()? {
                return Ok(Some(txn));
            }

            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }

    /// One attempt to start a write transaction, which waits a short while for the lock: `None` when
    /// another process holds it.
    pub fn try_write(&self) -> Result<Option<Txn<'_>>, String> {
        self.conn.busy_timeout(WRITE_SLICE).map_err(db)?;

        let started = Txn::start(&self.conn, true);

        self.conn.busy_timeout(BUSY_TIMEOUT).map_err(db)?;

        match started {
            Ok(txn) => Ok(Some(txn)),
            Err(error) if is_busy(&error) => Ok(None),
            Err(error) => Err(db(error)),
        }
    }

    /// Records that the repository `repo` was used now, when the last record is more than a day old. Best
    /// effort: a busy file is left for the next call.
    pub fn touch(&self, repo: i64, used_at: i64) {
        if now() - used_at < USED_EVERY_SECONDS {
            return;
        }

        if let Ok(Some(txn)) = self.try_write() {
            let _ = txn
                .conn()
                .execute("UPDATE repos SET used_at = ?2 WHERE id = ?1", params![repo, now()]);
            let _ = txn.finish();
        }
    }
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

/// Sets the file up. `false` when it was written by a newer version (nothing was changed).
fn initialise(conn: &Connection) -> rusqlite::Result<bool> {
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;")?;

    // a file of this version has its schema (it is written in one transaction that ends with the version)
    // and its journal mode, and needs no write lock: a worker started while another session builds
    // opens it at once
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    if current == SCHEMA_VERSION {
        return Ok(true);
    }

    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;

    let txn = Txn::start(conn, true)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    if version > SCHEMA_VERSION {
        return Ok(false); // the transaction rolls back untouched
    }

    if version != SCHEMA_VERSION {
        for table in TABLES {
            conn.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
        }
    }

    conn.execute_batch(SCHEMA)?;
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;

    txn.commit()?;

    Ok(true)
}

/// The rows of `root` on any connection or transaction.
fn known_in(conn: &Connection, root: &str) -> Result<Option<Known>, String> {
    let repo = conn
        .query_row(
            "SELECT id, generation, packages_hash, used_at, too_large, failed, validated_generation \
             FROM repos WHERE root = ?1",
            [root],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()
        .map_err(db)?;
    let Some((repo, generation, packages_hash, used_at, too_large, failed, validated)) = repo else {
        return Ok(None);
    };

    let mut statement = conn
        .prepare_cached("SELECT path, id, hash, mtime_ns, size, lang FROM files WHERE repo_id = ?1")
        .map_err(db)?;
    let rows = statement
        .query_map([repo], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Stamp {
                    id: row.get(1)?,
                    hash: row.get(2)?,
                    mtime_ns: row.get(3)?,
                    size: row.get(4)?,
                    lang: row.get(5)?,
                },
            ))
        })
        .map_err(db)?;
    let mut files = HashMap::new();

    for row in rows {
        let (path, stamp) = row.map_err(db)?;
        files.insert(path, stamp);
    }

    Ok(Some(Known {
        repo,
        generation,
        too_large,
        failed,
        validated,
        used_at,
        packages_hash,
        files,
    }))
}

/// An open transaction. Dropping it without [`Txn::finish`] rolls it back.
pub struct Txn<'a> {
    conn: &'a Connection,
    write: bool,
    open: bool,
    /// Since when the write lock has been held without a pause.
    held_since: Instant,
}

impl<'a> Txn<'a> {
    fn start(conn: &'a Connection, write: bool) -> rusqlite::Result<Self> {
        conn.execute_batch(if write { "BEGIN IMMEDIATE" } else { "BEGIN" })?;

        Ok(Self {
            conn,
            write,
            open: true,
            held_since: Instant::now(),
        })
    }

    fn begin(conn: &'a Connection, write: bool) -> Result<Self, String> {
        Self::start(conn, write).map_err(db)
    }

    /// Commits. A COMMIT that fails (a deferred constraint, a full disk) leaves the transaction open in
    /// SQLite, so the transaction stays marked open and the drop rolls it back.
    fn commit(mut self) -> rusqlite::Result<()> {
        self.conn.execute_batch("COMMIT")?;
        self.open = false;

        Ok(())
    }

    /// The connection inside the transaction.
    pub fn conn(&self) -> &Connection {
        self.conn
    }

    /// Commits and ends the transaction.
    pub fn finish(self) -> Result<(), String> {
        self.commit().map_err(db)
    }

    /// Commits and starts the next transaction, so other processes can write in between.
    pub fn renew(&mut self) -> Result<(), String> {
        self.renew_after(Duration::ZERO)
    }

    /// Like [`Txn::renew`], and once the lock was held for [`HOLD_MAX`] lets go of it for a moment first: a process that waits for it
    /// polls, and would never get a turn from a build that takes the lock again at once.
    pub fn renew_yielding(&mut self) -> Result<(), String> {
        if self.held_since.elapsed() < HOLD_MAX {
            return self.renew_after(Duration::ZERO);
        }

        self.renew_after(YIELD)
    }

    fn renew_after(&mut self, pause: Duration) -> Result<(), String> {
        // a failed COMMIT leaves the transaction open and marked so, and the drop rolls it back
        self.conn.execute_batch("COMMIT").map_err(db)?;
        self.open = false;

        if !pause.is_zero() {
            thread::sleep(pause);
            self.held_since = Instant::now();
        }

        self.conn
            .execute_batch(if self.write { "BEGIN IMMEDIATE" } else { "BEGIN" })
            .map_err(db)?;
        self.open = true;

        Ok(())
    }

    /// What the index holds for `root`, as seen inside the transaction.
    pub fn known(&self, root: &str) -> Result<Option<Known>, String> {
        known_in(self.conn, root)
    }

    /// The id of the repository at `root`, created when missing.
    pub fn repo(&self, root: &str) -> Result<i64, String> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO repos(root, used_at) VALUES (?1, ?2)",
                params![root, now()],
            )
            .map_err(db)?;
        self.conn
            .execute("UPDATE repos SET used_at = ?2 WHERE root = ?1", params![root, now()])
            .map_err(db)?;

        self.conn
            .query_row("SELECT id FROM repos WHERE root = ?1", [root], |row| row.get(0))
            .map_err(db)
    }

    /// Drops the repositories (but `keep`) that nothing used for [`UNUSED_SECONDS`], with all their rows.
    pub fn evict_unused(&self, keep: i64) -> Result<usize, String> {
        self.conn
            .execute(
                "DELETE FROM repos WHERE id != ?1 AND used_at < ?2",
                params![keep, now() - UNUSED_SECONDS],
            )
            .map_err(db)
    }

    /// Removes a file and, by cascade, its symbols, references, imports and bindings.
    pub fn drop_file(&self, id: i64) -> Result<(), String> {
        let held = self
            .conn
            .prepare_cached(
                "SELECT repo_id, (SELECT count(*) FROM symbols WHERE file_id = ?1), \
                 (SELECT count(*) FROM refs WHERE file_id = ?1), (SELECT count(*) FROM imports WHERE file_id = ?1), \
                 (SELECT count(*) FROM binds WHERE file_id = ?1) \
                 FROM files WHERE id = ?1",
            )
            .and_then(|mut statement| {
                statement
                    .query_row([id], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                        ))
                    })
                    .optional()
            })
            .map_err(db)?;

        self.conn
            .prepare_cached("DELETE FROM files WHERE id = ?1")
            .and_then(|mut statement| statement.execute([id]))
            .map_err(db)?;

        if let Some((repo, syms, refs, imports, binds)) = held {
            self.conn
                .execute(
                    "UPDATE repos SET syms = syms - ?2, refs = refs - ?3, imports = imports - ?4, binds = binds - ?5 \
                     WHERE id = ?1",
                    params![repo, syms, refs, imports, binds],
                )
                .map_err(db)?;
        }

        Ok(())
    }

    /// The rows the repository holds, as counted by its writes.
    pub fn held(&self, repo: i64) -> Result<Held, String> {
        self.conn
            .query_row(
                "SELECT syms, refs, imports, binds FROM repos WHERE id = ?1",
                [repo],
                |row| {
                    Ok(Held {
                        syms: usize::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                        refs: usize::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                        imports: usize::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                        binds: usize::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                    })
                },
            )
            .map_err(db)
    }

    /// Marks the repository at `root` as too large at the walk `fingerprint`.
    pub fn mark_too_large(&self, root: &str, fingerprint: &str) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE repos SET too_large = ?2 WHERE root = ?1",
                params![root, fingerprint],
            )
            .map(drop)
            .map_err(db)
    }

    /// Marks the repository at `root` as one a worker process broke on, at the walk `fingerprint`.
    pub fn mark_failed(&self, root: &str, fingerprint: &str) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE repos SET failed = ?2 WHERE root = ?1",
                params![root, fingerprint],
            )
            .map(drop)
            .map_err(db)
    }

    /// Takes the too-large and the failed marks off a repository whose index is complete.
    pub fn clear_marks(&self, repo: i64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE repos SET too_large = NULL, failed = NULL \
                 WHERE id = ?1 AND (too_large IS NOT NULL OR failed IS NOT NULL)",
                [repo],
            )
            .map(drop)
            .map_err(db)
    }

    /// Records that `generation` of the repository was loaded and ranked within the worker's limits, with the
    /// `bytes` its graph measured, which also takes a failed mark off it (a worker got through these files).
    /// A repository that moved on meanwhile keeps the record it has.
    pub fn validate(&self, repo: i64, generation: i64, bytes: usize) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE repos SET validated_generation = ?2, validated_bytes = ?3, failed = NULL \
                 WHERE id = ?1 AND generation = ?2",
                params![repo, generation, i64::try_from(bytes).unwrap_or(i64::MAX)],
            )
            .map(drop)
            .map_err(db)
    }

    /// Adds a file row and returns its id.
    pub fn put_file(&self, repo: i64, entry: &Entry, hash: &str, lang: &str) -> Result<i64, String> {
        self.conn
            .prepare_cached(
                "INSERT INTO files(repo_id, path, hash, mtime_ns, size, lang) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .and_then(|mut statement| {
                statement.execute(params![repo, entry.rel, hash, entry.mtime_ns, entry.size, lang])
            })
            .map_err(db)?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Records the new stamp of a file whose content did not change.
    pub fn touch_file(&self, id: i64, entry: &Entry) -> Result<(), String> {
        self.conn
            .prepare_cached("UPDATE files SET mtime_ns = ?2, size = ?3 WHERE id = ?1")
            .and_then(|mut statement| statement.execute(params![id, entry.mtime_ns, entry.size]))
            .map(drop)
            .map_err(db)
    }

    /// Writes what was read from a file. Fails with [`TOO_LARGE`], writing nothing, when the repository
    /// would hold more than `caps` afterwards.
    pub fn put_extract(&self, repo: i64, file: i64, extract: &Extract, caps: Caps) -> Result<(), String> {
        let held = self.held(repo)?;
        let added = Held {
            syms: extract.syms.len(),
            refs: extract.refs.len(),
            imports: extract.imports.len(),
            binds: extract.binds.len(),
        };

        if held.syms.saturating_add(added.syms) > caps.syms
            || held.refs.saturating_add(added.refs) > caps.refs
            || held.imports.saturating_add(added.imports) > caps.imports
            || held.binds.saturating_add(added.binds) > caps.binds
        {
            return Err(TOO_LARGE.into());
        }

        self.conn
            .execute(
                "UPDATE repos SET syms = syms + ?2, refs = refs + ?3, imports = imports + ?4, binds = binds + ?5 \
                 WHERE id = ?1",
                params![
                    repo,
                    added.syms as i64,
                    added.refs as i64,
                    added.imports as i64,
                    added.binds as i64
                ],
            )
            .map_err(db)?;

        let mut ids: Vec<i64> = Vec::with_capacity(extract.syms.len());

        {
            let mut insert = self
                .conn
                .prepare_cached(
                    "INSERT INTO symbols(repo_id, file_id, name, kind, parent, line, end_line, sig, ret, test) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                )
                .map_err(db)?;

            for sym in &extract.syms {
                insert
                    .execute(params![
                        repo,
                        file,
                        sym.name,
                        sym.kind,
                        sym.parent,
                        sym.line as i64,
                        sym.end_line as i64,
                        sym.sig,
                        sym.ret,
                        sym.test
                    ])
                    .map_err(db)?;
                ids.push(self.conn.last_insert_rowid());
            }
        }

        let mut insert = self
            .conn
            .prepare_cached(
                "INSERT INTO refs(repo_id, file_id, from_symbol, name, kind, qual, recv, line, handler) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .map_err(db)?;

        for reference in &extract.refs {
            let from = reference.from.and_then(|index| ids.get(index).copied());

            insert
                .execute(params![
                    repo,
                    file,
                    from,
                    reference.name,
                    reference.kind,
                    reference.qual,
                    reference.recv,
                    reference.line as i64,
                    reference.handler
                ])
                .map_err(db)?;
        }

        let mut insert = self
            .conn
            .prepare_cached("INSERT INTO imports(repo_id, file_id, local, orig, source) VALUES (?1, ?2, ?3, ?4, ?5)")
            .map_err(db)?;

        for import in &extract.imports {
            insert
                .execute(params![repo, file, import.local, import.orig, import.source])
                .map_err(db)?;
        }

        let mut insert = self
            .conn
            .prepare_cached(
                "INSERT INTO binds(repo_id, file_id, scope_symbol, name, type, kind) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(db)?;

        for bind in &extract.binds {
            let scope = bind.scope.and_then(|index| ids.get(index).copied());

            insert
                .execute(params![repo, file, scope, bind.name, bind.ty, bind.kind])
                .map_err(db)?;
        }

        Ok(())
    }

    /// Replaces the packages of a repository; true when their digest differs from the stored one.
    pub fn set_packages(&self, repo: i64, packages: &[Package], digest: &str) -> Result<bool, String> {
        let stored: String = self
            .conn
            .query_row("SELECT packages_hash FROM repos WHERE id = ?1", [repo], |row| {
                row.get(0)
            })
            .map_err(db)?;

        if stored == digest {
            return Ok(false);
        }

        self.conn
            .execute("DELETE FROM packages WHERE repo_id = ?1", [repo])
            .map_err(db)?;

        for package in packages {
            self.conn
                .execute(
                    "INSERT INTO packages(repo_id, kind, name, dir) VALUES (?1, ?2, ?3, ?4)",
                    params![repo, package.kind, package.name, package.dir],
                )
                .map_err(db)?;
        }

        self.conn
            .execute(
                "UPDATE repos SET packages_hash = ?2 WHERE id = ?1",
                params![repo, digest],
            )
            .map_err(db)?;

        Ok(true)
    }

    /// Moves the generation of a repository: the cached graphs of every process are stale.
    pub fn bump(&self, repo: i64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE repos SET generation = generation + 1, indexed_at = ?2 WHERE id = ?1",
                params![repo, now()],
            )
            .map(drop)
            .map_err(db)
    }

    /// The generation of a repository.
    pub fn generation(&self, repo: i64) -> Result<i64, String> {
        self.conn
            .query_row("SELECT generation FROM repos WHERE id = ?1", [repo], |row| row.get(0))
            .map_err(db)
    }
}

impl Drop for Txn<'_> {
    fn drop(&mut self) {
        if self.open {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(store: &Store, table: &str) -> i64 {
        store
            .conn()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0))
            .expect("count")
    }

    #[test]
    fn a_new_file_gets_the_schema_and_the_version() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(&dir.path().join("sub/code.sqlite3")).expect("open");

        let version: i64 = store
            .conn()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        let mode: String = store
            .conn()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("mode");

        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(mode, "wal");
        assert_eq!(count(&store, "repos"), 0);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("data/code.sqlite3");
        Store::open(&path).expect("open");

        let file = std::fs::metadata(&path).expect("file").permissions().mode() & 0o777;
        let folder = std::fs::metadata(path.parent().expect("parent"))
            .expect("folder")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(file, 0o600);
        assert_eq!(folder, 0o700);
    }

    #[test]
    fn an_older_schema_version_is_dropped_and_rebuilt() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("code.sqlite3");
        let store = Store::open(&path).expect("open");

        store
            .write()
            .and_then(|txn| txn.repo("/repo").and_then(|_| txn.finish()))
            .expect("repo");
        store.conn().execute_batch("PRAGMA user_version = 1").expect("version");
        drop(store);

        let store = Store::open(&path).expect("reopen");

        assert_eq!(count(&store, "repos"), 0);
        assert!(store.known("/repo").expect("known").is_none());
    }

    #[test]
    fn a_newer_schema_version_is_refused_and_left_untouched() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("code.sqlite3");
        let store = Store::open(&path).expect("open");

        store
            .write()
            .and_then(|txn| txn.repo("/repo").and_then(|_| txn.finish()))
            .expect("repo");

        let newer = SCHEMA_VERSION + 1;

        store
            .conn()
            .execute_batch(&format!("PRAGMA user_version = {newer}"))
            .expect("version");
        drop(store);

        assert_eq!(Store::open(&path).err().as_deref(), Some(NEWER));

        let raw = Connection::open(&path).expect("raw");
        let version: i64 = raw
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        let repos: i64 = raw
            .query_row("SELECT count(*) FROM repos", [], |row| row.get(0))
            .expect("repos");

        assert_eq!(version, newer);
        assert_eq!(repos, 1);
    }

    /// Writes a row that breaks a foreign key, which SQLite only notices at COMMIT.
    fn break_a_key_at_commit(txn: &Txn) {
        txn.conn()
            .execute_batch(
                "PRAGMA defer_foreign_keys = ON; \
                 INSERT INTO files(repo_id, path, hash, mtime_ns, size, lang) VALUES (999, 'a.rs', '', 0, 0, 'rust');",
            )
            .expect("insert");
    }

    #[test]
    fn a_commit_that_fails_does_not_leave_the_connection_inside_a_transaction() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(&dir.path().join("code.sqlite3")).expect("open");
        let txn = store.write().expect("write");

        break_a_key_at_commit(&txn);

        assert!(txn.finish().is_err());

        // every later write used to fail with "cannot start a transaction within a transaction"
        let txn = store.write().expect("a later write");

        txn.repo("/repo").expect("repo");
        txn.finish().expect("commit");
        assert_eq!(count(&store, "repos"), 1);
        assert_eq!(count(&store, "files"), 0);
    }

    #[test]
    fn a_renewal_that_fails_to_commit_rolls_back_and_frees_the_connection() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(&dir.path().join("code.sqlite3")).expect("open");
        let mut txn = store.write().expect("write");

        break_a_key_at_commit(&txn);

        assert!(txn.renew().is_err());
        drop(txn);

        store.write().expect("a later write").finish().expect("commit");
        assert_eq!(count(&store, "files"), 0);
    }

    #[test]
    fn a_session_that_waits_for_the_lock_gets_a_turn_from_a_build_that_yields() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("code.sqlite3");
        let waiter = Store::open(&path).expect("open");
        let got = std::sync::atomic::AtomicBool::new(false);
        let started = Instant::now();

        thread::scope(|scope| {
            scope.spawn(|| {
                let builder = Store::open(&path).expect("open");
                let mut txn = builder.write().expect("write");

                // a build that renews its transaction over and over (it used to hold the lock again at once)
                while !got.load(std::sync::atomic::Ordering::SeqCst) && started.elapsed() < Duration::from_secs(8) {
                    // a batch of work under the lock, then the renewal
                    thread::sleep(Duration::from_millis(20));
                    txn.renew_yielding().expect("renew");
                }

                txn.finish().expect("finish");
            });

            thread::sleep(Duration::from_millis(100));

            while started.elapsed() < Duration::from_secs(6) {
                if waiter.try_write().expect("attempt").is_some() {
                    got.store(true, std::sync::atomic::Ordering::SeqCst);
                    break;
                }
            }

            // let the builder end even when the waiter never got in
            got.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    #[test]
    fn what_is_known_of_a_repository_is_one_moment_not_two() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("code.sqlite3");
        let reader = Store::open(&path).expect("open");
        let base = 2_000;

        {
            let txn = reader.write().expect("write");
            let repo = txn.repo("/repo").expect("repo");

            for n in 0..base {
                txn.conn()
                    .execute(
                        "INSERT INTO files(repo_id, path, hash, mtime_ns, size, lang) VALUES (?1, ?2, '', 0, 0, 'rust')",
                        params![repo, format!("f{n}.rs")],
                    )
                    .expect("file");
            }

            txn.finish().expect("commit");
        }

        let writing = std::sync::atomic::AtomicBool::new(true);
        let mut torn = 0_usize;

        thread::scope(|scope| {
            scope.spawn(|| {
                let writer = Store::open(&path).expect("open");

                // a commit moves the generation and adds a file, which a reader must see together
                for n in 0..1_500 {
                    let txn = writer.write().expect("write");

                    txn.conn()
                        .execute("UPDATE repos SET generation = generation + 1", [])
                        .expect("generation");
                    txn.conn()
                        .execute(
                            "INSERT INTO files(repo_id, path, hash, mtime_ns, size, lang) VALUES (1, ?1, '', 0, 0, 'rust')",
                            params![format!("new{n}.rs")],
                        )
                        .expect("file");
                    txn.finish().expect("commit");
                }

                writing.store(false, std::sync::atomic::Ordering::SeqCst);
            });

            while writing.load(std::sync::atomic::Ordering::SeqCst) {
                let known = reader.known("/repo").expect("known").expect("repo");

                if known.files.len() != base + usize::try_from(known.generation).expect("generation") {
                    torn += 1;
                }
            }
        });

        // a reader used to take the generation of one moment and the files of the next
        assert_eq!(torn, 0);
    }

    #[test]
    fn a_rolled_back_transaction_leaves_nothing() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(&dir.path().join("code.sqlite3")).expect("open");

        {
            let txn = store.write().expect("write");
            txn.repo("/repo").expect("repo");
        }

        assert_eq!(count(&store, "repos"), 0);
    }

    #[test]
    fn a_generation_is_validated_only_while_it_is_the_current_one() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(&dir.path().join("code.sqlite3")).expect("open");
        let txn = store.write().expect("write");
        let repo = txn.repo("/repo").expect("repo");

        txn.bump(repo).expect("bump");
        txn.validate(repo, 1, 0).expect("validate");
        txn.finish().expect("commit");
        assert_eq!(store.state("/repo").expect("state"), Some((repo, 1, 1)));

        // the repository moved on while a worker was still trying generation 1: nothing to record
        let txn = store.write().expect("write");

        txn.bump(repo).expect("bump");
        txn.validate(repo, 1, 0).expect("late validation");
        txn.finish().expect("commit");
        assert_eq!(store.state("/repo").expect("state"), Some((repo, 2, 1)));
    }

    #[test]
    fn only_the_builds_of_one_process_are_ended_by_its_pid() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(&dir.path().join("code.sqlite3")).expect("open");
        let txn = store.write().expect("write");
        let repo = txn.repo("/repo").expect("repo");

        for pid in [41, 42] {
            txn.conn()
                .execute(
                    "INSERT INTO builds(repo_id, kind, status, started_at, pid) VALUES (?1, 'full', 'running', 0, ?2)",
                    params![repo, pid],
                )
                .expect("row");
        }

        let ended = super::super::builds::fail_pid(txn.conn(), repo, 42, "gone").expect("fail");

        txn.finish().expect("commit");

        assert_eq!(ended.len(), 1);

        let states: Vec<(i64, String)> = store
            .conn()
            .prepare("SELECT pid, status FROM builds ORDER BY pid")
            .expect("prepare")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query")
            .map(|row| row.expect("row"))
            .collect();

        assert_eq!(states, vec![(41, "running".to_owned()), (42, "error".to_owned())]);
    }

    #[test]
    fn the_digest_is_a_stable_32_digit_prefix() {
        assert_eq!(digest_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223");
    }
}
