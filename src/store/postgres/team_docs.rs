//! Team documents in a PostgreSQL team database: one row per document in the table of its kind
//! ([`DocKind::table`], the only name spliced into a statement). A put reads the stored row,
//! compares its hash and counts the scope inside a write transaction, which holds the write lock,
//! so two teammates sharing at once queue. Rows are read back through [`read_back`]: the stored
//! `sha256` column is never read.

use postgres::{GenericClient, Row, Transaction};

use super::{PostgresStore, backend, column, cut, now};
use crate::limits::{TEAM_DOC_AUTHOR_MAX, TEAM_DOC_BODY_MAX};
use crate::store::StoreError;
use crate::store::team_docs::{
    DOC_REPO_MAX, DocDelete, DocDraft, DocKind, DocPage, DocPut, LISTED_ID_MAX, StoredDoc, TeamDoc, TeamDocStore,
    check_author, checked_doc, doc_sha256, is_full, not_read_back, read_back, unreadable,
};

/// The columns [`stored_from_row`] reads, text cut one character over its limit (see [`cut`]), so
/// [`read_back`] recognizes a value over the limit.
fn columns() -> String {
    format!(
        "{id}, {repo}, {author}, created_at, updated_at, {body}",
        id = cut("id", 32),
        repo = cut("repo", DOC_REPO_MAX),
        author = cut("author", TEAM_DOC_AUTHOR_MAX),
        body = cut("body", TEAM_DOC_BODY_MAX),
    )
}

/// The rows of the global scope and of the repository `$1` (none when it is `NULL`).
fn listed(kind: DocKind) -> String {
    format!("FROM {} WHERE repo IS NULL OR repo = $1::text", kind.table())
}

impl TeamDocStore for PostgresStore {
    fn list_docs(
        &mut self,
        kind: DocKind,
        repo: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<DocPage, StoreError> {
        let skip = i64::try_from(offset).unwrap_or(i64::MAX);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let listed = listed(kind);

        self.read(|client| {
            let row = client
                .query_one(&format!("SELECT count(*) {listed}"), &[&repo])
                .map_err(backend)?;
            let total: i64 = column(&row, 0)?;

            let sql = format!(
                "SELECT {} {listed} ORDER BY repo IS NOT NULL, created_at, id COLLATE \"C\" LIMIT $2 OFFSET $3",
                columns()
            );
            let rows = client.query(&sql, &[&repo, &limit, &skip]).map_err(backend)?;
            let docs = rows.iter().filter_map(|row| doc_from_row(kind, row)).collect();

            Ok(DocPage::of(
                usize::try_from(total).unwrap_or(0),
                offset,
                rows.len(),
                docs,
            ))
        })
    }

    fn get_doc(&mut self, kind: DocKind, id: &str) -> Result<Option<TeamDoc>, StoreError> {
        self.read(|client| load(client, kind, id))
    }

    fn put_doc(&mut self, kind: DocKind, draft: &DocDraft, expect: Option<&str>) -> Result<DocPut, StoreError> {
        let draft = checked_doc(kind, draft)?;
        self.write(|tx| put(tx, kind, &draft, expect))
    }

    fn delete_doc(&mut self, kind: DocKind, id: &str, expect: Option<&str>) -> Result<DocDelete, StoreError> {
        self.write(|tx| {
            let Some((_, body)) = stored_body(tx, kind, id)? else {
                return Ok(DocDelete::Missing);
            };

            if expect.is_some_and(|expect| doc_sha256(&body) != expect) {
                return Ok(DocDelete::Changed);
            }

            tx.execute(&format!("DELETE FROM {} WHERE id = $1", kind.table()), &[&id])
                .map_err(backend)?;
            Ok(DocDelete::Deleted)
        })
    }

    fn delete_docs_by(&mut self, kind: DocKind, author: &str) -> Result<Vec<String>, StoreError> {
        let author = check_author(author)?;
        let sql = format!(
            "DELETE FROM {} WHERE author = $1 RETURNING left(id, {LISTED_ID_MAX})",
            kind.table()
        );

        let mut ids = self.write(|tx| {
            let rows = tx.query(&sql, &[&author]).map_err(backend)?;
            rows.iter()
                .map(|row| column::<String>(row, 0))
                .collect::<Result<Vec<_>, _>>()
        })?;

        ids.sort();
        Ok(ids)
    }
}

fn put(tx: &mut Transaction<'_>, kind: DocKind, draft: &DocDraft, expect: Option<&str>) -> Result<DocPut, StoreError> {
    let table = kind.table();
    let stored = stored_body(tx, kind, &draft.id)?;

    match (expect, &stored) {
        (None, Some(_)) => return Ok(DocPut::Exists),
        (Some(_), None) => return Ok(DocPut::Missing),
        (Some(expect), Some((_, body))) if doc_sha256(body) != expect => return Ok(DocPut::Changed),
        _ => {}
    }

    let moves = stored.as_ref().is_none_or(|(repo, _)| *repo != draft.repo);

    if moves {
        let row = tx
            .query_one(
                &format!("SELECT count(*) FROM {table} WHERE repo IS NOT DISTINCT FROM $1::text AND id <> $2"),
                &[&draft.repo, &draft.id],
            )
            .map_err(backend)?;

        if is_full(kind, column(&row, 0)?) {
            return Ok(DocPut::Full);
        }
    }

    let now = now();
    let sha256 = doc_sha256(&draft.body);

    if stored.is_some() {
        tx.execute(
            &format!(
                "UPDATE {table} SET repo = $2, body = $3, sha256 = $4, author = $5, written_by = current_user,
                     updated_at = $6
                 WHERE id = $1"
            ),
            &[&draft.id, &draft.repo, &draft.body, &sha256, &draft.author, &now],
        )
        .map_err(backend)?;
    } else {
        tx.execute(
            &format!(
                "INSERT INTO {table} (id, repo, body, sha256, author, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $6)"
            ),
            &[&draft.id, &draft.repo, &draft.body, &sha256, &draft.author, &now],
        )
        .map_err(backend)?;
    }

    let saved = load(tx, kind, &draft.id)?.ok_or_else(|| not_read_back(kind))?;

    Ok(DocPut::Saved(saved))
}

/// The repository and body of row `id` (the body cut one character over its limit), whatever
/// they hold.
fn stored_body(
    client: &mut impl GenericClient,
    kind: DocKind,
    id: &str,
) -> Result<Option<(Option<String>, String)>, StoreError> {
    let sql = format!(
        "SELECT repo, {} FROM {} WHERE id = $1",
        cut("body", TEAM_DOC_BODY_MAX),
        kind.table()
    );
    let row = client.query_opt(&sql, &[&id]).map_err(backend)?;

    row.map(|row| Ok((column(&row, 0)?, column(&row, 1)?))).transpose()
}

/// Document `id`; a row [`read_back`] leaves out counts as missing.
fn load(client: &mut impl GenericClient, kind: DocKind, id: &str) -> Result<Option<TeamDoc>, StoreError> {
    let sql = format!("SELECT {} FROM {} WHERE id = $1", columns(), kind.table());
    let rows = client.query(&sql, &[&id]).map_err(backend)?;

    Ok(rows.iter().find_map(|row| doc_from_row(kind, row)))
}

/// A row of [`columns`] as a document; `None` (noted) when a column holds an unexpected type or
/// [`read_back`] leaves it out.
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
        id: column(row, 0)?,
        repo: column(row, 1)?,
        author: column(row, 2)?,
        created_at: column(row, 3)?,
        updated_at: column(row, 4)?,
        body: column(row, 5)?,
    })
}
