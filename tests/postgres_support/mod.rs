//! A throwaway PostgreSQL schema per test, for the Postgres store suites.
//!
//! `HARNESS_MEMORY_TEST_POSTGRES_URL` names a server the tests may create schemas in (for example
//! `postgres://postgres:test@127.0.0.1:55432/postgres`). Each [`database`] creates a schema with a
//! unique name, points the store's URL at it (`options=-csearch_path=...`) and drops it with
//! everything in it when the [`Database`] goes. Without the variable each test prints that it is
//! skipped and passes.
//!
//! The helper's own connection uses no TLS, so the URL has to reach a server that accepts that
//! (a local test server does).
#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

use harness_memory::store::PostgresStore;
use postgres::{Client, NoTls};

pub const URL_VAR: &str = "HARNESS_MEMORY_TEST_POSTGRES_URL";

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// One test's schema; dropped (with its tables) on drop.
pub struct Database {
    admin_url: String,
    schema: String,
    url: String,
}

/// A fresh schema, or `None` (after saying so) when [`URL_VAR`] is unset.
pub fn database() -> Option<Database> {
    let Some(admin_url) = std::env::var(URL_VAR).ok().filter(|url| !url.trim().is_empty()) else {
        eprintln!("skipped: set {URL_VAR} to run the PostgreSQL store tests");
        return None;
    };

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    let schema = format!(
        "hm_test_{}_{}_{nanos}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );

    let mut admin = Client::connect(&admin_url, NoTls).expect("the test server accepts the admin connection");
    admin
        .batch_execute(&format!("CREATE SCHEMA \"{schema}\""))
        .expect("create the test schema");

    let separator = if admin_url.contains('?') { '&' } else { '?' };
    let url = format!("{admin_url}{separator}options=-csearch_path%3D{schema}");

    Some(Database { admin_url, schema, url })
}

impl Database {
    /// A store on this schema (migrating it on first use).
    pub fn open(&self) -> PostgresStore {
        PostgresStore::connect(&self.url).expect("connect to the test schema")
    }

    /// The store URL of this schema.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The store URL with more query parameters (`key=value&...`).
    pub fn url_with(&self, params: &str) -> String {
        format!("{}&{params}", self.url)
    }

    /// A plain client whose `search_path` is this schema, for checks below the store.
    pub fn admin(&self) -> Client {
        let mut client = Client::connect(&self.admin_url, NoTls).expect("admin connection");
        client
            .batch_execute(&format!("SET search_path = \"{}\"", self.schema))
            .expect("search path");
        client
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        if let Ok(mut admin) = Client::connect(&self.admin_url, NoTls) {
            admin
                .batch_execute(&format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", self.schema))
                .ok();
        }
    }
}
