//! `SplitScripts`: with `scripts = "remote+local"` the team's scripts and this computer's are read,
//! new ones saved here, a name looked up here first and the team's scripts read-only; team scripts
//! hidden on this computer are left out of every read with `remote` and `remote+local` alike.
//! Every case runs with a second SQLite file as the team database, and on PostgreSQL, libSQL and
//! MongoDB when their test servers are set (see the support modules; without them those tests
//! print that they are skipped and pass).

mod common;
mod libsql_support;
mod mongo_support;
mod postgres_support;
mod script_teams;

use std::path::PathBuf;

use common::scripts::{REPO, script};
use common::{both, global_only, repo, repo_only};
use harness_memory::backend::NotesPlace;
use harness_memory::store::split::LOCAL_ID_OFFSET;
use harness_memory::store::{
    HiddenScripts, Lazy, Script, ScriptOrigin, ScriptRef, ScriptStore, SplitScripts, SqliteStore, StoreError, Target,
    View,
};
use pretty_assertions::assert_eq;
use script_teams::{Fresh, Team};
use tempfile::TempDir;

/// The team key the hidden scripts of these tests are kept under.
const TEAM_KEY: &str = "team:0123456789abcdef";

/// One case's databases: the team one and the local file (which keeps the hidden list).
struct Fixture {
    team: Team,
    local: TempDir,
}

impl Fixture {
    fn path(&self) -> PathBuf {
        self.local.path().join("memory.sqlite3")
    }

    /// The scripts as `place` routes them, hiding what the local file hides.
    fn split(&self, place: NotesPlace) -> SplitScripts {
        let local = Box::new(SqliteStore::open(&self.path()).expect("local file"));
        SplitScripts::new(self.team.open(), local, place, Some(self.hidden()))
    }

    /// The hidden list, as another process sees it.
    fn hidden(&self) -> HiddenScripts {
        HiddenScripts::new(self.path(), TEAM_KEY)
    }

    /// Saves script `name` into `target` of the team database, as a teammate would; its id.
    fn team_save(&self, target: &Target, name: &str, body: &str) -> i64 {
        let saved = self
            .team
            .open()
            .save_script(target, &script(name, "", body))
            .expect("team script");
        saved.script.summary.id
    }

    /// Hides team script `id` of `view` (read from the team database).
    fn hide(&self, view: &View, id: i64) {
        let found = self.team.open().get_script(view, &ScriptRef::Id(id)).expect("get");
        let summary = found.expect("a team script").summary;
        assert!(self.hidden().hide(&summary).expect("hide"));
    }
}

type Case = fn(&Fixture);

const CASES: [Case; 6] = [
    remote_local_lists_both_and_finds_this_computers_first,
    team_scripts_are_read_only_with_remote_local,
    hidden_team_scripts_are_left_out_of_every_read,
    saving_under_a_hidden_name_is_refused,
    stale_hidden_rows_stop_hiding_scripts,
    remote_never_opens_the_local_file,
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
fn split_scripts_with_a_sqlite_team_database() {
    run(script_teams::sqlite);
}

#[test]
fn split_scripts_with_postgres() {
    run(script_teams::postgres);
}

#[test]
fn split_scripts_with_libsql() {
    run(script_teams::libsql);
}

#[test]
fn split_scripts_with_mongo() {
    run(script_teams::mongo);
}

/// `(name, body)` of `found`.
fn named(found: Option<Script>) -> (String, String) {
    let script = found.expect("a script");
    (script.summary.name, script.body)
}

/// `(id, name)` of each script `store` lists in `view`.
fn listed(store: &mut dyn ScriptStore, view: &View) -> Vec<(i64, String)> {
    store
        .list_scripts(view, None)
        .expect("list")
        .into_iter()
        .map(|summary| (summary.id, summary.name))
        .collect()
}

fn remote_local_lists_both_and_finds_this_computers_first(fixture: &Fixture) {
    let team_deploy = fixture.team_save(&repo(REPO), "deploy", "echo team deploy\n");
    let team_lint = fixture.team_save(&Target::Global, "lint", "echo team lint\n");

    let mut split = fixture.split(NotesPlace::RemoteLocal);
    let mine = split
        .save_script(&repo(REPO), &script("deploy", "", "echo my deploy\n"))
        .expect("saved here");
    let my_deploy = mine.script.summary.id;
    assert!(mine.created, "the same name in the team database is another script");
    assert!(my_deploy >= LOCAL_ID_OFFSET, "saved on this computer: {my_deploy}");

    let my_lint = split
        .save_script(&Target::Global, &script("lint", "", "echo my lint\n"))
        .expect("saved here")
        .script
        .summary
        .id;

    // Repository scripts first, then by name, this computer's before the team's.
    assert_eq!(
        listed(&mut split, &both(REPO)),
        [
            (my_deploy, "deploy".to_owned()),
            (team_deploy, "deploy".to_owned()),
            (my_lint, "lint".to_owned()),
            (team_lint, "lint".to_owned()),
        ]
    );

    // A name is looked up on this computer first, in each scope.
    let by_name = |split: &mut SplitScripts, view: &View, name: &str| {
        named(split.get_script(view, &ScriptRef::Name(name.to_owned())).expect("get"))
    };
    assert_eq!(by_name(&mut split, &both(REPO), "deploy").1, "echo my deploy\n");
    assert_eq!(by_name(&mut split, &global_only(), "lint").1, "echo my lint\n");

    // Shown ids round-trip to their own store.
    let get =
        |split: &mut SplitScripts, id: i64| named(split.get_script(&both(REPO), &ScriptRef::Id(id)).expect("get"));
    assert_eq!(get(&mut split, team_deploy).1, "echo team deploy\n");
    assert_eq!(get(&mut split, my_deploy).1, "echo my deploy\n");

    let raw = my_deploy - LOCAL_ID_OFFSET;
    let mut local = SqliteStore::open(&fixture.path()).expect("local file");
    let in_file = local.get_script(&both(REPO), &ScriptRef::Id(raw)).expect("get");
    assert_eq!(named(in_file).1, "echo my deploy\n");

    assert_eq!(split.origin(my_deploy), ScriptOrigin::Local { raw });
    assert_eq!(
        split.origin(team_deploy),
        ScriptOrigin::Team {
            raw: team_deploy,
            read_only: true
        }
    );

    // Without this computer's script, the name finds the team's.
    assert!(
        split
            .delete_script(&both(REPO), &ScriptRef::Id(my_deploy))
            .expect("delete")
    );
    assert_eq!(by_name(&mut split, &both(REPO), "deploy").1, "echo team deploy\n");
}

fn team_scripts_are_read_only_with_remote_local(fixture: &Fixture) {
    let team_deploy = fixture.team_save(&repo(REPO), "deploy", "echo team\n");
    let mut split = fixture.split(NotesPlace::RemoteLocal);
    let mine = split
        .save_script(&repo(REPO), &script("mine", "", "echo mine\n"))
        .expect("saved here")
        .script
        .summary
        .id;

    for named in [ScriptRef::Id(team_deploy), ScriptRef::Name("deploy".to_owned())] {
        let refused = split.delete_script(&both(REPO), &named).expect_err("read-only");
        assert!(matches!(refused, StoreError::Invalid(_)), "{refused:?}");
        assert!(
            refused
                .to_string()
                .contains(&format!("harness-memory scripts hide {team_deploy}")),
            "{refused}"
        );
    }

    // Deleting many leaves the team's scripts.
    assert_eq!(
        split
            .delete_scripts(&both(REPO), &[team_deploy, mine])
            .expect("delete many"),
        1
    );
    assert_eq!(listed(&mut split, &both(REPO)), [(team_deploy, "deploy".to_owned())]);

    // A run of a team script is still counted in the team database.
    split.record_run(team_deploy, Some(0), 1_700_000_000).expect("run");
    let counted = fixture
        .team
        .open()
        .get_script(&both(REPO), &ScriptRef::Id(team_deploy))
        .expect("get")
        .expect("team script");
    assert_eq!(counted.summary.run_count, 1);
}

fn hidden_team_scripts_are_left_out_of_every_read(fixture: &Fixture) {
    let hidden = fixture.team_save(&repo(REPO), "deploy", "echo repo deploy\n");
    let global = fixture.team_save(&Target::Global, "deploy", "echo global deploy\n");
    fixture.hide(&both(REPO), hidden);

    for place in [NotesPlace::Remote, NotesPlace::RemoteLocal] {
        let mut split = fixture.split(place);

        assert_eq!(
            listed(&mut split, &both(REPO)),
            [(global, "deploy".to_owned())],
            "{place:?}"
        );
        assert!(listed(&mut split, &repo_only(REPO)).is_empty(), "{place:?}");
        assert_eq!(
            split.get_script(&both(REPO), &ScriptRef::Id(hidden)).expect("get"),
            None,
            "{place:?}"
        );

        // The name falls through to the global script.
        let found = split
            .get_script(&both(REPO), &ScriptRef::Name("deploy".to_owned()))
            .expect("get");
        assert_eq!(named(found).1, "echo global deploy\n", "{place:?}");
    }

    // With `remote` the team's scripts are editable, but a hidden one is not found.
    let mut split = fixture.split(NotesPlace::Remote);
    assert!(
        !split
            .delete_script(&both(REPO), &ScriptRef::Id(hidden))
            .expect("delete")
    );
    assert_eq!(split.delete_scripts(&both(REPO), &[hidden]).expect("delete many"), 0);
    assert_eq!(listed(fixture.team.open().as_mut(), &repo_only(REPO)).len(), 1);

    assert!(fixture.hidden().unhide(hidden).expect("unhide"));
    assert_eq!(listed(&mut split, &repo_only(REPO)), [(hidden, "deploy".to_owned())]);
}

fn saving_under_a_hidden_name_is_refused(fixture: &Fixture) {
    let hidden = fixture.team_save(&repo(REPO), "deploy", "echo team\n");
    fixture.hide(&both(REPO), hidden);

    let mut split = fixture.split(NotesPlace::Remote);
    let refused = split
        .save_script(&repo(REPO), &script("deploy", "", "echo changed\n"))
        .expect_err("hidden name");
    assert!(
        refused
            .to_string()
            .contains(&format!("harness-memory scripts unhide {hidden}")),
        "{refused}"
    );

    // The same name in the other scope, or saved here with `remote+local`, is another script.
    split
        .save_script(&Target::Global, &script("deploy", "", "echo global\n"))
        .expect("global");
    let mut shared = fixture.split(NotesPlace::RemoteLocal);
    let mine = shared
        .save_script(&repo(REPO), &script("deploy", "", "echo mine\n"))
        .expect("saved here");
    assert!(mine.script.summary.id >= LOCAL_ID_OFFSET);

    let team = fixture
        .team
        .open()
        .get_script(&repo_only(REPO), &ScriptRef::Id(hidden))
        .expect("get");
    assert_eq!(named(team).1, "echo team\n", "the hidden script is unchanged");
}

fn stale_hidden_rows_stop_hiding_scripts(fixture: &Fixture) {
    let gone = fixture.team_save(&repo(REPO), "gone", "echo gone\n");
    fixture.hide(&both(REPO), gone);
    assert_eq!(fixture.hidden().ids(), [gone]);

    let mut team = fixture.team.open();
    assert!(team.delete_script(&both(REPO), &ScriptRef::Id(gone)).expect("delete"));

    // The first read prunes (none has yet), dropping the row of the script that is gone.
    let mut split = fixture.split(NotesPlace::Remote);
    assert!(listed(&mut split, &both(REPO)).is_empty());
    assert!(fixture.hidden().ids().is_empty(), "the stale row is dropped");
}

fn remote_never_opens_the_local_file(fixture: &Fixture) {
    let unopened: Box<dyn ScriptStore> = Box::new(Lazy::new(|| Err("the local file must not be opened".to_owned())));
    let mut split = SplitScripts::new(fixture.team.open(), unopened, NotesPlace::Remote, None);

    let saved = split
        .save_script(&repo(REPO), &script("deploy", "", "echo team\n"))
        .expect("saved in the team database");
    let id = saved.script.summary.id;
    assert!(id < LOCAL_ID_OFFSET);
    assert_eq!(
        split.origin(id),
        ScriptOrigin::Team {
            raw: id,
            read_only: false
        }
    );

    assert_eq!(listed(&mut split, &both(REPO)), [(id, "deploy".to_owned())]);
    assert_eq!(
        split
            .get_script(&both(REPO), &ScriptRef::Id(LOCAL_ID_OFFSET + 1))
            .expect("get"),
        None
    );
    split.record_run(id, Some(0), 1_700_000_000).expect("run");
    assert!(
        split
            .delete_script(&both(REPO), &ScriptRef::Name("deploy".to_owned()))
            .expect("delete")
    );
}

#[test]
fn the_hidden_scripts_list_lives_outside_the_schema() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("memory.sqlite3");

    let mut missing = HiddenScripts::new(path.clone(), TEAM_KEY);
    assert!(missing.ids().is_empty());
    assert!(!path.exists(), "reading creates no file");

    let mut store = SqliteStore::open(&path).expect("store");
    let saved = store
        .save_script(&repo(REPO), &script("deploy", "", "echo team\n"))
        .expect("script");
    drop(store);

    let version = |path: &std::path::Path| -> i64 {
        let conn = rusqlite::Connection::open(path).expect("raw");
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version")
    };
    let before = version(&path);

    let mut hidden = HiddenScripts::new(path.clone(), TEAM_KEY);
    assert!(hidden.hide(&saved.script.summary).expect("hide"));
    assert!(!hidden.hide(&saved.script.summary).expect("hide again"));
    assert_eq!(hidden.ids(), [saved.script.summary.id]);
    assert_eq!(version(&path), before, "the hidden list leaves user_version alone");

    let listed = hidden.list();
    assert_eq!(
        (listed[0].project.as_deref(), listed[0].label.as_str()),
        (Some("git:github.com/acme/widget"), "deploy")
    );

    let conn = rusqlite::Connection::open(&path).expect("raw");
    let mut tables = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'hidden_team_%' ORDER BY name")
        .expect("tables");
    let tables: Vec<String> = tables
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("names");
    assert_eq!(
        tables,
        ["hidden_team_scripts", "hidden_team_scripts_meta"],
        "a table of its own"
    );

    assert!(hidden.unhide(saved.script.summary.id).expect("unhide"));
    assert!(hidden.ids().is_empty());
}

#[cfg(unix)]
#[test]
fn team_script_changes_fail_while_the_hidden_list_cannot_be_read() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("temp dir");
    let team_path = dir.path().join("team.sqlite3");
    let mut team = SqliteStore::open(&team_path).expect("team file");
    let hidden_id = team
        .save_script(&repo(REPO), &script("hidden", "", "echo hidden\n"))
        .expect("script")
        .script;
    let shown = team
        .save_script(&repo(REPO), &script("shown", "", "echo shown\n"))
        .expect("script")
        .script
        .summary
        .id;

    let path = dir.path().join("memory.sqlite3");
    assert!(
        HiddenScripts::new(path.clone(), TEAM_KEY)
            .hide(&hidden_id.summary)
            .expect("hide")
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("unreadable");

    if std::fs::File::open(&path).is_ok() {
        eprintln!("skipped: the file stays readable (running as root?)");
        return;
    }

    let local = Box::new(SqliteStore::open(&dir.path().join("other.sqlite3")).expect("other file"));
    let hidden = HiddenScripts::new(path.clone(), TEAM_KEY);
    let mut split = SplitScripts::new(Box::new(team), local, NotesPlace::Remote, Some(hidden));

    let failed = split
        .delete_scripts(&both(REPO), &[shown, hidden_id.summary.id])
        .expect_err("delete many");
    assert!(
        failed
            .to_string()
            .contains("cannot read the team scripts hidden on this computer"),
        "{failed}"
    );
    assert!(split.delete_script(&both(REPO), &ScriptRef::Id(shown)).is_err());
    assert!(
        split
            .save_script(&repo(REPO), &script("new", "", "echo new\n"))
            .is_err()
    );

    // Reads still work, showing everything.
    assert_eq!(listed(&mut split, &both(REPO)).len(), 2);

    let mut team = SqliteStore::open(&team_path).expect("team file");
    assert_eq!(listed(&mut team, &both(REPO)).len(), 2, "no team script was deleted");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("readable again");
}
