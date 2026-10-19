//! `scripts push` through `push_scripts`: each local script is matched with the team's by scope,
//! repository and name and added, updated, overwritten, kept, left identical or left for the
//! team's newer one; `--overwrite` ids pick the conflicts; this computer's trust goes along only
//! for trusted versions; `--delete-copied` deletes only the copies unchanged since they were read,
//! and nothing whose push failed. Every case runs with a second SQLite file as the team database,
//! and on PostgreSQL, libSQL and MongoDB when their test servers are set.

mod common;
mod libsql_support;
mod mongo_support;
mod postgres_support;
mod script_teams;

use std::collections::BTreeSet;
use std::path::PathBuf;

use common::scripts::{REPO, script};
use common::{both, global_only, repo, repo_only};
use harness_memory::backend::LOCAL_KEY;
use harness_memory::push::{Action, Options, Overwrite};
use harness_memory::push_scripts::{ScriptChange, ScriptPush, TeamScripts, push_scripts};
use harness_memory::store::{
    PreviousVersion, Scope, Script, ScriptRef, ScriptStore, SqliteStore, StoreError, Target, TrustOrigin, TrustRecord,
    TrustStore, TrustedVersion, View,
};
use pretty_assertions::assert_eq;
use script_teams::{Fresh, Team};
use tempfile::TempDir;

/// The key the team database of these tests has in this machine's trust.
const TEAM_KEY: &str = "team:0123456789abcdef";

/// One case's databases: the team one and the local file (which holds the trust too).
struct Fixture {
    team: Team,
    local: TempDir,
}

/// The local scripts of [`Fixture::scenario`], by their local ids.
struct Local {
    added: i64,
    same: i64,
    described: i64,
    conflict: i64,
    second: i64,
    team_newer: i64,
}

impl Fixture {
    fn path(&self) -> PathBuf {
        self.local.path().join("memory.sqlite3")
    }

    fn local_store(&self) -> SqliteStore {
        SqliteStore::open(&self.path()).expect("local file")
    }

    /// Saves `name` with `body` and `description` into `target` of `store`; the saved script.
    fn save(store: &mut dyn ScriptStore, target: &Target, name: &str, body: &str, description: &str) -> Script {
        store
            .save_script(target, &script(name, description, body))
            .expect("saved")
            .script
    }

    /// Sets a local script's update time below the store.
    fn local_updated_at(&self, id: i64, at: i64) {
        let conn = rusqlite::Connection::open(self.path()).expect("raw local");
        let changed = conn
            .execute("UPDATE scripts SET updated_at = ?2 WHERE id = ?1", (id, at))
            .expect("update");
        assert_eq!(changed, 1);
    }

    /// Local and team scripts for every action: `added` (only here, trusted here), `same`
    /// (identical, trusted here), `described` (newer here, another description), `conflict` and
    /// `second` (newer here, another body; `conflict` waits for approval here) and `team_newer`
    /// (changed in the team database since).
    fn scenario(&self) -> Local {
        let mut team = self.team.open();
        let mut local = self.local_store();
        let far = 4_000_000_000;

        let added = Self::save(&mut local, &repo(REPO), "added", "echo added\n", "");
        let same = Self::save(&mut local, &repo(REPO), "same", "echo same\n", "Same");
        Self::save(team.as_mut(), &repo(REPO), "same", "echo same\n", "Same");

        let described = Self::save(&mut local, &repo(REPO), "described", "echo described\n", "New words");
        Self::save(team.as_mut(), &repo(REPO), "described", "echo described\n", "Old words");

        let conflict = Self::save(&mut local, &Target::Global, "conflict", "echo mine\n", "");
        Self::save(team.as_mut(), &Target::Global, "conflict", "echo theirs\n", "");
        let second = Self::save(&mut local, &Target::Global, "second", "echo mine\n", "");
        Self::save(team.as_mut(), &Target::Global, "second", "echo theirs\n", "");

        let team_newer = Self::save(&mut local, &Target::Global, "team-newer", "echo old\n", "");
        Self::save(team.as_mut(), &Target::Global, "team-newer", "echo newer\n", "");

        for id in [described.summary.id, conflict.summary.id, second.summary.id] {
            self.local_updated_at(id, far);
        }
        self.local_updated_at(team_newer.summary.id, 1);

        for (script, origin) in [(&added, TrustOrigin::Saved), (&same, TrustOrigin::Approved)] {
            let version = TrustedVersion::of(LOCAL_KEY, script.summary.id, script, origin);
            local.trust(&version).expect("trusted here");
        }
        local.note_unapproved(&conflict.summary.sha256).expect("waits here");

        Local {
            added: added.summary.id,
            same: same.summary.id,
            described: described.summary.id,
            conflict: conflict.summary.id,
            second: second.summary.id,
            team_newer: team_newer.summary.id,
        }
    }

    /// Pushes with `options`, `trust` standing for this machine's trust, nothing hidden but
    /// `hidden`.
    fn push(&self, options: &Options, trust: &mut dyn TrustStore, hidden: &[i64]) -> ScriptPush {
        let mut team = self.team.open();
        let mut target = TeamScripts {
            scripts: team.as_mut(),
            key: TEAM_KEY,
            hidden,
        };

        push_scripts(&self.path(), &mut target, trust, options).expect("push")
    }

    /// The team's script `name` in `view`.
    fn team_script(&self, view: &View, name: &str) -> Script {
        self.team
            .open()
            .get_script(view, &ScriptRef::Name(name.to_owned()))
            .expect("get")
            .expect("a team script")
    }

    /// The local scripts' names, sorted.
    fn local_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .local_store()
            .every_script()
            .expect("scripts")
            .into_iter()
            .map(|(script, _)| script.summary.name)
            .collect();
        names.sort();
        names
    }
}

type Case = fn(&Fixture);

const CASES: [Case; 5] = [
    a_dry_run_classifies_every_script_and_writes_nothing,
    a_dry_run_fails_hidden_matches_as_the_push_does,
    overwrite_ids_pick_the_conflicts_and_trust_goes_with_trusted_versions,
    delete_copied_deletes_only_unchanged_copies,
    failed_scripts_are_uncounted_and_stay,
];

fn run(fresh: Fresh) {
    for case in CASES {
        let Some(team) = fresh() else { return };
        let fixture = Fixture {
            team,
            local: TempDir::new().expect("temp dir"),
        };

        case(&fixture);
    }
}

#[test]
fn pushing_scripts_into_a_sqlite_team_database() {
    run(script_teams::sqlite);
}

#[test]
fn pushing_scripts_into_postgres() {
    run(script_teams::postgres);
}

#[test]
fn pushing_scripts_into_libsql() {
    run(script_teams::libsql);
}

#[test]
fn pushing_scripts_into_mongo() {
    run(script_teams::mongo);
}

fn dry_run() -> Options {
    Options {
        dry_run: true,
        ..Options::default()
    }
}

/// The change of local script `id`.
fn change(summary: &ScriptPush, id: i64) -> &ScriptChange {
    summary
        .changes
        .iter()
        .find(|change| change.local_id == id)
        .unwrap_or_else(|| panic!("no change for script {id}"))
}

/// `[added, updated, overwritten, kept, identical, team newer]`.
fn totals(summary: &ScriptPush) -> [usize; 6] {
    [
        summary.added,
        summary.updated,
        summary.overwritten,
        summary.kept,
        summary.identical,
        summary.team_newer,
    ]
}

fn a_dry_run_classifies_every_script_and_writes_nothing(fixture: &Fixture) {
    let ids = fixture.scenario();
    let mut trust = fixture.local_store();

    let summary = fixture.push(&dry_run(), &mut trust, &[]);
    assert_eq!(totals(&summary), [1, 1, 2, 0, 1, 1]);
    assert!(summary.failed.is_empty(), "{:?}", summary.failed);
    assert_eq!(summary.deleted, None);

    let actions: Vec<(i64, Action, bool)> = summary
        .changes
        .iter()
        .map(|change| (change.local_id, change.action, change.conflict))
        .collect();
    assert_eq!(
        actions,
        [
            (ids.added, Action::Added, false),
            (ids.same, Action::Identical, false),
            (ids.described, Action::Updated, false),
            (ids.conflict, Action::Overwritten, true),
            (ids.second, Action::Overwritten, true),
            (ids.team_newer, Action::TeamNewer, false),
        ]
    );

    let added = change(&summary, ids.added);
    assert_eq!(
        (added.scope, added.repo.as_deref()),
        (Scope::Repo, Some("git:github.com/acme/widget"))
    );
    assert_eq!((added.team_id, added.team_updated_at), (None, None));
    assert!(summary.changes.iter().all(|change| !change.trusted && !change.deleted));

    // Nothing was written.
    assert!(
        fixture
            .team
            .open()
            .get_script(&both(REPO), &ScriptRef::Name("added".to_owned()))
            .expect("get")
            .is_none()
    );
    assert_eq!(fixture.team_script(&global_only(), "conflict").body, "echo theirs\n");

    // `--overwrite none` keeps every conflict, still marked as one.
    let kept = fixture.push(
        &Options {
            overwrite: Overwrite::None,
            ..dry_run()
        },
        &mut trust,
        &[],
    );
    assert_eq!(totals(&kept), [1, 1, 0, 2, 1, 1]);
    assert_eq!(
        (change(&kept, ids.conflict).action, change(&kept, ids.conflict).conflict),
        (Action::Kept, true)
    );
}

fn a_dry_run_fails_hidden_matches_as_the_push_does(fixture: &Fixture) {
    let ids = fixture.scenario();
    let mut trust = fixture.local_store();
    let same = fixture.team_script(&repo_only(REPO), "same");
    let conflict = fixture.team_script(&global_only(), "conflict");
    let hidden = [same.summary.id, conflict.summary.id];

    // The preview fails the scripts the push would refuse to write over, and writes nothing.
    let preview = fixture.push(&dry_run(), &mut trust, &hidden);
    assert_eq!(totals(&preview), [1, 1, 1, 0, 0, 1]);
    assert_eq!(fixture.team_script(&global_only(), "conflict").body, "echo theirs\n");

    let failed: Vec<(Option<i64>, bool)> = preview
        .failed
        .iter()
        .map(|failure| (failure.local_id, failure.deleting))
        .collect();
    assert_eq!(failed, [(Some(ids.same), false), (Some(ids.conflict), false)]);
    assert!(
        preview.failed[0].message.contains("harness-memory scripts unhide"),
        "{}",
        preview.failed[0].message
    );

    for id in [ids.same, ids.conflict] {
        assert!(change(&preview, id).failed, "script {id} is not failed");
    }

    // The push then fails the same scripts with the same messages and the same totals.
    let pushed = fixture.push(&Options::default(), &mut trust, &hidden);
    assert_eq!(totals(&pushed), totals(&preview));
    assert_eq!(pushed.failed, preview.failed);
    assert_eq!(fixture.team_script(&global_only(), "conflict").body, "echo theirs\n");
}

fn overwrite_ids_pick_the_conflicts_and_trust_goes_with_trusted_versions(fixture: &Fixture) {
    let ids = fixture.scenario();
    let mut trust = fixture.local_store();

    let options = Options {
        overwrite: Overwrite::Ids(BTreeSet::from([ids.second])),
        ..Options::default()
    };
    let summary = fixture.push(&options, &mut trust, &[]);
    assert!(summary.failed.is_empty(), "{:?}", summary.failed);
    assert_eq!(totals(&summary), [1, 1, 1, 1, 1, 1]);
    assert_eq!(change(&summary, ids.conflict).action, Action::Kept);
    assert_eq!(change(&summary, ids.second).action, Action::Overwritten);

    assert_eq!(fixture.team_script(&global_only(), "conflict").body, "echo theirs\n");
    assert_eq!(fixture.team_script(&global_only(), "second").body, "echo mine\n");
    assert_eq!(
        fixture.team_script(&repo_only(REPO), "described").summary.description,
        "New words"
    );
    assert_eq!(fixture.team_script(&global_only(), "team-newer").body, "echo newer\n");

    // Trust went along for the versions trusted here, under the team's ids.
    let carried: Vec<i64> = summary
        .changes
        .iter()
        .filter(|change| change.trusted)
        .map(|change| change.local_id)
        .collect();
    assert_eq!(carried, [ids.added, ids.same]);

    let added = fixture.team_script(&repo_only(REPO), "added");
    assert_eq!(change(&summary, ids.added).team_id, Some(added.summary.id));
    let record = trust
        .trust_of(TEAM_KEY, added.summary.id, &added.summary.sha256)
        .expect("trust")
        .expect("trusted in the team database");
    assert_eq!(
        (record.origin, record.is_for(&added.summary)),
        (TrustOrigin::Saved, true)
    );

    let same = fixture.team_script(&repo_only(REPO), "same");
    let record = trust
        .trust_of(TEAM_KEY, same.summary.id, &same.summary.sha256)
        .expect("trust");
    assert_eq!(record.map(|record| record.origin), Some(TrustOrigin::Approved));

    // Never a version waiting here, nor one not trusted here.
    for name in ["second", "described"] {
        let view = if name == "second" {
            global_only()
        } else {
            repo_only(REPO)
        };
        let team = fixture.team_script(&view, name);
        let trusted = trust
            .trust_of(TEAM_KEY, team.summary.id, &team.summary.sha256)
            .expect("trust");
        assert_eq!(trusted, None, "{name}");
    }
}

fn delete_copied_deletes_only_unchanged_copies(fixture: &Fixture) {
    let ids = fixture.scenario();

    // A server writes `same` again while the push runs (when its trust is carried).
    let mut trust = Racing {
        inner: fixture.local_store(),
        path: fixture.path(),
        rewrite: ids.same,
    };
    let options = Options {
        delete_copied: true,
        overwrite: Overwrite::None,
        ..Options::default()
    };
    let summary = fixture.push(&options, &mut trust, &[]);
    assert!(summary.failed.is_empty(), "{:?}", summary.failed);

    // Added, updated and overwritten copies go; kept, team newer and the rewritten one stay.
    assert_eq!(summary.deleted, Some(2));
    let deleted: Vec<i64> = summary
        .changes
        .iter()
        .filter(|change| change.deleted)
        .map(|change| change.local_id)
        .collect();
    assert_eq!(deleted, [ids.added, ids.described]);
    assert_eq!(fixture.local_names(), ["conflict", "same", "second", "team-newer"]);

    // Only the rewritten copy is marked as changed since it was read.
    let changed: Vec<i64> = summary
        .changes
        .iter()
        .filter(|change| change.changed_since)
        .map(|change| change.local_id)
        .collect();
    assert_eq!(changed, [ids.same]);
}

fn failed_scripts_are_uncounted_and_stay(fixture: &Fixture) {
    let ids = fixture.scenario();
    let same = fixture.team_script(&repo_only(REPO), "same");

    // Trusting `added` fails, and `same` is hidden here.
    let mut trust = Failing {
        inner: fixture.local_store(),
        fail: ids.added,
    };
    let options = Options {
        delete_copied: true,
        ..Options::default()
    };
    let summary = fixture.push(&options, &mut trust, &[same.summary.id]);

    let failed: Vec<Option<i64>> = summary.failed.iter().map(|failure| failure.local_id).collect();
    assert_eq!(failed, [Some(ids.added), Some(ids.same)]);
    assert!(
        summary.failed[1].message.contains("harness-memory scripts unhide"),
        "{}",
        summary.failed[1].message
    );

    assert_eq!(totals(&summary), [0, 1, 2, 0, 0, 1]);
    for id in [ids.added, ids.same] {
        let change = change(&summary, id);
        assert_eq!((change.failed, change.trusted, change.deleted), (true, false, false));
    }

    // The copies that went through are deleted, the failed ones stay.
    assert_eq!(fixture.local_names(), ["added", "same", "team-newer"]);
}

/// This machine's trust, rewriting local script `rewrite` (as a server would) when a version of it
/// is trusted.
struct Racing {
    inner: SqliteStore,
    path: PathBuf,
    rewrite: i64,
}

impl TrustStore for Racing {
    fn trust_of(&mut self, place: &str, script_id: i64, sha256: &str) -> Result<Option<TrustRecord>, StoreError> {
        self.inner.trust_of(place, script_id, sha256)
    }

    fn trust(&mut self, version: &TrustedVersion) -> Result<(), StoreError> {
        if version.name == "same" {
            let conn = rusqlite::Connection::open(&self.path).expect("raw local");
            conn.execute(
                "UPDATE scripts SET body = 'echo rewritten', updated_at = updated_at + 1 WHERE id = ?1",
                [self.rewrite],
            )
            .expect("rewrite");
        }

        self.inner.trust(version)
    }

    fn previous(&mut self, place: &str, script_id: i64) -> Result<Option<PreviousVersion>, StoreError> {
        self.inner.previous(place, script_id)
    }

    fn note_unapproved(&mut self, sha256: &str) -> Result<(), StoreError> {
        self.inner.note_unapproved(sha256)
    }

    fn was_unapproved(&mut self, sha256s: &[String]) -> Result<bool, StoreError> {
        self.inner.was_unapproved(sha256s)
    }
}

/// This machine's trust, failing to trust anything for local script `fail` (its team copy).
struct Failing {
    inner: SqliteStore,
    fail: i64,
}

impl TrustStore for Failing {
    fn trust_of(&mut self, place: &str, script_id: i64, sha256: &str) -> Result<Option<TrustRecord>, StoreError> {
        self.inner.trust_of(place, script_id, sha256)
    }

    fn trust(&mut self, version: &TrustedVersion) -> Result<(), StoreError> {
        let failing = self
            .inner
            .get_script(&both(REPO), &ScriptRef::Id(self.fail))?
            .is_some_and(|script| script.summary.name == version.name);

        if failing {
            return Err(StoreError::Backend("the trust table is locked".to_owned()));
        }

        self.inner.trust(version)
    }

    fn previous(&mut self, place: &str, script_id: i64) -> Result<Option<PreviousVersion>, StoreError> {
        self.inner.previous(place, script_id)
    }

    fn note_unapproved(&mut self, sha256: &str) -> Result<(), StoreError> {
        self.inner.note_unapproved(sha256)
    }

    fn was_unapproved(&mut self, sha256s: &[String]) -> Result<bool, StoreError> {
        self.inner.was_unapproved(sha256s)
    }
}
