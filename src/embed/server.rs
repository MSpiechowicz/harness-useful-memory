//! The socket the embedder listens on, and the lock that keeps it to one embedder per computer.
//!
//! On Unix the socket is `<data>/harness-memory/embedder.sock` (`0600`). When that path is longer
//! than the 100 bytes a socket path may have, it is `$XDG_RUNTIME_DIR/harness-memory-<hash>.sock`
//! (a folder of this user, `0700`) or else `<temp>/harness-memory-<uid>/<hash>.sock`, in a folder
//! this user owns that no one else may write to: a folder of another user at that path is an
//! error, never used. On Windows it is a named pipe `harness-memory-embedder-<hash>-<secret>`,
//! open to this user and SYSTEM only. The secret is new at every start of the embedder and is kept
//! with its process id in a private file of the data folder (see [`super::pipe`]), so another user
//! can neither guess the name to squat on it nor make a client talk to a pipe of theirs. A request is one JSON line of at most 64 KiB, a
//! connection carries one request, and connections are served on their own threads (the model
//! serialises the inference itself).

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use interprocess::local_socket::traits::{ListenerExt as _, Stream as _};
use interprocess::local_socket::{ListenerOptions, Stream};
use sha2::{Digest, Sha256};

use super::protocol::{MAX_REQUEST, Request, error_reply};
use super::{LOCK_FILE, Service};

/// The longest socket path `sockaddr_un` holds on the platforms we run on (macOS has 104 bytes,
/// Linux 108).
#[cfg(unix)]
const SOCKET_PATH_MAX: usize = 100;

/// How many connections are served at once.
const CONNECTIONS_MAX: usize = 32;

/// How many times, and over how long, [`Lock::take`] tries before it reports the lock busy.
const LOCK_TRIES: u32 = 6;
const LOCK_PATIENCE: Duration = Duration::from_millis(300);

/// How long a client may take to send its request and read the reply.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the embedder listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    path: PathBuf,
    /// The folder of the socket when it is not the data folder or `$XDG_RUNTIME_DIR` (the one in
    /// the temporary folder, which this user creates and must own).
    #[cfg(unix)]
    own_folder: Option<PathBuf>,
    /// The data folder, where the pipe's secret and the embedder's process id are kept.
    #[cfg(windows)]
    root: PathBuf,
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl Endpoint {
    /// The endpoint for the data folder `root`. `temp` is where a socket goes when the data
    /// folder's path is too long (after `$XDG_RUNTIME_DIR`, when it is usable).
    #[cfg(unix)]
    pub fn of(root: &Path, temp: &Path) -> Self {
        Self::placed(root, temp, runtime_folder().as_deref())
    }

    /// [`Endpoint::of`] with the runtime folder given (`None` when there is none to use).
    #[cfg(unix)]
    pub(crate) fn placed(root: &Path, temp: &Path, runtime: Option<&Path>) -> Self {
        let near = root.join("embedder.sock");

        if near.as_os_str().len() <= SOCKET_PATH_MAX {
            return Self {
                path: near,
                own_folder: None,
            };
        }

        let hash = digest(root.to_string_lossy().as_bytes());

        if let Some(runtime) = runtime {
            let path = runtime.join(format!("harness-memory-{hash}.sock"));

            if path.as_os_str().len() <= SOCKET_PATH_MAX {
                return Self { path, own_folder: None };
            }
        }

        let folder = temp.join(format!("harness-memory-{}", nix::unistd::getuid().as_raw()));

        Self {
            path: folder.join(format!("{hash}.sock")),
            own_folder: Some(folder),
        }
    }

    /// The endpoint for the data folder `root`.
    #[cfg(windows)]
    pub fn of(root: &Path, _temp: &Path) -> Self {
        let user = std::env::var("USERNAME").unwrap_or_default();
        let hash = digest(format!("{user}\n{}", root.display()).as_bytes());

        Self {
            path: PathBuf::from(format!("harness-memory-embedder-{hash}")),
            root: root.to_path_buf(),
        }
    }

    /// The socket file (on Windows, the pipe's name without its secret).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Makes the endpoint ready for [`Endpoint::name`] on the serving side: on Windows a new
    /// secret for the pipe's name, recorded with this process's id (nothing on Unix). Clients read
    /// it from there.
    pub(crate) fn publish(&self) -> std::io::Result<()> {
        #[cfg(windows)]
        super::pipe::publish(&self.root)?;

        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn name(&self) -> std::io::Result<interprocess::local_socket::Name<'_>> {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        self.path.as_os_str().to_fs_name::<GenericFilePath>()
    }

    /// The pipe's name, with the secret of the embedder that published it.
    #[cfg(windows)]
    pub(crate) fn name(&self) -> std::io::Result<interprocess::local_socket::Name<'_>> {
        let published = super::pipe::read(&self.root)?;
        self.name_of(&published)
    }

    #[cfg(windows)]
    fn name_of(&self, published: &super::pipe::Published) -> std::io::Result<interprocess::local_socket::Name<'_>> {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};

        format!("{}-{}", self.path.display(), published.secret).to_ns_name::<GenericNamespaced>()
    }

    /// Connects to the embedder; where the operating system can, the streams time out after
    /// `timeout`. Windows named pipes cannot (the call is `Unsupported` there), so a caller that
    /// must not wait longer enforces its own deadline (see [`super::client`]).
    ///
    /// The server must be this user's: on Unix the socket file (and the folder made for it) is
    /// checked first, on Windows the process behind the pipe must be the embedder that published
    /// the pipe's secret.
    pub fn connect(&self, timeout: Duration) -> std::io::Result<Stream> {
        #[cfg(unix)]
        {
            if let Some(folder) = &self.own_folder {
                check_own_folder(folder)?;
            }

            check_socket_owner(&self.path)?;
        }

        #[cfg(unix)]
        let stream = Stream::connect(self.name()?)?;

        #[cfg(windows)]
        let stream = {
            let published = super::pipe::read(&self.root)?;
            let stream = Stream::connect(self.name_of(&published)?)?;
            super::pipe::check_server(&stream, &published)?;
            stream
        };

        without_unsupported(stream.set_recv_timeout(Some(timeout)))?;
        without_unsupported(stream.set_send_timeout(Some(timeout)))?;
        Ok(stream)
    }
}

/// `$XDG_RUNTIME_DIR` when it is usable (see [`usable_runtime`]).
#[cfg(unix)]
fn runtime_folder() -> Option<PathBuf> {
    usable_runtime(PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?))
}

/// `folder` when it is an absolute path to a folder (not a link) of this user that only this user
/// can enter.
#[cfg(unix)]
pub(crate) fn usable_runtime(folder: PathBuf) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(&folder).ok()?;

    let usable = folder.is_absolute()
        && metadata.file_type().is_dir()
        && metadata.uid() == nix::unistd::getuid().as_raw()
        && metadata.mode() & 0o077 == 0;

    usable.then_some(folder)
}

/// Creates the folder of the socket (`0700`) when it is missing, and fails unless it is a real
/// folder of this user that only this user can enter, so a stranger's folder or a link at that
/// path is never used (and never changed).
#[cfg(unix)]
fn secure_own_folder(folder: &Path) -> std::io::Result<()> {
    crate::support::private::private_leaf_folder(folder, "socket folder")
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::PermissionDenied, message))?;

    check_own_folder(folder)
}

#[cfg(unix)]
fn check_own_folder(folder: &Path) -> std::io::Result<()> {
    crate::support::private::check_folders(&[(folder, "socket folder")])
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::PermissionDenied, message))
}

/// `result`, except that a stream without timeouts (`Unsupported`) is not a failure.
fn without_unsupported(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => Ok(()),
        other => other,
    }
}

/// Refuses a socket file that is a link or belongs to another user: the fallback path in a shared
/// temporary folder could otherwise lead a query to a stranger's process.
#[cfg(unix)]
fn check_socket_owner(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let metadata = std::fs::symlink_metadata(path)?;

    if !metadata.file_type().is_socket() || metadata.uid() != nix::unistd::getuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "the socket is not this user's",
        ));
    }

    Ok(())
}

/// The lock that says an embedder runs. Dropping it (or the process ending) releases it.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

/// Why the lock was not taken.
#[derive(Debug)]
pub enum LockError {
    /// Another embedder holds it.
    Busy,
    Failed(String),
}

impl Lock {
    /// Takes the lock of the data folder `root`, creating the folder and the file.
    pub fn take(root: &Path) -> Result<Self, LockError> {
        crate::support::private::private_folder(root, "data folder").map_err(LockError::Failed)?;

        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);

        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);

        let file = options
            .open(root.join(LOCK_FILE))
            .map_err(|error| LockError::Failed(format!("cannot open the lock file: {}", error.kind())))?;

        // A `status` probe holds a shared lock for an instant ([`Lock::is_held`]): a few tries
        // over [`LOCK_PATIENCE`] tell it from an embedder that runs.
        for attempt in 0..LOCK_TRIES {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(LockError::Failed(format!("cannot lock: {}", error.kind())));
                }
            }

            if attempt + 1 < LOCK_TRIES {
                std::thread::sleep(LOCK_PATIENCE / LOCK_TRIES);
            }
        }

        Err(LockError::Busy)
    }

    /// Whether an embedder holds the lock of `root` (without creating anything).
    pub fn is_held(root: &Path) -> bool {
        let Ok(file) = File::open(root.join(LOCK_FILE)) else {
            return false;
        };

        matches!(file.try_lock_shared(), Err(std::fs::TryLockError::WouldBlock))
    }
}

/// A listening embedder socket.
pub struct Server {
    endpoint: Endpoint,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Starts serving `service` on `endpoint`. The caller holds the [`Lock`]: a stale socket file
    /// from an embedder that was killed is removed here, and nothing else is.
    pub fn start(endpoint: Endpoint, service: Arc<Service>) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            if let Some(folder) = &endpoint.own_folder {
                secure_own_folder(folder)?;
            }

            remove_stale_socket(endpoint.path())?;
        }

        endpoint.publish()?;

        let listener = match listen(&endpoint) {
            Ok(listener) => listener,
            Err(error) => {
                #[cfg(windows)]
                super::pipe::withdraw(&endpoint.root);

                return Err(error);
            }
        };

        let stopping = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stopping);
        let active = Arc::new(AtomicUsize::new(0));

        let thread = std::thread::spawn(move || {
            for connection in listener.incoming() {
                if flag.load(Ordering::SeqCst) {
                    return;
                }

                let Ok(connection) = connection else {
                    continue;
                };

                let service = Arc::clone(&service);
                let active = Arc::clone(&active);

                if active.fetch_add(1, Ordering::SeqCst) >= CONNECTIONS_MAX {
                    active.fetch_sub(1, Ordering::SeqCst);
                    let _ = reply(connection, &error_reply("the embedder is busy"));
                    continue;
                }

                std::thread::spawn(move || {
                    serve(connection, &service);
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });

        Ok(Self {
            endpoint,
            stopping,
            thread: Some(thread),
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Stops accepting and removes the socket file.
    pub fn shutdown(mut self) {
        self.stopping.store(true, Ordering::SeqCst);

        // The accept call returns for a connection; this one is it.
        let _ = self.endpoint.connect(Duration::from_millis(200));

        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }

        #[cfg(unix)]
        let _ = std::fs::remove_file(self.endpoint.path());

        #[cfg(windows)]
        super::pipe::withdraw(&self.endpoint.root);
    }
}

/// Starts listening at `endpoint`: the socket is `0600` on Unix, the pipe open to this user and
/// SYSTEM only on Windows.
#[cfg(unix)]
fn listen(endpoint: &Endpoint) -> std::io::Result<interprocess::local_socket::Listener> {
    use interprocess::os::unix::local_socket::ListenerOptionsExt;
    use std::os::unix::fs::PermissionsExt;

    match ListenerOptions::new().name(endpoint.name()?).mode(0o600).create_sync() {
        // macOS cannot set a socket's mode before `bind()`. The socket's folder is already private
        // to this user (the data folder or a 0700 folder of its own), so the mode is set right after.
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
            let listener = ListenerOptions::new().name(endpoint.name()?).create_sync()?;
            std::fs::set_permissions(endpoint.path(), std::fs::Permissions::from_mode(0o600))?;

            Ok(listener)
        }
        result => result,
    }
}

/// Starts listening at `endpoint`: the socket is `0600` on Unix, the pipe open to this user and
/// SYSTEM only on Windows.
#[cfg(windows)]
fn listen(endpoint: &Endpoint) -> std::io::Result<interprocess::local_socket::Listener> {
    use interprocess::os::windows::local_socket::ListenerOptionsExt;

    ListenerOptions::new()
        .name(endpoint.name()?)
        .security_descriptor(super::pipe::only_this_user()?)
        .create_sync()
}

/// Removes the socket file left by an embedder that was killed. Anything that is not a socket is
/// left alone and makes the start fail.
#[cfg(unix)]
fn remove_stale_socket(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::FileTypeExt;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(path),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "the socket path is taken by another file",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Reads one request from `connection`, answers it, and closes the connection.
fn serve(connection: Stream, service: &Service) {
    let _ = connection.set_recv_timeout(Some(IO_TIMEOUT));
    let _ = connection.set_send_timeout(Some(IO_TIMEOUT));

    let mut reader = BufReader::new(&connection).take(MAX_REQUEST as u64 + 1);
    let mut line = String::new();

    let mut warm = false;

    let answer = match reader.read_line(&mut line) {
        Ok(0) => return,
        Ok(read) if read > MAX_REQUEST => error_reply("the request is too large"),
        Ok(_) => match Request::parse(line.trim_end()) {
            Ok(request) => {
                warm = request == Request::Warm;
                service.handle(request)
            }
            Err(message) => error_reply(&message),
        },
        Err(_) => return,
    };

    let _ = reply(connection, &answer);

    // After the reply: the caller does not wait for the model, only this thread does.
    if warm {
        service.warm();
    }
}

fn reply(mut connection: Stream, answer: &serde_json::Value) -> std::io::Result<()> {
    writeln!(connection, "{answer}")?;
    connection.flush()
}
