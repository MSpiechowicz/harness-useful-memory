//! The team databases the script cases run on (`split_scripts`, `push_scripts`): a second SQLite
//! file, and PostgreSQL, libSQL and MongoDB when their test servers are set (see the support
//! modules; without them [`Fresh`] gives `None` after saying the case is skipped).
#![allow(dead_code)]

use harness_memory::store::{LibsqlStore, ScriptStore, SqliteStore};
use tempfile::TempDir;

use crate::{libsql_support, mongo_support, postgres_support};

/// The team database of one case.
pub enum Team {
    Sqlite(TempDir),
    Postgres(postgres_support::Database),
    Libsql(libsql_support::Database),
    Mongo(mongo_support::TestDb),
}

impl Team {
    /// Another script store on the team database (another connection).
    pub fn open(&self) -> Box<dyn ScriptStore> {
        match self {
            Self::Sqlite(dir) => Box::new(SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team file")),
            Self::Postgres(db) => Box::new(db.open()),
            Self::Libsql(libsql_support::Database::Libsql(server)) => {
                Box::new(LibsqlStore::connect(&server.url, server.token.as_deref()).expect("libSQL"))
            }
            Self::Libsql(libsql_support::Database::Sqlite(_)) => unreachable!("a libSQL server"),
            Self::Mongo(db) => Box::new(db.connect()),
        }
    }
}

/// A fresh team database, or `None` when its test server is not set.
pub type Fresh = fn() -> Option<Team>;

pub fn sqlite() -> Option<Team> {
    Some(Team::Sqlite(TempDir::new().expect("temp dir")))
}

pub fn postgres() -> Option<Team> {
    postgres_support::database().map(Team::Postgres)
}

pub fn libsql() -> Option<Team> {
    libsql_support::Database::libsql().map(Team::Libsql)
}

pub fn mongo() -> Option<Team> {
    mongo_support::TestDb::new().map(Team::Mongo)
}
