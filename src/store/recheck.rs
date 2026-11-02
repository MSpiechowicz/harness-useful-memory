//! How often a store that did not find the author columns looks for them again.
//!
//! Such a store (a lock it would not wait for, a role that may not change the schema) looks again
//! before a write, because columns another process added since must be written: a changed row
//! never keeps the name of its last writer. Over a network the look is a round trip, so a store
//! that finds nothing waits [`AUTHORS_RECHECK`] before the next one. Writes in between name no
//! author (the columns are unknown to them), which for a column that appeared a moment ago leaves
//! a row another process just named as it was, until the next look.

use std::time::{Duration, Instant};

/// How long a store over a network waits before it looks for the author columns again.
pub(crate) const AUTHORS_RECHECK: Duration = Duration::from_secs(30);

/// When a store last looked for the author columns, and how long it leaves between looks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Recheck {
    interval: Duration,
    last: Option<Instant>,
}

impl Recheck {
    /// Looks at most once per `interval`, and at the first write.
    pub(crate) const fn every(interval: Duration) -> Self {
        Self { interval, last: None }
    }

    /// Looks before every write: for a database in a local file, where it costs nothing.
    pub(crate) const fn always() -> Self {
        Self::every(Duration::ZERO)
    }

    /// True when a look is due now, which counts as the look.
    pub(crate) fn due(&mut self) -> bool {
        self.due_at(Instant::now())
    }

    /// [`Self::due`] at the time `now`.
    pub(crate) fn due_at(&mut self, now: Instant) -> bool {
        let due = self
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= self.interval);

        if due {
            self.last = Some(now);
        }

        due
    }
}

#[cfg(test)]
impl Recheck {
    /// Makes the last look `by` older, as if that time had passed.
    pub(crate) fn age(&mut self, by: Duration) {
        self.last = self.last.and_then(|last| last.checked_sub(by));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_write_looks_and_the_next_ones_wait_for_the_interval() {
        let start = Instant::now();
        let mut recheck = Recheck::every(AUTHORS_RECHECK);

        assert!(recheck.due_at(start), "the first write looks");
        assert!(!recheck.due_at(start + Duration::from_secs(1)));
        assert!(!recheck.due_at(start + AUTHORS_RECHECK - Duration::from_millis(1)));

        assert!(recheck.due_at(start + AUTHORS_RECHECK), "a look after the interval");
        assert!(
            !recheck.due_at(start + AUTHORS_RECHECK + Duration::from_secs(1)),
            "the interval counts from the last look, not the first"
        );
    }

    #[test]
    fn a_local_file_is_looked_at_before_every_write() {
        let now = Instant::now();
        let mut recheck = Recheck::always();

        assert!(recheck.due_at(now));
        assert!(recheck.due_at(now));
    }
}
