//! A stand-in for the embedder on a real local socket: it records every request and answers with
//! what the test scripts. Unix only (the socket is a file).
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use harness_memory::embed::server::Endpoint;
use serde_json::{Value, json};

type Answer = Box<dyn Fn(&Value) -> Value + Send + Sync>;

/// The embedder of the data folder given to [`FakeEmbedder::start`], until it is dropped.
pub struct FakeEmbedder {
    endpoint: Endpoint,
    requests: Arc<Mutex<Vec<Value>>>,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeEmbedder {
    /// Listens where the embedder of `root` would, answering each request with `answer(request)`.
    pub fn start(root: &Path, answer: impl Fn(&Value) -> Value + Send + Sync + 'static) -> Self {
        let endpoint = Endpoint::of(root, &std::env::temp_dir());
        let listener = UnixListener::bind(endpoint.path()).expect("listen on the embedder socket");

        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let (seen, flag) = (Arc::clone(&requests), Arc::clone(&stopping));
        let answer: Answer = Box::new(answer);

        let thread = std::thread::spawn(move || {
            for connection in listener.incoming().flatten() {
                if flag.load(Ordering::SeqCst) {
                    return;
                }

                let mut line = String::new();
                let mut reader = BufReader::new(&connection);

                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }

                let Ok(request) = serde_json::from_str::<Value>(line.trim_end()) else {
                    continue;
                };

                let reply = answer(&request);
                seen.lock().expect("requests").push(request);

                let mut connection = &connection;
                let _ = writeln!(connection, "{reply}");
                let _ = connection.flush();
            }
        });

        Self {
            endpoint,
            requests,
            stopping,
            thread: Some(thread),
        }
    }

    /// Every request received so far, oldest first.
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().expect("requests").clone()
    }

    /// The requests of operation `op` (`search`, `upsert`, `forget`).
    pub fn ops(&self, op: &str) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|request| request["op"] == op)
            .collect()
    }
}

impl Drop for FakeEmbedder {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);

        // The accept call returns for a connection; this one is it.
        let _ = std::os::unix::net::UnixStream::connect(self.endpoint.path());

        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }

        let _ = std::fs::remove_file(self.endpoint.path());
    }
}

/// An embedder that answers searches with `hits` (`store`, note id, cosine) of the stores asked
/// for and at least the floor asked for, and accepts every write.
pub fn answering(hits: &[(&str, i64, f64)]) -> impl Fn(&Value) -> Value + Send + Sync + 'static {
    let hits: Vec<(String, i64, f64)> = hits
        .iter()
        .map(|(store, id, cosine)| ((*store).to_owned(), *id, *cosine))
        .collect();

    move |request| match request["op"].as_str() {
        Some("search") => {
            let floor = request["floor"].as_f64().unwrap_or(0.0);
            let kept: Vec<Value> = hits
                .iter()
                .filter(|(_, _, cosine)| *cosine >= floor)
                .map(|(store, id, cosine)| json!({ "store": store, "id": id, "cosine": cosine }))
                .collect();

            json!({ "ok": true, "model": "fake@1", "hits": kept })
        }
        Some("ping") => json!({ "ok": true, "model": "fake@1", "ready": true, "pending": null }),
        _ => json!({ "ok": true }),
    }
}

/// An embedder that is running but has no model yet.
pub fn not_ready(request: &Value) -> Value {
    match request["op"].as_str() {
        Some("ping") => json!({ "ok": true, "model": null, "ready": false, "pending": null }),
        _ => json!({ "ok": false, "error": "the model is not ready" }),
    }
}
