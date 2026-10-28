//! A page of notes ([`crate::store::listing`]) in PostgreSQL: the counts per kind and scope and
//! the page, then the entity names of the page's notes. `strpos`, not `LIKE`, so `%` and `_` in
//! the text are plain characters; titles sort by code point (`COLLATE "C"`), like SQLite. The ids
//! left out ([`NoteListQuery::exclude`]) go in as one `bigint[]`.

use postgres::GenericClient;

use super::notes::{VISIBLE, kind_from_sql, note_from_row, row_columns};
use super::{KIND_MAX, backend, column, cut, view_params};
use crate::limits::ENTITY_MAX;
use crate::store::extensions::Columns;
use crate::store::listing::{
    LangFilter, NoteListQuery, NotePage, NoteRow, NoteSort, UsefulFilter, checked_exclude, direction, page_limit,
    query_entity, query_text, scoped,
};
use crate::store::{NoteKind, Scope, StoreError};

/// The notes the text (`$1`) and the normalized entity name (`$4`) keep (`NULL` for any).
const KEPT: &str = "($1::text IS NULL OR strpos(lower(n.title), lower($1::text)) > 0
                     OR strpos(lower(n.body), lower($1::text)) > 0 OR strpos(lower(n.tags), lower($1::text)) > 0)
                    AND ($4::text IS NULL OR EXISTS (SELECT 1 FROM note_entities ne JOIN entities e ON e.id = ne.entity_id
                                                     WHERE ne.note_id = n.id AND e.name_norm = $4::text))";

pub(super) fn list(
    client: &mut impl GenericClient,
    query: &NoteListQuery,
    columns: Columns,
) -> Result<NotePage, StoreError> {
    let text = query_text(query)?;
    let entity = query_entity(query);
    let exclude = checked_exclude(&query.exclude)?.to_vec();
    let (repo, global) = view_params(client, &query.view)?;
    let lang = LangFilter::condition(query.lang, "n.lang", columns.lang);
    let useful = UsefulFilter::condition(query.useful, "n.shown", "n.useful", columns.usage);

    let counts_sql = format!(
        "SELECT {}, n.project_id IS NULL, count(*) FROM notes n WHERE {VISIBLE} AND {KEPT} AND {lang} AND {useful}
           AND n.id <> ALL($5::bigint[])
         GROUP BY n.kind, n.project_id IS NULL",
        cut("n.kind", KIND_MAX)
    );
    let counted: Vec<(NoteKind, Scope, usize)> = client
        .query(&counts_sql, &[&text, &repo, &global, &entity, &exclude])
        .map_err(backend)?
        .iter()
        .map(|row| {
            let kind: String = column(row, 0)?;
            let is_global: bool = column(row, 1)?;
            let count: i64 = column(row, 2)?;
            let scope = if is_global { Scope::Global } else { Scope::Repo };
            Ok((kind_from_sql(&kind), scope, usize::try_from(count).unwrap_or(0)))
        })
        .collect::<Result<_, StoreError>>()?;

    let dir = direction(query);
    let order = match query.sort {
        NoteSort::Updated => format!("n.updated_at {dir}, n.id {dir}"),
        NoteSort::Title => format!("n.title_norm COLLATE \"C\" {dir}, n.id {dir}"),
        NoteSort::Kind => format!("n.kind COLLATE \"C\" {dir}, n.id {dir}"),
        NoteSort::Scope => format!("(n.project_id IS NOT NULL) {dir}, n.id {dir}"),
        NoteSort::Place => format!("n.id {dir}"),
        // Without the counters every note counts as 0, so only the id orders them.
        NoteSort::Useful | NoteSort::LastUseful if !columns.usage => format!("n.id {dir}"),
        NoteSort::Useful => format!("coalesce(n.useful, 0) {dir}, n.id {dir}"),
        NoteSort::LastUseful => format!("coalesce(n.last_useful_at, 0) {dir}, n.id {dir}"),
    };
    let sql = format!(
        "SELECT {} FROM notes n LEFT JOIN projects p ON p.id = n.project_id
         WHERE {VISIBLE} AND {KEPT} AND {lang} AND {useful} AND ($5::text IS NULL OR n.kind = $5::text) AND n.id <> ALL($8::bigint[])
         ORDER BY {order} LIMIT $6 OFFSET $7",
        row_columns(columns)
    );

    let limit = i64::try_from(page_limit(query)).unwrap_or(1);
    let offset = i64::try_from(query.offset).unwrap_or(i64::MAX);
    let kind = query.kind.map(NoteKind::as_str);
    let (repo, global) = scoped(query, repo, global);

    let mut rows: Vec<NoteRow> = client
        .query(
            &sql,
            &[&text, &repo, &global, &entity, &kind, &limit, &offset, &exclude],
        )
        .map_err(backend)?
        .iter()
        .map(|row| note_from_row(row).map(NoteRow::from))
        .collect::<Result<_, StoreError>>()?;

    fill_entities(client, &mut rows)?;
    Ok(NotePage::new(counted, query, rows))
}

/// Sets the entity names of `rows` (alphabetically, as a note's), in one statement.
fn fill_entities(client: &mut impl GenericClient, rows: &mut [NoteRow]) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }

    let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    let sql = format!(
        "SELECT ne.note_id, {} FROM note_entities ne JOIN entities e ON e.id = ne.entity_id
         WHERE ne.note_id = ANY($1) ORDER BY e.name_norm COLLATE \"C\"",
        cut("e.name", ENTITY_MAX)
    );

    for linked in client.query(&sql, &[&ids]).map_err(backend)? {
        let id: i64 = column(&linked, 0)?;
        let name: String = column(&linked, 1)?;

        if let Some(row) = rows.iter_mut().find(|row| row.id == id) {
            row.entities.push(name);
        }
    }

    Ok(())
}
