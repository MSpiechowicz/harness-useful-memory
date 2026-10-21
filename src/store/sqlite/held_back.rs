//! Script versions held back from team trust on this computer
//! ([`crate::store::TrustStore::hold_back`]): the scripts an agent saved while a save guard
//! applied, and the held scripts the user saved without approving them. Such a version needs the
//! user's approval before it runs even while every team script is trusted, until the user trusts
//! it (a [`crate::store::TrustOrigin::Approved`] trust releases it). Nothing else forgets a row.
//!
//! The table `script_held_back` is in the local file only: it is not part of the shared schema
//! ([`super::schema`], which also migrates libSQL team databases), so it is created here with
//! `CREATE TABLE IF NOT EXISTS` on the first write and leaves `user_version` alone; reading
//! without it holds nothing back. A row names the place (`local` or the team database's key), the
//! script's name, its version ([`crate::store::script_sha256`]) and, once saved, its id there: a
//! version matches by id, so a script renamed in the team database stays held back, or by a row
//! without an id written before the save, so a save whose id could not be recorded is held back
//! by its name.

use super::now;
use super::sql::{Sql, args};
use crate::store::StoreError;

const CREATE: &str = "
CREATE TABLE IF NOT EXISTS script_held_back (
    place TEXT NOT NULL,
    script_id INTEGER,
    name TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    held_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS script_held_back_version
    ON script_held_back (place, sha256, ifnull(script_id, -1), name);
";

/// The rows of version `?2` of script `?3` (named `?4`) in place `?1`.
const MATCHES: &str = "place = ?1 AND sha256 = ?2 AND (script_id = ?3 OR (script_id IS NULL AND name = ?4))";

/// Holds back version `sha256` of the script `name` in `place`, as script `script_id` there when
/// known.
pub(super) fn hold_back(
    db: &mut dyn Sql,
    place: &str,
    script_id: Option<i64>,
    name: &str,
    sha256: &str,
) -> Result<(), StoreError> {
    db.batch(CREATE)?;
    db.execute(
        "INSERT OR IGNORE INTO script_held_back (place, script_id, name, sha256, held_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        args![place, script_id, name, sha256, now()],
    )?;
    Ok(())
}

/// True when version `sha256` of script `script_id` (named `name`) in `place` is held back.
pub(super) fn is_held_back(
    db: &mut dyn Sql,
    place: &str,
    script_id: i64,
    name: &str,
    sha256: &str,
) -> Result<bool, StoreError> {
    if !exists(db)? {
        return Ok(false);
    }

    let found: Option<i64> = db.value(
        &format!("SELECT 1 FROM script_held_back WHERE {MATCHES} LIMIT 1"),
        args![place, sha256, script_id, name],
    )?;
    Ok(found.is_some())
}

/// Releases version `sha256` of script `script_id` (named `name`) in `place`: the user trusted it.
pub(super) fn release(
    db: &mut dyn Sql,
    place: &str,
    script_id: i64,
    name: &str,
    sha256: &str,
) -> Result<(), StoreError> {
    if exists(db)? {
        db.execute(
            &format!("DELETE FROM script_held_back WHERE {MATCHES}"),
            args![place, sha256, script_id, name],
        )?;
    }

    Ok(())
}

fn exists(db: &mut dyn Sql) -> Result<bool, StoreError> {
    let found: Option<i64> = db.value(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'script_held_back'",
        &[],
    )?;
    Ok(found.is_some())
}
