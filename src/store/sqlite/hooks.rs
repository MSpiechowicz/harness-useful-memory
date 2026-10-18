//! Team hooks in the SQLite-dialect stores: the statements of a libSQL team database, shared with
//! the local file through [`Engine`] (the local file has the table, and only tests fill it).
//!
//! A put reads the stored row, compares its hash and counts the scope inside the write
//! transaction, which holds the write lock, so two teammates sharing at once queue.

use super::SqliteStore;
use super::engine::Engine;
use super::now;
use super::sql::{Row, Sql, args};
use crate::limits::{HOOK_AUTHOR_MAX, HOOK_BODY_MAX};
use crate::store::hook_sha256;
use crate::store::hooks::{HOOK_REPO_MAX, StoredHook, is_full, read_back};
use crate::store::{HookDelete, HookDraft, HookPage, HookPut, HookStore, StoreError, TeamHook, checked_hook};

/// The columns [`stored_from_row`] reads, text cut one character over its limit and the times
/// read as integers whatever was stored, so a row changed by hand cannot make a reply of any size
/// or fail a list (see [`read_back`]).
fn columns() -> String {
    format!(
        "substr(id, 1, 33), substr(repo, 1, {repo}), substr(author, 1, {author}), CAST(created_at AS INTEGER), \
         CAST(updated_at AS INTEGER), substr(body, 1, {body})",
        repo = HOOK_REPO_MAX + 1,
        author = HOOK_AUTHOR_MAX + 1,
        body = HOOK_BODY_MAX + 1,
    )
}

/// The rows of the global scope and of the repository `?1` (none when it is `NULL`).
const LISTED: &str = "FROM hooks WHERE repo IS NULL OR repo = ?1";

pub(crate) fn list(
    engine: &mut impl Engine,
    repo: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<HookPage, StoreError> {
    let skip = i64::try_from(offset).unwrap_or(i64::MAX);
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);

    engine.read(|db| {
        let total: i64 = db
            .value(&format!("SELECT count(*) {LISTED}"), args![repo])?
            .unwrap_or(0);

        let sql = format!(
            "SELECT {} {LISTED} ORDER BY repo IS NOT NULL, created_at, id LIMIT ?2 OFFSET ?3",
            columns()
        );
        let rows = db.query(&sql, args![repo, limit, skip])?;
        let hooks = rows.iter().filter_map(hook_from_row).collect();

        Ok(HookPage::of(
            usize::try_from(total).unwrap_or(0),
            offset,
            rows.len(),
            hooks,
        ))
    })
}

pub(crate) fn get(engine: &mut impl Engine, id: &str) -> Result<Option<TeamHook>, StoreError> {
    engine.read(|db| load(db, id))
}

pub(crate) fn put(engine: &mut impl Engine, draft: &HookDraft, expect: Option<&str>) -> Result<HookPut, StoreError> {
    let draft = checked_hook(draft)?;

    engine.write(|db| put_in(db, &draft, expect))
}

pub(crate) fn delete(engine: &mut impl Engine, id: &str, expect: Option<&str>) -> Result<HookDelete, StoreError> {
    engine.write(|db| {
        let Some(stored) = stored_body(db, id)? else {
            return Ok(HookDelete::Missing);
        };

        if expect.is_some_and(|expect| hook_sha256(&stored.1) != expect) {
            return Ok(HookDelete::Changed);
        }

        db.execute("DELETE FROM hooks WHERE id = ?1", args![id])?;
        Ok(HookDelete::Deleted)
    })
}

fn put_in(db: &mut dyn Sql, draft: &HookDraft, expect: Option<&str>) -> Result<HookPut, StoreError> {
    let stored = stored_body(db, &draft.id)?;

    match (expect, &stored) {
        (None, Some(_)) => return Ok(HookPut::Exists),
        (Some(_), None) => return Ok(HookPut::Missing),
        (Some(expect), Some((_, body))) if hook_sha256(body) != expect => return Ok(HookPut::Changed),
        _ => {}
    }

    let moves = stored.as_ref().is_none_or(|(repo, _)| *repo != draft.repo);

    if moves {
        let count: i64 = db
            .value(
                "SELECT count(*) FROM hooks WHERE repo IS ?1 AND id <> ?2",
                args![draft.repo.as_deref(), &draft.id],
            )?
            .unwrap_or(0);

        if is_full(count) {
            return Ok(HookPut::Full);
        }
    }

    let now = now();
    let sha256 = hook_sha256(&draft.body);

    if stored.is_some() {
        db.execute(
            "UPDATE hooks SET repo = ?2, body = ?3, sha256 = ?4, author = ?5, updated_at = ?6 WHERE id = ?1",
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
            "INSERT INTO hooks (id, repo, body, sha256, author, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
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

    let saved =
        load(db, &draft.id)?.ok_or_else(|| StoreError::Backend("the saved hook could not be read back".to_owned()))?;

    Ok(HookPut::Saved(saved))
}

/// The repository and body of row `id` (the body cut one character over its limit), whatever
/// they hold.
fn stored_body(db: &mut dyn Sql, id: &str) -> Result<Option<(Option<String>, String)>, StoreError> {
    let sql = format!(
        "SELECT repo, substr(body, 1, {}) FROM hooks WHERE id = ?1",
        HOOK_BODY_MAX + 1
    );

    db.row(&sql, args![id], |row| Ok((row.get(0)?, row.get(1)?)))
}

/// Hook `id`; a row [`read_back`] leaves out counts as missing.
fn load(db: &mut dyn Sql, id: &str) -> Result<Option<TeamHook>, StoreError> {
    let sql = format!("SELECT {} FROM hooks WHERE id = ?1", columns());

    Ok(db.query(&sql, args![id])?.iter().find_map(hook_from_row))
}

/// A row of [`columns`] as a hook; `None` (noted) when it cannot be read or [`read_back`] leaves
/// it out.
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
        id: row.get(0)?,
        repo: row.get(1)?,
        author: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        body: row.get(5)?,
    })
}

impl HookStore for SqliteStore {
    fn list_hooks(&mut self, repo: Option<&str>, offset: usize, limit: usize) -> Result<HookPage, StoreError> {
        list(self, repo, offset, limit)
    }

    fn get_hook(&mut self, id: &str) -> Result<Option<TeamHook>, StoreError> {
        get(self, id)
    }

    fn put_hook(&mut self, draft: &HookDraft, expect: Option<&str>) -> Result<HookPut, StoreError> {
        put(self, draft, expect)
    }

    fn delete_hook(&mut self, id: &str, expect: Option<&str>) -> Result<HookDelete, StoreError> {
        delete(self, id, expect)
    }
}
