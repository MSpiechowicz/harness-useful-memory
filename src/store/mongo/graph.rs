//! Entities and relations: linking, a bounded walk around an entity, and deletion.
//!
//! An entity is identified by its scope and normalized name; a relation stores the normalized
//! names of its ends with their spellings. The walk is a breadth-first search, one query per hop,
//! that reads at most [`WALK_RELATIONS_MAX`] relations.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};

use mongodb::bson::{Bson, Document, doc, oid::ObjectId};

use super::{Ctx, ENTITIES, ENTITY_NORM_MAX, Fail, NOTES, PATH_MAX, RELATIONS, cut, id_only, int, now, number};
use super::{project_of, scope_of, scope_value, text, visible};
use crate::limits::{DEPTH_MAX, ENTITY_MAX, NEIGHBOR_NOTES_MAX, NEIGHBORS_LIMIT_MAX, RELATION_MAX, TITLE_MAX};
use crate::store::{Edge, Forget, Link, Neighborhood, StoreError, Target, View, clean};

/// Relations one walk reads at most; beyond it the walk stops and `more_edges` counts only what it read.
const WALK_RELATIONS_MAX: usize = 5000;

/// An entity as [`ensure_entity`] returns it.
pub(super) struct Entity {
    pub norm: String,
    pub name: String,
}

/// An entity in the walk: its scope and normalized name.
type Node = (Option<String>, String);

/// The entity `name` in the scope `project`, created when missing (the first spelling is kept).
pub(super) fn ensure_entity(ctx: &mut Ctx<'_>, project: Option<&str>, name: &str) -> Result<Entity, Fail> {
    let norm = clean::normalize(name);

    let entity = ctx.find_one_and_update(
        ENTITIES,
        doc! { "project": scope_value(project), "name_norm": &norm },
        doc! { "$setOnInsert": { "name": name } },
        true,
        true,
        doc! { "_id": 0, "name": cut("$name", ENTITY_MAX) },
    )?;

    let name = entity
        .as_ref()
        .and_then(|entity| text(entity, "name"))
        .unwrap_or_else(|| name.to_owned());

    Ok(Entity { norm, name })
}

/// Deletes the entity `norm` of the scope `project` when no note and no relation uses it any more.
pub(super) fn prune_entity(ctx: &mut Ctx<'_>, project: Option<&str>, norm: &str) -> Result<(), Fail> {
    let scope = scope_value(project);

    if ctx.exists(NOTES, doc! { "project": scope.clone(), "entities": norm })? {
        return Ok(());
    }

    let touching = doc! { "project": scope.clone(), "$or": [{ "src": norm }, { "dst": norm }] };
    if ctx.exists(RELATIONS, touching)? {
        return Ok(());
    }

    ctx.delete_one(ENTITIES, doc! { "project": scope, "name_norm": norm })?;
    Ok(())
}

/// Deletes the entities of the scope `project` that no relation uses, once its notes are gone.
pub(super) fn prune_scope(ctx: &mut Ctx<'_>, project: Option<&str>) -> Result<(), Fail> {
    let scope = scope_value(project);
    let relations = ctx.find(
        RELATIONS,
        doc! { "project": scope.clone() },
        doc! {},
        0,
        doc! { "src": cut("$src", ENTITY_NORM_MAX), "dst": cut("$dst", ENTITY_NORM_MAX) },
    )?;

    let mut used: Vec<String> = relations
        .iter()
        .flat_map(|relation| [text(relation, "src"), text(relation, "dst")])
        .flatten()
        .collect();
    used.sort_unstable();
    used.dedup();

    ctx.delete_many(ENTITIES, doc! { "project": scope, "name_norm": { "$nin": used } })?;
    Ok(())
}

/// `link` with its names and label cleaned and checked (before any request).
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

/// `what` with its names and label cleaned and checked (before any request).
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

/// Records a [`prepare`]d link; the note of an existing relation is kept when `link` names none.
pub(super) fn link(ctx: &mut Ctx<'_>, target: &Target, link: &Link) -> Result<Edge, Fail> {
    let project = project_of(target);
    let scope = scope_value(project.as_deref());

    if let Some(note) = link.note_id {
        let exists = ctx.find_one(NOTES, doc! { "_id": note, "project": scope.clone() }, id_only())?;

        if exists.is_none() {
            return Err(Fail::Store(StoreError::NotFound(format!(
                "note {note} not found in this scope"
            ))));
        }
    }

    let src = ensure_entity(ctx, project.as_deref(), &link.from)?;
    let dst = ensure_entity(ctx, project.as_deref(), &link.to)?;
    let now = now();

    let mut set = doc! { "updated_at": now };
    let mut on_insert = doc! { "created_at": now, "src_name": &src.name, "dst_name": &dst.name };

    match link.note_id {
        Some(note) => set.insert("note_id", note),
        None => on_insert.insert("note_id", Bson::Null),
    };

    let relation = ctx.find_one_and_update(
        RELATIONS,
        doc! { "project": scope, "src": &src.norm, "rel": &link.relation, "dst": &dst.norm },
        doc! { "$set": set, "$setOnInsert": on_insert },
        true,
        true,
        relation_fields(),
    )?;

    match relation {
        Some(relation) => Ok(edge(&relation)),
        None => Err(Fail::Retry),
    }
}

pub(super) fn neighbors(
    ctx: &mut Ctx<'_>,
    view: &View,
    entity: &str,
    depth: u8,
    limit: usize,
) -> Result<Neighborhood, Fail> {
    let name = clean::entity(entity)?;
    let norm = clean::normalize(&name);
    let depth = depth.clamp(1, DEPTH_MAX);
    let limit = limit.clamp(1, NEIGHBORS_LIMIT_MAX);

    let mut filter = visible(view);
    filter.insert("name_norm", &norm);
    let starts = ctx.find(
        ENTITIES,
        filter,
        doc! { "_id": 1 },
        0,
        doc! { "_id": 0, "project": cut("$project", PATH_MAX) },
    )?;

    if starts.is_empty() {
        return Err(Fail::Store(StoreError::NotFound(format!(
            "entity \"{name}\" not found; memory_search may find related notes"
        ))));
    }

    let mut reached: HashMap<Node, u8> = HashMap::new();
    let mut frontier: Vec<Node> = Vec::new();

    for start in &starts {
        let node = (text(start, "project"), norm.clone());
        if reached.insert(node.clone(), 0).is_none() {
            frontier.push(node);
        }
    }

    let mut relations: Vec<Document> = Vec::new();
    let mut seen: HashSet<Option<ObjectId>> = HashSet::new();

    for hop in 0..depth {
        if frontier.is_empty() || relations.len() >= WALK_RELATIONS_MAX {
            break;
        }

        let room = WALK_RELATIONS_MAX - relations.len();
        let found = ctx.find(
            RELATIONS,
            touching(&frontier),
            doc! { "_id": 1 },
            i64::try_from(room).unwrap_or(i64::MAX),
            relation_fields(),
        )?;

        let mut next: Vec<Node> = Vec::new();

        for relation in found {
            let project = text(&relation, "project");

            for end in ["src", "dst"] {
                let node = (project.clone(), text(&relation, end).unwrap_or_default());

                if !reached.contains_key(&node) {
                    reached.insert(node.clone(), hop + 1);
                    next.push(node);
                }
            }

            // A relation found on an earlier hop (from its other end) is listed once.
            if seen.insert(relation.get_object_id("_id").ok()) {
                relations.push(relation);
            }
        }

        frontier = next;
    }

    // Closest first (the nearer end's distance), then the most recently updated.
    let distance = |relation: &Document| {
        let project = text(relation, "project");

        ["src", "dst"]
            .into_iter()
            .map(|end| {
                let node = (project.clone(), text(relation, end).unwrap_or_default());
                reached.get(&node).copied().unwrap_or(depth)
            })
            .min()
            .unwrap_or(depth)
    };

    relations.sort_by_key(|relation| {
        (
            distance(relation),
            Reverse(int(relation, "updated_at")),
            relation.get_object_id("_id").ok(),
        )
    });

    let notes = linked_notes(ctx, &reached, &relations)?;
    let total = relations.len();
    let edges: Vec<Edge> = relations.iter().take(limit).map(edge).collect();

    Ok(Neighborhood {
        more_edges: total.saturating_sub(edges.len()),
        edges,
        notes,
    })
}

/// Deletes the entity (a [`prepare_forget`]ed name), its relations and the entities they leave
/// unused, and unlinks it from its notes; returns 1 (the entity) plus the number of its relations.
pub(super) fn forget_entity(ctx: &mut Ctx<'_>, target: &Target, name: &str) -> Result<usize, Fail> {
    let project = project_of(target);
    let scope = scope_value(project.as_deref());
    let norm = clean::normalize(name);

    let entity = ctx.find_one(
        ENTITIES,
        doc! { "project": scope.clone(), "name_norm": &norm },
        doc! { "_id": 1 },
    )?;
    if entity.is_none() {
        return Err(Fail::Store(StoreError::NotFound(format!(
            "entity \"{name}\" not found in this scope"
        ))));
    }

    let touching = doc! { "project": scope.clone(), "$or": [{ "src": &norm }, { "dst": &norm }] };
    let ends = doc! { "_id": 0, "src": cut("$src", ENTITY_NORM_MAX), "dst": cut("$dst", ENTITY_NORM_MAX) };
    let relations = ctx.find(RELATIONS, touching.clone(), doc! { "_id": 1 }, 0, ends)?;

    let others: Vec<String> = relations
        .iter()
        .filter_map(|relation| {
            let src = text(relation, "src")?;
            let dst = text(relation, "dst")?;
            Some(if src == norm { dst } else { src })
        })
        .collect();

    ctx.delete_one(ENTITIES, doc! { "project": scope.clone(), "name_norm": &norm })?;
    ctx.delete_many(RELATIONS, touching)?;
    ctx.update_many(
        NOTES,
        doc! { "project": scope, "entities": &norm },
        doc! { "$pull": { "entities": &norm } },
    )?;

    // Entities left without notes or relations go with it.
    for other in &others {
        prune_entity(ctx, project.as_deref(), other)?;
    }

    Ok(1 + relations.len())
}

/// Deletes one relation (a [`prepare_forget`]ed one) and the entities it leaves unused.
pub(super) fn forget_relation(
    ctx: &mut Ctx<'_>,
    target: &Target,
    from: &str,
    relation: &str,
    to: &str,
) -> Result<usize, Fail> {
    let missing = || {
        Fail::Store(StoreError::NotFound(format!(
            "relation \"{from} {relation} {to}\" not found in this scope"
        )))
    };

    let project = project_of(target);
    let scope = scope_value(project.as_deref());
    let src = clean::normalize(from);
    let dst = clean::normalize(to);

    for end in [&src, &dst] {
        if !ctx.exists(ENTITIES, doc! { "project": scope.clone(), "name_norm": end })? {
            return Err(missing());
        }
    }

    let deleted = ctx.delete_one(
        RELATIONS,
        doc! { "project": scope, "src": &src, "rel": relation, "dst": &dst },
    )?;

    if deleted == 0 {
        return Err(missing());
    }

    prune_entity(ctx, project.as_deref(), &src)?;
    prune_entity(ctx, project.as_deref(), &dst)?;
    Ok(1)
}

/// The filter of the relations touching any of `nodes`.
fn touching(nodes: &[Node]) -> Document {
    let mut by_scope: BTreeMap<Option<String>, Vec<String>> = BTreeMap::new();

    for (project, norm) in nodes {
        by_scope.entry(project.clone()).or_default().push(norm.clone());
    }

    let clauses: Vec<Document> = by_scope
        .into_iter()
        .map(|(project, norms)| {
            doc! {
                "project": scope_value(project.as_deref()),
                "$or": [{ "src": { "$in": norms.clone() } }, { "dst": { "$in": norms } }],
            }
        })
        .collect();

    doc! { "$or": clauses }
}

/// Notes linked to the reached entities or to the walked relations: `(id, title)`, most recently
/// updated first.
fn linked_notes(
    ctx: &mut Ctx<'_>,
    reached: &HashMap<Node, u8>,
    relations: &[Document],
) -> Result<Vec<(i64, String)>, Fail> {
    let mut by_scope: BTreeMap<Option<String>, Vec<String>> = BTreeMap::new();

    for (project, norm) in reached.keys() {
        by_scope.entry(project.clone()).or_default().push(norm.clone());
    }

    let mut clauses: Vec<Document> = by_scope
        .into_iter()
        .map(|(project, norms)| doc! { "project": scope_value(project.as_deref()), "entities": { "$in": norms } })
        .collect();

    let note_ids: Vec<i64> = relations
        .iter()
        .filter(|relation| matches!(relation.get("note_id"), Some(Bson::Int64(_) | Bson::Int32(_))))
        .map(|relation| int(relation, "note_id"))
        .collect();

    if !note_ids.is_empty() {
        clauses.push(doc! { "_id": { "$in": note_ids } });
    }

    let limit = i64::try_from(NEIGHBOR_NOTES_MAX).unwrap_or(10);
    let found = ctx.find(
        NOTES,
        doc! { "$or": clauses },
        doc! { "updated_at": -1, "_id": -1 },
        limit,
        doc! { "_id": number("$_id"), "title": cut("$title", TITLE_MAX), "updated_at": number("$updated_at") },
    )?;

    Ok(found
        .iter()
        .map(|note| (int(note, "_id"), text(note, "title").unwrap_or_default()))
        .collect())
}

/// The fields of a relation the walk and an [`Edge`] need, each bounded; `_id` is kept only when
/// it is an `ObjectId` (as this server writes it) and `note_id` only when it is a number.
fn relation_fields() -> Document {
    doc! {
        "_id": { "$cond": [{ "$eq": [{ "$type": "$_id" }, "objectId"] }, "$_id", Bson::Null] },
        "project": cut("$project", PATH_MAX),
        "src": cut("$src", ENTITY_NORM_MAX),
        "rel": cut("$rel", RELATION_MAX),
        "dst": cut("$dst", ENTITY_NORM_MAX),
        "src_name": cut("$src_name", ENTITY_MAX),
        "dst_name": cut("$dst_name", ENTITY_MAX),
        "note_id": { "$cond": [{ "$isNumber": "$note_id" }, "$note_id", Bson::Null] },
        "updated_at": number("$updated_at"),
    }
}

fn edge(relation: &Document) -> Edge {
    let note_id = match relation.get("note_id") {
        Some(Bson::Int64(_) | Bson::Int32(_)) => Some(int(relation, "note_id")),
        _ => None,
    };

    Edge {
        from: text(relation, "src_name").unwrap_or_default(),
        relation: text(relation, "rel").unwrap_or_default(),
        to: text(relation, "dst_name").unwrap_or_default(),
        scope: scope_of(text(relation, "project").as_deref()),
        note_id,
    }
}
