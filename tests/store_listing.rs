//! `MemoryStore::list_notes` on every backend: the shared cases (`common::listing`) on SQLite, and
//! on libSQL, PostgreSQL and MongoDB when their test servers are set (see the support modules;
//! without them those tests print that they are skipped and pass), on a `SplitStore` both ways
//! round, and the order by the time of the latest write with the times set behind the store.

mod common;
mod libsql_support;
mod mongo_support;
mod postgres_support;

use common::listing::{CASES, REPO, listing};
use common::{both, draft, repo};
use harness_memory::store::{MemoryStore, NoteKind, NoteListQuery, NoteSort, Scope, SplitStore, SqliteStore, Target};
use mongodb::bson::{Document, doc};
use pretty_assertions::assert_eq;
use tempfile::TempDir;

/// Runs every case on a fresh database `fresh` opens (with what keeps it alive), until it
/// returns `None`.
fn each_case<T>(fresh: impl Fn() -> Option<(T, Box<dyn MemoryStore>)>) {
    for (name, case) in CASES {
        eprintln!("list case {name}");
        let Some((_kept, mut store)) = fresh() else { return };
        case(store.as_mut());
    }
}

#[test]
fn every_case_on_sqlite() {
    each_case(|| {
        let dir = TempDir::new().expect("temp dir");
        let store: Box<dyn MemoryStore> =
            Box::new(SqliteStore::open(&dir.path().join("memory.sqlite3")).expect("store"));
        Some((dir, store))
    });
}

#[test]
fn every_case_on_libsql() {
    each_case(|| {
        let db = libsql_support::Database::libsql()?;
        let store = db.open();
        Some((db, store))
    });
}

#[test]
fn every_case_on_postgres() {
    each_case(|| {
        let db = postgres_support::database()?;
        let store: Box<dyn MemoryStore> = Box::new(db.open());
        Some((db, store))
    });
}

#[test]
fn every_case_on_mongo() {
    each_case(|| {
        let db = mongo_support::TestDb::new()?;
        let store: Box<dyn MemoryStore> = Box::new(db.connect());
        Some((db, store))
    });
}

/// Runs every case on split stores whose team database `team` opens, both ways round.
fn split_cases<T>(team: impl Fn() -> Option<(T, Box<dyn MemoryStore>)>) {
    for local_scope in [Scope::Repo, Scope::Global] {
        each_case(|| {
            let (kept, team) = team()?;
            let local = TempDir::new().expect("temp dir");
            let store = SqliteStore::open(&local.path().join("memory.sqlite3")).expect("local file");
            let split: Box<dyn MemoryStore> = Box::new(SplitStore::new(team, Box::new(store), local_scope));
            Some(((kept, local), split))
        });
    }
}

#[test]
fn every_case_on_a_split_store() {
    split_cases(|| {
        let dir = TempDir::new().expect("temp dir");
        let team: Box<dyn MemoryStore> =
            Box::new(SqliteStore::open(&dir.path().join("team.sqlite3")).expect("team file"));
        Some((dir, team))
    });
}

#[test]
fn every_case_on_a_split_store_with_team_servers() {
    split_cases(|| {
        let db = postgres_support::database()?;
        let team: Box<dyn MemoryStore> = Box::new(db.open());
        Some((db, team))
    });
    split_cases(|| {
        let db = libsql_support::Database::libsql()?;
        let team = db.open();
        Some((db, team))
    });
    split_cases(|| {
        let db = mongo_support::TestDb::new()?;
        let team: Box<dyn MemoryStore> = Box::new(db.connect());
        Some((db, team))
    });
}

/// A split store merges pages of both stores past the size of one store page.
#[test]
fn a_split_store_merges_pages_past_one_store_page() {
    let team_dir = TempDir::new().expect("temp dir");
    let local_dir = TempDir::new().expect("temp dir");
    let open = || {
        SplitStore::new(
            Box::new(SqliteStore::open(&team_dir.path().join("team.sqlite3")).expect("team")),
            Box::new(SqliteStore::open(&local_dir.path().join("memory.sqlite3")).expect("local")),
            Scope::Global,
        )
    };
    let mut split = open();

    for index in 0..230 {
        split
            .remember(&repo(REPO), &draft(&format!("Repo {index:03}"), "Body", NoteKind::Note))
            .expect("repository note");
        split
            .remember(
                &Target::Global,
                &draft(&format!("Global {index:03}"), "Body", NoteKind::Note),
            )
            .expect("global note");
    }

    let by_title = NoteListQuery {
        sort: NoteSort::Title,
        descending: false,
        ..listing(both(REPO))
    };

    let mut seen: Vec<String> = Vec::new();

    for offset in (0..460).step_by(200) {
        let page = split
            .list_notes(&NoteListQuery {
                offset,
                ..by_title.clone()
            })
            .expect("page");
        assert_eq!(page.total, 460);
        seen.extend(page.rows.into_iter().map(|row| row.title));
    }

    let mut expected: Vec<String> = (0..230)
        .flat_map(|index| [format!("Global {index:03}"), format!("Repo {index:03}")])
        .collect();
    expected.sort_by_key(|title| title.to_lowercase());
    assert_eq!(seen, expected);
}

/// A split store sorts by place: the notes kept on this computer first ascending, whichever scope
/// that is, by id within each store, and pages follow it.
#[test]
fn a_split_store_sorts_by_place_local_first() {
    for local_scope in [Scope::Repo, Scope::Global] {
        let team_dir = TempDir::new().expect("temp dir");
        let local_dir = TempDir::new().expect("temp dir");
        let mut split = SplitStore::new(
            Box::new(SqliteStore::open(&team_dir.path().join("team.sqlite3")).expect("team")),
            Box::new(SqliteStore::open(&local_dir.path().join("memory.sqlite3")).expect("local")),
            local_scope,
        );

        for index in 0..3 {
            split
                .remember(&repo(REPO), &draft(&format!("Repo {index}"), "Body", NoteKind::Note))
                .expect("repository note");
            split
                .remember(
                    &Target::Global,
                    &draft(&format!("Global {index}"), "Body", NoteKind::Note),
                )
                .expect("global note");
        }

        let mut by_place = |descending: bool, offset: usize, limit: usize| {
            split
                .list_notes(&NoteListQuery {
                    sort: NoteSort::Place,
                    descending,
                    offset,
                    limit,
                    ..listing(both(REPO))
                })
                .expect("page")
                .rows
                .into_iter()
                .map(|row| row.title)
                .collect::<Vec<_>>()
        };

        let (local, team) = match local_scope {
            Scope::Repo => ("Repo", "Global"),
            Scope::Global => ("Global", "Repo"),
        };
        let expected: Vec<String> = [local, team]
            .iter()
            .flat_map(|prefix| (0..3).map(move |index| format!("{prefix} {index}")))
            .collect();

        assert_eq!(by_place(false, 0, 200), expected, "{local_scope:?} kept locally");
        assert_eq!(by_place(false, 2, 2), expected[2..4], "a page follows the order");

        let mut reversed = expected.clone();
        reversed.reverse();
        assert_eq!(by_place(true, 0, 200), reversed);
    }
}

/// Saves three notes and sets their times of the latest write to 300, 100 and 200 with `set`
/// (`(store id, time)`); the list is then newest first by those times.
fn updated_times_order<S: MemoryStore + ?Sized>(store: &mut S, set: &mut dyn FnMut(i64, i64)) {
    let first = store
        .remember(&repo(REPO), &draft("First", "Body", NoteKind::Note))
        .expect("first")
        .id;
    let second = store
        .remember(&Target::Global, &draft("Second", "Body", NoteKind::Note))
        .expect("second")
        .id;
    let third = store
        .remember(&repo(REPO), &draft("Third", "Body", NoteKind::Note))
        .expect("third")
        .id;

    set(first, 300);
    set(second, 100);
    set(third, 200);

    let newest = store.list_notes(&listing(both(REPO))).expect("list");
    let titles: Vec<&str> = newest.rows.iter().map(|row| row.title.as_str()).collect();
    assert_eq!(titles, ["First", "Third", "Second"]);
    assert_eq!(newest.rows[0].updated_at, 300);

    let oldest = store
        .list_notes(&NoteListQuery {
            descending: false,
            ..listing(both(REPO))
        })
        .expect("list");
    let titles: Vec<&str> = oldest.rows.iter().map(|row| row.title.as_str()).collect();
    assert_eq!(titles, ["Second", "Third", "First"]);
}

fn sqlite_dialect_times(db: &libsql_support::Database) {
    let mut store = db.open();
    updated_times_order(store.as_mut(), &mut |id, at| {
        db.batch(&format!("UPDATE notes SET updated_at = {at} WHERE id = {id}"));
    });
}

store_cases!(sqlite_dialect_times);

#[test]
fn postgres_times() {
    let Some(db) = postgres_support::database() else { return };
    let mut store = db.open();
    let mut admin = db.admin();

    updated_times_order(&mut store, &mut |id, at| {
        admin
            .execute("UPDATE notes SET updated_at = $2 WHERE id = $1", &[&id, &at])
            .expect("set the time");
    });
}

#[test]
fn mongo_times() {
    let Some(db) = mongo_support::TestDb::new() else { return };
    let mut store = db.connect();
    let notes = db.raw().collection::<Document>("notes");

    updated_times_order(&mut store, &mut |id, at| {
        notes
            .update_one(doc! { "_id": id }, doc! { "$set": { "updated_at": at } })
            .run()
            .expect("set the time");
    });
}
