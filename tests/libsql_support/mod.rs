//! Runs each store case on both SQLite-dialect backends: a fresh local file, and (when
//! `HARNESS_MEMORY_TEST_LIBSQL_URL` names a libSQL server, e.g. `http://127.0.0.1:58080`) the
//! server's database, emptied before the case. Without the variable the libSQL half is skipped
//! with a message.
//!
//! The server holds one database, so its cases take a lock file first and run one at a time, also
//! across test binaries. `HARNESS_MEMORY_TEST_LIBSQL_TOKEN` is sent when set. The server's
//! database is wiped: point the variable at a throwaway server only.
#![allow(dead_code)]

use std::fs::File;
use std::sync::Arc;
use std::time::Duration;

use harness_memory::store::{LibsqlStore, MemoryStore, SqliteStore, StoreError};
use serde_json::{Value, json};
use tempfile::TempDir;

pub const URL_VAR: &str = "HARNESS_MEMORY_TEST_LIBSQL_URL";
pub const TOKEN_VAR: &str = "HARNESS_MEMORY_TEST_LIBSQL_TOKEN";

/// Everything the schema creates, dropped before each libSQL case (the FTS table takes its shadow
/// tables and the triggers with it).
const RESET: &str = "
PRAGMA foreign_keys = OFF;
DROP TABLE IF EXISTS notes_fts;
DROP TABLE IF EXISTS note_entities;
DROP TABLE IF EXISTS relations;
DROP TABLE IF EXISTS entities;
DROP TABLE IF EXISTS notes;
DROP TABLE IF EXISTS projects;
PRAGMA user_version = 0;
";

/// `store_cases!(name)` runs `fn name(db: &Database)` as `name::sqlite` and `name::libsql`.
#[macro_export]
macro_rules! store_cases {
    ($($name:ident),* $(,)?) => {
        $(
            mod $name {
                #[test]
                fn sqlite() {
                    super::$name(&$crate::libsql_support::Database::sqlite());
                }

                #[test]
                fn libsql() {
                    if let Some(db) = $crate::libsql_support::Database::libsql() {
                        super::$name(&db);
                    }
                }
            }
        )*
    };
}

/// The database one case runs on.
pub enum Database {
    Sqlite(TempDir),
    Libsql(Server),
}

/// The libSQL server under test, held exclusively while this lives.
pub struct Server {
    pub url: String,
    pub token: Option<String>,
    agent: ureq::Agent,
    _lock: File,
}

impl Database {
    pub fn sqlite() -> Self {
        Self::Sqlite(TempDir::new().expect("temp dir"))
    }

    /// The emptied server database, or `None` (with a message) when no server is configured.
    pub fn libsql() -> Option<Self> {
        let Some(url) = std::env::var(URL_VAR).ok().filter(|url| !url.trim().is_empty()) else {
            eprintln!("skipped: set {URL_VAR} (e.g. http://127.0.0.1:58080) to run this case on a libSQL server");
            return None;
        };

        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(std::env::temp_dir().join("harness-memory-libsql-tests.lock"))
            .expect("lock file");
        lock.lock().expect("lock");

        let server = Server {
            url,
            token: std::env::var(TOKEN_VAR).ok(),
            agent: agent(),
            _lock: lock,
        };
        server.batch(RESET);
        Some(Self::Libsql(server))
    }

    pub fn is_libsql(&self) -> bool {
        matches!(self, Self::Libsql(_))
    }

    /// A store on this database (another one each call, like another server process).
    pub fn open(&self) -> Box<dyn MemoryStore> {
        self.try_open().expect("open store")
    }

    pub fn try_open(&self) -> Result<Box<dyn MemoryStore>, StoreError> {
        Ok(match self {
            Self::Sqlite(dir) => Box::new(SqliteStore::open(&dir.path().join("memory.sqlite3"))?),
            Self::Libsql(server) => Box::new(LibsqlStore::connect(&server.url, server.token.as_deref())?),
        })
    }

    /// Runs `sql` (several statements) directly, bypassing the store.
    pub fn batch(&self, sql: &str) {
        match self {
            Self::Sqlite(dir) => {
                let conn = rusqlite::Connection::open(dir.path().join("memory.sqlite3")).expect("raw");
                conn.execute_batch(sql).expect("raw batch");
            }
            Self::Libsql(server) => server.batch(sql),
        }
    }

    /// The integer the query `sql` returns, read directly.
    pub fn integer(&self, sql: &str) -> i64 {
        match self {
            Self::Sqlite(dir) => {
                let conn = rusqlite::Connection::open(dir.path().join("memory.sqlite3")).expect("raw");
                conn.query_row(sql, [], |row| row.get(0)).expect("raw query")
            }
            Self::Libsql(server) => {
                let reply = server.pipeline(json!({ "type": "execute", "stmt": { "sql": sql } }));
                let value = &reply["response"]["result"]["rows"][0][0]["value"];
                value.as_str().expect("an integer").parse().expect("an integer")
            }
        }
    }
}

impl Server {
    fn batch(&self, sql: &str) {
        self.pipeline(json!({ "type": "sequence", "sql": sql }));
    }

    /// Sends one stream request (and a close) and returns its result.
    fn pipeline(&self, request: Value) -> Value {
        let body = json!({ "baton": null, "requests": [request, { "type": "close" }] });
        let mut call = self
            .agent
            .post(format!("{}/v2/pipeline", self.url.trim_end_matches('/')));

        if let Some(token) = &self.token {
            call = call.header("authorization", format!("Bearer {token}"));
        }

        let text = call
            .header("content-type", "application/json")
            .send(body.to_string())
            .expect("libSQL server")
            .body_mut()
            .read_to_string()
            .expect("reply");
        let reply: Value = serde_json::from_str(&text).expect("JSON reply");

        let result = reply["results"][0].clone();
        assert_eq!(result["type"], "ok", "{result}");
        result
    }
}

fn agent() -> ureq::Agent {
    let certs: Vec<ureq::tls::Certificate<'static>> = rustls_native_certs::load_native_certs()
        .certs
        .iter()
        .map(|cert| ureq::tls::Certificate::from_der(cert.as_ref()).to_owned())
        .collect();

    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .root_certs(ureq::tls::RootCerts::new_with_certs(&certs))
        .build();

    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(Duration::from_secs(30)))
        .proxy(None)
        .build()
        .new_agent()
}
