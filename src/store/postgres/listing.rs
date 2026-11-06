//! A page of notes ([`crate::store::listing`]) in PostgreSQL: the counts per kind and scope, the
//! page, then the entity names of the page's notes and who added the notes. `strpos`, not `LIKE`,
//! so `%` and `_` in the text are plain characters; titles and names sort by code point
//! (`COLLATE "C"`), like SQLite. The ids left out ([`NoteListQuery::exclude`]) go in as one
//! `bigint[]`.

use postgres::GenericClient;

use super::notes::{VISIBLE, kind_from_sql, note_from_row, row_columns};
use super::{KIND_MAX, backend, column, cut, view_params};
use crate::limits::{AUTHOR_GROUPS_READ_MAX, ENTITY_MAX, TEAM_DOC_AUTHOR_MAX};
use crate::store::author_list::AuthorFacet;
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

    let author = query.author.name();

    let counts_sql = format!(
        "SELECT {}, n.project_id IS NULL, count(*) FROM notes n WHERE {VISIBLE} AND {KEPT} AND {lang} AND {useful}
           AND n.id <> ALL($5::bigint[]) AND {author}
         GROUP BY n.kind, n.project_id IS NULL",
        cut("n.kind", KIND_MAX),
        author = query.author.condition("n.added_by_name", "$6::text", columns.authors)
    );
    let counted: Vec<(NoteKind, Scope, usize)> = client
        .query(&counts_sql, &[&text, &repo, &global, &entity, &exclude, &author])
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
        // Without the author columns nobody has a name, so only the id orders them. The notes
        // without a name come last whichever the direction, and the names sort by ASCII-folded
        // code point (`lower` would follow the locale).
        NoteSort::AddedBy if !columns.authors => format!("n.id {dir}"),
        NoteSort::AddedBy => {
            let key = "nullif(n.added_by_name, '')";
            format!(
                "({key} IS NULL) ASC, {} COLLATE \"C\" {dir}, n.id {dir}",
                fold_ascii(key)
            )
        }
    };
    let sql = format!(
        "SELECT {} FROM notes n LEFT JOIN projects p ON p.id = n.project_id
         WHERE {VISIBLE} AND {KEPT} AND {lang} AND {useful} AND ($5::text IS NULL OR n.kind = $5::text) AND n.id <> ALL($8::bigint[])
           AND {author}
         ORDER BY {order} LIMIT $6 OFFSET $7",
        row_columns(columns),
        author = query.author.condition("n.added_by_name", "$9::text", columns.authors)
    );

    let limit = i64::try_from(page_limit(query)).unwrap_or(1);
    let offset = i64::try_from(query.offset).unwrap_or(i64::MAX);
    let kind = query.kind.map(NoteKind::as_str);
    let (repo, global) = scoped(query, repo, global);

    let mut rows: Vec<NoteRow> = client
        .query(
            &sql,
            &[
                &text, &repo, &global, &entity, &kind, &limit, &offset, &exclude, &author,
            ],
        )
        .map_err(backend)?
        .iter()
        .map(|row| note_from_row(row).map(NoteRow::from))
        .collect::<Result<_, StoreError>>()?;

    fill_entities(client, &mut rows)?;

    let authors = if query.want_authors {
        let facet_sql = sql_facet(columns, &lang, &useful);
        author_facet(client, &facet_sql, &[&text, &repo, &global, &entity, &kind, &exclude])?
    } else {
        AuthorFacet::default()
    };

    Ok(NotePage::new(counted, query, rows).with_authors(authors))
}

/// `key` with the ASCII capitals turned to small letters.
fn fold_ascii(key: &str) -> String {
    format!("translate({key}, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz')")
}

/// The statement that counts who added the notes every filter but the author keeps (the page's
/// filters, without its order and its cut), the unknown first and then the most notes first. The
/// parameters are those of the page up to the kind (`$5`), then the notes left out (`$6`).
fn sql_facet(columns: Columns, lang: &str, useful: &str) -> String {
    let filter = format!(
        "FROM notes n WHERE {VISIBLE} AND {KEPT} AND {lang} AND {useful}
           AND ($5::text IS NULL OR n.kind = $5::text) AND n.id <> ALL($6::bigint[])"
    );

    if !columns.authors {
        return format!("SELECT NULL::text, count(*) {filter}");
    }

    let key = cut_key("n.added_by_name");

    format!(
        "SELECT {key}, count(*) {filter}
         GROUP BY {key} ORDER BY ({key} IS NULL) DESC, count(*) DESC, {key} COLLATE \"C\"
         LIMIT {AUTHOR_GROUPS_READ_MAX}"
    )
}

/// The name of whoever added a note: cut to its limit, `NULL` when none is known (a blank counts
/// as none).
fn cut_key(column: &str) -> String {
    format!("nullif({}, '')", cut(column, TEAM_DOC_AUTHOR_MAX))
}

/// Who added the notes `sql` ([`sql_facet`]) counts.
fn author_facet(
    client: &mut impl GenericClient,
    sql: &str,
    params: &[&(dyn postgres::types::ToSql + Sync)],
) -> Result<AuthorFacet, StoreError> {
    let groups: Vec<(Option<String>, usize)> = client
        .query(sql, params)
        .map_err(backend)?
        .iter()
        .map(|row| {
            let name: Option<String> = column(row, 0)?;
            let count: i64 = column(row, 1)?;

            Ok((name, usize::try_from(count).unwrap_or(0)))
        })
        .collect::<Result<_, StoreError>>()?;

    Ok(AuthorFacet::from_groups(groups))
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
