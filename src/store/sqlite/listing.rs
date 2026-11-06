//! A page of notes ([`crate::store::listing`]) in the SQLite dialect: the counts per kind and
//! scope, the page, then the entity names of the page's notes and who added the notes. `instr`,
//! not `LIKE`, so `%` and `_` in the text are plain characters. The ids left out
//! ([`NoteListQuery::exclude`]) go in as one JSON array read with `json_each`.

use super::authors::added_by_key;
use super::notes::{VISIBLE, kind_from_sql, note_from_row, row_columns};
use super::sql::{Sql, Value, args};
use super::view_params;
use crate::limits::AUTHOR_GROUPS_READ_MAX;
use crate::store::author_list::AuthorFacet;
use crate::store::extensions::Columns;
use crate::store::listing::{
    LangFilter, NoteListQuery, NotePage, NoteRow, NoteSort, UsefulFilter, direction, excluded_json, page_limit,
    query_entity, query_text, scoped,
};
use crate::store::{NoteKind, Scope, StoreError};

/// The notes the text (`?1`) and the normalized entity name (`?4`) keep (`NULL` for any).
const KEPT: &str = "(?1 IS NULL OR instr(lower(n.title), lower(?1)) > 0 OR instr(lower(n.body), lower(?1)) > 0
                     OR instr(lower(n.tags), lower(?1)) > 0)
                    AND (?4 IS NULL OR EXISTS (SELECT 1 FROM note_entities ne JOIN entities e ON e.id = ne.entity_id
                                               WHERE ne.note_id = n.id AND e.name_norm = ?4))";

/// The notes whose id is not in the JSON array `?<mark>`.
pub(super) fn not_excluded(mark: usize) -> String {
    format!("n.id NOT IN (SELECT value FROM json_each(?{mark}))")
}

pub(super) fn list(db: &mut dyn Sql, query: &NoteListQuery, columns: Columns) -> Result<NotePage, StoreError> {
    let text = query_text(query)?;
    let entity = query_entity(query);
    let exclude = excluded_json(&query.exclude)?;
    let (repo, global) = view_params(db, &query.view)?;
    let lang = LangFilter::condition(query.lang, "n.lang", columns.lang);
    let useful = UsefulFilter::condition(query.useful, "n.shown", "n.useful", columns.usage);

    let counted: Vec<(NoteKind, Scope, usize)> = db
        .query(
            &format!(
                "SELECT n.kind, n.project_id IS NULL, count(*) FROM notes n WHERE {VISIBLE} AND {KEPT} AND {lang} AND {useful} AND {} AND {}
                 GROUP BY n.kind, n.project_id IS NULL",
                not_excluded(5),
                query.author.condition(&added_by_key("n"), "?6", columns.authors)
            ),
            args![text, repo, global, entity.as_deref(), exclude.as_str(), query.author.name()],
        )?
        .iter()
        .map(|row| {
            let kind: String = row.get(0)?;
            let is_global: i64 = row.get(1)?;
            let count: i64 = row.get(2)?;
            let scope = if is_global == 0 { Scope::Repo } else { Scope::Global };
            Ok((kind_from_sql(&kind), scope, usize::try_from(count).unwrap_or(0)))
        })
        .collect::<Result<_, StoreError>>()?;

    let dir = direction(query);
    let added_by = added_by_key("n");
    let order = match query.sort {
        NoteSort::Updated => format!("n.updated_at {dir}, n.id {dir}"),
        NoteSort::Title => format!("n.title_norm {dir}, n.id {dir}"),
        NoteSort::Kind => format!("n.kind {dir}, n.id {dir}"),
        NoteSort::Scope => format!("(n.project_id IS NOT NULL) {dir}, n.id {dir}"),
        NoteSort::Place => format!("n.id {dir}"),
        // Without the counters every note counts as 0, so only the id orders them.
        NoteSort::Useful | NoteSort::LastUseful if !columns.usage => format!("n.id {dir}"),
        NoteSort::Useful => format!("coalesce(n.useful, 0) {dir}, n.id {dir}"),
        NoteSort::LastUseful => format!("coalesce(n.last_useful_at, 0) {dir}, n.id {dir}"),
        // Without the author columns nobody has a name, so only the id orders them. The notes
        // without a name come last whichever the direction.
        NoteSort::AddedBy if !columns.authors => format!("n.id {dir}"),
        NoteSort::AddedBy => format!("({added_by} IS NULL) ASC, lower({added_by}) {dir}, n.id {dir}"),
    };
    let sql = format!(
        "SELECT {} FROM notes n LEFT JOIN projects p ON p.id = n.project_id
         WHERE {VISIBLE} AND {KEPT} AND {lang} AND {useful} AND (?5 IS NULL OR n.kind = ?5) AND {} AND {}
         ORDER BY {order} LIMIT ?6 OFFSET ?7",
        row_columns(columns),
        not_excluded(8),
        query.author.condition(&added_by_key("n"), "?9", columns.authors)
    );

    let limit = i64::try_from(page_limit(query)).unwrap_or(1);
    let offset = i64::try_from(query.offset).unwrap_or(i64::MAX);
    let kind = query.kind.map(NoteKind::as_str);
    let (repo, global) = scoped(query, repo, global);

    let mut rows: Vec<NoteRow> = db
        .query(
            &sql,
            args![
                text,
                repo,
                global,
                entity.as_deref(),
                kind,
                limit,
                offset,
                exclude.as_str(),
                query.author.name()
            ],
        )?
        .iter()
        .map(|row| note_from_row(row).map(NoteRow::from))
        .collect::<Result<_, StoreError>>()?;

    fill_entities(db, &mut rows)?;
    let authors = if query.want_authors {
        author_facet(
            db,
            &sql_facet(columns, &lang, &useful),
            args![text, repo, global, entity.as_deref(), kind, exclude.as_str()],
        )?
    } else {
        AuthorFacet::default()
    };

    Ok(NotePage::new(counted, query, rows).with_authors(authors))
}

/// The statement that counts who added the notes every filter but the author keeps (the page's
/// filters, without its order and its cut), the unknown first and then the most notes first. The
/// parameters are those of the page up to the kind (`?5`), then the notes left out (`?6`).
///
/// The name is computed in a subquery that SQLite cannot fold into the outer one (`LIMIT -1`), so
/// the grouping sorts only the name, never the whole column, and it comes back as hex: whatever a
/// database written by hand holds, the reply is plain ASCII, and [`author_facet`] decodes it.
fn sql_facet(columns: Columns, lang: &str, useful: &str) -> String {
    let key = if columns.authors {
        added_by_key("n")
    } else {
        "NULL".to_owned()
    };

    format!(
        "SELECT hex(CAST(k AS BLOB)), count(*) FROM (
             SELECT {key} AS k FROM notes n
             WHERE {VISIBLE} AND {KEPT} AND {lang} AND {useful} AND (?5 IS NULL OR n.kind = ?5) AND {}
             LIMIT -1)
         GROUP BY k ORDER BY (k IS NULL) DESC, count(*) DESC, k LIMIT {AUTHOR_GROUPS_READ_MAX}",
        not_excluded(6)
    )
}

/// Who added the notes `sql` ([`sql_facet`]) counts. A name that is not valid UTF-8 counts as
/// unknown.
fn author_facet(db: &mut dyn Sql, sql: &str, args: &[Value]) -> Result<AuthorFacet, StoreError> {
    let groups: Vec<(Option<String>, usize)> = db
        .query(sql, args)?
        .iter()
        .map(|row| {
            let hex: String = row.get(0)?;
            let count: i64 = row.get(1)?;

            Ok((from_hex(&hex), usize::try_from(count).unwrap_or(0)))
        })
        .collect::<Result<_, StoreError>>()?;

    Ok(AuthorFacet::from_groups(groups))
}

/// The text the hex digits `hex` spell; `None` for none, an odd or bad hex, or bytes that are not
/// UTF-8.
fn from_hex(hex: &str) -> Option<String> {
    if hex.is_empty() || !hex.len().is_multiple_of(2) {
        return None;
    }

    let bytes: Option<Vec<u8>> = hex
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let digits = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(digits, 16).ok()
        })
        .collect();

    String::from_utf8(bytes?).ok()
}

/// Sets the entity names of `rows` (alphabetically, as a note's), in one statement.
fn fill_entities(db: &mut dyn Sql, rows: &mut [NoteRow]) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }

    let marks: Vec<String> = (1..=rows.len()).map(|index| format!("?{index}")).collect();
    let ids: Vec<Value> = rows.iter().map(|row| Value::from(row.id)).collect();
    let sql = format!(
        "SELECT ne.note_id, e.name FROM note_entities ne JOIN entities e ON e.id = ne.entity_id
         WHERE ne.note_id IN ({}) ORDER BY e.name_norm",
        marks.join(", ")
    );

    for linked in db.query(&sql, &ids)? {
        let id: i64 = linked.get(0)?;
        let name: String = linked.get(1)?;

        if let Some(row) = rows.iter_mut().find(|row| row.id == id) {
            row.entities.push(name);
        }
    }

    Ok(())
}
