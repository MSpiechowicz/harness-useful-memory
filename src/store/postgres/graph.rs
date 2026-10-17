//! Entities and relations: linking, a bounded walk around an entity, and deletion.
//!
//! Relations join two entities of the same scope. The walk is a recursive CTE over
//! `(entity, depth)` pairs combined with `UNION`, bounded by the depth, so cycles end it.

use postgres::{GenericClient, Row, Transaction};

use super::notes::scope_of;
use super::{backend, column, cut, now, target_project, target_project_existing, view_params};
use crate::limits::{DEPTH_MAX, ENTITY_MAX, NEIGHBOR_NOTES_MAX, NEIGHBORS_LIMIT_MAX, RELATION_MAX, TITLE_MAX};
use crate::store::{Edge, Link, Neighborhood, StoreError, Target, View, clean};

/// The entities within `$4` hops of the entity named `$1` in the view (`$2`, `$3`), each with its
/// shortest distance.
const REACHED: &str = "
WITH RECURSIVE
    start(id) AS (
        SELECT n.id FROM entities n
        WHERE n.name_norm = $1 AND (n.project_id = $2::bigint OR ($3::boolean AND n.project_id IS NULL))
    ),
    walk(id, depth) AS (
        SELECT id, 0 FROM start
        UNION
        SELECT CASE WHEN r.src = w.id THEN r.dst ELSE r.src END, w.depth + 1
        FROM walk w JOIN relations r ON r.src = w.id OR r.dst = w.id
        WHERE w.depth < $4::integer
    ),
    reached(id, depth) AS (SELECT id, min(depth) FROM walk GROUP BY id)
";

/// The id of the entity `name` in the scope `project`, created when missing.
pub(super) fn ensure_entity(tx: &mut Transaction<'_>, project: Option<i64>, name: &str) -> Result<i64, StoreError> {
    let norm = clean::normalize(name);

    tx.execute(
        "INSERT INTO entities (project_id, name, name_norm) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        &[&project, &name, &norm],
    )
    .map_err(backend)?;

    let row = tx
        .query_one(
            "SELECT id FROM entities WHERE coalesce(project_id, 0) = $1 AND name_norm = $2",
            &[&project.unwrap_or(0), &norm],
        )
        .map_err(backend)?;

    column(&row, 0)
}

/// Deletes entity `id` when no note and no relation uses it any more.
pub(super) fn prune_entity(tx: &mut Transaction<'_>, id: i64) -> Result<(), StoreError> {
    tx.execute(
        "DELETE FROM entities WHERE id = $1
           AND NOT EXISTS (SELECT 1 FROM note_entities WHERE entity_id = $1)
           AND NOT EXISTS (SELECT 1 FROM relations WHERE src = $1 OR dst = $1)",
        &[&id],
    )
    .map_err(backend)?;

    Ok(())
}

/// Deletes the entities of the repository `repo` (when given) and of the global scope (when
/// `global`) that no note and no relation uses.
pub(super) fn prune_entities(tx: &mut Transaction<'_>, repo: Option<i64>, global: bool) -> Result<(), StoreError> {
    tx.execute(
        "DELETE FROM entities e WHERE (e.project_id = $1::bigint OR ($2::boolean AND e.project_id IS NULL))
           AND NOT EXISTS (SELECT 1 FROM note_entities ne WHERE ne.entity_id = e.id)
           AND NOT EXISTS (SELECT 1 FROM relations r WHERE r.src = e.id OR r.dst = e.id)",
        &[&repo, &global],
    )
    .map_err(backend)?;

    Ok(())
}

/// Records a link cleaned by [`clean::link`].
pub(super) fn link(tx: &mut Transaction<'_>, target: &Target, link: &Link) -> Result<Edge, StoreError> {
    let project = target_project(tx, target)?;

    if let Some(note) = link.note_id {
        let exists = tx
            .query_opt(
                "SELECT id FROM notes WHERE id = $1 AND coalesce(project_id, 0) = $2",
                &[&note, &project.unwrap_or(0)],
            )
            .map_err(backend)?;

        if exists.is_none() {
            return Err(StoreError::NotFound(format!("note {note} not found in this scope")));
        }
    }

    let src = ensure_entity(tx, project, &link.from)?;
    let dst = ensure_entity(tx, project, &link.to)?;

    tx.execute(
        "INSERT INTO relations (project_id, src, rel, dst, note_id, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $6)
         ON CONFLICT (src, rel, dst) DO UPDATE SET
             note_id = coalesce(excluded.note_id, relations.note_id), updated_at = excluded.updated_at",
        &[&project, &src, &link.relation, &dst, &link.note_id, &now()],
    )
    .map_err(backend)?;

    let sql = format!(
        "SELECT {} FROM relations r
         JOIN entities s ON s.id = r.src JOIN entities d ON d.id = r.dst
         WHERE r.src = $1 AND r.rel = $2 AND r.dst = $3",
        edge_columns()
    );
    let row = tx.query_one(&sql, &[&src, &link.relation, &dst]).map_err(backend)?;

    edge_from_row(&row)
}

/// The columns [`edge_from_row`] reads, from `relations r` joined to its entities `s` and `d`,
/// with the text cut to the limits (see [`cut`]).
fn edge_columns() -> String {
    format!(
        "{}, {}, {}, r.project_id, r.note_id",
        cut("s.name", ENTITY_MAX),
        cut("r.rel", RELATION_MAX),
        cut("d.name", ENTITY_MAX)
    )
}

fn edge_from_row(row: &Row) -> Result<Edge, StoreError> {
    Ok(Edge {
        from: column(row, 0)?,
        relation: column(row, 1)?,
        to: column(row, 2)?,
        scope: scope_of(column(row, 3)?),
        note_id: column(row, 4)?,
    })
}

pub(super) fn neighbors(
    client: &mut impl GenericClient,
    view: &View,
    entity: &str,
    depth: u8,
    limit: usize,
) -> Result<Neighborhood, StoreError> {
    let name = clean::entity(entity)?;
    let norm = clean::normalize(&name);
    let (repo, global) = view_params(client, view)?;
    let depth = i32::from(depth.clamp(1, DEPTH_MAX));
    let limit = i64::try_from(limit.clamp(1, NEIGHBORS_LIMIT_MAX)).unwrap_or(1);

    let start = client
        .query_opt(
            "SELECT n.id FROM entities n
             WHERE n.name_norm = $1 AND (n.project_id = $2::bigint OR ($3::boolean AND n.project_id IS NULL))
             LIMIT 1",
            &[&norm, &repo, &global],
        )
        .map_err(backend)?;

    if start.is_none() {
        return Err(StoreError::NotFound(format!(
            "entity \"{name}\" not found; memory_search may find related notes"
        )));
    }

    let edges_sql = format!(
        "{REACHED}
         SELECT {}, count(*) OVER () AS total
         FROM relations r
         JOIN entities s ON s.id = r.src
         JOIN entities d ON d.id = r.dst
         LEFT JOIN reached rs ON rs.id = r.src
         LEFT JOIN reached rd ON rd.id = r.dst
         WHERE rs.depth < $4::integer OR rd.depth < $4::integer
         ORDER BY least(coalesce(rs.depth, $4::integer), coalesce(rd.depth, $4::integer)), r.updated_at DESC, r.id
         LIMIT $5",
        edge_columns()
    );

    let rows = client
        .query(&edges_sql, &[&norm, &repo, &global, &depth, &limit])
        .map_err(backend)?;

    let total = match rows.first() {
        Some(row) => usize::try_from(column::<i64>(row, 5)?).unwrap_or(0),
        None => 0,
    };
    let edges = rows.iter().map(edge_from_row).collect::<Result<Vec<_>, _>>()?;

    let notes_sql = format!(
        "{REACHED}
         SELECT n.id, {} FROM notes n
         WHERE n.id IN (SELECT note_id FROM note_entities WHERE entity_id IN (SELECT id FROM reached))
            OR n.id IN (SELECT r.note_id FROM relations r JOIN reached ON reached.id = r.src
                        WHERE reached.depth < $4::integer)
            OR n.id IN (SELECT r.note_id FROM relations r JOIN reached ON reached.id = r.dst
                        WHERE reached.depth < $4::integer)
         ORDER BY n.updated_at DESC
         LIMIT $5",
        cut("n.title", TITLE_MAX)
    );

    let note_limit = i64::try_from(NEIGHBOR_NOTES_MAX).unwrap_or(10);
    let notes = client
        .query(&notes_sql, &[&norm, &repo, &global, &depth, &note_limit])
        .map_err(backend)?
        .iter()
        .map(|row| Ok((column(row, 0)?, column(row, 1)?)))
        .collect::<Result<Vec<_>, StoreError>>()?;

    Ok(Neighborhood {
        more_edges: total.saturating_sub(edges.len()),
        edges,
        notes,
    })
}

/// Deletes the entity (cleaned by [`clean::forget`]), its relations and the entities they leave
/// unused; returns 1 (the entity) plus the number of its relations.
pub(super) fn forget_entity(tx: &mut Transaction<'_>, target: &Target, name: &str) -> Result<usize, StoreError> {
    let Some(id) = entity_id(tx, target, name)? else {
        return Err(StoreError::NotFound(format!(
            "entity \"{name}\" not found in this scope"
        )));
    };

    let others = tx
        .query(
            "SELECT CASE WHEN src = $1 THEN dst ELSE src END FROM relations WHERE src = $1 OR dst = $1",
            &[&id],
        )
        .map_err(backend)?
        .iter()
        .map(|row| column::<i64>(row, 0))
        .collect::<Result<Vec<_>, _>>()?;

    tx.execute("DELETE FROM entities WHERE id = $1", &[&id])
        .map_err(backend)?;

    // Entities left without notes or relations go with it.
    for other in &others {
        prune_entity(tx, *other)?;
    }

    Ok(1 + others.len())
}

/// Deletes one relation (cleaned by [`clean::forget`]) and the entities it leaves unused.
pub(super) fn forget_relation(
    tx: &mut Transaction<'_>,
    target: &Target,
    from: &str,
    relation: &str,
    to: &str,
) -> Result<usize, StoreError> {
    let missing = || StoreError::NotFound(format!("relation \"{from} {relation} {to}\" not found in this scope"));

    let (Some(src), Some(dst)) = (entity_id(tx, target, from)?, entity_id(tx, target, to)?) else {
        return Err(missing());
    };

    let deleted = tx
        .execute(
            "DELETE FROM relations WHERE src = $1 AND rel = $2 AND dst = $3",
            &[&src, &relation, &dst],
        )
        .map_err(backend)?;

    if deleted == 0 {
        return Err(missing());
    }

    prune_entity(tx, src)?;
    prune_entity(tx, dst)?;
    Ok(usize::try_from(deleted).unwrap_or(usize::MAX))
}

/// The id of entity `name` in the scope `target`.
fn entity_id(client: &mut impl GenericClient, target: &Target, name: &str) -> Result<Option<i64>, StoreError> {
    let Some(project) = target_project_existing(client, target)? else {
        return Ok(None);
    };

    let row = client
        .query_opt(
            "SELECT id FROM entities WHERE coalesce(project_id, 0) = $1 AND name_norm = $2",
            &[&project.unwrap_or(0), &clean::normalize(name)],
        )
        .map_err(backend)?;

    row.map(|row| column(&row, 0)).transpose()
}
