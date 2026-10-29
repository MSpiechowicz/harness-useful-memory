//! The two reasons to give up on a call into the index: the caller asked for it (`stop`), or it ran past
//! its time budget. A repository is untrusted input, so no refresh or graph load may run for ever.

use std::time::{Duration, Instant};

use super::walk::CANCELLED;

/// The time one call into the index may take (a refresh and the load of its graph). Whatever a refresh
/// committed before it ran out stays, so the next call goes on from there.
pub const BUDGET: Duration = Duration::from_secs(120);

/// The error of a call that ran past its time budget.
pub const TOO_SLOW: &str = "indexing took too long";

/// The cancellation and the time budget of one call.
pub struct Pace<'a> {
    stop: &'a dyn Fn() -> bool,
    deadline: Instant,
}

impl<'a> Pace<'a> {
    /// Starts counting `budget` now.
    pub fn new(stop: &'a dyn Fn() -> bool, budget: Duration) -> Self {
        Self {
            stop,
            deadline: Instant::now() + budget,
        }
    }

    /// `Err` when the caller cancelled or the budget is spent.
    pub fn check(&self) -> Result<(), String> {
        if (self.stop)() {
            return Err(CANCELLED.into());
        }

        if Instant::now() > self.deadline {
            return Err(TOO_SLOW.into());
        }

        Ok(())
    }

    /// The caller's cancellation alone.
    pub fn stop(&self) -> &'a dyn Fn() -> bool {
        self.stop
    }

    /// When the budget is spent.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
}
