//! Notes: create-or-update by title, patch, read (counting hits), lookup by title and delete.

use std::path::PathBuf;

use postgres::{GenericClient, Row, Transaction};

use super::search::fold;
use super::{KIND_MAX, PATH_MAX, SOURCE_MAX, backend, column, cut, graph, now, target_project};
use super::{target_project_existing, view_params};
use crate::limits::{BODY_MAX, ENTITY_MAX, GET_IDS_MAX, TAG_MAX, TAGS_MAX, TITLE_MAX};
use crate::store::clean::{self, CleanDraft, CleanPatch};
use crate::store::{Note, NoteKind, Provenance, Saved, Scope, StoreError, Target, View};

/// The columns [`note_from_row`] reads, from `notes n LEFT JOIN projects p`, with the text cut to
/// the limits (see [`cut`]).
fn note_columns() -> String {
    format!(
        "n.id, n.project_id, {key}, {kind}, {title}, {body}, {tags}, {source}, \
         n.created_at, n.updated_at, n.revision, n.hits, {root}, n.via_override",
        key = cut("p.key", PATH_MAX),
        kind = cut("n.kind", KIND_MAX),
        title = cut("n.title", TITLE_MAX),
        body = cut("n.body", BODY_MAX),
        tags = cut("n.tags", TAGS_MAX * (TAG_MAX + 1)),
        source = cut("n.source", SOURCE_MAX),
        root = cut("n.writer_root", PATH_MAX),
    )
}

/// The visibility condition of a view (`$2` the project id, `$3` whether global is visible).
pub(super) const VISIBLE: &str = "(n.project_id = $2::bigint OR ($3::boolean AND n.project_id IS NULL))";

pub(super) fn remember(tx: &mut Transaction<'_>, target: &Target, draft: &CleanDraft) -> Result<Saved, StoreError> {
    let project = target_project(tx, target)?;
    let now = now();
    let writer = &draft.writer;

    let sql = format!(
        "SELECT id, {}, via_override FROM notes WHERE coalesce(project_id, 0) = $1 AND title_norm = $2",
        cut("writer_root", PATH_MAX)
    );
    let existing = tx
        .query_opt(&sql, &[&project.unwrap_or(0), &draft.title_norm])
        .map_err(backend)?;

    let search_title = fold(&draft.title);
    let search_body = fold(&draft.body);

    let (id, created) = match existing {
        Some(row) => {
            let id: i64 = column(&row, 0)?;
            let known_root: Option<String> = column(&row, 1)?;
            let known_override: bool = column(&row, 2)?;

            // The same rule as the SQLite backend: a same-title write from another checkout, or
            // through `project_path`, or over a note last written that way, replaces by id only.
            let other_checkout = known_root.is_some() && known_root != writer.root;

            if project.is_some() && (other_checkout || known_override || writer.via_override) {
                return Err(StoreError::Conflict(id));
            }

            tx.execute(
                "UPDATE notes SET title = $2, body = $3, kind = $4, tags = $5, updated_at = $6,
                     revision = revision + 1, source = $7, writer_root = $8, via_override = $9,
                     written_by = current_user, search_title = $10, search_body = $11
                 WHERE id = $1",
                &[
                    &id,
                    &draft.title,
                    &draft.body,
                    &draft.kind.as_str(),
                    &draft.tags,
                    &now,
                    &writer.source,
                    &writer.root,
                    &writer.via_override,
                    &search_title,
                    &search_body,
                ],
            )
            .map_err(backend)?;
            (id, false)
        }
        None => {
            let row = tx
                .query_one(
                    "INSERT INTO notes (project_id, kind, title, title_norm, body, tags, source, created_at, updated_at,
                                        writer_root, via_override, search_title, search_body)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8, $9, $10, $11, $12)
                     RETURNING id",
                    &[
                        &project,
                        &draft.kind.as_str(),
                        &draft.title,
                        &draft.title_norm,
                        &draft.body,
                        &draft.tags,
                        &writer.source,
                        &now,
                        &writer.root,
                        &writer.via_override,
                        &search_title,
                        &search_body,
                    ],
                )
                .map_err(backend)?;
            (column(&row, 0)?, true)
        }
    };

    for name in &draft.entities {
        let entity = graph::ensure_entity(tx, project, name)?;
        tx.execute(
            "INSERT INTO note_entities (note_id, entity_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            &[&id, &entity],
        )
        .map_err(backend)?;
    }

    saved(tx, id, created)
}

/// Applies `patch` (the body is masked after `append` is joined, see [`clean::patched_body`]).
pub(super) fn update(tx: &mut Transaction<'_>, view: &View, id: i64, patch: &CleanPatch) -> Result<Saved, StoreError> {
    let Some(note) = load(tx, view, id)? else {
        return Err(not_found(id));
    };

    let title = patch.title.clone().unwrap_or_else(|| note.title.clone());
    let title_norm = clean::normalize(&title);
    ensure_title_free(tx, &note, &title_norm)?;

    let body = clean::patched_body(&note.body, patch)?;
    let kind = patch.kind.unwrap_or(note.kind);
    let tags = patch.tags.clone().unwrap_or_else(|| note.tags.clone()).join(" ");
    let writer = &patch.writer;

    tx.execute(
        "UPDATE notes SET title = $2, title_norm = $3, body = $4, kind = $5, tags = $6, updated_at = $7,
             revision = revision + 1, source = $8, writer_root = $9, via_override = $10,
             written_by = current_user, search_title = $11, search_body = $12
         WHERE id = $1",
        &[
            &id,
            &title,
            &title_norm,
            &body,
            &kind.as_str(),
            &tags,
            &now(),
            &writer.source,
            &writer.root,
            &writer.via_override,
            &fold(&title),
            &fold(&body),
        ],
    )
    .map_err(backend)?;

    saved(tx, id, false)
}

pub(super) fn get(tx: &mut Transaction<'_>, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
    if ids.is_empty() || ids.len() > GET_IDS_MAX {
        return Err(StoreError::Invalid(format!("ids must list 1-{GET_IDS_MAX} note ids")));
    }

    let mut found: Vec<Note> = Vec::new();

    for &id in ids {
        if found.iter().any(|note| note.id == id) {
            continue;
        }

        if let Some(note) = load(tx, view, id)? {
            tx.execute("UPDATE notes SET hits = hits + 1 WHERE id = $1", &[&id])
                .map_err(backend)?;
            found.push(note);
        }
    }

    Ok(found)
}

pub(super) fn forget(tx: &mut Transaction<'_>, view: &View, id: i64) -> Result<usize, StoreError> {
    if load(tx, view, id)?.is_none() {
        return Err(not_found(id));
    }

    let linked = tx
        .query("SELECT entity_id FROM note_entities WHERE note_id = $1", &[&id])
        .map_err(backend)?
        .iter()
        .map(|row| column::<i64>(row, 0))
        .collect::<Result<Vec<_>, _>>()?;

    tx.execute("DELETE FROM notes WHERE id = $1", &[&id]).map_err(backend)?;

    for entity in linked {
        graph::prune_entity(tx, entity)?;
    }

    Ok(1)
}

/// The note titled like `title` (a [`clean::title`]) in the scope `target`, without counting a
/// hit.
pub(super) fn find_by_title(
    client: &mut impl GenericClient,
    target: &Target,
    title: &str,
) -> Result<Option<Note>, StoreError> {
    let Some(project) = target_project_existing(client, target)? else {
        return Ok(None);
    };

    let sql = format!(
        "SELECT {} FROM notes n LEFT JOIN projects p ON p.id = n.project_id
         WHERE coalesce(n.project_id, 0) = $1 AND n.title_norm = $2",
        note_columns()
    );

    let row = client
        .query_opt(&sql, &[&project.unwrap_or(0), &clean::normalize(title)])
        .map_err(backend)?;

    let Some(row) = row else {
        return Ok(None);
    };

    let mut note = note_from_row(&row)?;
    note.entities = entities_of(client, note.id)?;
    Ok(Some(note))
}

/// The note `id` when it is visible in `view`.
pub(super) fn load(client: &mut impl GenericClient, view: &View, id: i64) -> Result<Option<Note>, StoreError> {
    let (repo, global) = view_params(client, view)?;
    let sql = format!(
        "SELECT {} FROM notes n LEFT JOIN projects p ON p.id = n.project_id
         WHERE n.id = $1 AND {VISIBLE}",
        note_columns()
    );

    let row = client.query_opt(&sql, &[&id, &repo, &global]).map_err(backend)?;

    let Some(row) = row else {
        return Ok(None);
    };

    let mut note = note_from_row(&row)?;
    note.entities = entities_of(client, id)?;
    Ok(Some(note))
}

/// The names of the entities linked to note `id`, alphabetically (by code point, like SQLite).
pub(super) fn entities_of(client: &mut impl GenericClient, id: i64) -> Result<Vec<String>, StoreError> {
    let sql = format!(
        "SELECT {} FROM note_entities ne JOIN entities e ON e.id = ne.entity_id
         WHERE ne.note_id = $1 ORDER BY e.name_norm COLLATE \"C\"",
        cut("e.name", ENTITY_MAX)
    );

    let rows = client.query(&sql, &[&id]).map_err(backend)?;
    rows.iter().map(|row| column(row, 0)).collect()
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

/// A row of [`note_columns`], without its entities.
fn note_from_row(row: &Row) -> Result<Note, StoreError> {
    let tags: String = column(row, 6)?;
    let kind: String = column(row, 3)?;
    let root: Option<String> = column(row, 12)?;

    Ok(Note {
        id: column(row, 0)?,
        scope: scope_of(column(row, 1)?),
        project: column(row, 2)?,
        kind: kind_from_sql(&kind),
        title: column(row, 4)?,
        body: column(row, 5)?,
        tags: tags.split_whitespace().map(str::to_owned).collect(),
        entities: Vec::new(),
        provenance: Provenance {
            root: root.map(PathBuf::from),
            source: column(row, 7)?,
            via_override: column(row, 13)?,
        },
        created_at: column(row, 8)?,
        updated_at: column(row, 9)?,
        revision: column(row, 10)?,
        hits: column(row, 11)?,
    })
}

/// Fails when another note in the same scope already has `title_norm`.
fn ensure_title_free(tx: &mut Transaction<'_>, note: &Note, title_norm: &str) -> Result<(), StoreError> {
    let other = tx
        .query_opt(
            "SELECT other.id FROM notes other JOIN notes this ON this.id = $1
             WHERE coalesce(other.project_id, 0) = coalesce(this.project_id, 0)
               AND other.title_norm = $2 AND other.id != $1",
            &[&note.id, &title_norm],
        )
        .map_err(backend)?;

    let Some(row) = other else {
        return Ok(());
    };

    let id: i64 = column(&row, 0)?;
    Err(StoreError::Invalid(format!(
        "note {id} already has this title; update that note or forget one of them"
    )))
}

fn saved(tx: &mut Transaction<'_>, id: i64, created: bool) -> Result<Saved, StoreError> {
    let sql = format!(
        "SELECT n.revision, n.project_id, {} FROM notes n LEFT JOIN projects p ON p.id = n.project_id
         WHERE n.id = $1",
        cut("p.key", PATH_MAX)
    );
    let row = tx.query_one(&sql, &[&id]).map_err(backend)?;

    Ok(Saved {
        id,
        created,
        revision: column(&row, 0)?,
        scope: scope_of(column(&row, 1)?),
        project: column(&row, 2)?,
    })
}

fn not_found(id: i64) -> StoreError {
    StoreError::NotFound(format!("note {id} not found in this repository or the global scope"))
}
