//! The worker process of the code index: `harness-memory code-worker`, which nobody types.
//!
//! A repository is untrusted input, and every kind of hostile shape that was found in its files (ignore
//! files, quadratic resolution, string amplification, deep nesting) could use all the memory or time of
//! the MCP server that read it. So the server does not index a repository it has not seen indexed: it
//! starts this process ([`super::supervise`]), which reads one JSON line on stdin
//! (`{"root": "...", "budget_ms": 120000}`), applies its own operating system limits, and
//! then does what the server used to do in its own process: refreshes the index of the repository (the
//! build status file included) and loads and ranks its whole graph once as a trial. When both fit, it
//! records the generation as validated (`repos.validated_generation`) and prints one JSON line,
//! `{"ok":true,"generation":7,...}`, or `{"ok":false,"error":"..."}`, and exits 0 or 1. The server then
//! loads the graph in its own process, and only a generation a worker validated.
//!
//! Limits, all inherited by nothing but this process:
//!
//! - Memory: `RLIMIT_DATA` on Linux (heap and anonymous mappings, since kernel 4.7), 2 GiB unless
//!   `HARNESS_MEMORY_CODE_WORKER_MEMORY_MB` says otherwise (256 to 65536). A failed allocation ends the
//!   process, and the server answers that the repository is past the limits. macOS does not enforce
//!   `RLIMIT_DATA` or `RLIMIT_AS` reliably (the call is tried, its failure ignored), and Windows has no
//!   such limit without a Job Object (not used): there memory is not limited.
//! - Time: the server kills the process when its budget and a short grace are spent (every operating
//!   system), and on Unix `RLIMIT_CPU` ends it at the budget plus ten seconds of processor time as a second
//!   line.
//! - The server's end: the server keeps the worker's standard input open for as long as it lives, and a
//!   thread of the worker that blocks reading it stops the worker when it ends (every operating system,
//!   Windows included, macOS too). On Linux the kernel also kills the worker when the server dies.
//! - What it reads stays in it: no core dump (`RLIMIT_CORE` 0, and on Linux the process is not dumpable), and
//!   the worker does not run for a repository the server would not have handed it (a folder that is not a git
//!   repository, a system folder, a hidden folder of the home folder), or for longer than the server's own
//!   budget, whatever its request says.
//!
//! Test hooks, honoured only by debug builds (`cfg!(debug_assertions)`): `HARNESS_MEMORY_CODE_BUDGET_MS` sets the
//! time one call may take, and `HARNESS_MEMORY_CODE_WORKER_TEST` is
//! `alloc` (allocate past the memory limit), `spin` (never finish), `abort`, or `kill` (the worker ends itself
//! with SIGKILL, as the system's out of memory killer would), run when the refresh has read the first file
//! of its build or ended, `late-abort`, which aborts after the generation was validated, and `stall`, which
//! sleeps for a second and a half there (past a budget of a second) unless the call continues an earlier one. The worker
//! writes its process id to `code-worker.pid` in the data folder first.

use std::cell::Cell;
use std::io::{BufRead, Read, Write};
use std::panic::catch_unwind;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::status::{Attempt, StatusFile};
use super::{BuildKind, BuildObserver, CodeIndex, Counts, Refresh, Skipped, pace};
use crate::project::{canonical_home, code_root_refusal};
use crate::support::Env;
use crate::support::private::data_folder;

/// The subcommand.
pub const COMMAND: &str = "code-worker";

/// Sets the memory the worker may use, in MiB.
pub const MEMORY_VAR: &str = "HARNESS_MEMORY_CODE_WORKER_MEMORY_MB";

/// Index in the server's own process, as before the worker existed: for the unit tests, which run no
/// executable.
pub const INPROCESS_VAR: &str = "HARNESS_MEMORY_CODE_INPROCESS";

/// The test hook (see the module docs).
pub const TEST_VAR: &str = "HARNESS_MEMORY_CODE_WORKER_TEST";

/// The executable a debug build starts as the worker instead of itself: the tests drive a [`crate::Server`]
/// from a test executable, which is not `harness-memory`.
pub const BIN_VAR: &str = "HARNESS_MEMORY_CODE_WORKER_BIN";

/// The time one call may take, in milliseconds, in a debug build (the tests wait for a deadline).
pub const BUDGET_VAR: &str = "HARNESS_MEMORY_CODE_BUDGET_MS";

/// The memory limit unless set, in MiB.
pub const MEMORY_MB_DEFAULT: u64 = 2048;

const MEMORY_MB_MIN: u64 = 256;
const MEMORY_MB_MAX: u64 = 65_536;

/// Processor seconds past the budget at which the operating system ends the worker (SIGXCPU).
#[cfg(unix)]
pub(super) const CPU_SLACK_SECONDS: u64 = 10;

/// Processor seconds past that at which it is killed (SIGKILL) when it ignores SIGXCPU or the signal does
/// not end it. A hard limit equal to the soft one would send SIGKILL at once, which says nothing about the
/// repository to the server.
#[cfg(unix)]
const CPU_HARD_SECONDS: u64 = 5;

/// The most bytes of the request.
const REQUEST_MAX: u64 = 16 * 1024;

/// Set once the server's end of the worker's standard input closed: the server is gone.
static SERVER_GONE: AtomicBool = AtomicBool::new(false);

/// How long a worker whose server is gone gets to notice and stop before it exits anyway.
const ORPHAN_GRACE: Duration = Duration::from_secs(5);

/// The words of the worker's refusal of a root.
const NOT_FOR_THE_WORKER: &str = "the worker was asked for a folder the code index does not read";

/// How long the test hook `stall` sleeps.
const STALL: Duration = Duration::from_millis(1500);

/// The file the test hook writes the worker's process id to.
pub const PID_FILE: &str = "code-worker.pid";

/// The memory the worker may use, in MiB: the variable kept within its range, else the default.
pub fn memory_limit_mb(env: &Env) -> u64 {
    env.var(MEMORY_VAR)
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map_or(MEMORY_MB_DEFAULT, |mb| mb.clamp(MEMORY_MB_MIN, MEMORY_MB_MAX))
}

/// What the server asks of the worker.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    /// The absolute, canonical root of the repository.
    pub root: String,
    /// The time the refresh and the trial may take together.
    pub budget_ms: u64,
    /// Where the call stands in the attempts of one `code build` (a tool call stands alone).
    #[serde(default)]
    pub attempt: Attempt,
}

/// What the worker prints.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Report {
    pub ok: bool,
    /// The reason, when not `ok`: the same words the in-process index uses ("repository too large to
    /// index", "cancelled", ...).
    pub error: Option<String>,
    /// The worker itself judged the repository to have broken it (it panicked), which the server treats
    /// like a worker that was killed.
    pub failed: bool,
    /// The generation the worker validated.
    pub generation: i64,
    pub checked: usize,
    pub reparsed: usize,
    pub removed: usize,
    pub unreadable: usize,
    pub not_utf8: usize,
    pub unsafe_or_large: usize,
    pub truncated: usize,
    pub generated: usize,
    pub stale: bool,
    /// The build this refresh was: its id and kind.
    pub build_id: Option<i64>,
    pub build_kind: Option<String>,
    pub files: usize,
    pub symbols: usize,
    pub refs: usize,
    pub resolved: usize,
    /// The milliseconds the worker took.
    pub ms: u64,
}

impl Report {
    fn error(message: impl Into<String>) -> Self {
        Self {
            error: Some(message.into()),
            ..Self::default()
        }
    }

    fn of(refresh: &Refresh, generation: i64, counts: Counts) -> Self {
        Self {
            ok: true,
            ms: u64::try_from(refresh.ms).unwrap_or(u64::MAX),
            generation,
            checked: refresh.checked,
            reparsed: refresh.reparsed,
            removed: refresh.removed,
            unreadable: refresh.skipped.unreadable,
            not_utf8: refresh.skipped.not_utf8,
            unsafe_or_large: refresh.skipped.unsafe_or_large,
            truncated: refresh.skipped.truncated,
            generated: refresh.skipped.generated,
            stale: refresh.stale,
            build_id: refresh.build.map(|(id, _)| id),
            build_kind: refresh.build.map(|(_, kind)| kind.as_str().to_owned()),
            files: counts.files,
            symbols: counts.symbols,
            refs: counts.refs,
            resolved: counts.resolved,
            ..Self::default()
        }
    }

    /// The refresh this report says the worker did.
    pub fn refresh(&self) -> Refresh {
        Refresh {
            checked: self.checked,
            reparsed: self.reparsed,
            removed: self.removed,
            skipped: Skipped {
                unreadable: self.unreadable,
                not_utf8: self.not_utf8,
                unsafe_or_large: self.unsafe_or_large,
                truncated: self.truncated,
                generated: self.generated,
            },
            ms: self.ms.into(),
            build: self.build_id.zip(self.build_kind.as_deref().and_then(BuildKind::parse)),
            graph_rebuilt: false,
            stale: self.stale,
        }
    }
}

/// `harness-memory code-worker`: one request on stdin, one report line on stdout.
pub fn main() -> ExitCode {
    let report = match catch_unwind(run) {
        Ok(report) => report,
        Err(_) => Report {
            error: Some("the worker panicked".into()),
            failed: true,
            ..Report::default()
        },
    };

    if let Ok(line) = serde_json::to_string(&report) {
        // a server that went away is not worth a panic
        let _ = writeln!(std::io::stdout(), "{line}");
    }

    if report.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn run() -> Report {
    let mut request = match read_request() {
        Ok(request) => request,
        Err(message) => return Report::error(message),
    };
    let Ok(env) = Env::from_process() else {
        return Report::error("the home folder is unknown");
    };

    // the worker trusts no request: a server that went wrong must not get a longer run, or a folder the
    // server itself refuses
    request.budget_ms = request.budget_ms.min(budget_max_ms());

    if let Some(refusal) = code_root_refusal(&canonical_home(&env), &canonicalized(&request.root)) {
        return Report::error(format!("{NOT_FOR_THE_WORKER}: {}", refusal_words(refusal)));
    }

    limits(&env, &request);

    work(&env, &request).unwrap_or_else(report_of_error)
}

/// The report of a run that ended with `message`: the repository broke the worker when SQLite ran out of
/// memory (the same as a failed allocation, which ends the process), else it is an error to pass on.
fn report_of_error(message: String) -> Report {
    if out_of_memory(&message) {
        return Report {
            error: Some(message),
            failed: true,
            ..Report::default()
        };
    }

    Report::error(message)
}

/// The longest budget a request may ask for: the server's own, in milliseconds.
fn budget_max_ms() -> u64 {
    u64::try_from(pace::BUDGET.as_millis()).unwrap_or(u64::MAX)
}

fn canonicalized(root: &str) -> PathBuf {
    dunce::canonicalize(root).unwrap_or_else(|_| PathBuf::from(root))
}

fn refusal_words(refusal: crate::project::CodeRootRefusal) -> &'static str {
    use crate::project::CodeRootRefusal;

    match refusal {
        CodeRootRefusal::Broad => "a system folder or a folder above the home folder",
        CodeRootRefusal::HiddenInHome => "a hidden folder of the home folder",
        CodeRootRefusal::NotGit => "not a git repository",
    }
}

/// True when `message` (an error of the index) says SQLite ran out of memory: the repository used the
/// worker's allowance, like a failed allocation does.
fn out_of_memory(message: &str) -> bool {
    message.to_ascii_lowercase().contains("out of memory")
}

/// Reads the request, one line, and then watches the rest of the standard input on a thread: the server
/// keeps the pipe open while it lives, so its end means the server is gone.
fn read_request() -> Result<Request, String> {
    let mut input = std::io::BufReader::new(std::io::stdin());
    let mut line = String::new();

    input
        .by_ref()
        .take(REQUEST_MAX)
        .read_line(&mut line)
        .map_err(|error| format!("cannot read the request: {}", error.kind()))?;

    std::thread::spawn(move || {
        let mut rest = Vec::new();

        // blocks until the server closes the pipe or ends
        let _ = input.read_to_end(&mut rest);

        SERVER_GONE.store(true, Ordering::Relaxed);

        // a worker that does not look at the flag (a computation that does not poll) is ended all the same
        std::thread::sleep(ORPHAN_GRACE);
        std::process::exit(1);
    });

    serde_json::from_str(&line).map_err(|_| "the request is not valid".to_owned())
}

/// Refreshes the index of the repository, loads and ranks its graph, and records the generation.
fn work(env: &Env, request: &Request) -> Result<Report, String> {
    let data = data_folder(env).ok_or("the data folder is unknown")?;
    let database = crate::database_path(env)
        .ok_or("the data folder is unknown")?
        .with_file_name("code.sqlite3");
    let root = PathBuf::from(&request.root);
    let hook = Hook::new(env, &data, request.attempt);
    let mut observer = Observed {
        status: StatusFile::new(&data, &root).attempt(request.attempt),
        hook: &hook,
    };
    let stop = server_gone;

    if stop() {
        return Err(super::CANCELLED.into());
    }

    let mut index = CodeIndex::open(&database)?;

    index.set_budget(Duration::from_millis(request.budget_ms));

    let (refresh, generation, counts) = index.validate(&root, &stop, &mut observer, &|| hook.fire())?;

    hook.fire_late();

    Ok(Report::of(&refresh, generation, counts))
}

/// True once the server that started this process is gone: its end of the standard input closed.
fn server_gone() -> bool {
    SERVER_GONE.load(Ordering::Relaxed)
}

/// The status file of the build, and the test hook fired once the build has read its first file.
struct Observed<'a> {
    status: StatusFile,
    hook: &'a Hook,
}

impl BuildObserver for Observed<'_> {
    fn on_start(&mut self, id: i64, kind: BuildKind, total: usize) {
        self.status.on_start(id, kind, total);
    }

    fn on_progress(&mut self, done: usize) {
        self.status.on_progress(done);

        // the build's row is committed by now: a worker killed here leaves it `running`
        self.hook.fire();
    }

    fn on_finish(&mut self, outcome: Result<(), &str>) {
        self.status.on_finish(outcome);
    }

    fn on_interrupted(&mut self, ids: &[i64]) {
        self.status.on_interrupted(ids);
    }

    fn on_interrupted_elsewhere(&mut self, root: &str, ids: &[i64]) {
        self.status.on_interrupted_elsewhere(root, ids);
    }

    fn on_joined_finish(&mut self, owner: i64) {
        self.status.on_joined_finish(owner);
    }
}

/// How the test hook misbehaves.
#[derive(Clone, Copy)]
enum Misbehaviour {
    Alloc,
    Spin,
    Abort,
    /// Ends itself with SIGKILL.
    Kill,
    /// Aborts once the generation is validated.
    LateAbort,
    /// Sleeps past a one second budget, in the first attempt of a build only.
    Stall,
}

/// The test hook; none in a release build.
struct Hook {
    mode: Option<Misbehaviour>,
    pid_file: PathBuf,
    memory_mb: u64,
    fired: Cell<bool>,
}

impl Hook {
    fn new(env: &Env, data: &Path, attempt: Attempt) -> Self {
        let mode = if cfg!(debug_assertions) {
            match env.var(TEST_VAR) {
                Some("alloc") => Some(Misbehaviour::Alloc),
                Some("spin") => Some(Misbehaviour::Spin),
                Some("abort") => Some(Misbehaviour::Abort),
                Some("kill") => Some(Misbehaviour::Kill),
                Some("late-abort") => Some(Misbehaviour::LateAbort),
                Some("stall") if attempt.continued.is_none() => Some(Misbehaviour::Stall),
                _ => None,
            }
        } else {
            None
        };

        Self {
            mode,
            pid_file: data.join(PID_FILE),
            memory_mb: memory_limit_mb(env),
            fired: Cell::new(false),
        }
    }

    /// Aborts after the generation was validated, when asked to.
    fn fire_late(&self) {
        if matches!(self.mode, Some(Misbehaviour::LateAbort)) {
            let _ = std::fs::write(&self.pid_file, std::process::id().to_string());

            std::process::abort();
        }
    }

    /// Misbehaves, the first time it is called.
    fn fire(&self) {
        let Some(mode) = self.mode else {
            return;
        };

        if matches!(mode, Misbehaviour::LateAbort) || self.fired.replace(true) {
            return;
        }

        let _ = std::fs::write(&self.pid_file, std::process::id().to_string());

        match mode {
            Misbehaviour::LateAbort => {}
            Misbehaviour::Stall => std::thread::sleep(STALL),
            Misbehaviour::Abort => std::process::abort(),
            Misbehaviour::Kill => {
                #[cfg(unix)]
                let _ = nix::sys::signal::raise(nix::sys::signal::Signal::SIGKILL);

                std::process::abort()
            }
            Misbehaviour::Spin => loop {
                std::hint::spin_loop();
            },
            Misbehaviour::Alloc => {
                // untouched zeroed memory counts against the limit but not against the machine; far past
                // the limit it stops (a system without the limit gets there without harm)
                let chunk: usize = 64 * 1024 * 1024;
                let mut held: Vec<Vec<u8>> = Vec::new();
                let wanted = usize::try_from(self.memory_mb.saturating_mul(2) / 64).unwrap_or(0) + 1;

                for _ in 0..wanted {
                    held.push(vec![0_u8; chunk]);
                }

                std::hint::black_box(&held);
                std::process::exit(3);
            }
        }
    }
}

/// Applies the memory and processor limits of this process (see the module docs).
fn limits(env: &Env, request: &Request) {
    #[cfg(unix)]
    {
        let bytes = memory_limit_mb(env).saturating_mul(1024 * 1024);

        // what the worker reads (a repository's files) must not end up in a core dump: no dump at all, and
        // on Linux the process is not dumpable either (a dump piped to a handler ignores the size limit)
        lower(rlimit::Resource::CORE, 0);

        #[cfg(target_os = "linux")]
        if let Err(error) = nix::sys::prctl::set_dumpable(false) {
            eprintln!("code worker: cannot refuse core dumps: {error}");
        }

        // the server blocks SIGTERM and SIGINT in its threads (it waits for them on one) and a child
        // inherits that: this process is stopped by them like any other
        unblock_signals();

        #[cfg(target_os = "linux")]
        {
            lower(rlimit::Resource::DATA, bytes);

            // the kernel ends this process with the server
            if let Err(error) = nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGKILL) {
                eprintln!("code worker: cannot follow the server's end: {error}");
            }
        }

        // not enforced reliably, tried all the same
        #[cfg(not(target_os = "linux"))]
        lower(rlimit::Resource::AS, bytes);

        lower_with(
            rlimit::Resource::CPU,
            (request.budget_ms / 1000).saturating_add(CPU_SLACK_SECONDS),
            CPU_HARD_SECONDS,
        );
    }

    #[cfg(not(unix))]
    {
        let _ = (env, request);
    }
}

/// Unblocks SIGTERM and SIGINT in this thread, which the server's threads block and a child inherits.
#[cfg(unix)]
fn unblock_signals() {
    use nix::sys::signal::{SigSet, Signal};

    let mut set = SigSet::empty();

    set.add(Signal::SIGTERM);
    set.add(Signal::SIGINT);

    if let Err(error) = set.thread_unblock() {
        eprintln!("code worker: cannot unblock signals: {error}");
    }
}

/// Lowers the limit of `resource` to `wanted`, or leaves it where it was when that is lower (a user's own
/// lower soft limit stays). A system that does not take the limit leaves the process unlimited in it: said on
/// stderr.
#[cfg(unix)]
fn lower(resource: rlimit::Resource, wanted: u64) {
    lower_with(resource, wanted, 0);
}

/// Like [`lower`], with a hard limit `extra` past the soft one (never above the one the process has).
#[cfg(unix)]
fn lower_with(resource: rlimit::Resource, wanted: u64, extra: u64) {
    let result = rlimit::getrlimit(resource).and_then(|(soft, hard)| {
        let value = wanted.min(hard).min(soft);

        rlimit::setrlimit(resource, value, value.saturating_add(extra).min(hard))
    });

    if let Err(error) = result {
        eprintln!("code worker: cannot limit {resource:?}: {error}");
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn sqlite_running_out_of_memory_is_a_failure_of_the_repository_and_other_errors_are_not() {
        let broke = report_of_error("code index database: out of memory".into());

        assert!(!broke.ok);
        assert!(broke.failed, "treated like a worker that was killed");
        assert!(broke.error.is_some());

        for message in [
            "repository too large to index",
            "cancelled",
            "code index database: database is locked",
        ] {
            let report = report_of_error(message.into());

            assert!(!report.failed, "{message}");
            assert_eq!(report.error.as_deref(), Some(message));
        }
    }

    #[test]
    fn the_memory_limit_is_the_variable_kept_within_its_range() {
        let env = Env::for_tests("/home/u");

        assert_eq!(memory_limit_mb(&env), MEMORY_MB_DEFAULT);
        assert_eq!(memory_limit_mb(&env.clone().with_var(MEMORY_VAR, "4096")), 4096);
        assert_eq!(memory_limit_mb(&env.clone().with_var(MEMORY_VAR, "1")), MEMORY_MB_MIN);
        assert_eq!(
            memory_limit_mb(&env.clone().with_var(MEMORY_VAR, "99999999")),
            MEMORY_MB_MAX
        );
        assert_eq!(memory_limit_mb(&env.with_var(MEMORY_VAR, "plenty")), MEMORY_MB_DEFAULT);
    }

    #[test]
    fn a_report_carries_what_a_refresh_did() {
        let refresh = Refresh {
            checked: 9,
            reparsed: 4,
            removed: 1,
            skipped: Skipped {
                unreadable: 2,
                not_utf8: 3,
                unsafe_or_large: 5,
                truncated: 7,
                generated: 6,
            },
            build: Some((12, BuildKind::Bulk)),
            stale: true,
            ..Refresh::default()
        };
        let counts = Counts {
            files: 8,
            symbols: 20,
            refs: 30,
            resolved: 25,
        };
        let line = serde_json::to_string(&Report::of(&refresh, 6, counts)).expect("json");
        let back: Report = serde_json::from_str(&line).expect("parse");

        assert!(back.ok);
        assert_eq!(back.generation, 6);
        assert_eq!(back.files, 8);

        let again = back.refresh();

        assert_eq!(
            (again.checked, again.reparsed, again.removed, again.skipped, again.stale),
            (9, 4, 1, refresh.skipped, true)
        );
        assert_eq!(again.build, Some((12, BuildKind::Bulk)));
    }
}
