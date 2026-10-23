//! [`MemoryStore`] on a libSQL server (`sqld` self-hosted, or Turso), so a whole team shares one
//! database.
//!
//! libSQL is SQLite, so this backend runs the very statements, FTS5 index and migrations of
//! [`crate::store::sqlite`] (through [`Sql`] and [`Engine`]); only the transport differs. Requests
//! go over the server's HTTP pipeline API ([`hrana`]): a write holds one server connection from
//! `BEGIN IMMEDIATE` to `COMMIT`, so revisions, same-title conflicts and migrations are decided
//! under the write lock exactly as on a local file, and two clients migrating at once queue. Each
//! write also re-reads the schema version under that lock, so a client connected before a newer
//! server migrated stops writing ([`StoreError::NewerSchema`]).
//!
//! A write whose `COMMIT` got no answer may have been applied: it fails saying so and is never
//! sent again.
//!
//! TLS is required except for a server on the same machine ([`endpoint`]); the token is sent as
//! `Authorization: Bearer` and appears in no message, like the URL. Proxy variables are ignored
//! ([`hrana`]).

mod endpoint;
mod hrana;
mod scripts;
mod team_docs;

use std::thread;
use std::time::{Duration, Instant};

use super::sqlite::engine::{self, Engine};
use super::sqlite::schema::{self, SCHEMA_VERSION, SchemaState};
use super::sqlite::sql::Sql;
use super::{Edge, Forget, Hit, Link, MemoryStore, Neighborhood, Note, NoteDraft, NotePatch, Saved, SearchQuery};
use super::{Forgotten, NoteListQuery, NotePage, StoreError, Target, View};
use hrana::{Client, Outcome, Stream, execute};

/// How long a write keeps asking for a lock the server reports busy.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Pause between those attempts.
const BUSY_RETRY: Duration = Duration::from_millis(50);

/// The team backend on a libSQL server. See the module docs.
pub struct LibsqlStore {
    client: Client,
    newer: bool,
    /// The additive schema extensions are in place (see [`engine::install`]).
    extensions: bool,
}

impl std::fmt::Debug for LibsqlStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibsqlStore")
            .field("newer", &self.newer)
            .field("extensions", &self.extensions)
            .finish_non_exhaustive()
    }
}

impl LibsqlStore {
    /// Connects to the server at `url` (`libsql://`, `https://`, or `http://` for localhost only)
    /// with an optional access token, and migrates its database. A database written by a newer
    /// server connects, but every operation then fails with [`StoreError::NewerSchema`].
    pub fn connect(url: &str, token: Option<&str>) -> Result<Self, StoreError> {
        let client = Client::new(endpoint::parse(url)?, token)?;
        let mut store = Self {
            client,
            newer: false,
            extensions: false,
        };

        store.newer = store.migrate()? == SchemaState::Newer;

        if !store.newer {
            store.extensions = engine::install(&mut store);
            engine::backfill(&mut store);
        }

        Ok(store)
    }

    /// True when the database has the additive schema extensions (English index, `lang`,
    /// settings); false when it is newer or refused them.
    pub fn has_extensions(&self) -> bool {
        self.extensions
    }

    /// True when the database was written by a newer server.
    pub fn is_newer(&self) -> bool {
        self.newer
    }

    /// Brings the database to [`SCHEMA_VERSION`]; a newer database is reported, not changed.
    fn migrate(&mut self) -> Result<SchemaState, StoreError> {
        match schema::version_of(&mut self.client.single())? {
            version if version > SCHEMA_VERSION => return Ok(SchemaState::Newer),
            SCHEMA_VERSION => return Ok(SchemaState::Current),
            _ => {}
        }

        // Re-read under the write lock: another client may have migrated meanwhile.
        self.transact(|db| {
            let version = schema::version_of(db)?;

            if version > SCHEMA_VERSION {
                return Ok(SchemaState::Newer);
            }

            if version < SCHEMA_VERSION {
                schema::upgrade(db, version)?;
            }

            Ok(SchemaState::Current)
        })
    }

    fn check_version(&self) -> Result<(), StoreError> {
        if self.newer {
            return Err(StoreError::NewerSchema);
        }

        Ok(())
    }

    /// Runs `op` between `BEGIN IMMEDIATE` and `COMMIT` on one server connection, rolling back
    /// when it fails. The commit is sent once; see [`hrana::Stream::commit`].
    fn transact<T>(&mut self, op: impl FnOnce(&mut dyn Sql) -> Result<T, StoreError>) -> Result<T, StoreError> {
        let mut stream = begin(&self.client)?;

        match op(&mut stream) {
            Ok(done) => {
                stream.commit()?;
                Ok(done)
            }
            Err(error) => {
                // The error that ended the transaction is the one to report.
                stream.rollback().ok();
                Err(error)
            }
        }
    }
}

impl Engine for LibsqlStore {
    /// Re-reads the schema version under the write lock first: a newer server may have migrated
    /// the shared database since this client connected.
    fn write<T>(&mut self, op: impl FnOnce(&mut dyn Sql) -> Result<T, StoreError>) -> Result<T, StoreError> {
        self.check_version()?;

        let written = self.transact(|db| {
            if schema::version_of(db)? > SCHEMA_VERSION {
                return Err(StoreError::NewerSchema);
            }

            op(db)
        });

        if matches!(written, Err(StoreError::NewerSchema)) {
            self.newer = true;
        }

        written
    }

    fn read<T>(&mut self, op: impl FnOnce(&mut dyn Sql) -> Result<T, StoreError>) -> Result<T, StoreError> {
        self.check_version()?;
        op(&mut self.client.single())
    }

    fn extensions(&self) -> bool {
        self.extensions
    }
}

impl MemoryStore for LibsqlStore {
    fn remember(&mut self, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError> {
        engine::remember(self, target, draft)
    }

    fn update(&mut self, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError> {
        engine::update(self, view, id, patch)
    }

    fn get(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        engine::get(self, view, ids)
    }

    fn peek(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        engine::peek(self, view, ids)
    }

    fn list_notes(&mut self, query: &NoteListQuery) -> Result<NotePage, StoreError> {
        engine::list_notes(self, query)
    }

    fn search(&mut self, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
        engine::search(self, query)
    }

    fn similar(&mut self, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
        engine::similar(self, view, id, limit)
    }

    fn similar_text(&mut self, view: &View, text: (&str, &str), limit: usize) -> Result<Vec<Hit>, StoreError> {
        engine::similar_text(self, view, text, limit)
    }

    fn link(&mut self, target: &Target, link: &Link) -> Result<Edge, StoreError> {
        engine::link(self, target, link)
    }

    fn neighbors(&mut self, view: &View, entity: &str, depth: u8, limit: usize) -> Result<Neighborhood, StoreError> {
        engine::neighbors(self, view, entity, depth, limit)
    }

    fn forget(&mut self, view: &View, target: &Target, what: &Forget) -> Result<usize, StoreError> {
        engine::forget(self, view, target, what)
    }

    fn forget_notes(&mut self, view: &View, exclude: &[i64]) -> Result<Forgotten, StoreError> {
        engine::forget_notes(self, view, exclude)
    }

    fn find_by_title(&mut self, target: &Target, title: &str) -> Result<Option<Note>, StoreError> {
        engine::find_by_title(self, target, title)
    }

    fn english_notes(&mut self) -> Result<bool, StoreError> {
        engine::english_notes(self)
    }

    fn set_english_notes(&mut self, on: bool) -> Result<(), StoreError> {
        engine::set_english_notes(self, on)
    }
}

/// A stream inside `BEGIN IMMEDIATE`, with foreign keys enforced (the cascades of `forget` rely
/// on them; the pragma is a no-op inside a transaction, so it goes first). A server that rejects
/// the pragma gets no write. The server queues a writer while another holds the lock; a busy
/// answer is retried until [`BUSY_TIMEOUT`].
fn begin(client: &Client) -> Result<Stream<'_>, StoreError> {
    let mut stream = client.stream();
    let deadline = Instant::now() + BUSY_TIMEOUT;

    let mut outcomes = stream.send(vec![execute("PRAGMA foreign_keys = ON"), execute("BEGIN IMMEDIATE")])?;
    let mut started = outcomes.remove(1);

    if let Outcome::Error { error } = outcomes.remove(0) {
        stream.close();
        return Err(StoreError::Backend(format!(
            "the libSQL server rejected PRAGMA foreign_keys = ON ({}), which forgetting a note needs to remove \
             its links, so nothing was written",
            error.code().unwrap_or("no error code")
        )));
    }

    loop {
        match started {
            Outcome::Ok { .. } => return Ok(stream),
            Outcome::Error { error } if error.is_busy() && Instant::now() < deadline => {
                thread::sleep(BUSY_RETRY);
                started = stream.send(vec![execute("BEGIN IMMEDIATE")])?.remove(0);
            }
            Outcome::Error { error } => {
                stream.close();
                return Err(error.into_store_error());
            }
        }
    }
}
