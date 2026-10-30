//! Refresh-on-read: bring the index of one repository up to date with the files on disk.
//!
//! 1. Walk without a lock and compare stamps (modification time and size). Nothing changed (manifests
//!    included): nothing to do.
//! 2. Otherwise take the write lock (`BEGIN IMMEDIATE`), walk again, reload the stamps, classify the
//!    refresh with [`toast_kind`], read and write. A build commits every [`COMMIT_EVERY`] files so other
//!    sessions wait only briefly; the stamps are reloaded after every commit, so a session that got in
//!    between shares the work instead of repeating it. The generation moves with every commit that wrote
//!    something. A second process blocks on the busy timeout and then finds nothing left to do.
//! 3. `stop` is checked between files; a cancelled refresh rolls back the batch in flight. A call that runs
//!    past its time budget ends the same way, with what it committed kept.
//! 4. A refresh stops with "repository too large to index" before it writes past the [`Caps`], and marks the
//!    repository with the files it saw ([`fingerprint`]). While the files on disk stay the same, later calls
//!    fail at once without reading anything. Any change (files deleted to fit, for one) is a new attempt.
//! 5. A session that cannot get the write lock within a few seconds (another session builds) answers from
//!    what is committed and says so ([`Refresh::stale`]), unless nothing is committed yet. A build lets go
//!    of the lock between batches, so such a session mostly gets a turn instead.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use tree_sitter::Parser;

use super::FAILURE_WAIT;
use super::builds::{self, BuildKind, BuildObserver, toast_kind};
use super::extract;
use super::pace::Pace;
use super::packages;
use super::store::{Caps, Known, PACKAGE_LANG, Stamp, Store, Txn, db, digest_hex};
use super::walk::{self, Class, Entry, LIMITS, Read, Skipped, TOO_LARGE, Walked};

/// A build commits after this many files.
const COMMIT_EVERY: usize = 500;

/// What a refresh did.
#[derive(Debug, Clone, Default)]
pub struct Refresh {
    /// Source and manifest files the walk found.
    pub checked: usize,
    /// Source files whose content changed and were read again.
    pub reparsed: usize,
    /// Files that are gone from disk and were dropped from the index.
    pub removed: usize,
    /// What was left out and why.
    pub skipped: Skipped,
    /// Milliseconds the call took, graph included.
    pub ms: u128,
    /// The build this refresh was, when it was one: its `builds` id and kind.
    pub build: Option<(i64, BuildKind)>,
    /// True when the graph was loaded again (false for a cached graph).
    pub graph_rebuilt: bool,
    /// True when another session held the write lock for long and this call answered from what that session
    /// had committed, without the changes it saw on disk.
    pub stale: bool,
}

/// The state of the repository after a refresh.
pub(super) struct Outcome {
    pub repo: i64,
    pub generation: i64,
    pub refresh: Refresh,
}

/// What the index lacks or holds too much of, compared with a walk.
struct Plan<'a> {
    /// New files and files with another stamp.
    todo: Vec<&'a Entry>,
    /// (file id, is a manifest) of the files that are gone from disk.
    gone: Vec<(i64, bool)>,
}

impl<'a> Plan<'a> {
    fn new(entries: &'a [Entry], files: &HashMap<String, Stamp>) -> Self {
        let present: HashSet<&str> = entries.iter().map(|entry| entry.rel.as_str()).collect();

        Self {
            todo: entries
                .iter()
                .filter(|entry| files.get(&entry.rel).is_none_or(|stamp| stamp.differs(entry)))
                .collect(),
            gone: files
                .iter()
                .filter(|(path, _)| !present.contains(path.as_str()))
                .map(|(_, stamp)| (stamp.id, stamp.lang == PACKAGE_LANG))
                .collect(),
        }
    }

    fn is_empty(&self) -> bool {
        self.todo.is_empty() && self.gone.is_empty()
    }
}

/// True when the walk shows anything the index does not hold, or the index holds anything the walk lost.
fn stale(entries: &[Entry], known: &Known) -> bool {
    entries.len() != known.files.len()
        || entries
            .iter()
            .any(|entry| known.files.get(&entry.rel).is_none_or(|stamp| stamp.differs(entry)))
}

/// What the walk saw: the digest of every file's path, modification time and size, in path order. The
/// same files give the same value.
fn fingerprint(entries: &[Entry]) -> String {
    let mut bytes = Vec::with_capacity(entries.len() * 48);

    for entry in entries {
        bytes.extend_from_slice(entry.rel.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&entry.mtime_ns.to_le_bytes());
        bytes.extend_from_slice(&entry.size.to_le_bytes());
    }

    digest_hex(&bytes)
}

/// The seconds a failed mark holds: a repository a worker broke on is tried again after a day, whatever the
/// files and the limits are.
const FAILED_MARK_SECONDS: i64 = 24 * 3600;

/// What a worker breaking on a repository leaves in `repos.failed`: the walk it broke at, and the conditions
/// of the try, so that a larger memory limit, a new harness-memory or a day's wait is a new attempt.
/// Written `<walk>|<memory limit in MiB>|<version>|<unix seconds>`.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct FailedMark {
    fingerprint: String,
    limit_mb: u64,
    version: String,
    at: i64,
}

impl FailedMark {
    /// A mark for a worker with `limit_mb` that broke on the walk `fingerprint` now.
    pub(super) fn new(fingerprint: &str, limit_mb: u64) -> Self {
        Self {
            fingerprint: fingerprint.to_owned(),
            limit_mb,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            at: super::store::now(),
        }
    }

    pub(super) fn encode(&self) -> String {
        format!("{}|{}|{}|{}", self.fingerprint, self.limit_mb, self.version, self.at)
    }

    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('|');
        let mark = Self {
            fingerprint: parts.next()?.to_owned(),
            limit_mb: parts.next()?.parse().ok()?,
            version: parts.next()?.to_owned(),
            at: parts.next()?.parse().ok()?,
        };

        parts.next().is_none().then_some(mark)
    }

    /// True when `text` (a stored mark) says a worker with `limit_mb` of this version broke on the files of
    /// `fingerprint` less than a day ago. A mark in any other shape holds nothing.
    fn holds(text: &str, fingerprint: &str, limit_mb: u64, now: i64) -> bool {
        Self::parse(text).is_some_and(|mark| {
            mark.fingerprint == fingerprint
                && mark.limit_mb == limit_mb
                && mark.version == env!("CARGO_PKG_VERSION")
                && (0..FAILED_MARK_SECONDS).contains(&(now - mark.at))
        })
    }
}

/// What the index holds when a call starts: the walk, and whether the index already has exactly its files.
pub(super) struct Started {
    pub walked: Walked,
    /// Some when the index holds exactly the files of the walk.
    pub current: Option<Current>,
}

/// The state of a repository whose index is up to date.
pub(super) struct Current {
    pub repo: i64,
    pub generation: i64,
    /// The generation a worker loaded and ranked within its limits (see [`Known::validated`]).
    pub validated: i64,
}

impl Started {
    /// The outcome of a call that found nothing to do.
    pub(super) fn outcome(&self, current: &Current) -> Outcome {
        Outcome {
            repo: current.repo,
            generation: current.generation,
            refresh: Refresh {
                checked: self.walked.entries.len(),
                skipped: self.walked.skipped,
                ..Refresh::default()
            },
        }
    }

    /// The digest of the files the walk saw, which the too-large and failed marks hold.
    pub(super) fn fingerprint(&self) -> String {
        fingerprint(&self.walked.entries)
    }
}

/// Walks `root` (stored as `key`) without a lock and compares it with the index. Fails at once for a
/// repository marked too large at these very files, and (for a server that starts workers, which passes the
/// `limit_mb` of its worker) for one a worker with that limit broke on, unless the index holds exactly these
/// files at a generation a worker validated: that one is served whatever any mark says.
pub(super) fn start(
    store: &Store,
    root: &Path,
    key: &str,
    pace: &Pace,
    limit_mb: Option<u64>,
) -> Result<Started, String> {
    let walked = walk::walk_paced(root, pace)?;
    let mut current = None;

    if let Some(known) = store.known(key)? {
        let print = fingerprint(&walked.entries);

        // too large at these very files: nothing to read, nothing to write
        if known.too_large.as_deref().is_some_and(|mark| mark == print) {
            return Err(TOO_LARGE.into());
        }

        if !stale(&walked.entries, &known) {
            store.touch(known.repo, known.used_at);

            current = Some(Current {
                repo: known.repo,
                generation: known.generation,
                validated: known.validated,
            });
        }

        let served = current
            .as_ref()
            .is_some_and(|current| current.validated == current.generation);
        let broken = match (limit_mb, known.failed.as_deref()) {
            (Some(limit), Some(mark)) => FailedMark::holds(mark, &print, limit, super::store::now()),
            _ => false,
        };

        // broken at these very files: no worker again
        if broken && !served {
            return Err(LIMITS.into());
        }
    }

    Ok(Started { walked, current })
}

/// Brings the index of `root` (stored as `key`) up to date.
pub(super) fn run(
    store: &Store,
    root: &Path,
    key: &str,
    pace: &Pace,
    caps: Caps,
    observer: &mut dyn BuildObserver,
) -> Result<Outcome, String> {
    let started = start(store, root, key, pace, None)?;

    if let Some(current) = &started.current {
        return Ok(started.outcome(current));
    }

    let walked = started.walked;
    let mut build = None;

    match locked(store, root, key, &walked, pace, caps, observer, &mut build) {
        Ok(outcome) => Ok(outcome),
        Err(message) => {
            let too_large = message == TOO_LARGE;

            // the transaction is rolled back; a build row that an earlier batch committed ends as an error,
            // and a repository past the caps is marked, so the next call does not start over
            if (build.is_some() || too_large)
                && let Ok(Some(failed)) = store.write_for(FAILURE_WAIT)
            {
                if let Some((id, _)) = build {
                    let _ = builds::finish(failed.conn(), id, Some(&message));
                }

                if too_large {
                    let _ = failed.mark_too_large(key, &fingerprint(&walked.entries));
                }

                let _ = failed.finish();
            }

            if build.is_some() {
                observer.on_finish(Err(&message));
            }

            Err(message)
        }
    }
}

/// The refresh under the write lock.
#[allow(clippy::too_many_arguments)]
fn locked(
    store: &Store,
    root: &Path,
    key: &str,
    seen: &Walked,
    pace: &Pace,
    caps: Caps,
    observer: &mut dyn BuildObserver,
    build: &mut Option<(i64, BuildKind)>,
) -> Result<Outcome, String> {
    let mut txn = loop {
        if let Some(txn) = store.write_for(store.patience())? {
            break txn;
        }

        // another session holds the lock, most likely a build: what it committed is a consistent graph
        if let Some(known) = store.known(key)?
            && !known.files.is_empty()
            && known.too_large.is_none()
            && known.failed.is_none()
        {
            return Ok(Outcome {
                repo: known.repo,
                generation: known.generation,
                refresh: Refresh {
                    checked: seen.entries.len(),
                    skipped: seen.skipped,
                    stale: true,
                    ..Refresh::default()
                },
            });
        }

        pace.check()?;
    };
    let walked = walk::walk_paced(root, pace)?;
    let repo = txn.repo(key)?;

    // a build whose process died leaves its row `running`
    let interrupted = builds::fail_stale(txn.conn(), repo).map_err(db)?;

    if !interrupted.mine.is_empty() {
        observer.on_interrupted(&interrupted.mine);
    }

    for (other, ids) in &interrupted.others {
        observer.on_interrupted_elsewhere(other, ids);
    }

    let mut work = Work {
        root: root.to_path_buf(),
        repo,
        parser: Parser::new(),
        reparsed: 0,
        removed: 0,
        skipped: walked.skipped,
        caps,
        wrote: false,
        done: 0,
    };
    let mut started: Option<Instant> = None;
    // another session's build of this repository is running: its row and its status file stand for the work
    // this call shares
    let mut joined: Option<i64> = None;

    loop {
        let known = txn.known(key)?.ok_or("the repository is missing from the code index")?;
        let plan = Plan::new(&walked.entries, &known.files);

        if started.is_none() {
            if plan.is_empty() {
                // another process did the work while this one waited for the lock
                txn.clear_marks(repo)?;
                txn.finish()?;

                return Ok(Outcome {
                    repo,
                    generation: known.generation,
                    refresh: Refresh {
                        checked: walked.entries.len(),
                        skipped: walked.skipped,
                        ..Refresh::default()
                    },
                });
            }

            started = Some(Instant::now());

            // manifests are read but are no code: a repository that holds only those is no build to announce
            let sources = plan.todo.iter().filter(|entry| is_source(entry)).count();
            let indexed = known.files.values().filter(|stamp| stamp.lang != PACKAGE_LANG).count();
            let removed = plan.gone.iter().filter(|(_, is_manifest)| !is_manifest).count();

            if let Some(kind) = toast_kind(indexed, sources, removed) {
                // another session's build is running: this call shares it instead of starting a second
                if let Some(owner) = builds::running(txn.conn(), repo).map_err(db)? {
                    joined = Some(owner);
                } else {
                    let id = builds::start(txn.conn(), repo, kind, sources).map_err(db)?;

                    builds::prune(txn.conn()).map_err(db)?;

                    // repositories nobody used for a month make room
                    txn.evict_unused(repo)?;

                    *build = Some((id, kind));

                    // the row is visible (and survives a rollback of the batch in flight, so a cancelled
                    // build ends as an error row)
                    txn.renew()?;

                    // the status file only says `building` once the row is committed: a process killed in
                    // between would leave a file that no row ever ends
                    observer.on_start(id, kind, sources);

                    // others may have worked in between: plan again from the stamps as they are now
                    continue;
                }
            }
        }

        if plan.is_empty() {
            break;
        }

        let batch = if build.is_some() || joined.is_some() {
            COMMIT_EVERY.min(plan.todo.len())
        } else {
            plan.todo.len()
        };
        let (now, later) = plan.todo.split_at(batch);

        work.apply(&txn, now, &plan.gone, &known.files, pace, observer, build.is_some())?;

        if later.is_empty() {
            break;
        }

        // more to do: let other sessions in between
        if work.wrote {
            txn.bump(repo)?;
            work.wrote = false;
        }

        if let Some((id, _)) = build {
            builds::progress(txn.conn(), *id, work.done).map_err(db)?;
        }

        txn.renew_yielding()?;
    }

    // always from the manifests as they are now: a build that was cancelled after it stamped a manifest
    // must not leave stale packages behind
    let found = packages::collect(root, &walked.entries, pace)?;

    if txn.set_packages(repo, &found, &packages::digest(&found))? {
        work.wrote = true;
    }

    if work.wrote {
        txn.bump(repo)?;
    }

    if let Some((id, _)) = build {
        builds::progress(txn.conn(), *id, work.done).map_err(db)?;
        builds::finish(txn.conn(), *id, None).map_err(db)?;
    }

    let generation = txn.generation(repo)?;

    // the shared build's row counts the files the whole repository has, not only the owner's share
    if let Some(owner) = joined {
        builds::complete_shared(txn.conn(), owner).map_err(db)?;
    }

    // the whole repository is in the index
    txn.clear_marks(repo)?;
    txn.finish()?;

    if build.is_some() {
        observer.on_finish(Ok(()));
    }

    // the owner's worker may have died while this call finished the work: the repository is complete, and
    // its status file must not go on saying `building`
    if let Some(owner) = joined {
        observer.on_joined_finish(owner);
    }

    Ok(Outcome {
        repo,
        generation,
        refresh: Refresh {
            checked: walked.entries.len(),
            reparsed: work.reparsed,
            removed: work.removed,
            skipped: work.skipped,
            ms: started.map_or(0, |started| started.elapsed().as_millis()),
            build: *build,
            graph_rebuilt: false,
            stale: false,
        },
    })
}

/// The running totals of a locked refresh.
struct Work {
    root: PathBuf,
    repo: i64,
    parser: Parser,
    reparsed: usize,
    removed: usize,
    skipped: Skipped,
    caps: Caps,
    /// Something that the graph shows changed since the last generation bump.
    wrote: bool,
    /// Files read so far.
    done: usize,
}

impl Work {
    /// Drops the missing files and reads and writes `entries`. The caller commits.
    #[allow(clippy::too_many_arguments)]
    fn apply(
        &mut self,
        txn: &Txn,
        entries: &[&Entry],
        gone: &[(i64, bool)],
        files: &HashMap<String, Stamp>,
        pace: &Pace,
        observer: &mut dyn BuildObserver,
        report: bool,
    ) -> Result<(), String> {
        for (id, is_manifest) in gone {
            txn.drop_file(*id)?;
            self.removed += 1;
            // a removed manifest moves the generation only through the packages digest
            self.wrote |= !is_manifest;
        }

        for entry in entries {
            pace.check()?;

            self.read_one(txn, entry, files.get(&entry.rel), pace)?;

            // the progress counts source files only, like the total it is shown against
            if is_source(entry) {
                self.done += 1;

                if report {
                    observer.on_progress(self.done);
                }
            }
        }

        Ok(())
    }

    fn read_one(&mut self, txn: &Txn, entry: &Entry, old: Option<&Stamp>, pace: &Pace) -> Result<(), String> {
        let lang = lang_of(entry);

        match walk::read_text(&self.root, entry) {
            Read::Skipped(skip) => {
                // stamped without symbols, so a file that stays unreadable does not make every call a refresh
                self.skipped.count(skip);

                if let Some(old) = old {
                    txn.drop_file(old.id)?;
                }

                txn.put_file(self.repo, entry, "", &lang)?;
                self.wrote |= old.is_some_and(|old| !old.hash.is_empty());
            }
            Read::Text(source) => {
                let hash = digest_hex(source.as_bytes());

                if let Some(old) = old
                    && old.hash == hash
                {
                    txn.touch_file(old.id, entry)?;

                    return Ok(());
                }

                if let Some(old) = old {
                    txn.drop_file(old.id)?;
                }

                let file = txn.put_file(self.repo, entry, &hash, &lang)?;

                // a manifest moves the generation only through the packages digest
                if let Class::Source(_) = entry.class {
                    let read = extract::extract_within(&mut self.parser, &entry.rel, &source, pace.deadline());

                    // a file read past the budget is half read: nothing of it is stored
                    pace.check()?;

                    if read.dropped > 0 {
                        self.skipped.truncated += 1;
                    }

                    txn.put_extract(self.repo, file, &read, self.caps)?;
                    self.reparsed += 1;
                    self.wrote = true;
                }
            }
        }

        Ok(())
    }
}

/// True for a source file, false for a manifest.
fn is_source(entry: &Entry) -> bool {
    matches!(entry.class, Class::Source(_))
}

/// The `files.lang` of an entry.
fn lang_of(entry: &Entry) -> String {
    match entry.class {
        Class::Source(lang) => lang.to_string(),
        Class::Package => PACKAGE_LANG.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;

    use super::super::store::now;
    use super::super::{BuildKind, BuildObserver, CodeIndex};

    #[derive(Default)]
    struct Recorder {
        events: Vec<String>,
    }

    impl BuildObserver for Recorder {
        fn on_start(&mut self, id: i64, kind: BuildKind, total: usize) {
            self.events.push(format!("start {id} {} {total}", kind.as_str()));
        }

        fn on_progress(&mut self, done: usize) {
            self.events.push(format!("progress {done}"));
        }

        fn on_finish(&mut self, outcome: Result<(), &str>) {
            self.events.push(format!("finish {}", outcome.is_ok()));
        }

        fn on_joined_finish(&mut self, owner: i64) {
            self.events.push(format!("joined {owner}"));
        }
    }

    fn write_files(root: &std::path::Path, prefix: &str, count: usize) {
        fs::create_dir_all(root.join("src")).expect("mkdir");

        for n in 0..count {
            fs::write(
                root.join(format!("src/{prefix}{n}.rs")),
                format!("pub fn {prefix}{n}() {{}}\n"),
            )
            .expect("write");
        }
    }

    #[test]
    fn a_session_joins_the_build_another_session_is_running_instead_of_starting_a_second() {
        let data = tempfile::tempdir().expect("data");
        let repo = tempfile::tempdir().expect("repo");
        let root = repo.path().canonicalize().expect("canonical");
        let database = data.path().join("code.sqlite3");
        let mut index = CodeIndex::open(&database).expect("open");
        let other = Connection::open(&database).expect("second connection");
        let builds = || -> i64 {
            other
                .query_row("SELECT COUNT(*) FROM builds", [], |row| row.get(0))
                .expect("count")
        };

        write_files(&root, "first", 1);
        index
            .graph_observed(&root, &|| false, &mut Recorder::default())
            .expect("first");

        assert_eq!(builds(), 1, "the first index is a build");

        // another session is part way through a bulk build of this repository
        other
            .execute(
                "INSERT INTO builds(repo_id, kind, status, started_at, files_total, files_done) \
                 VALUES ((SELECT id FROM repos LIMIT 1), 'bulk', 'running', ?1, 600, 100)",
                [now()],
            )
            .expect("running row");
        write_files(&root, "second", 300);

        let mut joined = Recorder::default();
        let (graph, refresh) = index.graph_observed(&root, &|| false, &mut joined).expect("joined");

        assert_eq!(graph.counts().files, 301, "the work is done");
        assert!(refresh.build.is_none(), "no build of its own");
        // no status of its own, only the word that the shared work is finished (the owner may be dead)
        assert_eq!(joined.events, vec!["joined 2".to_owned()]);
        assert_eq!(builds(), 2, "no row of its own");

        let (status, done): (String, i64) = other
            .query_row("SELECT status, files_done FROM builds WHERE id = 2", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("row");

        assert_eq!(
            (status.as_str(), done),
            ("running", 100),
            "the owner's row is left alone"
        );

        // once nobody is building, the same change is a build of its own
        other.execute("UPDATE builds SET status = 'ok'", []).expect("finish");
        write_files(&root, "third", 300);

        let mut alone = Recorder::default();
        let (_, refresh) = index.graph_observed(&root, &|| false, &mut alone).expect("alone");

        assert_eq!(refresh.build.map(|(_, kind)| kind), Some(BuildKind::Bulk));
        assert!(
            alone.events.first().is_some_and(|event| event.ends_with("bulk 300")),
            "{:?}",
            alone.events
        );
        assert_eq!(builds(), 3);
    }

    #[test]
    fn a_new_build_drops_the_rows_of_builds_from_over_thirty_days_ago() {
        let data = tempfile::tempdir().expect("data");
        let repo = tempfile::tempdir().expect("repo");
        let root = repo.path().canonicalize().expect("canonical");
        let database = data.path().join("code.sqlite3");
        let mut index = CodeIndex::open(&database).expect("open");
        let other = Connection::open(&database).expect("second connection");

        write_files(&root, "a", 1);
        index
            .graph_observed(&root, &|| false, &mut Recorder::default())
            .expect("first");

        for (age_days, status) in [(31, "ok"), (45, "error"), (29, "ok"), (90, "running")] {
            other
                .execute(
                    "INSERT INTO builds(repo_id, kind, status, started_at) VALUES ((SELECT id FROM repos LIMIT 1), 'full', ?2, ?1)",
                    rusqlite::params![now() - age_days * 24 * 3600, status],
                )
                .expect("old row");
        }

        write_files(&root, "b", 300);
        index
            .graph_observed(&root, &|| false, &mut Recorder::default())
            .expect("second");

        let ages: Vec<i64> = other
            .prepare("SELECT (?1 - started_at) / 86400 FROM builds ORDER BY id")
            .expect("prepare")
            .query_map([now()], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");

        // the first build, the 29 days old row and the new build stay. The 90 days old `running` row (a dead
        // process) was ended as an error first, and then it was old enough to go.
        assert_eq!(ages, vec![0, 29, 0]);
    }

    /// The mark a worker with `limit_mb` leaves on the repository as it is now.
    fn mark_of(root: &std::path::Path, limit_mb: u64) -> String {
        let walked = super::walk::walk(root, &|| false).expect("walk");

        super::FailedMark::new(&super::fingerprint(&walked.entries), limit_mb).encode()
    }

    fn started(index: &CodeIndex, root: &std::path::Path, limit_mb: Option<u64>) -> Result<bool, String> {
        let key = root.to_str().expect("utf8");
        let pace = super::super::pace::Pace::new(&|| false, std::time::Duration::from_secs(60));

        super::start(&index.store, root, key, &pace, limit_mb).map(|started| started.current.is_some())
    }

    #[test]
    fn a_failed_mark_holds_only_for_the_same_files_the_same_limit_and_this_version_within_a_day() {
        let data = tempfile::tempdir().expect("data");
        let repo = tempfile::tempdir().expect("repo");
        let root = repo.path().canonicalize().expect("canonical");
        let mut index = CodeIndex::open(&data.path().join("code.sqlite3")).expect("open");
        write_files(&root, "a", 2);
        index.graph(&root, &|| false).expect("first");

        let conn = Connection::open(data.path().join("code.sqlite3")).expect("second connection");
        let mark = |text: &str| {
            conn.execute("UPDATE repos SET failed = ?1, validated_generation = -1", [text])
                .expect("mark");
        };
        let good = mark_of(&root, 2048);

        mark(&good);
        assert_eq!(started(&index, &root, Some(2048)), Err(super::LIMITS.to_owned()));

        // a worker that is not limited like that one, or that has no limit to compare (the in-process path)
        assert!(started(&index, &root, Some(4096)).is_ok(), "another memory limit");
        assert!(started(&index, &root, None).is_ok(), "no worker");

        // another version, a day old, a mark in the old shape
        let parts: Vec<&str> = good.split('|').collect();

        mark(&format!("{}|{}|0.0.1|{}", parts[0], parts[1], parts[3]));
        assert!(started(&index, &root, Some(2048)).is_ok(), "another version");

        mark(&format!(
            "{}|{}|{}|{}",
            parts[0],
            parts[1],
            parts[2],
            super::super::store::now() - 90_000
        ));
        assert!(started(&index, &root, Some(2048)).is_ok(), "a day old");

        mark(parts[0]);
        assert!(started(&index, &root, Some(2048)).is_ok(), "the old shape");

        // other files are a new attempt
        mark(&good);
        write_files(&root, "b", 1);
        assert!(started(&index, &root, Some(2048)).is_ok(), "other files");
    }

    #[test]
    fn an_unchanged_index_that_a_worker_validated_is_served_whatever_the_mark_says() {
        let data = tempfile::tempdir().expect("data");
        let repo = tempfile::tempdir().expect("repo");
        let root = repo.path().canonicalize().expect("canonical");
        let mut index = CodeIndex::open(&data.path().join("code.sqlite3")).expect("open");

        write_files(&root, "a", 2);
        index.graph(&root, &|| false).expect("first");

        let conn = Connection::open(data.path().join("code.sqlite3")).expect("second connection");
        let mark = mark_of(&root, 2048);

        conn.execute(
            "UPDATE repos SET failed = ?1, validated_generation = generation",
            [&mark],
        )
        .expect("validated and marked");
        assert_eq!(started(&index, &root, Some(2048)), Ok(true), "served");

        conn.execute("UPDATE repos SET validated_generation = -1", [])
            .expect("not validated");
        assert_eq!(
            started(&index, &root, Some(2048)),
            Err(super::LIMITS.to_owned()),
            "refused"
        );
    }

    #[test]
    fn a_build_row_is_committed_before_its_status_is_announced() {
        #[derive(Default)]
        struct Probe {
            seen: Vec<Option<String>>,
            database: std::path::PathBuf,
        }

        impl BuildObserver for Probe {
            fn on_start(&mut self, id: i64, _kind: BuildKind, _total: usize) {
                // what another process would read at this moment
                let other = Connection::open(&self.database).expect("other process");
                let status = other
                    .query_row("SELECT status FROM builds WHERE id = ?1", [id], |row| row.get(0))
                    .ok();

                self.seen.push(status);
            }

            fn on_progress(&mut self, _done: usize) {}

            fn on_finish(&mut self, _outcome: Result<(), &str>) {}
        }

        let data = tempfile::tempdir().expect("data");
        let repo = tempfile::tempdir().expect("repo");
        let root = repo.path().canonicalize().expect("canonical");
        let database = data.path().join("code.sqlite3");
        let mut index = CodeIndex::open(&database).expect("open");
        let mut probe = Probe {
            seen: Vec::new(),
            database,
        };

        write_files(&root, "a", 1);
        index.graph_observed(&root, &|| false, &mut probe).expect("first");

        assert_eq!(
            probe.seen,
            vec![Some("running".to_owned())],
            "the row is visible when the status says building"
        );
    }
}
