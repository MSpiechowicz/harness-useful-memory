//! A TCP proxy in front of a team database whose network can be cut: the outages of the sync
//! suites are real ones, with connections that die in the middle of a call and a port nobody
//! listens on, and need no Docker from inside a test.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// How often the accepting thread looks for a new connection.
const POLL: Duration = Duration::from_millis(2);

/// How long [`Proxy::restore`] tries to take its port back.
const REBIND_WITHIN: Duration = Duration::from_secs(10);

/// What the accepting thread, the forwarding threads and the test share.
struct Shared {
    /// The listening socket; none while the network is cut, so connecting is refused.
    listener: Mutex<Option<TcpListener>>,
    /// Both ends of every forwarded connection, to be shut down by a cut.
    open: Mutex<Vec<TcpStream>>,
    accepted: AtomicUsize,
    stopped: AtomicBool,
}

/// Listens on 127.0.0.1 and forwards every connection to the real database.
pub struct Proxy {
    address: SocketAddr,
    shared: Arc<Shared>,
}

impl Proxy {
    /// A proxy on a free port that forwards to `upstream` (`host:port`).
    pub fn to(upstream: &str) -> Self {
        let upstream = resolve(upstream);
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind the proxy");
        listener.set_nonblocking(true).expect("non-blocking");
        let address = listener.local_addr().expect("the proxy's address");

        let shared = Arc::new(Shared {
            listener: Mutex::new(Some(listener)),
            open: Mutex::new(Vec::new()),
            accepted: AtomicUsize::new(0),
            stopped: AtomicBool::new(false),
        });

        let accepting = Arc::clone(&shared);
        thread::spawn(move || accept_loop(&accepting, &upstream));

        Self { address, shared }
    }

    /// The proxy's `host:port`.
    pub fn authority(&self) -> String {
        self.address.to_string()
    }

    /// Drops every live connection and refuses new ones until [`Self::restore`].
    pub fn cut(&self) {
        // The listener goes first, so nothing connects between the two steps.
        drop(lock(&self.shared.listener).take());

        for stream in lock(&self.shared.open).drain(..) {
            let _closed = stream.shutdown(Shutdown::Both);
        }
    }

    /// Accepts connections again, on the port it had.
    pub fn restore(&self) {
        let mut listener = lock(&self.shared.listener);

        if listener.is_some() {
            return;
        }

        let until = Instant::now() + REBIND_WITHIN;

        loop {
            match TcpListener::bind(self.address) {
                Ok(bound) => {
                    bound.set_nonblocking(true).expect("non-blocking");
                    *listener = Some(bound);
                    return;
                }
                Err(error) if Instant::now() >= until => panic!("the proxy cannot take its port back: {error}"),
                Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    /// How many connections the database was asked for through the proxy so far.
    pub fn connections(&self) -> usize {
        self.shared.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        self.cut();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The addresses `authority` names (`localhost` may be IPv4, IPv6 or both).
fn resolve(authority: &str) -> Vec<SocketAddr> {
    use std::net::ToSocketAddrs;

    authority.to_socket_addrs().expect("the database's address").collect()
}

/// A connection to the first of `upstream` that answers.
fn connect(upstream: &[SocketAddr]) -> Option<TcpStream> {
    upstream
        .iter()
        .find_map(|address| TcpStream::connect_timeout(address, Duration::from_secs(5)).ok())
}

fn accept_loop(shared: &Arc<Shared>, upstream: &[SocketAddr]) {
    while !shared.stopped.load(Ordering::SeqCst) {
        let accepted = match lock(&shared.listener).as_ref() {
            Some(listener) => listener.accept().ok(),
            None => None,
        };

        let Some((client, _)) = accepted else {
            thread::sleep(POLL);
            continue;
        };

        shared.accepted.fetch_add(1, Ordering::SeqCst);

        // A database that is down closes the connection at once, as an outage does.
        let Some(server) = connect(upstream) else {
            continue;
        };

        forward(shared, client, server);
    }
}

/// Copies both ways until one side ends, then ends the other.
fn forward(shared: &Arc<Shared>, client: TcpStream, server: TcpStream) {
    let _ = client.set_nonblocking(false);
    let _ = client.set_nodelay(true);
    let _ = server.set_nodelay(true);

    let (Ok(client_copy), Ok(server_copy)) = (client.try_clone(), server.try_clone()) else {
        return;
    };

    lock(&shared.open).extend([client_copy, server_copy]);

    // A cut that came between the accepting and the registering has drained the list already.
    if lock(&shared.listener).is_none() {
        let _ = client.shutdown(Shutdown::Both);
        let _ = server.shutdown(Shutdown::Both);
        return;
    }

    let (Ok(client_back), Ok(server_back)) = (client.try_clone(), server.try_clone()) else {
        return;
    };

    thread::spawn(move || pipe(client, server_back));
    thread::spawn(move || pipe(server, client_back));
}

/// Moves bytes from `from` to `to` until either ends, then shuts both.
fn pipe(mut from: TcpStream, mut to: TcpStream) {
    let mut buffer = [0_u8; 16 * 1024];

    let result: io::Result<()> = loop {
        match from.read(&mut buffer) {
            Ok(0) => break Ok(()),
            Ok(read) => {
                if let Err(error) = to.write_all(&buffer[..read]) {
                    break Err(error);
                }
            }
            Err(error) => break Err(error),
        }
    };

    drop(result);
    let _ = from.shutdown(Shutdown::Both);
    let _ = to.shutdown(Shutdown::Both);
}
