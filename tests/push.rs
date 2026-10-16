//! `harness-memory push` into a store reached through `MemoryStore` (a second SQLite file here;
//! `postgres.rs` runs the same scenario on PostgreSQL).

mod common;
mod push_support;

use std::collections::BTreeSet;

use common::{global_only, query, repo_only};
use harness_memory::push::{Options, push};
use harness_memory::store::{MemoryStore, SqliteStore, StoreError};
use push_support::{all, counts, local_memory};
use tempfile::TempDir;

#[test]
fn pushing_into_another_sqlite_store_is_idempotent() {
    let dir = TempDir::new().expect("temp dir");
    let mut remote = SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team store");

    push_support::scenario(&mut remote);
}

#[test]
fn a_push_can_take_one_repository_and_leave_global_out() {
    let dir = TempDir::new().expect("temp dir");
    let mut remote = SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team store");

    push_support::narrowed_scenario(&mut remote);
}

#[test]
fn a_push_can_leave_every_repository_out() {
    let dir = TempDir::new().expect("temp dir");
    let mut remote = SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team store");
    let local = local_memory();

    let without_repositories = Options {
        skip_repositories: true,
        ..all()
    };
    let pushed = push(&local.path, &mut remote, &without_repositories).expect("global only");
    assert_eq!(counts(&pushed).notes, [1, 0, 0, 0]);
    assert_eq!(counts(&pushed).links, [1, 0]);
    assert_eq!(pushed.projects, BTreeSet::from(["global".to_owned()]));

    let repository = remote
        .search(&query("", repo_only(push_support::REPO)))
        .expect("repository notes");
    assert!(repository.is_empty(), "{repository:?}");
    assert_eq!(remote.search(&query("", global_only())).expect("global").len(), 1);
}

#[test]
fn pushing_again_does_not_rewrite_the_relations() {
    let dir = TempDir::new().expect("temp dir");
    let team = dir.path().join("team.sqlite3");
    let mut remote = SqliteStore::open(&team).expect("team store");
    let local = local_memory();

    push(&local.path, &mut remote, &all()).expect("first push");

    let raw = rusqlite::Connection::open(&team).expect("raw team");
    raw.execute("UPDATE relations SET updated_at = 1", [])
        .expect("age the relations");

    let again = push(&local.path, &mut remote, &all()).expect("second push");
    assert_eq!(counts(&again).links, [0, 2]);

    let touched: i64 = raw
        .query_row("SELECT count(*) FROM relations WHERE updated_at <> 1", [], |row| {
            row.get(0)
        })
        .expect("count");
    assert_eq!(touched, 0, "an unchanged relation keeps its update time");
}

#[test]
fn a_missing_local_database_is_an_error_and_is_not_created() {
    let dir = TempDir::new().expect("temp dir");
    let missing = dir.path().join("memory.sqlite3");
    let mut remote = SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team store");

    let error = push(&missing, &mut remote, &all()).expect_err("no local database");
    assert!(matches!(error, StoreError::NotFound(_)), "{error:?}");
    assert!(!missing.exists());
}
