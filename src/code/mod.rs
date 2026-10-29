//! The code index: where code is defined and who uses it, from a local index of a project.
//!
//! [`CodeIndex`] owns one SQLite file (`code.sqlite3`) holding many repositories. Every
//! [`CodeIndex::graph`] call refreshes the repository first (it stat-checks the files and reads only the
//! changed ones, see `refresh.rs`) and then answers from an in-memory [`Graph`] that is loaded again only
//! when the repository's generation moved. There is no watcher and no rebuild command.
//!
//! - [`walk`]: the bounded, link-safe walk and the safe read.
//! - [`extract`]: definitions, references and imports per file, with tree-sitter.
//! - [`store`]: the schema and the writes.
//! - `packages`: crate names, Go modules and TypeScript path aliases.
//! - [`graph`]: resolution, ranking and the text the agent reads.
//! - [`builds`]: which refreshes count as builds, and the hook that reports their progress.
//! - [`worker`] and [`supervise`]: the process that indexes a repository within memory and time limits, and
//!   the server's side of it. The server loads only a graph that a worker has loaded and ranked first.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub mod builds;
pub mod extract;
pub mod graph;
pub mod hint;
pub mod pace;
mod packages;
mod refresh;
pub mod status;
pub mod store;
pub mod supervise;
pub mod switch;
pub mod walk;
pub mod worker;

pub use builds::{BULK_THRESHOLD, BuildKind, BuildObserver, NoObserver, toast_kind};
pub use graph::{Counts, Graph};
pub use pace::TOO_SLOW;
pub use refresh::Refresh;
pub use walk::{CANCELLED, LIMITS, Skipped, TOO_LARGE};

use pace::Pace;
use refresh::Outcome;
use store::{Caps, Store};
use supervise::{Ended, Worker};

/// How many repositories keep their graph in memory.
const CACHED_ROOTS: usize = 3;

/// The estimated bytes ([`Graph::estimated_bytes`]) the graphs in memory may add up to: past it the least
/// recently used ones go (the newest always stays, whatever its size). One graph is at most about 700 MB,
/// so three of the largest would be 2 GB; this keeps a second large graph only while both fit.
const CACHE_BYTES: usize = 1_500_000_000;

/// The most bytes ([`Graph::estimated_bytes`], or what the worker measured of the graph, the larger) one
/// graph may take in the server. A repository whose graph is bigger passed the worker (which has more
/// room) but is not served: the server never loads it. Three graphs of this size would be past
/// [`CACHE_BYTES`] alone.
const GRAPH_BYTES: usize = 700_000_000;

/// Said when the graph of a repository is past [`GRAPH_BYTES`].
pub const GRAPH_TOO_LARGE: &str = "The code graph of this repository is too large to serve.";

/// How often a call asks a worker again when the repository moved on between the worker's validation and
/// the load (another session wrote in between).
const ATTEMPTS: usize = 3;

/// Said when it did not settle in [`ATTEMPTS`] tries.
const MOVING: &str = "The code index kept changing while it was read. Try again.";

/// How long a failed worker run waits for the write lock to record its end. A worker's grace past its budget
/// ([`supervise::GRACE`]) is longer, so it is not killed while it records its own failure.
const FAILURE_WAIT: Duration = Duration::from_secs(5);

/// Said when a worker was ended by something other than the repository (the system's out of memory killer,
/// the user) and nothing is held against the repository.
const STOPPED: &str = "The code index worker was stopped before it finished. Try again.";

/// A graph kept in memory.
struct Cached {
    key: String,
    repo: i64,
    generation: i64,
    bytes: usize,
    graph: Graph,
}

/// The code index of this computer: the database and the graphs of the most recently used repositories.
pub struct CodeIndex {
    store: Store,
    /// The time one call may take.
    budget: Duration,
    /// The most rows of each kind one repository may hold.
    caps: Caps,
    /// Most recently used first, at most [`CACHED_ROOTS`] and [`CACHE_BYTES`] in all.
    cache: VecDeque<Cached>,
    /// Indexes in a process of its own when set (see [`CodeIndex::set_worker`]).
    worker: Option<Worker>,
    /// The folder of the index file, which holds the build status files.
    data: PathBuf,
}

impl CodeIndex {
    /// Opens (creating when missing) the index file at `path`, normally `code.sqlite3` next to the notes
    /// database. On Unix the folder is 0700 and the file 0600. A file of an older schema version is
    /// dropped and rebuilt; one written by a newer harness-memory is left alone and refused.
    pub fn open(path: &Path) -> Result<CodeIndex, String> {
        Ok(CodeIndex {
            store: Store::open(path)?,
            budget: pace::BUDGET,
            caps: Caps::default(),
            cache: VecDeque::new(),
            worker: None,
            data: path.parent().map(Path::to_path_buf).unwrap_or_default(),
        })
    }

    /// Has the refresh of a repository, and the trial load of its graph, done by a worker process
    /// ([`worker`]) that is killed past its memory and time limits, and loads in this process only a
    /// generation such a worker validated. Without it (the default, and the unit tests) everything happens
    /// in this process.
    pub fn set_worker(&mut self, worker: Worker) {
        self.worker = Some(worker);
    }

    /// Sets how long one call may take before it fails with [`TOO_SLOW`] (two minutes by default). What a
    /// refresh committed by then stays.
    pub fn set_budget(&mut self, budget: Duration) {
        self.budget = budget;
    }

    /// Sets how long a refresh waits for another session's write lock before it answers from what that
    /// session committed (two seconds by default).
    pub fn set_patience(&mut self, patience: Duration) {
        self.store.set_patience(patience);
    }

    /// Refreshes the index of the repository at `root` (an absolute, canonical path) and returns its graph.
    /// `stop` is polled between files; a cancelled call fails with [`CANCELLED`] and leaves the index
    /// consistent. A repository past the caps fails with [`TOO_LARGE`].
    pub fn graph(&mut self, root: &Path, stop: &dyn Fn() -> bool) -> Result<(&Graph, Refresh), String> {
        self.graph_observed(root, stop, &mut NoObserver)
    }

    /// Like [`CodeIndex::graph`], and reports the progress of a build (a first index or a bulk change) to
    /// `observer`. With a worker the worker reports it (the status file), and `observer` is not called.
    pub fn graph_observed(
        &mut self,
        root: &Path,
        stop: &dyn Fn() -> bool,
        observer: &mut dyn BuildObserver,
    ) -> Result<(&Graph, Refresh), String> {
        let started = Instant::now();
        let key = root_key(root)?;
        let mut tries = 0;

        let mut refresh = loop {
            tries += 1;

            if let Some(refresh) = self.settle(root, &key, stop, observer)? {
                break refresh;
            }

            if tries >= ATTEMPTS {
                return Err(MOVING.into());
            }
        };

        refresh.ms = started.elapsed().as_millis();

        let hit = self.cache.front().ok_or("the code graph is missing")?;

        Ok((&hit.graph, refresh))
    }

    /// Brings the index of `root` up to date and puts its graph first in the cache. `None` when another
    /// session wrote after the worker validated the generation, so the load did not happen.
    fn settle(
        &mut self,
        root: &Path,
        key: &str,
        stop: &dyn Fn() -> bool,
        observer: &mut dyn BuildObserver,
    ) -> Result<Option<Refresh>, String> {
        let pace = Pace::new(stop, self.budget);
        let outcome = match &self.worker {
            Some(worker) => self.supervised(worker, root, key, &pace)?,
            None => refresh::run(&self.store, root, key, &pace, self.caps, observer)?,
        };
        let mut refresh = outcome.refresh;

        let cached = self.cache.iter().position(|cached| {
            cached.key == key && cached.repo == outcome.repo && cached.generation == outcome.generation
        });

        if let Some(position) = cached {
            if let Some(hit) = self.cache.remove(position) {
                self.cache.push_front(hit);
            }

            return Ok(Some(refresh));
        }

        // the size a worker measured says how much room the graph needs before it is loaded: the graphs that
        // would not fit beside it are dropped first, so memory never holds them both
        let coming = self
            .store
            .conn()
            .query_row(
                "SELECT validated_bytes FROM repos WHERE id = ?1",
                [outcome.repo],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .and_then(|bytes| usize::try_from(bytes).ok())
            .unwrap_or(0);

        self.cache.retain(|cached| cached.key != key);
        self.make_room(coming);

        // the worker's trial was of the generation it validated: only that one is loaded here
        let validated_only = self.worker.is_some();
        let load_pace = Pace::new(stop, self.budget);
        let loaded = self.store.snapshot(|conn| {
            let (generation, validated, reported): (i64, i64, i64) = conn
                .query_row(
                    "SELECT generation, validated_generation, validated_bytes FROM repos WHERE id = ?1",
                    [outcome.repo],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(store::db)?;

            if validated_only && validated != generation {
                return Ok(None);
            }

            // the size the worker measured says it before the graph is loaded here
            let reported = usize::try_from(reported).unwrap_or(0);

            if reported > GRAPH_BYTES {
                return Err(GRAPH_TOO_LARGE.to_owned());
            }

            Ok(Some((
                generation,
                reported,
                Graph::load(conn, outcome.repo, &load_pace)?,
            )))
        })?;
        let Some((generation, reported, graph)) = loaded else {
            return Ok(None);
        };
        let bytes = graph.estimated_bytes().max(reported);

        if bytes > GRAPH_BYTES {
            return Err(GRAPH_TOO_LARGE.into());
        }

        self.cache.retain(|cached| cached.key != key);
        self.cache.push_front(Cached {
            key: key.to_owned(),
            repo: outcome.repo,
            generation,
            bytes,
            graph,
        });
        self.trim();
        refresh.graph_rebuilt = true;

        Ok(Some(refresh))
    }

    /// Drops the graphs that would not fit in memory beside one of `coming` bytes about to be loaded, least
    /// recently used first.
    fn make_room(&mut self, coming: usize) {
        let sizes: Vec<usize> = self.cache.iter().map(|cached| cached.bytes).collect();

        self.cache.truncate(kept_beside(&sizes, coming));
    }

    /// Drops the graphs past [`CACHED_ROOTS`] and past [`CACHE_BYTES`] in all, least recently used first.
    fn trim(&mut self) {
        let sizes: Vec<usize> = self.cache.iter().map(|cached| cached.bytes).collect();

        self.cache.truncate(kept(&sizes));
    }

    /// Like [`refresh::run`], with a worker process doing what can use much memory or time. A repository
    /// the index holds exactly, at a generation a worker validated, needs none.
    fn supervised(&self, worker: &Worker, root: &Path, key: &str, pace: &Pace) -> Result<Outcome, String> {
        let started = refresh::start(&self.store, root, key, pace, Some(worker.memory_mb()))?;

        if let Some(current) = &started.current
            && current.validated == current.generation
        {
            return Ok(started.outcome(current));
        }

        // the generation the repository had before the worker: one it validated is new only if it moved on
        let before = self.store.state(key)?.map(|(_, generation, _)| generation);
        let budget = pace.deadline().saturating_duration_since(Instant::now());
        let run = worker.run(root, budget, pace.stop())?;

        match run.ended {
            Ended::Report(report) if report.ok => {
                let refresh = report.refresh();
                let (repo, generation, _) = self
                    .store
                    .state(key)?
                    .ok_or("the repository is missing from the code index")?;

                Ok(Outcome {
                    repo,
                    generation,
                    refresh,
                })
            }
            // an error the worker named itself ("repository too large to index", "cancelled", ...)
            Ended::Report(report) => Err(report.error.unwrap_or_else(|| LIMITS.to_owned())),
            Ended::Cancelled => {
                self.ended(root, key, run.pid, CANCELLED, None);

                Err(CANCELLED.into())
            }
            Ended::TooSlow => self.gave_up(worker, root, key, run.pid, pace, &started, before, true),
            Ended::Broke { blame } => self.gave_up(worker, root, key, run.pid, pace, &started, before, blame),
        }
    }

    /// A worker ended without a report. A generation it validated before it ended (one the repository did not
    /// have when the call began) is served all the same. A
    /// call that was cancelled meanwhile is a cancel. Otherwise the repository is marked failed only when it
    /// is the likely cause (`blame`: a crash, an abort, a deadline we enforced), not when something else
    /// ended the worker.
    #[allow(clippy::too_many_arguments)]
    fn gave_up(
        &self,
        worker: &Worker,
        root: &Path,
        key: &str,
        pid: u32,
        pace: &Pace,
        started: &refresh::Started,
        before: Option<i64>,
        blame: bool,
    ) -> Result<Outcome, String> {
        if let Some((repo, generation, validated)) = self.store.state(key)?
            && validated == generation
            && before != Some(generation)
        {
            // the worker did its work and was ended after it recorded it (a late kill, the system)
            self.ended(root, key, pid, STOPPED, None);

            return Ok(Outcome {
                repo,
                generation,
                refresh: Refresh {
                    checked: started.walked.entries.len(),
                    skipped: started.walked.skipped,
                    ..Refresh::default()
                },
            });
        }

        if (pace.stop())() {
            self.ended(root, key, pid, CANCELLED, None);

            return Err(CANCELLED.into());
        }

        if blame {
            let mark = refresh::FailedMark::new(&started.fingerprint(), worker.memory_mb()).encode();

            self.ended(root, key, pid, LIMITS, Some(&mark));

            return Err(LIMITS.into());
        }

        self.ended(root, key, pid, STOPPED, None);

        Err(STOPPED.into())
    }

    /// Records that the worker `pid` ended without finishing: the builds it started end as errors (and the
    /// status file with them), and with a `mark` the repository is marked failed at those files, so the
    /// next call fails at once until a file changes. Best effort: a locked file is left for the next call
    /// (its dead `running` rows are ended when they are old enough).
    fn ended(&self, root: &Path, key: &str, pid: u32, reason: &str, mark: Option<&str>) {
        let Ok(Some(txn)) = self.store.write_for(FAILURE_WAIT) else {
            return;
        };
        let ids = txn
            .repo(key)
            .and_then(|repo| builds::fail_pid(txn.conn(), repo, pid, reason).map_err(store::db));
        let marked = match mark {
            Some(fingerprint) => txn.mark_failed(key, fingerprint),
            None => Ok(()),
        };

        if marked.is_ok() && txn.finish().is_ok() {
            status::fail_builds(&self.data, root, &ids.unwrap_or_default(), reason);
        }
    }

    /// What a worker does for [`CodeIndex::set_worker`]: refreshes the index of the repository at `root`,
    /// loads and ranks its whole graph once, and records the generation as validated. Returns the refresh,
    /// the generation and what the graph holds. `between` runs after the refresh and before the load.
    pub fn validate(
        &self,
        root: &Path,
        stop: &dyn Fn() -> bool,
        observer: &mut dyn BuildObserver,
        between: &dyn Fn(),
    ) -> Result<(Refresh, i64, Counts), String> {
        let started = Instant::now();
        let key = root_key(root)?;
        let pace = Pace::new(stop, self.budget);
        let mut outcome = refresh::run(&self.store, root, &key, &pace, self.caps, observer)?;

        between();

        let checked = self.store.snapshot(|conn| {
            let (generation, validated): (i64, i64) = conn
                .query_row(
                    "SELECT generation, validated_generation FROM repos WHERE id = ?1",
                    [outcome.repo],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(store::db)?;

            // a generation validated before (a call that was served what another session committed) needs
            // no second trial
            if validated == generation {
                return Ok((generation, None));
            }

            let before = graph::resident_bytes();
            let graph = Graph::load(conn, outcome.repo, &pace)?;
            let grown = graph::resident_bytes()
                .zip(before)
                .map_or(0, |(after, before)| after.saturating_sub(before));

            graph.trial(&pace)?;

            // what the graph is estimated to take, or what the process grew by loading it, the larger
            Ok((generation, Some((graph.counts(), graph.estimated_bytes().max(grown)))))
        })?;
        let (generation, counts) = match checked {
            (generation, Some((counts, bytes))) => {
                let txn = self
                    .store
                    .write_for(FAILURE_WAIT)?
                    .ok_or("code index database: database is locked")?;

                txn.validate(outcome.repo, generation, bytes)?;
                txn.finish()?;

                (generation, counts)
            }
            (generation, None) => (
                generation,
                Counts {
                    files: 0,
                    symbols: 0,
                    refs: 0,
                    resolved: 0,
                },
            ),
        };

        outcome.refresh.ms = started.elapsed().as_millis();

        Ok((outcome.refresh, generation, counts))
    }
}

/// How many of the cached graphs (`sizes` in bytes, most recently used first) stay: at most
/// [`CACHED_ROOTS`], at most [`CACHE_BYTES`] in all, and always the first.
fn kept(sizes: &[usize]) -> usize {
    let mut total = 0_usize;
    let mut keep = 0;

    for (position, size) in sizes.iter().take(CACHED_ROOTS).enumerate() {
        total = total.saturating_add(*size);

        if position > 0 && total > CACHE_BYTES {
            break;
        }

        keep = position + 1;
    }

    keep
}

/// How many of the cached graphs (`sizes` in bytes, most recently used first) stay when one of `coming` bytes is
/// loaded: at most [`CACHED_ROOTS`] counting the new one, and at most [`CACHE_BYTES`] with it.
fn kept_beside(sizes: &[usize], coming: usize) -> usize {
    let mut total = coming;
    let mut keep = 0;

    for size in sizes.iter().take(CACHED_ROOTS - 1) {
        total = total.saturating_add(*size);

        if total > CACHE_BYTES {
            break;
        }

        keep += 1;
    }

    keep
}

/// The stored name of a repository: its absolute path.
fn root_key(root: &Path) -> Result<String, String> {
    if !root.is_absolute() {
        return Err("the project path must be absolute".into());
    }

    if !root.is_dir() {
        return Err("the project path is not a folder".into());
    }

    root.to_str()
        .map(str::to_string)
        .ok_or_else(|| "the project path is not valid UTF-8".to_string())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn three_small_graphs_stay_and_a_fourth_does_not() {
        assert_eq!(kept(&[10, 10, 10, 10]), 3);
        assert_eq!(kept(&[10]), 1);
        assert_eq!(kept(&[]), 0);
    }

    #[test]
    fn large_graphs_make_room_for_the_newest_by_dropping_the_oldest() {
        let big = 700_000_000;

        // two of them fit within the estimate, a third does not
        assert_eq!(kept(&[big, big, big]), 2);
        // the newest stays even when it alone is past the estimate
        assert_eq!(kept(&[CACHE_BYTES * 2, 10, 10]), 1);
        // small ones behind a large one stay while the total fits
        assert_eq!(kept(&[big, 100_000_000, 100_000_000]), 3);
    }

    #[test]
    fn the_graphs_that_would_not_fit_beside_the_one_to_load_are_dropped_before_it_is_loaded() {
        let big = 541_000_000;

        // two of the size fit together, so with a third coming only one stays
        assert_eq!(kept_beside(&[big, big], big), 1);
        assert_eq!(kept_beside(&[big], big), 1);
        // small ones all stay, up to the number of graphs the cache holds, the new one included
        assert_eq!(kept_beside(&[10, 10, 10], 10), CACHED_ROOTS - 1);
        // nothing is known of the size of a new graph: only the number counts
        assert_eq!(kept_beside(&[big, big, big], 0), 2);
        assert_eq!(kept_beside(&[], big), 0);
        // a graph that is alone past the budget leaves room for nothing
        assert_eq!(kept_beside(&[10], CACHE_BYTES + 1), 0);
    }
}
