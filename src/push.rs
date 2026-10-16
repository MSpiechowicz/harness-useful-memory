//! `harness-memory push`: copies the local SQLite memory into a team database.
//!
//! The notes (repository and global) and relations of the local database are written to the team
//! store through [`MemoryStore`], so any backend can receive it. [`Options`] can narrow that to one
//! repository and leave the global scope out. A note whose title already exists in the same scope
//! of the team store is replaced only when the local note was changed later and differs, else it
//! is skipped, so pushing again adds nothing. Replacing it can overwrite a teammate's text: those
//! notes are listed in [`Summary::changes`]. A relation that already exists in the team store as
//! it would be written is left alone.
//!
//! What the trait cannot carry stays behind: the team store stamps its own creation and update
//! times, revisions and hit counts start fresh, and an updated note keeps the entities it had
//! (only added notes get the local ones; at most [`ENTITIES_PER_NOTE`] of them).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::limits::{ENTITIES_PER_NOTE, NEIGHBORS_LIMIT_MAX};
use crate::store::{Link, MemoryStore, NoteDraft, NoteKind, NotePatch, ProjectKey, Provenance, SqliteStore};
use crate::store::{StoreError, Target, View, clean};

/// The label of the global scope in [`Change::scope`] and [`Summary::projects`].
pub const GLOBAL: &str = "global";

/// What to push.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Count and list only; write nothing.
    pub dry_run: bool,
    /// Only the notes and relations of this repository key (`git:…` or `path:…`); `None` for
    /// every repository.
    pub repo: Option<String>,
    /// Leave the global notes and relations out.
    pub skip_global: bool,
}

impl Options {
    /// True when the scope `target` is pushed.
    fn takes(&self, target: &Target) -> bool {
        match target {
            Target::Global => !self.skip_global,
            Target::Repo(key) => self.repo.as_ref().is_none_or(|repo| *repo == key.key),
        }
    }
}

/// A local note as pushed.
#[derive(Debug, Clone)]
struct LocalNote {
    id: i64,
    target: Target,
    kind: NoteKind,
    title: String,
    body: String,
    tags: Vec<String>,
    entities: Vec<String>,
    provenance: Provenance,
    updated_at: i64,
}

/// A local relation as pushed.
#[derive(Debug, Clone)]
struct LocalLink {
    target: Target,
    link: Link,
}

/// What a push did to one note in the team store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The note was not there and was added.
    Added,
    /// The team's note had the same text; its kind or tags were replaced.
    Updated,
    /// The team's note had another text, which the newer local one replaced.
    Overwritten,
}

/// One note a push added or changed (or, for a dry run, would).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The repository key, or [`GLOBAL`].
    pub scope: String,
    pub title: String,
    pub action: Action,
}

/// What a push did (or, for a dry run, would do).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub added: usize,
    /// Notes whose kind or tags were replaced; the team's text was the same.
    pub updated: usize,
    /// Notes whose team text was replaced by a newer, different local text.
    pub overwritten: usize,
    pub skipped: usize,
    /// Relations written to the team store.
    pub links_added: usize,
    /// Relations already in the team store as they would be written, so left alone.
    pub links_unchanged: usize,
    /// Added notes that had more than [`ENTITIES_PER_NOTE`] entities, of which only the first
    /// were linked.
    pub entities_cut: usize,
    /// The added, updated and overwritten notes, in push order.
    pub changes: Vec<Change>,
    /// The repository keys (and [`GLOBAL`]) whose notes or relations were pushed or skipped.
    pub projects: BTreeSet<String>,
    /// Notes and relations that could not be written: what and why.
    pub failed: Vec<String>,
}

/// Pushes the local database at `local` into `remote` as `options` say.
///
/// The local database is migrated first like the server would (it is never created: a missing
/// file is an error), then read through a read-only connection.
pub fn push(local: &Path, remote: &mut dyn MemoryStore, options: &Options) -> Result<Summary, StoreError> {
    let (notes, links) = read_local(local)?;
    let mut summary = Summary::default();
    let mut remote_ids: HashMap<i64, i64> = HashMap::new();

    for note in notes.iter().filter(|note| options.takes(&note.target)) {
        summary.projects.insert(scope_label(&note.target));

        match push_note(remote, note, options.dry_run, &mut summary) {
            Ok(Some(id)) => {
                remote_ids.insert(note.id, id);
            }
            Ok(None) => {}
            Err(error) => summary
                .failed
                .push(format!("note {} \"{}\": {error}", note.id, note.title)),
        }
    }

    for local_link in links.iter().filter(|link| options.takes(&link.target)) {
        summary.projects.insert(scope_label(&local_link.target));

        let mapped = local_link.link.note_id.map(|id| remote_ids.get(&id).copied());
        let link = Link {
            note_id: mapped.flatten(),
            ..local_link.link.clone()
        };

        // In a dry run a note that would be added has no team id yet, so its relation would change.
        let awaits_note = matches!(mapped, Some(None));
        let exists = match link_exists(remote, &local_link.target, &link) {
            Ok(exists) => exists && !(options.dry_run && awaits_note),
            Err(error) => {
                summary.failed.push(relation_failure(&link, &error));
                continue;
            }
        };

        if exists {
            summary.links_unchanged += 1;
            continue;
        }

        if options.dry_run {
            summary.links_added += 1;
            continue;
        }

        match remote.link(&local_link.target, &link) {
            Ok(_) => summary.links_added += 1,
            Err(error) => summary.failed.push(relation_failure(&link, &error)),
        }
    }

    Ok(summary)
}

/// Adds, replaces or skips one note; the team store's id of the note when it has one.
fn push_note(
    remote: &mut dyn MemoryStore,
    note: &LocalNote,
    dry_run: bool,
    summary: &mut Summary,
) -> Result<Option<i64>, StoreError> {
    let existing = remote.find_by_title(&note.target, &note.title)?;

    let Some(existing) = existing else {
        record(summary, note, Action::Added);

        if note.entities.len() > ENTITIES_PER_NOTE {
            summary.entities_cut += 1;
        }

        if dry_run {
            return Ok(None);
        }

        let draft = NoteDraft {
            title: note.title.clone(),
            body: note.body.clone(),
            kind: note.kind,
            tags: note.tags.clone(),
            entities: note.entities.iter().take(ENTITIES_PER_NOTE).cloned().collect(),
            provenance: note.provenance.clone(),
        };

        return remote.remember(&note.target, &draft).map(|saved| Some(saved.id));
    };

    let same = existing.title == note.title
        && existing.body == note.body
        && existing.kind == note.kind
        && existing.tags == note.tags;

    if note.updated_at <= existing.updated_at || same {
        summary.skipped += 1;
        return Ok(Some(existing.id));
    }

    if existing.body == note.body {
        record(summary, note, Action::Updated);
    } else {
        record(summary, note, Action::Overwritten);
    }

    if dry_run {
        return Ok(Some(existing.id));
    }

    let patch = NotePatch {
        title: Some(note.title.clone()),
        body: Some(note.body.clone()),
        append: None,
        kind: Some(note.kind),
        tags: Some(note.tags.clone()),
        provenance: note.provenance.clone(),
    };

    remote
        .update(&View::around(&note.target), existing.id, &patch)
        .map(|saved| Some(saved.id))
}

/// Counts `action` for `note` and lists it in [`Summary::changes`].
fn record(summary: &mut Summary, note: &LocalNote, action: Action) {
    match action {
        Action::Added => summary.added += 1,
        Action::Updated => summary.updated += 1,
        Action::Overwritten => summary.overwritten += 1,
    }

    summary.changes.push(Change {
        scope: scope_label(&note.target),
        title: note.title.clone(),
        action,
    });
}

/// True when the scope `target` of the team store already holds `link` as writing it would leave
/// it: the same entities and label, and its note (a relation without a note keeps the one it has).
/// Only the relations [`MemoryStore::neighbors`] lists for the first entity are looked at, so one
/// past [`NEIGHBORS_LIMIT_MAX`] counts as missing and is written again.
fn link_exists(remote: &mut dyn MemoryStore, target: &Target, link: &Link) -> Result<bool, StoreError> {
    // An invalid relation is left to `link`, which reports why.
    let Ok(wanted) = clean::link(link) else {
        return Ok(false);
    };

    let view = match target {
        Target::Repo(key) => View {
            repo: Some(key.clone()),
            global: false,
        },
        Target::Global => View {
            repo: None,
            global: true,
        },
    };

    let around = match remote.neighbors(&view, &wanted.from, 1, NEIGHBORS_LIMIT_MAX) {
        Ok(around) => around,
        Err(StoreError::NotFound(_)) => return Ok(false),
        Err(error) => return Err(error),
    };

    let (from, to) = (clean::normalize(&wanted.from), clean::normalize(&wanted.to));

    Ok(around.edges.iter().any(|edge| {
        clean::normalize(&edge.from) == from
            && edge.relation == wanted.relation
            && clean::normalize(&edge.to) == to
            && wanted.note_id.is_none_or(|id| edge.note_id == Some(id))
    }))
}

fn relation_failure(link: &Link, error: &StoreError) -> String {
    format!("relation \"{} {} {}\": {error}", link.from, link.relation, link.to)
}

/// The repository key of `target`, or [`GLOBAL`].
fn scope_label(target: &Target) -> String {
    match target {
        Target::Repo(key) => key.key.clone(),
        Target::Global => GLOBAL.to_owned(),
    }
}

/// The notes (oldest first) and relations of the local database.
fn read_local(path: &Path) -> Result<(Vec<LocalNote>, Vec<LocalLink>), StoreError> {
    if !path.is_file() {
        return Err(StoreError::NotFound(format!(
            "there is no local memory database at {}",
            path.display()
        )));
    }

    // Opening migrates an older file, and reports a newer one.
    let store = SqliteStore::open(path)?;
    if store.is_newer() {
        return Err(StoreError::NewerSchema);
    }
    drop(store);

    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    let mut entities: HashMap<i64, Vec<String>> = HashMap::new();
    let mut statement = conn.prepare(
        "SELECT ne.note_id, e.name FROM note_entities ne JOIN entities e ON e.id = ne.entity_id
         ORDER BY ne.note_id, e.name_norm",
    )?;
    let mut rows = statement.query([])?;

    while let Some(row) = rows.next()? {
        entities.entry(row.get(0)?).or_default().push(row.get(1)?);
    }

    let mut statement = conn.prepare(
        "SELECT n.id, p.key, p.root_path, p.remote, n.kind, n.title, n.body, n.tags, n.source, n.writer_root,
                n.via_override, n.updated_at
         FROM notes n LEFT JOIN projects p ON p.id = n.project_id
         ORDER BY n.id",
    )?;
    let notes = statement
        .query_map([], |row| {
            let id: i64 = row.get(0)?;
            let kind: String = row.get(4)?;
            let tags: String = row.get(7)?;
            let root: Option<String> = row.get(9)?;

            Ok(LocalNote {
                id,
                target: target(row.get(1)?, row.get(2)?, row.get(3)?),
                kind: NoteKind::parse(&kind).unwrap_or(NoteKind::Note),
                title: row.get(5)?,
                body: row.get(6)?,
                tags: tags.split_whitespace().map(str::to_owned).collect(),
                entities: entities.remove(&id).unwrap_or_default(),
                provenance: Provenance {
                    root: root.map(PathBuf::from),
                    source: row.get(8)?,
                    via_override: row.get(10)?,
                },
                updated_at: row.get(11)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut statement = conn.prepare(
        "SELECT p.key, p.root_path, p.remote, s.name, r.rel, d.name, r.note_id
         FROM relations r
         JOIN entities s ON s.id = r.src
         JOIN entities d ON d.id = r.dst
         LEFT JOIN projects p ON p.id = r.project_id
         ORDER BY r.id",
    )?;
    let links = statement
        .query_map([], |row| {
            Ok(LocalLink {
                target: target(row.get(0)?, row.get(1)?, row.get(2)?),
                link: Link {
                    from: row.get(3)?,
                    relation: row.get(4)?,
                    to: row.get(5)?,
                    note_id: row.get(6)?,
                },
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok((notes, links))
}

/// The scope of a row from its project's key, root and remote (all `None` for global).
fn target(key: Option<String>, root: Option<String>, remote: Option<String>) -> Target {
    match key {
        Some(key) => Target::Repo(ProjectKey {
            key,
            root: PathBuf::from(root.unwrap_or_default()),
            remote,
        }),
        None => Target::Global,
    }
}
