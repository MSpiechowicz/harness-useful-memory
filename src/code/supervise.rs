//! The server's side of the worker process ([`super::worker`]): start it, wait for it within a deadline,
//! and kill it when the deadline passes or the request is cancelled.
//!
//! The worker gets the server's environment, except the team database settings, plus every variable of the
//! server's [`Env`] (a server started by a test has them only there) and its home folder. Its stdout is
//! the report, its stderr is forwarded to the server's stderr line by line with a prefix. A worker is
//! killed with [`Child::kill`], which works on every operating system, and always waited for: none is left
//! behind.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::status::Attempt;
use super::worker::{self, Report, Request};
use crate::support::Env;
use crate::support::git::TEAM_VARS;

/// How long past its budget a worker may run before it is killed: the time to answer an error of its own,
/// which includes waiting for the write lock to record it (`FAILURE_WAIT`) and some slack. A shorter grace
/// would kill a worker that is about to say why it failed.
pub const GRACE: Duration = Duration::from_secs(12);

const _: () = assert!(GRACE.as_secs() > super::FAILURE_WAIT.as_secs() + 3);

/// Sets the grace in milliseconds in a debug build (the tests wait for a deadline).
pub const GRACE_VAR: &str = "HARNESS_MEMORY_CODE_GRACE_MS";

/// The `MIMALLOC_ARENA_RESERVE` of a worker, in KiB (64 MiB).
const ARENA_RESERVE_KIB: &str = "65536";

/// Pause between looks at a running worker.
const POLL: Duration = Duration::from_millis(5);

/// The most bytes of a report read; the rest is thrown away.
const REPORT_MAX: usize = 64 * 1024;

/// The most bytes of one forwarded line of the worker's stderr, and of all of them.
const LINE_MAX: usize = 1024;
const STDERR_MAX: usize = 256 * 1024;

/// How to start a worker.
#[derive(Debug, Clone)]
pub struct Worker {
    exe: PathBuf,
    home: PathBuf,
    vars: Vec<(String, String)>,
    /// The memory the worker is limited to, in MiB (what a failed mark is made for).
    memory_mb: u64,
    grace: Duration,
}

/// How a worker ended.
#[derive(Debug)]
pub enum Ended {
    /// It ran to the end and printed a report, a success or an error of its own.
    Report(Report),
    /// The request was cancelled and the worker killed.
    Cancelled,
    /// It did not end in time and was killed.
    TooSlow,
    /// It died by a signal (past its memory limit, crashed, killed from outside), exited without a report, or
    /// said it panicked. `blame` is true when the repository is the likely cause (a crash, an abort, a
    /// non-zero exit without a report), false when something else ended the process (the system's out of
    /// memory killer, the user).
    Broke { blame: bool },
}

/// A worker that ran: its process id (the `builds` rows it started carry it) and how it ended.
#[derive(Debug)]
pub struct Run {
    pub pid: u32,
    pub ended: Ended,
}

impl Worker {
    /// The worker for a server with `env`: this executable, or in a debug build the one `BIN_VAR` names.
    pub fn new(env: &Env) -> Result<Self, String> {
        let exe = match env.var(worker::BIN_VAR) {
            Some(path) if cfg!(debug_assertions) => PathBuf::from(path),
            _ => current_exe()?,
        };

        let grace = match env.var(GRACE_VAR).and_then(|ms| ms.parse::<u64>().ok()) {
            Some(ms) if cfg!(debug_assertions) => Duration::from_millis(ms),
            _ => GRACE,
        };

        Ok(Self {
            exe,
            home: env.home.clone(),
            memory_mb: worker::memory_limit_mb(env),
            grace,
            vars: env
                .vars
                .iter()
                .filter(|(name, _)| !TEAM_VARS.contains(&name.as_str()))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        })
    }

    /// The memory limit of the workers, in MiB.
    pub fn memory_mb(&self) -> u64 {
        self.memory_mb
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.exe);

        command
            .arg(worker::COMMAND)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        for name in TEAM_VARS {
            command.env_remove(name);
        }

        command.envs(self.vars.iter().map(|(name, value)| (name, value)));
        command.env(if cfg!(windows) { "USERPROFILE" } else { "HOME" }, &self.home);

        // mimalloc reserves address space a gigabyte at a time, which a memory limit counts: reserved in
        // small steps, the limit is on what the worker really uses
        command.env("MIMALLOC_ARENA_RESERVE", ARENA_RESERVE_KIB);

        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut command, 0x0800_0000);

        command
    }

    /// Runs a worker on `root` that may take `budget`, killing it when that and the [`GRACE`] are spent or
    /// when `stop` says so. `attempt` says where the call stands in the attempts of one `code build`. Fails
    /// only when the process cannot be started.
    pub fn run(&self, root: &Path, budget: Duration, attempt: Attempt, stop: &dyn Fn() -> bool) -> Result<Run, String> {
        let request = Request {
            root: root.to_string_lossy().into_owned(),
            budget_ms: u64::try_from(budget.as_millis()).unwrap_or(u64::MAX),
            attempt,
        };
        let mut bytes = serde_json::to_vec(&request).map_err(|_| "cannot write the worker's request".to_owned())?;

        bytes.push(b'\n');
        let mut child = self
            .command()
            .spawn()
            .map_err(|error| format!("cannot start the code index worker: {}", error.kind()))?;
        let pid = child.id();

        // a request of a few hundred bytes fits the pipe: no thread needed, and a worker that is already
        // gone is told by its exit status. The pipe stays open until the worker is gone: it reads it for the
        // end (the worker stops when this process does, on every operating system).
        let mut stdin = child.stdin.take();

        if let Some(stdin) = &mut stdin {
            let _ = std::io::Write::write_all(stdin, &bytes);
        }

        let report = child.stdout.take().map(|out| thread::spawn(move || read_report(out)));
        let errors = child.stderr.take().map(|err| thread::spawn(move || forward(err)));
        let deadline = Instant::now() + budget + self.grace;
        let ended = wait(&mut child, deadline, stop);
        let ceiling = cpu_ceiling(budget);

        drop(stdin);

        // the worker is gone, so both pipes are at their end
        let printed = report.and_then(|handle| handle.join().ok()).unwrap_or_default();

        if let Some(handle) = errors {
            let _ = handle.join();
        }

        let ended = match ended {
            Waited::Exited { success, signal, cpu } => judge(success, signal, &printed, spent_its_cpu(cpu, ceiling)),
            Waited::Cancelled => Ended::Cancelled,
            Waited::TooSlow => Ended::TooSlow,
        };

        Ok(Run { pid, ended })
    }
}

enum Waited {
    /// Ended by itself: whether it exited with status 0, and the signal that ended it (Unix).
    Exited {
        success: bool,
        signal: Option<i32>,
        /// The processor time it had used when last looked at (Linux).
        cpu: Duration,
    },
    Cancelled,
    TooSlow,
}

/// Waits for `child` until `deadline`, killing it then or when `stop` says so; it is waited for either way.
fn wait(child: &mut Child, deadline: Instant, stop: &dyn Fn() -> bool) -> Waited {
    let mut cpu = Duration::ZERO;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Waited::Exited {
                    success: status.success(),
                    signal: signal_of(status),
                    cpu,
                };
            }
            Ok(None) => {}
            Err(_) => {
                kill(child);

                return Waited::Exited {
                    success: false,
                    signal: Some(SIGKILL),
                    cpu,
                };
            }
        }

        // a process that is gone cannot be asked, so its last known time is kept
        if let Some(used) = cpu_time(child.id()) {
            cpu = used;
        }

        if stop() {
            kill(child);

            return Waited::Cancelled;
        }

        if Instant::now() >= deadline {
            kill(child);

            return Waited::TooSlow;
        }

        thread::sleep(POLL);
    }
}

const SIGKILL: i32 = 9;

/// The signal that ended a process.
fn signal_of(status: std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        std::os::unix::process::ExitStatusExt::signal(&status)
    }

    #[cfg(not(unix))]
    {
        let _ = status;

        None
    }
}

/// The processor time (user and system) the process `pid` has used, from `/proc` (Linux; `None` elsewhere or
/// when it is gone). The clock ticks of `/proc` are 100 a second on every Linux.
fn cpu_time(pid: u32) -> Option<Duration> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // the name in parentheses may hold spaces: the numbers follow the last parenthesis
        let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
        let ticks = fields
            .get(11)?
            .parse::<u64>()
            .ok()?
            .checked_add(fields.get(12)?.parse::<u64>().ok()?)?;

        Some(Duration::from_millis(ticks.saturating_mul(10)))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;

        None
    }
}

/// The processor seconds at which a worker started with `budget` is ended by the system: the budget and the
/// slack, or the soft limit this process (and so the worker) already had, if lower.
fn cpu_ceiling(budget: Duration) -> Option<Duration> {
    #[cfg(unix)]
    {
        let own = rlimit::getrlimit(rlimit::Resource::CPU).map_or(u64::MAX, |(soft, _)| soft);

        Some(Duration::from_secs(
            budget.as_secs().saturating_add(worker::CPU_SLACK_SECONDS).min(own),
        ))
    }

    #[cfg(not(unix))]
    {
        let _ = budget;

        None
    }
}

/// True when a worker had used (within a second) the processor time it is allowed.
fn spent_its_cpu(used: Duration, ceiling: Option<Duration>) -> bool {
    ceiling.is_some_and(|ceiling| used + Duration::from_secs(1) >= ceiling)
}

/// True when a process ended by `signal` (or by none: a non-zero exit) was most likely broken by the
/// repository it read: it aborted (the allocator's answer to a failed allocation, a panic), crashed, or ran
/// out of processor time. A SIGKILL that came when the process had used the processor time it is allowed is
/// the system's answer to that too (the hard limit is the one that sends it). Anything else (the out of
/// memory killer's or the user's SIGKILL, SIGTERM, SIGHUP) says nothing about the repository.
fn repository_to_blame(signal: Option<i32>, spent_cpu: bool) -> bool {
    const ILL: i32 = 4;
    const ABRT: i32 = 6;
    const BUS: i32 = 7;
    const FPE: i32 = 8;
    const SEGV: i32 = 11;
    const XCPU: i32 = 24;

    signal
        .is_none_or(|signal| matches!(signal, ILL | ABRT | BUS | FPE | SEGV | XCPU) || (signal == SIGKILL && spent_cpu))
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// What a worker that ended by itself means.
fn judge(success: bool, signal: Option<i32>, printed: &[u8], spent_cpu: bool) -> Ended {
    let report = printed
        .split(|byte| *byte == b'\n')
        .rfind(|line| !line.iter().all(u8::is_ascii_whitespace))
        .and_then(|line| serde_json::from_slice::<Report>(line).ok());

    match report {
        Some(report) if report.ok && success => Ended::Report(report),
        Some(report) if !report.ok && !report.failed && report.error.is_some() && !success => Ended::Report(report),
        // the worker said it broke (it panicked, or SQLite ran out of memory)
        Some(report) if report.failed => Ended::Broke { blame: true },
        _ => Ended::Broke {
            blame: repository_to_blame(signal, spent_cpu),
        },
    }
}

/// The first [`REPORT_MAX`] bytes of `out`; the rest is read and dropped so the worker never blocks.
fn read_report(mut out: impl Read) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = [0_u8; 8192];

    while let Ok(read) = out.read(&mut chunk) {
        if read == 0 {
            break;
        }

        let room = REPORT_MAX.saturating_sub(kept.len()).min(read);

        kept.extend_from_slice(&chunk[..room]);
    }

    kept
}

/// Copies the worker's stderr to ours, a line at a time with a prefix, cut to bounded sizes.
fn forward(mut err: impl Read) {
    let mut line: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut total = 0_usize;

    while let Ok(read) = err.read(&mut chunk) {
        if read == 0 {
            break;
        }

        for byte in &chunk[..read] {
            total = total.saturating_add(1);

            if total > STDERR_MAX {
                break;
            }

            if *byte == b'\n' {
                show(&line);
                line.clear();
            } else if line.len() < LINE_MAX {
                line.push(*byte);
            }
        }
    }

    if !line.is_empty() {
        show(&line);
    }
}

fn show(line: &[u8]) {
    eprintln!("[harness-memory] code worker: {}", String::from_utf8_lossy(line));
}

/// This executable. A binary replaced on disk while the server runs reads as `... (deleted)` on Linux:
/// the file that took its place is the one to start.
fn current_exe() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|_| "cannot find the harness-memory executable".to_owned())?;

    if cfg!(target_os = "linux")
        && let Some(shown) = exe.to_str()
        && let Some(path) = shown.strip_suffix(" (deleted)")
    {
        return Ok(PathBuf::from(path));
    }

    Ok(exe)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn ended(success: bool, printed: &str) -> &'static str {
        verdict(success, None, printed)
    }

    fn verdict(success: bool, signal: Option<i32>, printed: &str) -> &'static str {
        match judge(success, signal, printed.as_bytes(), false) {
            Ended::Report(report) if report.ok => "ok",
            Ended::Report(_) => "error",
            Ended::Broke { blame: true } => "broke",
            Ended::Broke { blame: false } => "stopped",
            Ended::Cancelled | Ended::TooSlow => "other",
        }
    }

    #[test]
    fn a_report_is_trusted_only_with_a_matching_exit_status() {
        assert_eq!(ended(true, "{\"ok\":true,\"generation\":3}\n"), "ok");
        assert_eq!(ended(false, "{\"ok\":false,\"error\":\"cancelled\"}\n"), "error");

        assert_eq!(ended(false, "{\"ok\":true,\"generation\":3}\n"), "broke");
        assert_eq!(ended(true, "{\"ok\":false,\"error\":\"x\"}\n"), "broke");
        assert_eq!(
            ended(
                false,
                "{\"ok\":false,\"error\":\"the worker panicked\",\"failed\":true}\n"
            ),
            "broke"
        );
        assert_eq!(ended(false, ""), "broke");
        assert_eq!(ended(true, "not json"), "broke");
        assert_eq!(ended(false, "{\"ok\":false}\n"), "broke");
    }

    #[test]
    fn only_a_death_the_repository_can_cause_is_blamed_on_it() {
        // SIGABRT (the allocator gave up), SIGSEGV, SIGBUS, SIGXCPU: the repository did it
        for signal in [6, 11, 7, 24] {
            assert_eq!(verdict(false, Some(signal), ""), "broke", "signal {signal}");
        }

        // SIGKILL we did not send (the out of memory killer, the user), SIGTERM, SIGHUP: nobody can tell
        for signal in [9, 15, 1] {
            assert_eq!(verdict(false, Some(signal), ""), "stopped", "signal {signal}");
        }

        // an exit without a report is the worker's own failure
        assert_eq!(verdict(false, None, ""), "broke");
        // a report that says it failed is blamed even when a signal ended the process after it
        assert_eq!(
            verdict(false, Some(9), "{\"ok\":false,\"error\":\"x\",\"failed\":true}\n"),
            "broke"
        );
    }

    #[test]
    fn a_sigkill_after_the_processor_time_was_spent_is_the_repositorys_doing() {
        let ceiling = Some(Duration::from_secs(11));
        let spent = |seconds: u64| spent_its_cpu(Duration::from_secs(seconds), ceiling);

        assert!(spent(10) && spent(11));
        assert!(!spent(3), "the system's out of memory killer or the user did it");
        assert!(!spent_its_cpu(Duration::from_secs(100), None));

        let verdict = |signal, cpu| match judge(false, Some(signal), b"", cpu) {
            Ended::Broke { blame } => blame,
            _ => panic!("a worker without a report broke"),
        };

        assert!(verdict(9, true));
        assert!(!verdict(9, false));
        assert!(!verdict(15, true), "only SIGKILL is the limit's answer");
    }

    #[test]
    fn the_last_line_is_the_report() {
        assert_eq!(ended(true, "noise\n{\"ok\":true}\n\n"), "ok");
    }

    #[test]
    fn a_report_is_read_up_to_its_cap() {
        let big = vec![b'a'; REPORT_MAX * 3];

        assert_eq!(read_report(&big[..]).len(), REPORT_MAX);
    }
}
