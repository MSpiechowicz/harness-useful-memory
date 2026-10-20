//! Scripts in a PostgreSQL team database: saved by name per scope, listed, looked up by id or
//! name, deleted, and counted when run. Trust in a version stays on each machine (the local
//! [`crate::store::TrustStore`]), so nothing about it is stored here. Rows are read back through
//! [`read_back`]: the stored `sha256` column is never read.

use std::path::PathBuf;

use postgres::{GenericClient, Row, Transaction};

use super::notes::scope_of;
use super::{PATH_MAX, PostgresStore, SOURCE_MAX, backend, column, cut, now, target_project, view_params};
use crate::limits::{SCRIPT_BODY_MAX, SCRIPT_DESCRIPTION_MAX, SCRIPT_NAME_MAX};
use crate::store::scripts::{Stored, read_back, skipped};
use crate::store::{Provenance, SavedScript, Script, ScriptDraft, ScriptRef, ScriptStore, ScriptSummary};
use crate::store::{StoreError, Target, View, checked_draft, script_sha256};

/// The longest language name (`python`), with room to spare.
const LANGUAGE_MAX: usize = 16;

/// The tables every script query reads.
const FROM: &str = "FROM scripts s LEFT JOIN projects p ON p.id = s.project_id";

/// The visibility condition of a view (`$1` the project id, `$2` whether global is visible).
const VISIBLE: &str = "(s.project_id = $1::bigint OR ($1::bigint = -1 AND s.project_id IS NOT NULL) OR ($2::boolean AND s.project_id IS NULL))";

/// The columns [`stored_from_row`] reads, text cut one character over its limit (see [`cut`]), so
/// [`read_back`] recognizes a value over the limit.
fn columns() -> String {
    format!(
        "s.id, s.project_id, {key}, {name}, {description}, {language}, s.timeout_secs, {source}, {root}, \
         s.via_override, s.created_at, s.updated_at, s.run_count, s.last_run_at, s.last_exit_code, {body}",
        key = cut("p.key", PATH_MAX),
        name = cut("s.name", SCRIPT_NAME_MAX),
        description = cut("s.description", SCRIPT_DESCRIPTION_MAX),
        language = cut("s.language", LANGUAGE_MAX),
        source = cut("s.source", SOURCE_MAX),
        root = cut("s.writer_root", PATH_MAX),
        body = cut("s.body", SCRIPT_BODY_MAX),
    )
}

impl ScriptStore for PostgresStore {
    fn save_script(&mut self, target: &Target, draft: &ScriptDraft) -> Result<SavedScript, StoreError> {
        let draft = checked_draft(draft)?;
        self.write(|tx| save(tx, target, &draft))
    }

    fn list_scripts(&mut self, view: &View, query: Option<&str>) -> Result<Vec<ScriptSummary>, StoreError> {
        self.read(|client| list(client, view, query))
    }

    fn get_script(&mut self, view: &View, script: &ScriptRef) -> Result<Option<Script>, StoreError> {
        self.read(|client| find(client, view, script))
    }

    fn delete_script(&mut self, view: &View, script: &ScriptRef) -> Result<bool, StoreError> {
        self.write(|tx| {
            let Some(id) = resolve(tx, view, script)? else {
                return Ok(false);
            };

            let deleted = tx
                .execute("DELETE FROM scripts WHERE id = $1", &[&id])
                .map_err(backend)?;

            Ok(deleted > 0)
        })
    }

    fn delete_scripts(&mut self, view: &View, ids: &[i64]) -> Result<usize, StoreError> {
        self.write(|tx| {
            let (repo, global) = view_params(tx, view)?;
            let sql = format!("DELETE FROM scripts s WHERE {VISIBLE} AND s.id = ANY($3::bigint[])");
            let deleted = tx.execute(&sql, &[&repo, &global, &ids]).map_err(backend)?;

            Ok(usize::try_from(deleted).unwrap_or(usize::MAX))
        })
    }

    /// A script deleted while it ran is not an error: there is nothing left to count.
    fn record_run(&mut self, id: i64, exit_code: Option<i32>, at: i64) -> Result<(), StoreError> {
        self.write(|tx| {
            tx.execute(
                "UPDATE scripts SET run_count = run_count + 1, last_run_at = $2, last_exit_code = $3 WHERE id = $1",
                &[&id, &at, &exit_code],
            )
            .map_err(backend)?;

            Ok(())
        })
    }
}

/// Inserts `draft` in the scope `target`, or replaces the script with its name there (keeping the
/// run statistics and `created_at`).
fn save(tx: &mut Transaction<'_>, target: &Target, draft: &ScriptDraft) -> Result<SavedScript, StoreError> {
    let project = target_project(tx, target)?;
    let now = now();
    let sha256 = script_sha256(draft.language, &draft.body);
    let timeout =
        i64::try_from(draft.timeout_secs).map_err(|_| StoreError::Invalid("timeout_secs is too large".to_owned()))?;

    let provenance = &draft.provenance;
    let root = provenance.root.as_ref().map(|root| root.to_string_lossy().into_owned());

    let existing = tx
        .query_opt(
            "SELECT id FROM scripts WHERE coalesce(project_id, 0) = $1 AND name = $2",
            &[&project.unwrap_or(0), &draft.name],
        )
        .map_err(backend)?;

    let (id, created) = match existing {
        Some(row) => {
            let id: i64 = column(&row, 0)?;

            tx.execute(
                "UPDATE scripts SET description = $2, language = $3, body = $4, sha256 = $5, timeout_secs = $6,
                     source = $7, writer_root = $8, via_override = $9, written_by = current_user, updated_at = $10
                 WHERE id = $1",
                &[
                    &id,
                    &draft.description,
                    &draft.language.as_str(),
                    &draft.body,
                    &sha256,
                    &timeout,
                    &provenance.source,
                    &root,
                    &provenance.via_override,
                    &now,
                ],
            )
            .map_err(backend)?;
            (id, false)
        }
        None => {
            let row = tx
                .query_one(
                    "INSERT INTO scripts (project_id, name, description, language, body, sha256, timeout_secs,
                                          source, writer_root, via_override, created_at, updated_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $11)
                     RETURNING id",
                    &[
                        &project,
                        &draft.name,
                        &draft.description,
                        &draft.language.as_str(),
                        &draft.body,
                        &sha256,
                        &timeout,
                        &provenance.source,
                        &root,
                        &provenance.via_override,
                        &now,
                    ],
                )
                .map_err(backend)?;
            (column(&row, 0)?, true)
        }
    };

    let script = load(tx, id)?.ok_or_else(|| StoreError::Backend("the saved script was not found".to_owned()))?;

    Ok(SavedScript { script, created })
}

/// The scripts visible in `view`, repository scripts first, then by name (by code point, like
/// SQLite); `query` (trimmed, blank keeps all) keeps those whose name or description contains it,
/// ignoring case.
fn list(client: &mut impl GenericClient, view: &View, query: Option<&str>) -> Result<Vec<ScriptSummary>, StoreError> {
    let (repo, global) = view_params(client, view)?;
    let query = query.map(str::trim).filter(|text| !text.is_empty());

    // `strpos`, not `LIKE`: `%` and `_` in the query are plain characters.
    let sql = format!(
        "SELECT {} {FROM}
         WHERE {VISIBLE}
           AND ($3::text IS NULL
                OR strpos(lower(s.name), lower($3::text)) > 0
                OR strpos(lower(s.description), lower($3::text)) > 0)
         ORDER BY s.project_id IS NULL, s.name COLLATE \"C\", s.id",
        columns()
    );

    let rows = client.query(&sql, &[&repo, &global, &query]).map_err(backend)?;
    Ok(rows
        .iter()
        .filter_map(script_from_row)
        .map(|script| script.summary)
        .collect())
}

/// The script `script` names when it is visible in `view` (a name: in the repository first, then
/// in the global scope); a row [`read_back`] leaves out counts as missing.
fn find(client: &mut impl GenericClient, view: &View, script: &ScriptRef) -> Result<Option<Script>, StoreError> {
    let (repo, global) = view_params(client, view)?;
    let columns = columns();

    let rows = match script {
        ScriptRef::Id(id) => {
            let sql = format!("SELECT {columns} {FROM} WHERE {VISIBLE} AND s.id = $3");
            client.query(&sql, &[&repo, &global, id])
        }
        ScriptRef::Name(name) => {
            let sql = format!("SELECT {columns} {FROM} WHERE {VISIBLE} AND s.name = $3 ORDER BY s.project_id IS NULL");
            client.query(&sql, &[&repo, &global, &name.trim()])
        }
    }
    .map_err(backend)?;

    Ok(rows.iter().find_map(script_from_row))
}

/// The id of the visible row `script` names (a name is looked up in the repository first, then in
/// the global scope), whatever the row holds.
fn resolve(client: &mut impl GenericClient, view: &View, script: &ScriptRef) -> Result<Option<i64>, StoreError> {
    let (repo, global) = view_params(client, view)?;

    let row = match script {
        ScriptRef::Id(id) => {
            let sql = format!("SELECT s.id FROM scripts s WHERE {VISIBLE} AND s.id = $3");
            client.query_opt(&sql, &[&repo, &global, id])
        }
        ScriptRef::Name(name) => {
            let sql = format!(
                "SELECT s.id FROM scripts s WHERE {VISIBLE} AND s.name = $3
                 ORDER BY s.project_id IS NULL LIMIT 1"
            );
            client.query_opt(&sql, &[&repo, &global, &name.trim()])
        }
    }
    .map_err(backend)?;

    row.map(|row| column(&row, 0)).transpose()
}

/// Script `id` with its body, whatever its scope.
fn load(client: &mut impl GenericClient, id: i64) -> Result<Option<Script>, StoreError> {
    let sql = format!("SELECT {} {FROM} WHERE s.id = $1", columns());
    let rows = client.query(&sql, &[&id]).map_err(backend)?;

    Ok(rows.iter().find_map(script_from_row))
}

/// A row of [`columns`] as a script; `None` (noted) when a column holds an unexpected type or
/// [`read_back`] leaves it out.
fn script_from_row(row: &Row) -> Option<Script> {
    match stored_from_row(row) {
        Ok(stored) => read_back(stored),
        Err(error) => {
            skipped(column(row, 0).unwrap_or_default(), &error.to_string());
            None
        }
    }
}

fn stored_from_row(row: &Row) -> Result<Stored, StoreError> {
    let root: Option<String> = column(row, 8)?;
    let exit_code: Option<i32> = column(row, 14)?;

    Ok(Stored {
        id: column(row, 0)?,
        scope: scope_of(column(row, 1)?),
        project: column(row, 2)?,
        name: column(row, 3)?,
        description: column(row, 4)?,
        language: column(row, 5)?,
        timeout_secs: column(row, 6)?,
        provenance: Provenance {
            root: root.map(PathBuf::from),
            source: column(row, 7)?,
            via_override: column(row, 9)?,
        },
        created_at: column(row, 10)?,
        updated_at: column(row, 11)?,
        run_count: column(row, 12)?,
        last_run_at: column(row, 13)?,
        last_exit_code: exit_code.map(i64::from),
        body: column(row, 15)?,
    })
}
