//! Team hooks in a PostgreSQL team database: one row per hook in `hooks`. A put reads the stored
//! row, compares its hash and counts the scope inside a write transaction, which holds the write
//! lock, so two teammates sharing at once queue. Rows are read back through [`read_back`]: the
//! stored `sha256` column is never read.

use postgres::{GenericClient, Row, Transaction};

use super::{PostgresStore, backend, column, cut, now};
use crate::limits::{HOOK_AUTHOR_MAX, HOOK_BODY_MAX};
use crate::store::hooks::{HOOK_REPO_MAX, StoredHook, is_full, read_back};
use crate::store::{HookDelete, HookDraft, HookPage, HookPut, HookStore, StoreError, TeamHook};
use crate::store::{checked_hook, hook_sha256};

/// The columns [`stored_from_row`] reads, text cut one character over its limit (see [`cut`]), so
/// [`read_back`] recognizes a value over the limit.
fn columns() -> String {
    format!(
        "{id}, {repo}, {author}, created_at, updated_at, {body}",
        id = cut("id", 32),
        repo = cut("repo", HOOK_REPO_MAX),
        author = cut("author", HOOK_AUTHOR_MAX),
        body = cut("body", HOOK_BODY_MAX),
    )
}

/// The rows of the global scope and of the repository `$1` (none when it is `NULL`).
const LISTED: &str = "FROM hooks WHERE repo IS NULL OR repo = $1::text";

impl HookStore for PostgresStore {
    fn list_hooks(&mut self, repo: Option<&str>, offset: usize, limit: usize) -> Result<HookPage, StoreError> {
        let skip = i64::try_from(offset).unwrap_or(i64::MAX);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);

        self.read(|client| {
            let row = client
                .query_one(&format!("SELECT count(*) {LISTED}"), &[&repo])
                .map_err(backend)?;
            let total: i64 = column(&row, 0)?;

            let sql = format!(
                "SELECT {} {LISTED} ORDER BY repo IS NOT NULL, created_at, id COLLATE \"C\" LIMIT $2 OFFSET $3",
                columns()
            );
            let rows = client.query(&sql, &[&repo, &limit, &skip]).map_err(backend)?;
            let hooks = rows.iter().filter_map(hook_from_row).collect();

            Ok(HookPage::of(
                usize::try_from(total).unwrap_or(0),
                offset,
                rows.len(),
                hooks,
            ))
        })
    }

    fn get_hook(&mut self, id: &str) -> Result<Option<TeamHook>, StoreError> {
        self.read(|client| load(client, id))
    }

    fn put_hook(&mut self, draft: &HookDraft, expect: Option<&str>) -> Result<HookPut, StoreError> {
        let draft = checked_hook(draft)?;
        self.write(|tx| put(tx, &draft, expect))
    }

    fn delete_hook(&mut self, id: &str, expect: Option<&str>) -> Result<HookDelete, StoreError> {
        self.write(|tx| {
            let Some((_, body)) = stored_body(tx, id)? else {
                return Ok(HookDelete::Missing);
            };

            if expect.is_some_and(|expect| hook_sha256(&body) != expect) {
                return Ok(HookDelete::Changed);
            }

            tx.execute("DELETE FROM hooks WHERE id = $1", &[&id]).map_err(backend)?;
            Ok(HookDelete::Deleted)
        })
    }
}

fn put(tx: &mut Transaction<'_>, draft: &HookDraft, expect: Option<&str>) -> Result<HookPut, StoreError> {
    let stored = stored_body(tx, &draft.id)?;

    match (expect, &stored) {
        (None, Some(_)) => return Ok(HookPut::Exists),
        (Some(_), None) => return Ok(HookPut::Missing),
        (Some(expect), Some((_, body))) if hook_sha256(body) != expect => return Ok(HookPut::Changed),
        _ => {}
    }

    let moves = stored.as_ref().is_none_or(|(repo, _)| *repo != draft.repo);

    if moves {
        let row = tx
            .query_one(
                "SELECT count(*) FROM hooks WHERE repo IS NOT DISTINCT FROM $1::text AND id <> $2",
                &[&draft.repo, &draft.id],
            )
            .map_err(backend)?;

        if is_full(column(&row, 0)?) {
            return Ok(HookPut::Full);
        }
    }

    let now = now();
    let sha256 = hook_sha256(&draft.body);

    if stored.is_some() {
        tx.execute(
            "UPDATE hooks SET repo = $2, body = $3, sha256 = $4, author = $5, written_by = current_user,
                 updated_at = $6
             WHERE id = $1",
            &[&draft.id, &draft.repo, &draft.body, &sha256, &draft.author, &now],
        )
        .map_err(backend)?;
    } else {
        tx.execute(
            "INSERT INTO hooks (id, repo, body, sha256, author, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $6)",
            &[&draft.id, &draft.repo, &draft.body, &sha256, &draft.author, &now],
        )
        .map_err(backend)?;
    }

    let saved = load(tx, &draft.id)?.ok_or_else(|| StoreError::Backend("the saved hook was not found".to_owned()))?;

    Ok(HookPut::Saved(saved))
}

/// The repository and body of row `id` (the body cut one character over its limit), whatever
/// they hold.
fn stored_body(client: &mut impl GenericClient, id: &str) -> Result<Option<(Option<String>, String)>, StoreError> {
    let sql = format!("SELECT repo, {} FROM hooks WHERE id = $1", cut("body", HOOK_BODY_MAX));
    let row = client.query_opt(&sql, &[&id]).map_err(backend)?;

    row.map(|row| Ok((column(&row, 0)?, column(&row, 1)?))).transpose()
}

/// Hook `id`; a row [`read_back`] leaves out counts as missing.
fn load(client: &mut impl GenericClient, id: &str) -> Result<Option<TeamHook>, StoreError> {
    let sql = format!("SELECT {} FROM hooks WHERE id = $1", columns());
    let rows = client.query(&sql, &[&id]).map_err(backend)?;

    Ok(rows.iter().find_map(hook_from_row))
}

/// A row of [`columns`] as a hook; `None` (noted) when a column holds an unexpected type or
/// [`read_back`] leaves it out.
fn hook_from_row(row: &Row) -> Option<TeamHook> {
    match stored_from_row(row) {
        Ok(stored) => read_back(stored),
        Err(error) => {
            eprintln!("harness-memory: left out a team hook: {error}");
            None
        }
    }
}

fn stored_from_row(row: &Row) -> Result<StoredHook, StoreError> {
    Ok(StoredHook {
        id: column(row, 0)?,
        repo: column(row, 1)?,
        author: column(row, 2)?,
        created_at: column(row, 3)?,
        updated_at: column(row, 4)?,
        body: column(row, 5)?,
    })
}
