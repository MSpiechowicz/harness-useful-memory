//! Notes: create-or-update by title, patch, read (counting hits) and delete.
//!
//! The agent's text arrives cleaned and checked ([`clean::draft`], [`clean::patch`]) before the
//! write transaction opens, so nothing slow runs while other servers wait for the lock.

use std::path::PathBuf;

use super::sql::{Row, Sql, args};
use super::{graph, now, target_project, target_project_existing, view_params};
use crate::limits::GET_IDS_MAX;
use crate::store::clean::{self, CleanDraft, CleanPatch};
use crate::store::extensions::Columns;
use crate::store::{Forgotten, Note, NoteKind, NoteUsage, Provenance, Saved, Scope, StoreError, Target, View};
use crate::support::lang;

/// The columns [`note_from_row`] reads, from `notes n LEFT JOIN projects p`; the last four are the
/// language and the usefulness counters, which a database without them has no columns for.
pub(super) fn note_columns(columns: Columns) -> String {
    note_select("n.body", columns)
}

/// [`note_columns`] with an empty body, for lists.
pub(super) fn row_columns(columns: Columns) -> String {
    note_select("''", columns)
}

fn note_select(body: &str, columns: Columns) -> String {
    let lang = if columns.lang { "n.lang" } else { "NULL" };
    let usage = if columns.usage {
        "n.shown, n.useful, n.last_useful_at"
    } else {
        "NULL, NULL, NULL"
    };

    format!(
        "n.id, n.project_id, p.key, n.kind, n.title, {body}, n.tags, n.source, \
         n.created_at, n.updated_at, n.revision, n.hits, n.writer_root, n.via_override, {lang}, {usage}"
    )
}

/// The visibility condition of a view (`?2` the project id, `?3` whether global is visible).
pub(super) const VISIBLE: &str =
    "(n.project_id = ?2 OR (?2 = -1 AND n.project_id IS NOT NULL) OR (?3 AND n.project_id IS NULL))";

/// Saves the draft. With `extensions` the note's `lang` is stored too, in a statement after the
/// content (see [`set_lang`]).
pub(super) fn remember(
    db: &mut dyn Sql,
    target: &Target,
    draft: &CleanDraft,
    extensions: bool,
) -> Result<Saved, StoreError> {
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

    if extensions {
        set_lang(db, id, &draft.title, &draft.body)?;
    }

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
pub(super) fn update(
    db: &mut dyn Sql,
    view: &View,
    id: i64,
    patch: &CleanPatch,
    extensions: bool,
) -> Result<Saved, StoreError> {
    let Some(note) = load(db, view, id, Columns::NONE)? else {
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
            &title,
            title_norm,
            &body,
            kind.as_str(),
            tags.join(" "),
            now(),
            writer.source.as_deref(),
            writer.root.as_deref(),
            writer.via_override
        ],
    )?;

    if extensions {
        set_lang(db, id, &title, &body)?;
    }

    saved(db, id, false)
}

/// Stores the language of the note `id`. It is a statement of its own after the content one:
/// `notes_lang_reset` clears a `lang` that an edit of the title or body left unchanged (an older
/// server's edit), and an `UPDATE` of `lang` alone does not fire it.
fn set_lang(db: &mut dyn Sql, id: i64, title: &str, body: &str) -> Result<(), StoreError> {
    let lang = lang::stored(lang::detect(title, body));
    db.execute("UPDATE notes SET lang = ?2 WHERE id = ?1", args![id, lang])?;

    Ok(())
}

/// Whether a note has no `lang` yet (written by a server that does not know the column).
pub(super) fn has_pending_lang(db: &mut dyn Sql) -> Result<bool, StoreError> {
    let pending: Option<i64> = db.value("SELECT 1 FROM notes WHERE lang IS NULL LIMIT 1", args![])?;

    Ok(pending.is_some())
}

/// Fills `lang` of up to `page` notes that have none, and returns how many it filled. Meant for a
/// write transaction, so a note is not edited between its read and its `lang`.
pub(super) fn fill_lang(db: &mut dyn Sql, page: usize) -> Result<usize, StoreError> {
    let rows = db.query(
        "SELECT id, title, body FROM notes WHERE lang IS NULL ORDER BY id LIMIT ?1",
        args![i64::try_from(page).unwrap_or(i64::MAX)],
    )?;

    for row in &rows {
        set_lang_if_missing(db, row.get(0)?, &row.get::<String>(1)?, &row.get::<String>(2)?)?;
    }

    Ok(rows.len())
}

fn set_lang_if_missing(db: &mut dyn Sql, id: i64, title: &str, body: &str) -> Result<(), StoreError> {
    let lang = lang::stored(lang::detect(title, body));
    db.execute(
        "UPDATE notes SET lang = ?2 WHERE id = ?1 AND lang IS NULL",
        args![id, lang],
    )?;

    Ok(())
}

/// The visible notes among `ids`, each read as it was before this read's hit (when `count`).
pub(super) fn get(
    db: &mut dyn Sql,
    view: &View,
    ids: &[i64],
    count: bool,
    columns: Columns,
) -> Result<Vec<Note>, StoreError> {
    if ids.is_empty() || ids.len() > GET_IDS_MAX {
        return Err(StoreError::Invalid(format!("ids must list 1-{GET_IDS_MAX} note ids")));
    }

    let mut found: Vec<Note> = Vec::new();

    for &id in ids {
        if found.iter().any(|note| note.id == id) {
            continue;
        }

        let Some(note) = load(db, view, id, columns)? else {
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
    if load(db, view, id, Columns::NONE)?.is_none() {
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
pub(super) fn forget_all(db: &mut dyn Sql, view: &View, exclude: &str) -> Result<Forgotten, StoreError> {
    const SCOPES: &str =
        "(project_id = ?1 OR (?2 AND project_id IS NULL)) AND id NOT IN (SELECT value FROM json_each(?3))";

    let (repo, global) = view_params(db, view)?;
    let mut forgotten = Forgotten::default();

    for row in db.query(
        &format!("SELECT project_id IS NULL, count(*) FROM notes WHERE {SCOPES} GROUP BY 1"),
        args![repo, global, exclude],
    )? {
        let is_global: bool = row.get(0)?;
        let count = usize::try_from(row.get::<i64>(1)?).unwrap_or(0);

        if is_global {
            forgotten.global = count;
        } else {
            forgotten.repo = count;
        }
    }

    db.execute(
        &format!("DELETE FROM notes WHERE {SCOPES}"),
        args![repo, global, exclude],
    )?;
    graph::prune_entities(db, repo, global)?;

    Ok(forgotten)
}

/// The note titled like `title` (a [`clean::title`]) in the scope `target`, without counting a
/// hit.
pub(super) fn find_by_title(
    db: &mut dyn Sql,
    target: &Target,
    title: &str,
    columns: Columns,
) -> Result<Option<Note>, StoreError> {
    let Some(project) = target_project_existing(db, target)? else {
        return Ok(None);
    };

    let sql = format!(
        "SELECT {} FROM notes n LEFT JOIN projects p ON p.id = n.project_id
         WHERE ifnull(n.project_id, 0) = ?1 AND n.title_norm = ?2",
        note_columns(columns)
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

/// The note `id` when it is visible in `view`; the optional `columns` are read only when present.
pub(super) fn load(db: &mut dyn Sql, view: &View, id: i64, columns: Columns) -> Result<Option<Note>, StoreError> {
    let (repo, global) = view_params(db, view)?;
    let sql = format!(
        "SELECT {} FROM notes n LEFT JOIN projects p ON p.id = n.project_id
         WHERE n.id = ?1 AND {VISIBLE}",
        note_columns(columns)
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
        lang: row.get::<Option<String>>(14)?.as_deref().and_then(lang::Lang::parse),
        usage: usage_from_row(row)?,
    })
}

/// The counters of a row of [`note_columns`]; `None` when the database has none (the columns are
/// NULL then, and `NOT NULL` where they exist).
fn usage_from_row(row: &Row) -> Result<Option<NoteUsage>, StoreError> {
    let Some(shown) = row.get::<Option<i64>>(15)? else {
        return Ok(None);
    };

    Ok(Some(NoteUsage {
        shown,
        useful: row.get::<Option<i64>>(16)?.unwrap_or(0),
        last_useful_at: row.get(17)?,
    }))
}

/// `ids` as a JSON array (`[3,7]`) for `json_each`, each id once; `None` for no id.
pub(super) fn ids_json(ids: &[i64]) -> Option<String> {
    let mut unique: Vec<i64> = ids.to_vec();
    unique.sort_unstable();
    unique.dedup();

    if unique.is_empty() {
        return None;
    }

    let listed: Vec<String> = unique.iter().map(i64::to_string).collect();
    Some(format!("[{}]", listed.join(",")))
}

/// Counts a time shown on the notes in the JSON array `ids`, and nothing else of them: not the
/// update time, the revision, the hits, nor the search indexes (their triggers watch the content).
pub(super) fn record_shown(db: &mut dyn Sql, ids: &str) -> Result<(), StoreError> {
    db.execute(
        "UPDATE notes SET shown = shown + 1 WHERE id IN (SELECT value FROM json_each(?1))",
        args![ids],
    )?;

    Ok(())
}

/// Counts a time found useful, at `at`, on the notes in the JSON array `ids` (see [`record_shown`]).
pub(super) fn record_useful(db: &mut dyn Sql, ids: &str, at: i64) -> Result<(), StoreError> {
    db.execute(
        "UPDATE notes SET useful = useful + 1, last_useful_at = ?2 WHERE id IN (SELECT value FROM json_each(?1))",
        args![ids, at],
    )?;

    Ok(())
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
