//! Team documents in the SQLite-dialect stores: the statements of a libSQL team database, shared
//! with the local file through [`Engine`] (the local file has the tables, and only tests fill
//! them). Each kind has its table ([`DocKind::table`]), the only name spliced into a statement.
//!
//! A put reads the stored row, compares its hash and counts the scope inside the write
//! transaction, which holds the write lock, so two teammates sharing at once queue.

use super::SqliteStore;
use super::engine::Engine;
use super::now;
use super::sql::{Row, Sql, args};
use crate::limits::{TEAM_DOC_AUTHOR_MAX, TEAM_DOC_BODY_MAX};
use crate::store::StoreError;
use crate::store::team_docs::{
    DOC_REPO_MAX, DocDelete, DocDraft, DocKind, DocPage, DocPut, LISTED_ID_MAX, StoredDoc, TeamDoc, TeamDocStore,
    check_author, checked_doc, doc_sha256, is_full, not_read_back, read_back, unreadable,
};

/// The columns [`stored_from_row`] reads, text cut one character over its limit and the times
/// read as integers whatever was stored, so a row changed by hand cannot make a reply of any size
/// or fail a list (see [`read_back`]).
fn columns() -> String {
    format!(
        "substr(id, 1, 33), substr(repo, 1, {repo}), substr(author, 1, {author}), CAST(created_at AS INTEGER), \
         CAST(updated_at AS INTEGER), substr(body, 1, {body})",
        repo = DOC_REPO_MAX + 1,
        author = TEAM_DOC_AUTHOR_MAX + 1,
        body = TEAM_DOC_BODY_MAX + 1,
    )
}

/// The rows of the global scope and of the repository `?1` (none when it is `NULL`).
fn listed(kind: DocKind) -> String {
    format!("FROM {} WHERE repo IS NULL OR repo = ?1", kind.table())
}

pub(crate) fn list(
    engine: &mut impl Engine,
    kind: DocKind,
    repo: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<DocPage, StoreError> {
    let skip = i64::try_from(offset).unwrap_or(i64::MAX);
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let listed = listed(kind);

    engine.read(|db| {
        let total: i64 = db
            .value(&format!("SELECT count(*) {listed}"), args![repo])?
            .unwrap_or(0);

        let sql = format!(
            "SELECT {} {listed} ORDER BY repo IS NOT NULL, created_at, id LIMIT ?2 OFFSET ?3",
            columns()
        );
        let rows = db.query(&sql, args![repo, limit, skip])?;
        let docs = rows.iter().filter_map(|row| doc_from_row(kind, row)).collect();

        Ok(DocPage::of(
            usize::try_from(total).unwrap_or(0),
            offset,
            rows.len(),
            docs,
        ))
    })
}

pub(crate) fn get(engine: &mut impl Engine, kind: DocKind, id: &str) -> Result<Option<TeamDoc>, StoreError> {
    engine.read(|db| load(db, kind, id))
}

pub(crate) fn put(
    engine: &mut impl Engine,
    kind: DocKind,
    draft: &DocDraft,
    expect: Option<&str>,
) -> Result<DocPut, StoreError> {
    let draft = checked_doc(kind, draft)?;

    engine.write(|db| put_in(db, kind, &draft, expect))
}

pub(crate) fn delete(
    engine: &mut impl Engine,
    kind: DocKind,
    id: &str,
    expect: Option<&str>,
) -> Result<DocDelete, StoreError> {
    engine.write(|db| {
        let Some(stored) = stored_body(db, kind, id)? else {
            return Ok(DocDelete::Missing);
        };

        if expect.is_some_and(|expect| doc_sha256(&stored.1) != expect) {
            return Ok(DocDelete::Changed);
        }

        db.execute(&format!("DELETE FROM {} WHERE id = ?1", kind.table()), args![id])?;
        Ok(DocDelete::Deleted)
    })
}

pub(crate) fn delete_by(engine: &mut impl Engine, kind: DocKind, author: &str) -> Result<Vec<String>, StoreError> {
    let author = check_author(author)?;
    let table = kind.table();

    engine.write(|db| {
        let ids: Vec<String> = db.column(
            &format!("SELECT ifnull(substr(id, 1, {LISTED_ID_MAX}), '') FROM {table} WHERE author = ?1 ORDER BY id"),
            args![&author],
        )?;

        db.execute(&format!("DELETE FROM {table} WHERE author = ?1"), args![&author])?;
        Ok(ids)
    })
}

fn put_in(db: &mut dyn Sql, kind: DocKind, draft: &DocDraft, expect: Option<&str>) -> Result<DocPut, StoreError> {
    let table = kind.table();
    let stored = stored_body(db, kind, &draft.id)?;

    match (expect, &stored) {
        (None, Some(_)) => return Ok(DocPut::Exists),
        (Some(_), None) => return Ok(DocPut::Missing),
        (Some(expect), Some((_, body))) if doc_sha256(body) != expect => return Ok(DocPut::Changed),
        _ => {}
    }

    let moves = stored.as_ref().is_none_or(|(repo, _)| *repo != draft.repo);

    if moves {
        let count: i64 = db
            .value(
                &format!("SELECT count(*) FROM {table} WHERE repo IS ?1 AND id <> ?2"),
                args![draft.repo.as_deref(), &draft.id],
            )?
            .unwrap_or(0);

        if is_full(kind, count) {
            return Ok(DocPut::Full);
        }
    }

    let now = now();
    let sha256 = doc_sha256(&draft.body);

    if stored.is_some() {
        db.execute(
            &format!(
                "UPDATE {table} SET repo = ?2, body = ?3, sha256 = ?4, author = ?5, updated_at = ?6 WHERE id = ?1"
            ),
            args![
                &draft.id,
                draft.repo.as_deref(),
                &draft.body,
                &sha256,
                &draft.author,
                now
            ],
        )?;
    } else {
        db.execute(
            &format!(
                "INSERT INTO {table} (id, repo, body, sha256, author, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)"
            ),
            args![
                &draft.id,
                draft.repo.as_deref(),
                &draft.body,
                &sha256,
                &draft.author,
                now
            ],
        )?;
    }

    let saved = load(db, kind, &draft.id)?.ok_or_else(|| not_read_back(kind))?;

    Ok(DocPut::Saved(saved))
}

/// The repository and body of row `id` (the body cut one character over its limit), whatever
/// they hold.
fn stored_body(db: &mut dyn Sql, kind: DocKind, id: &str) -> Result<Option<(Option<String>, String)>, StoreError> {
    let sql = format!(
        "SELECT repo, substr(body, 1, {}) FROM {} WHERE id = ?1",
        TEAM_DOC_BODY_MAX + 1,
        kind.table()
    );

    db.row(&sql, args![id], |row| Ok((row.get(0)?, row.get(1)?)))
}

/// Document `id`; a row [`read_back`] leaves out counts as missing.
fn load(db: &mut dyn Sql, kind: DocKind, id: &str) -> Result<Option<TeamDoc>, StoreError> {
    let sql = format!("SELECT {} FROM {} WHERE id = ?1", columns(), kind.table());

    Ok(db
        .query(&sql, args![id])?
        .iter()
        .find_map(|row| doc_from_row(kind, row)))
}

/// A row of [`columns`] as a document; `None` (noted) when it cannot be read or [`read_back`]
/// leaves it out.
fn doc_from_row(kind: DocKind, row: &Row) -> Option<TeamDoc> {
    match stored_from_row(row) {
        Ok(stored) => read_back(kind, stored),
        Err(error) => {
            unreadable(kind, &error);
            None
        }
    }
}

fn stored_from_row(row: &Row) -> Result<StoredDoc, StoreError> {
    Ok(StoredDoc {
        id: row.get(0)?,
        repo: row.get(1)?,
        author: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        body: row.get(5)?,
    })
}

impl TeamDocStore for SqliteStore {
    fn list_docs(
        &mut self,
        kind: DocKind,
        repo: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<DocPage, StoreError> {
        list(self, kind, repo, offset, limit)
    }

    fn get_doc(&mut self, kind: DocKind, id: &str) -> Result<Option<TeamDoc>, StoreError> {
        get(self, kind, id)
    }

    fn put_doc(&mut self, kind: DocKind, draft: &DocDraft, expect: Option<&str>) -> Result<DocPut, StoreError> {
        put(self, kind, draft, expect)
    }

    fn delete_doc(&mut self, kind: DocKind, id: &str, expect: Option<&str>) -> Result<DocDelete, StoreError> {
        delete(self, kind, id, expect)
    }

    fn delete_docs_by(&mut self, kind: DocKind, author: &str) -> Result<Vec<String>, StoreError> {
        delete_by(self, kind, author)
    }
}
