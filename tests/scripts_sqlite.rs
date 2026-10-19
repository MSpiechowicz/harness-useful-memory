//! Scripts and script trust in the local SQLite file: the shared script cases
//! (`common::scripts`), the version 2 to 3 migration, case folding beyond ASCII, and trust.

mod common;

use std::path::PathBuf;

use common::scripts::{self, REPO, script};
use common::{both, draft, query, repo, temp_store};
use harness_memory::limits::{SCRIPT_BODY_MAX, SCRIPT_DESCRIPTION_MAX, SCRIPT_TIMEOUT_MAX};
use harness_memory::store::sqlite::schema::{SCHEMA_VERSION, V1, V2, user_version};
use harness_memory::store::{
    Language, Scope, ScriptRef, Target, TrustOrigin, TrustRecord, TrustedVersion, script_sha256,
};
use harness_memory::store::{MemoryStore, NoteKind, Provenance, ScriptStore, ScriptSummary, SqliteStore, TrustStore};
use pretty_assertions::assert_eq;
use tempfile::TempDir;

/// `cases!(name)` runs `common::scripts::name` on a fresh local file.
macro_rules! cases {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                let (_dir, mut store) = temp_store();
                scripts::$name(&mut store);
            }
        )*
    };
}

cases!(
    saving_upserts_by_name,
    bad_drafts_store_nothing,
    lists_repo_first_then_by_name,
    queries_match_name_or_description_ignoring_case,
    views_decide_what_is_visible,
    names_resolve_repo_first_and_delete,
    runs_are_counted,
    a_run_of_a_missing_script_is_no_error,
    deleting_many_takes_only_visible_ids,
);

#[test]
fn every_case_passes_through_conformance() {
    let mut dirs: Vec<TempDir> = Vec::new();

    scripts::conformance(&mut || {
        let (dir, store) = temp_store();
        dirs.push(dir);
        Box::new(store)
    });
}

#[test]
fn a_version_2_database_is_migrated_and_keeps_its_notes() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("memory.sqlite3");

    let conn = rusqlite::Connection::open(&path).expect("raw");
    conn.execute_batch(V1).expect("v1 schema");
    conn.execute_batch(V2).expect("v2 schema");
    conn.execute_batch(
        "INSERT INTO projects (key, root_path, remote, created_at, last_seen_at)
             VALUES ('git:github.com/acme/widget', '/work/github.com/acme/widget', 'github.com/acme/widget', 1, 1);
         INSERT INTO notes (project_id, kind, title, title_norm, body, source, created_at, updated_at, writer_root)
             VALUES (1, 'location', 'Build cache', 'build cache', 'Lives in target/', 'omp', 1, 1, '/elsewhere');
         PRAGMA user_version = 2;",
    )
    .expect("v2 rows");
    drop(conn);

    let mut store = SqliteStore::open(&path).expect("open migrates");
    let hits = store.search(&query("cache", both(REPO))).expect("search");
    assert_eq!(hits.len(), 1);

    let notes = store.get(&both(REPO), &[1]).expect("get");
    assert_eq!(
        notes[0].provenance,
        Provenance {
            root: Some(PathBuf::from("/elsewhere")),
            source: Some("omp".to_owned()),
            via_override: false,
        }
    );

    let saved = store
        .save_script(&repo(REPO), &script("open-prs", "", "gh pr list\n"))
        .expect("save");
    assert_eq!(
        saved.script.summary.project.as_deref(),
        Some("git:github.com/acme/widget")
    );
    assert!(
        !store
            .is_trusted("local", saved.script.summary.id, &saved.script.summary.sha256)
            .expect("trust table")
    );

    // The new tables live next to the notes and leave them alone.
    store
        .remember(&repo(REPO), &draft("Another", "Body", NoteKind::Note))
        .expect("remember");
    drop(store);

    let conn = rusqlite::Connection::open(&path).expect("raw");
    assert_eq!(user_version(&conn).expect("version"), SCHEMA_VERSION);
    assert_eq!(SCHEMA_VERSION, 5);
}

#[test]
fn queries_fold_case_beyond_ascii() {
    let (_dir, mut store) = temp_store();
    store
        .save_script(&repo(REPO), &script("powitanie", "Zażółć GĘŚLĄ jaźń", "echo hi\n"))
        .expect("save");

    let found = store.list_scripts(&both(REPO), Some("gęślą")).expect("list");
    assert_eq!(found.len(), 1);
}

/// Version `body` of global script 1 in the team database, trusted as `origin`.
fn version(body: &str, origin: TrustOrigin) -> TrustedVersion<'_> {
    TrustedVersion {
        place: "team",
        script_id: 1,
        name: "open-prs",
        scope: Scope::Global,
        project: None,
        language: Language::Bash,
        body,
        origin,
    }
}

#[test]
fn trust_is_per_script_and_a_new_trust_refreshes_the_version() {
    let (dir, mut store) = temp_store();
    let sha = script_sha256(Language::Bash, "gh pr list\n");
    let other = script_sha256(Language::Bash, "gh pr list --json\n");

    assert_eq!(store.trust_of("team", 1, &sha).expect("check"), None);
    assert_eq!(store.previous("team", 1).expect("previous"), None);

    store
        .trust(&version("gh pr list\n", TrustOrigin::Saved))
        .expect("trust");
    store
        .trust(&version("gh pr list\n", TrustOrigin::Approved))
        .expect("trust again");
    assert_eq!(
        store.trust_of("team", 1, &sha).expect("check"),
        Some(TrustRecord {
            origin: TrustOrigin::Approved,
            name: "open-prs".to_owned(),
            scope: "global".to_owned(),
            project: None,
        })
    );
    assert!(store.is_trusted("team", 1, &sha).expect("check"));
    assert!(!store.is_trusted("team", 1, &other).expect("check"));

    // The same body is not trusted for another script, another place or another team database.
    assert!(!store.is_trusted("team", 2, &sha).expect("check"));
    assert!(!store.is_trusted("local", 1, &sha).expect("check"));
    assert!(!store.is_trusted("team:0123456789abcdef", 1, &sha).expect("check"));
    drop(store);

    let mut reopened = SqliteStore::open(&dir.path().join("memory.sqlite3")).expect("reopen");
    assert!(reopened.is_trusted("team", 1, &sha).expect("check"));

    let previous = reopened.previous("team", 1).expect("previous").expect("one version");
    assert_eq!(
        (previous.body.as_str(), previous.language.as_str(), previous.origin),
        ("gh pr list\n", "bash", TrustOrigin::Approved)
    );
    assert_eq!(
        (previous.name.as_str(), previous.scope.as_str()),
        ("open-prs", "global")
    );
    assert!(
        previous.trusted_at.ends_with('Z') && previous.trusted_at.contains('T'),
        "{}",
        previous.trusted_at
    );
    assert_eq!(reopened.previous("local", 1).expect("other place"), None);
    assert_eq!(reopened.previous("team", 2).expect("other script"), None);

    let conn = rusqlite::Connection::open(dir.path().join("memory.sqlite3")).expect("raw");
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM script_trust", [], |row| row.get(0))
        .expect("count");
    assert_eq!(rows, 1);
}

#[test]
fn trusting_an_old_version_again_makes_it_the_latest() {
    let (_dir, mut store) = temp_store();

    for body in ["echo 1\n", "echo 2\n", "echo 1\n"] {
        store.trust(&version(body, TrustOrigin::Approved)).expect("trust");
    }

    let previous = store.previous("team", 1).expect("previous").expect("kept");
    assert_eq!(previous.body, "echo 1\n");
}

#[test]
fn only_the_last_five_versions_of_a_script_stay_trusted() {
    let (_dir, mut store) = temp_store();
    let bodies: Vec<String> = (1..=7).map(|n| format!("echo {n}\n")).collect();

    for body in &bodies {
        store.trust(&version(body, TrustOrigin::Approved)).expect("trust");
    }

    let other = TrustedVersion {
        script_id: 2,
        ..version("echo other\n", TrustOrigin::Saved)
    };
    store.trust(&other).expect("another script");

    let trusted: Vec<bool> = bodies
        .iter()
        .map(|body| {
            store
                .is_trusted("team", 1, &script_sha256(Language::Bash, body))
                .expect("check")
        })
        .collect();
    assert_eq!(trusted, [false, false, true, true, true, true, true]);
    assert!(
        store
            .is_trusted("team", 2, &script_sha256(Language::Bash, "echo other\n"))
            .expect("check")
    );

    let previous = store.previous("team", 1).expect("previous").expect("kept");
    assert_eq!(previous.body, "echo 7\n");
}

#[test]
fn rows_changed_by_hand_are_clamped_or_left_out_without_breaking_the_list() {
    let (dir, mut store) = temp_store();
    for name in ["bulky", "fine", "huge", "negative", "ruby", "soon", "wordy"] {
        store
            .save_script(&Target::Global, &script(name, "", "true"))
            .expect("save");
    }

    let conn = rusqlite::Connection::open(dir.path().join("memory.sqlite3")).expect("raw");
    conn.execute_batch(&format!(
        "PRAGMA ignore_check_constraints = ON;
         UPDATE scripts SET timeout_secs = -5 WHERE name = 'negative';
         UPDATE scripts SET timeout_secs = 9223372036854775807 WHERE name = 'huge';
         UPDATE scripts SET timeout_secs = 'soon' WHERE name = 'soon';
         UPDATE scripts SET language = 'ruby' WHERE name = 'ruby';
         UPDATE scripts SET description = '{}' WHERE name = 'wordy';
         UPDATE scripts SET body = '{}' WHERE name = 'bulky';",
        "d".repeat(SCRIPT_DESCRIPTION_MAX + 1),
        "a".repeat(SCRIPT_BODY_MAX + 1),
    ))
    .expect("tamper");

    let listed: Vec<(String, u64)> = store
        .list_scripts(&both(REPO), None)
        .expect("one bad row does not fail the list")
        .into_iter()
        .map(|summary| (summary.name, summary.timeout_secs))
        .collect();
    assert_eq!(
        listed,
        [
            ("fine".to_owned(), 60),
            ("huge".to_owned(), SCRIPT_TIMEOUT_MAX),
            ("negative".to_owned(), 1),
            ("soon".to_owned(), 1),
        ]
    );

    for name in ["bulky", "ruby", "wordy"] {
        let found = store
            .get_script(&both(REPO), &ScriptRef::Name(name.to_owned()))
            .expect("get");
        assert_eq!(found, None, "{name}");
    }
}

#[test]
fn listed_hashes_come_from_the_language_and_body_read_never_the_stored_column() {
    let (dir, mut store) = temp_store();
    let saved = store
        .save_script(&Target::Global, &script("edited", "", "echo original\n"))
        .expect("save");
    assert_eq!(
        saved.script.summary.sha256,
        script_sha256(Language::Bash, "echo original\n")
    );

    let conn = rusqlite::Connection::open(dir.path().join("memory.sqlite3")).expect("raw");
    conn.execute(
        "UPDATE scripts SET body = 'echo tampered', language = 'sh' WHERE name = 'edited'",
        [],
    )
    .expect("tamper");

    let listed = store.list_scripts(&both(REPO), None).expect("list");
    assert_eq!(listed[0].sha256, script_sha256(Language::Sh, "echo tampered"));
}

#[test]
fn a_version_is_trusted_as_the_script_it_was_then() {
    let (_dir, mut store) = temp_store();
    let saved = store
        .save_script(&repo(REPO), &script("open-prs", "", "gh pr list\n"))
        .expect("save");
    let place = "local";
    store
        .trust(&TrustedVersion::of(place, &saved.script, TrustOrigin::Saved))
        .expect("trust");

    let summary = &saved.script.summary;
    let record = store
        .trust_of(place, summary.id, &summary.sha256)
        .expect("check")
        .expect("trusted");
    assert_eq!(
        (record.scope.as_str(), record.project.as_deref()),
        ("repo", Some("git:github.com/acme/widget"))
    );
    assert!(record.is_for(summary));

    // A teammate renames the script or moves it to another scope or repository.
    let renamed = ScriptSummary {
        name: "list-prs".to_owned(),
        ..summary.clone()
    };
    let global = ScriptSummary {
        scope: Scope::Global,
        project: None,
        ..summary.clone()
    };
    let elsewhere = ScriptSummary {
        project: Some("git:github.com/acme/gadget".to_owned()),
        ..summary.clone()
    };
    for moved in [renamed, global, elsewhere] {
        assert!(!record.is_for(&moved), "{moved:?}");
    }
}

#[test]
fn versions_reported_as_waiting_for_approval_are_remembered() {
    let (dir, mut store) = temp_store();
    let sha = script_sha256(Language::Sh, "touch x");
    let other = script_sha256(Language::Bash, "touch x");

    assert!(!store.was_unapproved(std::slice::from_ref(&sha)).expect("check"));
    store.note_unapproved(&sha).expect("note");
    store.note_unapproved(&sha).expect("noted again");
    assert!(store.was_unapproved(&[other.clone(), sha.clone()]).expect("check"));
    assert!(!store.was_unapproved(&[other]).expect("check"));
    drop(store);

    let mut reopened = SqliteStore::open(&dir.path().join("memory.sqlite3")).expect("reopen");
    assert!(reopened.was_unapproved(&[sha]).expect("kept"));
}
