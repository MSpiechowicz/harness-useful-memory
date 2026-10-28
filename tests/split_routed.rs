//! `SplitStore::routed`: a scope routed `remote+local` reads the team database and this computer
//! and saves here, its team notes read-only; team notes hidden on this computer are left out of
//! every read, whatever the routing. Every case runs with a second SQLite file as the team
//! database, and on PostgreSQL, libSQL and MongoDB when their test servers are set (see the
//! support modules; without them those tests print that they are skipped and pass).

mod common;
mod libsql_support;
mod mongo_support;
mod postgres_support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{both, draft, global_only, query, repo, repo_only};
use harness_memory::backend::{NotesPlace, Place};
use harness_memory::store::split::LOCAL_ID_OFFSET;
use harness_memory::store::{
    Edge, Forget, Forgotten, HiddenNotes, Hit, Link, MemoryStore, Neighborhood, Note, NoteDraft, NoteKind,
    NoteListQuery, NoteOrigin, NotePage, NotePatch, NoteSort, NoteUsage, Saved, Scope, SearchQuery, SplitStore,
    SqliteStore, StoreError, Target, View,
};
use pretty_assertions::assert_eq;
use tempfile::TempDir;

const REPO: &str = "github.com/acme/widget";

/// The team key the hidden notes of these tests are kept under.
const TEAM_KEY: &str = "team:0123456789abcdef";

/// The team database of one case.
enum Team {
    Sqlite(TempDir),
    Postgres(postgres_support::Database),
    Libsql(libsql_support::Database),
    Mongo(mongo_support::TestDb),
}

impl Team {
    /// Another store on the team database (another connection).
    fn open(&self) -> Box<dyn MemoryStore> {
        match self {
            Self::Sqlite(dir) => Box::new(SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team file")),
            Self::Postgres(db) => Box::new(db.open()),
            Self::Libsql(db) => db.open(),
            Self::Mongo(db) => Box::new(db.connect()),
        }
    }
}

/// A fresh team database, or `None` when its test server is not set.
type Fresh = fn() -> Option<Team>;

fn sqlite_team() -> Option<Team> {
    Some(Team::Sqlite(TempDir::new().expect("temp dir")))
}

fn postgres_team() -> Option<Team> {
    postgres_support::database().map(Team::Postgres)
}

fn libsql_team() -> Option<Team> {
    libsql_support::Database::libsql().map(Team::Libsql)
}

fn mongo_team() -> Option<Team> {
    mongo_support::TestDb::new().map(Team::Mongo)
}

/// One case's databases: the team one and the local file (which keeps the hidden list).
struct Fixture {
    team: Team,
    local: TempDir,
}

impl Fixture {
    fn path(&self) -> PathBuf {
        self.local.path().join("memory.sqlite3")
    }

    /// The store routed `repo` and `global`, hiding what the local file hides.
    fn routed(&self, repo: NotesPlace, global: NotesPlace) -> SplitStore {
        let local = Box::new(SqliteStore::open(&self.path()).expect("local file"));
        SplitStore::routed(self.team.open(), local, repo, global, Some(self.hidden()))
    }

    /// The hidden list, as another process sees it.
    fn hidden(&self) -> HiddenNotes {
        HiddenNotes::new(self.path(), TEAM_KEY)
    }

    /// Hides team note `id` of `view` (read from the team database).
    fn hide(&self, view: &View, id: i64) {
        let note = self.team.open().peek(view, &[id]).expect("peek").remove(0);
        assert!(self.hidden().hide(&note).expect("hide"));
    }
}

type Case = fn(&Fixture);

const CASES: [Case; 12] = [
    remote_local_reads_both_and_saves_here,
    pages_merge_both_stores_in_order,
    a_place_lists_its_store_and_counts_both,
    team_notes_of_a_remote_local_scope_are_read_only,
    hidden_team_notes_are_left_out_of_every_read,
    similar_notes_come_from_both_stores_in_a_remote_local_scope,
    deleting_every_note_only_reaches_the_store_a_scope_saves_to,
    stale_hidden_rows_stop_hiding_notes,
    another_process_skips_a_recent_prune,
    saving_under_a_hidden_title_is_refused,
    deleting_every_note_leaves_the_hidden_team_notes,
    counters_reach_the_store_of_each_note_also_when_it_is_read_only,
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
fn routed_with_a_sqlite_team_database() {
    run(sqlite_team);
}

#[test]
fn routed_with_postgres() {
    run(postgres_team);
}

#[test]
fn routed_with_libsql() {
    run(libsql_team);
}

#[test]
fn routed_with_mongo() {
    run(mongo_team);
}

/// The first page (of up to 200) of `view`, newest first.
fn listing(view: View) -> NoteListQuery {
    NoteListQuery {
        view,
        text: String::new(),
        kind: None,
        scope: None,
        place: None,
        entity: None,
        lang: None,
        useful: None,
        sort: NoteSort::Updated,
        descending: true,
        offset: 0,
        limit: 200,
        exclude: Vec::new(),
    }
}

fn listed_titles(store: &mut dyn MemoryStore, query: &NoteListQuery) -> Vec<String> {
    let page = store.list_notes(query).expect("list");
    page.rows.into_iter().map(|row| row.title).collect()
}

fn searched_titles(store: &mut dyn MemoryStore, view: View) -> Vec<String> {
    let mut found: Vec<String> = store
        .search(&query("", view))
        .expect("search")
        .into_iter()
        .map(|hit| hit.title)
        .collect();
    found.sort();
    found
}

fn remote_local_reads_both_and_saves_here(fixture: &Fixture) {
    let mut team = fixture.team.open();
    let shared = team
        .remember(
            &repo(REPO),
            &draft("Team deploy", "deploy with make", NoteKind::Decision),
        )
        .expect("team repository note");
    team.remember(&Target::Global, &draft("Team tabs", "Four wide", NoteKind::Note))
        .expect("team global note");

    let mut split = fixture.routed(NotesPlace::RemoteLocal, NotesPlace::Remote);
    let mine = split
        .remember(&repo(REPO), &draft("My deploy", "deploy by hand", NoteKind::Fix))
        .expect("saved here");
    assert!(
        mine.id >= LOCAL_ID_OFFSET,
        "a remote+local scope saves on this computer: {mine:?}"
    );

    let global = split
        .remember(&Target::Global, &draft("Global rule", "Shared", NoteKind::Note))
        .expect("global note");
    assert!(global.id < LOCAL_ID_OFFSET, "a remote scope saves in the team database");

    assert_eq!(
        searched_titles(&mut split, both(REPO)),
        ["Global rule", "My deploy", "Team deploy", "Team tabs"]
    );

    let page = split.list_notes(&listing(both(REPO))).expect("list");
    assert_eq!(page.total, 4);
    assert_eq!(page.scopes, [(Scope::Repo, 2), (Scope::Global, 2)]);

    let notes = split.get(&both(REPO), &[shared.id, mine.id]).expect("get");
    assert_eq!(notes.len(), 2);

    assert_eq!(
        split.origin(shared.id, Scope::Repo),
        NoteOrigin::Team { read_only: true }
    );
    assert_eq!(
        split.origin(global.id, Scope::Global),
        NoteOrigin::Team { read_only: false }
    );
    assert_eq!(split.origin(mine.id, Scope::Repo), NoteOrigin::Local);

    let deploy = split.search(&query("deploy", repo_only(REPO))).expect("search");
    let mut found: Vec<i64> = deploy.iter().map(|hit| hit.id).collect();
    found.sort_unstable();
    assert_eq!(found, [shared.id, mine.id], "the hits of both stores take turns");
}

fn pages_merge_both_stores_in_order(fixture: &Fixture) {
    let mut team = fixture.team.open();

    for index in 0..4 {
        team.remember(
            &repo(REPO),
            &draft(&format!("Team {index}"), "team body", NoteKind::Note),
        )
        .expect("team note");
    }

    let mut split = fixture.routed(NotesPlace::RemoteLocal, NotesPlace::RemoteLocal);

    for index in 0..3 {
        split
            .remember(
                &repo(REPO),
                &draft(&format!("Local {index}"), "local body", NoteKind::Note),
            )
            .expect("local note");
    }

    let by_title = NoteListQuery {
        sort: NoteSort::Title,
        descending: false,
        ..listing(repo_only(REPO))
    };
    let whole = listed_titles(&mut split, &by_title);
    assert_eq!(
        whole,
        ["Local 0", "Local 1", "Local 2", "Team 0", "Team 1", "Team 2", "Team 3"]
    );

    let mut paged: Vec<String> = Vec::new();

    for offset in (0..7).step_by(3) {
        let page = split
            .list_notes(&NoteListQuery {
                offset,
                limit: 3,
                ..by_title.clone()
            })
            .expect("page");
        assert_eq!(page.total, 7, "every page counts both stores");
        paged.extend(page.rows.into_iter().map(|row| row.title));
    }

    assert_eq!(paged, whole, "the pages follow on without gaps or repeats");

    let by_place = NoteListQuery {
        sort: NoteSort::Place,
        descending: true,
        ..listing(repo_only(REPO))
    };
    let places: Vec<bool> = split
        .list_notes(&by_place)
        .expect("by place")
        .rows
        .iter()
        .map(|row| row.id >= LOCAL_ID_OFFSET)
        .collect();
    assert_eq!(
        places,
        [false, false, false, false, true, true, true],
        "team first, descending"
    );
}

fn a_place_lists_its_store_and_counts_both(fixture: &Fixture) {
    let mut team = fixture.team.open();

    for index in 0..4 {
        team.remember(
            &repo(REPO),
            &draft(&format!("Team {index}"), "team body", NoteKind::Note),
        )
        .expect("team note");
    }

    let hidden = team
        .remember(&repo(REPO), &draft("Team hidden", "hidden body", NoteKind::Note))
        .expect("hidden team note");
    team.remember(&Target::Global, &draft("Team rule", "global body", NoteKind::Decision))
        .expect("team global note");
    fixture.hide(&repo_only(REPO), hidden.id);

    let mut split = fixture.routed(NotesPlace::RemoteLocal, NotesPlace::Remote);

    for index in 0..3 {
        split
            .remember(
                &repo(REPO),
                &draft(&format!("Local {index}"), "local body", NoteKind::Fix),
            )
            .expect("local note");
    }

    let by_title = |place: Option<Place>| NoteListQuery {
        sort: NoteSort::Title,
        descending: false,
        place,
        ..listing(both(REPO))
    };
    let counts = [(Place::Local, 3), (Place::Remote, 5)];
    let kind_count = |page: &NotePage, kind: NoteKind| {
        page.kinds
            .iter()
            .find(|(listed, _)| *listed == kind)
            .map(|(_, count)| *count)
    };

    let every = split.list_notes(&by_title(None)).expect("every note");
    assert_eq!(every.total, 8);
    assert_eq!(every.places, counts, "the hidden team note is not counted");

    let team_notes = split.list_notes(&by_title(Some(Place::Remote))).expect("team notes");
    let team_titles: Vec<String> = team_notes.rows.iter().map(|row| row.title.clone()).collect();
    assert_eq!(team_titles, ["Team 0", "Team 1", "Team 2", "Team 3", "Team rule"]);
    assert!(team_notes.rows.iter().all(|row| row.id < LOCAL_ID_OFFSET));
    assert_eq!(team_notes.total, 5, "the total follows the place");
    assert_eq!(team_notes.places, counts, "the place counts ignore the place");
    assert_eq!(team_notes.scopes, [(Scope::Repo, 4), (Scope::Global, 1)]);
    assert_eq!(
        (
            kind_count(&team_notes, NoteKind::Note),
            kind_count(&team_notes, NoteKind::Decision),
            kind_count(&team_notes, NoteKind::Fix)
        ),
        (Some(4), Some(1), Some(0)),
        "the kind counts follow the place"
    );

    let mine = split.list_notes(&by_title(Some(Place::Local))).expect("local notes");
    assert_eq!(
        listed_titles(&mut split, &by_title(Some(Place::Local))),
        ["Local 0", "Local 1", "Local 2"]
    );
    assert!(mine.rows.iter().all(|row| row.id >= LOCAL_ID_OFFSET));
    assert_eq!(mine.total, 3);
    assert_eq!(mine.places, counts);
    assert_eq!(mine.scopes, [(Scope::Repo, 3), (Scope::Global, 0)]);

    let mut paged: Vec<String> = Vec::new();

    for offset in (0..5).step_by(2) {
        let page = split
            .list_notes(&NoteListQuery {
                offset,
                limit: 2,
                ..by_title(Some(Place::Remote))
            })
            .expect("page");
        assert_eq!(
            (page.total, page.places.clone()),
            (5, counts.to_vec()),
            "offset {offset}"
        );
        paged.extend(page.rows.into_iter().map(|row| row.title));
    }

    assert_eq!(paged, team_titles, "the pages of one place follow on");

    let team_fixes = split
        .list_notes(&NoteListQuery {
            kind: Some(NoteKind::Fix),
            ..by_title(Some(Place::Remote))
        })
        .expect("team fixes");
    assert_eq!((team_fixes.total, team_fixes.rows.len()), (0, 0));
    assert_eq!(
        team_fixes.places,
        [(Place::Local, 3), (Place::Remote, 0)],
        "the place counts follow the kind"
    );

    let global = split
        .list_notes(&NoteListQuery {
            scope: Some(Scope::Global),
            ..by_title(Some(Place::Local))
        })
        .expect("local global notes");
    assert_eq!((global.total, global.rows.len()), (0, 0));
    assert_eq!(
        global.places,
        [(Place::Local, 0), (Place::Remote, 1)],
        "the place counts follow the scope"
    );

    // Only the team database is read: every note is a team note.
    let mut remote = fixture.routed(NotesPlace::Remote, NotesPlace::Remote);
    let elsewhere = remote
        .list_notes(&by_title(Some(Place::Local)))
        .expect("no local notes");
    assert_eq!((elsewhere.total, elsewhere.rows.len()), (0, 0));
    assert_eq!(elsewhere.places, [(Place::Local, 0), (Place::Remote, 5)]);
    assert_eq!(elsewhere.scopes, [(Scope::Repo, 0), (Scope::Global, 0)]);

    let read = remote.list_notes(&by_title(Some(Place::Remote))).expect("team notes");
    assert_eq!(
        (read.total, read.places),
        (5, vec![(Place::Local, 0), (Place::Remote, 5)])
    );
}

fn team_notes_of_a_remote_local_scope_are_read_only(fixture: &Fixture) {
    let mut team = fixture.team.open();
    let shared = team
        .remember(
            &repo(REPO),
            &draft("Team deploy", "deploy with make", NoteKind::Decision),
        )
        .expect("team repository note");
    let global = team
        .remember(&Target::Global, &draft("Team tabs", "Four wide", NoteKind::Note))
        .expect("team global note");

    let mut split = fixture.routed(NotesPlace::RemoteLocal, NotesPlace::Remote);
    let patch = NotePatch {
        append: Some("more".to_owned()),
        ..NotePatch::default()
    };

    let refused = split.update(&both(REPO), shared.id, &patch).expect_err("read-only");
    assert!(matches!(refused, StoreError::Invalid(_)), "{refused:?}");
    let message = refused.to_string();
    assert!(
        message.contains(&format!("harness-memory notes hide {}", shared.id))
            && message.contains("repository_notes is \"remote+local\"")
            && message.contains("Hide on this computer"),
        "{message}"
    );

    let deleting = split
        .forget(&both(REPO), &Target::Global, &Forget::Note(shared.id))
        .expect_err("read-only");
    assert_eq!(deleting.to_string(), message);

    let link = Link {
        from: "Deploy".to_owned(),
        relation: "uses".to_owned(),
        to: "Make".to_owned(),
        note_id: Some(shared.id),
    };
    let linking = split.link(&repo(REPO), &link).expect_err("read-only");
    assert_eq!(linking.to_string(), message);

    assert_eq!(
        team.peek(&repo_only(REPO), &[shared.id]).expect("peek")[0].revision,
        1,
        "the team note is unchanged"
    );

    let updated = split
        .update(&both(REPO), global.id, &patch)
        .expect("a remote scope stays editable");
    assert_eq!(updated.revision, 2);

    let mine = split
        .remember(&repo(REPO), &draft("My deploy", "deploy by hand", NoteKind::Fix))
        .expect("saved here");
    assert_eq!(
        split
            .update(&both(REPO), mine.id, &patch)
            .expect("a local note")
            .revision,
        2
    );

    let own_link = Link {
        note_id: Some(mine.id),
        ..link
    };
    assert_eq!(split.link(&repo(REPO), &own_link).expect("link").note_id, Some(mine.id));
}

fn hidden_team_notes_are_left_out_of_every_read(fixture: &Fixture) {
    let mut split = fixture.routed(NotesPlace::Remote, NotesPlace::Remote);
    let link = |note_id| Link {
        from: "Deploy".to_owned(),
        relation: "uses".to_owned(),
        to: "Make".to_owned(),
        note_id: Some(note_id),
    };

    let mut deploy = draft(
        "Deploy pipeline caching",
        "pipeline caching speeds deploy builds",
        NoteKind::Fix,
    );
    deploy.entities = vec!["Deploy".to_owned()];
    let hidden = split.remember(&repo(REPO), &deploy).expect("note");
    split.link(&repo(REPO), &link(hidden.id)).expect("link");

    let kept = split
        .remember(
            &repo(REPO),
            &draft(
                "Pipeline caching rules",
                "caching the pipeline speeds deploy",
                NoteKind::Note,
            ),
        )
        .expect("note");

    assert_eq!(split.list_notes(&listing(both(REPO))).expect("list").total, 2);

    // Hidden by another process: the running store sees it on its next call.
    fixture.hide(&both(REPO), hidden.id);

    let page = split.list_notes(&listing(both(REPO))).expect("list");
    assert_eq!(page.total, 1);
    assert_eq!(page.rows.iter().map(|row| row.id).collect::<Vec<_>>(), [kept.id]);
    assert_eq!(
        page.scopes,
        [(Scope::Repo, 1), (Scope::Global, 0)],
        "the counts leave it out"
    );

    let hits = split.search(&query("pipeline", both(REPO))).expect("search");
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [kept.id]);
    assert_eq!(searched_titles(&mut split, both(REPO)), ["Pipeline caching rules"]);

    assert!(split.get(&both(REPO), &[hidden.id]).expect("get").is_empty());
    assert!(split.peek(&both(REPO), &[hidden.id]).expect("peek").is_empty());
    assert!(split.similar(&both(REPO), hidden.id, 3).expect("similar").is_empty());
    assert!(
        split
            .similar(&both(REPO), kept.id, 3)
            .expect("similar")
            .iter()
            .all(|hit| hit.id != hidden.id),
        "a hidden note is not similar to anything"
    );

    let around = split.neighbors(&both(REPO), "Deploy", 1, 20).expect("neighbors");
    assert!(around.notes.iter().all(|(id, _)| *id != hidden.id), "{around:?}");
    assert!(around.edges.iter().all(|edge| edge.note_id.is_none()), "{around:?}");

    let missing = split
        .update(&both(REPO), hidden.id, &NotePatch::default())
        .expect_err("hidden");
    assert!(matches!(missing, StoreError::NotFound(_)), "{missing:?}");

    assert!(fixture.hidden().unhide(hidden.id).expect("unhide"));
    assert_eq!(split.get(&both(REPO), &[hidden.id]).expect("get").len(), 1);
    assert_eq!(split.list_notes(&listing(both(REPO))).expect("list").total, 2);
}

fn similar_notes_come_from_both_stores_in_a_remote_local_scope(fixture: &Fixture) {
    let mut team = fixture.team.open();
    let theirs = team
        .remember(
            &repo(REPO),
            &draft(
                "Pipeline caching",
                "pipeline caching speeds deploy builds",
                NoteKind::Fix,
            ),
        )
        .expect("team note");

    let mut split = fixture.routed(NotesPlace::RemoteLocal, NotesPlace::Local);
    let mine = split
        .remember(
            &repo(REPO),
            &draft(
                "Caching the pipeline",
                "caching the pipeline speeds deploy",
                NoteKind::Note,
            ),
        )
        .expect("local note");

    let similar = split.similar(&both(REPO), mine.id, 3).expect("similar");
    assert_eq!(similar.iter().map(|hit| hit.id).collect::<Vec<_>>(), [theirs.id]);

    fixture.hide(&repo_only(REPO), theirs.id);
    assert!(split.similar(&both(REPO), mine.id, 3).expect("similar").is_empty());
}

fn deleting_every_note_only_reaches_the_store_a_scope_saves_to(fixture: &Fixture) {
    let mut team = fixture.team.open();
    let shared = team
        .remember(
            &repo(REPO),
            &draft("Team deploy", "deploy with make", NoteKind::Decision),
        )
        .expect("team note");

    let mut split = fixture.routed(NotesPlace::RemoteLocal, NotesPlace::Local);
    split
        .remember(&repo(REPO), &draft("My deploy", "deploy by hand", NoteKind::Fix))
        .expect("local note");
    split
        .remember(&Target::Global, &draft("My rule", "Mine", NoteKind::Note))
        .expect("local global note");

    let forgotten = split.forget_notes(&both(REPO), &[]).expect("forget");
    assert_eq!((forgotten.repo, forgotten.global), (1, 1), "only this computer's notes");

    let left = split.list_notes(&listing(both(REPO))).expect("list");
    assert_eq!(left.rows.iter().map(|row| row.id).collect::<Vec<_>>(), [shared.id]);
    assert_eq!(team.peek(&repo_only(REPO), &[shared.id]).expect("peek").len(), 1);
    assert!(split.list_notes(&listing(global_only())).expect("list").rows.is_empty());
}

#[test]
fn the_hidden_list_lives_outside_the_schema() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("memory.sqlite3");

    let mut missing = HiddenNotes::new(path.clone(), TEAM_KEY);
    assert!(missing.ids().is_empty());
    assert!(!path.exists(), "reading creates no file");

    let mut store = SqliteStore::open(&path).expect("store");
    let saved = store
        .remember(&repo(REPO), &draft("Team note", "Body", NoteKind::Note))
        .expect("note");
    let note = store.peek(&both(REPO), &[saved.id]).expect("peek").remove(0);
    drop(store);

    let version = |path: &std::path::Path| -> i64 {
        let conn = rusqlite::Connection::open(path).expect("raw");
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version")
    };
    let before = version(&path);
    assert_eq!(before, 5);

    let mut hidden = HiddenNotes::new(path.clone(), TEAM_KEY);
    assert!(hidden.hide(&note).expect("hide"));
    assert!(!hidden.hide(&note).expect("hide again"), "hiding twice keeps one row");
    assert_eq!(hidden.ids(), [saved.id]);
    assert_eq!(version(&path), before, "the hidden list leaves user_version alone");

    let listed = hidden.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].scope, listed[0].project.as_deref(), listed[0].label.as_str()),
        (Scope::Repo, Some("git:github.com/acme/widget"), "Team note")
    );
    assert_eq!(listed[0].team_created_at, note.created_at);

    assert!(
        HiddenNotes::new(path.clone(), "team:another").ids().is_empty(),
        "each team database has its own list"
    );

    let reopened = SqliteStore::open(&path).expect("the file still opens");
    assert!(!reopened.is_newer());
    drop(reopened);

    assert!(hidden.unhide(saved.id).expect("unhide"));
    assert!(!hidden.unhide(saved.id).expect("unhide again"));
    assert!(hidden.ids().is_empty());

    let created = dir.path().join("new").join("memory.sqlite3");
    assert!(HiddenNotes::new(created.clone(), TEAM_KEY).hide(&note).expect("hide"));
    assert!(created.is_file(), "hiding creates the local file");
    assert_eq!(version(&created), 5);
}

fn stale_hidden_rows_stop_hiding_notes(fixture: &Fixture) {
    let mut team = fixture.team.open();
    let reused = team
        .remember(&repo(REPO), &draft("Reused id", "a newer note", NoteKind::Note))
        .expect("team note");
    let gone = team
        .remember(
            &repo(REPO),
            &draft("Gone note", "deleted by a teammate", NoteKind::Note),
        )
        .expect("team note");
    fixture.hide(&both(REPO), reused.id);
    fixture.hide(&both(REPO), gone.id);

    // The team database gave the hidden id to another note (SQLite and libSQL reuse ids), and a
    // teammate deleted the other hidden note.
    let raw = rusqlite::Connection::open(fixture.path()).expect("raw local");
    raw.execute(
        "UPDATE hidden_team_notes SET team_created_at = team_created_at - 60 WHERE note_id = ?1",
        [reused.id],
    )
    .expect("older row");
    team.forget(&repo_only(REPO), &repo(REPO), &Forget::Note(gone.id))
        .expect("teammate delete");

    let mut split = fixture.routed(NotesPlace::Remote, NotesPlace::Remote);
    assert_eq!(listed_titles(&mut split, &listing(both(REPO))), ["Reused id"]);
    assert_eq!(split.get(&both(REPO), &[reused.id]).expect("get").len(), 1);
    assert!(fixture.hidden().ids().is_empty(), "both rows left the hidden list");
}

fn another_process_skips_a_recent_prune(fixture: &Fixture) {
    let mut team = fixture.team.open();
    let first = team
        .remember(&repo(REPO), &draft("First", "pruned at once", NoteKind::Note))
        .expect("team note");
    let second = team
        .remember(
            &repo(REPO),
            &draft("Second", "pruned once the window passed", NoteKind::Note),
        )
        .expect("team note");
    let raw = rusqlite::Connection::open(fixture.path()).expect("raw local");
    let stale = |id: i64| {
        raw.execute(
            "UPDATE hidden_team_notes SET team_created_at = team_created_at - 60 WHERE note_id = ?1",
            [id],
        )
        .expect("older row");
    };

    fixture.hide(&both(REPO), first.id);
    stale(first.id);
    let mut split = fixture.routed(NotesPlace::Remote, NotesPlace::Remote);
    assert_eq!(listed_titles(&mut split, &listing(both(REPO))), ["Second", "First"]);

    // The next process within the window does not read the team database for the hidden list.
    fixture.hide(&both(REPO), second.id);
    stale(second.id);
    let mut next = fixture.routed(NotesPlace::Remote, NotesPlace::Remote);
    assert_eq!(listed_titles(&mut next, &listing(both(REPO))), ["First"]);
    assert_eq!(fixture.hidden().ids(), [second.id]);

    raw.execute("UPDATE hidden_team_notes_meta SET pruned_at = pruned_at - 301", [])
        .expect("window passed");
    let mut later = fixture.routed(NotesPlace::Remote, NotesPlace::Remote);
    assert_eq!(listed_titles(&mut later, &listing(both(REPO))), ["Second", "First"]);
    assert!(fixture.hidden().ids().is_empty());
}

fn saving_under_a_hidden_title_is_refused(fixture: &Fixture) {
    let mut split = fixture.routed(NotesPlace::Remote, NotesPlace::Remote);
    let hidden = split
        .remember(
            &repo(REPO),
            &draft("Deploy steps", "Run make deploy", NoteKind::Decision),
        )
        .expect("team note");
    fixture.hide(&both(REPO), hidden.id);

    let refused = split
        .remember(
            &repo(REPO),
            &draft("Deploy steps", "Run make release", NoteKind::Decision),
        )
        .expect_err("hidden title");
    assert!(matches!(refused, StoreError::Invalid(_)), "{refused:?}");
    assert!(
        refused
            .to_string()
            .contains(&format!("harness-memory notes unhide {}", hidden.id)),
        "{refused}"
    );

    let mut team = fixture.team.open();
    let note = team.peek(&both(REPO), &[hidden.id]).expect("peek").remove(0);
    assert_eq!((note.body.as_str(), note.revision), ("Run make deploy", 1), "unchanged");

    split
        .remember(
            &repo(REPO),
            &draft("Release steps", "Run make release", NoteKind::Decision),
        )
        .expect("another title saves");

    let mut shared = fixture.routed(NotesPlace::RemoteLocal, NotesPlace::Remote);
    let mine = shared
        .remember(&repo(REPO), &draft("Deploy steps", "My way", NoteKind::Decision))
        .expect("a remote+local scope saves here");
    assert!(mine.id >= LOCAL_ID_OFFSET);
}

fn deleting_every_note_leaves_the_hidden_team_notes(fixture: &Fixture) {
    let mut split = fixture.routed(NotesPlace::Remote, NotesPlace::Remote);
    let hidden = split
        .remember(&repo(REPO), &draft("Hidden", "Body", NoteKind::Note))
        .expect("team note");
    split
        .remember(&repo(REPO), &draft("Shown", "Body", NoteKind::Note))
        .expect("team note");
    split
        .remember(&Target::Global, &draft("Rule", "Body", NoteKind::Note))
        .expect("team note");
    fixture.hide(&both(REPO), hidden.id);

    let visible = split.list_notes(&listing(both(REPO))).expect("list").total;
    let forgotten = split.forget_notes(&both(REPO), &[]).expect("forget");
    assert_eq!((forgotten.repo, forgotten.global), (1, 1));
    assert_eq!(forgotten.total(), visible, "deletes what the list shows");

    let mut team = fixture.team.open();
    assert_eq!(team.peek(&both(REPO), &[hidden.id]).expect("peek").len(), 1);
    assert!(fixture.hidden().unhide(hidden.id).expect("unhide"));
    assert_eq!(listed_titles(&mut split, &listing(both(REPO))), ["Hidden"]);
}

#[cfg(unix)]
#[test]
fn an_unreadable_hidden_list_hides_nothing() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("temp dir");
    let team_path = dir.path().join("team.sqlite3");
    let mut team = SqliteStore::open(&team_path).expect("team file");
    let saved = team
        .remember(&repo(REPO), &draft("Team note", "Body", NoteKind::Note))
        .expect("note");
    let note = team.peek(&both(REPO), &[saved.id]).expect("peek").remove(0);

    // A local file without the table that cannot be written.
    let path = dir.path().join("memory.sqlite3");
    drop(SqliteStore::open(&path).expect("local file"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).expect("read-only");

    if rusqlite::Connection::open(&path)
        .and_then(|conn| conn.execute_batch("CREATE TABLE probe (x)"))
        .is_ok()
    {
        eprintln!("skipped: the file stays writable (running as root?)");
        return;
    }

    let local = Box::new(SqliteStore::open(&dir.path().join("other.sqlite3")).expect("other file"));
    let hidden = HiddenNotes::new(path.clone(), TEAM_KEY);
    let mut split = SplitStore::routed(
        Box::new(team),
        local,
        NotesPlace::Remote,
        NotesPlace::Remote,
        Some(hidden),
    );

    assert_eq!(listed_titles(&mut split, &listing(both(REPO))), ["Team note"]);
    assert_eq!(split.get(&both(REPO), &[saved.id]).expect("get").len(), 1);

    let mut list = HiddenNotes::new(path, TEAM_KEY);
    assert!(list.ids().is_empty());
    assert!(list.list().is_empty());
    assert!(list.hide(&note).is_err(), "hiding needs the file");
    assert!(list.unhide(saved.id).is_err(), "so does showing again");
}

#[cfg(unix)]
#[test]
fn team_changes_fail_while_the_hidden_list_cannot_be_read() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("temp dir");
    let team_path = dir.path().join("team.sqlite3");
    let mut team = SqliteStore::open(&team_path).expect("team file");
    let hidden_note = team
        .remember(&repo(REPO), &draft("Hidden note", "Body", NoteKind::Note))
        .expect("note");
    let shown = team
        .remember(&repo(REPO), &draft("Shown note", "Body", NoteKind::Note))
        .expect("note");
    let note = team.peek(&both(REPO), &[hidden_note.id]).expect("peek").remove(0);

    let path = dir.path().join("memory.sqlite3");
    assert!(HiddenNotes::new(path.clone(), TEAM_KEY).hide(&note).expect("hide"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("unreadable");

    if std::fs::File::open(&path).is_ok() {
        eprintln!("skipped: the file stays readable (running as root?)");
        return;
    }

    let local = Box::new(SqliteStore::open(&dir.path().join("other.sqlite3")).expect("other file"));
    let hidden = HiddenNotes::new(path.clone(), TEAM_KEY);
    let mut split = SplitStore::routed(
        Box::new(team),
        local,
        NotesPlace::Remote,
        NotesPlace::Remote,
        Some(hidden),
    );

    let failed = split.forget_notes(&both(REPO), &[]).expect_err("delete all");
    assert!(
        failed
            .to_string()
            .contains("cannot read the team notes hidden on this computer"),
        "{failed}"
    );
    assert!(split.update(&both(REPO), shown.id, &NotePatch::default()).is_err());
    assert!(split.forget(&both(REPO), &repo(REPO), &Forget::Note(shown.id)).is_err());
    assert!(
        split
            .remember(&repo(REPO), &draft("New note", "Body", NoteKind::Note))
            .is_err()
    );

    // Reads still work, showing everything.
    assert_eq!(listed_titles(&mut split, &listing(both(REPO))).len(), 2);

    let mut team = SqliteStore::open(&team_path).expect("team file");
    let left = team.peek(&both(REPO), &[hidden_note.id, shown.id]).expect("peek");
    assert_eq!(left.len(), 2, "no team note was deleted");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("readable again");
}

#[test]
fn hiding_at_once_never_passes_the_limit() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("memory.sqlite3");
    let mut store = SqliteStore::open(&path).expect("store");
    let saved = store
        .remember(&repo(REPO), &draft("Team note", "Body", NoteKind::Note))
        .expect("note");
    let note = store.peek(&both(REPO), &[saved.id]).expect("peek").remove(0);
    drop(store);

    let mut hidden = HiddenNotes::new(path.clone(), TEAM_KEY);
    for id in 1..=498 {
        assert!(
            hidden
                .hide(&harness_memory::store::Note { id, ..note.clone() })
                .expect("hide")
        );
    }

    let threads: Vec<_> = (0..8)
        .map(|index| {
            let (path, note) = (path.clone(), note.clone());
            std::thread::spawn(move || {
                let id = 10_000 + index;
                HiddenNotes::new(path, TEAM_KEY)
                    .hide(&harness_memory::store::Note { id, ..note })
                    .is_ok()
            })
        })
        .collect();
    let hid = threads
        .into_iter()
        .map(|thread| thread.join().expect("thread"))
        .filter(|ok| *ok)
        .count();

    let rows: i64 = rusqlite::Connection::open(&path)
        .expect("raw")
        .query_row("SELECT count(*) FROM hidden_team_notes", [], |row| row.get(0))
        .expect("count");
    assert_eq!((hid, rows), (2, 500), "only two more fit");
}

/// A usefulness counter is not a change to a note: it counts on a read-only team note and on one
/// kept on this computer, each in the store that holds the note, and changes nothing else of them.
fn counters_reach_the_store_of_each_note_also_when_it_is_read_only(fixture: &Fixture) {
    let mut team = fixture.team.open();
    let shared = team
        .remember(
            &repo(REPO),
            &draft("Team deploy", "deploy with make", NoteKind::Decision),
        )
        .expect("team repository note");

    let mut split = fixture.routed(NotesPlace::RemoteLocal, NotesPlace::Remote);
    let mine = split
        .remember(&repo(REPO), &draft("My deploy", "deploy by hand", NoteKind::Fix))
        .expect("saved here")
        .id;
    assert!(mine >= LOCAL_ID_OFFSET);
    assert!(split.origin(shared.id, Scope::Repo) == NoteOrigin::Team { read_only: true });

    let before = split.list_notes(&listing(both(REPO))).expect("list");

    split.record_shown(&[shared.id, mine, mine]).expect("shown");
    split.record_shown(&[shared.id]).expect("shown");
    split.record_useful(&[shared.id], 100).expect("useful");
    split.record_useful(&[mine], 200).expect("useful");

    let counters = |shown, useful, last_useful_at| {
        Some(NoteUsage {
            shown,
            useful,
            last_useful_at,
        })
    };

    // Each store has counted its own note.
    let at_team = team.peek(&repo_only(REPO), &[shared.id]).expect("peek").remove(0);
    assert_eq!(at_team.usage, counters(2, 1, Some(100)));
    assert_eq!((at_team.revision, at_team.hits), (1, 0), "the team note is unchanged");

    let after = split.list_notes(&listing(both(REPO))).expect("list");
    let usage = |page: &NotePage, id: i64| page.rows.iter().find(|row| row.id == id).expect("listed").usage;
    assert_eq!(usage(&after, shared.id), counters(2, 1, Some(100)));
    assert_eq!(usage(&after, mine), counters(1, 1, Some(200)));

    for (was, now) in before.rows.iter().zip(&after.rows) {
        assert_eq!(
            (was.id, was.updated_at, was.revision, was.hits),
            (now.id, now.updated_at, now.revision, now.hits)
        );
    }

    // The merged list orders by them, the note found useful most recently first.
    let latest = NoteListQuery {
        sort: NoteSort::LastUseful,
        ..listing(both(REPO))
    };
    assert_eq!(
        split
            .list_notes(&latest)
            .expect("list")
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![mine, shared.id]
    );

    // And it is still read-only.
    let patch = NotePatch {
        append: Some("more".to_owned()),
        ..NotePatch::default()
    };
    assert!(split.update(&both(REPO), shared.id, &patch).is_err());
}

/// A team store that counts the notes it is asked to read.
struct Reads {
    inner: SqliteStore,
    reads: Arc<AtomicUsize>,
}

impl Reads {
    fn read(&self) {
        self.reads.fetch_add(1, Ordering::SeqCst);
    }
}

impl MemoryStore for Reads {
    fn remember(&mut self, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError> {
        self.inner.remember(target, draft)
    }

    fn update(&mut self, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError> {
        self.inner.update(view, id, patch)
    }

    fn get(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        self.read();
        self.inner.get(view, ids)
    }

    fn peek(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        self.read();
        self.inner.peek(view, ids)
    }

    fn list_notes(&mut self, query: &NoteListQuery) -> Result<NotePage, StoreError> {
        self.read();
        self.inner.list_notes(query)
    }

    fn search(&mut self, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
        self.read();
        self.inner.search(query)
    }

    fn record_shown(&mut self, ids: &[i64]) -> Result<(), StoreError> {
        self.inner.record_shown(ids)
    }

    fn record_useful(&mut self, ids: &[i64], at: i64) -> Result<(), StoreError> {
        self.inner.record_useful(ids, at)
    }

    fn similar(&mut self, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
        self.read();
        self.inner.similar(view, id, limit)
    }

    fn link(&mut self, target: &Target, link: &Link) -> Result<Edge, StoreError> {
        self.inner.link(target, link)
    }

    fn neighbors(&mut self, view: &View, entity: &str, depth: u8, limit: usize) -> Result<Neighborhood, StoreError> {
        self.read();
        self.inner.neighbors(view, entity, depth, limit)
    }

    fn forget(&mut self, view: &View, target: &Target, what: &Forget) -> Result<usize, StoreError> {
        self.inner.forget(view, target, what)
    }

    fn forget_notes(&mut self, view: &View, exclude: &[i64]) -> Result<Forgotten, StoreError> {
        self.inner.forget_notes(view, exclude)
    }

    fn find_by_title(&mut self, target: &Target, title: &str) -> Result<Option<Note>, StoreError> {
        self.read();
        self.inner.find_by_title(target, title)
    }
}

/// A counter never waits for the team database: with the hidden list due for a prune (which reads
/// the team database with the long timeouts of a call), it reads the list as it is and counts only
/// what is not hidden. A call that replies still prunes.
#[test]
fn a_counter_does_not_prune_the_hidden_list() {
    let dir = TempDir::new().expect("temp dir");
    let local = dir.path().join("memory.sqlite3");
    let team_file = dir.path().join("team.sqlite3");

    let mut team = SqliteStore::open(&team_file).expect("team file");
    let kept = team
        .remember(&repo(REPO), &draft("Kept", "counted as shown", NoteKind::Note))
        .expect("team note");
    let hidden = team
        .remember(&repo(REPO), &draft("Hidden", "never counted", NoteKind::Note))
        .expect("team note");
    let note = team.peek(&both(REPO), &[hidden.id]).expect("peek").remove(0);
    assert!(HiddenNotes::new(local.clone(), TEAM_KEY).hide(&note).expect("hide"));

    // Pruned long ago (the epoch): the next call that replies is due to prune.
    let raw = rusqlite::Connection::open(&local).expect("raw local");
    raw.execute(
        "INSERT INTO hidden_team_notes_meta (team, pruned_at) VALUES (?1, 1)
         ON CONFLICT (team) DO UPDATE SET pruned_at = 1",
        [TEAM_KEY],
    )
    .expect("window passed");

    let reads = Arc::new(AtomicUsize::new(0));
    let counting = Reads {
        inner: SqliteStore::open(&team_file).expect("team file"),
        reads: Arc::clone(&reads),
    };
    let mut split = SplitStore::routed(
        Box::new(counting),
        Box::new(SqliteStore::open(&local).expect("local file")),
        NotesPlace::Remote,
        NotesPlace::Remote,
        Some(HiddenNotes::new(local, TEAM_KEY)),
    );

    split.record_shown(&[kept.id, hidden.id]).expect("shown");
    split.record_useful(&[kept.id, hidden.id], 100).expect("useful");
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "the counters did not read the team database"
    );

    let mut counted = |id: i64| {
        let usage = team.peek(&both(REPO), &[id]).expect("peek").remove(0).usage;
        usage.map(|usage| (usage.shown, usage.useful))
    };
    assert_eq!(counted(hidden.id), Some((0, 0)), "a hidden note is not counted");
    assert_eq!(counted(kept.id), Some((1, 1)), "a note that is not hidden is");

    // A call that replies prunes the list (the hidden note still exists, so it stays hidden).
    listed_titles(&mut split, &listing(both(REPO)));
    assert!(reads.load(Ordering::SeqCst) > 0, "a read reached the team database");
    let pruned_at: i64 = raw
        .query_row("SELECT pruned_at FROM hidden_team_notes_meta", [], |row| row.get(0))
        .expect("pruned_at");
    assert!(pruned_at > 1, "the prune was recorded");
}
