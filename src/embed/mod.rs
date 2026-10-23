//! The embedder: one process per computer that holds the multilingual MiniLM, a cache of the note
//! vectors of this computer, and a local-socket server every `harness-memory mcp` asks for the
//! notes closest in meaning to a query (see `docs/plans/hybrid-search.md`).
//!
//! - [`manifest`] and [`model`]: the pinned model files and their download.
//! - [`engine`]: the model in memory, one note per inference call, dropped after ten idle minutes.
//! - [`cache`]: the vectors, `vectors.sqlite3`; only the active model's are ever compared.
//! - [`backfill`]: reads every store this computer is configured with, embeds new and changed
//!   notes and drops the deleted ones.
//! - [`update`]: a new model manifest is downloaded and every note re-embedded while the old model
//!   keeps answering, then the answer switches at once.
//! - [`protocol`], [`server`] and [`client`]: the socket, and what `mcp` calls.
//!
//! [`Service`] ties these together without threads or sockets of its own, so tests drive it with
//! a fake engine; `cli_embedder` runs it.

pub mod backfill;
pub mod cache;
pub mod client;
pub mod engine;
pub mod manifest;
pub mod model;
pub mod protocol;
pub mod server;
mod update;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

pub use backfill::{Source, content_hash, note_text, store_key_local, store_key_team};
pub use cache::Cache;
pub use client::EmbedderClient;
pub use engine::{Embed, Loader, Resident, TractLoader};
pub use manifest::{MODEL, Manifest};
pub use protocol::{Request, VectorHit};
pub use update::Step;

use protocol::{MAX_HITS, error_reply};

/// The file name of the vector cache in the data folder.
pub const CACHE_FILE: &str = "vectors.sqlite3";

/// The file name of the settings in the data folder.
pub const CONFIG_FILE: &str = "embedder.toml";

/// The file whose lock says an embedder runs.
pub const LOCK_FILE: &str = "embedder.lock";

/// Something the embedder reports on its standard output, one JSON line each.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Another embedder runs already.
    Busy,
    Download {
        received: u64,
        total: u64,
    },
    Verifying,
    Loading,
    Ready {
        model: String,
        dim: usize,
    },
    Backfill {
        store: String,
        done: usize,
        total: usize,
    },
    ModelUpdate {
        from: String,
        to: String,
    },
    Reembed {
        store: String,
        done: usize,
        total: usize,
    },
    Switched {
        model: String,
    },
    CleanupDone,
    Idle,
    Error {
        message: String,
    },
}

impl Event {
    /// The JSON line: `{"event":"ready","model":"...","dim":384}`.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Busy => json!({ "event": "busy" }),
            Self::Download { received, total } => json!({ "event": "download", "received": received, "total": total }),
            Self::Verifying => json!({ "event": "verifying" }),
            Self::Loading => json!({ "event": "loading" }),
            Self::Ready { model, dim } => json!({ "event": "ready", "model": model, "dim": dim }),
            Self::Backfill { store, done, total } => {
                json!({ "event": "backfill", "store": store, "done": done, "total": total })
            }
            Self::ModelUpdate { from, to } => json!({ "event": "model-update", "from": from, "to": to }),
            Self::Reembed { store, done, total } => {
                json!({ "event": "reembed", "store": store, "done": done, "total": total })
            }
            Self::Switched { model } => json!({ "event": "switched", "model": model }),
            Self::CleanupDone => json!({ "event": "cleanup-done" }),
            Self::Idle => json!({ "event": "idle" }),
            Self::Error { message } => json!({ "event": "error", "message": message }),
        }
    }
}

/// Where events go.
pub trait Events: Send + Sync {
    fn emit(&self, event: Event);
}

/// Events as JSON lines on standard output.
pub struct JsonLines;

impl Events for JsonLines {
    fn emit(&self, event: Event) {
        use std::io::Write;

        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{}", event.to_json());
        let _ = out.flush();
    }
}

/// Events thrown away.
pub struct Silent;

impl Events for Silent {
    fn emit(&self, _event: Event) {}
}

/// A flag that wakes whoever waits on it.
#[derive(Default)]
pub struct Stop {
    stopped: Mutex<bool>,
    wake: Condvar,
}

impl Stop {
    pub fn stop(&self) {
        *locked(&self.stopped) = true;
        self.wake.notify_all();
    }

    pub fn is_stopped(&self) -> bool {
        *locked(&self.stopped)
    }

    /// Waits up to `time`; true when stopped.
    pub fn wait(&self, time: Duration) -> bool {
        let guard = locked(&self.stopped);
        let (guard, _) = self
            .wake
            .wait_timeout_while(guard, time, |stopped| !*stopped)
            .unwrap_or_else(PoisonError::into_inner);

        *guard
    }

    /// Waits until stopped.
    pub fn wait_forever(&self) {
        let guard = locked(&self.stopped);
        drop(
            self.wake
                .wait_while(guard, |stopped| !*stopped)
                .unwrap_or_else(PoisonError::into_inner),
        );
    }
}

/// The lock of `mutex`, also when a thread panicked holding it (the embedder keeps serving).
pub(crate) fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How an embedder is set up. [`Options::new`] has the real timings.
#[derive(Clone)]
pub struct Options {
    /// `<data>/harness-memory`.
    pub root: PathBuf,
    pub manifest: Manifest,
    /// Replaces the base of the model URLs ([`manifest::BASE_URL_VAR`]).
    pub base_url: Option<String>,
    /// How long a loaded model may sit unused.
    pub idle: Duration,
    /// The pause between two backfill passes, and between two tries to load the model.
    pub poll: Duration,
    /// How often a backfill pass reads every note again to find the deleted ones.
    pub reconcile: Duration,
    /// The pause before a failed model update is tried again.
    pub retry: Duration,
}

impl Options {
    pub fn new(root: PathBuf, manifest: Manifest) -> Self {
        Self {
            root,
            manifest,
            base_url: None,
            idle: engine::IDLE,
            poll: Duration::from_secs(5 * 60),
            reconcile: Duration::from_secs(60 * 60),
            retry: Duration::from_secs(60 * 60),
        }
    }
}

/// The model that answers searches, and its engine.
pub(crate) struct Active {
    pub model: String,
    pub resident: Resident,
}

type Checkpoint = Box<dyn Fn(Step) -> bool + Send + Sync>;

/// The embedder without its threads and its socket. Lock order: `active`, then `cache`; a store
/// is locked on its own.
pub struct Service {
    pub(crate) opts: Options,
    pub(crate) events: Arc<dyn Events>,
    pub(crate) loader: Arc<dyn Loader>,
    pub(crate) active: Mutex<Option<Active>>,
    pub(crate) cache: Mutex<Cache>,
    pub(crate) sources: Vec<Mutex<Source>>,
    pub(crate) pending: Mutex<Option<String>>,
    pub(crate) stop: Stop,
    pub(crate) checkpoint: Mutex<Option<Checkpoint>>,
}

impl Service {
    /// Opens the vector cache under `opts.root`. No model is loaded and nothing is read yet.
    pub fn new(
        opts: Options,
        loader: Arc<dyn Loader>,
        events: Arc<dyn Events>,
        sources: Vec<Source>,
    ) -> Result<Self, String> {
        let cache = Cache::open(&opts.root.join(CACHE_FILE))?;

        Ok(Self {
            opts,
            events,
            loader,
            active: Mutex::new(None),
            cache: Mutex::new(cache),
            sources: sources.into_iter().map(Mutex::new).collect(),
            pending: Mutex::new(None),
            stop: Stop::default(),
            checkpoint: Mutex::new(None),
        })
    }

    /// The stop flag every loop of the service watches.
    pub fn stop(&self) -> &Stop {
        &self.stop
    }

    /// Runs `hook` at each step of a model update; when it returns false the update stops there,
    /// as if the process had been killed (for tests).
    pub fn set_checkpoint(&self, hook: impl Fn(Step) -> bool + Send + Sync + 'static) {
        *locked(&self.checkpoint) = Some(Box::new(hook));
    }

    pub fn clear_checkpoint(&self) {
        *locked(&self.checkpoint) = None;
    }

    /// The model that answers searches, once it is loaded.
    pub fn active_model(&self) -> Option<String> {
        locked(&self.active).as_ref().map(|active| active.model.clone())
    }

    /// The model being switched to, while a switch runs.
    pub fn pending_model(&self) -> Option<String> {
        locked(&self.pending).clone()
    }

    /// How many vectors of `model` the cache holds.
    pub fn vectors(&self, model: &str) -> usize {
        locked(&self.cache).count(model)
    }

    /// The models that have vectors in the cache.
    pub fn models_with_vectors(&self) -> Vec<String> {
        locked(&self.cache).models().unwrap_or_default()
    }

    /// Answers one request. The reply is the JSON line to send.
    pub fn handle(&self, request: Request) -> Value {
        match request {
            Request::Ping => {
                let model = self.active_model();
                json!({ "ok": true, "ready": model.is_some(), "model": model, "pending": self.pending_model() })
            }
            // The load itself follows the reply (see [`Service::warm`]).
            Request::Warm => json!({ "ok": true }),
            Request::Search {
                text,
                targets,
                limit,
                floor,
            } => self.search(&text, &targets, limit, floor),
            Request::Upsert { store, notes } => self.upsert(&store, &notes),
            Request::Forget { store, ids } => match locked(&self.cache).forget(&store, &ids) {
                Ok(()) => json!({ "ok": true }),
                Err(error) => error_reply(&error),
            },
        }
    }

    /// Loads the model when it was dropped for being idle, and starts its idle time again; waits
    /// for the load, so the caller of a `warm` request runs it after its reply. A search that
    /// comes meanwhile waits for the model like it would have loaded it.
    pub fn warm(&self) {
        if let Some(active) = locked(&self.active).as_mut() {
            let _ = active.resident.ensure();
        }
    }

    fn search(&self, text: &str, targets: &[protocol::Target], limit: usize, floor: f32) -> Value {
        let mut guard = locked(&self.active);

        let Some(active) = guard.as_mut() else {
            return error_reply("the model is not ready");
        };

        if text.trim().is_empty() {
            return json!({ "ok": true, "model": active.model, "hits": Vec::<VectorHit>::new() });
        }

        let query = match active.resident.embed(&clip(text)) {
            Ok(query) => query,
            Err(error) => return error_reply(&error),
        };

        // Held until the reply is built: a switch of the model waits for the `active` lock, so the
        // vectors compared are always those of the model that embedded the query.
        let hits = locked(&self.cache).search(&query, targets, limit.min(MAX_HITS), floor);
        json!({ "ok": true, "model": active.model, "hits": hits })
    }

    fn upsert(&self, store: &str, notes: &[protocol::NoteInput]) -> Value {
        let mut guard = locked(&self.active);

        let Some(active) = guard.as_mut() else {
            return error_reply("the model is not ready");
        };

        for note in notes {
            let text = note_text(&note.title, &note.body, &note.tags);
            let hash = content_hash(&text);

            if locked(&self.cache).hash_of(store, note.id, &active.model).as_deref() == Some(hash.as_str()) {
                continue;
            }

            let vector = match active.resident.embed(&text) {
                Ok(vector) => vector,
                Err(error) => return error_reply(&error),
            };

            let stored =
                locked(&self.cache).put(store, note.id, note.project.as_deref(), &hash, &active.model, &vector);

            if let Err(error) = stored {
                return error_reply(&error);
            }
        }

        json!({ "ok": true })
    }
}

/// The data folder's `embedder.toml` says embedding is on (a missing file or key means off).
pub fn enabled(root: &Path) -> bool {
    let Ok(Some(bytes)) = crate::support::private::read_private(&root.join(CONFIG_FILE), 4096) else {
        return false;
    };

    let Ok(text) = String::from_utf8(bytes) else {
        return false;
    };

    toml::from_str::<toml::Table>(&text)
        .ok()
        .and_then(|table| table.get("enabled").and_then(toml::Value::as_bool))
        .unwrap_or(false)
}

/// Turns embedding on or off in `embedder.toml`.
pub fn set_enabled(root: &Path, on: bool) -> Result<(), String> {
    crate::support::private::private_folder(root, "data folder")?;

    let text = format!("enabled = {on}\n");
    crate::support::private::write_atomic(
        root,
        &root.join(CONFIG_FILE),
        text.as_bytes(),
        "embedder",
        "embedder settings",
    )
}

/// `text` cut to what the tokenizer is given.
fn clip(text: &str) -> String {
    text.chars().take(engine::TEXT_MAX).collect()
}
