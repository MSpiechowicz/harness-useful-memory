//! The server's input, read on a thread of its own so that a running script can be stopped while
//! the server waits for it ([`Stop`]):
//!
//! - the client cancels the request (`notifications/cancelled` with its `requestId`): the script
//!   is killed and the request gets no answer, as the MCP spec asks;
//! - the input ends (the client closed it, the MCP way to shut a server down): the script running
//!   then is killed, and so is any script a request still waiting asks for;
//! - (on Unix) the server gets SIGTERM ([`stop_on_sigterm`]): the script is killed and the server
//!   stops serving.
//!
//! Lines reach the serving loop in the order they were read; a cancel that arrives before its
//! request started still stops it (the last [`MAX_EARLY_CANCELS`] are kept).

use std::io::{self, BufRead};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use serde_json::Value;

use super::protocol::{self, Line};
use crate::limits::LINE_MAX;
use crate::support::runner::Stop;

/// How many cancels of requests that have not started yet are kept.
const MAX_EARLY_CANCELS: usize = 64;

/// The method of a cancel.
const CANCELLED: &str = "notifications/cancelled";

/// The request running now and the cancels that came before their request started.
#[derive(Debug, Default)]
struct Requests {
    running: Option<Value>,
    early: Vec<Value>,
}

/// Tells the [`Stop`] of a server what its input says about the request running now.
#[derive(Debug)]
pub(super) struct Watch {
    stop: Stop,
    requests: Mutex<Requests>,
}

impl Watch {
    pub(super) fn new(stop: Stop) -> Arc<Self> {
        Arc::new(Self {
            stop,
            requests: Mutex::new(Requests::default()),
        })
    }

    fn requests(&self) -> std::sync::MutexGuard<'_, Requests> {
        self.requests.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Request `id` starts (`None`: a notification).
    pub(super) fn begin(&self, id: Option<&Value>) {
        let mut requests = self.requests();
        self.stop.begin();

        let early = id.and_then(|id| requests.early.iter().position(|cancelled| cancelled == id));

        if let Some(at) = early {
            requests.early.remove(at);
            self.stop.cancel();
        }

        requests.running = id.cloned();
    }

    /// The request that ran is done: true when the client cancelled it (it gets no answer).
    pub(super) fn finish(&self) -> bool {
        self.requests().running = None;
        self.stop.is_cancelled()
    }

    /// Looks at one line read: a cancel stops its request, now or when it starts.
    fn saw(&self, line: &[u8]) {
        if !line
            .windows(CANCELLED.len())
            .any(|window| window == CANCELLED.as_bytes())
        {
            return;
        }

        let Ok(message) = serde_json::from_slice::<Value>(line) else {
            return;
        };

        if message.get("method").and_then(Value::as_str) != Some(CANCELLED) || message.get("id").is_some() {
            return;
        }

        let Some(id) = message.pointer("/params/requestId") else {
            return;
        };

        let mut requests = self.requests();

        if requests.running.as_ref() == Some(id) {
            self.stop.cancel();
            return;
        }

        if requests.early.len() == MAX_EARLY_CANCELS {
            requests.early.remove(0);
        }

        requests.early.push(id.clone());
    }
}

/// Reads `reader` line by line on a thread of its own and sends each line to `lines`, until the
/// input ends (also sent), reading fails (sent too) or nobody takes the lines any more.
pub(super) fn spawn_reader<R: BufRead + Send + 'static>(
    mut reader: R,
    watch: Arc<Watch>,
    lines: Sender<io::Result<Line>>,
) {
    thread::spawn(move || {
        let mut buffer = Vec::new();

        loop {
            let line = protocol::read_line(&mut reader, &mut buffer, LINE_MAX);
            let more = matches!(line, Ok(Line::Text(_) | Line::TooLong));

            if let Ok(Line::Text(text)) = &line {
                watch.saw(text);
            }

            if !more {
                watch.stop.end();
            }

            if lines.send(line).is_err() || !more {
                return;
            }
        }
    });
}

/// Makes SIGTERM stop `stop` ([`Stop::terminate`]) instead of ending the process at once, so a
/// running script is killed and its file removed first. Blocks SIGTERM in the calling thread and
/// every thread it starts afterwards (scripts and git do not inherit that: the standard library
/// clears the signal mask of a child), and waits for it on a thread of its own. Call it first
/// thing, before anything starts a thread.
#[cfg(unix)]
pub fn stop_on_sigterm(stop: &Stop) {
    use nix::sys::signal::{SigSet, Signal};

    let mut set = SigSet::empty();
    set.add(Signal::SIGTERM);

    if set.thread_block().is_err() {
        return;
    }

    let stop = stop.clone();

    thread::spawn(move || {
        loop {
            match set.wait() {
                Ok(Signal::SIGTERM) => break,
                Ok(_) => {}
                Err(_) => return,
            }
        }

        stop.terminate();
    });
}

/// SIGTERM ends the process as usual (Windows has none).
#[cfg(not(unix))]
pub fn stop_on_sigterm(_stop: &Stop) {}
