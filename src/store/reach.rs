//! Whether the team database could be reached when it was last opened.
//!
//! A [`Reach`] is shared by the [`crate::store::Lazy`] store that opens the team database and the
//! stores that fall back to this computer while it cannot be opened (see
//! [`crate::store::SplitStore`]). A failed open starts a cooldown: until it ends, the open is not
//! tried again and the failure is repeated at once, so a server whose team database is down does
//! not wait for a connection timeout on every call.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long a failed open is not tried again.
pub const COOLDOWN: Duration = Duration::from_secs(60);

/// The last failed open of the team database.
struct Failure {
    at: Instant,
    message: String,
}

/// Shared state of the team database's reachability; clones share it.
#[derive(Clone)]
pub struct Reach {
    cooldown: Duration,
    last: Arc<Mutex<Option<Failure>>>,
}

impl Default for Reach {
    fn default() -> Self {
        Self::with_cooldown(COOLDOWN)
    }
}

impl std::fmt::Debug for Reach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reach")
            .field("cooling", &self.cooling().is_some())
            .finish_non_exhaustive()
    }
}

impl Reach {
    /// A reach whose failed opens are not retried for `cooldown`.
    pub fn with_cooldown(cooldown: Duration) -> Self {
        Self {
            cooldown,
            last: Arc::new(Mutex::new(None)),
        }
    }

    /// Records a failed open (`message` must be safe to show).
    pub fn failed(&self, message: &str) {
        let failure = Failure {
            at: Instant::now(),
            message: message.to_owned(),
        };

        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(failure);
    }

    /// Records a successful open.
    pub fn opened(&self) {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// The message of the last failed open while its cooldown runs; `None` once it has ended (the
    /// next open is tried) or when the last open worked.
    pub fn cooling(&self) -> Option<String> {
        let last = self.last.lock().unwrap_or_else(PoisonError::into_inner);

        last.as_ref()
            .filter(|failure| failure.at.elapsed() < self.cooldown)
            .map(|failure| failure.message.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_is_repeated_until_its_cooldown_ends_or_an_open_works() {
        let reach = Reach::with_cooldown(Duration::from_millis(40));
        assert_eq!(reach.cooling(), None, "nothing failed yet");

        reach.failed("down");
        assert_eq!(reach.cooling().as_deref(), Some("down"));
        assert_eq!(reach.clone().cooling().as_deref(), Some("down"), "clones share it");

        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(reach.cooling(), None, "the next open is tried");

        reach.failed("down again");
        reach.opened();
        assert_eq!(reach.cooling(), None);
    }
}
