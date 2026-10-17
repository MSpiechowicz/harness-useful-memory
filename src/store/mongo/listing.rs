//! A page of notes ([`crate::store::listing`]) in MongoDB: two aggregations, the counts per kind
//! and scope and the page (sorted, skipped and cut on the server, the bodies left there), then
//! the entity names of the page's notes, one query per scope on the page.

use mongodb::bson::{Bson, Document, doc};

use super::notes::{Stored, stored};
use super::scripts::literal_pattern;
use super::{Ctx, ENTITIES, ENTITY_NORM_MAX, Fail, NOTES, cut, int, note_fields, scope_value, text, visible};
use crate::limits::ENTITY_MAX;
use crate::store::listing::{NoteListQuery, NotePage, NoteRow, NoteSort, page_limit, query_entity, query_text, scoped};
use crate::store::{NoteKind, Scope, View};

pub(super) fn list(ctx: &mut Ctx<'_>, query: &NoteListQuery) -> Result<NotePage, Fail> {
    let text = query_text(query)?;
    let mut kept = Document::new();

    if let Some(text) = text {
        let pattern = literal_pattern(text);
        let matches = doc! { "$regex": &pattern, "$options": "i" };

        kept.insert(
            "$or",
            vec![
                doc! { "title": matches.clone() },
                doc! { "body": matches.clone() },
                doc! { "tags": matches },
            ],
        );
    }

    if let Some(entity) = query_entity(query) {
        kept.insert("entities", entity);
    }

    let mut counts_filter = visible(&query.view);
    counts_filter.extend(kept.clone());

    // A view shows one repository at most, so a group's project tells its scope: a key, or
    // `null` (or no project at all) for a global note.
    let counted: Vec<(NoteKind, Scope, usize)> = ctx
        .aggregate(
            NOTES,
            vec![
                doc! { "$match": counts_filter },
                doc! { "$group": {
                    "_id": { "kind": "$kind", "project": "$project" },
                    "count": { "$sum": 1_i64 },
                } },
            ],
        )?
        .iter()
        .map(|group| {
            let id = group.get_document("_id").ok();
            let kind = id
                .and_then(|id| id.get_str("kind").ok())
                .and_then(NoteKind::parse)
                .unwrap_or(NoteKind::Note);
            let scope = match id.and_then(|id| id.get_str("project").ok()) {
                Some(_) => Scope::Repo,
                None => Scope::Global,
            };
            (kind, scope, usize::try_from(int(group, "count")).unwrap_or(0))
        })
        .collect();

    let (repo, global) = scoped(query, query.view.repo.clone(), query.view.global);
    let mut page_filter = visible(&View { repo, global });
    page_filter.extend(kept);

    if let Some(kind) = query.kind {
        page_filter.insert("kind", kind.as_str());
    }

    let way = if query.descending { -1_i32 } else { 1_i32 };
    // A global note's `project` is `null`, which sorts before a repository key.
    let sort = match query.sort {
        NoteSort::Updated => doc! { "updated_at": way, "_id": way },
        NoteSort::Title => doc! { "title_norm": way, "_id": way },
        NoteSort::Kind => doc! { "kind": way, "_id": way },
        NoteSort::Scope => doc! { "project": way, "_id": way },
        NoteSort::Place => doc! { "_id": way },
    };

    let mut fields = note_fields();
    fields.remove("body");

    let found = ctx.aggregate(
        NOTES,
        vec![
            doc! { "$match": page_filter },
            doc! { "$sort": sort },
            doc! { "$skip": i64::try_from(query.offset).unwrap_or(i64::MAX) },
            doc! { "$limit": i64::try_from(page_limit(query)).unwrap_or(1) },
            doc! { "$project": fields },
        ],
    )?;

    let stored: Vec<Stored> = found.iter().map(stored).collect();
    let rows = with_entities(ctx, stored)?;
    Ok(NotePage::new(counted, query, rows))
}

/// The rows of `stored` with their entity names (alphabetically by normalized name, as a
/// note's), read with one query per scope.
fn with_entities(ctx: &mut Ctx<'_>, stored: Vec<Stored>) -> Result<Vec<NoteRow>, Fail> {
    let mut scopes: Vec<Option<String>> = Vec::new();

    for Stored { note, norms } in &stored {
        if !norms.is_empty() && !scopes.contains(&note.project) {
            scopes.push(note.project.clone());
        }
    }

    // `(scope, normalized name, name)`, in normalized-name order.
    let mut names: Vec<(Option<String>, String, String)> = Vec::new();

    for scope in scopes {
        let norms: Vec<Bson> = stored
            .iter()
            .filter(|row| row.note.project == scope)
            .flat_map(|row| row.norms.iter().cloned().map(Bson::String))
            .collect();

        let found = ctx.find(
            ENTITIES,
            doc! { "project": scope_value(scope.as_deref()), "name_norm": { "$in": norms } },
            doc! { "name_norm": 1 },
            0,
            doc! { "_id": 0, "name_norm": cut("$name_norm", ENTITY_NORM_MAX), "name": cut("$name", ENTITY_MAX) },
        )?;

        names.extend(
            found.iter().filter_map(|entity: &Document| {
                Some((scope.clone(), text(entity, "name_norm")?, text(entity, "name")?))
            }),
        );
    }

    Ok(stored
        .into_iter()
        .map(|Stored { mut note, norms }| {
            note.entities = names
                .iter()
                .filter(|(scope, norm, _)| *scope == note.project && norms.contains(norm))
                .map(|(_, _, name)| name.clone())
                .collect();
            NoteRow::from(note)
        })
        .collect())
}
