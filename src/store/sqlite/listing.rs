//! A page of notes ([`crate::store::listing`]) in the SQLite dialect: two statements, the counts
//! per kind and the page, then the entity names of the page's notes. `instr`, not `LIKE`, so `%`
//! and `_` in the text are plain characters.

use super::notes::{ROW_COLUMNS, VISIBLE, kind_from_sql, note_from_row};
use super::sql::{Sql, Value, args};
use super::view_params;
use crate::store::listing::{
    NoteListQuery, NotePage, NoteRow, NoteSort, direction, page_limit, query_entity, query_text,
};
use crate::store::{NoteKind, StoreError};

/// The notes the text (`?1`) and the normalized entity name (`?4`) keep (`NULL` for any).
const KEPT: &str = "(?1 IS NULL OR instr(lower(n.title), lower(?1)) > 0 OR instr(lower(n.body), lower(?1)) > 0
                     OR instr(lower(n.tags), lower(?1)) > 0)
                    AND (?4 IS NULL OR EXISTS (SELECT 1 FROM note_entities ne JOIN entities e ON e.id = ne.entity_id
                                               WHERE ne.note_id = n.id AND e.name_norm = ?4))";

pub(super) fn list(db: &mut dyn Sql, query: &NoteListQuery) -> Result<NotePage, StoreError> {
    let text = query_text(query)?;
    let entity = query_entity(query);
    let (repo, global) = view_params(db, &query.view)?;

    let counted: Vec<(NoteKind, usize)> = db
        .query(
            &format!("SELECT n.kind, count(*) FROM notes n WHERE {VISIBLE} AND {KEPT} GROUP BY n.kind"),
            args![text, repo, global, entity.as_deref()],
        )?
        .iter()
        .map(|row| {
            let kind: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((kind_from_sql(&kind), usize::try_from(count).unwrap_or(0)))
        })
        .collect::<Result<_, StoreError>>()?;

    let dir = direction(query);
    let order = match query.sort {
        NoteSort::Updated => format!("n.updated_at {dir}, n.id {dir}"),
        NoteSort::Title => format!("n.title_norm {dir}, n.id {dir}"),
        NoteSort::Kind => format!("n.kind {dir}, n.id {dir}"),
        NoteSort::Scope => format!("(n.project_id IS NOT NULL) {dir}, n.id {dir}"),
        NoteSort::Place => format!("n.id {dir}"),
    };
    let sql = format!(
        "SELECT {ROW_COLUMNS} FROM notes n LEFT JOIN projects p ON p.id = n.project_id
         WHERE {VISIBLE} AND {KEPT} AND (?5 IS NULL OR n.kind = ?5)
         ORDER BY {order} LIMIT ?6 OFFSET ?7"
    );

    let limit = i64::try_from(page_limit(query)).unwrap_or(1);
    let offset = i64::try_from(query.offset).unwrap_or(i64::MAX);
    let kind = query.kind.map(NoteKind::as_str);

    let mut rows: Vec<NoteRow> = db
        .query(&sql, args![text, repo, global, entity.as_deref(), kind, limit, offset])?
        .iter()
        .map(|row| note_from_row(row).map(NoteRow::from))
        .collect::<Result<_, StoreError>>()?;

    fill_entities(db, &mut rows)?;
    Ok(NotePage::new(counted, query.kind, rows))
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
