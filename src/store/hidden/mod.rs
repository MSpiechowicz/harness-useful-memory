//! Team notes and team scripts hidden on this computer (`harness-memory notes hide`,
//! `harness-memory scripts hide`).
//!
//! Each list lives in the local SQLite file, in a table of its own (`hidden_team_notes`, see
//! [`notes`], and `hidden_team_scripts`, see [`scripts`]), one row per team database
//! ([`crate::backend::Backend::team_key`]) and entry; the team database is never changed. The tables
//! are not part of the shared schema ([`crate::store::sqlite::schema`], which also migrates libSQL
//! team databases): they are created here with `CREATE TABLE IF NOT EXISTS` and leave
//! `user_version` alone. [`Hidden`] reads its table on every call, so running servers see an entry
//! hidden or shown again by another process. Without the local file nothing is hidden, and reading
//! never creates it; [`Hidden::hide`] does.
//!
//! Each row keeps the scope, repository, label (a note's title, a script's name) and creation time
//! of the entry, so the hidden entries can be listed and an id the team database gave to another
//! entry later told apart ([`Hidden::prune`] drops such rows, and those of entries the team
//! database no longer has). When it last did is kept per team database in the table's `_meta`
//! table (created the same way), so the processes started one after another (a `notes list` every
//! few seconds) prune at most once per [`PRUNE_EVERY`] between them ([`Hidden::prune_if_due`]).
//!
//! **Reading never fails.** When the local file cannot be read (it is read-only and has no table
//! yet, another process holds it locked past [`BUSY_TIMEOUT`], or it is damaged), the read paths
//! ([`Hidden::ids`], [`Hidden::list`], [`Hidden::remove`]) act as if nothing were hidden and say so
//! once on stderr. [`Hidden::hide`] and [`Hidden::unhide`] fail, and so does [`Hidden::try_ids`],
//! which a change to the team's entries uses: deleting every entry, or changing one, must not reach
//! a hidden one because the list could not be read.

pub mod notes;
pub mod scripts;

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::limits::HIDDEN_MAX;
use crate::store::{ProjectKey, Scope, SqliteStore, StoreError, View};

pub use notes::{HiddenNote, HiddenNotes, Notes};
pub use scripts::{HiddenScripts, Scripts};

/// How long a statement waits for another server's lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a hidden list is checked against the team database, across processes (see the
/// module docs).
pub const PRUNE_EVERY: Duration = Duration::from_secs(300);

/// The table of one kind of hidden entry and how messages name the entries.
#[derive(Debug)]
pub struct Table {
    /// The table (`hidden_team_notes`).
    name: &'static str,
    /// The table of prune times (`hidden_team_notes_meta`).
    meta: &'static str,
    /// The id column (`note_id`).
    id: &'static str,
    /// The label column (`title`).
    label: &'static str,
    /// What an entry is, as messages and commands name it (`note`).
    noun: &'static str,
    /// Both tables, created when missing.
    create: &'static str,
    /// Set once the warning that the list cannot be read was printed.
    warned: AtomicBool,
}

/// A kind of team entry that can be hidden: the table and how the team database is read to prune.
pub trait Kind {
    /// What [`Hidden::hide`] takes, as read from the team database.
    type Item: ?Sized;
    /// The team store [`Hidden::prune`] reads.
    type Store: ?Sized;

    fn table() -> &'static Table;

    /// The row `item` is recorded as (its `hidden_at` is set when written).
    fn entry(item: &Self::Item) -> HiddenEntry;

    /// The ids of `entries` (all of `view`) that `store` no longer has, or gave to a newer entry
    /// (created at another time).
    fn gone(store: &mut Self::Store, view: &View, entries: &[&HiddenEntry]) -> Result<Vec<i64>, StoreError>;
}

/// A hidden team entry, as recorded when it was hidden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HiddenEntry {
    /// The team database's id (the id the entry shows).
    pub id: i64,
    pub scope: Scope,
    /// The repository key, for a repository entry.
    pub project: Option<String>,
    /// A note's title, a script's name.
    pub label: String,
    /// When the team entry was created (Unix seconds).
    pub team_created_at: i64,
    /// When it was hidden (Unix seconds).
    pub hidden_at: i64,
}

/// The team entries of kind `K` of one team database hidden on this computer. See the module docs.
pub struct Hidden<K: Kind> {
    path: PathBuf,
    team: String,
    conn: Option<Connection>,
    kind: PhantomData<fn() -> K>,
}

impl<K: Kind> std::fmt::Debug for Hidden<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hidden")
            .field("table", &K::table().name)
            .field("path", &self.path)
            .field("team", &self.team)
            .finish_non_exhaustive()
    }
}

impl<K: Kind> Hidden<K> {
    /// The entries of the team database `team` (its [`crate::backend::Backend::team_key`]) hidden
    /// in the local SQLite file at `path`. Nothing is opened yet.
    pub fn new(path: PathBuf, team: impl Into<String>) -> Self {
        Self {
            path,
            team: team.into(),
            conn: None,
            kind: PhantomData,
        }
    }

    /// The ids of the hidden entries, smallest first (at most [`HIDDEN_MAX`]); none when the list
    /// cannot be read (see the module docs).
    pub fn ids(&mut self) -> Vec<i64> {
        let read = self.read_ids();
        self.or_nothing(read)
    }

    /// The ids [`Hidden::ids`] reads, or why the list cannot be read (none hidden without the
    /// local file).
    pub fn try_ids(&mut self) -> Result<Vec<i64>, StoreError> {
        let noun = K::table().noun;

        self.read_ids().map_err(|error| {
            StoreError::Backend(format!(
                "cannot read the team {noun}s hidden on this computer from {} ({error}), so the team {noun}s \
                 were not changed",
                self.path.display()
            ))
        })
    }

    fn read_ids(&mut self) -> Result<Vec<i64>, StoreError> {
        let table = K::table();
        let team = self.team.clone();
        let Some(conn) = self.open(false)? else {
            return Ok(Vec::new());
        };

        let sql = format!(
            "SELECT {id} FROM {name} WHERE team = ?1 ORDER BY {id} LIMIT ?2",
            id = table.id,
            name = table.name
        );
        let mut statement = conn.prepare_cached(&sql)?;
        let ids = statement
            .query_map(params![team, limit()], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(ids)
    }

    /// The hidden entries, the latest hidden first (at most [`HIDDEN_MAX`]); none when the list
    /// cannot be read (see the module docs).
    pub fn list(&mut self) -> Vec<HiddenEntry> {
        let read = self.read_list();
        self.or_nothing(read)
    }

    fn read_list(&mut self) -> Result<Vec<HiddenEntry>, StoreError> {
        let table = K::table();
        let team = self.team.clone();
        let Some(conn) = self.open(false)? else {
            return Ok(Vec::new());
        };

        let sql = format!(
            "SELECT {id}, scope, project, {label}, team_created_at, hidden_at FROM {name}
             WHERE team = ?1 ORDER BY hidden_at DESC, {id} DESC LIMIT ?2",
            id = table.id,
            label = table.label,
            name = table.name
        );
        let mut statement = conn.prepare_cached(&sql)?;
        let hidden = statement
            .query_map(params![team, limit()], |row| {
                let scope: String = row.get(1)?;

                Ok(HiddenEntry {
                    id: row.get(0)?,
                    scope: if scope == "global" { Scope::Global } else { Scope::Repo },
                    project: row.get(2)?,
                    label: row.get(3)?,
                    team_created_at: row.get(4)?,
                    hidden_at: row.get(5)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(hidden)
    }

    /// Hides the team entry `item` (creating the local file when missing); false when it already
    /// was. Refused once [`HIDDEN_MAX`] entries are hidden. The check and the write are one
    /// `BEGIN IMMEDIATE` transaction, so processes hiding at once cannot pass the limit.
    pub fn hide(&mut self, item: &K::Item) -> Result<bool, StoreError> {
        let team = self.team.clone();
        let entry = K::entry(item);
        let Some(conn) = self.open(true)? else {
            return Err(StoreError::Backend("cannot open the local database".to_owned()));
        };

        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let hidden = insert_hidden(&tx, K::table(), &team, &entry)?;
        tx.commit()?;
        Ok(hidden)
    }

    /// Shows the team entry `id` again; false when it was not hidden.
    pub fn unhide(&mut self, id: i64) -> Result<bool, StoreError> {
        self.delete(&[id]).map(|removed| removed > 0)
    }

    /// Drops `ids` from the list (gone from the team database); how many were hidden. Nothing is
    /// dropped when the list cannot be changed (see the module docs).
    pub fn remove(&mut self, ids: &[i64]) -> usize {
        let deleted = self.delete(ids);
        self.or_nothing(deleted)
    }

    /// True when some entry is hidden and no process pruned the list in the last [`PRUNE_EVERY`]
    /// (or the clock went back since); false when the list cannot be read.
    pub fn prune_due(&mut self) -> bool {
        if self.ids().is_empty() {
            return false;
        }

        let read = self.pruned_at();
        let pruned_at = self.or_nothing(read.map(Some));
        let window = i64::try_from(PRUNE_EVERY.as_secs()).unwrap_or(i64::MAX);

        match pruned_at {
            Some(Some(at)) => {
                let now = now();
                now < at || now - at >= window
            }
            Some(None) => true,
            None => false,
        }
    }

    /// Prunes the list ([`Hidden::prune`]) when [`Hidden::prune_due`], recording the time first so
    /// other processes skip it; how many rows went. Nothing is pruned when the time cannot be
    /// recorded (the list could not be changed either).
    pub fn prune_if_due(&mut self, team: &mut K::Store) -> Result<usize, StoreError> {
        if !self.prune_due() {
            return Ok(0);
        }

        let marked = self.mark_pruned().map(|()| true);

        if !self.or_nothing(marked) {
            return Ok(0);
        }

        self.prune(team)
    }

    /// When the list of this team database was last pruned (Unix seconds); `None` when never.
    fn pruned_at(&mut self) -> Result<Option<i64>, StoreError> {
        let meta = K::table().meta;
        let team = self.team.clone();
        let Some(conn) = self.open(false)? else {
            return Ok(None);
        };

        let at = conn
            .query_row(
                &format!("SELECT pruned_at FROM {meta} WHERE team = ?1"),
                params![team],
                |row| row.get(0),
            )
            .optional()?;
        Ok(at)
    }

    fn mark_pruned(&mut self) -> Result<(), StoreError> {
        let meta = K::table().meta;
        let team = self.team.clone();
        let Some(conn) = self.open(false)? else {
            return Err(StoreError::Backend("the local database is gone".to_owned()));
        };

        conn.execute(
            &format!(
                "INSERT INTO {meta} (team, pruned_at) VALUES (?1, ?2)
                 ON CONFLICT (team) DO UPDATE SET pruned_at = excluded.pruned_at"
            ),
            params![team, now()],
        )?;
        Ok(())
    }

    /// Drops the rows of entries the team store `team` no longer has, or whose id it gave to a
    /// newer entry (created at another time); how many went. The team store is read per scope.
    pub fn prune(&mut self, team: &mut K::Store) -> Result<usize, StoreError> {
        let listed = self.list();

        if listed.is_empty() {
            return Ok(0);
        }

        let mut gone: Vec<i64> = Vec::new();

        for (view, entries) in by_view(&listed) {
            gone.extend(K::gone(team, &view, &entries)?);
        }

        Ok(self.remove(&gone))
    }

    fn delete(&mut self, ids: &[i64]) -> Result<usize, StoreError> {
        let table = K::table();
        let team = self.team.clone();
        let Some(conn) = self.open(false)? else {
            return Ok(0);
        };

        let sql = format!("DELETE FROM {} WHERE team = ?1 AND {} = ?2", table.name, table.id);
        let mut removed = 0;

        for id in ids {
            removed += conn.execute(&sql, params![team, id])?;
        }

        Ok(removed)
    }

    /// The connection to the local file with the tables in it; `None` when the file does not exist
    /// and `create` is off. A file another server creates later is opened by the next call.
    fn open(&mut self, create: bool) -> Result<Option<&Connection>, StoreError> {
        if self.conn.is_none() {
            if !self.path.exists() {
                if !create {
                    return Ok(None);
                }

                // Creates the file as the notes store does (private, migrated).
                drop(SqliteStore::open(&self.path)?);
            }

            let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let conn = Connection::open_with_flags(&self.path, flags)?;
            conn.busy_timeout(BUSY_TIMEOUT)?;
            conn.execute_batch(K::table().create)?;
            self.conn = Some(conn);
        }

        Ok(self.conn.as_ref())
    }

    /// The value `read` gives, or nothing (with a warning, once per kind) when it failed.
    fn or_nothing<T: Default>(&self, read: Result<T, StoreError>) -> T {
        match read {
            Ok(value) => value,
            Err(error) => {
                let table = K::table();

                if !table.warned.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "harness-memory: cannot read the team {}s hidden on this computer from {} ({error}), so \
                         they are shown",
                        table.noun,
                        self.path.display()
                    );
                }

                T::default()
            }
        }
    }
}

/// `entries` grouped by where they are read: one repository's, or the global scope.
fn by_view(entries: &[HiddenEntry]) -> Vec<(View, Vec<&HiddenEntry>)> {
    let mut grouped: BTreeMap<Option<String>, Vec<&HiddenEntry>> = BTreeMap::new();

    for entry in entries {
        let place = match entry.scope {
            Scope::Repo => Some(entry.project.clone().unwrap_or_default()),
            Scope::Global => None,
        };
        grouped.entry(place).or_default().push(entry);
    }

    grouped
        .into_iter()
        .map(|(place, entries)| (view_of(place), entries))
        .collect()
}

/// The view of one repository's entries (`Some` key) or the global ones.
pub(crate) fn view_of(project: Option<String>) -> View {
    match project {
        Some(key) => View {
            repo: Some(ProjectKey {
                key,
                root: PathBuf::new(),
                remote: None,
            }),
            global: false,
        },
        None => View {
            repo: None,
            global: true,
        },
    }
}

/// Records `entry` as hidden for `team` in the open transaction `tx`; false when it already was.
fn insert_hidden(tx: &Connection, table: &Table, team: &str, entry: &HiddenEntry) -> Result<bool, StoreError> {
    let Table {
        name, id, label, noun, ..
    } = table;

    let known: Option<i64> = tx
        .query_row(
            &format!("SELECT 1 FROM {name} WHERE team = ?1 AND {id} = ?2"),
            params![team, entry.id],
            |row| row.get(0),
        )
        .optional()?;

    if known.is_some() {
        return Ok(false);
    }

    let count: i64 = tx.query_row(
        &format!("SELECT count(*) FROM {name} WHERE team = ?1"),
        params![team],
        |row| row.get(0),
    )?;

    if count >= limit() {
        return Err(StoreError::Invalid(format!(
            "{HIDDEN_MAX} team {noun}s are hidden on this computer already: show one again with harness-memory \
             {noun}s unhide <id> first"
        )));
    }

    tx.execute(
        &format!(
            "INSERT OR IGNORE INTO {name} (team, {id}, scope, project, {label}, team_created_at, hidden_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
        ),
        params![
            team,
            entry.id,
            entry.scope.as_str(),
            entry.project,
            entry.label,
            entry.team_created_at,
            now()
        ],
    )?;
    Ok(true)
}

/// [`HIDDEN_MAX`] as a statement parameter.
fn limit() -> i64 {
    i64::try_from(HIDDEN_MAX).unwrap_or(i64::MAX)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
}
