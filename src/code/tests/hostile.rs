//! A repository is untrusted input: hostile files, a stuck or dead neighbour and a slow refresh must end in a
//! bounded answer, not in an out-of-memory kill, an endless wait or a wedged database.

use std::time::{Duration, Instant};

use super::*;

/// `count` lines of `line` (one definition that calls itself, many times).
fn lines(line: &str, count: usize) -> String {
    let mut text = String::with_capacity(line.len() * count + count);

    for _ in 0..count {
        text.push_str(line);
        text.push('\n');
    }

    text
}

/// Indexes a repository of one file and returns how long it took, the counts and the edges.
fn index_one(file: &str, text: &str) -> (Duration, Counts, usize) {
    let mut fixture = Fixture::new();

    fixture.write(file, text);

    let root = fixture.root().to_path_buf();
    let started = Instant::now();
    let (graph, _) = fixture.index.graph(&root, &|| false).expect("graph");

    (started.elapsed(), graph.counts(), graph.edges())
}

#[test]
fn eighty_thousand_self_calling_functions_of_one_name_make_no_edges_and_finish_fast() {
    let cases = [
        ("a.rs", "fn x(){x()}"),
        ("a.rs", "impl A { fn x(&self){self.x()} }"),
        ("a.ts", "function x(){x()}"),
        ("a.ts", "class A { x(){this.x()} }"),
        ("a.py", "def x(): x()"),
        ("a.go", "func x(){x()}"),
    ];

    for (file, line) in cases {
        let (took, counts, edges) = index_one(file, &lines(line, 80_000));

        // the graph used to hold every definition-reference pair: 80 000 x 80 000 edges
        assert!(took < Duration::from_secs(60), "{file} {line}: {took:?}");
        assert!(edges <= 64 * counts.refs, "{file} {line}: {edges} edges");
        assert!(counts.symbols <= 20_000, "{file} {line}: {} symbols", counts.symbols);
        assert!(counts.refs <= 100_000, "{file} {line}: {} refs", counts.refs);
        assert_eq!(
            counts.resolved, 0,
            "{file} {line}: an ambiguous name resolves to nothing"
        );
    }
}

#[test]
fn a_name_defined_in_thousands_of_files_is_too_ambiguous_to_link() {
    let mut fixture = Fixture::new();

    for n in 0..1_500 {
        fixture.write(&format!("d{n}/mod.rs"), "pub fn shared() {}\n");
    }

    fixture.write("user.rs", &lines("fn caller() { shared(); }", 5_000));

    let root = fixture.root().to_path_buf();
    let (graph, _) = fixture.index.graph(&root, &|| false).expect("graph");

    assert_eq!(graph.counts().resolved, 0);
    assert_eq!(graph.edges(), 0);
}

#[test]
fn the_definitions_a_file_may_hold_are_capped_and_the_cut_is_counted() {
    let mut fixture = Fixture::new();

    fixture.write(
        "big.rs",
        &(0..30_000).map(|n| format!("fn f{n}(){{}}\n")).collect::<String>(),
    );

    let refresh = fixture.refresh();

    assert_eq!(refresh.skipped.truncated, 1);
    assert_eq!(fixture.counts().symbols, 20_000);
}

#[test]
fn a_call_between_two_definitions_still_resolves() {
    let mut fixture = Fixture::new();

    fixture.write("a.rs", "pub fn one() { two(); }\npub fn two() {}\n");

    let counts = fixture.counts();

    assert_eq!(counts.resolved, 1);
}

/// An observer that spends the time budget of the build it watches.
struct Sleeper(Duration);

impl BuildObserver for Sleeper {
    fn on_start(&mut self, _id: i64, _kind: BuildKind, _total: usize) {
        std::thread::sleep(self.0);
    }

    fn on_progress(&mut self, _done: usize) {}

    fn on_finish(&mut self, _outcome: Result<(), &str>) {}
}

#[test]
fn a_refresh_past_its_time_budget_fails_and_the_next_call_goes_on() {
    let mut fixture = Fixture::new();

    for n in 0..1_200 {
        fixture.write(&format!("src/f{n}.rs"), &format!("pub fn g{n}_fn() {{}}\n"));
    }

    let root = fixture.root().to_path_buf();

    fixture.index.set_budget(Duration::from_secs(1));

    // the build starts within the budget and then runs past it
    let outcome = fixture
        .index
        .graph_observed(&root, &|| false, &mut Sleeper(Duration::from_millis(1_300)));

    assert_eq!(outcome.err().as_deref(), Some(TOO_SLOW));

    let status: String = fixture
        .index
        .store
        .conn()
        .query_row("SELECT status || ':' || error FROM builds", [], |row| row.get(0))
        .expect("the build row");

    assert_eq!(status, "error:indexing took too long");

    fixture.index.set_budget(pace::BUDGET);

    assert_eq!(fixture.counts().files, 1_200);
}

#[test]
fn the_walk_alone_is_held_to_the_time_budget() {
    let mut fixture = Fixture::new();

    // nothing to index and nothing to read: only the walk and its ignore files take time
    for n in 0..30 {
        fixture.write(&format!("d{n}/notes.md"), "text");
        fixture.write(&format!("d{n}/.gitignore"), &"# comment\n".repeat(20_000));
    }

    let root = fixture.root().to_path_buf();

    fixture.index.set_budget(Duration::ZERO);

    let outcome = fixture.index.graph(&root, &|| false);

    assert_eq!(outcome.err().as_deref(), Some(TOO_SLOW));

    fixture.index.set_budget(pace::BUDGET);

    assert_eq!(fixture.counts().files, 0);
}

#[test]
fn the_second_session_serves_the_last_committed_graph_while_the_first_holds_the_lock() {
    let mut fixture = Fixture::new();

    fixture.write("src/lib.rs", "pub fn old_name() {}\n");
    fixture.refresh();
    fixture.write("src/lib.rs", "pub fn new_name() {}\n");

    let path = fixture._data.path().join("code.sqlite3");
    let held = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (held_there, release_there) = (held.clone(), release.clone());

    // another session in the middle of a long build: it has the write lock and does not give it back
    let other = std::thread::spawn(move || {
        let conn = rusqlite::Connection::open(path).expect("open");

        conn.execute_batch("BEGIN IMMEDIATE").expect("lock");
        held_there.store(true, std::sync::atomic::Ordering::SeqCst);

        while !release_there.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }

        conn.execute_batch("ROLLBACK").expect("release");
    });

    while !held.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(5));
    }

    fixture.index.set_patience(Duration::from_millis(300));

    let root = fixture.root().to_path_buf();
    let started = Instant::now();
    let (graph, refresh) = fixture.index.graph(&root, &|| false).expect("served, not failed");
    let served = graph.symbol("old_name", 5);

    // it used to wait for the whole busy timeout (a minute) and then fail with "database is locked"
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert!(refresh.stale);
    assert!(served.contains("old_name"), "{served}");

    release.store(true, std::sync::atomic::Ordering::SeqCst);
    other.join().expect("the other session");

    let refresh = fixture.refresh();

    assert!(!refresh.stale);
    assert_eq!(refresh.reparsed, 1);
    assert!(fixture.symbol("new_name").contains("src/lib.rs:1"));
}

#[test]
fn a_build_that_died_is_ended_as_an_error_and_its_status_file_stops_saying_building() {
    let mut fixture = Fixture::new();

    fixture.write("src/lib.rs", "pub fn alpha() {}\n");
    fixture.refresh();

    // the corpse of an earlier process: a `running` row and a `building` status file, both long untouched
    let key = fixture.root().to_str().expect("utf-8").to_string();
    let repo = fixture.index.store.known(&key).expect("known").expect("repo").repo;

    fixture
        .index
        .store
        .conn()
        .execute(
            "INSERT INTO builds(repo_id, kind, status, started_at, files_total, files_done) VALUES (?1, 'full', 'running', ?2, 10, 3)",
            rusqlite::params![repo, store::now() - 10_000],
        )
        .expect("dead build");

    let data = fixture._data.path().to_path_buf();
    let mut status = status::StatusFile::new(&data, fixture.root());
    let path = status.path();

    std::fs::create_dir_all(path.parent().expect("folder")).expect("folder");
    std::fs::write(
        &path,
        r#"{"version":1,"buildId":1,"root":"x","repoName":"x","kind":"full","phase":"building","filesTotal":10,"filesDone":3,"startedAt":1,"finishedAt":null,"error":null}"#,
    )
    .expect("status");

    let old = std::time::SystemTime::now() - Duration::from_secs(3_600);

    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("status file")
        .set_modified(old)
        .expect("age it");

    // any change makes the next refresh take the write lock, and that is when the corpse is found
    fixture.write("src/lib.rs", "pub fn beta() {}\n");

    let root = fixture.root().to_path_buf();

    fixture
        .index
        .graph_observed(&root, &|| false, &mut status)
        .expect("graph");

    let rows: String = fixture
        .index
        .store
        .conn()
        .query_row(
            "SELECT group_concat(status || ':' || ifnull(error, ''), ',') FROM builds",
            [],
            |row| row.get(0),
        )
        .expect("rows");
    let document: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).expect("file")).expect("json");

    assert!(rows.contains("error:the build was interrupted"), "{rows}");
    assert!(!rows.contains("running"), "{rows}");
    assert_eq!(document["phase"], "error");
    assert_eq!(document["error"], "the build was interrupted");
}

#[test]
fn a_dead_build_of_another_repository_is_ended_and_its_status_file_stops_saying_building() {
    let mut fixture = Fixture::new();
    let other = tempfile::tempdir().expect("other repo");
    let other_root = other.path().to_path_buf();

    fixture.write("src/lib.rs", "pub fn alpha() {}\n");
    fixture.refresh();
    fs::write(other_root.join("lib.rs"), "pub fn gamma() {}\n").expect("write");
    fixture.index.graph(&other_root, &|| false).expect("other graph");

    // the corpse of a build of the other repository: a `running` row and an untouched `building` file
    let other_key = other_root.to_str().expect("utf-8").to_string();
    let other_repo = fixture
        .index
        .store
        .known(&other_key)
        .expect("known")
        .expect("repo")
        .repo;

    fixture
        .index
        .store
        .conn()
        .execute(
            "INSERT INTO builds(repo_id, kind, status, started_at, files_total, files_done) VALUES (?1, 'full', 'running', ?2, 10, 3)",
            rusqlite::params![other_repo, store::now() - 10_000],
        )
        .expect("dead build");

    let data = fixture._data.path().to_path_buf();
    let path = status::path_of(&data, &other_root);

    fs::create_dir_all(path.parent().expect("folder")).expect("folder");
    fs::write(
        &path,
        r#"{"version":1,"buildId":1,"root":"x","repoName":"x","kind":"full","phase":"building","filesTotal":10,"filesDone":3,"startedAt":1,"finishedAt":null,"error":null}"#,
    )
    .expect("status");
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("status file")
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(3_600))
        .expect("age it");

    // a refresh of this repository takes the write lock and finds both corpses
    fixture.write("src/lib.rs", "pub fn beta() {}\n");

    let root = fixture.root().to_path_buf();
    let mut status = status::StatusFile::new(&data, &root);

    fixture
        .index
        .graph_observed(&root, &|| false, &mut status)
        .expect("graph");

    let running: i64 = fixture
        .index
        .store
        .conn()
        .query_row("SELECT count(*) FROM builds WHERE status = 'running'", [], |row| {
            row.get(0)
        })
        .expect("rows");
    let document: serde_json::Value = serde_json::from_slice(&fs::read(&path).expect("file")).expect("json");

    assert_eq!(running, 0);
    assert_eq!(document["phase"], "error");
    assert_eq!(document["error"], "the build was interrupted");
}

#[test]
fn a_repository_nobody_used_for_a_month_is_dropped_when_another_one_is_built() {
    let mut fixture = Fixture::new();

    fixture.write("src/lib.rs", "pub fn alpha() {}\n");
    fixture.refresh();

    let key = fixture.root().to_str().expect("utf-8").to_string();

    fixture
        .index
        .store
        .conn()
        .execute(
            "UPDATE repos SET used_at = ?1 WHERE root = ?2",
            rusqlite::params![store::now() - store::UNUSED_SECONDS - 10, key],
        )
        .expect("age it");

    let other = tempfile::tempdir().expect("other repo");

    fs::write(other.path().join("lib.rs"), "pub fn beta() {}\n").expect("write");
    fixture
        .index
        .graph(other.path(), &|| false)
        .expect("the other repository");

    assert!(fixture.index.store.known(&key).expect("known").is_none());
    assert!(
        fixture
            .index
            .store
            .known(other.path().to_str().expect("utf-8"))
            .expect("known")
            .is_some()
    );

    let files: i64 = fixture
        .index
        .store
        .conn()
        .query_row("SELECT count(*) FROM files WHERE path = 'src/lib.rs'", [], |row| {
            row.get(0)
        })
        .expect("count");

    assert_eq!(files, 0, "the rows of the dropped repository are gone too");
}

#[test]
fn a_repository_in_use_is_kept() {
    let mut fixture = Fixture::new();

    fixture.write("src/lib.rs", "pub fn alpha() {}\n");
    fixture.refresh();

    let other = tempfile::tempdir().expect("other repo");

    fs::write(other.path().join("lib.rs"), "pub fn beta() {}\n").expect("write");
    fixture
        .index
        .graph(other.path(), &|| false)
        .expect("the other repository");

    let key = fixture.root().to_str().expect("utf-8");

    assert!(fixture.index.store.known(key).expect("known").is_some());
}

#[test]
fn the_path_from_a_symbol_to_itself_is_zero_hops() {
    let mut fixture = Fixture::new();

    fixture.write("src/lib.rs", "pub fn alpha() { beta(); }\npub fn beta() { alpha(); }\n");

    let same = fixture.chain("alpha", "alpha", 4);

    assert!(same.starts_with("`alpha` → `alpha`: 0 hops"), "{same}");
    assert!(same.contains("src/lib.rs:1"), "{same}");
    assert!(!same.contains("no chain"), "{same}");
}

#[test]
fn impact_does_not_count_the_file_that_defines_the_symbol_as_affected() {
    let mut fixture = Fixture::new();

    fixture.write("src/def.rs", "pub fn target() {}\n");
    fixture.write("src/user.rs", "pub fn user() { target(); }\n");

    let impact = fixture.impact("target", 2);

    assert!(impact.contains("1 production file(s)"), "{impact}");
    assert!(impact.contains("d0 src/def.rs  [defined here: target]"), "{impact}");
    assert!(impact.contains("d1 src/user.rs"), "{impact}");
}

#[test]
fn a_file_that_defines_and_calls_the_symbol_is_affected_by_its_callers() {
    let mut fixture = Fixture::new();

    fixture.write("src/both.rs", "pub fn target() {}\npub fn caller() { target(); }\n");

    let impact = fixture.impact("target", 2);

    assert!(impact.contains("1 production file(s)"), "{impact}");
    assert!(impact.contains("d0 src/both.rs  [defined here: target]"), "{impact}");
    assert!(impact.contains("d1 src/both.rs  [caller → target]"), "{impact}");
}

#[test]
fn a_method_defined_on_one_type_name_in_over_a_thousand_places_is_ambiguous_not_cut_to_the_first_ones() {
    let mut fixture = Fixture::new();

    // 1 016 Rust definitions come first, then 100 TypeScript ones: the call below could only mean the latter
    fixture.write("a.rs", &"impl A { fn x(&self) {} }\n".repeat(1_016));
    fixture.write("b.ts", &"class A { x() {} }\n".repeat(100));
    fixture.write("c.ts", "import { A } from './b';\nfunction f(a: A) { a.x(); }\n");

    let root = fixture.root().to_path_buf();
    let (graph, _) = fixture.index.graph(&root, &|| false).expect("graph");

    // the 8 definitions inside the first 1 024 used to be taken for all of them
    assert_eq!(graph.edges(), 0);
}

#[test]
fn a_method_defined_in_a_few_places_still_resolves() {
    let mut fixture = Fixture::new();

    fixture.write("a.rs", &"impl A { fn x(&self) {} }\n".repeat(3));
    fixture.write("b.ts", &"class A { x() {} }\n".repeat(3));
    fixture.write("c.ts", "import { A } from './b';\nfunction f(a: A) { a.x(); }\n");

    let root = fixture.root().to_path_buf();
    let (graph, _) = fixture.index.graph(&root, &|| false).expect("graph");

    assert!(graph.edges() > 0);
}

/// How many rows `table` holds.
fn rows(fixture: &Fixture, table: &str) -> i64 {
    fixture
        .index
        .store
        .conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0))
        .expect("count")
}

#[test]
fn a_repository_past_the_caps_stops_writing_at_the_cap_fails_fast_and_retries_once_it_shrank() {
    let mut fixture = Fixture::new();

    // 600 files of one definition each: the first batch of 500 fits, the second would pass the cap
    for n in 0..600 {
        fixture.write(&format!("src/f{n:03}.rs"), &format!("pub fn g{n:03}() {{}}\n"));
    }

    fixture.index.caps = store::Caps {
        syms: 550,
        refs: 10_000,
        imports: 10_000,
        ..store::Caps::default()
    };

    let root = fixture.root().to_path_buf();

    assert_eq!(fixture.index.graph(&root, &|| false).err().as_deref(), Some(TOO_LARGE));

    // the database never held more than the cap (it used to take every file: 600 rows)
    let held = rows(&fixture, "symbols");
    let files = rows(&fixture, "files");
    let builds = rows(&fixture, "builds");

    assert!(held <= 550, "{held}");
    assert_eq!(held, 500);

    // the next calls do nothing: no build, no row
    for _ in 0..2 {
        assert_eq!(fixture.index.graph(&root, &|| false).err().as_deref(), Some(TOO_LARGE));
        assert_eq!(rows(&fixture, "symbols"), held);
        assert_eq!(rows(&fixture, "files"), files);
        assert_eq!(rows(&fixture, "builds"), builds);
    }

    // the repository shrinks to what fits: the index goes on from where it stopped
    for n in 550..600 {
        fs::remove_file(fixture.path(&format!("src/f{n:03}.rs"))).expect("remove");
    }

    let counts = fixture.counts();

    assert_eq!(counts.files, 550);
    assert_eq!(counts.symbols, 550);

    // and it is a normal index again
    assert!(fixture.symbol("g010").contains("src/f010.rs:1"));
}

#[test]
fn the_caps_hold_for_references_and_imports_too() {
    let mut fixture = Fixture::new();

    fixture.write(
        "a.rs",
        &format!("use crate::b::one;\nfn f() {{ {} }}\n", "g();".repeat(30)),
    );
    fixture.index.caps = store::Caps {
        syms: 100,
        refs: 20,
        imports: 100,
        ..store::Caps::default()
    };

    let root = fixture.root().to_path_buf();

    assert_eq!(fixture.index.graph(&root, &|| false).err().as_deref(), Some(TOO_LARGE));
    assert_eq!(rows(&fixture, "refs"), 0);

    fixture.write("a.rs", "use crate::b::one;\nuse crate::b::two;\nfn f() {}\n");
    fixture.index.caps = store::Caps {
        syms: 100,
        refs: 100,
        imports: 1,
        ..store::Caps::default()
    };

    assert_eq!(fixture.index.graph(&root, &|| false).err().as_deref(), Some(TOO_LARGE));
    assert_eq!(rows(&fixture, "imports"), 0);
}

#[test]
fn a_repository_past_the_binding_cap_is_marked_too_large_and_writes_no_more() {
    let mut fixture = Fixture::new();

    // many files, each with a few assignments that bind a name: the bindings alone pass the cap
    for n in 0..30 {
        fixture.write(&format!("m{n}.py"), &format!("def f():\n{}", " a=b\n".repeat(10)));
    }

    fixture.index.caps = store::Caps {
        binds: 100,
        ..store::Caps::default()
    };

    let root = fixture.root().to_path_buf();

    assert_eq!(fixture.index.graph(&root, &|| false).err().as_deref(), Some(TOO_LARGE));

    let held = rows(&fixture, "binds");
    let builds = rows(&fixture, "builds");

    assert!(held <= 100, "{held}");

    // the repository is marked, so the next calls read nothing and write nothing
    for _ in 0..2 {
        assert_eq!(fixture.index.graph(&root, &|| false).err().as_deref(), Some(TOO_LARGE));
        assert_eq!(rows(&fixture, "binds"), held);
        assert_eq!(rows(&fixture, "builds"), builds);
    }
}

#[test]
fn the_bindings_of_a_file_that_goes_are_taken_off_the_repository_total() {
    let mut fixture = Fixture::new();

    fixture.write("a.py", &format!("def f():\n{}", " a=b\n".repeat(10)));
    fixture.write("b.py", &format!("def g():\n{}", " a=b\n".repeat(10)));

    let root = fixture.root().to_path_buf();
    fixture.index.graph(&root, &|| false).expect("graph");
    fs::remove_file(fixture.path("a.py")).expect("remove");
    fixture.index.graph(&root, &|| false).expect("graph");

    let counted: i64 = fixture
        .index
        .store
        .conn()
        .query_row("SELECT binds FROM repos", [], |row| row.get(0))
        .expect("total");

    assert_eq!(counted, rows(&fixture, "binds"));
    assert!(counted > 0);
}

#[test]
fn a_string_that_names_nothing_defined_is_stored_but_not_loaded_into_the_graph() {
    let key = ["sk_", "live_", "4eC39HqLyjWDarjtT1zdp7dc"].concat();
    let mut named = Fixture::new();

    named.write(
        "a.rs",
        "pub fn run() { send(\"never_defined_name\"); send(\"other_unknown_name\"); }\n",
    );
    named.write(
        "b.rs",
        &format!("pub fn keyed() {{ send(\"{key}\"); }}\npub fn defined_handler() {{}}\n"),
    );
    named.write("c.rs", "pub fn caller() { send(\"defined_handler\"); }\n");

    // the strings that name nothing are in the index file, the one that names a definition links to it
    let _ = named.counts();
    let stored: i64 = named
        .index
        .store
        .conn()
        .query_row("SELECT COUNT(*) FROM refs WHERE kind = 'str'", [], |row| row.get(0))
        .expect("count");
    let texts: Vec<String> = named
        .index
        .store
        .conn()
        .prepare("SELECT name FROM refs WHERE kind = 'str'")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .map(|row| row.expect("row"))
        .collect();

    assert!(texts.contains(&"never_defined_name".to_owned()), "{texts:?}");
    assert!(
        !texts.contains(&key),
        "a credential written like a name is not stored: {texts:?}"
    );
    assert_eq!(stored, 3, "{texts:?}");

    // in the graph only what can resolve is held: the four calls of `send` and the one string that names a
    // definition (the two other strings are left out)
    assert_eq!(named.counts().refs, 5);
}
