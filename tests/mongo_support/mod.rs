//! MongoDB test databases: each test gets its own database on the server named by
//! `HARNESS_MEMORY_TEST_MONGO_URL`, dropped when the test ends. Without the variable the tests
//! skip with a message.
//!
//! A local server with transactions (`directConnection=true` lets a loopback URL skip TLS):
//!
//! ```text
//! docker run --rm -d --name hm-mongo -p 127.0.0.1:57017:27017 mongo:8 --replSet rs0
//! docker exec hm-mongo mongosh --quiet --eval 'rs.initiate({_id:"rs0",members:[{_id:0,host:"localhost:27017"}]})'
//! HARNESS_MEMORY_TEST_MONGO_URL='mongodb://localhost:57017/?directConnection=true' cargo test --locked mongo_
//! ```
//!
//! With `HARNESS_MEMORY_TEST_MONGO_FLAVOR=documentdb` every store these tests connect runs in its
//! Amazon DocumentDB mode (no text index, search over the `terms`), so the MongoDB suites also
//! check that mode against the same server. [`TestDb::connect_documentdb`] does it for one store.
#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use harness_memory::store::MongoStore;
use harness_memory::store::mongo::Flavor;
use mongodb::bson::{Document, doc};
use mongodb::sync::{Client, Database};

/// The variable naming the test server.
pub const URL_VARIABLE: &str = "HARNESS_MEMORY_TEST_MONGO_URL";

/// The variable running every store in the Amazon DocumentDB mode (`documentdb`).
pub const FLAVOR_VARIABLE: &str = "HARNESS_MEMORY_TEST_MONGO_FLAVOR";

/// The flavor [`FLAVOR_VARIABLE`] picks: MongoDB unless it says `documentdb`.
pub fn flavor() -> Flavor {
    match std::env::var(FLAVOR_VARIABLE) {
        Ok(value) if value.eq_ignore_ascii_case("documentdb") => Flavor::DocumentDb,
        _ => Flavor::MongoDb,
    }
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A database of its own on the test server, dropped on drop.
pub struct TestDb {
    /// The URL of the database (the test server's URL with its name as the path).
    pub url: String,
    pub name: String,
    client: Client,
}

impl TestDb {
    /// A new, empty database, or `None` (with a message) when no test server is configured.
    pub fn new() -> Option<Self> {
        let Ok(base) = std::env::var(URL_VARIABLE) else {
            eprintln!("skipped: set {URL_VARIABLE} to a MongoDB URL to run the MongoDB store tests");
            return None;
        };

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos())
            .unwrap_or(0);
        let name = format!(
            "hmt_{}_{}_{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );

        let client = Client::with_uri_str(&base).expect("test server client");
        let url = with_database(&base, &name);

        Some(Self { url, name, client })
    }

    /// A store connected to this database, in the flavor [`FLAVOR_VARIABLE`] picks.
    pub fn connect(&self) -> MongoStore {
        MongoStore::connect_as(&self.url, &flavor()).expect("connect to the test database")
    }

    /// A store connected to this database in the Amazon DocumentDB mode.
    pub fn connect_documentdb(&self) -> MongoStore {
        MongoStore::connect_as(&self.url, &Flavor::DocumentDb).expect("connect to the test database as DocumentDB")
    }

    /// The names of the indexes on `collection`.
    pub fn index_names(&self, collection: &str) -> Vec<String> {
        let mut names = self
            .raw()
            .collection::<Document>(collection)
            .list_index_names()
            .run()
            .expect("list indexes");
        names.sort();
        names
    }

    /// The test server's client, for sessions behind the store's back.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The database itself, for checks behind the store's back.
    pub fn raw(&self) -> Database {
        self.client.database(&self.name)
    }

    /// The documents of `collection` matching `filter`.
    pub fn count(&self, collection: &str, filter: Document) -> u64 {
        self.raw()
            .collection::<Document>(collection)
            .count_documents(filter)
            .run()
            .expect("count")
    }

    /// The stored schema version.
    pub fn schema_version(&self) -> i64 {
        let schema = self
            .raw()
            .collection::<Document>("meta")
            .find_one(doc! { "_id": "schema" })
            .run()
            .expect("read meta")
            .expect("schema document");

        schema.get_i64("version").expect("version")
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = self.raw().drop().run();
    }
}

/// A fresh database with a store on it (`None` when no test server is configured).
pub fn store() -> Option<(TestDb, MongoStore)> {
    let db = TestDb::new()?;
    let store = db.connect();
    Some((db, store))
}

/// A fresh database with a store on it in the Amazon DocumentDB mode.
pub fn documentdb_store() -> Option<(TestDb, MongoStore)> {
    let db = TestDb::new()?;
    let store = db.connect_documentdb();
    Some((db, store))
}

/// `base` with its path set to `/name` (the options kept).
fn with_database(base: &str, name: &str) -> String {
    let (scheme, rest) = base.split_once("://").expect("a URL with a scheme");
    let (address, query) = match rest.split_once('?') {
        Some((address, query)) => (address, Some(query)),
        None => (rest, None),
    };
    let authority = address.split('/').next().unwrap_or(address);

    let mut url = format!("{scheme}://{authority}/{name}");
    if let Some(query) = query {
        url.push('?');
        url.push_str(query);
    }

    url
}
