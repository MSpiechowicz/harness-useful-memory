//! Keeps the vector cache in step with the stores this computer is configured with: the local
//! SQLite file and the team database, each read on its own (never through a split store, whose
//! note ids mean two things).
//!
//! A pass pages through a store's notes by `updated_at` from the saved cursor, embeds the notes
//! whose content hash differs from the cached one, and moves the cursor. The first pass after a
//! start, and one every hour, read every note and drop the vectors of the deleted ones. A store
//! that cannot be opened or read reports one `error` event and is tried again at the next pass.

use std::path::PathBuf;
use std::time::Instant;

use sha2::{Digest, Sha256};

use super::engine::TEXT_MAX;
use super::{Event, Service, locked};
use crate::store::{MemoryStore, NoteText, StoreError};

/// Notes read per page.
const PAGE: usize = 100;

/// The largest page, when more notes than that share one second.
const PAGE_MAX: usize = 100_000;

/// Notes between two progress events.
const PROGRESS_EVERY: usize = 25;

/// One physical store, read through its own connection.
pub struct Source {
    pub key: String,
    pub(crate) store: Box<dyn MemoryStore>,
    /// The database file a local store reads; the store is not opened while the file is missing.
    file: Option<PathBuf>,
    last_error: Option<String>,
}

impl Source {
    pub fn new(key: impl Into<String>, store: Box<dyn MemoryStore>) -> Self {
        Self {
            key: key.into(),
            store,
            file: None,
            last_error: None,
        }
    }

    /// Skips the store while `path` does not exist (the local database is created by the first
    /// server that runs, not by the embedder).
    pub fn when_file_exists(mut self, path: PathBuf) -> Self {
        self.file = Some(path);
        self
    }

    pub(crate) fn waiting(&self) -> bool {
        self.file.as_ref().is_some_and(|path| !path.exists())
    }
}

/// The store key of the local database file: `local:` and the first 16 hex digits of the SHA-256
/// of its canonical path.
pub fn store_key_local(path: &std::path::Path) -> String {
    let canonical = dunce::canonicalize(path)
        .or_else(|_| {
            let parent = path.parent().map(dunce::canonicalize).transpose()?;
            Ok::<_, std::io::Error>(match (parent, path.file_name()) {
                (Some(parent), Some(name)) => parent.join(name),
                _ => path.to_path_buf(),
            })
        })
        .unwrap_or_else(|_| path.to_path_buf());

    format!("local:{}", short_hash(canonical.to_string_lossy().as_bytes()))
}

/// The store key of a team database: its kind and the first 16 hex digits of the SHA-256 of its
/// identity (hosts and database name, without credentials; see
/// [`crate::backend::Backend::team_identity`]). `None` for the local backend.
pub fn store_key_team(backend: &crate::backend::Backend) -> Option<String> {
    let identity = backend.team_identity()?;
    let kind = identity.split("://").next().unwrap_or("team");
    Some(format!("{kind}:{}", short_hash(identity.as_bytes())))
}

fn short_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The text the model reads for a note: the title, the body and the tags, cut to what the
/// tokenizer is given.
pub fn note_text(title: &str, body: &str, tags: &[String]) -> String {
    let mut text = format!("{title}\n{body}");

    if !tags.is_empty() {
        text.push('\n');
        text.push_str(&tags.join(" "));
    }

    text.chars().take(TEXT_MAX).collect()
}

/// The hash of a note's text, which tells an edited note from one already embedded.
pub fn content_hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Why a pass ended before the store was read to the end.
pub(crate) enum Halt {
    /// The embedder is stopping.
    Stopped,
    /// The model that answers searches changed under the pass.
    Changed,
    Failed(String),
}

impl From<String> for Halt {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<StoreError> for Halt {
    fn from(error: StoreError) -> Self {
        Self::Failed(error.to_string())
    }
}

/// Which event a scan reports.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Report {
    Backfill,
    Reembed,
}

/// What a scan did.
#[derive(Default)]
pub(crate) struct Scanned {
    pub processed: usize,
    pub embedded: usize,
    /// The largest `updated_at` read.
    pub newest: i64,
}

/// Embeds a text; the error says why the scan must end.
pub(crate) type EmbedFn<'a> = dyn FnMut(&str) -> Result<Vec<f32>, Halt> + 'a;

impl Service {
    /// One backfill pass over every store. `full` reads every note, as the first pass does.
    pub fn backfill_pass(&self, full: bool) {
        let Some(model) = self.active_model() else {
            return;
        };

        if full && !self.sources.is_empty() {
            let keys: Vec<String> = self.sources.iter().map(|source| locked(source).key.clone()).collect();

            if let Err(error) = locked(&self.cache).keep_stores(&keys) {
                self.events.emit(Event::Error { message: error });
            }
        }

        for source in &self.sources {
            if self.stop.is_stopped() {
                return;
            }

            let mut source = locked(source);
            let outcome = self.sync(&mut source, full, &model);
            self.settle(&mut source, outcome);
        }
    }

    /// Runs passes until stopped: the first one reads every note, then one every poll interval,
    /// and a full one every reconcile interval.
    pub fn backfill_loop(&self) {
        let mut last_full: Option<Instant> = None;

        loop {
            let full = last_full.is_none_or(|at| at.elapsed() >= self.opts.reconcile);

            if full {
                last_full = Some(Instant::now());
            }

            self.backfill_pass(full);

            if self.stop.wait(self.opts.poll) {
                return;
            }
        }
    }

    /// Reports a failed pass once per distinct message, and forgets the message after a good one.
    fn settle(&self, source: &mut Source, outcome: Result<(), Halt>) {
        match outcome {
            Ok(()) => source.last_error = None,
            Err(Halt::Stopped | Halt::Changed) => {}
            Err(Halt::Failed(message)) => {
                if source.last_error.as_deref() != Some(message.as_str()) {
                    self.events.emit(Event::Error {
                        message: format!("{}: {message}", source.key),
                    });
                }

                source.last_error = Some(message);
            }
        }
    }

    /// Brings the vectors of the active `model` for one store up to date.
    fn sync(&self, source: &mut Source, full: bool, model: &str) -> Result<(), Halt> {
        if source.waiting() {
            return Ok(());
        }

        let key = source.key.clone();
        let start = if full {
            0
        } else {
            locked(&self.cache).cursor(&key).unwrap_or(0)
        };
        let whole = start == 0;
        let total = if whole { source.store.note_ids()?.len() } else { 0 };

        let mut embed = |text: &str| self.embed_with_active(model, text);
        let scanned = self.scan(source, Report::Backfill, model, start, total, &mut embed)?;

        locked(&self.cache).set_cursor(&key, scanned.newest.max(start))?;

        if whole {
            // After the scan, so a note saved while it ran is in the list and is not dropped.
            let present: std::collections::HashSet<i64> = source.store.note_ids()?.into_iter().collect();
            let cached = locked(&self.cache).ids(&key, model)?;
            let gone: Vec<i64> = cached.into_iter().filter(|id| !present.contains(id)).collect();

            if !gone.is_empty() {
                locked(&self.cache).forget(&key, &gone)?;
            }
        }

        if whole || scanned.embedded > 0 {
            self.events.emit(Event::Backfill {
                store: key,
                done: scanned.processed,
                total: if whole {
                    total.max(scanned.processed)
                } else {
                    scanned.processed
                },
            });
        }

        Ok(())
    }

    /// Embeds `text` with the active model, if `model` still is the active one.
    pub(crate) fn embed_with_active(&self, model: &str, text: &str) -> Result<Vec<f32>, Halt> {
        let mut guard = locked(&self.active);
        let active = guard.as_mut().ok_or(Halt::Changed)?;

        if active.model != model {
            return Err(Halt::Changed);
        }

        active.resident.embed(text).map_err(Halt::Failed)
    }

    /// Reads the notes of `source` from `start` on, page by page, and stores a vector under
    /// `model` for each one whose text changed. Notes that share a second are read again after a
    /// page ends inside it; the (second, id) of the last note handled skips them.
    pub(crate) fn scan(
        &self,
        source: &mut Source,
        report: Report,
        model: &str,
        start: i64,
        total: usize,
        embed: &mut EmbedFn<'_>,
    ) -> Result<Scanned, Halt> {
        let mut scanned = Scanned {
            newest: start,
            ..Scanned::default()
        };
        let mut cursor = start;
        let mut limit = PAGE;
        let mut last = (i64::MIN, i64::MIN);

        loop {
            self.check_stop()?;

            let page = source.store.notes_since(cursor, limit)?;
            let full_page = page.len() >= limit;
            let tail = page.last().map(|note| note.updated_at);

            for note in &page {
                self.check_stop()?;

                if (note.updated_at, note.id) <= last {
                    continue;
                }

                self.index_note(&source.key, note, model, embed, &mut scanned)?;
                last = (note.updated_at, note.id);
                scanned.processed += 1;
                scanned.newest = scanned.newest.max(note.updated_at);

                if scanned.embedded > 0 && scanned.processed.is_multiple_of(PROGRESS_EVERY) {
                    self.progress(report, &source.key, scanned.processed, total);
                }
            }

            match tail {
                Some(tail) if full_page && tail == cursor && limit < PAGE_MAX => limit = (limit * 2).min(PAGE_MAX),
                Some(tail) if full_page && tail != cursor => cursor = tail,
                _ => break,
            }
        }

        if report == Report::Reembed {
            self.progress(report, &source.key, scanned.processed, total.max(scanned.processed));
        }

        Ok(scanned)
    }

    fn index_note(
        &self,
        key: &str,
        note: &NoteText,
        model: &str,
        embed: &mut EmbedFn<'_>,
        scanned: &mut Scanned,
    ) -> Result<(), Halt> {
        let text = note_text(&note.title, &note.body, &note.tags);
        let hash = content_hash(&text);

        if locked(&self.cache).hash_of(key, note.id, model).as_deref() == Some(hash.as_str()) {
            return Ok(());
        }

        let vector = embed(&text)?;
        locked(&self.cache).put(key, note.id, note.project.as_deref(), &hash, model, &vector)?;
        scanned.embedded += 1;
        Ok(())
    }

    fn progress(&self, report: Report, store: &str, done: usize, total: usize) {
        let store = store.to_owned();
        let total = total.max(done);

        self.events.emit(match report {
            Report::Backfill => Event::Backfill { store, done, total },
            Report::Reembed => Event::Reembed { store, done, total },
        });
    }

    pub(crate) fn check_stop(&self) -> Result<(), Halt> {
        if self.stop.is_stopped() {
            return Err(Halt::Stopped);
        }

        Ok(())
    }
}
