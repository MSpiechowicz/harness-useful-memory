//! Who may let a script run: never the agent through these tools. A version runs when this
//! machine trusts it for that script (saved here, or approved by a person here), or when it comes
//! from the team database and `trust_team_scripts` is on ([`approval_of`]).
//!
//! For any other version `script_run` asks Harness Desktop when it runs ([`ask`]): the desktop
//! touches `desktop.alive` next to the database every few seconds, the server writes a request
//! `requests/<id>.json` there (`0600` in a `0700` folder, written to a temporary file and renamed,
//! the id 128 bits from the operating system) and waits up to [`SCRIPT_APPROVAL_WAIT`] for the
//! desktop to write `requests/<id>.decision` holding `allow` or `deny` (read without following a
//! symbolic link, at most 16 bytes). On Unix the data folder and the requests folder must be
//! folders of this user that no one else may write to, or nothing is asked. Both files are
//! removed however the wait ends, and requests and answers older than [`STALE_AFTER`] (left by a
//! killed server) before each new request. Without a live desktop the run is refused at once, and
//! the reply tells the agent to ask the user. No reply about an unapproved version names its hash.
//!
//! The server reads no message while it waits: a client that stops waiting or hangs up meanwhile
//! changes nothing. Without an answer nothing runs; after `allow` the approved version still runs
//! with no one to read its reply, and the agent's next `script_run` runs it again.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use super::Server;
use super::calls::Reply;
use super::reply::now;
use super::script_blocks::{Approval, shown_text};
use crate::store::{PreviousVersion, Script, ScriptSummary, TrustOrigin, TrustStore, clean};
use crate::support::Env;
use crate::support::time::rfc3339;

/// How long `script_run` waits for the user's answer in Harness Desktop.
pub const SCRIPT_APPROVAL_WAIT: Duration = Duration::from_secs(45);

/// Environment variable shortening [`SCRIPT_APPROVAL_WAIT`], in milliseconds (for tests).
pub const APPROVAL_WAIT_VAR: &str = "HARNESS_MEMORY_APPROVAL_WAIT_MS";

/// Pause between looks for the answer.
const POLL: Duration = Duration::from_millis(250);

/// How fresh `desktop.alive` must be for the desktop to count as running.
const ALIVE_FOR: Duration = Duration::from_secs(15);

/// The file Harness Desktop keeps touching while it runs, next to the database.
pub const ALIVE_FILE: &str = "desktop.alive";

/// The folder next to the database that holds the run requests and their answers.
pub const REQUESTS_FOLDER: &str = "requests";

/// The largest request Harness Desktop reads.
const REQUEST_MAX: usize = 512 * 1024;

/// The longest answer file read.
const DECISION_MAX: u64 = 16;

/// How old a request or an answer left in the folder must be to be removed.
const STALE_AFTER: Duration = Duration::from_secs(120);

/// The longest client name a request shows.
const CLIENT_MAX: usize = 64;

/// The characters of standard input a request shows.
const STDIN_PREVIEW: usize = 2048;

/// How asking Harness Desktop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Answer {
    Allowed,
    Declined,
    TimedOut,
    /// Not asked: the desktop is not running, or (the reason) the request could not be made.
    Unasked(Option<String>),
}

/// How this machine may run version `sha256` of the script `summary` describes, kept in `place`
/// (its trust name, see [`crate::backend::Selection::scripts_key`]); `team_trusted` when the
/// script was read from the team database and `trust_team_scripts` is on. A version trusted as
/// another name, scope or repository ([`crate::store::TrustRecord::is_for`]) needs approval
/// again. A version that needs approval is remembered ([`TrustStore::note_unapproved`], as well
/// as it can be), so `script_save` never trusts its code.
pub(crate) fn approval_of(
    trust: &mut dyn TrustStore,
    team_trusted: bool,
    place: &str,
    summary: &ScriptSummary,
    sha256: &str,
) -> Result<Approval, String> {
    let record = trust
        .trust_of(place, summary.id, sha256)
        .map_err(|error| error.to_string())?
        .filter(|record| record.is_for(summary));

    let approval = match record.map(|record| record.origin) {
        Some(TrustOrigin::Saved) => Approval::Local,
        Some(TrustOrigin::Approved) => Approval::Approved,
        None if team_trusted => Approval::Team,
        None => Approval::Needed,
    };

    if approval == Approval::Needed {
        // A failed write leaves the copy check of `script_save` (and a local save fails too).
        let _ = trust.note_unapproved(sha256);
    }

    Ok(approval)
}

/// What a run would get besides the script: its arguments, standard input and working
/// directory, as a run request shows them.
pub(super) struct RunInput<'a> {
    pub args: &'a [String],
    pub stdin: &'a str,
    pub cwd: &'a Path,
}

/// Asks Harness Desktop whether `script`, shown as `summary` (its list object), may run with
/// `input`, and waits for the answer (see the module docs).
pub(super) fn ask(server: &mut Server, script: &Script, summary: Value, input: &RunInput) -> Answer {
    let env = server.projects.env().clone();
    let Some(folder) = data_folder(&env) else {
        return Answer::Unasked(None);
    };

    if !desktop_alive(&folder) {
        return Answer::Unasked(None);
    }

    let requests = folder.join(REQUESTS_FOLDER);

    if let Err(reason) = private_folder(&requests).and_then(|()| check_folders(&requests, &folder)) {
        return Answer::Unasked(Some(reason));
    }

    sweep(&requests);

    let place = server.place_key(server.scripts_place);
    let previous = match server.trust.previous(&place, script.summary.id) {
        Ok(previous) => previous,
        Err(error) => return Answer::Unasked(Some(error.to_string())),
    };

    let Some(id) = request_id() else {
        return Answer::Unasked(Some("no random request id could be made".to_owned()));
    };

    let client = server.client.as_deref().map(|client| shown_text(client, CLIENT_MAX));
    let request = request_json(&id, client.as_deref(), script, summary, input, previous.as_ref());
    let Ok(text) = serde_json::to_vec(&request) else {
        return Answer::Unasked(Some("the request could not be written".to_owned()));
    };

    if text.len() > REQUEST_MAX {
        return Answer::Unasked(Some(
            "the arguments are too large to show in Harness Desktop".to_owned(),
        ));
    }

    let pending = Pending {
        request: requests.join(format!("{id}.json")),
        decision: requests.join(format!("{id}.decision")),
    };

    if let Err(reason) = write_atomic(&requests, &pending.request, &text) {
        return Answer::Unasked(Some(reason));
    }

    wait_for(&pending.decision, wait_time(&env))
}

/// The refusal for `summary` after `answer`; `None` when the run was allowed.
pub(super) fn refusal(summary: &ScriptSummary, answer: &Answer) -> Option<Reply> {
    let ScriptSummary { id, name, .. } = summary;

    let (text, pending) = match answer {
        Answer::Allowed => return None,
        Answer::Declined => (format!("the user declined to run script {name}"), json!("declined")),
        Answer::TimedOut => (
            format!(
                "no answer from the user in Harness Desktop; ask the user to approve script {name} in Harness \
                 Desktop → Memory, then call script_run again"
            ),
            json!("timeout"),
        ),
        Answer::Unasked(reason) => {
            let mut text = format!(
                "script {name} is not approved on this machine (written by a teammate, changed, renamed or moved \
                 in the team database). Ask the user to review and approve it in Harness Desktop → Memory (or the \
                 user can run `harness-memory trust {id}` in a terminal), then call script_run again."
            );

            if let Some(reason) = reason {
                text.push_str(&format!(" (Harness Desktop could not be asked: {reason})"));
            }

            (text, json!(false))
        }
    };

    Some(Reply {
        text,
        is_error: true,
        structured: Some(json!({ "untrusted": true, "id": id, "name": name, "pending": pending })),
    })
}

/// The folder that holds the database, where Harness Desktop and the server meet.
fn data_folder(env: &Env) -> Option<PathBuf> {
    crate::database_path(env).and_then(|path| path.parent().map(Path::to_path_buf))
}

/// True when `desktop.alive` in `folder` was touched less than [`ALIVE_FOR`] ago (or is dated at
/// most that far ahead, for a clock set back).
fn desktop_alive(folder: &Path) -> bool {
    let Ok(touched) = std::fs::metadata(folder.join(ALIVE_FILE)).and_then(|metadata| metadata.modified()) else {
        return false;
    };

    match SystemTime::now().duration_since(touched) {
        Ok(age) => age < ALIVE_FOR,
        Err(ahead) => ahead.duration() < ALIVE_FOR,
    }
}

/// The request object Harness Desktop reads (camelCase, version 1).
fn request_json(
    id: &str,
    client: Option<&str>,
    script: &Script,
    summary: Value,
    input: &RunInput,
    previous: Option<&PreviousVersion>,
) -> Value {
    let previous = previous.map(|previous| {
        json!({
            "body": previous.body,
            "language": previous.language,
            "name": previous.name,
            "scope": previous.scope,
            "trustedAt": previous.trusted_at,
            "origin": previous.origin.as_str(),
        })
    });

    json!({
        "version": 1,
        "requestId": id,
        "createdAt": rfc3339(now()),
        "client": client.unwrap_or("unknown"),
        "script": summary,
        "body": script.body,
        "language": script.summary.language.as_str(),
        "args": input.args,
        "cwd": input.cwd.to_string_lossy(),
        "stdin": stdin_json(input.stdin),
        "previous": previous,
    })
}

/// The standard input a request shows: its first [`STDIN_PREVIEW`] characters without control or
/// invisible characters (newlines and tabs kept) and its size in bytes; `null` for none.
fn stdin_json(stdin: &str) -> Value {
    if stdin.is_empty() {
        return Value::Null;
    }

    let start: String = stdin.chars().take(STDIN_PREVIEW).collect();

    json!({ "preview": clean::block(&start), "bytes": stdin.len() })
}

/// 128 bits from the operating system's random source, in lowercase hex; `None` when it fails.
fn request_id() -> Option<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).ok()?;

    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// [`SCRIPT_APPROVAL_WAIT`], or the shorter wait [`APPROVAL_WAIT_VAR`] names.
fn wait_time(env: &Env) -> Duration {
    env.var(APPROVAL_WAIT_VAR)
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .map_or(SCRIPT_APPROVAL_WAIT, |wait| wait.min(SCRIPT_APPROVAL_WAIT))
}

/// Looks for the answer every [`POLL`] until `wait` has passed.
fn wait_for(decision: &Path, wait: Duration) -> Answer {
    let started = Instant::now();

    loop {
        if let Some(answer) = read_decision(decision) {
            return answer;
        }

        let left = wait.saturating_sub(started.elapsed());

        if left.is_zero() {
            return Answer::TimedOut;
        }

        std::thread::sleep(left.min(POLL));
    }
}

/// The answer in `path`: `allow` or `deny` (surrounding blanks ignored); `None` for no file yet, a
/// symbolic link, more than [`DECISION_MAX`] bytes, or anything else.
fn read_decision(path: &Path) -> Option<Answer> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);

    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, nix::libc::O_NOFOLLOW);

    let mut bytes = Vec::new();
    options
        .open(path)
        .ok()?
        .take(DECISION_MAX + 1)
        .read_to_end(&mut bytes)
        .ok()?;

    if bytes.len() > usize::try_from(DECISION_MAX).unwrap_or(usize::MAX) {
        return None;
    }

    match std::str::from_utf8(&bytes).ok()?.trim() {
        "allow" => Some(Answer::Allowed),
        "deny" => Some(Answer::Declined),
        _ => None,
    }
}

/// Creates `folder` when missing and makes it `0700` on Unix (failing for a folder another user
/// owns).
fn private_folder(folder: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);

    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);

    builder
        .create(folder)
        .map_err(|error| format!("creating the requests folder failed: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("securing the requests folder failed: {error}"))?;
    }

    Ok(())
}

/// Fails unless `requests` and the data folder above it are folders (not symbolic links) owned
/// by this user that no one else may write to, so no other user can read or swap a request.
#[cfg(unix)]
fn check_folders(requests: &Path, data: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    let user = nix::unistd::getuid().as_raw();

    for (folder, name) in [(data, "data folder"), (requests, "requests folder")] {
        let metadata =
            std::fs::symlink_metadata(folder).map_err(|error| format!("the {name} cannot be read: {error}"))?;

        if !metadata.file_type().is_dir() {
            return Err(format!("the {name} is not a folder"));
        }

        if metadata.uid() != user {
            return Err(format!("the {name} belongs to another user"));
        }

        if metadata.mode() & 0o022 != 0 {
            return Err(format!("other users may write to the {name}"));
        }
    }

    Ok(())
}

#[cfg(not(unix))]
fn check_folders(_requests: &Path, _data: &Path) -> Result<(), String> {
    Ok(())
}

/// Removes the requests, answers and half-written requests in `requests` older than
/// [`STALE_AFTER`], left by a server that was killed while it waited.
fn sweep(requests: &Path) {
    let Ok(entries) = std::fs::read_dir(requests) else {
        return;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let ours = name.ends_with(".json")
            || name.ends_with(".decision")
            || (name.starts_with(".request-") && name.ends_with(".tmp"));

        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age > STALE_AFTER);

        if ours && stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Writes `text` to `path` through a temporary file in `folder` (`0600` on Unix) renamed into
/// place, so the desktop never reads half a request.
fn write_atomic(folder: &Path, path: &Path, text: &[u8]) -> Result<(), String> {
    let failed = |error: std::io::Error| format!("writing the request failed: {error}");

    let mut file = tempfile::Builder::new()
        .prefix(".request-")
        .suffix(".tmp")
        .tempfile_in(folder)
        .map_err(failed)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(failed)?;
    }

    file.write_all(text).and_then(|()| file.flush()).map_err(failed)?;
    file.persist(path).map_err(|error| failed(error.error))?;

    Ok(())
}

/// The request and answer files of one run, removed when dropped.
struct Pending {
    request: PathBuf,
    decision: PathBuf,
}

impl Drop for Pending {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.request);
        let _ = std::fs::remove_file(&self.decision);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_128_bit_hex_and_differ() {
        let first = request_id().expect("random");
        let second = request_id().expect("random");

        assert_eq!(first.len(), 32);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert_ne!(first, second);
    }

    #[test]
    fn the_wait_can_only_be_shortened() {
        let env = Env::for_tests("/home/u");
        assert_eq!(wait_time(&env), SCRIPT_APPROVAL_WAIT);
        assert_eq!(
            wait_time(&env.clone().with_var(APPROVAL_WAIT_VAR, "300")),
            Duration::from_millis(300)
        );
        assert_eq!(
            wait_time(&env.clone().with_var(APPROVAL_WAIT_VAR, "600000")),
            SCRIPT_APPROVAL_WAIT
        );
        assert_eq!(
            wait_time(&env.with_var(APPROVAL_WAIT_VAR, "soon")),
            SCRIPT_APPROVAL_WAIT
        );
    }

    #[test]
    fn only_allow_or_deny_is_an_answer() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("x.decision");
        assert_eq!(read_decision(&path), None);

        for (text, answer) in [
            ("allow", Some(Answer::Allowed)),
            ("deny\n", Some(Answer::Declined)),
            ("yes", None),
            ("allowed", None),
            ("", None),
            ("allow           x", None),
            ("  allow\n", Some(Answer::Allowed)),
        ] {
            std::fs::write(&path, text).expect("write");
            assert_eq!(read_decision(&path), answer, "{text:?}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_linked_answer_is_not_read() {
        let dir = tempfile::tempdir().expect("temp");
        let real = dir.path().join("elsewhere");
        let link = dir.path().join("x.decision");
        std::fs::write(&real, "allow").expect("write");
        std::os::unix::fs::symlink(&real, &link).expect("link");

        assert_eq!(read_decision(&link), None);
    }

    #[test]
    #[cfg(unix)]
    fn requests_need_private_folders_of_this_user() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp");
        let data = dir.path().join("data");
        let requests = data.join(REQUESTS_FOLDER);
        std::fs::create_dir_all(&data).expect("mkdir");
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        private_folder(&requests).expect("created");
        assert_eq!(check_folders(&requests, &data), Ok(()));

        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        assert_eq!(
            check_folders(&requests, &data),
            Err("other users may write to the data folder".to_owned())
        );
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).expect("chmod");

        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("mkdir");
        std::fs::remove_dir(&requests).expect("rmdir");
        std::os::unix::fs::symlink(&elsewhere, &requests).expect("link");
        assert_eq!(
            check_folders(&requests, &data),
            Err("the requests folder is not a folder".to_owned())
        );
    }

    #[test]
    fn stale_requests_and_answers_are_swept() {
        let dir = tempfile::tempdir().expect("temp");
        let old = SystemTime::now() - Duration::from_secs(300);

        for name in ["old.json", "old.decision", ".request-x.tmp", "keep.txt", "new.json"] {
            std::fs::write(dir.path().join(name), "x").expect("write");
        }

        for name in ["old.json", "old.decision", ".request-x.tmp", "keep.txt"] {
            let file = std::fs::File::options()
                .write(true)
                .open(dir.path().join(name))
                .expect("open");
            file.set_modified(old).expect("mtime");
        }

        sweep(dir.path());

        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .expect("list")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["keep.txt", "new.json"]);
    }
}
