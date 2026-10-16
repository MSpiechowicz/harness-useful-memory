//! Entities and relations: linking, a bounded walk around an entity, and deletion.
//!
//! Relations join two entities of the same scope. The walk is a recursive CTE over
//! `(entity, depth)` pairs combined with `UNION`, bounded by the depth, so cycles end it.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::notes::scope_of;
use super::{now, target_project, target_project_existing, view_params};
use crate::limits::{DEPTH_MAX, NEIGHBOR_NOTES_MAX, NEIGHBORS_LIMIT_MAX};
use crate::store::{Edge, Forget, Link, Neighborhood, StoreError, Target, View, clean};

/// The entities within `?4` hops of the entity named `?1` in the view (`?2`, `?3`), each with its
/// shortest distance.
const REACHED: &str = "
WITH RECURSIVE
    start(id) AS (
        SELECT n.id FROM entities n
        WHERE n.name_norm = ?1 AND (n.project_id = ?2 OR (?3 AND n.project_id IS NULL))
    ),
    walk(id, depth) AS (
        SELECT id, 0 FROM start
        UNION
        SELECT CASE WHEN r.src = w.id THEN r.dst ELSE r.src END, w.depth + 1
        FROM walk w JOIN relations r ON r.src = w.id OR r.dst = w.id
        WHERE w.depth < ?4
    ),
    reached(id, depth) AS (SELECT id, min(depth) FROM walk GROUP BY id)
";

/// The id of the entity `name` in the scope `project`, created when missing.
pub(super) fn ensure_entity(tx: &Transaction<'_>, project: Option<i64>, name: &str) -> Result<i64, StoreError> {
    let norm = clean::normalize(name);

    tx.execute(
        "INSERT INTO entities (project_id, name, name_norm) VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
        params![project, name, norm],
    )?;

    let id = tx.query_row(
        "SELECT id FROM entities WHERE ifnull(project_id, 0) = ?1 AND name_norm = ?2",
        params![project.unwrap_or(0), norm],
        |row| row.get(0),
    )?;

    Ok(id)
}

/// Deletes entity `id` when no note and no relation uses it any more.
pub(super) fn prune_entity(tx: &Transaction<'_>, id: i64) -> Result<(), StoreError> {
    tx.execute(
        "DELETE FROM entities WHERE id = ?1
           AND NOT EXISTS (SELECT 1 FROM note_entities WHERE entity_id = ?1)
           AND NOT EXISTS (SELECT 1 FROM relations WHERE src = ?1 OR dst = ?1)",
        [id],
    )?;

    Ok(())
}

/// `link` with its names and label cleaned and checked (before the write transaction opens).
pub(super) fn prepare(link: &Link) -> Result<Link, StoreError> {
    let from = clean::entity(&link.from)?;
    let to = clean::entity(&link.to)?;

    if clean::normalize(&from) == clean::normalize(&to) {
        return Err(StoreError::Invalid("an entity cannot be linked to itself".to_owned()));
    }

    Ok(Link {
        from,
        relation: clean::relation(&link.relation)?,
        to,
        note_id: link.note_id,
    })
}

/// `what` with its names and label cleaned and checked (before the write transaction opens).
pub(super) fn prepare_forget(what: &Forget) -> Result<Forget, StoreError> {
    Ok(match what {
        Forget::Note(id) => Forget::Note(*id),
        Forget::Entity(name) => Forget::Entity(clean::entity(name)?),
        Forget::Relation { from, relation, to } => Forget::Relation {
            from: clean::entity(from)?,
            relation: clean::relation(relation)?,
            to: clean::entity(to)?,
        },
    })
}

/// Records a [`prepare`]d link.
pub(super) fn link(tx: &Transaction<'_>, target: &Target, link: &Link) -> Result<Edge, StoreError> {
    let Link {
        from,
        relation,
        to,
        note_id: _,
    } = link;

    let project = target_project(tx, target)?;

    if let Some(note) = link.note_id {
        let exists: Option<i64> = tx
            .query_row(
                "SELECT id FROM notes WHERE id = ?1 AND ifnull(project_id, 0) = ?2",
                params![note, project.unwrap_or(0)],
                |row| row.get(0),
            )
            .optional()?;

        if exists.is_none() {
            return Err(StoreError::NotFound(format!("note {note} not found in this scope")));
        }
    }

    let src = ensure_entity(tx, project, from)?;
    let dst = ensure_entity(tx, project, to)?;

    tx.execute(
        "INSERT INTO relations (project_id, src, rel, dst, note_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT (src, rel, dst) DO UPDATE SET
             note_id = coalesce(excluded.note_id, relations.note_id), updated_at = excluded.updated_at",
        params![project, src, relation, dst, link.note_id, now()],
    )?;

    let edge = tx.query_row(
        "SELECT s.name, r.rel, d.name, r.project_id, r.note_id FROM relations r
         JOIN entities s ON s.id = r.src JOIN entities d ON d.id = r.dst
         WHERE r.src = ?1 AND r.rel = ?2 AND r.dst = ?3",
        params![src, relation, dst],
        |row| {
            Ok(Edge {
                from: row.get(0)?,
                relation: row.get(1)?,
                to: row.get(2)?,
                scope: scope_of(row.get(3)?),
                note_id: row.get(4)?,
            })
        },
    )?;

    Ok(edge)
}

pub(super) fn neighbors(
    conn: &Connection,
    view: &View,
    entity: &str,
    depth: u8,
    limit: usize,
) -> Result<Neighborhood, StoreError> {
    let name = clean::entity(entity)?;
    let norm = clean::normalize(&name);
    let (repo, global) = view_params(conn, view)?;
    let depth = depth.clamp(1, DEPTH_MAX);
    let limit = i64::try_from(limit.clamp(1, NEIGHBORS_LIMIT_MAX)).unwrap_or(1);

    let start: Option<i64> = conn
        .query_row(
            "SELECT n.id FROM entities n
             WHERE n.name_norm = ?1 AND (n.project_id = ?2 OR (?3 AND n.project_id IS NULL)) LIMIT 1",
            params![norm, repo, global],
            |row| row.get(0),
        )
        .optional()?;

    if start.is_none() {
        return Err(StoreError::NotFound(format!(
            "entity \"{name}\" not found; memory_search may find related notes"
        )));
    }

    let edges_sql = format!(
        "{REACHED}
         SELECT s.name, r.rel, d.name, r.project_id, r.note_id, count(*) OVER () AS total
         FROM relations r
         JOIN entities s ON s.id = r.src
         JOIN entities d ON d.id = r.dst
         LEFT JOIN reached rs ON rs.id = r.src
         LEFT JOIN reached rd ON rd.id = r.dst
         WHERE rs.depth < ?4 OR rd.depth < ?4
         ORDER BY min(ifnull(rs.depth, ?4), ifnull(rd.depth, ?4)), r.updated_at DESC, r.id
         LIMIT ?5"
    );

    let mut total = 0usize;
    let mut edges: Vec<Edge> = Vec::new();
    let mut statement = conn.prepare(&edges_sql)?;
    let mut rows = statement.query(params![norm, repo, global, depth, limit])?;

    while let Some(row) = rows.next()? {
        total = usize::try_from(row.get::<_, i64>(5)?).unwrap_or(0);
        edges.push(Edge {
            from: row.get(0)?,
            relation: row.get(1)?,
            to: row.get(2)?,
            scope: scope_of(row.get(3)?),
            note_id: row.get(4)?,
        });
    }

    let notes_sql = format!(
        "{REACHED}
         SELECT n.id, n.title FROM notes n
         WHERE n.id IN (SELECT note_id FROM note_entities WHERE entity_id IN (SELECT id FROM reached))
            OR n.id IN (SELECT r.note_id FROM relations r JOIN reached ON reached.id = r.src WHERE reached.depth < ?4)
            OR n.id IN (SELECT r.note_id FROM relations r JOIN reached ON reached.id = r.dst WHERE reached.depth < ?4)
         ORDER BY n.updated_at DESC
         LIMIT ?5"
    );

    let notes = conn
        .prepare(&notes_sql)?
        .query_map(params![norm, repo, global, depth, NEIGHBOR_NOTES_MAX as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<Result<_, _>>()?;

    Ok(Neighborhood {
        more_edges: total.saturating_sub(edges.len()),
        edges,
        notes,
    })
}

/// Deletes the entity (a [`prepare_forget`]ed name), its relations and the entities they leave
/// unused; returns 1 (the entity) plus the number of its relations.
pub(super) fn forget_entity(tx: &Transaction<'_>, target: &Target, name: &str) -> Result<usize, StoreError> {
    let Some(id) = entity_id(tx, target, name)? else {
        return Err(StoreError::NotFound(format!(
            "entity \"{name}\" not found in this scope"
        )));
    };

    let others: Vec<i64> = tx
        .prepare("SELECT CASE WHEN src = ?1 THEN dst ELSE src END FROM relations WHERE src = ?1 OR dst = ?1")?
        .query_map([id], |row| row.get(0))?
        .collect::<Result<_, _>>()?;

    tx.execute("DELETE FROM entities WHERE id = ?1", [id])?;

    // Entities left without notes or relations go with it.
    for other in &others {
        prune_entity(tx, *other)?;
    }

    Ok(1 + others.len())
}

/// Deletes one relation (a [`prepare_forget`]ed one) and the entities it leaves unused.
pub(super) fn forget_relation(
    tx: &Transaction<'_>,
    target: &Target,
    from: &str,
    relation: &str,
    to: &str,
) -> Result<usize, StoreError> {
    let missing = || StoreError::NotFound(format!("relation \"{from} {relation} {to}\" not found in this scope"));

    let (Some(src), Some(dst)) = (entity_id(tx, target, from)?, entity_id(tx, target, to)?) else {
        return Err(missing());
    };

    let deleted = tx.execute(
        "DELETE FROM relations WHERE src = ?1 AND rel = ?2 AND dst = ?3",
        params![src, relation, dst],
    )?;

    if deleted == 0 {
        return Err(missing());
    }

    prune_entity(tx, src)?;
    prune_entity(tx, dst)?;
    Ok(deleted)
}

/// The id of entity `name` in the scope `target`.
fn entity_id(conn: &Connection, target: &Target, name: &str) -> Result<Option<i64>, StoreError> {
    let Some(project) = target_project_existing(conn, target)? else {
        return Ok(None);
    };

    let id = conn
        .query_row(
            "SELECT id FROM entities WHERE ifnull(project_id, 0) = ?1 AND name_norm = ?2",
            params![project.unwrap_or(0), clean::normalize(name)],
            |row| row.get(0),
        )
        .optional()?;

    Ok(id)
}
