//! Small, self-contained helpers the server is built on.
//!
//! - [`env`]: the injected process environment ([`Env`]) and the per-machine data folder.
//! - [`paths`]: project folder validation.
//! - [`private`]: the private files Harness Desktop and the server share next to the database.
//! - [`git`]: a hardened, shell-free `git` runner with a timeout and an output cap.
//! - [`redact`]: masks secrets in free text before it is stored.
//! - [`text`]: display safety checks and the search stopword list.
//! - [`lang`]: whether a note is English or clearly another language.
//! - [`runner`]: runs a saved script with a timeout and output caps.
//! - [`time`]: calendar dates and timestamps from Unix time.
//! - [`ids`]: note ids listed in an argument or on standard input, capped.

pub mod env;
pub mod git;
pub mod ids;
pub mod lang;
pub mod paths;
pub mod private;
pub mod redact;
pub mod runner;
pub mod text;
pub mod time;

use std::io;
use std::time::Duration;

pub use env::{Env, OsKind};

/// Failure of a support helper. Messages name what failed (an operation, a path), never file
/// contents or secrets.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The user's home folder could not be determined.
    #[error("the home folder is unknown")]
    HomeUnknown,

    /// A path failed validation.
    #[error("invalid path: {0}")]
    InvalidPath(String),

    /// A filesystem or process failure; `context` names the operation (and path, if any).
    #[error("{context}: {source}")]
    Io {
        /// What was being done when the error happened.
        context: String,
        /// The underlying OS error.
        #[source]
        source: io::Error,
    },

    /// No usable `git` executable was found.
    #[error("git was not found")]
    GitNotFound,

    /// A git command exceeded its time limit and was killed.
    #[error("git did not finish within {} s", .0.as_secs())]
    Timeout(Duration),
}

impl Error {
    /// Wraps an I/O error with what was being done.
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}
