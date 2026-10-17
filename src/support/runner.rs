//! Runs a saved script, shell-free.
//!
//! The body is written (after a UTF-8 byte-order mark when [`Run::bom`] asks for one) to a file in a
//! private temporary folder (`0700` on Unix, removed after the
//! run) inside `run` next to the database (`<data dir>/harness-memory/run`), never in the shared
//! system temporary folder: Node and Bun look for modules and settings in every parent folder of
//! the script, and no other user may plant one there. Its interpreter, found by name in the
//! absolute `PATH` entries and the usual system folders (as for git), runs it as
//! `<interpreter> <file> <args…>`: arguments are passed as they are, never through a shell.
//! Standard input is piped (empty when none is given), stdout and stderr are read at the same
//! time, masked with [`redact_output`] and then cut to their caps (a little more than the cap is
//! kept until then, so a secret crossing the cap is masked whole; the rest is read and dropped so
//! the script never blocks on a full pipe).
//!
//! A run ends when the script exits. Processes it left in the background that still hold its
//! output get [`DRAIN_GRACE`], then (on Unix) its process group is killed.
//!
//! The script inherits the server's environment, `GH_TOKEN`, `PATH` and the rest included, except
//! the team database settings and the script settings (`HARNESS_MEMORY_SCRIPTS`,
//! `HARNESS_MEMORY_TRUST_TEAM_SCRIPTS`): a script has no use for them and must not see the
//! database credentials. On Unix it runs in its own process group, so a timeout kills
//! everything it started. On Windows a timeout kills the interpreter only (a Job Object needs
//! unsafe code, which this crate forbids): processes it started in the background keep running.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::git::{TEAM_VARS, search_dirs};
use super::redact::redact_output;
use super::{Env, OsKind};

/// The script settings (routing, team trust, the approval wait), never passed on.
const SCRIPT_VARS: [&str; 3] = [
    "HARNESS_MEMORY_SCRIPTS",
    "HARNESS_MEMORY_TRUST_TEAM_SCRIPTS",
    "HARNESS_MEMORY_APPROVAL_WAIT_MS",
];

/// Pause between checks while the script runs.
const POLL: Duration = Duration::from_millis(5);

/// How long the output readers get to finish after the script exited or was killed.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Read buffer size for capturing output.
const CHUNK: usize = 16 * 1024;

/// Bytes kept past a stream's cap until it is masked, so a secret crossing the cap (a private key
/// block included) is recognized and masked before the cut.
const MASK_LOOKAHEAD: usize = 16 * 1024;

/// The folder next to the database that holds the private run folders.
const RUN_FOLDER: &str = "run";

/// How to run one script.
#[derive(Debug, Clone)]
pub struct Run<'a> {
    /// Interpreter names to look for, in order (`python3`, then `python`).
    pub interpreters: &'a [&'a str],
    /// The name of the file the body is written to (its extension matters to some interpreters).
    pub file_name: &'a str,
    pub body: &'a str,
    /// Writes a UTF-8 byte-order mark before the body (for Python: the file is then read as UTF-8
    /// whatever it declares).
    pub bom: bool,
    pub args: &'a [String],
    pub stdin: &'a str,
    /// The working directory.
    pub cwd: &'a Path,
    pub timeout: Duration,
    /// Bytes of stdout kept.
    pub stdout_max: usize,
    /// Bytes of stderr kept.
    pub stderr_max: usize,
}

/// One captured output stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// Lossy UTF-8 of the kept bytes, secrets masked.
    pub text: String,
    /// True when the stream was longer than its cap.
    pub truncated: bool,
}

/// What a run left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// The exit code; `None` when the script was killed or ended by a signal.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration: Duration,
    pub stdout: Output,
    pub stderr: Output,
}

/// Runs `run` with the interpreter found for `env`. Fails when no interpreter is installed or the
/// script cannot be started; a script that fails or times out is a [`Finished`] run.
pub fn run(env: &Env, run: &Run) -> Result<Finished, String> {
    let program = find_interpreter(env, run.interpreters).ok_or_else(|| missing(run.interpreters))?;
    let folder = private_folder(env)?;
    let file = folder.path().join(run.file_name);
    write_private(&file, run.bom, run.body)?;

    let mut command = Command::new(&program);
    command
        .arg(&file)
        .args(run.args)
        .current_dir(run.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for name in TEAM_VARS.into_iter().chain(SCRIPT_VARS) {
        command.env_remove(name);
    }

    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);

    let started = Instant::now();
    let name = program.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let mut child = command
        .spawn()
        .map_err(|error| format!("starting {name} failed: {error}"))?;

    feed_stdin(&mut child, run.stdin.as_bytes().to_vec());

    let stdout = child.stdout.take().map(|pipe| Reader::spawn(pipe, run.stdout_max));
    let stderr = child.stderr.take().map(|pipe| Reader::spawn(pipe, run.stderr_max));
    let deadline = started.checked_add(run.timeout);
    let (exit_code, timed_out) = wait(&mut child, deadline, [&stdout, &stderr]);

    Ok(Finished {
        exit_code,
        timed_out,
        duration: started.elapsed(),
        stdout: stdout.map(Reader::finish).unwrap_or_default(),
        stderr: stderr.map(Reader::finish).unwrap_or_default(),
    })
}

/// Waits until the script exits or `deadline` passes (`None`: no deadline), when the script and
/// (on Unix) its process group are killed. Returns the exit code and whether the deadline passed.
/// After the exit, processes the script started that still hold its output get [`DRAIN_GRACE`],
/// then (on Unix) the group is killed: a group outlives its leader while a member runs, and the
/// system hands out no process id still in use as a group id, so this reaches only what the script
/// started.
fn wait(child: &mut Child, deadline: Option<Instant>, readers: [&Option<Reader>; 2]) -> (Option<i32>, bool) {
    let drained = || readers.iter().all(|reader| reader.as_ref().is_none_or(Reader::is_done));

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let until = Instant::now() + DRAIN_GRACE;
                while !drained() && Instant::now() < until {
                    thread::sleep(POLL);
                }

                #[cfg(unix)]
                if !drained() {
                    super::git::kill_group(child.id());
                }

                return (status.code(), false);
            }
            Ok(None) => {}
            Err(_) => {
                kill_tree(child);
                return (None, false);
            }
        }

        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            kill_tree(child);
            return (None, true);
        }

        thread::sleep(POLL);
    }
}

/// Writes `input` to the child's stdin from its own thread, then closes it. A script that exits
/// without reading it ends the write with a broken pipe, which is fine.
fn feed_stdin(child: &mut Child, input: Vec<u8>) {
    let Some(mut pipe) = child.stdin.take() else {
        return;
    };

    thread::spawn(move || {
        pipe.write_all(&input).ok();
    });
}

/// Kills the child and, on Unix, its whole process group, then reaps it.
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    super::git::kill_group(child.id());

    child.kill().ok();
    child.wait().ok();
}

/// The bytes read so far from one stream.
#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

/// A stream read to its end on its own thread.
struct Reader {
    captured: Arc<Mutex<Captured>>,
    thread: JoinHandle<()>,
    /// Bytes of masked text kept.
    cap: usize,
}

impl Reader {
    /// Reads `pipe` to EOF, keeping at most `cap` bytes and [`MASK_LOOKAHEAD`] more.
    fn spawn(mut pipe: impl Read + Send + 'static, cap: usize) -> Self {
        let captured = Arc::new(Mutex::new(Captured::default()));
        let shared = Arc::clone(&captured);
        let kept = cap.saturating_add(MASK_LOOKAHEAD);

        let thread = thread::spawn(move || {
            let mut chunk = vec![0_u8; CHUNK];

            loop {
                let read = match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };

                let mut captured = shared.lock().unwrap_or_else(PoisonError::into_inner);
                let room = kept.saturating_sub(captured.bytes.len());
                captured.bytes.extend_from_slice(&chunk[..read.min(room)]);
                captured.truncated |= read > room;
            }
        });

        Self { captured, thread, cap }
    }

    fn is_done(&self) -> bool {
        self.thread.is_finished()
    }

    /// What was read, masked, then cut to the cap on a character boundary, after giving a reader
    /// still running (the stream held open by a process outside the script's group)
    /// [`DRAIN_GRACE`] to finish.
    fn finish(self) -> Output {
        let until = Instant::now() + DRAIN_GRACE;
        while !self.is_done() && Instant::now() < until {
            thread::sleep(POLL);
        }

        let captured = self.captured.lock().unwrap_or_else(PoisonError::into_inner);
        let mut text = redact_output(&String::from_utf8_lossy(&captured.bytes));
        let over = text.len() > self.cap;

        if over {
            text.truncate(text.floor_char_boundary(self.cap));
        }

        Output {
            text,
            truncated: captured.truncated || over,
        }
    }
}

/// The first of `names` found in [`search_dirs`] (`.exe` added on Windows).
fn find_interpreter(env: &Env, names: &[&str]) -> Option<PathBuf> {
    let dirs = search_dirs(env);

    names.iter().find_map(|name| {
        let file = match env.os {
            OsKind::Windows => format!("{name}.exe"),
            OsKind::Linux | OsKind::MacOs => (*name).to_owned(),
        };

        dirs.iter()
            .map(|dir| dir.join(&file))
            .find(|candidate| super::git::is_executable_file(candidate))
    })
}

/// The error for a missing interpreter: `python3 (or python) was not found on PATH`.
fn missing(names: &[&str]) -> String {
    let shown = match names {
        [first, rest @ ..] if !rest.is_empty() => format!("{first} (or {})", rest.join(", ")),
        _ => names.join(", "),
    };

    format!("{shown} was not found on PATH: install it to run this script")
}

/// A new temporary folder only this user can open, in [`RUN_FOLDER`] next to the database (both
/// `0700` on Unix).
fn private_folder(env: &Env) -> Result<tempfile::TempDir, String> {
    let Some(base) = crate::database_path(env).and_then(|path| path.parent().map(|dir| dir.join(RUN_FOLDER))) else {
        return Err("the data folder is unknown, so there is nowhere to write the script".to_owned());
    };

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);

    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);

    builder
        .create(&base)
        .map_err(|error| format!("creating the script folder failed: {error}"))?;
    make_private(&base)?;

    let folder = tempfile::Builder::new()
        .prefix("script-")
        .tempdir_in(&base)
        .map_err(|error| format!("creating the script folder failed: {error}"))?;
    make_private(folder.path())?;

    Ok(folder)
}

/// Makes `folder` `0700` on Unix.
fn make_private(folder: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("securing the script folder failed: {error}"))?;
    }

    #[cfg(not(unix))]
    let _ = folder;

    Ok(())
}

/// The UTF-8 byte-order mark.
const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Writes `body` to the new file `path` (`0600` on Unix), after a [`BOM`] when `bom`.
fn write_private(path: &Path, bom: bool, body: &str) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);

    let prefix = if bom { BOM } else { &[] };

    options
        .open(path)
        .and_then(|mut file| {
            file.write_all(prefix)?;
            file.write_all(body.as_bytes())
        })
        .map_err(|error| format!("writing the script file failed: {error}"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(body: &str, timeout: Duration) -> Finished {
        let home = tempfile::tempdir().expect("temp");
        let env = Env::for_tests(home.path()).with_var("PATH", std::env::var("PATH").unwrap_or_default());

        run(
            &env,
            &Run {
                interpreters: &["sh"],
                file_name: "script.sh",
                body,
                bom: false,
                args: &["a b".to_owned(), "$(echo no)".to_owned()],
                stdin: "piped\n",
                cwd: home.path(),
                timeout,
                stdout_max: 16,
                stderr_max: 8,
            },
        )
        .expect("sh runs")
    }

    #[test]
    fn arguments_and_stdin_reach_the_script_as_they_are() {
        let finished = sh("printf '%s|' \"$1\" \"$2\"; cat", Duration::from_secs(10));

        assert_eq!(finished.exit_code, Some(0));
        assert_eq!(finished.stdout.text, "a b|$(echo no)|p");
        assert!(finished.stdout.truncated);
        assert!(!finished.timed_out);
    }

    #[test]
    fn exit_codes_and_stderr_are_reported() {
        let finished = sh("echo oops >&2; exit 3", Duration::from_secs(10));

        assert_eq!(finished.exit_code, Some(3));
        assert_eq!(
            finished.stderr,
            Output {
                text: "oops\n".to_owned(),
                truncated: false
            }
        );
    }

    #[test]
    fn a_script_that_exits_ends_the_run_though_its_background_child_holds_the_output() {
        let started = Instant::now();
        let finished = sh("sleep 30 &\necho done", Duration::from_secs(20));

        assert_eq!(finished.exit_code, Some(0));
        assert!(!finished.timed_out);
        assert_eq!(finished.stdout.text, "done\n");
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    #[test]
    fn a_secret_crossing_the_cap_is_masked_before_the_cut() {
        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4).get(..36).expect("36"));
        let body = format!("printf 'xxxxxxxxx {token}'");
        let finished = sh(&body, Duration::from_secs(10));

        assert!(finished.stdout.truncated);
        assert!(finished.stdout.text.len() <= 16, "{:?}", finished.stdout.text);
        assert!(
            finished.stdout.text.starts_with("xxxxxxxxx "),
            "{:?}",
            finished.stdout.text
        );
        assert!(!finished.stdout.text.contains("ghp_a"), "{:?}", finished.stdout.text);
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        let finished = sh("printf 'aaaaaaaaaaaaaaażżż'", Duration::from_secs(10));

        assert!(finished.stdout.truncated);
        assert_eq!(finished.stdout.text, "aaaaaaaaaaaaaaa");
    }

    #[test]
    fn the_script_file_is_kept_in_a_private_folder_next_to_the_database() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().expect("temp");
        let env = Env::for_tests(home.path()).with_var("PATH", std::env::var("PATH").unwrap_or_default());
        let request = Run {
            interpreters: &["sh"],
            file_name: "script.sh",
            body: "ls -ld \"$(dirname \"$0\")\" | cut -c1-10\nprintf '%s' \"$0\"",
            bom: false,
            args: &[],
            stdin: "",
            cwd: home.path(),
            timeout: Duration::from_secs(10),
            stdout_max: 4096,
            stderr_max: 4096,
        };

        let finished = run(&env, &request).expect("sh runs");
        let (mode, file) = finished.stdout.text.split_once('\n').expect("two lines");
        let run_folder = crate::location::data_dir(&env)
            .expect("data dir")
            .join("harness-memory/run");

        assert_eq!(mode, "drwx------");
        assert_eq!(
            Path::new(file).parent().and_then(Path::parent),
            Some(run_folder.as_path())
        );
        assert_eq!(
            std::fs::metadata(&run_folder).expect("run").permissions().mode() & 0o777,
            0o700
        );
        assert!(!Path::new(file).exists(), "the script folder is removed after the run");
    }

    #[test]
    fn a_missing_interpreter_is_named() {
        let home = tempfile::tempdir().expect("temp");
        let env = Env::for_tests(home.path()).with_var("PATH", home.path().to_string_lossy());
        let names = ["no-such-interpreter-3", "no-such-interpreter"];
        let request = Run {
            interpreters: &names,
            file_name: "x",
            body: "x",
            bom: false,
            args: &[],
            stdin: "",
            cwd: home.path(),
            timeout: Duration::from_secs(1),
            stdout_max: 1,
            stderr_max: 1,
        };

        let error = run(&env, &request).expect_err("not installed");
        assert!(
            error.starts_with("no-such-interpreter-3 (or no-such-interpreter) was not found on PATH"),
            "{error}"
        );
    }
}
