//! The vector cache of this computer: `<data>/harness-memory/vectors.sqlite3`.
//!
//! Vectors never go to a team database. The file holds the vectors of every model the embedder
//! knows (the active one and, while it switches, the next), but only the active model's vectors
//! are ever loaded into memory or compared: a vector of another model means nothing next to a
//! query of this one.
//!
//! Tables: `vectors(store, note_id, project, content_hash, model, dim, vec)` (one row per note and
//! model, `vec` the little-endian `f32`s), `cursors(store, updated_at_max)` (how far the backfill
//! read each store) and `active(model)` (the model that answers searches).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{Connection, OpenFlags, params};

use super::protocol::{Target, VectorHit};

/// What the cache keeps in memory per note of the active model.
struct Entry {
    project: Option<String>,
    hash: String,
    vector: Vec<f32>,
}

/// The cache. See the module docs.
pub struct Cache {
    conn: Connection,
    active: Option<String>,
    index: HashMap<(String, i64), Entry>,
}

type Result<T> = std::result::Result<T, String>;

fn failed(error: rusqlite::Error) -> String {
    format!("the vector cache failed: {error}")
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS vectors (
    store TEXT NOT NULL,
    note_id INTEGER NOT NULL,
    project TEXT,
    content_hash TEXT NOT NULL,
    model TEXT NOT NULL,
    dim INTEGER NOT NULL,
    vec BLOB NOT NULL,
    PRIMARY KEY (store, note_id, model)
);
CREATE INDEX IF NOT EXISTS vectors_model ON vectors (model, store);
CREATE TABLE IF NOT EXISTS cursors (store TEXT PRIMARY KEY, updated_at_max INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS active (model TEXT NOT NULL);
";

impl Cache {
    /// Opens (creating, `0600` on Unix) the cache at `path` and loads the active model's vectors.
    pub fn open(path: &Path) -> Result<Self> {
        create_private(path).map_err(|error| format!("cannot create the vector cache: {}", error.kind()))?;

        let conn = Connection::open(path).map_err(failed)?;
        conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(failed)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
            .map_err(failed)?;
        conn.execute_batch(SCHEMA).map_err(failed)?;

        let mut cache = Self {
            conn,
            active: None,
            index: HashMap::new(),
        };

        cache.active = cache.stored_active()?;
        cache.reload()?;
        Ok(cache)
    }

    /// The model that answers searches.
    pub fn active(&self) -> Option<&str> {
        self.active.as_deref()
    }

    fn stored_active(&self) -> Result<Option<String>> {
        let mut statement = self.conn.prepare("SELECT model FROM active LIMIT 1").map_err(failed)?;
        let mut rows = statement.query([]).map_err(failed)?;

        match rows.next().map_err(failed)? {
            Some(row) => Ok(Some(row.get(0).map_err(failed)?)),
            None => Ok(None),
        }
    }

    /// Loads the vectors of the active model into memory, replacing what was there.
    fn reload(&mut self) -> Result<()> {
        let mut index = HashMap::new();

        if let Some(model) = &self.active {
            let mut statement = self
                .conn
                .prepare("SELECT store, note_id, project, content_hash, dim, vec FROM vectors WHERE model = ?1")
                .map_err(failed)?;
            let mut rows = statement.query([model]).map_err(failed)?;

            while let Some(row) = rows.next().map_err(failed)? {
                let dim: usize = row.get::<_, i64>(4).map_err(failed)?.try_into().unwrap_or(0);
                let blob: Vec<u8> = row.get(5).map_err(failed)?;

                let Some(vector) = decode(&blob, dim) else {
                    continue;
                };

                index.insert(
                    (row.get(0).map_err(failed)?, row.get(1).map_err(failed)?),
                    Entry {
                        project: row.get(2).map_err(failed)?,
                        hash: row.get(3).map_err(failed)?,
                        vector,
                    },
                );
            }
        }

        self.index = index;
        Ok(())
    }

    /// Makes `model` the one that answers searches, in one transaction, and loads its vectors.
    /// The cursors start over: the next backfill reads every store whole.
    pub fn activate(&mut self, model: &str) -> Result<()> {
        let tx = self.conn.transaction().map_err(failed)?;
        tx.execute("DELETE FROM active", []).map_err(failed)?;
        tx.execute("INSERT INTO active (model) VALUES (?1)", [model])
            .map_err(failed)?;
        tx.execute("DELETE FROM cursors", []).map_err(failed)?;
        tx.commit().map_err(failed)?;

        self.active = Some(model.to_owned());
        self.reload()
    }

    /// Stores the vector of a note under `model`; also in memory when `model` is the active one.
    pub fn put(
        &mut self,
        store: &str,
        id: i64,
        project: Option<&str>,
        hash: &str,
        model: &str,
        vector: &[f32],
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO vectors (store, note_id, project, content_hash, model, dim, vec)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![store, id, project, hash, model, vector.len() as i64, encode(vector)],
            )
            .map_err(failed)?;

        if self.active.as_deref() == Some(model) {
            self.index.insert(
                (store.to_owned(), id),
                Entry {
                    project: project.map(str::to_owned),
                    hash: hash.to_owned(),
                    vector: vector.to_vec(),
                },
            );
        }

        Ok(())
    }

    /// The content hash stored for a note under `model`.
    pub fn hash_of(&self, store: &str, id: i64, model: &str) -> Option<String> {
        if self.active.as_deref() == Some(model) {
            return self.index.get(&(store.to_owned(), id)).map(|entry| entry.hash.clone());
        }

        self.conn
            .query_row(
                "SELECT content_hash FROM vectors WHERE store = ?1 AND note_id = ?2 AND model = ?3",
                params![store, id, model],
                |row| row.get(0),
            )
            .ok()
    }

    /// The ids stored for `store` under `model`.
    pub fn ids(&self, store: &str, model: &str) -> Result<Vec<i64>> {
        let mut statement = self
            .conn
            .prepare("SELECT note_id FROM vectors WHERE store = ?1 AND model = ?2")
            .map_err(failed)?;

        statement
            .query_map(params![store, model], |row| row.get(0))
            .map_err(failed)?
            .collect::<std::result::Result<_, _>>()
            .map_err(failed)
    }

    /// Deletes the vectors of `ids` in `store`, under every model.
    pub fn forget(&mut self, store: &str, ids: &[i64]) -> Result<()> {
        let tx = self.conn.transaction().map_err(failed)?;

        for id in ids {
            tx.execute(
                "DELETE FROM vectors WHERE store = ?1 AND note_id = ?2",
                params![store, id],
            )
            .map_err(failed)?;
        }

        tx.commit().map_err(failed)?;

        for id in ids {
            self.index.remove(&(store.to_owned(), *id));
        }

        Ok(())
    }

    /// The `limit` notes of `targets` closest to `query` with a cosine of at least `floor`, the
    /// closest first. Only the active model's vectors are compared.
    pub fn search(&self, query: &[f32], targets: &[Target], limit: usize, floor: f32) -> Vec<VectorHit> {
        let mut wanted: HashMap<&str, HashSet<Option<&str>>> = HashMap::new();

        for target in targets {
            wanted
                .entry(target.store.as_str())
                .or_default()
                .extend(target.projects.iter().map(Option::as_deref));
        }

        let mut hits: Vec<VectorHit> = self
            .index
            .iter()
            .filter(|((store, _), entry)| {
                wanted
                    .get(store.as_str())
                    .is_some_and(|projects| projects.contains(&entry.project.as_deref()))
                    && entry.vector.len() == query.len()
            })
            .map(|((store, id), entry)| VectorHit {
                store: store.clone(),
                id: *id,
                cosine: dot(query, &entry.vector),
            })
            .filter(|hit| hit.cosine >= floor)
            .collect();

        hits.sort_by(|a, b| b.cosine.total_cmp(&a.cosine).then_with(|| a.id.cmp(&b.id)));
        hits.truncate(limit);
        hits
    }

    /// How far the backfill has read `store`.
    pub fn cursor(&self, store: &str) -> Option<i64> {
        self.conn
            .query_row("SELECT updated_at_max FROM cursors WHERE store = ?1", [store], |row| {
                row.get(0)
            })
            .ok()
    }

    pub fn set_cursor(&mut self, store: &str, updated_at_max: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO cursors (store, updated_at_max) VALUES (?1, ?2)",
                params![store, updated_at_max],
            )
            .map_err(failed)?;

        Ok(())
    }

    /// The vectors stored under `model`.
    pub fn count(&self, model: &str) -> usize {
        self.conn
            .query_row("SELECT count(*) FROM vectors WHERE model = ?1", [model], |row| {
                row.get::<_, i64>(0)
            })
            .map_or(0, |count| usize::try_from(count).unwrap_or(0))
    }

    /// The models that have vectors.
    pub fn models(&self) -> Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT DISTINCT model FROM vectors")
            .map_err(failed)?;

        statement
            .query_map([], |row| row.get(0))
            .map_err(failed)?
            .collect::<std::result::Result<_, _>>()
            .map_err(failed)
    }

    /// Deletes every vector of `model`.
    pub fn delete_model(&mut self, model: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM vectors WHERE model = ?1", [model])
            .map_err(failed)?;

        if self.active.as_deref() == Some(model) {
            self.index.clear();
        }

        Ok(())
    }

    /// Deletes the vectors and cursors of the stores not in `keep`.
    pub fn keep_stores(&mut self, keep: &[String]) -> Result<()> {
        let stores: Vec<String> = {
            let mut statement = self
                .conn
                .prepare("SELECT store FROM vectors UNION SELECT store FROM cursors")
                .map_err(failed)?;

            statement
                .query_map([], |row| row.get(0))
                .map_err(failed)?
                .collect::<std::result::Result<_, _>>()
                .map_err(failed)?
        };

        for store in stores.iter().filter(|store| !keep.contains(store)) {
            self.conn
                .execute("DELETE FROM vectors WHERE store = ?1", [store])
                .map_err(failed)?;
            self.conn
                .execute("DELETE FROM cursors WHERE store = ?1", [store])
                .map_err(failed)?;
            self.index.retain(|(held, _), _| held != store);
        }

        Ok(())
    }

    /// Forgets how far every store was read, so the next backfill reads them whole.
    pub fn reset_cursors(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM cursors", []).map_err(failed)?;
        Ok(())
    }

    /// Deletes everything (for `remove-model`).
    pub fn clear(&mut self) -> Result<()> {
        self.conn
            .execute_batch("DELETE FROM vectors; DELETE FROM cursors; DELETE FROM active;")
            .map_err(failed)?;
        self.active = None;
        self.index.clear();
        Ok(())
    }
}

/// What `embedder status` reads without starting the embedder: the active model and how many
/// vectors it has. `None` when there is no cache file.
pub fn peek(path: &Path) -> Option<(Option<String>, usize)> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.busy_timeout(std::time::Duration::from_secs(2)).ok()?;

    let active: Option<String> = conn
        .query_row("SELECT model FROM active LIMIT 1", [], |row| row.get(0))
        .ok();

    let count = match &active {
        Some(model) => conn
            .query_row("SELECT count(*) FROM vectors WHERE model = ?1", [model], |row| {
                row.get::<_, i64>(0)
            })
            .map_or(0, |count| usize::try_from(count).unwrap_or(0)),
        None => 0,
    };

    Some((active, count))
}

/// Creates the file (and its folder) private when it does not exist.
fn create_private(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);

        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);

        builder.create(parent)?;
    }

    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);

    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);

    options.open(path).map(drop)
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn encode(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|value| value.to_le_bytes()).collect()
}

fn decode(blob: &[u8], dim: usize) -> Option<Vec<f32>> {
    if dim == 0 || blob.len() != dim * 4 {
        return None;
    }

    let (chunks, _) = blob.as_chunks::<4>();
    Some(chunks.iter().map(|bytes| f32::from_le_bytes(*bytes)).collect())
}
