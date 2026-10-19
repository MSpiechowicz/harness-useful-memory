//! `harness-memory push` into a store reached through `MemoryStore` (a second SQLite file here;
//! `postgres.rs` runs the same scenario on PostgreSQL).

mod common;
mod libsql_support;
mod mongo_support;
mod push_support;

use std::collections::BTreeSet;

use common::{both, draft, global_only, query, repo, repo_only};
use harness_memory::push::{Action, Options, Overwrite, push};
use harness_memory::store::{
    Edge, Forget, Forgotten, Hit, Link, MemoryStore, Neighborhood, Note, NoteDraft, NoteKind, NoteListQuery, NotePage,
    NotePatch, Saved, SearchQuery, SqliteStore, StoreError, Target, View,
};
use push_support::{all, change_of, counts, local_memory};
use tempfile::TempDir;

#[test]
fn pushing_into_another_sqlite_store_is_idempotent() {
    let dir = TempDir::new().expect("temp dir");
    let mut remote = SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team store");

    push_support::scenario(&mut remote);
}

#[test]
fn hidden_team_notes_are_never_written_over() {
    let dir = TempDir::new().expect("temp dir");
    let mut remote = SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team store");

    push_support::hidden_scenario(&mut remote);
}

#[test]
fn hidden_team_notes_are_never_written_over_in_libsql() {
    let Some(db) = libsql_support::Database::libsql() else {
        return;
    };

    push_support::hidden_scenario(db.open().as_mut());
}

#[test]
fn hidden_team_notes_are_never_written_over_in_mongo() {
    let Some((_db, mut store)) = mongo_support::store() else {
        return;
    };

    push_support::hidden_scenario(&mut store);
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
    let pushed = push(&local.path, &mut remote, &[], &without_repositories).expect("global only");
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

    push(&local.path, &mut remote, &[], &all()).expect("first push");

    let raw = rusqlite::Connection::open(&team).expect("raw team");
    raw.execute("UPDATE relations SET updated_at = 1", [])
        .expect("age the relations");

    let again = push(&local.path, &mut remote, &[], &all()).expect("second push");
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

    let error = push(&missing, &mut remote, &[], &all()).expect_err("no local database");
    assert!(matches!(error, StoreError::NotFound(_)), "{error:?}");
    assert!(!missing.exists());
}

/// The text of the team note `text` finds in `view`.
fn team_body(remote: &mut SqliteStore, text: &str, view: View) -> String {
    let hits = remote.search(&query(text, view.clone())).expect("search");
    assert_eq!(hits.len(), 1, "{text}: {hits:?}");
    remote.get(&view, &[hits[0].id]).expect("get").remove(0).body
}

/// The titles of the notes left in the local database at `path`, sorted.
fn local_titles(path: &std::path::Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).expect("raw local");
    let mut statement = conn.prepare("SELECT title FROM notes ORDER BY title").expect("select");
    statement
        .query_map([], |row| row.get(0))
        .expect("titles")
        .collect::<Result<_, _>>()
        .expect("titles")
}

fn now() -> i64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock");
    i64::try_from(elapsed.as_secs()).expect("seconds")
}

#[test]
fn conflicts_are_overwritten_only_as_asked() {
    let dir = TempDir::new().expect("temp dir");
    let mut remote = SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team store");
    let local = local_memory();
    push(&local.path, &mut remote, &[], &all()).expect("first push");

    let later = now() + 3600;
    local.edit("Deploy steps", "Run make release", later);
    local.edit("Editor tabs", "Eight wide", later);

    let none = Options {
        overwrite: Overwrite::None,
        ..all()
    };
    let kept = push(&local.path, &mut remote, &[], &none).expect("overwrite none");
    assert_eq!((kept.overwritten, kept.kept, kept.identical), (0, 2, 1));
    let deploy = change_of(&kept, "Deploy steps").clone();
    assert_eq!(deploy.action, Action::Kept);
    assert!(deploy.conflict);
    assert_eq!(deploy.local_updated_at, later);
    assert!(deploy.team_id.is_some() && deploy.team_updated_at.is_some());
    assert!(!change_of(&kept, "Build cache").conflict);
    assert_eq!(
        team_body(&mut remote, "deploy", repo_only(push_support::REPO)),
        "Run make deploy"
    );

    let picked = Options {
        overwrite: Overwrite::Ids(BTreeSet::from([deploy.local_id])),
        ..all()
    };
    let one = push(&local.path, &mut remote, &[], &picked).expect("overwrite one");
    assert_eq!(change_of(&one, "Deploy steps").action, Action::Overwritten);
    assert!(change_of(&one, "Deploy steps").conflict);
    assert_eq!(change_of(&one, "Editor tabs").action, Action::Kept);
    assert_eq!((one.overwritten, one.kept), (1, 1));
    assert_eq!(
        team_body(&mut remote, "deploy", repo_only(push_support::REPO)),
        "Run make release"
    );
    assert_eq!(team_body(&mut remote, "tabs", global_only()), "Four wide");

    let every = Options {
        overwrite: Overwrite::All,
        ..all()
    };
    let rest = push(&local.path, &mut remote, &[], &every).expect("overwrite all");
    assert_eq!(change_of(&rest, "Editor tabs").action, Action::Overwritten);
    assert_eq!(change_of(&rest, "Deploy steps").action, Action::Identical);
    assert_eq!(team_body(&mut remote, "tabs", global_only()), "Eight wide");
}

#[test]
fn delete_copied_deletes_only_the_notes_the_team_now_holds() {
    let dir = TempDir::new().expect("temp dir");
    let mut remote = SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team store");
    let local = local_memory();
    push(&local.path, &mut remote, &[], &all()).expect("first push");

    // A conflict the push keeps, a note the team changed later, a new note, and an identical one.
    local.edit("Deploy steps", "Run make release", now() + 3600);
    let tabs = remote.search(&query("tabs", global_only())).expect("tabs");
    let patch = NotePatch {
        body: Some("Two wide".to_owned()),
        ..NotePatch::default()
    };
    remote.update(&global_only(), tabs[0].id, &patch).expect("team edit");
    local.edit("Editor tabs", "Eight wide", 1);
    let mut store = SqliteStore::open(&local.path).expect("local store");
    store
        .remember(
            &repo(push_support::REPO),
            &draft("Release train", "Every second Tuesday", NoteKind::Decision),
        )
        .expect("new note");
    drop(store);

    let options = Options {
        overwrite: Overwrite::None,
        delete_copied: true,
        ..all()
    };
    let pushed = push(&local.path, &mut remote, &[], &options).expect("push");

    assert!(pushed.failed.is_empty(), "{:?}", pushed.failed);
    assert_eq!(pushed.deleted, Some(2));
    assert_eq!(local_titles(&local.path), ["Deploy steps", "Editor tabs"]);

    for (title, action, deleted) in [
        ("Build cache", Action::Identical, true),
        ("Release train", Action::Added, true),
        ("Deploy steps", Action::Kept, false),
        ("Editor tabs", Action::TeamNewer, false),
    ] {
        let change = change_of(&pushed, title);
        assert_eq!((change.action, change.deleted), (action, deleted), "{title}");
    }

    assert_eq!(
        team_body(&mut remote, "release train", repo_only(push_support::REPO)),
        "Every second Tuesday"
    );
}

#[test]
fn a_dry_run_deletes_nothing() {
    let dir = TempDir::new().expect("temp dir");
    let mut remote = SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team store");
    let local = local_memory();

    let options = Options {
        dry_run: true,
        delete_copied: true,
        ..all()
    };
    let listed = push(&local.path, &mut remote, &[], &options).expect("dry run");

    assert_eq!(listed.added, 3);
    assert_eq!(listed.deleted, None);
    assert!(listed.changes.iter().all(|change| !change.deleted));
    assert_eq!(
        local_titles(&local.path),
        ["Build cache", "Deploy steps", "Editor tabs"]
    );
}

#[test]
fn a_note_that_failed_to_push_is_not_deleted() {
    let dir = TempDir::new().expect("temp dir");
    let team = dir.path().join("team.sqlite3");
    let mut remote = SqliteStore::open(&team).expect("team store");
    let local = local_memory();

    let raw = rusqlite::Connection::open(&team).expect("raw team");
    raw.execute_batch(
        "CREATE TRIGGER refuse_deploy BEFORE INSERT ON notes WHEN NEW.title = 'Deploy steps'
         BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    )
    .expect("trigger");

    let options = Options {
        delete_copied: true,
        ..all()
    };
    let pushed = push(&local.path, &mut remote, &[], &options).expect("push");

    let deploy = change_of(&pushed, "Deploy steps");
    assert_eq!(pushed.failed.len(), 1, "{:?}", pushed.failed);
    assert_eq!(pushed.failed[0].local_id, Some(deploy.local_id));
    assert!(!pushed.failed[0].deleting);
    assert!(!deploy.deleted);
    assert_eq!(pushed.deleted, Some(2));
    assert_eq!(local_titles(&local.path), ["Deploy steps"]);

    assert_eq!((deploy.action, deploy.failed), (Action::Added, true));
    assert!(!change_of(&pushed, "Build cache").failed);
    assert_eq!(pushed.added, 2, "the failed note is not counted as added");
}

#[test]
fn copied_notes_whose_entities_the_team_lacks_stay() {
    let dir = TempDir::new().expect("temp dir");
    let mut remote = SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team store");
    let local = local_memory();
    push(&local.path, &mut remote, &[], &all()).expect("first push");

    // The same note here, now linked to an entity the team's note does not have.
    let mut store = SqliteStore::open(&local.path).expect("local store");
    let mut deploy = draft("Deploy steps", "Run make deploy", NoteKind::Decision);
    deploy.entities = vec!["Makefile".to_owned()];
    store.remember(&repo(push_support::REPO), &deploy).expect("entity");
    drop(store);

    let options = Options {
        delete_copied: true,
        ..all()
    };
    let pushed = push(&local.path, &mut remote, &[], &options).expect("push");

    assert!(pushed.failed.is_empty(), "{:?}", pushed.failed);
    let deploy = change_of(&pushed, "Deploy steps");
    assert_eq!((deploy.action, deploy.deleted), (Action::Identical, false));
    assert!(
        change_of(&pushed, "Build cache").deleted,
        "the team's note has its entity"
    );
    assert_eq!(pushed.deleted, Some(2));
    assert_eq!(local_titles(&local.path), ["Deploy steps"]);
}

#[test]
fn a_failed_relation_deletes_nothing() {
    let dir = TempDir::new().expect("temp dir");
    let team = dir.path().join("team.sqlite3");
    let mut remote = SqliteStore::open(&team).expect("team store");
    let local = local_memory();

    let raw = rusqlite::Connection::open(&team).expect("raw team");
    raw.execute_batch(
        "CREATE TRIGGER refuse_relations BEFORE INSERT ON relations
         BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    )
    .expect("trigger");

    let options = Options {
        delete_copied: true,
        ..all()
    };
    let pushed = push(&local.path, &mut remote, &[], &options).expect("push");

    assert_eq!(pushed.added, 3);
    assert_eq!(pushed.deleted, Some(0));
    assert!(pushed.changes.iter().all(|change| !change.deleted));
    assert!(
        pushed
            .failed
            .iter()
            .any(|failure| failure.deleting && failure.message.contains("a relation could not be pushed")),
        "{:?}",
        pushed.failed
    );
    assert_eq!(
        local_titles(&local.path),
        ["Build cache", "Deploy steps", "Editor tabs"]
    );
}

#[test]
fn a_relation_whose_note_failed_is_not_pushed_without_it() {
    let dir = TempDir::new().expect("temp dir");
    let team = dir.path().join("team.sqlite3");
    let mut remote = SqliteStore::open(&team).expect("team store");
    let local = local_memory();

    let raw = rusqlite::Connection::open(&team).expect("raw team");
    raw.execute_batch(
        "CREATE TRIGGER refuse_cache BEFORE INSERT ON notes WHEN NEW.title = 'Build cache'
         BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    )
    .expect("trigger");

    let options = Options {
        delete_copied: true,
        ..all()
    };
    let pushed = push(&local.path, &mut remote, &[], &options).expect("push");
    let cache = change_of(&pushed, "Build cache").local_id;

    // The note fails, and so does its relation (tied to the note), which never reaches the team.
    let failed: Vec<(Option<i64>, bool)> = pushed
        .failed
        .iter()
        .map(|failure| (failure.local_id, failure.deleting))
        .collect();
    assert_eq!(
        failed,
        [(Some(cache), false), (Some(cache), false)],
        "{:?}",
        pushed.failed
    );
    assert!(
        pushed.failed[1]
            .message
            .contains("relation \"Cache located_in target/\""),
        "{}",
        pushed.failed[1].message
    );
    assert_eq!((pushed.links_added, pushed.links_unchanged), (1, 0));

    let around = remote.neighbors(&both(push_support::REPO), "cache", 1, 20);
    assert!(
        around.as_ref().map_or(true, |around| around.edges.is_empty()),
        "{around:?}"
    );

    // The other notes are copied and deleted here; the failed one keeps its relation.
    assert_eq!(pushed.deleted, Some(2));
    assert_eq!(local_titles(&local.path), ["Build cache"]);
}

#[test]
fn relations_are_not_linked_to_a_hidden_team_note_the_push_leaves() {
    for team_newer in [false, true] {
        let dir = TempDir::new().expect("temp dir");
        let mut remote = SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team store");
        let local = local_memory();
        push(&local.path, &mut remote, &[], &all()).expect("first push");

        let mut store = SqliteStore::open(&local.path).expect("local store");
        let cache = store
            .find_by_title(&repo(push_support::REPO), "Build cache")
            .expect("find")
            .expect("local cache")
            .id;
        let feeds = Link {
            from: "Cache".to_owned(),
            relation: "feeds".to_owned(),
            to: "Builds".to_owned(),
            note_id: Some(cache),
        };
        store.link(&repo(push_support::REPO), &feeds).expect("new relation");
        drop(store);

        let team_cache = remote
            .find_by_title(&repo(push_support::REPO), "Build cache")
            .expect("find")
            .expect("team cache");

        let (options, action) = if team_newer {
            let patch = NotePatch {
                body: Some("Lives in build/".to_owned()),
                ..NotePatch::default()
            };
            remote
                .update(&both(push_support::REPO), team_cache.id, &patch)
                .expect("team edit");
            local.edit("Build cache", "Lives in out/", 1);
            (all(), Action::TeamNewer)
        } else {
            local.edit("Build cache", "Lives in out/", now() + 3600);
            let options = Options {
                overwrite: Overwrite::None,
                ..all()
            };
            (options, Action::Kept)
        };

        let hidden = [team_cache.id];
        let preview = push(
            &local.path,
            &mut remote,
            &hidden,
            &Options {
                dry_run: true,
                ..options.clone()
            },
        )
        .expect("dry run");
        let pushed = push(&local.path, &mut remote, &hidden, &options).expect("push");

        for summary in [&preview, &pushed] {
            assert!(summary.failed.is_empty(), "{:?}", summary.failed);
            assert_eq!(change_of(summary, "Build cache").action, action);
            assert_eq!(
                (summary.links_added, summary.links_unchanged),
                (0, 3),
                "team newer: {team_newer}"
            );
        }

        let around = remote
            .neighbors(&both(push_support::REPO), "cache", 1, 20)
            .expect("neighbors");
        assert!(
            around.edges.iter().all(|edge| edge.relation != "feeds"),
            "team newer: {team_newer}: {:?}",
            around.edges
        );
    }
}

#[test]
fn delete_copied_marks_the_notes_that_changed_since_they_were_read() {
    let dir = TempDir::new().expect("temp dir");
    let local = local_memory();
    let mut remote = Racing {
        inner: SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team store"),
        local: local.path.clone(),
        rewrite: "UPDATE notes SET updated_at = updated_at + 1 WHERE title = 'Deploy steps'",
    };

    let preview = push(&local.path, &mut remote, &[], &push_support::dry()).expect("dry run");
    assert!(preview.changes.iter().all(|change| !change.changed_since));

    let options = Options {
        delete_copied: true,
        ..all()
    };
    let pushed = push(&local.path, &mut remote, &[], &options).expect("push");
    assert!(pushed.failed.is_empty(), "{:?}", pushed.failed);

    let marks: Vec<(&str, bool, bool)> = pushed
        .changes
        .iter()
        .map(|change| (change.title.as_str(), change.deleted, change.changed_since))
        .collect();
    assert_eq!(
        marks,
        [
            ("Build cache", true, false),
            ("Deploy steps", false, true),
            ("Editor tabs", true, false)
        ]
    );
    assert_eq!(pushed.deleted, Some(2));
    assert_eq!(local_titles(&local.path), ["Deploy steps"]);
}

#[test]
fn delete_copied_keeps_a_note_edited_within_the_second_it_was_read() {
    let rewrites = [
        "UPDATE notes SET body = body || ' (edited)' WHERE title = 'Deploy steps'",
        "UPDATE notes SET tags = 'edited', revision = revision + 1 WHERE title = 'Deploy steps'",
    ];

    for rewrite in rewrites {
        let dir = TempDir::new().expect("temp dir");
        let local = local_memory();
        let mut remote = Racing {
            inner: SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team store"),
            local: local.path.clone(),
            rewrite,
        };

        let options = Options {
            delete_copied: true,
            ..all()
        };
        let pushed = push(&local.path, &mut remote, &[], &options).expect("push");
        assert!(pushed.failed.is_empty(), "{rewrite}: {:?}", pushed.failed);

        let deploy = change_of(&pushed, "Deploy steps");
        assert!(!deploy.deleted, "{rewrite}");
        assert!(deploy.changed_since, "{rewrite}");
        assert_eq!(local_titles(&local.path), ["Deploy steps"], "{rewrite}");
    }
}

/// A team store that runs `rewrite` on the local database (as a server would write the note
/// "Deploy steps") while "Editor tabs" is added.
struct Racing {
    inner: SqliteStore,
    local: std::path::PathBuf,
    rewrite: &'static str,
}

impl MemoryStore for Racing {
    fn remember(&mut self, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError> {
        if draft.title == "Editor tabs" {
            let conn = rusqlite::Connection::open(&self.local).expect("raw local");
            conn.execute(self.rewrite, []).expect("rewrite");
        }

        self.inner.remember(target, draft)
    }

    fn update(&mut self, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError> {
        self.inner.update(view, id, patch)
    }

    fn get(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        self.inner.get(view, ids)
    }

    fn peek(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        self.inner.peek(view, ids)
    }

    fn list_notes(&mut self, query: &NoteListQuery) -> Result<NotePage, StoreError> {
        self.inner.list_notes(query)
    }

    fn search(&mut self, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
        self.inner.search(query)
    }

    fn similar(&mut self, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
        self.inner.similar(view, id, limit)
    }

    fn link(&mut self, target: &Target, link: &Link) -> Result<Edge, StoreError> {
        self.inner.link(target, link)
    }

    fn neighbors(&mut self, view: &View, entity: &str, depth: u8, limit: usize) -> Result<Neighborhood, StoreError> {
        self.inner.neighbors(view, entity, depth, limit)
    }

    fn forget(&mut self, view: &View, target: &Target, what: &Forget) -> Result<usize, StoreError> {
        self.inner.forget(view, target, what)
    }

    fn forget_notes(&mut self, view: &View, exclude: &[i64]) -> Result<Forgotten, StoreError> {
        self.inner.forget_notes(view, exclude)
    }

    fn find_by_title(&mut self, target: &Target, title: &str) -> Result<Option<Note>, StoreError> {
        self.inner.find_by_title(target, title)
    }
}
