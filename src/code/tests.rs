//! End-to-end behaviour of the code index over small repositories written to temporary folders.

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

use super::*;

mod hostile;

/// A repository folder, an index over a database folder of its own, and shortcuts to ask it things.
struct Fixture {
    repo: tempfile::TempDir,
    _data: tempfile::TempDir,
    index: CodeIndex,
}

impl Fixture {
    fn new() -> Self {
        let data = tempfile::tempdir().expect("data dir");
        let index = CodeIndex::open(&data.path().join("code.sqlite3")).expect("open");

        Self {
            repo: tempfile::tempdir().expect("repo dir"),
            _data: data,
            index,
        }
    }

    fn root(&self) -> &Path {
        self.repo.path()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.path(rel);

        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, content).expect("write");
    }

    fn refresh(&mut self) -> Refresh {
        let root = self.repo.path().to_path_buf();

        self.index.graph(&root, &|| false).expect("graph").1
    }

    fn counts(&mut self) -> Counts {
        let root = self.repo.path().to_path_buf();

        self.index.graph(&root, &|| false).expect("graph").0.counts()
    }

    fn symbol(&mut self, query: &str) -> String {
        let root = self.repo.path().to_path_buf();

        self.index.graph(&root, &|| false).expect("graph").0.symbol(query, 20)
    }

    fn impact(&mut self, query: &str, depth: usize) -> String {
        let root = self.repo.path().to_path_buf();

        self.index
            .graph(&root, &|| false)
            .expect("graph")
            .0
            .impact(query, depth)
    }

    fn chain(&mut self, from: &str, to: &str, max: usize) -> String {
        let root = self.repo.path().to_path_buf();

        self.index.graph(&root, &|| false).expect("graph").0.path(from, to, max)
    }

    fn generation(&self) -> i64 {
        let key = self.repo.path().to_str().expect("utf-8");

        self.index.store.known(key).expect("known").expect("repo").generation
    }
}

#[derive(Default)]
struct Recorder {
    events: Vec<String>,
}

impl BuildObserver for Recorder {
    fn on_start(&mut self, id: i64, kind: BuildKind, total: usize) {
        self.events.push(format!("start {id} {} {total}", kind.as_str()));
    }

    fn on_progress(&mut self, done: usize) {
        self.events.push(format!("progress {done}"));
    }

    fn on_finish(&mut self, outcome: Result<(), &str>) {
        self.events.push(match outcome {
            Ok(()) => "finish ok".to_string(),
            Err(message) => format!("finish error {message}"),
        });
    }
}

#[test]
fn an_empty_project_is_an_empty_graph_not_an_error() {
    let mut fixture = Fixture::new();
    let refresh = fixture.refresh();

    assert_eq!((refresh.checked, refresh.reparsed, refresh.removed), (0, 0, 0));
    assert!(refresh.build.is_none());
    assert_eq!(fixture.counts().files, 0);
    assert!(
        fixture
            .symbol("anything")
            .starts_with("No symbol, file or command matches `anything`")
    );

    let root = fixture.repo.path().to_path_buf();
    let (graph, _) = fixture.index.graph(&root, &|| false).expect("graph");

    assert!(graph.map(700, None).contains("No source files"));
    assert!(
        graph
            .impact("anything", 2)
            .contains("No symbol, file or command matches")
    );
}

#[test]
fn a_folder_without_git_is_indexed_and_the_first_index_is_a_full_build() {
    let mut fixture = Fixture::new();
    fixture.write("src/lib.rs", "pub fn alpha() { beta(); }\nfn beta() {}\n");

    let refresh = fixture.refresh();

    assert!(!fixture.path(".git").exists());
    assert_eq!(refresh.reparsed, 1);
    assert_eq!(refresh.build.map(|(_, kind)| kind), Some(BuildKind::Full));
    assert!(fixture.symbol("beta").contains("src/lib.rs:1 alpha"));
}

#[test]
fn a_file_over_one_megabyte_is_not_indexed() {
    let mut fixture = Fixture::new();
    fixture.write("small.rs", "pub fn small_one() {}\n");
    fixture.write(
        "big.rs",
        &format!("pub fn huge_one() {{}}\n// {}\n", "x".repeat(1_000_100)),
    );

    fixture.refresh();

    assert!(fixture.symbol("small_one").starts_with("fn small_one"));
    assert!(
        fixture
            .symbol("huge_one")
            .starts_with("No symbol, file or command matches")
    );
}

#[test]
fn a_deleted_file_leaves_the_index_on_the_next_refresh() {
    let mut fixture = Fixture::new();
    fixture.write("a.rs", "pub fn only_in_a() {}\n");
    fixture.write("b.rs", "pub fn only_in_b() {}\n");
    fixture.refresh();
    let before = fixture.generation();

    fs::remove_file(fixture.path("a.rs")).expect("remove");
    let refresh = fixture.refresh();

    assert_eq!(refresh.removed, 1);
    assert!(fixture.generation() > before);
    let gone = fixture.symbol("only_in_a");

    assert!(gone.starts_with("no definition named `only_in_a`"), "{gone}");
    assert!(fixture.symbol("only_in_b").starts_with("fn only_in_b"));
}

#[cfg(unix)]
#[test]
fn an_unreadable_file_is_skipped_and_stays_cheap() {
    use std::os::unix::fs::PermissionsExt;

    let mut fixture = Fixture::new();
    fixture.write("ok.rs", "pub fn readable_one() {}\n");
    fixture.write("locked.rs", "pub fn hidden_one() {}\n");
    fs::set_permissions(fixture.path("locked.rs"), fs::Permissions::from_mode(0o000)).expect("chmod");

    let refresh = fixture.refresh();
    let readable = fs::File::open(fixture.path("locked.rs")).is_ok();

    if !readable {
        assert_eq!(refresh.skipped.unreadable, 1);
        assert!(
            fixture
                .symbol("hidden_one")
                .starts_with("No symbol, file or command matches")
        );
    }

    assert!(fixture.symbol("readable_one").starts_with("fn readable_one"));

    let again = fixture.refresh();

    assert_eq!(again.reparsed, 0);
    assert!(!again.graph_rebuilt);
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed_by_the_index() {
    use std::os::unix::fs::symlink;

    let outside = tempfile::tempdir().expect("outside");
    fs::write(outside.path().join("secret.rs"), "pub fn leaked_secret() {}\n").expect("write");

    let mut fixture = Fixture::new();
    fixture.write("src/real.rs", "pub fn real_one() {}\n");
    symlink(fixture.root(), fixture.path("src/loop")).expect("loop");
    symlink(outside.path().join("secret.rs"), fixture.path("src/linked.rs")).expect("file link");
    symlink(outside.path(), fixture.path("outside_dir")).expect("dir link");

    fixture.refresh();

    assert_eq!(fixture.counts().files, 1);
    assert!(
        fixture
            .symbol("leaked_secret")
            .starts_with("No symbol, file or command matches")
    );
}

#[test]
fn a_component_with_multibyte_text_before_the_script_end_is_indexed() {
    let mut fixture = Fixture::new();
    fixture.write(
        "ui/Panel.svelte",
        "<script lang=\"ts\">\n// zażółć gęślą jaźń \u{1F980} 日本語\nfunction łaskawy_helper() { return invoke('remote_command'); }\n</script>\n\n<Child />\n<p>żółć</p>\n",
    );
    fixture.write("ui/Child.svelte", "<div>child</div>\n");

    fixture.refresh();

    assert!(fixture.symbol("Panel").contains("component Panel"));
    assert!(fixture.symbol("Child").contains("ui/Panel.svelte:6 Panel"));
}

#[test]
fn malformed_sources_do_not_stop_the_index() {
    let mut fixture = Fixture::new();
    let deep = format!("{}1{}", "(".repeat(30_000), ")".repeat(30_000));

    fixture.write("src/good.rs", "pub fn still_fine() {}\n");
    fixture.write("src/bad.rs", "fn ((( { struct }}} impl<<< let = ;;;");
    fixture.write("src/deep.rs", &format!("fn deep_one() {{ let _ = {deep}; }}\n"));
    fixture.write("web/bad.ts", "export function ( { class }} <<< => =>");
    fixture.write("web/bad.tsx", "const a = <div><b></div>;");
    fixture.write("web/deep.js", &format!("const x = {deep};\n"));
    fixture.write("web/Bad.vue", "<script>function (((</script><Bad");
    fixture.write("srv/bad.go", "package\nfunc (((( {{");
    fixture.write("srv/binary.go", "\u{0}\u{1}\u{2}");
    fs::write(fixture.path("srv/latin1.go"), [0xe9_u8, 0xe8, 0xff]).expect("write");

    let refresh = fixture.refresh();

    assert_eq!(refresh.skipped.not_utf8, 1);
    assert!(fixture.symbol("still_fine").starts_with("fn still_fine"));
}

#[test]
fn a_second_refresh_without_changes_reads_nothing_and_reuses_the_graph() {
    let mut fixture = Fixture::new();
    fixture.write("src/lib.rs", "pub fn one() { two(); }\nfn two() {}\n");
    fixture.write("web/app.ts", "export function three() {}\n");
    let root = fixture.repo.path().to_path_buf();

    let (first, refresh) = fixture.index.graph(&root, &|| false).expect("graph");
    let first_graph = std::ptr::from_ref(first) as usize;

    assert!(refresh.graph_rebuilt);
    assert_eq!(refresh.reparsed, 2);

    let generation = fixture.generation();
    let (second, refresh) = fixture.index.graph(&root, &|| false).expect("graph");

    assert_eq!(std::ptr::from_ref(second) as usize, first_graph);
    assert_eq!((refresh.checked, refresh.reparsed, refresh.removed), (2, 0, 0));
    assert!(!refresh.graph_rebuilt);
    assert!(refresh.build.is_none());
    assert_eq!(fixture.generation(), generation);
}

#[test]
fn an_edit_is_picked_up_and_a_touch_without_a_content_change_is_not() {
    let mut fixture = Fixture::new();
    fixture.write("src/lib.rs", "pub fn before_edit() {}\n");
    fixture.refresh();
    let generation = fixture.generation();

    // same bytes, newer modification time
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    fs::File::options()
        .write(true)
        .open(fixture.path("src/lib.rs"))
        .and_then(|file| file.set_modified(later))
        .expect("touch");
    let refresh = fixture.refresh();

    assert_eq!(refresh.reparsed, 0);
    assert_eq!(fixture.generation(), generation);
    assert!(!refresh.graph_rebuilt);

    fixture.write("src/lib.rs", "pub fn after_edit() {}\n// more\n");
    let refresh = fixture.refresh();

    assert_eq!(refresh.reparsed, 1);
    assert!(refresh.graph_rebuilt);
    assert!(fixture.generation() > generation);
    assert!(fixture.symbol("after_edit").starts_with("fn after_edit"));
    assert!(
        fixture
            .symbol("before_edit")
            .starts_with("No symbol, file or command matches")
    );
}

#[test]
fn a_package_file_change_moves_the_generation_only_when_the_packages_change() {
    let mut fixture = Fixture::new();
    fixture.write("Cargo.toml", "[package]\nname = \"first\"\n");
    fixture.write("src/lib.rs", "pub fn lib_fn() {}\n");
    fixture.refresh();
    let generation = fixture.generation();

    // another dependency, same package name: nothing the resolvers use
    fixture.write(
        "Cargo.toml",
        "[package]\nname = \"first\"\n[dependencies]\nserde = \"1\"\n",
    );
    let refresh = fixture.refresh();

    assert_eq!(refresh.reparsed, 0);
    assert_eq!(fixture.generation(), generation);

    fixture.write(
        "Cargo.toml",
        "[package]\nname = \"renamed\"\n[dependencies]\nserde = \"1\"\n",
    );
    let refresh = fixture.refresh();

    assert_eq!(refresh.reparsed, 0);
    assert!(fixture.generation() > generation);
    assert!(refresh.graph_rebuilt);

    // and the unchanged manifests are not read as changes again
    let steady = fixture.refresh();

    assert!(!steady.graph_rebuilt);
}

#[test]
fn a_tsconfig_alias_resolves_imports_and_follows_extends() {
    let mut fixture = Fixture::new();
    fixture.write(
        "tsconfig.base.json",
        "{ // shared\n \"compilerOptions\": { \"paths\": { \"@app/*\": [\"./src/*\"], }, }, }",
    );
    fixture.write("tsconfig.json", "{ \"extends\": \"./tsconfig.base.json\" }");
    fixture.write(
        "src/main.ts",
        "import { util_fn } from '@app/lib/u';\nexport function run_it() { util_fn(); }\n",
    );
    fixture.write("src/lib/u.ts", "export function util_fn() {}\n");
    fixture.write("src/other/u.ts", "export function util_fn() {}\n");

    let used = fixture.symbol("lib/u::util_fn");
    let decoy = fixture.symbol("other/u::util_fn");

    assert!(used.contains("src/main.ts:2 run_it"), "{used}");
    assert!(decoy.contains("used by 0 site(s)"), "{decoy}");
}

#[test]
fn rust_paths_pick_the_module_and_methods_only_resolve_to_methods() {
    let mut fixture = Fixture::new();
    fixture.write("Cargo.toml", "[package]\nname = \"app\"\n");
    fixture.write(
        "src/main.rs",
        "mod git;\nmod net;\nfn main() { git::diff(); let s = git::S; s.run(); }\n",
    );
    fixture.write(
        "src/git.rs",
        "pub fn diff() {}\npub struct S;\nimpl S { pub fn run(&self) {} }\n",
    );
    fixture.write("src/net.rs", "pub fn diff() {}\npub fn run() {}\n");

    let git = fixture.symbol("git::diff");
    let net = fixture.symbol("net::diff");
    let method = fixture.symbol("S::run");
    let free = fixture.symbol("net::run");

    assert!(git.contains("src/main.rs:3 main"), "{git}");
    assert!(net.contains("used by 0 site(s)"), "{net}");
    assert!(method.contains("src/main.rs:3 main"), "{method}");
    assert!(free.contains("used by 0 site(s)"), "{free}");
}

#[test]
fn cfg_test_code_counts_as_tests() {
    let mut fixture = Fixture::new();
    fixture.write(
        "src/lib.rs",
        "pub fn target_fn() {}\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn checks() { target_fn(); }\n}\n",
    );

    let out = fixture.symbol("target_fn");

    assert!(out.contains("used by 0 site(s) + 1 test(s)"), "{out}");
}

#[test]
fn go_packages_resolve_through_the_module_path() {
    let mut fixture = Fixture::new();
    fixture.write("go.mod", "module example.com/app\n\ngo 1.22\n");
    fixture.write(
        "cmd/main.go",
        "package main\n\nimport \"example.com/app/git\"\n\nfunc main() { git.Diff() }\n",
    );
    fixture.write("git/git.go", "package git\n\nfunc Diff() {}\n");
    fixture.write("other/other.go", "package other\n\nfunc Diff() {}\n");

    let used = fixture.symbol("git/git.go::Diff");
    let decoy = fixture.symbol("other/other.go::Diff");

    assert!(used.contains("cmd/main.go:5 main"), "{used}");
    assert!(decoy.contains("used by 0 site(s)"), "{decoy}");
}

#[test]
fn a_string_naming_a_function_in_another_language_is_a_by_name_link() {
    let mut fixture = Fixture::new();
    fixture.write(
        "backend/src/lib.rs",
        "pub fn git_file_lines() {}\npub fn other_command() {}\n",
    );
    fixture.write(
        "ui/api.ts",
        "export function load() { return invoke('git_file_lines'); }\nexport const label = 'plain_words_here';\n",
    );

    let out = fixture.symbol("git_file_lines");
    let impact = fixture.impact("git_file_lines", 2);

    assert!(out.contains("ui/api.ts:1 load (by name)"), "{out}");
    assert!(impact.contains("load → by name git_file_lines"), "{impact}");
    assert!(fixture.symbol("other_command").contains("used by 0 site(s)"));
}

#[test]
fn a_string_in_the_same_language_is_a_link_only_for_functions_nobody_calls() {
    let mut fixture = Fixture::new();
    fixture.write(
        "src/a.ts",
        "export function called_one() {}\nexport function entry_point() {}\ncalled_one();\n",
    );
    fixture.write("src/b.ts", "export const routes = ['called_one', 'entry_point'];\n");

    assert!(fixture.symbol("entry_point").contains("src/b.ts:1 routes (by name)"));
    assert!(!fixture.symbol("called_one").contains("(by name)"));
}

#[test]
fn a_big_change_is_a_bulk_build_and_a_small_one_is_silent() {
    let mut fixture = Fixture::new();

    for n in 0..250 {
        fixture.write(&format!("src/f{n}.rs"), &format!("pub fn f{n}_fn() {{}}\n"));
    }

    let root = fixture.repo.path().to_path_buf();
    let mut first = Recorder::default();
    let (_, refresh) = fixture
        .index
        .graph_observed(&root, &|| false, &mut first)
        .expect("graph");

    assert_eq!(refresh.build.map(|(_, kind)| kind), Some(BuildKind::Full));
    assert_eq!(first.events.first().map(String::as_str), Some("start 1 full 250"));
    assert_eq!(first.events.get(1).map(String::as_str), Some("progress 1"));
    assert_eq!(first.events.iter().filter(|e| e.starts_with("progress")).count(), 250);
    assert_eq!(first.events.last().map(String::as_str), Some("finish ok"));

    for n in 0..199 {
        fixture.write(&format!("src/f{n}.rs"), &format!("pub fn f{n}_fn() {{}}\n// edited\n"));
    }

    let mut quiet = Recorder::default();
    let (_, refresh) = fixture
        .index
        .graph_observed(&root, &|| false, &mut quiet)
        .expect("graph");

    assert_eq!(refresh.reparsed, 199);
    assert!(refresh.build.is_none());
    assert!(quiet.events.is_empty());

    for n in 0..200 {
        fixture.write(
            &format!("src/f{n}.rs"),
            &format!("pub fn f{n}_fn() {{}}\n// edited again\n"),
        );
    }

    let mut bulk = Recorder::default();
    let (_, refresh) = fixture
        .index
        .graph_observed(&root, &|| false, &mut bulk)
        .expect("graph");

    assert_eq!(refresh.build.map(|(_, kind)| kind), Some(BuildKind::Bulk));
    assert_eq!(bulk.events.first().map(String::as_str), Some("start 2 bulk 200"));
    assert_eq!(bulk.events.last().map(String::as_str), Some("finish ok"));

    let status: String = fixture
        .index
        .store
        .conn()
        .query_row(
            "SELECT group_concat(kind || ':' || status || ':' || files_done, ',') FROM builds",
            [],
            |row| row.get(0),
        )
        .expect("builds");

    assert_eq!(status, "full:ok:250,bulk:ok:200");
}

#[test]
fn a_cancelled_build_rolls_back_cleanly_and_the_next_call_finishes_it() {
    let mut fixture = Fixture::new();

    for n in 0..10 {
        fixture.write(&format!("src/f{n}.rs"), &format!("pub fn g{n}_fn() {{}}\n"));
    }

    let root = fixture.repo.path().to_path_buf();
    let calls = Cell::new(0_usize);
    // two walks of 11 entries, then the third file
    let stop = || {
        calls.set(calls.get() + 1);
        calls.get() > 24
    };
    let mut recorder = Recorder::default();
    let outcome = fixture.index.graph_observed(&root, &stop, &mut recorder);

    assert_eq!(outcome.err().as_deref(), Some(CANCELLED));
    assert!(recorder.events.last().is_some_and(|e| e.starts_with("finish error")));

    let status: String = fixture
        .index
        .store
        .conn()
        .query_row("SELECT status FROM builds", [], |row| row.get(0))
        .expect("build row");

    assert_eq!(status, "error");

    let refresh = fixture.refresh();

    assert_eq!(fixture.counts().files, 10);
    assert!(refresh.reparsed >= 10 || refresh.checked == 10);
    assert!(fixture.symbol("g9_fn").starts_with("fn g9_fn"));
}

#[test]
fn only_the_three_most_recent_repositories_keep_a_graph() {
    let mut fixture = Fixture::new();
    let repos: Vec<tempfile::TempDir> = (0..4)
        .map(|n| {
            let dir = tempfile::tempdir().expect("dir");
            fs::write(dir.path().join("lib.rs"), format!("pub fn repo{n}_fn() {{}}\n")).expect("write");
            dir
        })
        .collect();

    for repo in &repos {
        fixture.index.graph(repo.path(), &|| false).expect("graph");
    }

    assert_eq!(fixture.index.cache.len(), 3);
    assert!(
        fixture
            .index
            .cache
            .iter()
            .all(|c| c.key != repos[0].path().to_str().expect("utf-8"))
    );

    let (_, refresh) = fixture.index.graph(repos[0].path(), &|| false).expect("graph");

    assert!(refresh.graph_rebuilt);
    assert_eq!(refresh.reparsed, 0);
    assert_eq!(fixture.index.cache.len(), 3);
}

#[test]
fn a_relative_or_missing_project_path_is_an_error() {
    let mut fixture = Fixture::new();

    assert!(fixture.index.graph(Path::new("relative/path"), &|| false).is_err());
    assert!(fixture.index.graph(&fixture.path("missing"), &|| false).is_err());
}

#[test]
fn the_map_lists_central_files_first_and_leaves_tests_out() {
    let mut fixture = Fixture::new();
    fixture.write("src/core.rs", "pub fn central_fn() {}\npub fn rare_fn() {}\n");
    fixture.write("src/a.rs", "pub fn use_a() { central_fn(); }\n");
    fixture.write("src/b.rs", "pub fn use_b() { central_fn(); }\n");
    fixture.write("tests/it.rs", "fn test_it() { central_fn(); }\n");

    let root = fixture.repo.path().to_path_buf();
    let (graph, _) = fixture.index.graph(&root, &|| false).expect("graph");
    let map = graph.map(700, None);

    assert!(map.starts_with("src/core.rs\n  pub fn central_fn() {}"), "{map}");
    assert!(!map.contains("tests/it.rs"));
    assert!(!map.contains("rare_fn"));
}

#[test]
fn two_sessions_refreshing_the_same_repository_share_the_work() {
    let data = tempfile::tempdir().expect("data dir");
    let repo = tempfile::tempdir().expect("repo dir");
    let database = data.path().join("code.sqlite3");

    for n in 0..700 {
        fs::write(repo.path().join(format!("f{n}.rs")), format!("pub fn h{n}_fn() {{}}\n")).expect("write");
    }

    let sessions: Vec<_> = (0..2)
        .map(|_| {
            let database = database.clone();
            let root = repo.path().to_path_buf();

            std::thread::spawn(move || {
                let mut index = CodeIndex::open(&database).expect("open");
                let (graph, refresh) = index.graph(&root, &|| false).expect("graph");

                (graph.counts().files, refresh.reparsed)
            })
        })
        .collect();
    let results: Vec<(usize, usize)> = sessions.into_iter().map(|s| s.join().expect("session")).collect();

    assert!(results.iter().all(|(files, _)| *files == 700), "{results:?}");
    assert!(results.iter().map(|(_, reparsed)| reparsed).sum::<usize>() >= 700);

    let index = CodeIndex::open(&database).expect("open");
    let rows: i64 = index
        .store
        .conn()
        .query_row("SELECT count(*) FROM files", [], |row| row.get(0))
        .expect("count");

    assert_eq!(rows, 700);
}

#[test]
fn python_relative_imports_resolve_against_the_importing_files_folder() {
    let mut fixture = Fixture::new();
    fixture.write("pkg/__init__.py", "");
    fixture.write("pkg/utils.py", "def shared_helper():\n    pass\n");
    fixture.write("pkg/m.py", "def climb_target():\n    pass\n");
    fixture.write("pkg/sub/__init__.py", "");
    fixture.write(
        "pkg/sub/user.py",
        "from . import local\nfrom .. import utils\nfrom ..m import climb_target\n\n\
         def run_it():\n    local.shared_helper()\n    utils.shared_helper()\n    climb_target()\n",
    );
    fixture.write("pkg/sub/local.py", "def shared_helper():\n    pass\n");
    fixture.write("other/__init__.py", "");
    fixture.write("other/m.py", "def climb_target():\n    pass\n");

    let local = fixture.symbol("pkg/sub/local.py::shared_helper");
    let up = fixture.symbol("pkg/utils.py::shared_helper");
    let climbed = fixture.symbol("pkg/m.py::climb_target");
    let decoy = fixture.symbol("other/m.py::climb_target");

    assert!(local.contains("pkg/sub/user.py:6 run_it"), "{local}");
    assert!(up.contains("pkg/sub/user.py:7 run_it"), "{up}");
    assert!(climbed.contains("pkg/sub/user.py:8 run_it"), "{climbed}");
    assert!(decoy.contains("used by 0 site(s)"), "{decoy}");
}

#[test]
fn python_src_layout_and_module_aliases_resolve() {
    let mut fixture = Fixture::new();
    fixture.write("src/shop/__init__.py", "");
    fixture.write("src/shop/billing.py", "def charge_card():\n    pass\n");
    fixture.write("src/other/__init__.py", "");
    fixture.write("src/other/billing.py", "def charge_card():\n    pass\n");
    fixture.write(
        "app.py",
        "import shop.billing as bill\nimport other.billing\nfrom shop.billing import charge_card as pay\n\n\
         def checkout():\n    bill.charge_card()\n\ndef refund():\n    pay()\n\ndef audit():\n    other.billing.charge_card()\n",
    );

    let shop = fixture.symbol("src/shop/billing.py::charge_card");
    let other = fixture.symbol("src/other/billing.py::charge_card");

    assert!(shop.contains("app.py:6 checkout"), "{shop}");
    assert!(shop.contains("app.py:9 refund"), "{shop}");
    assert!(!shop.contains("audit"), "{shop}");
    assert!(other.contains("app.py:12 audit"), "{other}");
    assert!(!other.contains("checkout"), "{other}");
}

#[test]
fn python_standard_library_and_site_packages_stay_unresolved() {
    let mut fixture = Fixture::new();
    fixture.write("pkg/__init__.py", "");
    fixture.write("pkg/a.py", "import json\nfrom os import path\nimport requests\n\ndef load():\n    json.dumps({})\n    path.join('a')\n    requests.get('x')\n");
    fixture.write(
        "pkg/b.py",
        "def dumps():\n    pass\n\ndef join():\n    pass\n\nclass Session:\n    def get(self):\n        pass\n",
    );

    for name in ["dumps", "join"] {
        let out = fixture.symbol(&format!("pkg/b.py::{name}"));

        assert!(out.contains("used by 0 site(s)"), "{out}");
    }

    assert!(fixture.symbol("Session.get").contains("used by 0 site(s)"));
}

#[test]
fn python_methods_resolve_through_self_the_class_and_package_reexports() {
    let mut fixture = Fixture::new();
    fixture.write("lib/__init__.py", "from ._client import Client\n");
    fixture.write(
        "lib/_client.py",
        "class Client:\n    def send(self):\n        self.build()\n\n    def build(self):\n        pass\n\n    @classmethod\n    def make(cls):\n        return cls.build_default()\n\n    @classmethod\n    def build_default(cls):\n        pass\n",
    );
    fixture.write("lib/_other.py", "class Other:\n    def build(self):\n        pass\n");
    fixture.write(
        "main.py",
        "import lib\nfrom lib import Client\n\ndef go():\n    Client.make()\n    lib.Client.make()\n",
    );

    let build = fixture.symbol("Client.build");
    let make = fixture.symbol("Client.make");
    let default = fixture.symbol("Client.build_default");
    let other = fixture.symbol("Other.build");

    assert!(build.contains("lib/_client.py:3 Client::send"), "{build}");
    assert!(other.contains("used by 0 site(s)"), "{other}");
    assert!(make.contains("main.py:5 go"), "{make}");
    assert!(default.contains("lib/_client.py:10 Client::make"), "{default}");
}

#[test]
fn python_test_files_are_tests() {
    let mut fixture = Fixture::new();
    fixture.write("pkg/__init__.py", "");
    fixture.write("pkg/core.py", "def target_one():\n    pass\n");
    fixture.write(
        "pkg/test_core.py",
        "from pkg.core import target_one\n\ndef check_it():\n    target_one()\n",
    );
    fixture.write(
        "pkg/core_test.py",
        "from pkg.core import target_one\n\ndef check_two():\n    target_one()\n",
    );
    fixture.write("conftest.py", "def fixture_helper():\n    pass\n");
    fixture.write("tests/helpers.py", "def helper_fn():\n    pass\n");
    fixture.write(
        "pkg/prod.py",
        "from pkg.core import target_one\n\ndef serve():\n    target_one()\n",
    );

    let out = fixture.impact("target_one", 1);

    assert!(out.contains("pkg/prod.py  [serve → target_one]"), "{out}");
    assert!(
        out.contains("tests: pkg/core_test.py (1), pkg/test_core.py (1)"),
        "{out}"
    );

    let tests_first = fixture.symbol("target_one");
    let prod = tests_first.find("pkg/prod.py").expect("prod caller");
    let test = tests_first.find("pkg/test_core.py").expect("test caller");

    assert!(prod < test, "production callers come before tests: {tests_first}");
}

#[test]
fn malformed_python_does_not_stop_the_index() {
    let mut fixture = Fixture::new();
    fixture.write("good.py", "def still_fine():\n    pass\n");
    fixture.write("bad.py", "def (((:\n  class\n from import\n@@@\n");
    fixture.write("deep.py", &format!("x = {}1{}\n", "(".repeat(5_000), ")".repeat(5_000)));

    assert!(fixture.symbol("still_fine").starts_with("fn still_fine"));
}

#[test]
fn python_bare_calls_never_resolve_to_methods() {
    let mut fixture = Fixture::new();
    fixture.write(
        "pkg/client.py",
        "class Client:\n    def send(self):\n        pass\n\ndef transmit():\n    pass\n",
    );
    fixture.write(
        "pkg/app.py",
        "def handler(send, other):\n    send()\n    other.send()\n",
    );

    let method = fixture.symbol("Client.send");

    assert!(method.contains("pkg/app.py:3 handler"), "{method}");
    assert!(!method.contains("pkg/app.py:2"), "{method}");
}

// ---- receiver types -------------------------------------------------------------------------------------

/// The part of a symbol answer that belongs to the definition whose first line starts with `header`.
fn section<'a>(answer: &'a str, header: &str) -> &'a str {
    let start = answer.find(header).unwrap_or(0);
    let rest = answer.get(start..).unwrap_or("");
    let end = rest.find("\n\n").unwrap_or(rest.len());
    let next = rest
        .get(1..)
        .and_then(|tail| tail.find("\nfn ").or_else(|| tail.find("\nmethod ")))
        .map_or(rest.len(), |at| at + 1);

    rest.get(..end.min(next)).unwrap_or(rest)
}

/// The "used by" site lines of a symbol answer, without the source line after a ` | `.
fn sites(answer: &str) -> Vec<&str> {
    let mut in_users = false;

    answer
        .lines()
        .filter_map(|line| {
            let indent = line.len() - line.trim_start().len();

            // a heading ("  used by", "  uses:") starts a section
            if indent < 4 {
                in_users = line.starts_with("  used by");

                return None;
            }

            (in_users && indent == 4).then(|| line.trim().split(" | ").next().unwrap_or_default())
        })
        .collect()
}

#[test]
fn typescript_receivers_follow_annotations_fields_constructors_and_return_types() {
    let mut fixture = Fixture::new();
    fixture.write(
        "src/store.ts",
        "export class Store { save(v: string): void {} }\nexport class Cache { save(v: string): void {} }\n\
         export interface Client { fetchAll: (id: string) => Promise<string[]>; name: string }\n\
         export function makeStore(): Store { return new Store(); }\n",
    );
    fixture.write(
        "src/use.ts",
        "import { Store, Cache, Client, makeStore } from './store';\n\
         export class Service {\n  private cache: Cache = new Cache();\n  constructor(private store: Store) {}\n\
         \x20 run(client: Client) {\n    this.store.save('a');\n    this.cache.save('b');\n    client.fetchAll('x');\n\
         \x20   const fresh = makeStore();\n    fresh.save('c');\n    const other = new Cache();\n    other.save('d');\n  }\n}\n",
    );

    let store = fixture.symbol("Store.save");
    let cache = fixture.symbol("Cache.save");
    let fetch = fixture.symbol("fetchAll");

    assert_eq!(sites(&store), ["src/use.ts:6 Service::run"], "{store}");
    assert!(!store.contains(" ?"), "{store}");
    assert_eq!(sites(&cache), ["src/use.ts:7 Service::run"], "{cache}");
    assert!(
        fetch.contains("src/use.ts:8 Service::run"),
        "an interface member exists as a method: {fetch}"
    );

    // `fresh = makeStore()` is a Store through the written return type, `other = new Cache()` a Cache
    let both = fixture.impact("Store.save", 1);

    assert!(both.contains("Service::run → Store::save"), "{both}");
}

#[test]
fn an_object_literal_property_is_a_member_of_the_declared_type() {
    let mut fixture = Fixture::new();
    fixture.write(
        "src/contract.ts",
        "export interface Api { listSubjects: (w: string) => string[] }\n\
         export function build(): Api {\n  return { listSubjects: (w) => [w] };\n}\n",
    );
    fixture.write(
        "src/page.ts",
        "import { Api } from './contract';\nexport function page(api: Api) {\n  return api.listSubjects('w');\n}\n",
    );

    let answer = fixture.symbol("listSubjects");

    assert!(
        answer.contains("member Api::listSubjects  src/contract.ts:1"),
        "{answer}"
    );
    assert!(
        answer.contains("member Api::listSubjects  src/contract.ts:3"),
        "{answer}"
    );
    assert!(answer.contains("src/page.ts:3 page ?"), "two members of Api: {answer}");
}

#[test]
fn a_receiver_of_a_type_the_repository_does_not_define_resolves_to_nothing() {
    let mut fixture = Fixture::new();
    fixture.write(
        "src/model.ts",
        "export class Repo { get(id: string) {} set(id: string) {} }\n",
    );
    fixture.write(
        "src/use.ts",
        "import { Repo } from './model';\nexport function typed(repo: Repo) {\n  repo.get('a');\n}\n\
         export function foreign(cache: Map<string, string>) {\n  cache.get('b');\n}\n\
         export function unknown(raw) {\n  raw.get('c');\n}\n\
         export function literal() {\n  new Map<string, string>().set('d', 'e');\n}\n",
    );

    let get = fixture.symbol("Repo.get");
    let set = fixture.symbol("Repo.set");

    assert_eq!(sites(&get), ["src/use.ts:3 typed", "src/use.ts:9 unknown ?"], "{get}");
    assert!(sites(&set).is_empty(), "{set}");
}

#[test]
fn rust_receivers_follow_self_fields_lets_and_return_types() {
    let mut fixture = Fixture::new();
    fixture.write("Cargo.toml", "[package]\nname = \"app\"\n");
    fixture.write(
        "src/lib.rs",
        "pub struct Store { n: u32 }\nimpl Store {\n    pub fn new() -> Self { Store { n: 0 } }\n    pub fn save(&self) {}\n    pub fn len(&self) -> usize { 0 }\n}\n\
         pub struct Quote;\nimpl Quote { pub fn save(&self) {} pub fn len(&self) -> usize { 0 } }\n\
         pub struct Service { store: Store, names: Vec<String> }\n\
         impl Service {\n    pub fn run(&self, quote: &Quote) {\n        self.store.save();\n        quote.save();\n\
         \x20       let fresh = Store::new();\n        fresh.len();\n        self.names.len();\n\
         \x20       for name in &self.names { name.len(); }\n    }\n}\n",
    );

    let store_save = fixture.symbol("Store::save");
    let quote_save = fixture.symbol("Quote::save");
    let store_len = fixture.symbol("Store::len");
    let quote_len = fixture.symbol("Quote::len");

    assert_eq!(sites(&store_save), ["src/lib.rs:12 Service::run"], "{store_save}");
    assert_eq!(sites(&quote_save), ["src/lib.rs:13 Service::run"], "{quote_save}");
    assert_eq!(sites(&store_len), ["src/lib.rs:15 Service::run"], "{store_len}");
    assert!(
        sites(&quote_len).is_empty(),
        "Vec::len and str::len are not Quote::len: {quote_len}"
    );
}

#[test]
fn go_receivers_follow_fields_constructors_literals_and_foreign_calls() {
    let mut fixture = Fixture::new();
    fixture.write("go.mod", "module example.com/app\n\ngo 1.22\n");
    fixture.write(
        "main.go",
        "package main\n\nimport \"os/exec\"\n\ntype Store struct{}\n\nfunc (s *Store) Run() {}\nfunc (s *Store) append(v int) {}\n\
         func NewStore() *Store { return &Store{} }\nfunc Run() {}\n\ntype Server struct {\n\tstore *Store\n}\n\n\
         func (s *Server) ByField() { s.store.Run() }\n\
         func ByConstructor() {\n\tlocal := NewStore()\n\tlocal.Run()\n}\n\
         func ByLiteral() {\n\tliteral := &Store{}\n\tliteral.Run()\n}\n\
         func Foreign() {\n\tcmd := exec.Command(\"ls\")\n\tcmd.Run()\n\texec.Command(\"ls\").Run()\n}\n\
         func Builtin(xs []int) {\n\t_ = append(xs, 1)\n}\n",
    );

    let method = fixture.symbol("Store.Run");
    let function = fixture.symbol("Run");
    let function = section(&function, "fn Run");
    let append = fixture.symbol("Store.append");

    assert_eq!(
        sites(&method),
        [
            "main.go:16 Server::ByField",
            "main.go:19 ByConstructor",
            "main.go:23 ByLiteral"
        ],
        "{method}"
    );
    assert!(
        sites(function).is_empty(),
        "exec.Command(..).Run() is no call of Run: {function}"
    );
    assert!(sites(&append).is_empty(), "the builtin append is no method: {append}");
}

#[test]
fn python_receivers_follow_annotations_constructors_and_literals() {
    let mut fixture = Fixture::new();
    fixture.write(
        "app.py",
        "class Params:\n    def set(self, k):\n        pass\n\nclass Event:\n    def set(self):\n        pass\n\n\
         class Url:\n    def __init__(self):\n        self.params: Params = Params()\n\n    @property\n    def query(self) -> Params:\n        return self.params\n\n\
         \x20   def by_field(self):\n        self.params.set('a')\n\n    def by_property(self):\n        self.query.set('b')\n\n\
         \x20   def by_literal(self):\n        b\"\".join([])\n\n    def by_annotation(self, other: Event):\n        other.set()\n\n\
         \x20   def by_constructor(self):\n        done = Event()\n        done.set()\n\n    def by_nothing(self, flow):\n        flow.set()\n",
    );

    let params = fixture.symbol("Params.set");
    let event = fixture.symbol("Event.set");

    assert_eq!(
        sites(&params),
        [
            "app.py:18 Url::by_field",
            "app.py:21 Url::by_property",
            "app.py:34 Url::by_nothing ?"
        ],
        "{params}"
    );
    assert_eq!(
        sites(&event),
        [
            "app.py:27 Url::by_annotation",
            "app.py:31 Url::by_constructor",
            "app.py:34 Url::by_nothing ?"
        ],
        "an unknown receiver still matches by name: {event}"
    );
}

#[test]
fn unknown_receivers_keep_matching_by_name() {
    let mut fixture = Fixture::new();
    fixture.write("a.ts", "export class A { run() {} }\n");
    fixture.write("b.ts", "export function go(x) { x.run(); }\n");

    let answer = fixture.symbol("A.run");

    assert_eq!(sites(&answer), ["b.ts:1 go ?"], "{answer}");
}

// ---- builtins, constants, macros ------------------------------------------------------------------------

#[test]
fn builtins_never_resolve_to_a_method_or_to_a_far_definition() {
    let mut fixture = Fixture::new();
    fixture.write("go.mod", "module example.com/app\n\ngo 1.22\n");
    fixture.write("pkg/hist.go", "package pkg\n\ntype History struct{}\n\nfunc (h *History) append(v int) {}\nfunc (h *History) len() int { return 0 }\n");
    fixture.write(
        "pkg/use.go",
        "package pkg\n\nfunc Use(xs []int) int {\n\txs = append(xs, 1)\n\treturn len(xs)\n}\n",
    );
    fixture.write(
        "lib/helpers.py",
        "def len(x):\n    return 0\n\ndef print_all(x):\n    pass\n",
    );
    fixture.write("app/main.py", "def run(xs):\n    print_all(xs)\n    return len(xs)\n");
    fixture.write(
        "app/own.py",
        "def len(x):\n    return 1\n\ndef run_own(xs):\n    return len(xs)\n",
    );

    assert!(sites(&fixture.symbol("History.append")).is_empty());
    assert!(sites(&fixture.symbol("History.len")).is_empty());

    let own = fixture.symbol("app/own::len");

    assert!(
        own.contains("app/own.py:5 run_own"),
        "a definition of the same package wins: {own}"
    );

    let far = fixture.symbol("lib/helpers::len");

    assert!(
        !far.contains("app/main.py"),
        "the builtin len is not lib/helpers::len: {far}"
    );
}

#[test]
fn constants_and_enum_members_are_mentions_across_languages() {
    let mut fixture = Fixture::new();
    fixture.write("Cargo.toml", "[package]\nname = \"app\"\n");
    fixture.write(
        "src/lib.rs",
        "pub const LIMIT: u32 = 3;\npub enum Mode { Fast, Slow }\npub fn pick(m: Mode) -> u32 {\n    match m { Mode::Fast => LIMIT, Mode::Slow => 0 }\n}\n\
         pub fn check() { assert_eq!(pick(Mode::Fast), 3); }\n",
    );
    fixture.write(
        "src/events.ts",
        "export const EVT_DONE = 'done';\nexport enum Phase { Start, Stop }\n\
         export function emit(p: Phase) { if (p === Phase.Stop) { send(EVT_DONE); } }\nfunction send(x: string) {}\n",
    );
    fixture.write(
        "tools/kinds.py",
        "import enum\n\nMAX_TRIES = 3\n\nclass Color(enum.Enum):\n    RED = 1\n    BLUE = 2\n\ndef paint(c):\n    if c == Color.RED:\n        return MAX_TRIES\n",
    );

    let limit = fixture.symbol("LIMIT");
    let fast = fixture.symbol("Mode::Fast");
    let done = fixture.symbol("EVT_DONE");
    let stop = fixture.symbol("Phase::Stop");
    let red = fixture.symbol("Color::RED");
    let tries = fixture.symbol("MAX_TRIES");

    assert!(limit.contains("src/lib.rs:4 pick"), "{limit}");
    assert!(fast.contains("enum_member Mode::Fast"), "{fast}");
    assert!(
        fast.contains("src/lib.rs:4 pick") && fast.contains("src/lib.rs:6 check"),
        "{fast}"
    );
    assert!(done.contains("src/events.ts:3 emit"), "{done}");
    assert!(stop.contains("src/events.ts:3 emit"), "{stop}");
    assert!(red.contains("tools/kinds.py:10 paint"), "{red}");
    assert!(tries.contains("tools/kinds.py:11 paint"), "{tries}");
}

#[test]
fn calls_inside_macro_arguments_are_references() {
    let mut fixture = Fixture::new();
    fixture.write("Cargo.toml", "[package]\nname = \"app\"\n");
    fixture.write(
        "src/lib.rs",
        "mod util;\npub fn run() {\n    println!(\"{}\", util::double(2));\n    let v = vec![helper(1), helper(2)];\n    assert_eq!(helper(3), v.len() as u32);\n}\nfn helper(x: u32) -> u32 { x }\n",
    );
    fixture.write("src/util.rs", "pub fn double(x: u32) -> u32 { x * 2 }\n");

    assert!(fixture.symbol("util::double").contains("src/lib.rs:3 run"));
    assert_eq!(sites(&fixture.symbol("helper")), ["src/lib.rs:4 run"]);
}

// ---- walking, tests, the single-file component and re-exports ---------------------------------------------

#[test]
fn build_and_dist_folders_are_output_only_at_the_top_or_next_to_a_manifest() {
    let mut fixture = Fixture::new();
    fixture.write("build/out.js", "export function from_output() {}\n");
    fixture.write("dist/out.js", "export function from_dist() {}\n");
    fixture.write("src/node/build/build.ts", "export function build() {}\n");
    fixture.write("packages/app/package.json", "{}\n");
    fixture.write("packages/app/dist/bundle.js", "export function bundled() {}\n");
    fixture.write("packages/app/src/main.ts", "export function main() {}\n");
    fixture.write("target/debug/x.rs", "pub fn skipped() {}\n");

    assert!(
        fixture
            .symbol("build")
            .starts_with("fn build  src/node/build/build.ts:1")
    );
    assert!(fixture.symbol("main").starts_with("fn main"));

    for hidden in ["from_output", "from_dist", "bundled", "skipped"] {
        assert!(
            fixture.symbol(hidden).starts_with("No symbol, file or command matches"),
            "{hidden}"
        );
    }
}

#[test]
fn test_folders_and_file_names_of_typescript_are_tests() {
    let mut fixture = Fixture::new();
    fixture.write("src/core.ts", "export function target() {}\n");

    for (path, name) in [
        ("src/__tests__/a.ts", "a"),
        ("src/a_test.ts", "b"),
        ("src/c.spec.ts", "c"),
        ("src/d.test.tsx", "d"),
        ("e2e/e.ts", "e"),
        ("src/__mocks__/f.ts", "f"),
    ] {
        fixture.write(
            path,
            &format!("import {{ target }} from '../src/core';\nexport function check_{name}() {{ target(); }}\n"),
        );
    }

    fixture.write(
        "src/g.ts",
        "import { target } from './core';\nexport function prod() { target(); }\n",
    );

    let out = fixture.impact("target", 1);

    assert!(
        out.contains("1 production file(s)") || out.contains("2 production file(s)"),
        "{out}"
    );
    assert!(out.contains("src/g.ts  [prod → target]"), "{out}");
    assert!(out.contains("6 test file(s)"), "{out}");
}

#[test]
fn a_top_level_const_in_a_component_script_belongs_to_the_component() {
    let mut fixture = Fixture::new();
    fixture.write("src/lib.ts", "export function load() { return 1; }\n");
    fixture.write(
        "src/Card.svelte",
        "<script lang=\"ts\">\n  import { load } from './lib';\n  const value = load();\n</script>\n<p>{value}</p>\n",
    );
    fixture.write(
        "src/App.svelte",
        "<script lang=\"ts\">\n  import Card from './Card.svelte';\n</script>\n<Card />\n",
    );

    let out = fixture.impact("load", 3);

    assert!(out.contains("Card → load"), "{out}");
    assert!(out.contains("App → Card"), "{out}");
}

#[test]
fn reexports_and_lazy_imports_lead_to_the_defining_file() {
    let mut fixture = Fixture::new();
    fixture.write(
        "tsconfig.json",
        "{ \"compilerOptions\": { \"paths\": { \"lib\": [\"./src/index.ts\"] } } }\n",
    );
    fixture.write(
        "src/index.ts",
        "export { useData } from './app/data';\nexport * from './app/other';\n",
    );
    fixture.write("src/app/data.ts", "export function useData() {}\n");
    fixture.write("src/app/other.ts", "export function other() {}\n");
    fixture.write("src/theme/data.ts", "export function useData() {}\n");
    fixture.write(
        "src/use.ts",
        "import { useData, other } from 'lib';\nexport function setup() { useData(); other(); }\n",
    );
    fixture.write("src/pages/Home.tsx", "export function Home() { return 1; }\n");
    fixture.write(
        "src/App.tsx",
        "import { lazy } from 'react';\nconst Home = lazy(() => import('./pages/Home').then((m) => ({ default: m.Home })));\n\
         export function App() { return <Home />; }\n",
    );

    let app = fixture.symbol("app/data::useData");
    let theme = fixture.symbol("theme/data::useData");
    let other = fixture.symbol("other");
    let home = fixture.symbol("Home");

    assert!(app.contains("src/use.ts:2 setup"), "{app}");
    assert!(!theme.contains("src/use.ts"), "{theme}");
    assert!(other.contains("src/use.ts:2 setup"), "{other}");
    assert!(home.contains("src/App.tsx:3 App"), "{home}");
    assert!(!home.contains("const Home"), "the lazy const is no definition: {home}");
}

#[test]
fn a_function_passed_as_a_value_is_a_reference_when_it_is_defined_or_imported_here() {
    let mut fixture = Fixture::new();
    fixture.write("src/serialize.ts", "export function toPublic(x) { return x; }\n");
    fixture.write(
        "src/routes.ts",
        "import { toPublic } from './serialize';\nfunction handler(c) { return c; }\nexport function register(app, rows, other) {\n\
         \x20 app.get('/x', handler);\n  return rows.map(toPublic).filter(other);\n}\n",
    );
    fixture.write("src/other.ts", "export function other() {}\n");

    assert_eq!(sites(&fixture.symbol("toPublic")), ["src/routes.ts:5 register"]);
    assert_eq!(sites(&fixture.symbol("handler")), ["src/routes.ts:4 register"]);
    assert!(
        sites(&fixture.symbol("src/other.ts::other")).is_empty(),
        "a parameter is not that function"
    );
}

// ---- ranking, the answers ---------------------------------------------------------------------------------

#[test]
fn name_only_method_matches_count_less_in_the_map_than_typed_ones() {
    let mut fixture = Fixture::new();

    // `hub.ts` is the target of two real imports; `noisy.ts` only shares the method names `add` / `get`
    fixture.write("src/hub.ts", "export function hub() {}\n");
    fixture.write("src/noisy.ts", "export class Bag { add(x) {} get(x) {} clear() {} }\n");

    for index in 0..4 {
        fixture.write(
            &format!("src/user{index}.ts"),
            &format!("import {{ hub }} from './hub';\nexport function u{index}(thing) {{ hub(); thing.add(1); thing.get(2); thing.clear(); }}\n"),
        );
    }

    let root = fixture.repo.path().to_path_buf();
    let (graph, _) = fixture.index.graph(&root, &|| false).expect("graph");
    let map = graph.map(700, None);
    let order: Vec<&str> = map.lines().filter(|l| !l.starts_with(' ')).collect();
    let hub = order.iter().position(|f| *f == "src/hub.ts").expect("hub");
    let noisy = order.iter().position(|f| *f == "src/noisy.ts");

    assert!(noisy.is_none_or(|noisy| hub < noisy), "{map}");
}

#[test]
fn the_most_certain_use_is_the_line_shown_for_a_caller() {
    let mut fixture = Fixture::new();
    fixture.write("go.mod", "module example.com/app\n\ngo 1.22\n");
    fixture.write(
        "a.go",
        "package main\n\ntype Matcher struct{}\ntype Terminal struct{}\n\nfunc (m *Matcher) Loop() {}\nfunc (t *Terminal) Loop() {}\n\
         func Run(unknown *Other, m *Matcher, t *Terminal) {\n\tunknown.Loop()\n\tgo m.Loop()\n\tgo t.Loop()\n}\n\ntype Other struct{}\n",
    );

    let terminal = fixture.symbol("Terminal.Loop");

    assert_eq!(
        sites(&terminal),
        ["a.go:11 Run"],
        "the typed call, not the unknown receiver at line 9: {terminal}"
    );
}

#[test]
fn an_impact_answer_never_cuts_a_direct_caller() {
    let mut fixture = Fixture::new();
    fixture.write("src/core.ts", "export function core() {}\n");

    for index in 0..60 {
        fixture.write(
            &format!("src/direct/d{index:02}.ts"),
            &format!("import {{ core }} from '../core';\nexport function d{index:02}() {{ core(); }}\n"),
        );
    }

    for index in 0..120 {
        fixture.write(
            &format!("src/far/f{index:03}.ts"),
            &format!(
                "import {{ d{:02} }} from '../direct/d{:02}';\nexport function f{index:03}() {{ d{:02}(); }}\n",
                index % 60,
                index % 60,
                index % 60
            ),
        );
    }

    let out = fixture.impact("core", 2);
    let direct = out.lines().filter(|l| l.starts_with("  d1 ")).count();
    let far_detailed = out
        .lines()
        .filter(|l| l.starts_with("  d2 ") && l.contains('['))
        .count();
    let far_bare = out
        .lines()
        .filter(|l| l.starts_with("  d2 ") && !l.contains('['))
        .count();

    assert_eq!(direct, 60, "{out}");
    assert_eq!(far_detailed, 0, "the direct callers used the detailed lines: {out}");
    assert_eq!(far_bare, 60, "{out}");
    assert!(out.contains("… not listed: 60 at d2"), "{out}");
}

#[test]
fn a_path_goes_through_production_code_through_a_shared_constant_and_stops_at_the_depth() {
    let mut fixture = Fixture::new();
    fixture.write("Cargo.toml", "[package]\nname = \"app\"\n");
    fixture.write(
        "src/lib.rs",
        "pub const EVT_SAVE: u32 = 1;\npub fn producer() -> u32 { EVT_SAVE }\npub fn consumer(e: u32) { if e == EVT_SAVE { sink(); } }\nfn sink() {}\n\
         pub fn long_a() { long_b(); }\nfn long_b() { long_c(); }\nfn long_c() { long_d(); }\nfn long_d() {}\n\
         #[cfg(test)]\nmod tests { use super::*; #[test] fn shortcut() { producer(); } }\n",
    );

    let bridged = fixture.chain("producer", "sink", 6);

    assert_eq!(
        bridged,
        "`producer` → `sink`: 3 hops\n    producer  src/lib.rs:2\n\
         \x20 → EVT_SAVE  src/lib.rs:1\n        at src/lib.rs:2 | pub fn producer() -> u32 { EVT_SAVE }\n\
         \x20 → consumer  src/lib.rs:3\n        at src/lib.rs:3 | pub fn consumer(e: u32) { if e == EVT_SAVE { sink(); } }\n\
         \x20 → sink  src/lib.rs:4\n        at src/lib.rs:3 | pub fn consumer(e: u32) { if e == EVT_SAVE { sink(); } }\n"
    );
    assert!(
        fixture
            .chain("long_a", "long_d", 2)
            .starts_with("no chain within 2 hops")
    );
    assert!(fixture.chain("long_a", "long_d", 3).contains(": 3 hops"));
    assert!(
        fixture
            .chain("sink", "producer", 6)
            .starts_with("no chain from `sink` to `producer`, reverse:")
    );
}

#[test]
fn the_builds_a_worker_started_are_those_of_its_pid_since_it_was_spawned() {
    let fixture = Fixture::new();
    let key = "/repo";
    let txn = fixture.index.store.write().expect("write");
    let repo = txn.repo(key).expect("repo");
    let since = store::now();

    let mut ids = Vec::new();

    // a row of a process that had this pid weeks ago, one of another pid, and the worker's own
    for (pid, started_at) in [(42, since - 86_400 * 20), (43, since), (42, since - 1), (42, since)] {
        txn.conn()
            .execute(
                "INSERT INTO builds(repo_id, kind, status, started_at, pid) VALUES (?1, 'full', 'running', ?2, ?3)",
                rusqlite::params![repo, started_at, pid],
            )
            .expect("row");
        ids.push(txn.conn().last_insert_rowid());
    }

    txn.finish().expect("commit");

    assert_eq!(fixture.index.started_by(key, 42, since - 1), vec![ids[2], ids[3]]);
    assert_eq!(fixture.index.started_by(key, 42, since), vec![ids[3]]);
}
