//! What `harness-memory mcp` calls to ask the embedder, which may not run: every call fails fast
//! (the socket is missing or refuses) or within its timeout, and the caller falls back to keyword
//! results. A failure is never fatal and `upsert` and `forget` are safe to fire and ignore.
//!
//! The timeout of a call covers all of it (connect, request, reply) and is kept by waiting for
//! the exchange, which runs on a thread of its own, for that long: the operating system's socket
//! timeouts alone would not do (Windows named pipes have none). A thread that outlives its call
//! is abandoned, and ends when the embedder answers or closes the connection (on Unix at the
//! socket timeout).

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use super::protocol::{MAX_REQUEST, NoteInput, Pong, Reply, Request, Target, VectorHit};
use super::server::Endpoint;

/// How long a search may take: room for the model to load when a search is the first call after
/// the embedder unloaded it (about two seconds at worst), though [`EmbedderClient::warm`]
/// normally has it loaded already.
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(4);

/// How long an upsert or a forget may take.
pub const WRITE_TIMEOUT: Duration = Duration::from_millis(200);

/// How long a ping may take.
pub const PING_TIMEOUT: Duration = Duration::from_millis(500);

/// How long a warm request may take: the embedder answers it before it loads anything.
pub const WARM_TIMEOUT: Duration = Duration::from_millis(250);

/// The longest reply read: 200 hits at about 80 bytes each, with room to spare.
const MAX_REPLY: u64 = 256 * 1024;

/// Why a call to the embedder failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// The embedder is not running or did not answer in time.
    #[error("the embedder is not reachable ({0})")]
    Unreachable(String),
    /// The embedder answered with an error.
    #[error("the embedder refused: {0}")]
    Refused(String),
}

/// A client of the embedder of one data folder.
#[derive(Debug, Clone)]
pub struct EmbedderClient {
    endpoint: Endpoint,
}

impl EmbedderClient {
    /// A client for the embedder of the data folder `root` (`<data>/harness-memory`); `temp` is
    /// the temporary folder a long socket path falls back to.
    pub fn new(root: &Path, temp: &Path) -> Self {
        Self {
            endpoint: Endpoint::of(root, temp),
        }
    }

    pub fn from_endpoint(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }

    /// Whether the embedder answers searches, and with which model.
    pub fn ping(&self) -> Result<Pong, ClientError> {
        let reply = self.call(&Request::Ping, PING_TIMEOUT)?;

        Ok(Pong {
            model: reply.model,
            ready: reply.ready,
            pending: reply.pending,
        })
    }

    /// Asks the embedder to load its model now if it unloaded it, best effort: the answer comes
    /// at once and the load goes on in the embedder.
    pub fn warm(&self) -> Result<(), ClientError> {
        self.call(&Request::Warm, WARM_TIMEOUT).map(drop)
    }

    /// The notes of `targets` closest in meaning to `text`, closest first, with a cosine of at
    /// least `floor`; at most `limit`.
    pub fn search(
        &self,
        text: &str,
        targets: &[Target],
        limit: usize,
        floor: f32,
    ) -> Result<Vec<VectorHit>, ClientError> {
        self.search_with_model(text, targets, limit, floor)
            .map(|(_, hits)| hits)
    }

    /// [`EmbedderClient::search`], also saying which model answered.
    pub fn search_with_model(
        &self,
        text: &str,
        targets: &[Target],
        limit: usize,
        floor: f32,
    ) -> Result<(String, Vec<VectorHit>), ClientError> {
        let request = Request::Search {
            text: text.to_owned(),
            targets: targets.to_vec(),
            limit,
            floor,
        };

        let reply = self.call(&request, SEARCH_TIMEOUT)?;
        Ok((reply.model.unwrap_or_default(), reply.hits))
    }

    /// Tells the embedder to index `notes` of `store`. Notes are sent in as few requests as fit.
    pub fn upsert(&self, store: &str, notes: &[NoteInput]) -> Result<(), ClientError> {
        let mut batch: Vec<NoteInput> = Vec::new();

        for note in notes {
            batch.push(note.clone());

            if line_of(store, &batch) > MAX_REQUEST / 2 {
                self.send_upsert(store, std::mem::take(&mut batch))?;
            }
        }

        if !batch.is_empty() {
            self.send_upsert(store, batch)?;
        }

        Ok(())
    }

    fn send_upsert(&self, store: &str, notes: Vec<NoteInput>) -> Result<(), ClientError> {
        let request = Request::Upsert {
            store: store.to_owned(),
            notes,
        };

        self.call(&request, WRITE_TIMEOUT).map(drop)
    }

    /// Tells the embedder the notes `ids` of `store` are gone.
    pub fn forget(&self, store: &str, ids: &[i64]) -> Result<(), ClientError> {
        for chunk in ids.chunks(2000) {
            let request = Request::Forget {
                store: store.to_owned(),
                ids: chunk.to_vec(),
            };

            self.call(&request, WRITE_TIMEOUT)?;
        }

        Ok(())
    }

    fn call(&self, request: &Request, timeout: Duration) -> Result<Reply, ClientError> {
        let line = request.to_line();

        if line.len() >= MAX_REQUEST {
            return Err(ClientError::Refused("the request is too large".to_owned()));
        }

        let endpoint = self.endpoint.clone();

        let answer = within(timeout, move || exchange(&endpoint, &line, timeout))
            .ok_or_else(|| ClientError::Unreachable(std::io::ErrorKind::TimedOut.to_string()))?
            .map_err(|error| ClientError::Unreachable(error.kind().to_string()))?;

        let reply: Reply = serde_json::from_str(answer.trim_end())
            .map_err(|_| ClientError::Unreachable("an unreadable reply".to_owned()))?;

        if !reply.ok {
            return Err(ClientError::Refused(reply.error.unwrap_or_default()));
        }

        Ok(reply)
    }
}

/// Sends `line` to the embedder and reads its one line of reply.
fn exchange(endpoint: &Endpoint, line: &str, timeout: Duration) -> std::io::Result<String> {
    let mut stream = endpoint.connect(timeout)?;

    writeln!(stream, "{line}")?;
    stream.flush()?;

    let mut answer = String::new();
    BufReader::new(&stream).take(MAX_REPLY).read_line(&mut answer)?;

    Ok(answer)
}

/// The result of `work`, run on a thread of its own; `None` when it did not finish within
/// `timeout` (the thread is left to end by itself) or could not be started.
fn within<T: Send + 'static>(timeout: Duration, work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (sender, receiver) = mpsc::channel();

    std::thread::Builder::new()
        .name("embedder-call".to_owned())
        .spawn(move || {
            let _ = sender.send(work());
        })
        .ok()?;

    receiver.recv_timeout(timeout).ok()
}

fn line_of(store: &str, notes: &[NoteInput]) -> usize {
    Request::Upsert {
        store: store.to_owned(),
        notes: notes.to_vec(),
    }
    .to_line()
    .len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_that_outlasts_its_timeout_is_given_up_on_at_the_timeout() {
        let started = std::time::Instant::now();
        let finished = within(Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_secs(3));
            1
        });

        assert_eq!(finished, None);
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        assert_eq!(within(Duration::from_secs(5), || 7), Some(7));
    }
}
