//! A local memory to push, and the push scenario every team backend has to pass.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use harness_memory::push::{Action, Change, Options, Summary, push};
use harness_memory::store::{Link, MemoryStore, NoteKind, NotePatch, ProjectKey, Provenance, SqliteStore, Target};
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use crate::common::{both, draft, global_only, query, repo, repo_only};

pub const REPO: &str = "github.com/acme/widget";

/// A local database file with two repository notes, one global note and two relations.
pub struct Local {
    _dir: TempDir,
    pub path: PathBuf,
}

pub fn local_memory() -> Local {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("memory.sqlite3");
    let mut store = SqliteStore::open(&path).expect("local store");

    let mut cache = draft("Build cache", "Lives in target/", NoteKind::Location);
    cache.entities = vec!["Cache".to_owned()];
    cache.tags = vec!["build".to_owned()];
    cache.provenance = Provenance {
        root: Some(PathBuf::from("/work/widget")),
        source: Some("omp".to_owned()),
        via_override: false,
    };
    let cache = store.remember(&repo(REPO), &cache).expect("cache");

    store
        .remember(
            &repo(REPO),
            &draft("Deploy steps", "Run make deploy", NoteKind::Decision),
        )
        .expect("deploy");
    store
        .remember(&Target::Global, &draft("Editor tabs", "Four wide", NoteKind::Note))
        .expect("tabs");

    let located = Link {
        from: "Cache".to_owned(),
        relation: "located_in".to_owned(),
        to: "target/".to_owned(),
        note_id: Some(cache.id),
    };
    store.link(&repo(REPO), &located).expect("repo link");

    let uses = Link {
        from: "Editor".to_owned(),
        relation: "uses".to_owned(),
        to: "Tabs".to_owned(),
        note_id: None,
    };
    store.link(&Target::Global, &uses).expect("global link");

    Local { _dir: dir, path }
}

impl Local {
    /// Sets a local note's body and update time below the store (`updated_at` in Unix seconds).
    pub fn edit(&self, title: &str, body: &str, updated_at: i64) {
        self.set(title, "body", body, updated_at);
    }

    /// Sets a local note's tags (space-separated) and update time below the store.
    pub fn retag(&self, title: &str, tags: &str, updated_at: i64) {
        self.set(title, "tags", tags, updated_at);
    }

    fn set(&self, title: &str, column: &str, value: &str, updated_at: i64) {
        let conn = rusqlite::Connection::open(&self.path).expect("raw local");
        let changed = conn
            .execute(
                &format!("UPDATE notes SET {column} = ?1, updated_at = ?2 WHERE title = ?3"),
                rusqlite::params![value, updated_at, title],
            )
            .expect("edit");
        assert_eq!(changed, 1);
    }

    /// Adds a note to a repository that has no remote, so it is keyed by its path.
    pub fn add_private_note(&self) {
        let mut store = SqliteStore::open(&self.path).expect("local store");
        let private = Target::Repo(ProjectKey {
            key: PRIVATE.to_owned(),
            root: PathBuf::from("/home/me/diary"),
            remote: None,
        });

        store
            .remember(&private, &draft("Diary layout", "One file per day", NoteKind::Location))
            .expect("private note");
    }
}

/// The key of [`Local::add_private_note`]'s repository.
pub const PRIVATE: &str = "path:/home/me/diary";

/// A push that writes everything.
pub fn all() -> Options {
    Options::default()
}

/// A dry run of everything.
pub fn dry() -> Options {
    Options {
        dry_run: true,
        ..Options::default()
    }
}

/// The counts of a push, failures included: notes added, updated, overwritten, skipped (identical
/// and team newer), then relations added and unchanged.
#[derive(Debug, PartialEq, Eq)]
pub struct Counts {
    pub notes: [usize; 4],
    pub links: [usize; 2],
    pub failed: Vec<String>,
}

pub fn counts(summary: &Summary) -> Counts {
    Counts {
        notes: [summary.added, summary.updated, summary.overwritten, summary.skipped()],
        links: [summary.links_added, summary.links_unchanged],
        failed: summary.failed.iter().map(|failure| failure.message.clone()).collect(),
    }
}

/// The notes a push added, updated or overwrote: their scope, title and action.
pub fn moved(summary: &Summary) -> Vec<(String, String, Action)> {
    summary
        .changes
        .iter()
        .filter(|change| matches!(change.action, Action::Added | Action::Updated | Action::Overwritten))
        .map(|change| (change.scope.clone(), change.title.clone(), change.action))
        .collect()
}

/// The change a push lists for the note titled `title`.
pub fn change_of<'a>(summary: &'a Summary, title: &str) -> &'a Change {
    summary
        .changes
        .iter()
        .find(|change| change.title == title)
        .unwrap_or_else(|| panic!("no change for {title}: {summary:?}"))
}

fn expect(notes: [usize; 4], links: [usize; 2]) -> Counts {
    Counts {
        notes,
        links,
        failed: Vec::new(),
    }
}

fn change(scope: &str, title: &str, action: Action) -> (String, String, Action) {
    (scope.to_owned(), title.to_owned(), action)
}

fn now() -> i64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock");
    i64::try_from(elapsed.as_secs()).expect("seconds")
}

/// Pushes [`local_memory`] into `remote` (empty) again and again: a dry run lists what it would
/// share and writes nothing, the first push adds everything with its provenance and relations,
/// pushing again changes nothing (relations included), a later local text overwrites the team's
/// one and says so, a later local retag is an update, and a team note changed after the local
/// one is kept.
pub fn scenario(remote: &mut dyn MemoryStore) {
    let local = local_memory();
    let key = format!("git:{REPO}");

    let listed = push(&local.path, remote, &[], &dry()).expect("dry run");
    assert_eq!(counts(&listed), expect([3, 0, 0, 0], [2, 0]));
    assert_eq!(
        moved(&listed),
        vec![
            change(&key, "Build cache", Action::Added),
            change(&key, "Deploy steps", Action::Added),
            change("global", "Editor tabs", Action::Added),
        ]
    );
    assert_eq!(listed.projects, BTreeSet::from([key.clone(), "global".to_owned()]));
    assert!(remote.search(&query("", both(REPO))).expect("recent").is_empty());

    let first = push(&local.path, remote, &[], &all()).expect("first push");
    assert_eq!(counts(&first), expect([3, 0, 0, 0], [2, 0]));

    let hits = remote.search(&query("cache", repo_only(REPO))).expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "Build cache");
    assert_eq!(hits[0].entities, vec!["Cache"]);
    assert_eq!(
        hits[0].provenance,
        Provenance {
            root: Some(PathBuf::from("/work/widget")),
            source: Some("omp".to_owned()),
            via_override: false,
        }
    );

    let around = remote.neighbors(&both(REPO), "cache", 1, 20).expect("neighbors");
    assert_eq!(around.edges.len(), 1);
    assert_eq!(around.edges[0].to, "target/");
    assert_eq!(around.notes, vec![(hits[0].id, "Build cache".to_owned())]);
    assert_eq!(
        remote
            .neighbors(&global_only(), "editor", 1, 20)
            .expect("global")
            .edges
            .len(),
        1
    );

    let again = push(&local.path, remote, &[], &all()).expect("second push");
    assert_eq!(counts(&again), expect([0, 0, 0, 3], [0, 2]));
    assert!(moved(&again).is_empty());
    assert_eq!(again.identical, 3);
    assert_eq!(remote.search(&query("", both(REPO))).expect("recent").len(), 3);

    // A newer local text replaces the team's one, and the push names the note.
    local.edit("Deploy steps", "Run make release", now() + 3600);
    assert_eq!(
        counts(&push(&local.path, remote, &[], &dry()).expect("dry overwrite")),
        expect([0, 0, 1, 2], [0, 2])
    );

    let changed = push(&local.path, remote, &[], &all()).expect("third push");
    assert_eq!(counts(&changed), expect([0, 0, 1, 2], [0, 2]));
    assert_eq!(moved(&changed), vec![change(&key, "Deploy steps", Action::Overwritten)]);

    let deploy = remote.search(&query("deploy", repo_only(REPO))).expect("deploy");
    let note = remote.get(&both(REPO), &[deploy[0].id]).expect("get").remove(0);
    assert_eq!(note.body, "Run make release");

    // Newer local tags with the same text are an update, not an overwrite.
    local.retag("Build cache", "build cache", now() + 3600);
    let retagged = push(&local.path, remote, &[], &all()).expect("fourth push");
    assert_eq!(counts(&retagged), expect([0, 1, 0, 2], [0, 2]));
    assert_eq!(moved(&retagged), vec![change(&key, "Build cache", Action::Updated)]);

    assert_eq!(
        counts(&push(&local.path, remote, &[], &all()).expect("fifth push")),
        expect([0, 0, 0, 3], [0, 2])
    );

    // A team note changed after the local one keeps the team's text.
    let tabs = remote.search(&query("tabs", global_only())).expect("tabs");
    let patch = NotePatch {
        body: Some("Two wide".to_owned()),
        ..NotePatch::default()
    };
    remote.update(&global_only(), tabs[0].id, &patch).expect("team edit");
    local.edit("Editor tabs", "Eight wide", 1);

    let sixth = push(&local.path, remote, &[], &all()).expect("sixth push");
    assert_eq!(counts(&sixth), expect([0, 0, 0, 3], [0, 2]));
    assert_eq!((sixth.identical, sixth.team_newer), (2, 1));
    assert_eq!(change_of(&sixth, "Editor tabs").action, Action::TeamNewer);
    assert!(!change_of(&sixth, "Editor tabs").conflict);
    let kept = remote.get(&global_only(), &[tabs[0].id]).expect("get").remove(0);
    assert_eq!(kept.body, "Two wide");
}

/// Pushes [`local_memory`] into `remote` (empty) narrowed by `--repo` and `--no-global`: another
/// repository without global takes nothing, leaving global out takes only the repository, and the
/// rest follows in a later full push.
pub fn narrowed_scenario(remote: &mut dyn MemoryStore) {
    let local = local_memory();
    let key = format!("git:{REPO}");

    let other = Options {
        repo: Some("git:github.com/acme/other".to_owned()),
        skip_global: true,
        ..all()
    };
    let nothing = push(&local.path, remote, &[], &other).expect("other repository");
    assert_eq!(counts(&nothing), expect([0, 0, 0, 0], [0, 0]));
    assert!(nothing.projects.is_empty());

    let repo_alone = Options {
        repo: Some(key.clone()),
        skip_global: true,
        ..all()
    };
    let pushed = push(&local.path, remote, &[], &repo_alone).expect("repository only");
    assert_eq!(counts(&pushed), expect([2, 0, 0, 0], [1, 0]));
    assert_eq!(pushed.projects, BTreeSet::from([key.clone()]));
    assert!(remote.search(&query("", global_only())).expect("global").is_empty());

    let rest = push(&local.path, remote, &[], &all()).expect("everything");
    assert_eq!(counts(&rest), expect([1, 0, 0, 2], [1, 1]));
    assert_eq!(moved(&rest), vec![change("global", "Editor tabs", Action::Added)]);
}

/// Pushes [`local_memory`] into `remote`, then pushes again with two of its team notes hidden on
/// this computer: one the newer local text would overwrite, one identical. Both fail, in the dry
/// run as in the push, naming `harness-memory notes unhide`; neither is written over, counted or
/// deleted here, the relation of the failed note fails with it, while the rest goes as usual.
pub fn hidden_scenario(remote: &mut dyn MemoryStore) {
    let local = local_memory();
    push(&local.path, remote, &[], &all()).expect("first push");
    local.edit("Deploy steps", "Run make release", now() + 3600);

    let team_id = |remote: &mut dyn MemoryStore, title: &str| {
        remote
            .find_by_title(&repo(REPO), title)
            .expect("find")
            .unwrap_or_else(|| panic!("no team note {title}"))
            .id
    };
    let (cache, deploy) = (team_id(remote, "Build cache"), team_id(remote, "Deploy steps"));
    let hidden = [cache, deploy];

    let preview = push(&local.path, remote, &hidden, &dry()).expect("dry run");
    let failed: Vec<(Option<i64>, bool)> = preview
        .failed
        .iter()
        .map(|failure| (failure.local_id, failure.deleting))
        .collect();
    let [cache_id, deploy_id] = ["Build cache", "Deploy steps"].map(|title| Some(change_of(&preview, title).local_id));

    // The relation of the failed "Build cache" fails with it, tied to that note.
    assert_eq!(failed, [(cache_id, false), (deploy_id, false), (cache_id, false)]);
    assert!(
        preview.failed[2]
            .message
            .starts_with("relation \"Cache located_in target/\""),
        "{}",
        preview.failed[2].message
    );
    assert_eq!(
        [preview.added, preview.updated, preview.overwritten, preview.skipped()],
        [0, 0, 0, 1]
    );
    assert_eq!((preview.links_added, preview.links_unchanged), (0, 1));

    for (title, id) in [("Build cache", cache), ("Deploy steps", deploy)] {
        assert!(change_of(&preview, title).failed, "{title}");
        let failure = preview
            .failed
            .iter()
            .find(|failure| failure.local_id == Some(change_of(&preview, title).local_id))
            .expect("a failure");
        assert!(!failure.deleting);
        assert!(
            failure.message.contains(&format!("harness-memory notes unhide {id}")),
            "{}",
            failure.message
        );
    }

    let options = Options {
        delete_copied: true,
        ..all()
    };
    let pushed = push(&local.path, remote, &hidden, &options).expect("push");
    assert_eq!(counts(&pushed), counts(&preview));
    assert_eq!(pushed.failed, preview.failed);
    assert_eq!(pushed.deleted, Some(1));

    let team = remote
        .find_by_title(&repo(REPO), "Deploy steps")
        .expect("find")
        .expect("deploy");
    assert_eq!(team.body, "Run make deploy");

    let left: Vec<String> = SqliteStore::open(&local.path)
        .expect("local store")
        .search(&query("", both(REPO)))
        .expect("local notes")
        .into_iter()
        .map(|hit| hit.title)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(left, ["Build cache", "Deploy steps"]);
}
