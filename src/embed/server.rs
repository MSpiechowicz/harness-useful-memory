//! The socket the embedder listens on, and the lock that keeps it to one embedder per computer.
//!
//! On Unix the socket is `<data>/harness-memory/embedder.sock` (`0600`; when that path is longer
//! than the 100 bytes a socket path may have, `$TMPDIR/harness-memory-<hash>.sock`), on Windows a
//! named pipe `harness-memory-embedder-<hash>`. A request is one JSON line of at most 64 KiB, a
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
const SOCKET_PATH_MAX: usize = 100;

/// How many connections are served at once.
const CONNECTIONS_MAX: usize = 32;

/// How long a client may take to send its request and read the reply.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the embedder listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    path: PathBuf,
}

impl Endpoint {
    /// The endpoint for the data folder `root`. `temp` is where a socket goes when the data
    /// folder's path is too long.
    pub fn of(root: &Path, temp: &Path) -> Self {
        let digest = |bytes: &[u8]| -> String {
            Sha256::digest(bytes)
                .iter()
                .take(8)
                .map(|byte| format!("{byte:02x}"))
                .collect()
        };

        if cfg!(windows) {
            let user = std::env::var("USERNAME").unwrap_or_default();
            let hash = digest(format!("{user}\n{}", root.display()).as_bytes());
            return Self {
                path: PathBuf::from(format!("harness-memory-embedder-{hash}")),
            };
        }

        let path = root.join("embedder.sock");

        if path.as_os_str().len() <= SOCKET_PATH_MAX {
            return Self { path };
        }

        let hash = digest(root.to_string_lossy().as_bytes());
        Self {
            path: temp.join(format!("harness-memory-{hash}.sock")),
        }
    }

    /// The socket file (on Windows, the pipe name).
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(unix)]
    fn name(&self) -> std::io::Result<interprocess::local_socket::Name<'_>> {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        self.path.as_os_str().to_fs_name::<GenericFilePath>()
    }

    #[cfg(windows)]
    fn name(&self) -> std::io::Result<interprocess::local_socket::Name<'_>> {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        self.path.as_os_str().to_ns_name::<GenericNamespaced>()
    }

    /// Connects to the embedder; the streams time out after `timeout`.
    pub fn connect(&self, timeout: Duration) -> std::io::Result<Stream> {
        #[cfg(unix)]
        check_socket_owner(&self.path)?;

        let stream = Stream::connect(self.name()?)?;
        stream.set_recv_timeout(Some(timeout))?;
        stream.set_send_timeout(Some(timeout))?;
        Ok(stream)
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

        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(LockError::Busy),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(LockError::Failed(format!("cannot lock: {}", error.kind())))
            }
        }
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
        remove_stale_socket(endpoint.path())?;

        let mut options = ListenerOptions::new().name(endpoint.name()?);

        #[cfg(unix)]
        {
            use interprocess::os::unix::local_socket::ListenerOptionsExt;
            options = options.mode(0o600);
        }

        let listener = options.create_sync()?;
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
    }
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

    let answer = match reader.read_line(&mut line) {
        Ok(0) => return,
        Ok(read) if read > MAX_REQUEST => error_reply("the request is too large"),
        Ok(_) => match Request::parse(line.trim_end()) {
            Ok(request) => service.handle(request),
            Err(message) => error_reply(&message),
        },
        Err(_) => return,
    };

    let _ = reply(connection, &answer);
}

fn reply(mut connection: Stream, answer: &serde_json::Value) -> std::io::Result<()> {
    writeln!(connection, "{answer}")?;
    connection.flush()
}
