//! The live team databases the sync suites run on: PostgreSQL, libSQL, MongoDB and MongoDB in its
//! Amazon DocumentDB mode. Each case gets a database of its own (a schema, a MongoDB database,
//! or the libSQL server's one database, emptied under the other suites' lock) and skips with a
//! message when the variable of its backend is unset.

use std::fmt;

use crate::libsql_support;
use crate::mongo_support::{self, TestDb};
use crate::postgres_support;

/// The backends a sync case runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Postgres,
    Libsql,
    Mongodb,
    Documentdb,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Postgres => "postgres",
            Self::Libsql => "libsql",
            Self::Mongodb => "mongodb",
            Self::Documentdb => "documentdb",
        })
    }
}

/// What keeps the case's database alive.
enum Hold {
    Postgres(postgres_support::Database),
    Libsql(libsql_support::Database),
    Mongo(TestDb),
}

/// A database of its own on a live server.
pub struct Live {
    pub kind: Kind,
    url: String,
    token: Option<String>,
    _hold: Hold,
}

/// A refused MongoDB connection is told after this long, instead of the driver's 30 seconds
/// (the way a user sets it in the URL).
const MONGO_SELECTION: &str = "serverSelectionTimeoutMS=3000&connectTimeoutMS=3000";

impl Live {
    /// A fresh database on the server of `kind`, or `None` (with a message) without its variable.
    pub fn open(kind: Kind) -> Option<Self> {
        match kind {
            Kind::Postgres => {
                let database = postgres_support::database()?;
                Some(Self {
                    kind,
                    url: database.url().to_owned(),
                    token: None,
                    _hold: Hold::Postgres(database),
                })
            }
            Kind::Libsql => {
                let database = libsql_support::Database::libsql()?;
                let libsql_support::Database::Libsql(server) = &database else {
                    unreachable!("a libSQL database")
                };
                let (url, token) = (server.url.clone(), server.token.clone());

                Some(Self {
                    kind,
                    url,
                    token,
                    _hold: Hold::Libsql(database),
                })
            }
            Kind::Mongodb | Kind::Documentdb => {
                let database = mongo_support::TestDb::new()?;
                let separator = if database.url.contains('?') { '&' } else { '?' };

                Some(Self {
                    kind,
                    url: format!("{}{separator}{MONGO_SELECTION}", database.url),
                    token: None,
                    _hold: Hold::Mongo(database),
                })
            }
        }
    }

    /// The `host:port` of the real server.
    pub fn authority(&self) -> String {
        split(&self.url).1.to_owned()
    }

    /// The URL of the database as a user sets it, direct (`through` none) or by way of a proxy at
    /// `host:port`.
    pub fn url(&self, through: Option<&str>) -> String {
        let (before, authority, after) = split(&self.url);
        format!("{before}{}{after}", through.unwrap_or(authority))
    }

    /// The environment variables that point a user at this database through `through`.
    pub fn settings(&self, through: Option<&str>) -> Vec<(&'static str, String)> {
        let mut settings = vec![("HARNESS_MEMORY_DATABASE_URL", self.url(through))];

        if let Some(token) = &self.token {
            settings.push(("HARNESS_MEMORY_DATABASE_TOKEN", token.clone()));
        }

        match self.kind {
            Kind::Mongodb => settings.push(("HARNESS_MEMORY_DATABASE_KIND", "mongodb".to_owned())),
            Kind::Documentdb => settings.push(("HARNESS_MEMORY_DATABASE_KIND", "documentdb".to_owned())),
            Kind::Postgres | Kind::Libsql => {}
        }

        settings
    }
}

/// `url` as the text before its `host:port`, the `host:port`, and what follows it.
fn split(url: &str) -> (&str, &str, &str) {
    let scheme_end = url.find("://").expect("a URL with a scheme") + 3;
    let rest = &url[scheme_end..];
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let host_start = authority.rfind('@').map_or(0, |at| at + 1);

    (
        &url[..scheme_end + host_start],
        &authority[host_start..],
        &rest[authority_end..],
    )
}
