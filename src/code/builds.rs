//! Which refreshes count as a build, the `builds` rows that record them, and the hook that reports their
//! progress.
//!
//! A refresh that re-reads a handful of files is silent. A refresh of a repository that has no index yet and
//! has source files to read (`Full`) or that touches [`BULK_THRESHOLD`] files or more (`Bulk`) is a build: it gets a `builds` row
//! (only the process that holds the write lock creates it, and only when no other session's build of the
//! repository is running: that one is joined, not repeated) and tells its [`BuildObserver`]. The status
//! file Harness Desktop reads is written by an observer, not here.

use rusqlite::{Connection, OptionalExtension, params};

use super::store::now;

/// A refresh that touches this many files or more (changed plus removed) is a bulk build.
pub const BULK_THRESHOLD: usize = 200;

/// The kind of a build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildKind {
    /// The repository had no indexed files (first index, schema reset, dropped).
    Full,
    /// An index that changed a lot at once (a branch switch, a big pull).
    Bulk,
}

impl BuildKind {
    /// The kind a [`BuildKind::as_str`] value names.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "full" => Some(Self::Full),
            "bulk" => Some(Self::Bulk),
            _ => None,
        }
    }

    /// The value in `builds.kind` and in the status file.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Bulk => "bulk",
        }
    }
}

/// Whether a refresh is a build: `Full` when the repository has no indexed files and there are some to
/// read (a repository without source files has nothing to announce), `Bulk` when `changed + removed`
/// reaches [`BULK_THRESHOLD`], otherwise `None`. Decided after the second walk, inside the write
/// transaction. The counts are of source files: manifests are left out.
pub fn toast_kind(prior_files: usize, changed: usize, removed: usize) -> Option<BuildKind> {
    if prior_files == 0 {
        return (changed > 0).then_some(BuildKind::Full);
    }

    if changed.saturating_add(removed) >= BULK_THRESHOLD {
        Some(BuildKind::Bulk)
    } else {
        None
    }
}

/// Receives the progress of a build, in order: `on_start`, any number of `on_progress`, one `on_finish`.
/// `on_progress` is called for every file, so an observer that writes somewhere should throttle itself.
pub trait BuildObserver {
    /// A build started: its `builds` row, its kind and how many files it will read.
    fn on_start(&mut self, id: i64, kind: BuildKind, total: usize);

    /// `done` of the `total` files are read.
    fn on_progress(&mut self, done: usize);

    /// The build ended: `Ok` when the index is complete, else the reason (also when it was cancelled).
    fn on_finish(&mut self, outcome: Result<(), &str>);

    /// Earlier builds of this repository (the rows `ids`) never ended (their process died): their `builds` rows
    /// now say so.
    fn on_interrupted(&mut self, _ids: &[i64]) {}

    /// The same for the builds `ids` of another repository (at `root`), found while this one was refreshed.
    fn on_interrupted_elsewhere(&mut self, _root: &str, _ids: &[i64]) {}

    /// This call shared the build `owner` of another session (it wrote no row and no status of its own) and
    /// finished the work: the repository is complete, even if the owner died on the way.
    fn on_joined_finish(&mut self, _owner: i64) {}
}

/// An observer that ignores everything.
pub struct NoObserver;

impl BuildObserver for NoObserver {
    fn on_start(&mut self, _id: i64, _kind: BuildKind, _total: usize) {}

    fn on_progress(&mut self, _done: usize) {}

    fn on_finish(&mut self, _outcome: Result<(), &str>) {}
}

/// Inserts the `running` row of a build, owned by this process, and returns its id.
pub(super) fn start(conn: &Connection, repo: i64, kind: BuildKind, total: usize) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO builds(repo_id, kind, status, started_at, files_total, files_done, pid) \
         VALUES (?1, ?2, 'running', ?3, ?4, 0, ?5)",
        params![repo, kind.as_str(), now(), total as i64, i64::from(std::process::id())],
    )?;

    Ok(conn.last_insert_rowid())
}

/// Ends the `running` builds of `repo` that the process `pid` started (a worker that was killed or broke
/// before it could), as errors with the reason; returns their ids.
pub(super) fn fail_pid(conn: &Connection, repo: i64, pid: u32, error: &str) -> rusqlite::Result<Vec<i64>> {
    let mut statement = conn.prepare("SELECT id FROM builds WHERE repo_id = ?1 AND status = 'running' AND pid = ?2")?;
    let ids = statement
        .query_map(params![repo, i64::from(pid)], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    drop(statement);

    for id in &ids {
        finish(conn, *id, Some(error))?;
    }

    Ok(ids)
}

/// `builds` rows older than this many seconds are deleted when a build starts (thirty days).
const KEEP_ROWS_SECONDS: i64 = 30 * 24 * 3600;

/// Deletes the finished rows of old builds, of every repository.
pub(super) fn prune(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM builds WHERE status != 'running' AND started_at < ?1",
        [now() - KEEP_ROWS_SECONDS],
    )
    .map(drop)
}

/// The build of `repo` that is running now, if any: another session's. Call after [`fail_stale`], so a row
/// of a process that is gone does not count.
pub(super) fn running(conn: &Connection, repo: i64) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM builds WHERE repo_id = ?1 AND status = 'running' ORDER BY id DESC LIMIT 1",
        [repo],
        |row| row.get(0),
    )
    .optional()
}

/// A `running` build row older than this many seconds belongs to a process that is gone: a call ends within
/// its time budget (two minutes). Where a process id can be looked up (Linux), a row whose process is gone is
/// ended at once, however young.
pub const STALE_SECONDS: i64 = 300;

/// What [`fail_stale`] ended.
pub(super) struct Interrupted {
    /// The builds of the repository the caller works on.
    pub mine: Vec<i64>,
    /// The roots of the other repositories that had one, with the ids of their builds.
    pub others: Vec<(String, Vec<i64>)>,
}

/// Whether the process `pid` runs: `None` where that cannot be told (not Linux, or a row from before rows
/// carried a process id). A zombie, a process that ended but was not waited for yet, is gone.
pub(super) fn process_alive(pid: i64) -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        if pid <= 0 {
            return None;
        }

        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));

        Some(match stat {
            Ok(stat) => !stat
                .rsplit(')')
                .next()
                .is_some_and(|rest| rest.trim_start().starts_with('Z')),
            Err(error) => error.kind() != std::io::ErrorKind::NotFound,
        })
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;

        None
    }
}

/// Ends the `running` rows, of every repository, whose process is gone as errors: the process id of the row
/// does not run (where that can be told), or the row is too old to be alive.
pub(super) fn fail_stale(conn: &Connection, repo: i64) -> rusqlite::Result<Interrupted> {
    let cutoff = now() - STALE_SECONDS;
    let mut statement = conn.prepare(
        "SELECT builds.id, repos.id, repos.root, builds.started_at, builds.pid FROM builds \
         JOIN repos ON repos.id = builds.repo_id WHERE builds.status = 'running'",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    drop(statement);

    let mut interrupted = Interrupted {
        mine: Vec::new(),
        others: Vec::new(),
    };

    for (id, repo_id, root, started_at, pid) in rows {
        let gone = match process_alive(pid) {
            Some(alive) => !alive,
            None => false,
        };

        if !gone && started_at >= cutoff {
            continue;
        }

        conn.execute(
            "UPDATE builds SET status = 'error', finished_at = ?2, error = 'the build was interrupted' WHERE id = ?1",
            params![id, now()],
        )?;

        if repo_id == repo {
            interrupted.mine.push(id);
        } else if let Some((_, ids)) = interrupted.others.iter_mut().find(|(other, _)| *other == root) {
            ids.push(id);
        } else {
            interrupted.others.push((root, vec![id]));
        }
    }

    Ok(interrupted)
}

/// Records how many files a running build has read.
pub(super) fn progress(conn: &Connection, id: i64, done: usize) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE builds SET files_done = ?2 WHERE id = ?1",
        params![id, done as i64],
    )
    .map(drop)
}

/// Ends a build: `ok`, or `error` with the reason. A build that ended `ok` read every file, whoever read them
/// (a session that shared the work did part of it), so its `files_done` is its `files_total`.
pub(super) fn finish(conn: &Connection, id: i64, error: Option<&str>) -> rusqlite::Result<()> {
    let status = if error.is_some() { "error" } else { "ok" };

    conn.execute(
        "UPDATE builds SET status = ?2, finished_at = ?3, error = ?4, \
         files_done = CASE WHEN ?2 = 'ok' THEN files_total ELSE files_done END WHERE id = ?1",
        params![id, status, now(), error],
    )
    .map(drop)
}

/// A session that shared the build `owner` finished the work: the row, once it is no longer `running` (the
/// owner ended it, or it was ended as interrupted), counts every file as done.
pub(super) fn complete_shared(conn: &Connection, owner: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE builds SET files_done = files_total WHERE id = ?1 AND status != 'running'",
        [owner],
    )
    .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repository_without_files_gets_a_full_build() {
        assert_eq!(toast_kind(0, 1, 0), Some(BuildKind::Full));
        assert_eq!(toast_kind(0, 5000, 0), Some(BuildKind::Full));
        // nothing to read, so nothing to announce
        assert_eq!(toast_kind(0, 0, 0), None);
    }

    #[test]
    fn two_hundred_changes_make_a_bulk_build() {
        assert_eq!(toast_kind(1000, 199, 0), None);
        assert_eq!(toast_kind(1000, 200, 0), Some(BuildKind::Bulk));
        assert_eq!(toast_kind(1000, 0, 199), None);
        assert_eq!(toast_kind(1000, 0, 200), Some(BuildKind::Bulk));
    }

    #[test]
    fn removed_files_count_with_the_changed_ones() {
        assert_eq!(toast_kind(1000, 150, 49), None);
        assert_eq!(toast_kind(1000, 150, 50), Some(BuildKind::Bulk));
        assert_eq!(toast_kind(10, 1, 1), None);
    }

    #[test]
    fn the_kind_names_are_the_stored_values() {
        assert_eq!(BuildKind::Full.as_str(), "full");
        assert_eq!(BuildKind::Bulk.as_str(), "bulk");
    }

    fn memory() -> Connection {
        let conn = Connection::open_in_memory().expect("memory");

        conn.execute_batch(
            "CREATE TABLE repos(id INTEGER PRIMARY KEY, root TEXT);
             CREATE TABLE builds(id INTEGER PRIMARY KEY, repo_id INTEGER, kind TEXT, status TEXT, started_at INTEGER,
               finished_at INTEGER, files_total INTEGER DEFAULT 0, files_done INTEGER DEFAULT 0, error TEXT,
               pid INTEGER DEFAULT 0);
             INSERT INTO repos VALUES (1, '/work/app'), (2, '/work/other');",
        )
        .expect("schema");

        conn
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_running_row_whose_process_is_gone_is_ended_at_once_and_one_whose_process_runs_is_not() {
        let conn = memory();
        let mut finished = std::process::Command::new("true").spawn().expect("true");

        finished.wait().expect("wait");

        let dead = i64::from(finished.id());
        let alive = i64::from(std::process::id());

        for (repo, pid) in [(1, dead), (2, dead), (1, alive)] {
            conn.execute(
                "INSERT INTO builds(repo_id, kind, status, started_at, pid) VALUES (?1, 'full', 'running', ?2, ?3)",
                params![repo, now(), pid],
            )
            .expect("row");
        }

        let interrupted = fail_stale(&conn, 1).expect("fail_stale");
        let status: Vec<String> = conn
            .prepare("SELECT status FROM builds ORDER BY id")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("rows");

        assert_eq!(
            status,
            vec!["error", "error", "running"],
            "young rows of a dead process end at once"
        );
        assert_eq!(interrupted.mine, vec![1]);
        assert_eq!(interrupted.others, vec![("/work/other".to_owned(), vec![2])]);
        assert_eq!(
            running(&conn, 1).expect("running"),
            Some(3),
            "only the live build can be joined"
        );
    }

    #[test]
    fn a_running_row_without_a_process_id_ends_by_age_only() {
        let conn = memory();

        for started in [now(), now() - STALE_SECONDS - 5] {
            conn.execute(
                "INSERT INTO builds(repo_id, kind, status, started_at) VALUES (1, 'full', 'running', ?1)",
                [started],
            )
            .expect("row");
        }

        fail_stale(&conn, 1).expect("fail_stale");

        let status: Vec<String> = conn
            .prepare("SELECT status FROM builds ORDER BY id")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("rows");

        assert_eq!(status, vec!["running", "error"]);
    }

    fn files_of(conn: &Connection, id: i64) -> (String, i64, i64) {
        conn.query_row(
            "SELECT status, files_done, files_total FROM builds WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("row")
    }

    #[test]
    fn a_build_that_ended_ok_counts_every_file_as_done_whoever_read_it() {
        let conn = memory();

        conn.execute(
            "INSERT INTO builds(repo_id, kind, status, started_at, files_total, files_done) \
             VALUES (1, 'bulk', 'running', ?1, 600, 100), (1, 'bulk', 'running', ?1, 600, 100)",
            [now()],
        )
        .expect("rows");

        finish(&conn, 1, None).expect("ok");
        finish(&conn, 2, Some("cancelled")).expect("error");

        assert_eq!(
            files_of(&conn, 1),
            ("ok".to_owned(), 600, 600),
            "the share of a session that helped counts"
        );
        assert_eq!(
            files_of(&conn, 2),
            ("error".to_owned(), 100, 600),
            "a failed build did what it did"
        );
    }

    #[test]
    fn a_session_that_finished_a_shared_build_completes_the_count_of_the_row_once_it_has_ended() {
        let conn = memory();

        conn.execute(
            "INSERT INTO builds(repo_id, kind, status, started_at, files_total, files_done) \
             VALUES (1, 'bulk', 'running', ?1, 600, 100), (1, 'bulk', 'error', ?1, 600, 100)",
            [now()],
        )
        .expect("rows");

        complete_shared(&conn, 1).expect("running owner");
        complete_shared(&conn, 2).expect("ended owner");

        assert_eq!(
            files_of(&conn, 1),
            ("running".to_owned(), 100, 600),
            "a live owner counts for itself"
        );
        assert_eq!(files_of(&conn, 2), ("error".to_owned(), 600, 600));
    }
}
