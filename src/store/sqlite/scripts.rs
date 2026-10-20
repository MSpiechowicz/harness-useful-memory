//! Scripts in the SQLite-dialect stores (the local file and libSQL share these functions through
//! [`Engine`]), and the local file's script trust.
//!
//! A script is unique by name in its scope (`ifnull(project_id, 0)`, like note titles). The draft
//! is checked with [`checked_draft`] before the write transaction opens.

use std::path::PathBuf;

use super::SqliteStore;
use super::engine::Engine;
use super::notes::scope_of;
use super::sql::{Row, Sql, Value, args};
use super::{now, target_project, view_params};
use crate::limits::{SCRIPT_BODY_MAX, SCRIPT_DESCRIPTION_MAX, SCRIPT_NAME_MAX};
use crate::store::scripts::{Stored, read_back, skipped};
use crate::store::scripts::{TRUSTED_VERSIONS_KEPT, UNAPPROVED_KEPT};
use crate::store::{
    PreviousVersion, Target, TrustOrigin, TrustRecord, TrustStore, TrustedVersion, View, checked_draft, script_sha256,
};
use crate::store::{
    ProjectKey, Provenance, SavedScript, Script, ScriptDraft, ScriptRef, ScriptStore, ScriptSummary, StoreError,
};

/// The longest language name (`python`), with room to spare.
const LANGUAGE_MAX: usize = 16;

/// The tables every script query reads.
const FROM: &str = "FROM scripts s LEFT JOIN projects p ON p.id = s.project_id";

/// The visibility condition of a view (`?1` the project id, `?2` whether global is visible).
const VISIBLE: &str = "(s.project_id = ?1 OR (?1 = -1 AND s.project_id IS NOT NULL) OR (?2 AND s.project_id IS NULL))";

/// The columns [`stored_from_row`] reads, text cut one character over its limit and the timeout
/// read as an integer whatever was stored, so a row changed by hand cannot make a reply of any
/// size or fail a list (see [`read_back`]).
fn columns() -> String {
    format!(
        "s.id, s.project_id, p.key, substr(s.name, 1, {name}), substr(s.description, 1, {description}), \
         substr(s.language, 1, {LANGUAGE_MAX}), CAST(s.timeout_secs AS INTEGER), s.source, s.writer_root, \
         s.via_override, s.created_at, s.updated_at, s.run_count, s.last_run_at, s.last_exit_code, \
         substr(s.body, 1, {body})",
        name = SCRIPT_NAME_MAX + 1,
        description = SCRIPT_DESCRIPTION_MAX + 1,
        body = SCRIPT_BODY_MAX + 1,
    )
}

pub(crate) fn save(engine: &mut impl Engine, target: &Target, draft: &ScriptDraft) -> Result<SavedScript, StoreError> {
    let draft = checked_draft(draft)?;
    let sha256 = script_sha256(draft.language, &draft.body);

    engine.write(|db| save_in(db, target, &draft, &sha256))
}

/// The visible scripts, repository scripts first, then by name. The query is matched here rather
/// than in SQL, whose `lower()` folds ASCII only.
pub(crate) fn list(
    engine: &mut impl Engine,
    view: &View,
    query: Option<&str>,
) -> Result<Vec<ScriptSummary>, StoreError> {
    let needle = query
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_lowercase);

    let scripts = engine.read(|db| {
        let (repo, global) = view_params(db, view)?;
        let sql = format!(
            "SELECT {} {FROM} WHERE {VISIBLE} ORDER BY s.project_id IS NULL, s.name",
            columns()
        );

        let rows = db.query(&sql, args![repo, global])?;
        Ok(rows
            .iter()
            .filter_map(script_from_row)
            .map(|script| script.summary)
            .collect::<Vec<_>>())
    })?;

    let Some(needle) = needle else {
        return Ok(scripts);
    };

    Ok(scripts
        .into_iter()
        .filter(|script| {
            script.name.to_lowercase().contains(&needle) || script.description.to_lowercase().contains(&needle)
        })
        .collect())
}

pub(crate) fn get(engine: &mut impl Engine, view: &View, script: &ScriptRef) -> Result<Option<Script>, StoreError> {
    engine.read(|db| find(db, view, script))
}

pub(crate) fn delete(engine: &mut impl Engine, view: &View, script: &ScriptRef) -> Result<bool, StoreError> {
    engine.write(|db| {
        let Some(id) = resolve(db, view, script)? else {
            return Ok(false);
        };

        db.execute("DELETE FROM scripts WHERE id = ?1", args![id])?;
        Ok(true)
    })
}

/// The most ids one statement of [`delete_many`] binds.
const DELETE_CHUNK: usize = 500;

/// Deletes the scripts among `ids` visible in `view`, in one write transaction.
pub(crate) fn delete_many(engine: &mut impl Engine, view: &View, ids: &[i64]) -> Result<usize, StoreError> {
    engine.write(|db| {
        let (repo, global) = view_params(db, view)?;
        let mut deleted = 0;

        for chunk in ids.chunks(DELETE_CHUNK) {
            let marks: Vec<String> = (3..chunk.len() + 3).map(|index| format!("?{index}")).collect();
            let mut values = vec![Value::from(repo), Value::from(global)];
            values.extend(chunk.iter().map(|id| Value::from(*id)));

            let sql = format!(
                "DELETE FROM scripts AS s WHERE {VISIBLE} AND s.id IN ({})",
                marks.join(", ")
            );
            deleted += db.execute(&sql, &values)?.changes;
        }

        Ok(deleted)
    })
}

/// Counts a run; a script deleted meanwhile is no error.
pub(crate) fn record_run(engine: &mut impl Engine, id: i64, exit_code: Option<i32>, at: i64) -> Result<(), StoreError> {
    engine.write(|db| {
        db.execute(
            "UPDATE scripts SET run_count = run_count + 1, last_run_at = ?2, last_exit_code = ?3 WHERE id = ?1",
            args![id, at, exit_code.map(i64::from)],
        )?;
        Ok(())
    })
}

fn save_in(db: &mut dyn Sql, target: &Target, draft: &ScriptDraft, sha256: &str) -> Result<SavedScript, StoreError> {
    let project = target_project(db, target)?;
    let now = now();
    let timeout = i64::try_from(draft.timeout_secs)
        .map_err(|_| StoreError::Invalid("timeout_secs is out of range".to_owned()))?;
    let writer = &draft.provenance;
    let root = writer.root.as_ref().map(|root| root.to_string_lossy().into_owned());

    let existing: Option<i64> = db.value(
        "SELECT id FROM scripts WHERE ifnull(project_id, 0) = ?1 AND name = ?2",
        args![project.unwrap_or(0), &draft.name],
    )?;

    let (id, created) = match existing {
        Some(id) => {
            db.execute(
                "UPDATE scripts SET description = ?2, language = ?3, body = ?4, sha256 = ?5, timeout_secs = ?6,
                     source = ?7, writer_root = ?8, via_override = ?9, updated_at = ?10
                 WHERE id = ?1",
                args![
                    id,
                    &draft.description,
                    draft.language.as_str(),
                    &draft.body,
                    sha256,
                    timeout,
                    writer.source.as_deref(),
                    root,
                    writer.via_override,
                    now
                ],
            )?;
            (id, false)
        }
        None => {
            let inserted = db.execute(
                "INSERT INTO scripts (project_id, name, description, language, body, sha256, timeout_secs,
                                      source, writer_root, via_override, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
                args![
                    project,
                    &draft.name,
                    &draft.description,
                    draft.language.as_str(),
                    &draft.body,
                    sha256,
                    timeout,
                    writer.source.as_deref(),
                    root,
                    writer.via_override,
                    now
                ],
            )?;
            (inserted.last_insert_rowid, true)
        }
    };

    let sql = format!("SELECT {} {FROM} WHERE s.id = ?1", columns());
    let script = db
        .query(&sql, args![id])?
        .iter()
        .find_map(script_from_row)
        .ok_or_else(|| StoreError::Backend("the saved script could not be read back".to_owned()))?;

    Ok(SavedScript { script, created })
}

/// The script `script` names when it is visible in `view`; a name resolves in the repository
/// first, then in the global scope. A row [`read_back`] leaves out counts as missing.
fn find(db: &mut dyn Sql, view: &View, script: &ScriptRef) -> Result<Option<Script>, StoreError> {
    let (repo, global) = view_params(db, view)?;
    let columns = columns();

    let rows = match script {
        ScriptRef::Id(id) => {
            let sql = format!("SELECT {columns} {FROM} WHERE s.id = ?3 AND {VISIBLE}");
            db.query(&sql, args![repo, global, *id])?
        }
        ScriptRef::Name(name) => {
            let sql = format!("SELECT {columns} {FROM} WHERE s.name = ?3 AND {VISIBLE} ORDER BY s.project_id IS NULL");
            db.query(&sql, args![repo, global, name.trim()])?
        }
    };

    Ok(rows.iter().find_map(script_from_row))
}

/// The id of the visible row `script` names (repository first), whatever it holds.
fn resolve(db: &mut dyn Sql, view: &View, script: &ScriptRef) -> Result<Option<i64>, StoreError> {
    let (repo, global) = view_params(db, view)?;

    match script {
        ScriptRef::Id(id) => {
            let sql = format!("SELECT s.id FROM scripts s WHERE s.id = ?3 AND {VISIBLE}");
            db.value(&sql, args![repo, global, *id])
        }
        ScriptRef::Name(name) => {
            let sql = format!(
                "SELECT s.id FROM scripts s WHERE s.name = ?3 AND {VISIBLE} ORDER BY s.project_id IS NULL LIMIT 1"
            );
            db.value(&sql, args![repo, global, name.trim()])
        }
    }
}

/// A row of [`columns`] as a script; `None` (noted) when it cannot be read or [`read_back`]
/// leaves it out.
fn script_from_row(row: &Row) -> Option<Script> {
    match stored_from_row(row) {
        Ok(stored) => read_back(stored),
        Err(error) => {
            skipped(row.get(0).unwrap_or_default(), &error.to_string());
            None
        }
    }
}

fn stored_from_row(row: &Row) -> Result<Stored, StoreError> {
    let root: Option<String> = row.get(8)?;

    Ok(Stored {
        id: row.get(0)?,
        scope: scope_of(row.get(1)?),
        project: row.get(2)?,
        name: row.get(3)?,
        description: row.get(4)?,
        language: row.get(5)?,
        timeout_secs: row.get(6)?,
        provenance: Provenance {
            root: root.map(PathBuf::from),
            source: row.get(7)?,
            via_override: row.get(9)?,
        },
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        run_count: row.get(12)?,
        last_run_at: row.get(13)?,
        last_exit_code: row.get(14)?,
        body: row.get(15)?,
    })
}

impl SqliteStore {
    /// Every script of the file, every repository's, oldest first, with its repository (`None` for a
    /// global script): what `scripts push` copies and `scripts discard-local` deletes.
    pub fn every_script(&mut self) -> Result<Vec<(Script, Option<ProjectKey>)>, StoreError> {
        self.read(|db| {
            let sql = format!("SELECT {}, p.root_path, p.remote {FROM} ORDER BY s.id", columns());
            let rows = db.query(&sql, &[])?;
            let mut scripts = Vec::with_capacity(rows.len());

            for row in &rows {
                let Some(script) = script_from_row(row) else {
                    continue;
                };

                let project = match &script.summary.project {
                    Some(key) => {
                        let root: Option<String> = row.get(16)?;

                        Some(ProjectKey {
                            key: key.clone(),
                            root: PathBuf::from(root.unwrap_or_default()),
                            remote: row.get(17)?,
                        })
                    }
                    None => None,
                };

                scripts.push((script, project));
            }

            Ok(scripts)
        })
    }

    /// How many scripts the file holds per scope, every repository's: `(repository, global)`.
    pub fn script_scope_counts(&mut self) -> Result<(usize, usize), StoreError> {
        let rows = self.read(|db| db.query("SELECT project_id IS NULL, count(*) FROM scripts GROUP BY 1", &[]))?;
        let mut counts = (0, 0);

        for row in &rows {
            let is_global: i64 = row.get(0)?;
            let count = usize::try_from(row.get::<i64>(1)?).unwrap_or(0);

            if is_global == 0 {
                counts.0 += count;
            } else {
                counts.1 += count;
            }
        }

        Ok(counts)
    }
}

impl ScriptStore for SqliteStore {
    fn save_script(&mut self, target: &Target, draft: &ScriptDraft) -> Result<SavedScript, StoreError> {
        save(self, target, draft)
    }

    fn list_scripts(&mut self, view: &View, query: Option<&str>) -> Result<Vec<ScriptSummary>, StoreError> {
        list(self, view, query)
    }

    fn get_script(&mut self, view: &View, script: &ScriptRef) -> Result<Option<Script>, StoreError> {
        get(self, view, script)
    }

    fn delete_script(&mut self, view: &View, script: &ScriptRef) -> Result<bool, StoreError> {
        delete(self, view, script)
    }

    fn delete_scripts(&mut self, view: &View, ids: &[i64]) -> Result<usize, StoreError> {
        delete_many(self, view, ids)
    }

    fn record_run(&mut self, id: i64, exit_code: Option<i32>, at: i64) -> Result<(), StoreError> {
        record_run(self, id, exit_code, at)
    }
}

impl TrustStore for SqliteStore {
    fn trust_of(&mut self, place: &str, script_id: i64, sha256: &str) -> Result<Option<TrustRecord>, StoreError> {
        self.read(|db| {
            db.row(
                "SELECT origin, name, scope, project FROM script_trust
                 WHERE place = ?1 AND script_id = ?2 AND sha256 = ?3",
                args![place, script_id, sha256],
                |row| {
                    let origin: String = row.get(0)?;

                    Ok(TrustRecord {
                        origin: TrustOrigin::parse(&origin),
                        name: row.get(1)?,
                        scope: row.get(2)?,
                        project: row.get(3)?,
                    })
                },
            )
        })
    }

    /// Replaces the row of a version trusted before, so it becomes the script's newest, then drops
    /// the script's versions past the newest [`TRUSTED_VERSIONS_KEPT`].
    fn trust(&mut self, version: &TrustedVersion) -> Result<(), StoreError> {
        let sha256 = script_sha256(version.language, version.body);

        self.write(|db| {
            db.execute(
                "DELETE FROM script_trust WHERE place = ?1 AND script_id = ?2 AND sha256 = ?3",
                args![version.place, version.script_id, &sha256],
            )?;

            db.execute(
                "INSERT INTO script_trust
                     (place, script_id, sha256, trusted_at, name, scope, project, language, body, origin)
                 VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), ?4, ?5, ?6, ?7, ?8, ?9)",
                args![
                    version.place,
                    version.script_id,
                    &sha256,
                    version.name,
                    version.scope.as_str(),
                    version.project,
                    version.language.as_str(),
                    version.body,
                    version.origin.as_str()
                ],
            )?;

            db.execute(
                "DELETE FROM script_trust WHERE place = ?1 AND script_id = ?2 AND rowid NOT IN (
                     SELECT rowid FROM script_trust WHERE place = ?1 AND script_id = ?2
                     ORDER BY trusted_at DESC, rowid DESC LIMIT ?3)",
                args![version.place, version.script_id, TRUSTED_VERSIONS_KEPT],
            )?;
            Ok(())
        })
    }

    fn previous(&mut self, place: &str, script_id: i64) -> Result<Option<PreviousVersion>, StoreError> {
        self.read(|db| {
            db.row(
                "SELECT body, language, name, scope, trusted_at, origin FROM script_trust
                 WHERE place = ?1 AND script_id = ?2 ORDER BY trusted_at DESC, rowid DESC LIMIT 1",
                args![place, script_id],
                |row| {
                    let origin: String = row.get(5)?;

                    Ok(PreviousVersion {
                        body: row.get(0)?,
                        language: row.get(1)?,
                        name: row.get(2)?,
                        scope: row.get(3)?,
                        trusted_at: row.get(4)?,
                        origin: TrustOrigin::parse(&origin),
                    })
                },
            )
        })
    }

    /// Writes only for a version not remembered yet, so listing the same scripts again costs one
    /// read each.
    fn note_unapproved(&mut self, sha256: &str) -> Result<(), StoreError> {
        if self.was_unapproved(&[sha256.to_owned()])? {
            return Ok(());
        }

        self.write(|db| {
            db.execute(
                "INSERT OR IGNORE INTO script_seen_unapproved (sha256, seen_at) VALUES (?1, ?2)",
                args![sha256, now()],
            )?;

            db.execute(
                "DELETE FROM script_seen_unapproved WHERE sha256 NOT IN (
                     SELECT sha256 FROM script_seen_unapproved ORDER BY seen_at DESC LIMIT ?1)",
                args![UNAPPROVED_KEPT],
            )?;
            Ok(())
        })
    }

    fn was_unapproved(&mut self, sha256s: &[String]) -> Result<bool, StoreError> {
        self.read(|db| {
            for sha256 in sha256s {
                let seen: Option<i64> =
                    db.value("SELECT 1 FROM script_seen_unapproved WHERE sha256 = ?1", args![sha256])?;

                if seen.is_some() {
                    return Ok(true);
                }
            }

            Ok(false)
        })
    }
}
