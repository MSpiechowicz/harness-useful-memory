//! How long opening a database may wait to add the schema extensions.
//!
//! A user's own command (the CLI, Harness Desktop) may wait for another process's write lock to
//! add them. The MCP server may not: an agent's first tool call after an upgrade would stall
//! behind it. It opens with [`Patience::Skip`], uses what the database has (notes then show no
//! authors) and the next open tries again.

use std::sync::atomic::{AtomicBool, Ordering};

/// What an open does when adding the schema extensions would have to wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Patience {
    /// Wait for the lock, as every write does.
    Wait,
    /// Give up at once and open with the extensions the database has.
    Skip,
}

static SKIP: AtomicBool = AtomicBool::new(false);

/// Makes every database this process opens skip the schema extensions it would have to wait for.
/// Called once by the MCP server before it opens its stores.
pub fn skip_schema_waits() {
    SKIP.store(true, Ordering::Relaxed);
}

/// The patience of an open in this process: [`Patience::Wait`] unless [`skip_schema_waits`] ran.
pub fn patience() -> Patience {
    if SKIP.load(Ordering::Relaxed) {
        Patience::Skip
    } else {
        Patience::Wait
    }
}
