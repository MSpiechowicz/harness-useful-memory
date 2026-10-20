//! [`MemoryStore`] over a shared PostgreSQL database, so a team writes and reads one memory.
//!
//! The semantics are the SQLite backend's: the same scopes, title rule, revisions, conflicts,
//! provenance, cleaning, limits, ranking order and graph walk. Every write runs in a transaction
//! that first takes one advisory lock, so writers queue like they do on the SQLite file and the
//! read-then-write steps (same-title check, entity pruning) cannot race. Sessions set a lock,
//! statement and idle-in-transaction timeout, so a stuck client cannot hold the others for long.
//!
//! Every write transaction also re-reads the schema version under that lock, so a client stops
//! writing as soon as a newer server migrates the database, not only when it connects.
//!
//! A dropped connection is opened again before the next call. A read that failed because the
//! connection closed is tried once more; a write only when it failed before `COMMIT` was sent (the
//! server rolls back a transaction whose connection closes, so nothing was saved). A write whose
//! `COMMIT` failed is never repeated: the server may have committed it, and running it again would
//! apply it twice (a second revision, a doubled hit count).
//!
//! The database is shared, so its rows and errors are not trusted: a NULL or a value of another
//! type is an error instead of a panic, text columns are cut to the limits by the server before
//! they are sent (see [`cut`]), and errors carry the SQLSTATE with a fixed description, never the
//! server's message, stored text, the URL or a password.

mod connect;
mod graph;
mod listing;
mod notes;
pub mod schema;
mod scripts;
mod search;
mod team_docs;

use std::time::{SystemTime, UNIX_EPOCH};

use postgres::config::SslMode;
use postgres::types::FromSql;
use postgres::{Client, GenericClient, NoTls, Row, Transaction};

use super::{Edge, Forget, Hit, Link, MemoryStore, Neighborhood, Note, NoteDraft, NotePatch, ProjectKey};
use super::{Forgotten, NoteListQuery, NotePage, Saved, SearchQuery, StoreError, Target, View, clean};
use crate::store::listing::checked_exclude;
pub use connect::UrlError;
use schema::SchemaState;

/// The advisory lock every write transaction takes ("harness-memory write").
const WRITE_LOCK: i64 = 0x6861_726e_6573_7302;

/// Session limits: how long a statement waits for a lock or runs, and how long a transaction may
/// sit idle before the server ends it.
const SESSION: &str = "SET lock_timeout = '10s'; SET statement_timeout = '30s'; \
                       SET idle_in_transaction_session_timeout = '60s';";

/// The PostgreSQL backend. See the module docs.
pub struct PostgresStore {
    settings: connect::Settings,
    client: Client,
    newer: bool,
}

impl std::fmt::Debug for PostgresStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresStore")
            .field("newer", &self.newer)
            .finish_non_exhaustive()
    }
}

impl PostgresStore {
    /// Connects to `url` (`postgres://` or `postgresql://`, see [`connect`] for the TLS rules)
    /// and migrates the schema. A database written by a newer server connects, but every
    /// operation then fails with [`StoreError::NewerSchema`].
    pub fn connect(url: &str) -> Result<Self, StoreError> {
        let settings = connect::parse(url).map_err(|error| StoreError::Backend(error.to_string()))?;
        let mut client = open(&settings)?;
        let state = schema::migrate(&mut client)?;

        Ok(Self {
            settings,
            client,
            newer: state == SchemaState::Newer,
        })
    }

    /// True when the database was written by a newer server.
    pub fn is_newer(&self) -> bool {
        self.newer
    }

    /// Fails once the database is known to be newer, and opens the connection again when it closed.
    fn ready(&mut self) -> Result<(), StoreError> {
        if self.newer {
            return Err(StoreError::NewerSchema);
        }

        if self.client.is_closed() {
            self.client = open(&self.settings)?;
        }

        Ok(())
    }

    /// Runs the read `operation` on a live client, once more after reconnecting when it failed
    /// because the connection closed meanwhile.
    fn read<T>(&mut self, mut operation: impl FnMut(&mut Client) -> Result<T, StoreError>) -> Result<T, StoreError> {
        self.ready()?;

        match operation(&mut self.client) {
            Err(error) if self.client.is_closed() || ended(&error) => {
                self.client = open(&self.settings)?;
                operation(&mut self.client)
            }
            other => other,
        }
    }

    /// Runs `operation` in a write transaction (see [`transact`]). It runs once more after
    /// reconnecting only when the connection closed before `COMMIT` was sent; see the module docs.
    fn write<T>(
        &mut self,
        mut operation: impl FnMut(&mut Transaction<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.ready()?;

        let result = match transact(&mut self.client, &mut operation) {
            Err(Failure::BeforeCommit(error)) if self.client.is_closed() || ended(&error) => {
                self.client = open(&self.settings)?;
                transact(&mut self.client, &mut operation)
            }
            other => other,
        };

        let error = match result {
            Ok(value) => return Ok(value),
            Err(Failure::BeforeCommit(error)) => error,
            Err(Failure::AtCommit(error)) if self.client.is_closed() => uncertain(error),
            Err(Failure::AtCommit(error)) => error,
        };

        if matches!(error, StoreError::NewerSchema) {
            self.newer = true;
        }

        Err(error)
    }
}

/// Why a write attempt failed: before `COMMIT` was sent (nothing was saved, so it may run again)
/// or at `COMMIT` (the server may have saved it, so it must not).
enum Failure {
    BeforeCommit(StoreError),
    AtCommit(StoreError),
}

/// One write attempt: a transaction that takes [`WRITE_LOCK`] (and [`schema::MIGRATE_LOCK`]
/// shared), fails with
/// [`StoreError::NewerSchema`] when a newer server migrated the database meanwhile, runs
/// `operation` and commits. A failure drops the transaction, which rolls it back.
fn transact<T>(
    client: &mut Client,
    operation: &mut impl FnMut(&mut Transaction<'_>) -> Result<T, StoreError>,
) -> Result<T, Failure> {
    let before = |error: postgres::Error| Failure::BeforeCommit(backend(error));

    // The migration lock, held shared, keeps a newer server's migration (which holds it alone)
    // from running between the version check and the commit.
    let mut tx = client.transaction().map_err(before)?;
    tx.execute(
        "SELECT pg_advisory_xact_lock($1), pg_advisory_xact_lock_shared($2)",
        &[&WRITE_LOCK, &schema::MIGRATE_LOCK],
    )
    .map_err(before)?;

    let version = schema::stored_version(&mut tx).map_err(Failure::BeforeCommit)?;

    if version.is_some_and(|found| found > schema::SCHEMA_VERSION) {
        return Err(Failure::BeforeCommit(StoreError::NewerSchema));
    }

    let result = operation(&mut tx).map_err(Failure::BeforeCommit)?;
    tx.commit().map_err(|error| Failure::AtCommit(backend(error)))?;
    Ok(result)
}

/// The error of a `COMMIT` whose connection closed: the server may or may not have saved it.
fn uncertain(error: StoreError) -> StoreError {
    match error {
        StoreError::Backend(message) => StoreError::Backend(format!(
            "{message}; the connection closed while the write was committed, so it may have been saved: \
             read it back before repeating it"
        )),
        other => other,
    }
}

impl MemoryStore for PostgresStore {
    fn remember(&mut self, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError> {
        let draft = clean::draft(draft)?;
        self.write(|tx| notes::remember(tx, target, &draft))
    }

    fn update(&mut self, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError> {
        let patch = clean::patch(patch)?;
        self.write(|tx| notes::update(tx, view, id, &patch))
    }

    fn get(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        self.write(|tx| notes::get(tx, view, ids, true))
    }

    fn peek(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        self.read(|client| notes::get(client, view, ids, false))
    }

    fn list_notes(&mut self, query: &NoteListQuery) -> Result<NotePage, StoreError> {
        self.read(|client| listing::list(client, query))
    }

    fn search(&mut self, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
        self.read(|client| search::search(client, query))
    }

    fn similar(&mut self, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
        self.read(|client| search::similar(client, view, id, limit))
    }

    fn similar_text(&mut self, view: &View, text: (&str, &str), limit: usize) -> Result<Vec<Hit>, StoreError> {
        self.read(|client| search::resembling(client, view, text, None, limit))
    }

    fn link(&mut self, target: &Target, link: &Link) -> Result<Edge, StoreError> {
        let link = clean::link(link)?;
        self.write(|tx| graph::link(tx, target, &link))
    }

    fn neighbors(&mut self, view: &View, entity: &str, depth: u8, limit: usize) -> Result<Neighborhood, StoreError> {
        self.read(|client| graph::neighbors(client, view, entity, depth, limit))
    }

    fn forget(&mut self, view: &View, target: &Target, what: &Forget) -> Result<usize, StoreError> {
        let what = clean::forget(what)?;
        self.write(|tx| match &what {
            Forget::Note(id) => notes::forget(tx, view, *id),
            Forget::Entity(name) => graph::forget_entity(tx, target, name),
            Forget::Relation { from, relation, to } => graph::forget_relation(tx, target, from, relation, to),
        })
    }

    fn forget_notes(&mut self, view: &View, exclude: &[i64]) -> Result<Forgotten, StoreError> {
        let exclude = checked_exclude(exclude)?.to_vec();
        self.write(|tx| notes::forget_all(tx, view, &exclude))
    }

    fn find_by_title(&mut self, target: &Target, title: &str) -> Result<Option<Note>, StoreError> {
        let title = clean::title(title)?;
        self.read(|client| notes::find_by_title(client, target, &title))
    }
}

/// Opens a session with the TLS policy of `settings` and sets the session limits.
fn open(settings: &connect::Settings) -> Result<Client, StoreError> {
    let mut client = match settings.config.get_ssl_mode() {
        SslMode::Disable => settings.config.connect(NoTls),
        mode => {
            let required = mode == SslMode::Require;
            let tls = connect::tls(settings.root_cert.as_ref(), required).map_err(StoreError::Backend)?;
            settings.config.connect(tls)
        }
    }
    .map_err(backend)?;

    client.batch_execute(SESSION).map_err(backend)?;
    Ok(client)
}

/// A [`StoreError::Backend`] for a client error. A database error becomes its SQLSTATE with a
/// fixed description (see [`described`]): the server's message, detail and hint can quote stored
/// values or text a teammate raised, so none of them is kept. Any other error is the error and its
/// causes, at most 200 characters.
/// Whether `error` says the server ended this session (an administrator or a restart terminated
/// it: SQLSTATE 57P01, 57P02, 57P03). The client may not have noticed the closed socket yet, so
/// this counts as a lost connection like [`Client::is_closed`].
fn ended(error: &StoreError) -> bool {
    matches!(error, StoreError::Backend(message)
        if ["57P01", "57P02", "57P03"].iter().any(|code| message.contains(&format!("SQLSTATE {code}"))))
}

fn backend(error: postgres::Error) -> StoreError {
    let message = match error.as_db_error() {
        Some(db) => described(db.code().code()),
        None => with_causes(&error).chars().take(200).collect(),
    };

    StoreError::Backend(message)
}

/// A fixed description of the SQLSTATE `code`, followed by the code, or `database error <code>`
/// for a code without one. A code that is not five letters or digits is not shown.
fn described(code: &str) -> String {
    let valid = code.len() == 5 && code.bytes().all(|byte| byte.is_ascii_alphanumeric());

    if !valid {
        return "database error (an invalid SQLSTATE)".to_owned();
    }

    let known = match code {
        "23505" => "a row with this key already exists",
        "25P03" => "the server ended a transaction that sat idle too long",
        "3D000" => "the database does not exist",
        "3F000" => "the schema does not exist",
        "40001" | "40P01" => "the server stopped a transaction that conflicted with another; try again",
        "42501" => "the database role lacks a permission the memory needs",
        "42P01" | "42703" | "42883" => {
            "the memory tables do not match this server (a table, column or function is missing)"
        }
        "53300" => "the database server has too many connections",
        "55P03" => "a database lock was not granted in time",
        "57014" => "a database statement ran too long and was cancelled",
        "57P01" | "57P02" | "57P03" => "the database server is shutting down or starting up",
        _ => match &code[..2] {
            "08" => "the connection to the database failed",
            "22" => "the database refused a value",
            "23" => "a write breaks a database constraint",
            "28" => "the database refused the login",
            "53" => "the database server is out of resources",
            _ => return format!("database error {code}"),
        },
    };

    format!("{known} (SQLSTATE {code})")
}

/// Column `index` of `row`. A NULL or a value of another type (a shared database can hold rows
/// changed by hand) is a [`StoreError::Backend`] naming the column position, never the value.
fn column<'a, T: FromSql<'a>>(row: &'a Row, index: usize) -> Result<T, StoreError> {
    row.try_get(index).map_err(|_| {
        StoreError::Backend(format!(
            "a stored row holds a NULL or an unexpected type (result column {index})"
        ))
    })
}

/// `expression` (a text column) cut to `max + 1` characters by the server, so a row written by
/// hand cannot make a reply of any size; the one character over the limit keeps such a value
/// recognizably longer than anything this server writes.
fn cut(expression: &str, max: usize) -> String {
    format!("left({expression}, {})", max + 1)
}

/// The longest kind name (`optimisation`), with room to spare.
const KIND_MAX: usize = 16;

/// The longest note source ([`clean::source`]).
const SOURCE_MAX: usize = 64;

/// The longest checkout root or project key read back.
const PATH_MAX: usize = 4096;

/// `error` followed by its causes (`error connecting to server: Connection refused`).
fn with_causes(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut cause = error.source();

    while let Some(inner) = cause {
        let text = inner.to_string();

        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }

        cause = inner.source();
    }

    message
}

/// The current time in Unix seconds.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The SQL parameters of a view: the repository's project id (`None` when the repository has no
/// rows yet or is not visible) and whether global rows are visible.
fn view_params(client: &mut impl GenericClient, view: &View) -> Result<(Option<i64>, bool), StoreError> {
    let repo = match &view.repo {
        Some(key) if key.is_every() => Some(-1),
        Some(key) => project_id(client, key)?,
        None => None,
    };

    Ok((repo, view.global))
}

/// The project id of `key`, if it has rows.
fn project_id(client: &mut impl GenericClient, key: &ProjectKey) -> Result<Option<i64>, StoreError> {
    let row = client
        .query_opt("SELECT id FROM projects WHERE key = $1", &[&key.key])
        .map_err(backend)?;

    row.map(|row| column(&row, 0)).transpose()
}

/// The project id a write to `target` uses (`None` for global), registering the repository.
fn target_project(tx: &mut Transaction<'_>, target: &Target) -> Result<Option<i64>, StoreError> {
    let Target::Repo(key) = target else {
        return Ok(None);
    };

    let root = key.root.to_string_lossy().into_owned();
    let row = tx
        .query_one(
            "INSERT INTO projects (key, root_path, remote, created_at, last_seen_at) VALUES ($1, $2, $3, $4, $4)
             ON CONFLICT (key) DO UPDATE SET
                 root_path = excluded.root_path, remote = excluded.remote, last_seen_at = excluded.last_seen_at
             RETURNING id",
            &[&key.key, &root, &key.remote, &now()],
        )
        .map_err(backend)?;

    Ok(Some(column(&row, 0)?))
}

/// The project id of an existing scope for reads and deletes (`Some(None)` means global; `None`
/// means the repository has no rows).
fn target_project_existing(
    client: &mut impl GenericClient,
    target: &Target,
) -> Result<Option<Option<i64>>, StoreError> {
    match target {
        Target::Global => Ok(Some(None)),
        Target::Repo(key) => Ok(project_id(client, key)?.map(Some)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_errors_are_described_by_their_code_only() {
        assert_eq!(
            described("55P03"),
            "a database lock was not granted in time (SQLSTATE 55P03)"
        );
        assert_eq!(
            described("08006"),
            "the connection to the database failed (SQLSTATE 08006)"
        );
        assert_eq!(described("28P01"), "the database refused the login (SQLSTATE 28P01)");
        assert_eq!(described("XX000"), "database error XX000");
    }

    #[test]
    fn a_code_that_is_not_a_sqlstate_is_not_shown() {
        assert_eq!(described("leaked secret"), "database error (an invalid SQLSTATE)");
        assert_eq!(described("ab\ncd"), "database error (an invalid SQLSTATE)");
        assert_eq!(described("é1234"), "database error (an invalid SQLSTATE)");
    }
}
