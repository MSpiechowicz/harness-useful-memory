//! Whether the team database could be reached when it was last opened.
//!
//! A [`Reach`] is shared by the [`crate::store::Lazy`] store that opens the team database and the
//! stores that fall back to this computer while it cannot be opened (see
//! [`crate::store::SplitStore`]). A failed open starts a cooldown: until it ends, the open is not
//! tried again and the failure is repeated at once, so a server whose team database is down does
//! not wait for a connection timeout on every call.
//!
//! The cooldown is also kept in `team-unreachable.json` next to the local database ([`Reach::beside`]),
//! because `harness-memory notes list` and the other commands Harness Desktop runs are a new process
//! each: without the file every one of them would wait out the timeout itself. The file names the
//! team database it is about and the (already scrubbed) reason; it is private to the owner, and one
//! that cannot be read or does not match counts as no failure.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::support::private::{read_private, write_atomic};

/// The file name of the shared cooldown, next to the local database.
const FILE: &str = "team-unreachable.json";

/// The most bytes read from the file.
const MAX_BYTES: usize = 16 * 1024;

/// How long a failed open is not tried again.
pub const COOLDOWN: Duration = Duration::from_secs(60);

/// The last failed open of the team database.
struct Failure {
    at: Instant,
    message: String,
}

/// The failure as other processes see it.
#[derive(Serialize, Deserialize)]
struct Recorded {
    team: String,
    /// Seconds since the Unix epoch.
    at: u64,
    message: String,
}

/// Where the cooldown is shared between processes, for the team database `team`.
#[derive(Clone)]
struct Shared {
    folder: PathBuf,
    path: PathBuf,
    team: String,
}

/// Shared state of the team database's reachability; clones share it.
#[derive(Clone)]
pub struct Reach {
    cooldown: Duration,
    last: Arc<Mutex<Option<Failure>>>,
    shared: Option<Shared>,
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
            shared: None,
        }
    }

    /// A reach whose cooldown is also kept next to the local `database`, for the team database
    /// `team` ([`crate::backend::Backend::team_key`]), so other processes skip the wait too.
    pub fn beside(database: &Path, team: &str) -> Self {
        Self::beside_with(database, team, COOLDOWN)
    }

    fn beside_with(database: &Path, team: &str, cooldown: Duration) -> Self {
        let path = database.with_file_name(FILE);
        let folder = path.parent().map(Path::to_path_buf).unwrap_or_default();

        Self {
            shared: Some(Shared {
                folder,
                path,
                team: team.to_owned(),
            }),
            ..Self::with_cooldown(cooldown)
        }
    }

    /// Records a failed open (`message` must be safe to show).
    pub fn failed(&self, message: &str) {
        let failure = Failure {
            at: Instant::now(),
            message: message.to_owned(),
        };

        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(failure);

        if let Some(shared) = &self.shared {
            let recorded = Recorded {
                team: shared.team.clone(),
                at: now(),
                message: message.to_owned(),
            };

            // A file that cannot be written only costs the other processes their own wait.
            if let Ok(bytes) = serde_json::to_vec(&recorded) {
                let _written = write_atomic(
                    &shared.folder,
                    &shared.path,
                    &bytes,
                    "team-unreachable-",
                    "record of the unreachable team database",
                );
            }
        }
    }

    /// Records a successful open.
    pub fn opened(&self) {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = None;

        if let Some(shared) = &self.shared {
            let _removed = std::fs::remove_file(&shared.path);
        }
    }

    /// The message of the last failed open while its cooldown runs; `None` once it has ended (the
    /// next open is tried) or when the last open worked.
    pub fn cooling(&self) -> Option<String> {
        let last = self.last.lock().unwrap_or_else(PoisonError::into_inner);

        if let Some(failure) = last.as_ref() {
            return (failure.at.elapsed() < self.cooldown).then(|| failure.message.clone());
        }

        drop(last);

        self.recorded()
    }

    /// The failure another process recorded for this team database, while its cooldown runs.
    fn recorded(&self) -> Option<String> {
        let shared = self.shared.as_ref()?;
        let bytes = read_private(&shared.path, MAX_BYTES).ok().flatten()?;
        let recorded: Recorded = serde_json::from_slice(&bytes).ok()?;

        // A time in the future (a clock set back) is not trusted: the next open is tried.
        let age = now().checked_sub(recorded.at)?;

        (recorded.team == shared.team && age < self.cooldown.as_secs()).then_some(recorded.message)
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
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

    #[test]
    fn another_process_sees_the_failure_of_its_own_team_database_until_it_ends() {
        let dir = tempfile::tempdir().expect("dir");
        let database = dir.path().join("memory.sqlite3");

        Reach::beside(&database, "team-a").failed("down");

        let other_process = |team: &str| Reach::beside(&database, team).cooling();
        assert_eq!(other_process("team-a").as_deref(), Some("down"));
        assert_eq!(other_process("team-b"), None, "another team database is tried");

        let short = Reach::beside_with(&database, "team-a", Duration::from_secs(0));
        assert_eq!(short.cooling(), None, "an ended cooldown is not repeated");

        Reach::beside(&database, "team-a").opened();
        assert_eq!(
            other_process("team-a"),
            None,
            "an open that worked clears it for everyone"
        );
    }
}
