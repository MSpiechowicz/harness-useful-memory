//! Helpers shared by the test crates: temporary stores, project keys, drafts, an in-process server
//! (`mcp`), a client for the real executable (`stdio`), and the script store, team hook store,
//! team routine store and note list cases every backend passes (`scripts`, `hooks`, `routines`,
//! `listing`).
//!
//! Each test crate uses a different subset, hence the `dead_code` allowance.
#![allow(dead_code)]

pub mod approval;
pub mod embedder;
pub mod guards;
pub mod hooks;
pub mod listing;
pub mod mcp;
pub mod routines;
pub mod scripts;
pub mod search_cases;
pub mod stdio;

use std::path::PathBuf;

use harness_memory::store::{NoteDraft, NoteKind, ProjectKey, Provenance, SearchQuery, SqliteStore, Target, View};
use tempfile::TempDir;

/// A store on a fresh database file; keep the folder alive as long as the store.
pub fn temp_store() -> (TempDir, SqliteStore) {
    let dir = TempDir::new().expect("temp dir");
    let store = SqliteStore::open(&dir.path().join("memory.sqlite3")).expect("open store");
    (dir, store)
}

/// A repository identified by remote `host/owner/repo`.
pub fn repo_key(remote: &str) -> ProjectKey {
    ProjectKey {
        key: format!("git:{remote}"),
        root: PathBuf::from("/work").join(remote),
        remote: Some(remote.to_owned()),
    }
}

pub fn repo(remote: &str) -> Target {
    Target::Repo(repo_key(remote))
}

/// The repository and the global scope.
pub fn both(remote: &str) -> View {
    View {
        repo: Some(repo_key(remote)),
        global: true,
    }
}

pub fn global_only() -> View {
    View {
        repo: None,
        global: true,
    }
}

pub fn repo_only(remote: &str) -> View {
    View {
        repo: Some(repo_key(remote)),
        global: false,
    }
}

pub fn draft(title: &str, body: &str, kind: NoteKind) -> NoteDraft {
    NoteDraft {
        title: title.to_owned(),
        body: body.to_owned(),
        kind,
        tags: Vec::new(),
        entities: Vec::new(),
        provenance: Provenance {
            root: None,
            source: Some("tests".to_owned()),
            via_override: false,
        },
    }
}

pub fn query(text: &str, view: View) -> SearchQuery {
    SearchQuery {
        text: text.to_owned(),
        view,
        kind: None,
        limit: 8,
        checkout: None,
        exclude: Vec::new(),
    }
}
