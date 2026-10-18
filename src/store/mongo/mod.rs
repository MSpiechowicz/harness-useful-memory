//! [`MemoryStore`] over a MongoDB database, so a team can share one memory.
//!
//! # Layout
//!
//! The database is the one in the URL path (`harness_memory` when the URL names none) and holds
//! six collections:
//!
//! - `notes`: one document per note, `_id` the note id (an `i64` from the `note_ids` counter in
//!   `meta`), `project` the repository key (`null` for global notes), the linked entity names
//!   (normalized) in `entities`, and the folded words of the title, body and tags in `terms`.
//!   Indexes: unique `(project, title_norm)`, `(project, updated_at)`, `(project, entities)`,
//!   `(project, terms)`, and the text index on `title`, `body` and `tags` (weights 5 : 1 : 3,
//!   language `none`: no stemming and no stopwords, like FTS5's `unicode61`).
//! - `entities`: unique `(project, name_norm)`; the first spelling is kept in `name`.
//! - `relations`: unique `(project, src, rel, dst)` over normalized entity names, with the
//!   entities' spellings copied in, plus `(project, dst)` and `note_id` indexes.
//! - `scripts` (schema 2): one document per script, `_id` from the `script_ids` counter in `meta`,
//!   `project` as for notes; unique `(project, name)`.
//! - `hooks` (schema 3): one document per team hook, `_id` the hook id, `repo` the repository
//!   key (`null` for global hooks), the file bytes in `body`; index `(repo, created_at, _id)`.
//! - `meta`: the schema version (`_id: "schema"`), the note and script id counters and the
//!   migration lock.
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
//! Words are matched with `$text` (case- and diacritic-insensitive) and ranked by `textScore` with
//! the title, body and tag weights of the SQLite store; the order is the repository first, then
//! relevance, then the most recently updated (then hits, then id), with the same filters and
//! limits. Differences from FTS5:
//!
//! - `textScore` is not BM25: it weighs how often a word occurs in a field, not how rare the word
//!   is across all notes, so a rare word does not outrank a common one by itself.
//! - `$text` matches whole words only. The prefix match of the last word (`hard` finds
//!   `hardening`) is a second query on the indexed `terms`, scored with MongoDB's own formula and
//!   added to `textScore`.
//! - At most 100 candidates of each query are ranked, which only matters when more than 100
//!   notes match.
//! - Snippets are cut around the first matching word of the body by this module, not by
//!   FTS5's `snippet()`, so the excerpt can start a few words earlier or later.
//!
//! # Connection
//!
//! `mongodb://` or `mongodb+srv://`. A server that is not on this machine must use TLS (turned
//! on when the URL leaves it unset; `tls=false` and disabled certificate checks are refused).
//! Loopback hosts count as this machine only with `directConnection=true` or `loadBalanced=true`
//! (otherwise the driver also connects to the replica-set members the server lists), and Unix
//! sockets always do. Error messages carry error kinds, codes and plain code names, never server
//! text, the URL or its credentials.
//!
//! # Reads
//!
//! Every read names the fields it needs and has the server cut them to the limits this server
//! writes, so a document written by hand (up to 16 MB) never reaches the agent whole.

mod connection;
mod graph;
mod hooks;
mod listing;
mod notes;
pub mod schema;
mod scripts;
mod search;

use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mongodb::action::Action;
use mongodb::bson::{Bson, Document, doc};
use mongodb::error::WriteFailure;
use mongodb::error::{Error, ErrorKind, TRANSIENT_TRANSACTION_ERROR, UNKNOWN_TRANSACTION_COMMIT_RESULT};
use mongodb::options::{ClientOptions, ReadConcern, ReturnDocument, WriteConcern};
use mongodb::sync::{Client, ClientSession, Collection, Database};

use super::{Edge, Forget, Hit, Link, MemoryStore, Neighborhood, Note, NoteDraft, NotePatch, Saved, SearchQuery};
use super::{Forgotten, NoteListQuery, NotePage, Scope, StoreError, Target, View};
use crate::limits::{BODY_MAX, ENTITY_MAX, TAG_MAX, TAGS_MAX, TITLE_MAX};
use schema::SchemaState;

/// The database used when the URL path names none.
pub const DEFAULT_DATABASE: &str = "harness_memory";

const NOTES: &str = "notes";
const ENTITIES: &str = "entities";
const RELATIONS: &str = "relations";
const SCRIPTS: &str = "scripts";
const HOOKS: &str = "hooks";
const META: &str = "meta";

/// How long the driver looks for a usable server when the URL does not say.
const SERVER_SELECTION_TIMEOUT: Duration = Duration::from_secs(10);

/// Attempts of one write before it gives up on concurrent changes.
const WRITE_ATTEMPTS: u32 = 20;

/// Attempts to commit a transaction whose outcome is unknown.
const COMMIT_ATTEMPTS: u32 = 3;

/// The MongoDB team backend. See the module docs.
pub struct MongoStore {
    client: Client,
    db: Database,
    transactions: bool,
    newer: bool,
}

impl std::fmt::Debug for MongoStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MongoStore")
            .field("database", &self.db.name())
            .field("transactions", &self.transactions)
            .field("newer", &self.newer)
            .finish_non_exhaustive()
    }
}

impl MongoStore {
    /// Connects to the database of `url` (`mongodb://` or `mongodb+srv://`; the database is the
    /// URL path, [`DEFAULT_DATABASE`] when there is none) and migrates it. A database written by a
    /// newer server connects, but every operation then fails with [`StoreError::NewerSchema`].
    pub fn connect(url: &str) -> Result<Self, StoreError> {
        let checked = connection::check(url)?;
        let mut options = ClientOptions::parse(url)
            .run()
            .map_err(|error| connection::connect_error(&error, url))?;
        connection::enforce_tls(&mut options, &checked)?;

        let name = options
            .default_database
            .clone()
            .unwrap_or_else(|| DEFAULT_DATABASE.to_owned());
        options.app_name.get_or_insert_with(|| "harness-memory".to_owned());
        options.server_selection_timeout.get_or_insert(SERVER_SELECTION_TIMEOUT);

        let client = Client::with_options(options).map_err(|error| connection::connect_error(&error, url))?;
        let db = client.database(&name);

        let hello = client
            .database("admin")
            .run_command(doc! { "hello": 1 })
            .run()
            .map_err(|error| connection::backend(&error))?;
        let transactions = hello.contains_key("setName") || hello.get_str("msg").is_ok_and(|msg| msg == "isdbgrid");

        let state = schema::migrate(&db)?;

        Ok(Self {
            client,
            db,
            transactions,
            newer: state == SchemaState::Newer,
        })
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

    /// A context for reads, outside any transaction.
    fn reader(&self) -> Result<Ctx<'_>, StoreError> {
        if self.newer {
            return Err(StoreError::NewerSchema);
        }

        Ok(Ctx::new(&self.db, None))
    }

    /// Fails once the stored schema is newer than this server, also when it changed after connect.
    fn check_version(&mut self) -> Result<(), StoreError> {
        if self.newer {
            return Err(StoreError::NewerSchema);
        }

        let version = schema::version(&self.db).map_err(|error| connection::backend(&error))?;

        if version > schema::SCHEMA_VERSION {
            self.newer = true;
            return Err(StoreError::NewerSchema);
        }

        Ok(())
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
                op(&mut Ctx::new(&self.db, None))
            };

            match outcome {
                Ok(value) => return Ok(value),
                Err(Fail::Store(error)) => return Err(error),
                Err(Fail::Retry) => {}
                Err(Fail::Db(error)) if retryable(&error) => {}
                Err(Fail::Db(error)) => return Err(connection::backend(&error)),
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

        let value = match op(&mut Ctx::new(&self.db, Some(&mut session))) {
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
        self.write(|ctx| notes::remember(ctx, target, &draft))
    }

    fn update(&mut self, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError> {
        let patch = notes::prepare_patch(patch)?;
        self.write(|ctx| notes::update(ctx, view, id, &patch))
    }

    fn get(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        notes::check_ids(ids)?;
        self.check_version()?;

        // Each hit is one atomic increment, so reads need no transaction.
        notes::get(&mut Ctx::new(&self.db, None), view, ids, true).map_err(Fail::into_store)
    }

    fn peek(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        notes::check_ids(ids)?;
        notes::get(&mut self.reader()?, view, ids, false).map_err(Fail::into_store)
    }

    fn list_notes(&mut self, query: &NoteListQuery) -> Result<NotePage, StoreError> {
        listing::list(&mut self.reader()?, query).map_err(Fail::into_store)
    }

    fn search(&mut self, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
        search::search(&mut self.reader()?, query).map_err(Fail::into_store)
    }

    fn similar(&mut self, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
        search::similar(&mut self.reader()?, view, id, limit).map_err(Fail::into_store)
    }

    fn link(&mut self, target: &Target, link: &Link) -> Result<Edge, StoreError> {
        let link = graph::prepare(link)?;
        self.write(|ctx| graph::link(ctx, target, &link))
    }

    fn neighbors(&mut self, view: &View, entity: &str, depth: u8, limit: usize) -> Result<Neighborhood, StoreError> {
        graph::neighbors(&mut self.reader()?, view, entity, depth, limit).map_err(Fail::into_store)
    }

    fn forget(&mut self, view: &View, target: &Target, what: &Forget) -> Result<usize, StoreError> {
        let what = graph::prepare_forget(what)?;

        self.write(|ctx| match &what {
            Forget::Note(id) => notes::forget(ctx, view, *id),
            Forget::Entity(name) => graph::forget_entity(ctx, target, name),
            Forget::Relation { from, relation, to } => graph::forget_relation(ctx, target, from, relation, to),
        })
    }

    fn forget_notes(&mut self, view: &View) -> Result<Forgotten, StoreError> {
        self.write(|ctx| notes::forget_all(ctx, view))
    }

    fn find_by_title(&mut self, target: &Target, title: &str) -> Result<Option<Note>, StoreError> {
        let title = super::clean::title(title)?;
        notes::find_by_title(&mut self.reader()?, target, &title).map_err(Fail::into_store)
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
    fn into_store(self) -> StoreError {
        match self {
            Self::Store(error) => error,
            Self::Db(error) => connection::backend(&error),
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
pub(super) struct Ctx<'a> {
    db: &'a Database,
    session: Option<&'a mut ClientSession>,
}

impl<'a> Ctx<'a> {
    fn new(db: &'a Database, session: Option<&'a mut ClientSession>) -> Self {
        Self { db, session }
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
        let collection = self.collection(collection);
        let action = collection.find(filter).sort(sort).limit(limit).projection(projection);

        let found = match self.session.as_deref_mut() {
            Some(session) => {
                let mut cursor = action.session(&mut *session).run()?;
                cursor.iter(session).collect::<Result<Vec<_>, _>>()?
            }
            None => action.run()?.collect::<Result<Vec<_>, _>>()?,
        };

        Ok(found)
    }

    /// An aggregation, outside any transaction (only reads use it).
    pub(super) fn aggregate(&mut self, collection: &str, pipeline: Vec<Document>) -> Result<Vec<Document>, Fail> {
        let found = self
            .collection(collection)
            .aggregate(pipeline)
            .run()?
            .collect::<Result<Vec<_>, _>>()?;

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
            .projection(doc! { "value": number("$value") })
            .run()?;

        Ok(counter.as_ref().map_or(1, |counter| int(counter, "value")))
    }
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
    }
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
    Bson::Document(doc! { "$cond": [{ "$isNumber": path }, path, 0_i64] })
}

/// The boolean at `path`; false when it is not a boolean.
fn flag(path: &str) -> Bson {
    Bson::Document(doc! { "$cond": [{ "$eq": [{ "$type": path }, "bool"] }, path, false] })
}
