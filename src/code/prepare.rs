//! `harness-memory code repo` and `code build`: what Harness Desktop asks when it opens a project, so that the
//! first agent call does not wait for the index (and the person sees one build toast, not one per call).
//!
//! [`repo_state`] reads the index and asks `git` for the repository root (through [`ProjectResolver`], as a
//! tool call does): it opens `code.sqlite3` read-only, never builds or refreshes, never creates the index
//! (`code.sqlite3`) or the build status folder, and loads no graph. (Opening a database of the WAL mode
//! read-only may make its `-shm` and `-wal` files.) [`build`] builds only a project that has no index at all
//! (state `none`). A project that is indexed, too large or failed is skipped without starting the worker, so
//! opening a project again costs next to nothing and a project the index gave up on is not retried on every
//! open. An agent's tool call is what notices changed files later, and loads the graph.
//!
//! [`build`] is one build to the person however many calls of the index it takes (see [`Attempt`]), and ends
//! within [`MAX_RUN`].
//!
//! The root is resolved exactly as a tool call does ([`ProjectResolver`], then [`code_root_refusal`]), not
//! like [`super::hint`] finds it: the status file name and `repos.root` are made from the root, so both must
//! agree with what a tool call would write.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension};

use super::hint::open;
use super::status::{self, Attempt, StatusFile};
use super::store::PACKAGE_LANG;
use super::supervise::GRACE;
use super::{FAILURE_WAIT, TOO_SLOW, switch};
use crate::mcp::calls_code::open_index;
use crate::project::{ProjectResolver, canonical_home, code_root_refusal};
use crate::support::Env;
use crate::support::private::data_folder;

/// How long [`build`] keeps asking for a very large repository: a call has a time budget, and what it
/// committed before the budget ended stays, so the next call goes on from there. Each call gets the time that
/// is left of this.
const BUILD_CAP: Duration = Duration::from_secs(10 * 60);

/// The least time left of [`BUILD_CAP`] that another call is worth starting with.
const MIN_ATTEMPT: Duration = Duration::from_secs(5);

/// The longest [`build`] is meant to run, counted from its start: the cap, and what one call may take past its
/// budget, which is the grace of the worker and the wait for the write lock to record that it was killed
/// (`FAILURE_WAIT`). The clock starts before the project is looked up and the index opened, so the cap
/// bounds those too: a call that starts late is cut to what is left of it. What it does not bound are the
/// waits for a lock that are rare and short in practice: `git` in the lookup, the open of the index (up to
/// 60 seconds when another session holds the database) and the reads after a worker ended. They add a
/// little, not a second cap. Harness Desktop kills the command at 720 seconds, whatever happens.
pub const MAX_RUN: Duration = Duration::from_secs(BUILD_CAP.as_secs() + GRACE.as_secs() + FAILURE_WAIT.as_secs());

/// What the code index holds for a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Indexed, and a worker validated a generation of it.
    Indexed,
    /// Nothing usable yet: no index, or a first build that did not finish.
    None,
    /// Past the size caps: a call fails at once until the files change.
    TooLarge,
    /// A worker broke on it.
    Failed,
    /// A folder the code index does not read (not a git repository, a system folder, ...).
    Refused,
    /// The code index is switched off.
    Off,
}

impl State {
    /// The value in the JSON answers.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Indexed => "indexed",
            Self::None => "none",
            Self::TooLarge => "tooLarge",
            Self::Failed => "failed",
            Self::Refused => "refused",
            Self::Off => "off",
        }
    }
}

/// The state of a project, with what its index holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Repo {
    pub state: State,
    /// Source files in the index (manifests are not counted).
    pub files: usize,
    pub symbols: usize,
    /// Unix seconds of the last change to the index.
    pub indexed_at: Option<i64>,
}

impl Repo {
    /// A project the index holds nothing of.
    fn empty(state: State) -> Self {
        Self {
            state,
            files: 0,
            symbols: 0,
            indexed_at: None,
        }
    }
}

/// What [`build`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Built {
    /// Built: the source files the index holds now.
    Done(usize),
    /// Left alone, because of this state.
    Skipped(State),
    /// Failed with this message.
    Error(String),
}

/// The repository root of `project` and the data folder, or why there is none: the switch is off, or the code
/// index does not read the folder.
fn located(env: &Env, project: &str) -> Result<(PathBuf, PathBuf), State> {
    // without a data folder the tools are not offered either (see `switched_on`)
    let Some(data) = data_folder(env) else {
        return Err(State::Off);
    };

    if !switch::enabled(env, &data) {
        return Err(State::Off);
    }

    let root = ProjectResolver::new(env.clone(), None)
        .resolve(Some(project))
        .ok()
        .flatten()
        .map(|key| key.root)
        .ok_or(State::Refused)?;

    if code_root_refusal(&canonical_home(env), &root).is_some() {
        return Err(State::Refused);
    }

    Ok((data, root))
}

/// What the code index holds for the project at `project`. It changes nothing (a read-only open of the database
/// may make its `-shm` and `-wal` files).
pub fn repo_state(env: &Env, project: &str) -> Repo {
    match located(env, project) {
        Ok((data, root)) => stored(&data, &root),
        Err(state) => Repo::empty(state),
    }
}

/// What `code.sqlite3` in `data` holds for the repository at `root`.
fn stored(data: &Path, root: &Path) -> Repo {
    let Some(row) = root.to_str().and_then(|root| {
        let conn = open(&data.join("code.sqlite3"))?;

        read(&conn, root)
    }) else {
        return Repo::empty(State::None);
    };

    // too large and failed come first: the generation they stopped at may well be validated
    let state = if row.too_large {
        State::TooLarge
    } else if row.failed {
        State::Failed
    } else if row.validated >= 0 {
        // a generation that moved since its last check is still served: only a first build that never
        // finished has nothing validated
        State::Indexed
    } else {
        return Repo::empty(State::None);
    };

    Repo {
        state,
        files: row.files,
        symbols: row.symbols,
        indexed_at: (row.indexed_at > 0).then_some(row.indexed_at),
    }
}

/// The row of a repository.
struct Row {
    validated: i64,
    too_large: bool,
    failed: bool,
    symbols: usize,
    indexed_at: i64,
    files: usize,
}

fn read(conn: &Connection, root: &str) -> Option<Row> {
    conn.query_row(
        "SELECT validated_generation, too_large IS NOT NULL, failed IS NOT NULL, syms, indexed_at, \
                (SELECT COUNT(*) FROM files WHERE repo_id = repos.id AND lang <> ?2 AND hash <> '') \
         FROM repos WHERE root = ?1",
        rusqlite::params![root, PACKAGE_LANG],
        |row| {
            Ok(Row {
                validated: row.get(0)?,
                too_large: row.get(1)?,
                failed: row.get(2)?,
                symbols: usize::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                indexed_at: row.get(4)?,
                files: usize::try_from(row.get::<_, i64>(5)?).unwrap_or(0),
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

/// Builds the index of the project at `project` the way a `code_*` call would, when it has none yet. The
/// status file of the build is written (by the worker) as for a call, and is one build however many calls it
/// takes. The graph is not loaded here: the worker validated it, which is what the next call needs.
pub fn build(env: &Env, project: &str) -> Built {
    // the cap counts from here, the lookup of the project and the open of the index included
    let started = Instant::now();
    let (data, root) = match located(env, project) {
        Ok(found) => found,
        Err(state) => return Built::Skipped(state),
    };
    let state = stored(&data, &root).state;

    if state != State::None {
        return Built::Skipped(state);
    }

    let mut index = match open_index(env) {
        Ok(index) => index,
        Err(error) => return Built::Error(error),
    };
    let per_call = index.budget();
    // the `builds` rows that attempts of this run started: only a build of these is this run's to take over or
    // end, never another session's that is `building` meanwhile
    let mut own: Vec<i64> = Vec::new();
    // the build the status file shows, once a call ran out of time and left it `building`
    let mut owner = None;

    let result = loop {
        let Some(plan) = Plan::of(started.elapsed(), per_call) else {
            break Err(TOO_SLOW.to_owned());
        };
        let attempt = Attempt {
            // taken over only while the file still shows that build: another session may have replaced it
            continued: owner.filter(|&id| status::building_id(&data, &root) == Some(id)),
            more_follows: plan.more_follows,
        };
        let mut status = StatusFile::new(&data, &root).attempt(attempt);

        index.set_budget(plan.budget);

        match index.prepare(&root, &|| false, &mut status, attempt, &mut own) {
            Err(error) if error == TOO_SLOW => {
                own.extend(status.began());

                owner = status::building_id(&data, &root)
                    .filter(|id| own.contains(id))
                    .or(owner);
            }
            result => break result,
        }
    };

    // a call that ran out of time left the file `building`, and the last call may have had no build of its own
    // to end it: the build ends here, as it really did
    if let Some(owner) = owner {
        match &result {
            Ok(_) => status::finish_joined(&data, &status::path_of(&data, &root), owner),
            Err(error) => status::fail_builds(&data, &root, &[owner], error),
        }
    }

    match result {
        Ok(_) => Built::Done(stored(&data, &root).files),
        Err(error) => Built::Error(error),
    }
}

/// What the next call of the index may do.
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    /// The time it may take: the time left of [`BUILD_CAP`], at most `per_call`.
    budget: Duration,
    /// Whether enough of the cap is left after it for another call.
    more_follows: bool,
}

impl Plan {
    /// The plan for a build that has run for `spent`, or `None` when less than [`MIN_ATTEMPT`] is left.
    fn of(spent: Duration, per_call: Duration) -> Option<Self> {
        let left = BUILD_CAP.saturating_sub(spent);

        if left < MIN_ATTEMPT {
            return None;
        }

        let budget = per_call.min(left);

        Some(Self {
            budget,
            more_follows: left.saturating_sub(budget) >= MIN_ATTEMPT,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code::pace;

    #[test]
    fn a_call_gets_its_usual_budget_while_the_cap_leaves_room_for_another() {
        assert_eq!(
            Plan::of(Duration::ZERO, pace::BUDGET),
            Some(Plan {
                budget: pace::BUDGET,
                more_follows: true
            })
        );
        assert_eq!(
            Plan::of(Duration::from_secs(60), pace::BUDGET),
            Some(Plan {
                budget: pace::BUDGET,
                more_follows: true
            })
        );
    }

    #[test]
    fn the_last_calls_are_cut_to_the_time_left_and_none_starts_with_less_than_the_minimum() {
        // 100 s left: shorter than a call's budget, and nothing is left after it
        assert_eq!(
            Plan::of(BUILD_CAP - Duration::from_secs(100), pace::BUDGET),
            Some(Plan {
                budget: Duration::from_secs(100),
                more_follows: false
            })
        );
        // 130 s left: a call of 120 s leaves 10 s, enough for one more
        assert_eq!(
            Plan::of(BUILD_CAP - Duration::from_secs(130), pace::BUDGET),
            Some(Plan {
                budget: pace::BUDGET,
                more_follows: true
            })
        );
        // 123 s left: it leaves 3 s, which is not worth a call
        assert_eq!(
            Plan::of(BUILD_CAP - Duration::from_secs(123), pace::BUDGET).map(|plan| plan.more_follows),
            Some(false)
        );
        assert_eq!(
            Plan::of(BUILD_CAP - MIN_ATTEMPT, pace::BUDGET).map(|plan| plan.budget),
            Some(MIN_ATTEMPT)
        );
        assert_eq!(
            Plan::of(BUILD_CAP - MIN_ATTEMPT + Duration::from_millis(1), pace::BUDGET),
            None
        );
        assert_eq!(Plan::of(BUILD_CAP * 2, pace::BUDGET), None);
    }

    #[test]
    fn calls_that_each_use_their_whole_budget_and_the_grace_end_within_the_longest_run() {
        let mut spent = Duration::ZERO;

        // the worst a call does: it ends past its budget by the grace and the wait for the lock
        while let Some(plan) = Plan::of(spent, pace::BUDGET) {
            spent += plan.budget + GRACE + FAILURE_WAIT;
        }

        assert!(spent <= MAX_RUN, "{spent:?}");
        assert!(spent > BUILD_CAP, "{spent:?}");
        assert!(MAX_RUN < Duration::from_secs(11 * 60), "{MAX_RUN:?}");
    }
}
