//! [`MemoryStore`] over a MongoDB database, so a team can share one memory.
//!
//! # Layout
//!
//! The database is the one in the URL path (`harness_memory` when the URL names none) and holds
//! seven collections:
//!
//! - `notes`: one document per note, `_id` the note id (an `i64` from the `note_ids` counter in
//!   `meta`), `project` the repository key (`null` for global notes), the linked entity names
//!   (normalized) in `entities`, and the folded words of the title, body and tags in `terms`.
//!   Indexes: unique `(project, title_norm)`, `(project, updated_at)`, `(project, entities)`,
//!   `(project, terms)`, and the text index on `title`, `body` and `tags` (weights 5 : 1 : 3,
//!   language `none`: no stemming and no stopwords, like FTS5's `unicode61`). A database set up
//!   for Amazon DocumentDB ([`Flavor::DocumentDb`]) has no text index, at the same schema version:
//!   nothing else reads it.
//!   Added without a schema version (`schema::ensure_extensions`, `backfill`): `terms_en`,
//!   the Snowball English stems of `terms` (index `(project, terms_en)`), `lang` (`en`, `other` or
//!   `unknown`) and `en_rev`, the `revision` those two describe. A server that does not know them
//!   inserts a note without them and edits it without refreshing them, which `en_rev` tells apart
//!   from a current note, so the next open fills them in.
//! - `entities`: unique `(project, name_norm)`; the first spelling is kept in `name`.
//! - `relations`: unique `(project, src, rel, dst)` over normalized entity names, with the
//!   entities' spellings copied in, plus `(project, dst)` and `note_id` indexes.
//! - `scripts` (schema 2): one document per script, `_id` from the `script_ids` counter in `meta`,
//!   `project` as for notes; unique `(project, name)`.
//! - `hooks` (schema 3): one document per team hook, `_id` the hook id, `repo` the repository
//!   key (`null` for global hooks), the file bytes in `body`; index `(repo, created_at, _id)`.
//! - `routines` (schema 4): the team routines, laid out like `hooks`; index
//!   `(repo, created_at, _id)`.
//! - `meta`: the schema version (`_id: "schema"`), the note and script id counters, the
//!   migration lock and the team's settings (`_id: "settings"`, `english_notes`).
//!
//! # Writes
//!
//! On a replica set or a sharded cluster every write runs in a transaction (snapshot reads,
//! majority writes), retried on transient errors, so a note, its entities and its relations change
//! together. On a standalone server each document changes atomically and a multi-document write is
//! not atomic as a whole. Either way a note is replaced only at the revision it was read at (a
//! guarded update), a concurrent insert of the same title or entity hits the unique index, and the
//! write is then retried from a fresh read, so concurrent writers never lose an update.
//!
//! Input is cleaned and checked (see [`super::clean`]) before any request is sent. Every write
//! first reads the stored schema version, so a server older than the database refuses to write
//! even when another server migrated after this one connected.
//!
//! # Search ranking compared with SQLite
//!
//! A search for words takes a pool of candidates and ranks it in this crate like every other
//! backend (`store::rank`): stopwords left out, English stems, a note has to match at least half
//! of the query's stems, one order for repository and global notes (a repository boost), a
//! relevance floor. The pool is the notes the `$text` index finds (case- and diacritic-insensitive,
//! whole words), the notes with a stem of a query word in `terms_en` (best by the number of
//! stems they have) and the notes with a word or stem the last query word starts. Differences from
//! FTS5:
//!
//! - The relevance is MongoDB's `textScore` formula computed here over the words of each field,
//!   matched by stem (title, body and tag weights of the SQLite store): it weighs how often a
//!   word occurs in a field, not how rare the word is across all notes, so a rare word does not
//!   outrank a common one by itself. The `$text` index and `terms_en` only choose the pool.
//! - At most as many candidates as the pool size (see `store::rank`) come from each source, which
//!   only matters when more notes match.
//! - Snippets are cut around the first matching word of the body by this module, not by
//!   FTS5's `snippet()`, so the excerpt can start a few words earlier or later.
//! - Notes an older server wrote have no `terms_en` yet. They are found through `$text` (or
//!   `terms`), until the next open fills in the stems.
//!
//! # Search on Amazon DocumentDB
//!
//! [`Flavor::DocumentDb`] has no text index (see [`documentdb`]). The words are folded like the
//! indexed `terms` (lowercase, no diacritics) and matched against them, and `terms_en` finds the
//! other forms of the words. The relevance and the ranking are the ones above. Differences from
//! the MongoDB search:
//!
//! - The candidates are the most recently updated matching notes, not the best scored ones, which
//!   only matters when more notes match.
//! - A field's words are the ones this module splits it into (at every character that is not a
//!   letter or a digit), not the ones MongoDB's tokenizer finds, so the score of a field with
//!   punctuation inside words can differ slightly.
//! - A note is found by the `terms` this server stored with it, so a note changed in the database
//!   behind this server's back is found by the words it was last saved with.
//!
//! The similar-note lookup keeps the order of the first versions: the repository first, then
//! `textScore` (or the same formula computed here on DocumentDB), then the most recently updated.
//!
//! # Connection
//!
//! `mongodb://` or `mongodb+srv://`. A server that is not on this machine must use TLS (turned
//! on when the URL leaves it unset; `tls=false` and disabled certificate checks are refused).
//! Loopback hosts count as this machine only with `directConnection=true` or `loadBalanced=true`
//! (otherwise the driver also connects to the replica-set members the server lists), and Unix
//! sockets always do. `tlsCAFile` and `tlsCertificateKeyFile` must be absolute paths for every
//! server, as a relative one would be read from the project (see [`connection::tls_files`]).
//! Error messages carry error kinds, codes and plain code names, never server text, the URL or
//! its credentials.
//!
//! # Reads
//!
//! Every read names the fields it needs and has the server cut them to the limits this server
//! writes, so a document written by hand (up to 16 MB) never reaches the agent whole.
//!
//! On Amazon DocumentDB those reads run as aggregations, as a `find` projection holding
//! expressions is MongoDB 4.4 and newer (see [`Ctx`]), and every read goes to the primary,
//! whatever read preference the URL sets: transactions only run there, and a replica could miss a
//! note saved a moment earlier.

mod backfill;
mod connection;
pub mod documentdb;
mod embed;
mod graph;
mod listing;
mod notes;
pub mod schema;
mod scripts;
mod search;
mod settings;
mod team_docs;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mongodb::action::Action;
use mongodb::bson::{Bson, Document, doc};
use mongodb::error::WriteFailure;
use mongodb::error::{Error, ErrorKind, TRANSIENT_TRANSACTION_ERROR, UNKNOWN_TRANSACTION_COMMIT_RESULT};
use mongodb::options::{ClientOptions, ReadConcern, ReturnDocument, WriteConcern};
use mongodb::sync::{Client, ClientSession, Collection, Database};

use super::{Edge, Forget, Hit, Link, MemoryStore, Neighborhood, Note, NoteDraft, NotePatch, Saved, SearchQuery};
use super::{Forgotten, NoteListQuery, NotePage, NoteText, Scope, StoreError, Target, VectorTarget, View};
use crate::backend::Place;
use crate::limits::{BODY_MAX, ENTITY_MAX, TAG_MAX, TAGS_MAX, TITLE_MAX};
use crate::store::extensions;
use crate::store::listing::checked_exclude;
use schema::SchemaState;

/// The database used when the URL path names none.
pub const DEFAULT_DATABASE: &str = "harness_memory";

const NOTES: &str = "notes";
const ENTITIES: &str = "entities";
const RELATIONS: &str = "relations";
const SCRIPTS: &str = "scripts";
const META: &str = "meta";

/// How long the driver looks for a usable server when the URL does not say.
const SERVER_SELECTION_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest a usefulness counter write is waited for: the driver has no deadline for one
/// operation, so the write runs on its own thread and is left behind when it is late.
const COUNT_BUDGET: Duration = Duration::from_millis(750);

/// Lets the next counter write start once dropped: the counter thread owns it, so a panic frees it too.
struct CountGate(Arc<AtomicBool>);

impl Drop for CountGate {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Attempts of one write before it gives up on concurrent changes.
const WRITE_ATTEMPTS: u32 = 20;

/// Attempts to commit a transaction whose outcome is unknown.
const COMMIT_ATTEMPTS: u32 = 3;

/// Which server the store talks to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Flavor {
    /// MongoDB itself.
    #[default]
    MongoDb,
    /// Amazon DocumentDB (see [`documentdb`]).
    DocumentDb,
}

/// The MongoDB team backend. See the module docs.
pub struct MongoStore {
    client: Client,
    db: Database,
    transactions: bool,
    newer: bool,
    /// Search uses the text index (MongoDB) or the `terms` (Amazon DocumentDB).
    text_index: bool,
    /// Amazon DocumentDB: reads run as aggregations (see [`Ctx`]), and errors of a server that
    /// cannot be reached carry the VPN hint (see [`driver_error`]).
    documentdb: bool,
    /// The additive schema extensions are in place (see [`schema::ensure_extensions`]).
    extensions: bool,
    /// A counter write left behind by [`COUNT_BUDGET`] is still running: no other one starts.
    counting: Arc<AtomicBool>,
}

impl std::fmt::Debug for MongoStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MongoStore")
            .field("database", &self.db.name())
            .field("transactions", &self.transactions)
            .field("newer", &self.newer)
            .field("text_index", &self.text_index)
            .field("documentdb", &self.documentdb)
            .field("extensions", &self.extensions)
            .finish_non_exhaustive()
    }
}

impl MongoStore {
    /// Connects to the database of `url` (`mongodb://` or `mongodb+srv://`; the database is the
    /// URL path, [`DEFAULT_DATABASE`] when there is none) and migrates it. A database written by a
    /// newer server connects, but every operation then fails with [`StoreError::NewerSchema`].
    pub fn connect(url: &str) -> Result<Self, StoreError> {
        Self::connect_as(url, &Flavor::MongoDb)
    }

    /// [`MongoStore::connect`] to the server `flavor` names: Amazon DocumentDB gets its CA bundle,
    /// no retryable writes and no text index (see [`documentdb`]).
    pub fn connect_as(url: &str, flavor: &Flavor) -> Result<Self, StoreError> {
        let documentdb = *flavor == Flavor::DocumentDb;

        let checked = connection::check(url)?;
        let mut options = ClientOptions::parse(url)
            .run()
            .map_err(|error| connection::connect_error(&error, url))?;
        connection::enforce_tls(&mut options, &checked)?;

        if documentdb {
            documentdb::prepare(&mut options, url);
        }

        // Kept until the client is built: the driver reads its CA file then.
        let bundle = connection::tls_files(&mut options, documentdb)?;

        let name = options
            .default_database
            .clone()
            .unwrap_or_else(|| DEFAULT_DATABASE.to_owned());
        options.app_name.get_or_insert_with(|| "harness-memory".to_owned());
        options.server_selection_timeout.get_or_insert(SERVER_SELECTION_TIMEOUT);

        let client = Client::with_options(options).map_err(|error| connection::connect_error(&error, url))?;
        drop(bundle);
        let db = client.database(&name);

        let transactions = probe(&client, documentdb)?;
        let state = schema::migrate(&db, documentdb)?;

        let mut store = Self {
            client,
            db,
            transactions,
            newer: state == SchemaState::Newer,
            text_index: !documentdb,
            documentdb,
            extensions: false,
            counting: Arc::new(AtomicBool::new(false)),
        };

        if !store.newer {
            store.extensions = match schema::ensure_extensions(&store.db, documentdb) {
                Ok(()) => true,
                Err(error) => {
                    extensions::warn_unavailable(&error);
                    false
                }
            };

            if store.extensions {
                backfill::fill(&store.db, documentdb);
            }
        }

        Ok(store)
    }

    /// True when the database has the additive schema extensions (the `terms_en` index); false
    /// when it is newer or refused them.
    pub fn has_extensions(&self) -> bool {
        self.extensions
    }

    /// True when the database was written by a newer server.
    pub fn is_newer(&self) -> bool {
        self.newer
    }

    /// True when writes run in transactions (a replica set or a sharded cluster).
    pub fn uses_transactions(&self) -> bool {
        self.transactions
    }

    /// The name of the database in use.
    pub fn database_name(&self) -> &str {
        self.db.name()
    }

    /// [`Fail::into_store`] for this store.
    fn failed(&self, fail: Fail) -> StoreError {
        fail.into_store(self.documentdb)
    }

    /// A context for reads, outside any transaction.
    fn reader(&self) -> Result<Ctx<'_>, StoreError> {
        if self.newer {
            return Err(StoreError::NewerSchema);
        }

        Ok(Ctx::new(&self.db, None, self.documentdb))
    }

    /// Fails once the stored schema is newer than this server, also when it changed after connect.
    fn check_version(&mut self) -> Result<(), StoreError> {
        if self.newer {
            return Err(StoreError::NewerSchema);
        }

        let version =
            schema::version(&self.db, self.documentdb).map_err(|error| driver_error(&error, self.documentdb))?;

        if version > schema::SCHEMA_VERSION {
            self.newer = true;
            return Err(StoreError::NewerSchema);
        }

        Ok(())
    }

    /// Runs the counter write `count` once, for at most [`COUNT_BUDGET`]: it checks the schema
    /// version and writes on a thread of its own, which a late write is left running on (a new
    /// counter is dropped meanwhile), so a stalled database never holds up the caller.
    fn count(
        &mut self,
        count: impl FnOnce(&Database, bool) -> Result<(), Fail> + Send + 'static,
    ) -> Result<(), StoreError> {
        if self.newer {
            return Err(StoreError::NewerSchema);
        }

        if self.counting.swap(true, Ordering::AcqRel) {
            return Err(StoreError::Backend("a counter write is still running".to_owned()));
        }

        let (db, documentdb, running) = (self.db.clone(), self.documentdb, Arc::clone(&self.counting));
        let (sender, receiver) = mpsc::channel();

        let spawned = thread::Builder::new().name("mongo-counter".to_owned()).spawn(move || {
            // Clears the gate when the thread ends, also when the write panics.
            let gate = CountGate(running);

            let written = (|| {
                let version = schema::version(&db, documentdb).map_err(|error| driver_error(&error, documentdb))?;

                if version > schema::SCHEMA_VERSION {
                    return Err(StoreError::NewerSchema);
                }

                count(&db, documentdb).map_err(|fail| fail.into_store(documentdb))
            })();

            drop(gate);
            let _ = sender.send(written);
        });

        if let Err(error) = spawned {
            // The closure, and the gate it owned, is gone: open the gate again.
            self.counting.store(false, Ordering::Release);
            return Err(StoreError::Backend(format!("a counter write could not start: {error}")));
        }

        let written = receiver
            .recv_timeout(COUNT_BUDGET)
            .unwrap_or_else(|_| Err(StoreError::Backend("the counter write timed out".to_owned())));

        if matches!(written, Err(StoreError::NewerSchema)) {
            self.newer = true;
        }

        written
    }

    /// Runs `op` as one write: in a transaction when the deployment has them, retried from the
    /// start on transient errors, lost revision guards and unique-index races.
    fn write<T>(&mut self, mut op: impl FnMut(&mut Ctx<'_>) -> Result<T, Fail>) -> Result<T, StoreError> {
        self.check_version()?;

        for attempt in 0..WRITE_ATTEMPTS {
            if attempt > 0 {
                thread::sleep(backoff(attempt));
            }

            let outcome = if self.transactions {
                self.in_transaction(&mut op)
            } else {
                op(&mut Ctx::new(&self.db, None, self.documentdb))
            };

            match outcome {
                Ok(value) => return Ok(value),
                Err(Fail::Store(error)) => return Err(error),
                Err(Fail::Retry) => {}
                Err(Fail::Db(error)) if retryable(&error) => {}
                Err(Fail::Db(error)) => return Err(driver_error(&error, self.documentdb)),
            }
        }

        Err(StoreError::Backend(
            "too many concurrent changes to the same data; try again".to_owned(),
        ))
    }

    fn in_transaction<T>(&self, op: &mut impl FnMut(&mut Ctx<'_>) -> Result<T, Fail>) -> Result<T, Fail> {
        let mut session = self.client.start_session().run()?;
        session
            .start_transaction()
            .read_concern(ReadConcern::snapshot())
            .write_concern(WriteConcern::majority())
            .run()?;

        let value = match op(&mut Ctx::new(&self.db, Some(&mut session), self.documentdb)) {
            Ok(value) => value,
            Err(fail) => {
                // Dropping the session aborts too; an abort failure changes nothing here.
                let _ = session.abort_transaction().run();
                return Err(fail);
            }
        };

        let mut attempt = 0;
        loop {
            attempt += 1;

            match session.commit_transaction().run() {
                Ok(()) => return Ok(value),
                Err(error) if error.contains_label(UNKNOWN_TRANSACTION_COMMIT_RESULT) && attempt < COMMIT_ATTEMPTS => {}
                Err(error) => return Err(Fail::Db(error)),
            }
        }
    }
}

impl MemoryStore for MongoStore {
    fn remember(&mut self, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError> {
        let draft = notes::prepare(draft)?;
        let extensions = self.extensions;
        self.write(|ctx| notes::remember(ctx, target, &draft, extensions))
    }

    fn update(&mut self, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError> {
        let patch = notes::prepare_patch(patch)?;
        let extensions = self.extensions;
        self.write(|ctx| notes::update(ctx, view, id, &patch, extensions))
    }

    fn get(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        notes::check_ids(ids)?;
        self.check_version()?;

        // Each hit is one atomic increment, so reads need no transaction.
        notes::get(&mut Ctx::new(&self.db, None, self.documentdb), view, ids, true).map_err(|fail| self.failed(fail))
    }

    fn peek(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        notes::check_ids(ids)?;
        notes::get(&mut self.reader()?, view, ids, false).map_err(|fail| self.failed(fail))
    }

    fn list_notes(&mut self, query: &NoteListQuery) -> Result<NotePage, StoreError> {
        listing::list(&mut self.reader()?, query).map_err(|fail| self.failed(fail))
    }

    fn search(&mut self, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
        let (text_index, extensions) = (self.text_index, self.extensions);
        search::search(&mut self.reader()?, query, text_index, extensions).map_err(|fail| self.failed(fail))
    }

    fn vector_targets(&mut self, view: &View) -> Vec<VectorTarget> {
        VectorTarget::of_view(Place::Remote, view, 0).into_iter().collect()
    }

    fn hits_for(&mut self, query: &SearchQuery, ids: &[i64]) -> Result<Vec<Hit>, StoreError> {
        search::by_ids(&mut self.reader()?, query, ids).map_err(|fail| self.failed(fail))
    }

    fn record_shown(&mut self, ids: &[i64]) -> Result<(), StoreError> {
        let ids = ids.to_vec();

        // One atomic increment per note, so this needs no transaction (like the hit of a read).
        self.count(move |db, documentdb| notes::record_shown(&mut Ctx::new(db, None, documentdb), &ids))
    }

    fn record_useful(&mut self, ids: &[i64], at: i64) -> Result<(), StoreError> {
        let ids = ids.to_vec();

        self.count(move |db, documentdb| notes::record_useful(&mut Ctx::new(db, None, documentdb), &ids, at))
    }

    fn similar(&mut self, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
        let text_index = self.text_index;
        search::similar(&mut self.reader()?, view, id, limit, text_index).map_err(|fail| self.failed(fail))
    }

    fn similar_text(&mut self, view: &View, text: (&str, &str), limit: usize) -> Result<Vec<Hit>, StoreError> {
        let text_index = self.text_index;
        search::resembling(&mut self.reader()?, view, text, None, limit, text_index).map_err(|fail| self.failed(fail))
    }

    fn notes_since(&mut self, cursor: i64, limit: usize) -> Result<Vec<NoteText>, StoreError> {
        embed::notes_since(&mut self.reader()?, cursor, limit).map_err(|fail| self.failed(fail))
    }

    fn note_ids(&mut self) -> Result<Vec<i64>, StoreError> {
        embed::note_ids(&mut self.reader()?).map_err(|fail| self.failed(fail))
    }

    fn link(&mut self, target: &Target, link: &Link) -> Result<Edge, StoreError> {
        let link = graph::prepare(link)?;
        self.write(|ctx| graph::link(ctx, target, &link))
    }

    fn neighbors(&mut self, view: &View, entity: &str, depth: u8, limit: usize) -> Result<Neighborhood, StoreError> {
        graph::neighbors(&mut self.reader()?, view, entity, depth, limit).map_err(|fail| self.failed(fail))
    }

    fn forget(&mut self, view: &View, target: &Target, what: &Forget) -> Result<usize, StoreError> {
        let what = graph::prepare_forget(what)?;

        self.write(|ctx| match &what {
            Forget::Note(id) => notes::forget(ctx, view, *id),
            Forget::Entity(name) => graph::forget_entity(ctx, target, name),
            Forget::Relation { from, relation, to } => graph::forget_relation(ctx, target, from, relation, to),
        })
    }

    fn forget_notes(&mut self, view: &View, exclude: &[i64]) -> Result<Forgotten, StoreError> {
        let exclude = checked_exclude(exclude)?.to_vec();
        self.write(|ctx| notes::forget_all(ctx, view, &exclude))
    }

    fn find_by_title(&mut self, target: &Target, title: &str) -> Result<Option<Note>, StoreError> {
        let title = super::clean::title(title)?;
        notes::find_by_title(&mut self.reader()?, target, &title).map_err(|fail| self.failed(fail))
    }

    fn english_notes(&mut self) -> Result<bool, StoreError> {
        self.check_version()?;
        settings::english_notes(&self.db).map_err(|error| driver_error(&error, self.documentdb))
    }

    fn set_english_notes(&mut self, on: bool) -> Result<(), StoreError> {
        self.check_version()?;
        settings::set_english_notes(&self.db, on).map_err(|error| driver_error(&error, self.documentdb))
    }
}

/// Why one attempt of an operation stopped.
pub(super) enum Fail {
    /// A final answer for the agent (not found, invalid, conflict).
    Store(StoreError),
    /// A driver error; retried when transient or a unique-index race.
    Db(Error),
    /// The data changed under the attempt (a revision guard missed); retried from a fresh read.
    Retry,
}

impl Fail {
    /// The error for the agent; a driver error as [`driver_error`] puts it.
    fn into_store(self, documentdb: bool) -> StoreError {
        match self {
            Self::Store(error) => error,
            Self::Db(error) => driver_error(&error, documentdb),
            Self::Retry => StoreError::Backend("the data changed during the request; try again".to_owned()),
        }
    }
}

impl From<StoreError> for Fail {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<Error> for Fail {
    fn from(error: Error) -> Self {
        Self::Db(error)
    }
}

/// A driver error as a store error. On Amazon DocumentDB (`documentdb`) a server that cannot be
/// reached gets the VPN hint, at connect and later (see [`documentdb::backend`]).
pub(super) fn driver_error(error: &Error, documentdb: bool) -> StoreError {
    if documentdb {
        return documentdb::backend(error);
    }

    connection::backend(error)
}

/// Asks the server what it is (the first contact) and returns whether writes can run in
/// transactions: on a replica set (`setName`) or behind `mongos` (`msg: "isdbgrid"`).
///
/// Amazon DocumentDB is asked with `isMaster`, which engine 4.0 may know where it does not know
/// `hello`. Only its replica sets have transactions: a cluster without `setName` (an elastic
/// cluster) runs without them, with one warning on stderr per process (stdout is the MCP stream).
fn probe(client: &Client, documentdb: bool) -> Result<bool, StoreError> {
    let command = if documentdb {
        doc! { "isMaster": 1 }
    } else {
        doc! { "hello": 1 }
    };

    let reply = client
        .database("admin")
        .run_command(command)
        .run()
        .map_err(|error| driver_error(&error, documentdb))?;

    let replica_set = reply.contains_key("setName");

    if !documentdb {
        return Ok(replica_set || reply.get_str("msg").is_ok_and(|msg| msg == "isdbgrid"));
    }

    if !replica_set {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| eprintln!("harness-memory: {}", documentdb::NO_TRANSACTIONS_WARNING));
    }

    Ok(replica_set)
}

/// A transient transaction error, a write conflict, or a duplicate key (a concurrent insert of the
/// same title, entity or relation: the next attempt finds it).
fn retryable(error: &Error) -> bool {
    error.contains_label(TRANSIENT_TRANSACTION_ERROR) || matches!(error_code(error), Some(11000 | 112))
}

/// The server error code of `error`, if any.
pub(super) fn error_code(error: &Error) -> Option<i32> {
    match error.kind.as_ref() {
        ErrorKind::Command(command) => Some(command.code),
        ErrorKind::Write(WriteFailure::WriteError(write)) => Some(write.code),
        ErrorKind::Write(WriteFailure::WriteConcernError(concern)) => Some(concern.code),
        _ => None,
    }
}

/// A growing pause between attempts, spread by the thread so racing writers drift apart.
fn backoff(attempt: u32) -> Duration {
    let spread = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        thread::current().id().hash(&mut hasher);
        attempt.hash(&mut hasher);
        hasher.finish() % 8
    };

    Duration::from_millis(u64::from(attempt.min(6)) * 4 + spread)
}

/// One attempt's access to the database: inside a transaction when `session` is set.
///
/// On Amazon DocumentDB (`documentdb`) the reads that shape fields on the server run as
/// aggregations: a `find` or `findAndModify` projection holding expressions (`$cond`,
/// `$substrCP`, …) is MongoDB 4.4 and newer, while `$project` takes them on every DocumentDB
/// engine, also inside a transaction. The documents read are the same as on MongoDB.
pub(super) struct Ctx<'a> {
    db: &'a Database,
    session: Option<&'a mut ClientSession>,
    documentdb: bool,
}

impl<'a> Ctx<'a> {
    fn new(db: &'a Database, session: Option<&'a mut ClientSession>, documentdb: bool) -> Self {
        Self {
            db,
            session,
            documentdb,
        }
    }

    fn collection(&self, name: &str) -> Collection<Document> {
        self.db.collection(name)
    }

    /// The first document matching `filter`, with only the fields of `projection`.
    pub(super) fn find_one(
        &mut self,
        collection: &str,
        filter: Document,
        projection: Document,
    ) -> Result<Option<Document>, Fail> {
        if self.documentdb {
            let pipeline = vec![
                doc! { "$match": filter },
                doc! { "$limit": 1_i64 },
                doc! { "$project": projection },
            ];

            return Ok(self.aggregate(collection, pipeline)?.into_iter().next());
        }

        let found = self
            .collection(collection)
            .find_one(filter)
            .projection(projection)
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        Ok(found)
    }

    /// The documents matching `filter`, in `sort` order, at most `limit` (0: no limit), with only
    /// the fields of `projection`.
    pub(super) fn find(
        &mut self,
        collection: &str,
        filter: Document,
        sort: Document,
        limit: i64,
        projection: Document,
    ) -> Result<Vec<Document>, Fail> {
        self.find_page(collection, filter, sort, 0, limit, projection)
    }

    /// [`Ctx::find`] after skipping the first `skip` documents.
    pub(super) fn find_page(
        &mut self,
        collection: &str,
        filter: Document,
        sort: Document,
        skip: u64,
        limit: i64,
        projection: Document,
    ) -> Result<Vec<Document>, Fail> {
        if self.documentdb {
            let (pipeline, hidden) = sorted_pipeline(filter, sort, skip, limit, projection);
            let mut found = self.aggregate(collection, pipeline)?;

            for document in &mut found {
                for key in &hidden {
                    document.remove(key);
                }
            }

            return Ok(found);
        }

        let collection = self.collection(collection);
        let action = collection
            .find(filter)
            .sort(sort)
            .skip(skip)
            .limit(limit)
            .projection(projection);

        let found = match self.session.as_deref_mut() {
            Some(session) => {
                let mut cursor = action.session(&mut *session).run()?;
                cursor.iter(session).collect::<Result<Vec<_>, _>>()?
            }
            None => action.run()?.collect::<Result<Vec<_>, _>>()?,
        };

        Ok(found)
    }

    /// An aggregation, inside the transaction when there is one.
    pub(super) fn aggregate(&mut self, collection: &str, pipeline: Vec<Document>) -> Result<Vec<Document>, Fail> {
        let collection = self.collection(collection);
        let action = collection.aggregate(pipeline);

        let found = match self.session.as_deref_mut() {
            Some(session) => {
                let mut cursor = action.session(&mut *session).run()?;
                cursor.iter(session).collect::<Result<Vec<_>, _>>()?
            }
            None => action.run()?.collect::<Result<Vec<_>, _>>()?,
        };

        Ok(found)
    }

    pub(super) fn insert_one(&mut self, collection: &str, document: Document) -> Result<(), Fail> {
        self.collection(collection)
            .insert_one(document)
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        Ok(())
    }

    /// Applies `update` to the first match and returns it as it was before (`after` false) or
    /// after, with only the fields of `projection`.
    pub(super) fn find_one_and_update(
        &mut self,
        collection: &str,
        filter: Document,
        update: Document,
        upsert: bool,
        after: bool,
        projection: Document,
    ) -> Result<Option<Document>, Fail> {
        if self.documentdb {
            return self.update_and_read(collection, filter, update, upsert, after, projection);
        }

        let returned = if after {
            ReturnDocument::After
        } else {
            ReturnDocument::Before
        };

        let found = self
            .collection(collection)
            .find_one_and_update(filter, update)
            .upsert(upsert)
            .return_document(returned)
            .projection(projection)
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        Ok(found)
    }

    /// [`Ctx::find_one_and_update`] on Amazon DocumentDB, in two steps around a `findAndModify`
    /// with a plain projection. The document as it was before is read first and then updated by
    /// its `_id` (the update lands right after the read, as one more change after it). The
    /// document as it is after is updated first and then read by its `_id`, in the same
    /// transaction (every write runs in one on DocumentDB).
    fn update_and_read(
        &mut self,
        collection: &str,
        filter: Document,
        update: Document,
        upsert: bool,
        after: bool,
        projection: Document,
    ) -> Result<Option<Document>, Fail> {
        if !after {
            let mut fields = projection;
            fields.insert(RAW_ID, "$_id");

            let Some(mut before) = self.find_one(collection, filter.clone(), fields)? else {
                if upsert {
                    self.update_one_upsert(collection, filter, update)?;
                }

                return Ok(None);
            };

            let mut same = filter;
            same.insert("_id", before.remove(RAW_ID).unwrap_or(Bson::Null));
            self.update_one(collection, same, update)?;

            return Ok(Some(before));
        }

        let changed = self
            .collection(collection)
            .find_one_and_update(filter, update)
            .upsert(upsert)
            .return_document(ReturnDocument::After)
            .projection(doc! { "_id": 1 })
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        let Some(id) = changed.and_then(|mut changed| changed.remove("_id")) else {
            return Ok(None);
        };

        self.find_one(collection, doc! { "_id": id }, projection)
    }

    /// Updates the first match; returns the number of matched documents.
    pub(super) fn update_one(&mut self, collection: &str, filter: Document, update: Document) -> Result<u64, Fail> {
        let result = self
            .collection(collection)
            .update_one(filter, update)
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        Ok(result.matched_count)
    }

    /// Updates the first match, inserting a document when none matches.
    pub(super) fn update_one_upsert(
        &mut self,
        collection: &str,
        filter: Document,
        update: Document,
    ) -> Result<(), Fail> {
        self.collection(collection)
            .update_one(filter, update)
            .upsert(true)
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        Ok(())
    }

    pub(super) fn update_many(&mut self, collection: &str, filter: Document, update: Document) -> Result<u64, Fail> {
        let result = self
            .collection(collection)
            .update_many(filter, update)
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        Ok(result.matched_count)
    }

    pub(super) fn delete_one(&mut self, collection: &str, filter: Document) -> Result<u64, Fail> {
        let result = self
            .collection(collection)
            .delete_one(filter)
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        Ok(result.deleted_count)
    }

    pub(super) fn delete_many(&mut self, collection: &str, filter: Document) -> Result<u64, Fail> {
        let result = self
            .collection(collection)
            .delete_many(filter)
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        Ok(result.deleted_count)
    }

    /// How many documents match `filter`.
    pub(super) fn count(&mut self, collection: &str, filter: Document) -> Result<u64, Fail> {
        let count = self
            .collection(collection)
            .count_documents(filter)
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        Ok(count)
    }

    /// True when a document matches `filter`.
    pub(super) fn exists(&mut self, collection: &str, filter: Document) -> Result<bool, Fail> {
        let count = self
            .collection(collection)
            .count_documents(filter)
            .limit(1)
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        Ok(count > 0)
    }

    /// A new note id. Taken outside the transaction, so concurrent writers do not conflict on the
    /// counter; an id of an attempt that is retried is skipped, never reused.
    pub(super) fn next_note_id(&mut self) -> Result<i64, Fail> {
        let counter = self
            .collection(META)
            .find_one_and_update(doc! { "_id": "note_ids" }, doc! { "$inc": { "value": 1_i64 } })
            .upsert(true)
            .return_document(ReturnDocument::After)
            .projection(counter_value())
            .run()?;

        Ok(counter.as_ref().map_or(1, |counter| int(counter, "value")))
    }
}

/// The value of an id counter in `meta`. `$inc` only succeeds on a number, so the value needs no
/// cut, and a plain projection works on every server.
pub(super) fn counter_value() -> Document {
    doc! { "value": 1 }
}

/// The field a read in two steps ([`Ctx::update_and_read`]) carries the stored `_id` in.
const RAW_ID: &str = "__hm_raw_id";

/// The fields a sorted read on Amazon DocumentDB carries its sort keys in, when the projection
/// leaves them out (`__hm_sort_0`, …).
const SORT_KEY: &str = "__hm_sort_";

/// The longest string a sort key is cut to: longer than every sorted field this server writes.
const SORT_KEY_MAX: usize = PATH_MAX;

/// The aggregation [`Ctx::find_page`] runs on Amazon DocumentDB, and the fields it adds for the
/// sort, which the caller removes.
///
/// DocumentDB keeps the order of a `$sort` only when it is the last stage, so the documents are
/// sorted, skipped and cut down first, then shaped by `projection`, then sorted again. The second
/// sort uses the fields the projection keeps, and carries the other sort keys along, each bounded
/// by [`sort_key`].
fn sorted_pipeline(
    filter: Document,
    sort: Document,
    skip: u64,
    limit: i64,
    mut projection: Document,
) -> (Vec<Document>, Vec<String>) {
    let mut pipeline = vec![doc! { "$match": filter }];

    if !sort.is_empty() {
        pipeline.push(doc! { "$sort": sort.clone() });
    }

    if skip > 0 {
        pipeline.push(doc! { "$skip": i64::try_from(skip).unwrap_or(i64::MAX) });
    }

    if limit > 0 {
        pipeline.push(doc! { "$limit": limit });
    }

    let mut again = Document::new();
    let mut hidden = Vec::new();

    for (index, (key, way)) in sort.iter().enumerate() {
        if keeps(&projection, key) {
            again.insert(key.clone(), way.clone());
            continue;
        }

        let carried = format!("{SORT_KEY}{index}");
        projection.insert(carried.clone(), sort_key(&format!("${key}")));
        again.insert(carried.clone(), way.clone());
        hidden.push(carried);
    }

    pipeline.push(doc! { "$project": projection });

    if !again.is_empty() {
        pipeline.push(doc! { "$sort": again });
    }

    (pipeline, hidden)
}

/// True when `projection` (inclusions and expressions) keeps the top-level field `key`: named
/// and not excluded, or `_id` when it is not named.
fn keeps(projection: &Document, key: &str) -> bool {
    match projection.get(key) {
        None => key == "_id",
        Some(Bson::Boolean(kept)) => *kept,
        Some(Bson::Int32(value)) => *value != 0,
        Some(Bson::Int64(value)) => *value != 0,
        Some(Bson::Double(value)) => *value != 0.0,
        Some(_) => true,
    }
}

/// The value at `path` as a sort key: a string cut to [`SORT_KEY_MAX`] characters, a number,
/// an id, a date, a boolean or `null` as it is, anything else (an array, a document, binary
/// data) `null`, so a document written by hand cannot make the reply large.
fn sort_key(path: &str) -> Bson {
    let kept = [
        "int",
        "long",
        "double",
        "decimal",
        "objectId",
        "date",
        "timestamp",
        "bool",
        "null",
    ];

    Bson::Document(doc! {
        "$cond": [
            { "$eq": [{ "$type": path }, "string"] },
            cut(path, SORT_KEY_MAX),
            { "$cond": [{ "$in": [{ "$type": path }, kept.to_vec()] }, path, Bson::Null] },
        ]
    })
}

/// The current time in Unix seconds.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The repository key a write to `target` uses (`None` for global).
fn project_of(target: &Target) -> Option<String> {
    match target {
        Target::Repo(key) => Some(key.key.clone()),
        Target::Global => None,
    }
}

/// A scope as a BSON value: the repository key, or `null` for global.
fn scope_value(project: Option<&str>) -> Bson {
    match project {
        Some(key) => Bson::String(key.to_owned()),
        None => Bson::Null,
    }
}

/// The filter of the scopes `view` can read (matches nothing when it can read none).
fn visible(view: &View) -> Document {
    if view.sees_every_repo() {
        // Every repository note has a string `project`; the global ones have `null`.
        return if view.global {
            doc! {}
        } else {
            doc! { "project": { "$ne": Bson::Null } }
        };
    }

    let mut scopes: Vec<Bson> = Vec::new();

    if let Some(repo) = &view.repo {
        scopes.push(Bson::String(repo.key.clone()));
    }

    if view.global {
        scopes.push(Bson::Null);
    }

    doc! { "project": { "$in": scopes } }
}

fn scope_of(project: Option<&str>) -> Scope {
    match project {
        Some(_) => Scope::Repo,
        None => Scope::Global,
    }
}

/// An integer field (stored as `i64`; `i32` and doubles are read too), 0 when missing.
fn int(document: &Document, key: &str) -> i64 {
    match document.get(key) {
        Some(Bson::Int64(value)) => *value,
        Some(Bson::Int32(value)) => i64::from(*value),
        Some(Bson::Double(value)) => *value as i64,
        _ => 0,
    }
}

/// A string field, `None` when missing or `null`.
fn text(document: &Document, key: &str) -> Option<String> {
    document.get_str(key).ok().map(str::to_owned)
}

/// The strings of an array field (empty when missing).
fn strings(document: &Document, key: &str) -> Vec<String> {
    document
        .get_array(key)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

// Reading documents back.
//
// A teammate with write access can store fields of any size and type (up to MongoDB's 16 MB per
// document) behind this server's back. Every read names the fields it needs, cut on the server to
// the length this server would write, so no reply carries more than that.

/// The longest kind name (`optimisation`), with room to spare.
const KIND_MAX: usize = 16;

/// The longest language name (`unknown`), with room to spare.
const LANG_MAX: usize = 16;

/// The longest note source ([`super::clean::source`]).
const SOURCE_MAX: usize = 64;

/// The longest checkout root or project key read back.
const PATH_MAX: usize = 4096;

/// Entities listed for one note (each `note_save` adds at most
/// [`ENTITIES_PER_NOTE`](crate::limits::ENTITIES_PER_NOTE)).
const NOTE_ENTITIES_MAX: usize = 100;

/// The longest normalized entity name read back (lowercasing can lengthen a name).
const ENTITY_NORM_MAX: usize = 4 * ENTITY_MAX;

/// The fields of a note a [`Note`] or a [`Hit`] needs, each bounded.
fn note_fields() -> Document {
    doc! {
        "_id": number("$_id"),
        "project": cut("$project", PATH_MAX),
        "kind": cut("$kind", KIND_MAX),
        "title": cut("$title", TITLE_MAX),
        "body": cut("$body", BODY_MAX),
        "tags": cut_list("$tags", TAGS_MAX, TAG_MAX),
        "entities": cut_list("$entities", NOTE_ENTITIES_MAX, ENTITY_NORM_MAX),
        "source": cut("$source", SOURCE_MAX),
        "writer_root": cut("$writer_root", PATH_MAX),
        "via_override": flag("$via_override"),
        "created_at": number("$created_at"),
        "updated_at": number("$updated_at"),
        "revision": number("$revision"),
        "hits": number("$hits"),
        "lang": fresh_lang(),
        "shown": number("$shown"),
        "useful": number("$useful"),
        "last_useful_at": optional_number("$last_useful_at"),
    }
}

/// True when the English fields of a note describe its current revision. An older server edits a
/// note and raises `revision` without refreshing `lang` and `en_rev`, until the next backfill.
fn lang_is_fresh() -> Bson {
    Bson::Document(doc! { "$eq": [number("$en_rev"), number("$revision")] })
}

/// The stored `lang` of a note, or `null` (unknown) while it is stale.
fn fresh_lang() -> Bson {
    Bson::Document(doc! { "$cond": [lang_is_fresh(), cut("$lang", LANG_MAX), Bson::Null] })
}

/// Only the `_id` of a note (0 when it is not a number).
fn id_only() -> Document {
    doc! { "_id": number("$_id") }
}

/// The string at `path` cut to `max` characters by the server; `null` when it is not a string.
fn cut(path: &str, max: usize) -> Bson {
    let max = i64::try_from(max).unwrap_or(i64::MAX);

    Bson::Document(doc! {
        "$cond": [
            { "$eq": [{ "$type": path }, "string"] },
            { "$substrCP": [path, 0, max] },
            Bson::Null,
        ]
    })
}

/// The first `count` items of the array at `path`, strings cut to `max` characters (other items
/// become `null`); an empty array when it is not an array.
fn cut_list(path: &str, count: usize, max: usize) -> Bson {
    let count = i64::try_from(count).unwrap_or(i64::MAX);

    Bson::Document(doc! {
        "$cond": [
            { "$isArray": path },
            { "$map": { "input": { "$slice": [path, count] }, "as": "item", "in": cut("$$item", max) } },
            Bson::Array(Vec::new()),
        ]
    })
}

/// The number at `path`; 0 when it is not a number.
fn number(path: &str) -> Bson {
    Bson::Document(doc! { "$cond": [is_number(path), path, 0_i64] })
}

/// The number at `path`; `null` when it is not a number.
fn optional_number(path: &str) -> Bson {
    Bson::Document(doc! { "$cond": [is_number(path), path, Bson::Null] })
}

/// True when the value at `path` is a number. `$type` instead of `$isNumber`, which Amazon
/// DocumentDB only has from engine 8.0.1.
fn is_number(path: &str) -> Bson {
    Bson::Document(doc! { "$in": [{ "$type": path }, ["int", "long", "double", "decimal"]] })
}

/// The boolean at `path`; false when it is not a boolean.
fn flag(path: &str) -> Bson {
    Bson::Document(doc! { "$cond": [{ "$eq": [{ "$type": path }, "bool"] }, path, false] })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database of its own on the server `HARNESS_MEMORY_TEST_MONGO_URL` names, dropped on drop.
    struct Scratch(Database);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = self.0.drop().run();
        }
    }

    fn scratch() -> Option<Scratch> {
        let Ok(url) = std::env::var("HARNESS_MEMORY_TEST_MONGO_URL") else {
            eprintln!("skipped: set HARNESS_MEMORY_TEST_MONGO_URL to a MongoDB URL to run the sorted read tests");
            return None;
        };

        let client = Client::with_uri_str(&url).expect("test server client");
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos())
            .unwrap_or(0);

        Some(Scratch(
            client.database(&format!("hmu_sorted_{}_{nanos}", std::process::id())),
        ))
    }

    #[test]
    fn a_counter_write_that_panics_does_not_stop_the_next_one() {
        let Ok(url) = std::env::var("HARNESS_MEMORY_TEST_MONGO_URL") else {
            eprintln!("skipped: set HARNESS_MEMORY_TEST_MONGO_URL to a MongoDB URL to run the counter gate test");
            return;
        };

        let (base, query) = url
            .split_once('?')
            .map_or((url.as_str(), ""), |(base, query)| (base, query));
        let database = format!("hmu_gate_{}", std::process::id());
        let url = format!("{}/{database}?{query}", base.trim_end_matches('/'));
        let mut store = MongoStore::connect(&url).expect("connect");

        let panicked = store.count(|_, _| panic!("a counter write that dies"));
        assert!(panicked.is_err(), "{panicked:?}");

        store.count(|_, _| Ok(())).expect("the next counter is not refused");
        let _ = store.db.drop().run();
    }

    #[test]
    fn the_sorted_pipeline_ends_with_its_sort_and_hides_the_keys_it_carries() {
        let (pipeline, hidden) = sorted_pipeline(
            doc! {},
            doc! { "rank": -1, "meta.at": 1, "_id": 1, "title": 1 },
            1,
            3,
            doc! { "_id": 0, "title": 1 },
        );

        // Amazon DocumentDB keeps the order of a `$sort` only when it is the last stage.
        let last = pipeline.last().expect("stages");
        assert_eq!(
            last.get_document("$sort").expect("the last stage sorts"),
            &doc! { "__hm_sort_0": -1, "__hm_sort_1": 1, "__hm_sort_2": 1, "title": 1 }
        );
        assert_eq!(hidden, ["__hm_sort_0", "__hm_sort_1", "__hm_sort_2"]);
    }

    #[test]
    fn sorted_reads_on_documentdb_page_like_find() {
        let Some(scratch) = scratch() else { return };
        let db = &scratch.0;

        // Two ranks (ties), a nested time running against the ids.
        let notes: Vec<Document> = (1..=6_i64)
            .map(|id| doc! { "_id": id, "title": format!("note {id}"), "rank": id % 2, "meta": { "at": 10 - id } })
            .collect();
        db.collection::<Document>(NOTES)
            .insert_many(notes)
            .run()
            .expect("insert");

        let titles =
            |ids: &[i64]| -> Vec<Document> { ids.iter().map(|id| doc! { "title": format!("note {id}") }).collect() };
        let with_ids = |ids: &[i64]| -> Vec<Document> {
            ids.iter()
                .map(|id| doc! { "_id": id, "title": format!("note {id}") })
                .collect()
        };

        let cases = [
            // Ties in `rank` broken by `_id`, a page from the middle, neither field in the reply.
            (
                doc! { "rank": -1, "_id": 1 },
                1,
                3,
                doc! { "_id": 0, "title": 1 },
                titles(&[3, 5, 2]),
            ),
            // A dotted sort key.
            (doc! { "meta.at": 1 }, 2, 2, doc! { "title": 1 }, with_ids(&[4, 3])),
            // A sort on `_id`, which the projection leaves out.
            (
                doc! { "_id": -1 },
                0,
                4,
                doc! { "_id": 0, "title": 1 },
                titles(&[6, 5, 4, 3]),
            ),
        ];

        for (sort, skip, limit, projection, expected) in cases {
            for documentdb in [true, false] {
                let found = Ctx::new(db, None, documentdb)
                    .find_page(NOTES, doc! {}, sort.clone(), skip, limit, projection.clone())
                    .unwrap_or_else(|_| panic!("{sort:?}: the read failed"));

                assert_eq!(found, expected, "{sort:?} documentdb={documentdb}");
            }
        }
    }
}
