//! The status file of a code index build, which Harness Desktop reads to show a toast.
//!
//! [`StatusFile`] is the [`BuildObserver`] of a `code_*` call. A build (a first index or a bulk change,
//! see [`super::toast_kind`]) writes `<data dir>/code-builds/<sha256(root)[..16]>.json` (0600, replaced
//! atomically) when it starts, at most every 500 ms while it reads files, and when it ends. A refresh
//! that is not a build writes nothing. The file of a finished build is deleted by the next build started
//! more than ten minutes later.
//!
//! ```json
//! {"version":1,"buildId":42,"root":"/work/app","repoName":"app","kind":"full","phase":"building",
//!  "filesTotal":812,"filesDone":120,"startedAt":1761700000,"finishedAt":null,"error":null}
//! ```
//!
//! `kind` is `full` or `bulk`, `phase` is `building`, `ready` or `error`, times are Unix seconds.
//!
//! `code build` may ask for a very large repository again and again, each time a call of its own that runs out
//! of time and goes on where the last one stopped (see [`Attempt`]). The attempts are one build to the person:
//! an attempt that is followed by another does not end the file as an error, and the next one keeps its
//! `buildId`, `kind` and `startedAt` and counts its progress on from where the last one stopped.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::store::{digest_hex, now};
use super::{BuildKind, BuildObserver, TOO_SLOW};
use crate::support::private::{private_folder, read_private, write_atomic};

/// The folder in the data folder that holds the status files.
pub const FOLDER: &str = "code-builds";

/// The version of the document.
const VERSION: u32 = 1;

/// How often progress is written.
const PROGRESS_EVERY: Duration = Duration::from_millis(500);

/// A finished status file older than this many seconds is deleted when the next build starts.
const KEEP_SECONDS: i64 = 600;

/// The most bytes read from a status file when cleaning up.
const READ_MAX: usize = 64 * 1024;

/// The longest error text written.
const ERROR_MAX: usize = 300;

/// The status file of a repository, relative to the data folder `data`: its name is the first 16 hex
/// digits of the SHA-256 of the root path.
pub fn path_of(data: &Path, root: &Path) -> PathBuf {
    let digest = digest_hex(root.to_string_lossy().as_bytes());

    data.join(FOLDER).join(format!("{}.json", &digest[..16]))
}

/// Where a call stands in the attempts that make up one build (see the module docs). The default is a call
/// that stands alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Attempt {
    /// The id of the build an earlier attempt began and the status file showed `building`: that file is taken
    /// over, not replaced by a new build, only while it still shows this id (another session may have written
    /// its own meanwhile).
    pub continued: Option<i64>,
    /// Another attempt follows if this one runs out of time: it does not end the file as an error.
    pub more_follows: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Document {
    version: u32,
    build_id: i64,
    root: String,
    repo_name: String,
    kind: &'static str,
    phase: &'static str,
    files_total: usize,
    files_done: usize,
    started_at: i64,
    finished_at: Option<i64>,
    error: Option<String>,
}

/// What a `building` status file says of the build it shows.
struct Earlier {
    build_id: i64,
    kind: BuildKind,
    started_at: i64,
    files_total: usize,
    files_done: usize,
}

/// Writes the status file of the builds of one repository.
pub struct StatusFile {
    data: PathBuf,
    root: PathBuf,
    document: Option<Document>,
    written: Option<Instant>,
    attempt: Attempt,
    /// The files that earlier attempts of the build read: this attempt counts on from there.
    carried: usize,
    /// The id of the `builds` row of the build this writer began, if it did.
    began: Option<i64>,
}

impl StatusFile {
    /// A writer for the repository at `root`, with the data folder `data`.
    pub fn new(data: &Path, root: &Path) -> Self {
        Self {
            data: data.to_path_buf(),
            root: root.to_path_buf(),
            document: None,
            written: None,
            attempt: Attempt::default(),
            carried: 0,
            began: None,
        }
    }

    /// Makes this writer the one of an attempt of a build that takes several.
    pub fn attempt(mut self, attempt: Attempt) -> Self {
        self.attempt = attempt;

        self
    }

    /// The id of the `builds` row of the build this writer began, once it did. The file may show an earlier
    /// attempt's id instead (see [`Attempt`]), which an earlier writer began.
    pub fn began(&self) -> Option<i64> {
        self.began
    }

    /// The file this writer writes.
    pub fn path(&self) -> PathBuf {
        path_of(&self.data, &self.root)
    }

    /// The build the file on disk shows while it is `building`.
    fn building(&self) -> Option<Earlier> {
        let bytes = read_private(&self.path(), READ_MAX).ok().flatten()?;
        let document = serde_json::from_slice::<Value>(&bytes).ok()?;

        if document.get("phase").and_then(Value::as_str) != Some("building") {
            return None;
        }

        let count = |name: &str| {
            document
                .get(name)
                .and_then(Value::as_u64)
                .and_then(|count| usize::try_from(count).ok())
                .unwrap_or(0)
        };

        Some(Earlier {
            build_id: document.get("buildId").and_then(Value::as_i64)?,
            kind: document
                .get("kind")
                .and_then(Value::as_str)
                .and_then(BuildKind::parse)?,
            started_at: document.get("startedAt").and_then(Value::as_i64)?,
            files_total: count("filesTotal"),
            files_done: count("filesDone"),
        })
    }

    fn write(&mut self) {
        let Some(document) = &self.document else {
            return;
        };

        self.written = Some(Instant::now());

        let result = serde_json::to_vec(document)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                let folder = self.data.join(FOLDER);
                private_folder(&folder, "code builds folder")?;
                write_atomic(&folder, &self.path(), &bytes, "build", "code build status")
            });

        if let Err(message) = result {
            eprintln!("[harness-memory] code index: {message}");
        }
    }
}

impl BuildObserver for StatusFile {
    fn on_start(&mut self, id: i64, kind: BuildKind, total: usize) {
        remove_old(&self.data.join(FOLDER), &self.path());

        self.began = Some(id);

        let earlier = self
            .attempt
            .continued
            .and_then(|expected| self.building().filter(|earlier| earlier.build_id == expected));

        self.carried = earlier.as_ref().map_or(0, |earlier| earlier.files_done);

        // a later attempt has fewer files left to read: the total stays the one the person was told, unless
        // more files turned up
        let total = earlier.as_ref().map_or(total, |earlier| {
            earlier.files_total.max(self.carried.saturating_add(total))
        });

        self.document = Some(Document {
            version: VERSION,
            build_id: earlier.as_ref().map_or(id, |earlier| earlier.build_id),
            root: self.root.to_string_lossy().into_owned(),
            repo_name: self
                .root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            kind: earlier.as_ref().map_or(kind, |earlier| earlier.kind).as_str(),
            phase: "building",
            files_total: total,
            files_done: self.carried,
            started_at: earlier.as_ref().map_or_else(now, |earlier| earlier.started_at),
            finished_at: None,
            error: None,
        });
        self.write();
    }

    fn on_progress(&mut self, done: usize) {
        let due = self.written.is_none_or(|at| at.elapsed() >= PROGRESS_EVERY);

        if let Some(document) = &mut self.document {
            document.files_done = self.carried.saturating_add(done).min(document.files_total);
        }

        if due {
            self.write();
        }
    }

    fn on_interrupted(&mut self, ids: &[i64]) {
        mark_interrupted(&self.data, &self.path(), ids);
    }

    fn on_interrupted_elsewhere(&mut self, root: &str, ids: &[i64]) {
        mark_interrupted(&self.data, &path_of(&self.data, Path::new(root)), ids);
    }

    fn on_joined_finish(&mut self, owner: i64) {
        finish_joined(&self.data, &self.path(), owner);
    }

    fn on_finish(&mut self, outcome: Result<(), &str>) {
        let Some(document) = &mut self.document else {
            return;
        };

        // another attempt goes on: the person still sees one build that runs
        if self.attempt.more_follows && outcome == Err(TOO_SLOW) {
            self.write();
            self.document = None;

            return;
        }

        document.finished_at = Some(now());

        match outcome {
            Ok(()) => {
                document.phase = "ready";
                document.files_done = document.files_total;
            }
            Err(reason) => {
                document.phase = "error";
                document.error = Some(reason.chars().take(ERROR_MAX).collect());
            }
        }

        self.write();
        self.document = None;
    }
}

/// A `building` file that nobody touched for this many seconds belongs to a process that is gone.
const DEAD_SECONDS: i64 = 60;

/// Turns the status file `path` into an error when it still says `building` a build of `ids` (the builds that
/// were just found interrupted: their process is gone, however recently the file was written), or says
/// `building` and nobody touched it: its process died (a live one rewrites it every half second).
fn mark_interrupted(data: &Path, path: &Path, ids: &[i64]) {
    let Ok(Some(bytes)) = read_private(path, READ_MAX) else {
        return;
    };
    let Ok(mut document) = serde_json::from_slice::<Value>(&bytes) else {
        return;
    };

    let ended = document
        .get("buildId")
        .and_then(Value::as_i64)
        .is_some_and(|id| ids.contains(&id));

    if document.get("phase").and_then(Value::as_str) != Some("building")
        || !(ended || untouched_for(path, DEAD_SECONDS))
    {
        return;
    }

    document["phase"] = Value::from("error");
    document["finishedAt"] = Value::from(now());
    document["error"] = Value::from("the build was interrupted");

    if let Ok(bytes) = serde_json::to_vec(&document)
        && let Err(message) = write_atomic(&data.join(FOLDER), path, &bytes, "build", "code build status")
    {
        eprintln!("[harness-memory] code index: {message}");
    }
}

/// The id of the build the status file of the repository at `root` says is `building`, if it does.
pub fn building_id(data: &Path, root: &Path) -> Option<i64> {
    StatusFile::new(data, root).building().map(|earlier| earlier.build_id)
}

/// Turns the status file `path` into `ready` when it still says `building` the build `owner` (a session that
/// shared that build finished the work, and the owner may be dead).
pub fn finish_joined(data: &Path, path: &Path, owner: i64) {
    let Ok(Some(bytes)) = read_private(path, READ_MAX) else {
        return;
    };
    let Ok(mut document) = serde_json::from_slice::<Value>(&bytes) else {
        return;
    };
    let theirs = document.get("buildId").and_then(Value::as_i64) == Some(owner);

    if document.get("phase").and_then(Value::as_str) != Some("building") || !theirs {
        return;
    }

    let total = document.get("filesTotal").cloned().unwrap_or(Value::from(0));

    document["phase"] = Value::from("ready");
    document["filesDone"] = total;
    document["finishedAt"] = Value::from(now());

    if let Ok(bytes) = serde_json::to_vec(&document)
        && let Err(message) = write_atomic(&data.join(FOLDER), path, &bytes, "build", "code build status")
    {
        eprintln!("[harness-memory] code index: {message}");
    }
}

/// Ends the status file of the repository at `root` as an error with `reason` when it still says
/// `building` one of the builds `ids` (a worker process that wrote it was killed before it could end it).
pub fn fail_builds(data: &Path, root: &Path, ids: &[i64], reason: &str) {
    let path = path_of(data, root);
    let Ok(Some(bytes)) = read_private(&path, READ_MAX) else {
        return;
    };
    let Ok(mut document) = serde_json::from_slice::<Value>(&bytes) else {
        return;
    };
    let ours = document
        .get("buildId")
        .and_then(Value::as_i64)
        .is_some_and(|id| ids.contains(&id));

    if document.get("phase").and_then(Value::as_str) != Some("building") || !ours {
        return;
    }

    document["phase"] = Value::from("error");
    document["finishedAt"] = Value::from(now());
    document["error"] = Value::from(reason.chars().take(ERROR_MAX).collect::<String>());

    if let Ok(bytes) = serde_json::to_vec(&document)
        && let Err(message) = write_atomic(&data.join(FOLDER), &path, &bytes, "build", "code build status")
    {
        eprintln!("[harness-memory] code index: {message}");
    }
}

/// Deletes the status files in `folder`, except `keep`, that are old: finished more than
/// [`KEEP_SECONDS`] ago, or still `building` but untouched for that long (their process is gone).
/// Anything unreadable is left alone.
fn remove_old(folder: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };

    let current = now();

    for entry in entries.flatten() {
        let path = entry.path();

        if path == keep || path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }

        let Ok(Some(bytes)) = read_private(&path, READ_MAX) else {
            continue;
        };

        let Ok(document) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };

        let old = match document.get("finishedAt").and_then(Value::as_i64) {
            Some(finished) => current.saturating_sub(finished) > KEEP_SECONDS,
            None => untouched_for(&path, KEEP_SECONDS),
        };

        if old {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// True when `path` was last modified more than `seconds` ago.
fn untouched_for(path: &Path, seconds: i64) -> bool {
    let Ok(modified) = std::fs::metadata(path).and_then(|metadata| metadata.modified()) else {
        return false;
    };

    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age.as_secs() > u64::try_from(seconds).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    fn read(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).expect("file")).expect("json")
    }

    #[test]
    fn a_build_writes_building_then_ready() {
        let data = tempfile::tempdir().expect("data");
        let root = Path::new("/work/app");
        let mut status = StatusFile::new(data.path(), root);

        status.on_start(7, BuildKind::Full, 40);
        let started = read(&status.path());
        assert_eq!(started["version"], 1);
        assert_eq!(started["buildId"], 7);
        assert_eq!(started["root"], "/work/app");
        assert_eq!(started["repoName"], "app");
        assert_eq!(started["kind"], "full");
        assert_eq!(started["phase"], "building");
        assert_eq!(started["filesTotal"], 40);
        assert_eq!(started["filesDone"], 0);
        assert_eq!(started["finishedAt"], Value::Null);
        assert_eq!(started["error"], Value::Null);

        status.on_finish(Ok(()));
        let done = read(&status.path());
        assert_eq!(done["phase"], "ready");
        assert_eq!(done["filesDone"], 40);
        assert!(done["finishedAt"].is_i64());
    }

    #[test]
    fn a_session_that_finished_the_shared_work_turns_the_owners_building_file_into_ready() {
        let data = tempfile::tempdir().expect("data");
        let root = Path::new("/work/app");
        let mut owner = StatusFile::new(data.path(), root);

        // the owner's worker wrote `building` and was killed
        owner.on_start(7, BuildKind::Bulk, 300);

        let mut joined = StatusFile::new(data.path(), root);

        // another build's id is none of its business
        joined.on_joined_finish(8);
        assert_eq!(read(&joined.path())["phase"], "building");

        joined.on_joined_finish(7);

        let done = read(&joined.path());

        assert_eq!(done["phase"], "ready");
        assert_eq!(done["filesDone"], 300);
        assert!(done["finishedAt"].is_i64());
    }

    #[test]
    fn the_file_of_a_build_found_interrupted_ends_at_once_and_any_other_waits_for_its_age() {
        let data = tempfile::tempdir().expect("data");
        let root = Path::new("/work/app");
        let mut owner = StatusFile::new(data.path(), root);

        // the file was just written, as the file of a process that died a moment ago was
        owner.on_start(7, BuildKind::Bulk, 300);

        let mut other = StatusFile::new(data.path(), Path::new("/work/other"));

        other.on_interrupted_elsewhere("/work/app", &[8]);
        assert_eq!(
            read(&owner.path())["phase"],
            "building",
            "another build's id is none of its business"
        );

        other.on_interrupted_elsewhere("/work/app", &[8, 7]);

        let ended = read(&owner.path());

        assert_eq!(ended["phase"], "error");
        assert_eq!(ended["error"], "the build was interrupted");
        assert!(ended["finishedAt"].is_i64());
    }

    #[test]
    fn progress_is_written_at_most_every_half_second() {
        let data = tempfile::tempdir().expect("data");
        let mut status = StatusFile::new(data.path(), Path::new("/work/app"));

        status.on_start(1, BuildKind::Bulk, 1000);
        for done in 1..=500 {
            status.on_progress(done);
        }

        assert_eq!(read(&status.path())["filesDone"], 0);

        status.written = Instant::now().checked_sub(PROGRESS_EVERY);
        status.on_progress(501);
        assert_eq!(read(&status.path())["filesDone"], 501);
    }

    #[test]
    fn a_failed_build_says_why() {
        let data = tempfile::tempdir().expect("data");
        let mut status = StatusFile::new(data.path(), Path::new("/work/app"));

        status.on_start(2, BuildKind::Full, 5);
        status.on_finish(Err("repository too large to index"));

        let failed = read(&status.path());
        assert_eq!(failed["phase"], "error");
        assert_eq!(failed["error"], "repository too large to index");
    }

    #[test]
    fn attempts_that_ran_out_of_time_are_one_build_that_never_shows_an_error() {
        let data = tempfile::tempdir().expect("data");
        let root = Path::new("/work/app");
        let first = Attempt {
            continued: None,
            more_follows: true,
        };
        let middle = Attempt {
            continued: Some(7),
            more_follows: true,
        };
        let last = Attempt {
            continued: Some(7),
            more_follows: false,
        };

        let mut one = StatusFile::new(data.path(), root).attempt(first);

        one.on_start(7, BuildKind::Bulk, 1000);
        one.on_progress(300);
        one.on_finish(Err(TOO_SLOW));

        let held = read(&one.path());

        assert_eq!(held["phase"], "building");
        assert_eq!(held["error"], Value::Null);
        assert_eq!(held["filesDone"], 300);
        assert_eq!(building_id(data.path(), root), Some(7));

        // the next attempt has 700 files left, on a row of its own: the person sees the same build go on
        let mut two = StatusFile::new(data.path(), root).attempt(middle);

        two.on_start(8, BuildKind::Bulk, 700);
        assert_eq!(read(&two.path())["buildId"], 7);
        assert_eq!(read(&two.path())["filesDone"], 300);

        two.on_progress(100);
        two.on_finish(Err(TOO_SLOW));
        assert_eq!(read(&two.path())["filesDone"], 400);
        assert_eq!(read(&two.path())["phase"], "building");

        let mut three = StatusFile::new(data.path(), root).attempt(last);

        three.on_start(9, BuildKind::Full, 600);
        three.on_progress(50);
        three.on_finish(Ok(()));

        let done = read(&three.path());

        assert_eq!(done["phase"], "ready");
        assert_eq!(done["buildId"], 7);
        assert_eq!(done["kind"], "bulk");
        assert_eq!(done["filesTotal"], 1000);
        assert_eq!(done["filesDone"], 1000);
        assert_eq!(done["startedAt"], held["startedAt"]);
        assert_eq!(building_id(data.path(), root), None);
    }

    #[test]
    fn a_writer_knows_the_row_it_began_even_when_the_file_shows_an_earlier_attempts_build() {
        let data = tempfile::tempdir().expect("data");
        let root = Path::new("/work/app");
        let mut one = StatusFile::new(data.path(), root).attempt(Attempt {
            continued: None,
            more_follows: true,
        });

        assert_eq!(one.began(), None);

        one.on_start(7, BuildKind::Full, 10);
        one.on_finish(Err(TOO_SLOW));
        assert_eq!(one.began(), Some(7));

        let mut two = StatusFile::new(data.path(), root).attempt(Attempt {
            continued: Some(7),
            more_follows: false,
        });

        two.on_start(8, BuildKind::Full, 10);
        assert_eq!(read(&two.path())["buildId"], 7);
        assert_eq!(two.began(), Some(8));
    }

    #[test]
    fn the_last_attempt_that_runs_out_of_time_ends_the_build_as_an_error() {
        let data = tempfile::tempdir().expect("data");
        let root = Path::new("/work/app");
        let mut one = StatusFile::new(data.path(), root).attempt(Attempt {
            continued: None,
            more_follows: true,
        });

        one.on_start(7, BuildKind::Full, 10);
        one.on_finish(Err(TOO_SLOW));

        let mut last = StatusFile::new(data.path(), root).attempt(Attempt {
            continued: Some(7),
            more_follows: false,
        });

        last.on_start(8, BuildKind::Full, 10);
        last.on_finish(Err(TOO_SLOW));

        let failed = read(&last.path());

        assert_eq!(
            (failed["phase"].clone(), failed["buildId"].clone()),
            (json!("error"), json!(7))
        );
        assert_eq!(failed["error"], TOO_SLOW);
    }

    #[test]
    fn an_attempt_that_fails_for_another_reason_ends_the_build_and_one_after_an_end_starts_a_new_one() {
        let data = tempfile::tempdir().expect("data");
        let root = Path::new("/work/app");
        let mut one = StatusFile::new(data.path(), root).attempt(Attempt {
            continued: None,
            more_follows: true,
        });

        one.on_start(7, BuildKind::Full, 10);
        one.on_finish(Err("cancelled"));
        assert_eq!(read(&one.path())["phase"], "error");

        // a continued attempt finds no `building` file to take over
        let mut next = StatusFile::new(data.path(), root).attempt(Attempt {
            continued: Some(7),
            more_follows: false,
        });

        next.on_start(8, BuildKind::Full, 10);
        assert_eq!(read(&next.path())["buildId"], 8);
        assert_eq!(read(&next.path())["phase"], "building");
    }

    #[test]
    fn an_attempt_does_not_take_over_a_building_file_another_session_wrote_after_the_check() {
        let data = tempfile::tempdir().expect("data");
        let root = Path::new("/work/app");
        let mut one = StatusFile::new(data.path(), root).attempt(Attempt {
            continued: None,
            more_follows: true,
        });

        one.on_start(7, BuildKind::Bulk, 1000);
        one.on_progress(300);
        one.on_finish(Err(TOO_SLOW));

        // the parent checked the file showed build 7: the worker is told to continue that one
        let mut worker = StatusFile::new(data.path(), root).attempt(Attempt {
            continued: building_id(data.path(), root),
            more_follows: false,
        });

        // another session replaced the file before the worker got the write lock
        let mut foreign = StatusFile::new(data.path(), root);

        foreign.on_start(50, BuildKind::Full, 20);
        foreign.written = Instant::now().checked_sub(PROGRESS_EVERY);
        foreign.on_progress(5);
        assert_eq!(read(&foreign.path())["filesDone"], 5);

        worker.on_start(51, BuildKind::Bulk, 700);

        let shown = read(&worker.path());

        // its own build, not the foreign 50 nor the earlier 7, and none of their progress
        assert_eq!(shown["buildId"], 51);
        assert_eq!(worker.began(), Some(51));
        assert_eq!(shown["kind"], "bulk");
        assert_eq!(shown["filesDone"], 0);
        assert_eq!(shown["filesTotal"], 700);
        assert_eq!(building_id(data.path(), root), Some(51));
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private_and_named_by_the_root() {
        use std::os::unix::fs::PermissionsExt;

        let data = tempfile::tempdir().expect("data");
        let mut status = StatusFile::new(data.path(), Path::new("/work/app"));
        status.on_start(1, BuildKind::Full, 1);

        let path = status.path();
        assert_eq!(
            std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(path.file_name().expect("name").len(), "0123456789abcdef.json".len());
        assert_eq!(path.parent().expect("folder"), data.path().join(FOLDER),);
    }

    #[test]
    fn the_next_build_deletes_old_finished_files_only() {
        let data = tempfile::tempdir().expect("data");
        let folder = data.path().join(FOLDER);
        private_folder(&folder, "builds").expect("folder");

        let put = |name: &str, finished: Value| {
            let text = json!({ "phase": "ready", "finishedAt": finished }).to_string();
            write_atomic(&folder, &folder.join(name), text.as_bytes(), "t", "test").expect("write");
        };

        put("old.json", json!(now() - 3600));
        put("fresh.json", json!(now() - 5));
        put("running.json", Value::Null);

        let mut status = StatusFile::new(data.path(), Path::new("/work/app"));
        status.on_start(1, BuildKind::Full, 1);

        assert!(!folder.join("old.json").exists());
        assert!(folder.join("fresh.json").exists());
        assert!(folder.join("running.json").exists());
        assert!(status.path().exists());
    }
}
