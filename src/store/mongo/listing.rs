//! A page of notes ([`crate::store::listing`]) in MongoDB: the aggregations of the counts per kind
//! and scope and of the page (sorted, skipped and cut on the server, the bodies left there), then
//! the entity names of the page's notes, one query per scope on the page, and the aggregation of
//! who added the notes.

use mongodb::bson::{Bson, Document, doc};

use super::notes::{Stored, stored};
use super::scripts::literal_pattern;
use super::{
    Ctx, ENTITIES, ENTITY_NORM_MAX, Fail, NOTES, cut, int, lang_is_fresh, note_fields, number, scope_value, text,
    visible,
};
use crate::limits::{AUTHOR_GROUPS_READ_MAX, ENTITY_MAX, TEAM_DOC_AUTHOR_MAX, TITLE_MAX};
use crate::store::author_list::{AuthorFacet, AuthorFilter};
use crate::store::listing::{
    LangFilter, NoteListQuery, NotePage, NoteRow, NoteSort, UsefulFilter, checked_exclude, page_limit, query_entity,
    query_text, scoped,
};
use crate::store::{NoteKind, Scope, View};

/// The longest normalized title read back (lowercasing can lengthen a title).
const TITLE_NORM_MAX: usize = 4 * TITLE_MAX;

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

    let exclude = checked_exclude(&query.exclude)?;

    if !exclude.is_empty() {
        kept.insert("_id", doc! { "$nin": exclude });
    }

    kept.extend(useful_match(query.useful));

    let mut counts_filter = visible(&query.view);
    counts_filter.extend(kept.clone());
    counts_filter.extend(author_match(&query.author));

    let mut counting = vec![doc! { "$match": counts_filter }];
    counting.extend(lang_stages(query.lang));
    counting.push(doc! { "$group": {
        "_id": { "kind": "$kind", "project": "$project" },
        "count": { "$sum": 1_i64 },
    } });

    // A view shows one repository at most, so a group's project tells its scope: a key, or
    // `null` (or no project at all) for a global note.
    let counted: Vec<(NoteKind, Scope, usize)> = ctx
        .aggregate(NOTES, counting)?
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

    // Who added the notes: everything the page keeps but the author.
    let mut facet = vec![doc! { "$match": page_filter.clone() }];
    facet.extend(lang_stages(query.lang));
    facet.extend(author_groups());

    page_filter.extend(author_match(&query.author));

    let way = if query.descending { -1_i32 } else { 1_i32 };
    // A global note's `project` is `null`, which sorts before a repository key.
    let sort = match query.sort {
        NoteSort::Updated => doc! { "updated_at": way, "_id": way },
        NoteSort::Title => doc! { "title_norm": way, "_id": way },
        NoteSort::Kind => doc! { "kind": way, "_id": way },
        NoteSort::Scope => doc! { "project": way, "_id": way },
        NoteSort::Place => doc! { "_id": way },
        NoteSort::Useful | NoteSort::LastUseful => doc! { USEFUL_ORDER: way, "_id": way },
        NoteSort::AddedBy => doc! { AUTHOR_RANK: 1, AUTHOR_ORDER: way, "_id": way },
    };

    // The page is sorted again at the end: Amazon DocumentDB keeps the order of a `$sort` only
    // when it is the last stage, so the fields it sorts on stay in the projection.
    let mut fields = note_fields();
    fields.remove("body");
    fields.insert("title_norm", cut("$title_norm", TITLE_NORM_MAX));

    let mut paging = vec![doc! { "$match": page_filter }];
    paging.extend(lang_stages(query.lang));

    // A note nobody rated has no counters: it sorts as 0, in a field of its own so the counters
    // read back stay as they are stored.
    if let Some(order) = useful_order(query.sort) {
        paging.push(doc! { "$addFields": { USEFUL_ORDER: order } });
        fields.insert(USEFUL_ORDER, 1);
    }

    // The notes without a name come last whichever the direction: they rank after the others.
    if query.sort == NoteSort::AddedBy {
        paging.push(doc! { "$addFields": {
            AUTHOR_RANK: { "$cond": [unknown_author(), 1, 0] },
            AUTHOR_ORDER: { "$toLower": cut("$added_by_name", TEAM_DOC_AUTHOR_MAX + 1) },
        } });
        fields.insert(AUTHOR_RANK, 1);
        fields.insert(AUTHOR_ORDER, 1);
    }

    paging.extend([
        doc! { "$sort": sort.clone() },
        doc! { "$skip": i64::try_from(query.offset).unwrap_or(i64::MAX) },
        doc! { "$limit": i64::try_from(page_limit(query)).unwrap_or(1) },
        doc! { "$project": fields },
        doc! { "$sort": sort },
    ]);

    let found = ctx.aggregate(NOTES, paging)?;

    let stored: Vec<Stored> = found.iter().map(stored).collect();
    let rows = with_entities(ctx, stored)?;

    let mut groups: Vec<(Option<String>, usize)> = Vec::new();

    if query.want_authors {
        for group in ctx.aggregate(NOTES, facet)? {
            groups.push((
                super::text(&group, "_id"),
                usize::try_from(int(&group, "count")).unwrap_or(0),
            ));
        }
    }

    Ok(NotePage::new(counted, query, rows).with_authors(AuthorFacet::from_groups(groups)))
}

/// The fields the page is sorted on for the sort by the person who added the notes.
const AUTHOR_RANK: &str = "author_rank";
const AUTHOR_ORDER: &str = "author_order";

/// The match that keeps the notes of `filter` (nothing for no filter). `$or` is the text search's
/// key, so unknown is `$in` of a missing name, `null` and blank, which also matches a missing field.
fn author_match(filter: &AuthorFilter) -> Document {
    match filter {
        AuthorFilter::Any => Document::new(),
        AuthorFilter::Named(name) => doc! { "added_by_name": name },
        AuthorFilter::Unknown => doc! { "added_by_name": { "$in": [Bson::Null, Bson::String(String::new())] } },
    }
}

/// True (as an aggregation expression) for a note with no name that is a string, or a blank one.
fn unknown_author() -> Bson {
    let name = cut("$added_by_name", TEAM_DOC_AUTHOR_MAX + 1);

    Bson::Document(doc! { "$or": [{ "$eq": [name.clone(), Bson::Null] }, { "$eq": [name, ""] }] })
}

/// The stages that count the notes per name of whoever added them, the unknown first and then the
/// most notes first, at most [`AUTHOR_GROUPS_READ_MAX`] groups. The name is cut to its limit by
/// the server and is `null` for a note without a string name.
fn author_groups() -> Vec<Document> {
    vec![
        doc! { "$group": {
            "_id": cut("$added_by_name", TEAM_DOC_AUTHOR_MAX + 1),
            "count": { "$sum": 1_i64 },
        } },
        doc! { "$addFields": { "unknown_first": { "$cond": [
            { "$or": [{ "$eq": ["$_id", Bson::Null] }, { "$eq": ["$_id", ""] }] },
            1,
            0,
        ] } } },
        doc! { "$sort": { "unknown_first": -1, "count": -1, "_id": 1 } },
        doc! { "$limit": i64::try_from(AUTHOR_GROUPS_READ_MAX).unwrap_or(i64::MAX) },
    ]
}

/// The field the page is sorted on for the sorts by usefulness.
const USEFUL_ORDER: &str = "useful_order";

/// The value a sort by usefulness orders by (`None` for the other sorts).
fn useful_order(sort: NoteSort) -> Option<Bson> {
    match sort {
        NoteSort::Useful => Some(number("$useful")),
        NoteSort::LastUseful => Some(number("$last_useful_at")),
        _ => None,
    }
}

/// The match that keeps the notes of `filter` (nothing for no filter). A note nobody rated has no
/// `useful` field, which counts as 0: `$not` of `$gte` also matches a missing field.
fn useful_match(filter: Option<UsefulFilter>) -> Document {
    match filter {
        None => Document::new(),
        Some(UsefulFilter::Ever) => doc! { "useful": { "$gte": 1_i64 } },
        Some(UsefulFilter::Never) => doc! {
            "shown": { "$gte": UsefulFilter::SHOWN_MIN },
            "useful": { "$not": { "$gte": 1_i64 } },
        },
    }
}

/// The stages that keep the notes of the language `filter` (none for no filter). A note whose
/// `lang` an older server's edit left stale (its `en_rev` is not its `revision`) counts as
/// unknown, so the three filters partition the notes the same way in the counts and on the page.
/// The freshness is computed in a stage of its own, not with `$expr` in the `$match`, which older
/// Amazon DocumentDB engines may lack.
fn lang_stages(filter: Option<LangFilter>) -> Vec<Document> {
    const FRESH: &str = "lang_fresh";

    let Some(filter) = filter else {
        return Vec::new();
    };

    let keep = match filter {
        LangFilter::En => doc! { "lang": "en", FRESH: true },
        LangFilter::Other => doc! { "lang": "other", FRESH: true },
        // Also matches a missing field and null.
        LangFilter::Unknown => doc! { "$or": [
            { "lang": { "$nin": ["en", "other"] } },
            { FRESH: false },
        ] },
    };

    vec![
        doc! { "$addFields": { FRESH: lang_is_fresh() } },
        doc! { "$match": keep },
    ]
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
