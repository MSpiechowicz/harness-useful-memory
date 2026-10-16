//! Hardened, shell-free `git` invocations.
//!
//! `git` is looked up in the absolute entries of the injected `PATH` (relative entries would
//! resolve against the project, which could plant a fake binary), then in the usual system
//! folders. Every run has a 15 s timeout and a 16 MiB stdout cap, null stdin, and on Unix its own
//! process group, so a timeout kills git and everything it started.
//!
//! Each command line starts with options that switch off what a repository's own configuration
//! could otherwise execute or redirect: the fsmonitor hook, the untracked cache, external diff
//! programs, pagers, colors and path quoting. Pathspecs are literal. The environment disables
//! prompts and optional locks and fixes the locale. `HOME` and `XDG_CONFIG_HOME` come from the
//! injected [`Env`], so the global configuration read is the one of that home.
//!
//! Every `GIT_*` variable inherited from the server's own environment is dropped (only the ones
//! set here reach git): `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY`,
//! `GIT_COMMON_DIR` and friends would point git at another repository, and `GIT_CONFIG_*`,
//! `GIT_EXTERNAL_DIFF`, `GIT_PAGER`, `GIT_EXEC_PATH`, `GIT_SSH*`, `GIT_ASKPASS` or `GIT_TRACE*`
//! would add configuration, run programs or write files. This includes `GIT_CONFIG_GLOBAL`,
//! `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM` and `GIT_CEILING_DIRECTORIES`: the user's global
//! configuration is still read from the default locations under `HOME`/`XDG_CONFIG_HOME`, and the
//! system configuration from its default location.
//!
//! The team database settings (`HARNESS_MEMORY_DATABASE_URL`, `HARNESS_MEMORY_DATABASE_TOKEN` and
//! `HARNESS_MEMORY_CONFIG`) are dropped too: git has no use for them, and a hook or helper it runs
//! must not see the credentials.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::{Env, Error, OsKind};

/// Time limit of one git command.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(15);

/// Stdout cap of one git command; the rest is read and discarded so git never blocks on a full
/// pipe.
pub const GIT_OUTPUT_CAP: usize = 16 * 1024 * 1024;

/// Options placed before every git subcommand.
const HARDENING: [&str; 15] = [
    "--no-pager",
    "--literal-pathspecs",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.untrackedCache=false",
    "-c",
    "diff.external=",
    "-c",
    "core.pager=cat",
    "-c",
    "color.ui=false",
    "-c",
    "core.quotepath=off",
    "--no-optional-locks",
];

/// Environment of every git command.
const GIT_ENV: [(&str, &str); 3] = [
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("LC_ALL", "C"),
];

/// The server's team database settings (see `crate::backend`), never passed on to git.
const TEAM_VARS: [&str; 3] = [
    "HARNESS_MEMORY_DATABASE_URL",
    "HARNESS_MEMORY_DATABASE_TOKEN",
    "HARNESS_MEMORY_CONFIG",
];

/// Pause between exit checks once git closed its output.
const POLL: Duration = Duration::from_millis(5);

/// Read buffer size for capturing output.
const CHUNK: usize = 16 * 1024;

/// What a finished git command left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    /// Exit code; `None` when killed by a signal.
    pub status: Option<i32>,
    /// Captured stdout (lossy UTF-8), at most [`GIT_OUTPUT_CAP`] bytes before conversion.
    pub stdout: String,
}

impl GitOutput {
    /// True for a normal exit with code 0.
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }
}

/// A git runner bound to one directory.
#[derive(Debug, Clone)]
pub struct Git {
    program: PathBuf,
    dir: PathBuf,
    home: PathBuf,
    xdg_config: PathBuf,
}

impl Git {
    /// Finds `git` for `env`; [`Error::GitNotFound`] when it is not installed.
    pub fn new(env: &Env, dir: &Path) -> Result<Self, Error> {
        let program = find_git(env).ok_or(Error::GitNotFound)?;

        let xdg_config = env
            .var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .unwrap_or_else(|| env.home.join(".config"));

        Ok(Self {
            program,
            dir: dir.to_path_buf(),
            home: env.home.clone(),
            xdg_config,
        })
    }

    /// Runs `git -C <dir> <hardening> <args>` and returns its output, whatever the exit code.
    /// Fails when git cannot start or exceeds [`GIT_TIMEOUT`].
    pub fn run(&self, args: &[&str]) -> Result<GitOutput, Error> {
        let mut command = Command::new(&self.program);
        command
            .arg("-C")
            .arg(&self.dir)
            .args(HARDENING)
            .args(args)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        // Removals first: the variables set below must survive them.
        for (name, _) in std::env::vars_os() {
            if is_git_var(&name) {
                command.env_remove(name);
            }
        }

        for name in TEAM_VARS {
            command.env_remove(name);
        }

        command
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.xdg_config)
            .envs(GIT_ENV);

        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);

        let child = command.spawn().map_err(|error| Error::io("starting git", error))?;
        wait_with_timeout(child, GIT_TIMEOUT)
    }
}

/// Waits for `child` and its stdout until `timeout`, killing it (and its process group) after.
fn wait_with_timeout(mut child: Child, timeout: Duration) -> Result<GitOutput, Error> {
    let deadline = Instant::now() + timeout;

    let Some(stdout) = child.stdout.take() else {
        kill_tree(&mut child);
        return Err(Error::io("reading git output", std::io::ErrorKind::BrokenPipe.into()));
    };

    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || sender.send(read_capped(stdout, GIT_OUTPUT_CAP)));

    // git normally exits right after closing stdout, so the output comes first.
    let Ok(bytes) = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) else {
        kill_tree(&mut child);
        return Err(Error::Timeout(timeout));
    };

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(GitOutput {
                    status: status.code(),
                    stdout: String::from_utf8_lossy(&bytes).into_owned(),
                });
            }
            Ok(None) => {}
            Err(error) => {
                kill_tree(&mut child);
                return Err(Error::io("waiting for git", error));
            }
        }

        if Instant::now() >= deadline {
            kill_tree(&mut child);
            return Err(Error::Timeout(timeout));
        }

        thread::sleep(POLL);
    }
}

/// Reads `reader` to EOF, keeping at most `cap` bytes and discarding the rest.
fn read_capped(mut reader: impl Read, cap: usize) -> Vec<u8> {
    let mut captured = Vec::new();
    let mut chunk = vec![0_u8; CHUNK];

    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };

        let room = cap.saturating_sub(captured.len());
        captured.extend_from_slice(&chunk[..read.min(room)]);
    }

    captured
}

/// Kills the child and, on Unix, its whole process group, then reaps it.
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    kill_group(child.id());

    child.kill().ok();
    child.wait().ok();
}

/// Sends SIGKILL to the process group led by `pid` (the child was spawned with `process_group(0)`).
/// Called only while the child is not reaped yet, so its pid and group id are still its own.
#[cfg(unix)]
fn kill_group(pid: u32) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;

    let Ok(raw) = i32::try_from(pid) else {
        return;
    };

    // ESRCH (group already gone) is the expected race; anything else is not actionable here.
    killpg(Pid::from_raw(raw), Signal::SIGKILL).ok();
}

/// True when variable `name` starts with `GIT_`; Windows variable names ignore ASCII case.
fn is_git_var(name: &OsStr) -> bool {
    let Some(head) = name.as_encoded_bytes().get(..4) else {
        return false;
    };

    if cfg!(windows) {
        head.eq_ignore_ascii_case(b"GIT_")
    } else {
        head == b"GIT_"
    }
}

/// The `git` executable: the first in [`search_dirs`]. On Windows only `git.exe` counts: a
/// `.cmd`/`.bat` wrapper would let `cmd.exe` re-parse the arguments.
fn find_git(env: &Env) -> Option<PathBuf> {
    let name = match env.os {
        OsKind::Windows => "git.exe",
        OsKind::Linux | OsKind::MacOs => "git",
    };

    search_dirs(env)
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

/// The absolute entries of `PATH`, then the usual system folders: a client may start the server
/// with a minimal `PATH` (apps started from the macOS Finder lack the login shell's).
fn search_dirs(env: &Env) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = env
        .var("PATH")
        .map(|path| std::env::split_paths(path).filter(|dir| dir.is_absolute()).collect())
        .unwrap_or_default();

    let fallback: &[&str] = match env.os {
        OsKind::MacOs => &["/opt/homebrew/bin", "/usr/local/bin"],
        OsKind::Linux => &["/usr/local/bin"],
        OsKind::Windows => &[],
    };

    dirs.extend(fallback.iter().map(PathBuf::from));
    dirs
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn env_with_path(home: &Path) -> Env {
        Env::for_tests(home).with_var("PATH", std::env::var("PATH").unwrap_or_default())
    }

    #[test]
    fn only_git_variables_are_dropped() {
        assert!(is_git_var(OsStr::new("GIT_DIR")));
        assert!(is_git_var(OsStr::new("GIT_CONFIG_GLOBAL")));
        assert!(!is_git_var(OsStr::new("GITHUB_TOKEN")));
        assert!(!is_git_var(OsStr::new("PATH")));
        assert!(!is_git_var(OsStr::new("GIT")));
        assert_eq!(is_git_var(OsStr::new("git_dir")), cfg!(windows));
    }

    #[test]
    #[cfg(unix)] // the sample PATH entries are only absolute on Unix
    fn git_is_looked_up_in_absolute_path_entries_then_system_folders() {
        let path = std::env::join_paths(["bin", "/opt/git/bin", ""]).expect("joinable");
        let env = Env::for_tests("/home/u")
            .with_var("PATH", path.to_string_lossy())
            .with_os(OsKind::Linux);

        assert_eq!(
            search_dirs(&env),
            [PathBuf::from("/opt/git/bin"), PathBuf::from("/usr/local/bin")]
        );
    }

    #[test]
    fn git_runs_in_the_directory_with_a_hardened_command_line() {
        let home = TempDir::new().expect("temp");
        let dir = home.path().join("repo");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let git = Git::new(&env_with_path(home.path()), &dir).expect("git is installed");

        let init = git.run(&["init", "-q"]).expect("git runs");
        assert!(init.success());

        let pager = git.run(&["config", "--get", "core.pager"]).expect("git runs");
        assert_eq!(pager.stdout.trim(), "cat");

        let missing = git.run(&["config", "--get", "remote.origin.url"]).expect("git runs");
        assert!(!missing.success());
        assert_eq!(missing.stdout, "");
    }

    #[cfg(unix)]
    #[test]
    fn a_hanging_command_is_killed_at_the_timeout() {
        use std::os::unix::process::CommandExt;

        let child = Command::new("sleep")
            .arg("5")
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("sleep runs");
        let started = Instant::now();

        let result = wait_with_timeout(child, Duration::from_millis(200));

        assert!(matches!(result, Err(Error::Timeout(_))));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
