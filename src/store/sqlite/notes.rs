//! Notes: create-or-update by title, patch, read (counting hits) and delete.
//!
//! The agent's text arrives cleaned and checked ([`clean::draft`], [`clean::patch`]) before the
//! write transaction opens, so nothing slow runs while other servers wait for the lock.

use std::path::PathBuf;

use super::sql::{Row, Sql, args};
use super::{graph, now, target_project, target_project_existing, view_params};
use crate::limits::GET_IDS_MAX;
use crate::store::clean::{self, CleanDraft, CleanPatch};
use crate::store::{Forgotten, Note, NoteKind, Provenance, Saved, Scope, StoreError, Target, View};

/// The columns [`note_from_row`] reads, from `notes n LEFT JOIN projects p`.
const NOTE_COLUMNS: &str = "n.id, n.project_id, p.key, n.kind, n.title, n.body, n.tags, n.source, \
                            n.created_at, n.updated_at, n.revision, n.hits, n.writer_root, n.via_override";

/// [`NOTE_COLUMNS`] with an empty body, for lists.
pub(super) const ROW_COLUMNS: &str = "n.id, n.project_id, p.key, n.kind, n.title, '', n.tags, n.source, \
                                      n.created_at, n.updated_at, n.revision, n.hits, n.writer_root, n.via_override";

/// The visibility condition of a view (`?2` the project id, `?3` whether global is visible).
pub(super) const VISIBLE: &str = "(n.project_id = ?2 OR (?3 AND n.project_id IS NULL))";

pub(super) fn remember(db: &mut dyn Sql, target: &Target, draft: &CleanDraft) -> Result<Saved, StoreError> {
    let project = target_project(db, target)?;
    let now = now();

    let existing: Option<(i64, Option<String>, bool)> = db.row(
        "SELECT id, writer_root, via_override FROM notes WHERE ifnull(project_id, 0) = ?1 AND title_norm = ?2",
        args![project.unwrap_or(0), &draft.title_norm],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;

    let writer = &draft.writer;
    let (id, created) = match existing {
        Some((id, known_root, known_override)) => {
            // Any folder can claim a repository's remote, and `project_path` can name any
            // checkout: a same-title write from another checkout, or through `project_path`, must
            // replace the note on purpose, by id. A note last written through `project_path` holds
            // its writer's own root (none when that writer ran outside a checkout), not the
            // repository's, so it is replaced by id only too.
            let other_checkout = known_root.is_some() && known_root != writer.root;

            if project.is_some() && (other_checkout || known_override || writer.via_override) {
                return Err(StoreError::Conflict(id));
            }

            db.execute(
                "UPDATE notes SET title = ?2, body = ?3, kind = ?4, tags = ?5, updated_at = ?6,
                     revision = revision + 1, source = ?7, writer_root = ?8, via_override = ?9
                 WHERE id = ?1",
                args![
                    id,
                    &draft.title,
                    &draft.body,
                    draft.kind.as_str(),
                    &draft.tags,
                    now,
                    writer.source.as_deref(),
                    writer.root.as_deref(),
                    writer.via_override
                ],
            )?;
            (id, false)
        }
        None => {
            let inserted = db.execute(
                "INSERT INTO notes (project_id, kind, title, title_norm, body, tags, source, created_at, updated_at,
                                    writer_root, via_override)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9, ?10)",
                args![
                    project,
                    draft.kind.as_str(),
                    &draft.title,
                    &draft.title_norm,
                    &draft.body,
                    &draft.tags,
                    writer.source.as_deref(),
                    now,
                    writer.root.as_deref(),
                    writer.via_override
                ],
            )?;
            (inserted.last_insert_rowid, true)
        }
    };

    for name in &draft.entities {
        let entity = graph::ensure_entity(db, project, name)?;
        db.execute(
            "INSERT OR IGNORE INTO note_entities (note_id, entity_id) VALUES (?1, ?2)",
            args![id, entity],
        )?;
    }

    saved(db, id, created)
}

/// Applies `patch`. The body is masked after `append` is joined to it, so a secret split across
/// the two is masked as a whole (see [`clean::patched_body`]).
pub(super) fn update(db: &mut dyn Sql, view: &View, id: i64, patch: &CleanPatch) -> Result<Saved, StoreError> {
    let Some(note) = load(db, view, id)? else {
        return Err(not_found(id));
    };

    let title = patch.title.clone().unwrap_or_else(|| note.title.clone());
    let title_norm = clean::normalize(&title);
    ensure_title_free(db, &note, &title_norm)?;

    let body = clean::patched_body(&note.body, patch)?;
    let kind = patch.kind.unwrap_or(note.kind);
    let tags = patch.tags.clone().unwrap_or_else(|| note.tags.clone());
    let writer = &patch.writer;

    db.execute(
        "UPDATE notes SET title = ?2, title_norm = ?3, body = ?4, kind = ?5, tags = ?6, updated_at = ?7,
             revision = revision + 1, source = ?8, writer_root = ?9, via_override = ?10
         WHERE id = ?1",
        args![
            id,
            title,
            title_norm,
            body,
            kind.as_str(),
            tags.join(" "),
            now(),
            writer.source.as_deref(),
            writer.root.as_deref(),
            writer.via_override
        ],
    )?;

    saved(db, id, false)
}

/// The visible notes among `ids`, each read as it was before this read's hit (when `count`).
pub(super) fn get(db: &mut dyn Sql, view: &View, ids: &[i64], count: bool) -> Result<Vec<Note>, StoreError> {
    if ids.is_empty() || ids.len() > GET_IDS_MAX {
        return Err(StoreError::Invalid(format!("ids must list 1-{GET_IDS_MAX} note ids")));
    }

    let mut found: Vec<Note> = Vec::new();

    for &id in ids {
        if found.iter().any(|note| note.id == id) {
            continue;
        }

        let Some(note) = load(db, view, id)? else {
            continue;
        };

        if count {
            db.execute("UPDATE notes SET hits = hits + 1 WHERE id = ?1", args![id])?;
        }

        found.push(note);
    }

    Ok(found)
}

pub(super) fn forget(db: &mut dyn Sql, view: &View, id: i64) -> Result<usize, StoreError> {
    if load(db, view, id)?.is_none() {
        return Err(not_found(id));
    }

    let linked: Vec<i64> = db.column("SELECT entity_id FROM note_entities WHERE note_id = ?1", args![id])?;

    db.execute("DELETE FROM notes WHERE id = ?1", args![id])?;

    for entity in linked {
        graph::prune_entity(db, entity)?;
    }

    Ok(1)
}

/// Deletes every note visible in `view`, then the entities of those scopes that no note or
/// relation uses (a deleted note's relations stay, their `note_id` cleared by the schema).
pub(super) fn forget_all(db: &mut dyn Sql, view: &View) -> Result<Forgotten, StoreError> {
    const SCOPES: &str = "(project_id = ?1 OR (?2 AND project_id IS NULL))";

    let (repo, global) = view_params(db, view)?;
    let mut forgotten = Forgotten::default();

    for row in db.query(
        &format!("SELECT project_id IS NULL, count(*) FROM notes WHERE {SCOPES} GROUP BY 1"),
        args![repo, global],
    )? {
        let is_global: bool = row.get(0)?;
        let count = usize::try_from(row.get::<i64>(1)?).unwrap_or(0);

        if is_global {
            forgotten.global = count;
        } else {
            forgotten.repo = count;
        }
    }

    db.execute(&format!("DELETE FROM notes WHERE {SCOPES}"), args![repo, global])?;
    graph::prune_entities(db, repo, global)?;

    Ok(forgotten)
}

/// The note titled like `title` (a [`clean::title`]) in the scope `target`, without counting a
/// hit.
pub(super) fn find_by_title(db: &mut dyn Sql, target: &Target, title: &str) -> Result<Option<Note>, StoreError> {
    let Some(project) = target_project_existing(db, target)? else {
        return Ok(None);
    };

    let sql = format!(
        "SELECT {NOTE_COLUMNS} FROM notes n LEFT JOIN projects p ON p.id = n.project_id
         WHERE ifnull(n.project_id, 0) = ?1 AND n.title_norm = ?2"
    );

    let Some(mut note) = db.row(
        &sql,
        args![project.unwrap_or(0), clean::normalize(title)],
        note_from_row,
    )?
    else {
        return Ok(None);
    };

    note.entities = entities_of(db, note.id)?;
    Ok(Some(note))
}

/// The note `id` when it is visible in `view`.
pub(super) fn load(db: &mut dyn Sql, view: &View, id: i64) -> Result<Option<Note>, StoreError> {
    let (repo, global) = view_params(db, view)?;
    let sql = format!(
        "SELECT {NOTE_COLUMNS} FROM notes n LEFT JOIN projects p ON p.id = n.project_id
         WHERE n.id = ?1 AND {VISIBLE}"
    );

    let note = db.row(&sql, args![id, repo, global], note_from_row)?;

    let Some(mut note) = note else {
        return Ok(None);
    };

    note.entities = entities_of(db, id)?;
    Ok(Some(note))
}

/// The names of the entities linked to note `id`, alphabetically.
pub(super) fn entities_of(db: &mut dyn Sql, id: i64) -> Result<Vec<String>, StoreError> {
    db.column(
        "SELECT e.name FROM note_entities ne JOIN entities e ON e.id = ne.entity_id
         WHERE ne.note_id = ?1 ORDER BY e.name_norm",
        args![id],
    )
}

/// Parses a stored kind (the CHECK constraint keeps it valid).
pub(super) fn kind_from_sql(raw: &str) -> NoteKind {
    NoteKind::parse(raw).unwrap_or(NoteKind::Note)
}

/// The scope of a row from its `project_id`.
pub(super) fn scope_of(project_id: Option<i64>) -> Scope {
    match project_id {
        Some(_) => Scope::Repo,
        None => Scope::Global,
    }
}

pub(super) fn note_from_row(row: &Row) -> Result<Note, StoreError> {
    let tags: String = row.get(6)?;
    let kind: String = row.get(3)?;
    let root: Option<String> = row.get(12)?;

    Ok(Note {
        id: row.get(0)?,
        scope: scope_of(row.get(1)?),
        project: row.get(2)?,
        kind: kind_from_sql(&kind),
        title: row.get(4)?,
        body: row.get(5)?,
        tags: tags.split_whitespace().map(str::to_owned).collect(),
        entities: Vec::new(),
        provenance: Provenance {
            root: root.map(PathBuf::from),
            source: row.get(7)?,
            via_override: row.get(13)?,
        },
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
        revision: row.get(10)?,
        hits: row.get(11)?,
    })
}

/// Fails when another note in the same scope already has `title_norm`.
fn ensure_title_free(db: &mut dyn Sql, note: &Note, title_norm: &str) -> Result<(), StoreError> {
    let other: Option<i64> = db.value(
        "SELECT other.id FROM notes other JOIN notes this ON this.id = ?1
         WHERE ifnull(other.project_id, 0) = ifnull(this.project_id, 0)
           AND other.title_norm = ?2 AND other.id != ?1",
        args![note.id, title_norm],
    )?;

    match other {
        Some(other) => Err(StoreError::Invalid(format!(
            "note {other} already has this title: update that note or delete one of them"
        ))),
        None => Ok(()),
    }
}

fn saved(db: &mut dyn Sql, id: i64, created: bool) -> Result<Saved, StoreError> {
    let saved = db.one(
        "SELECT n.revision, n.project_id, p.key FROM notes n LEFT JOIN projects p ON p.id = n.project_id
         WHERE n.id = ?1",
        args![id],
        |row| {
            Ok(Saved {
                id,
                created,
                revision: row.get(0)?,
                scope: scope_of(row.get(1)?),
                project: row.get(2)?,
            })
        },
    )?;

    Ok(saved)
}

fn not_found(id: i64) -> StoreError {
    StoreError::NotFound(format!("note {id} not found in this repository or the global scope"))
}
