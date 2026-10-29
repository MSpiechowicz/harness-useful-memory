//! Resolution and rendering behaviours that need a small repository on disk, asked through the public
//! `CodeIndex` the way the tools do.

use std::fs;
use std::time::{Duration, Instant};

use super::super::CodeIndex;
use super::Graph;

/// A repository folder and an index over a database folder of its own.
struct Repo {
    dir: tempfile::TempDir,
    _data: tempfile::TempDir,
    index: CodeIndex,
}

impl Repo {
    fn new() -> Self {
        let data = tempfile::tempdir().expect("data dir");
        let index = CodeIndex::open(&data.path().join("code.sqlite3")).expect("open");

        Self {
            dir: tempfile::tempdir().expect("repo dir"),
            _data: data,
            index,
        }
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.dir.path().join(rel);

        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, content).expect("write");
    }

    fn graph(&mut self) -> &Graph {
        let root = self.dir.path().canonicalize().expect("canonical");

        self.index.graph(&root, &|| false).expect("graph").0
    }

    fn symbol(&mut self, query: &str) -> String {
        self.graph().symbol(query, 20)
    }
}

/// The `used by` site lines of a symbol answer, without the source line after a ` | `.
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

// ---- what the agent reads ------------------------------------------------------------------------------

#[test]
fn paths_and_signatures_with_line_breaks_and_controls_are_written_out_in_every_answer() {
    let mut repo = Repo::new();

    // a file name that starts a forged line of an answer, and a signature that reorders what follows it
    repo.write(
        "src/evil\n  d1 src/trusted.rs  [pretty_function]\u{1b}[2J.rs",
        "pub fn target_fn(/* \u{202e}hidden */ n: i32) {}\n",
    );
    repo.write("src/caller.rs", "pub fn call_it() { target_fn(1); }\n");

    let graph = repo.graph();
    let answers = [
        graph.symbol("target_fn", 20),
        graph.impact("target_fn", 2),
        graph.map(2000, None),
        graph.path("call_it", "target_fn", 3),
    ];

    for answer in &answers {
        assert!(!answer.contains(['\u{1b}', '\u{202e}']), "{answer:?}");
        assert!(
            !answer
                .lines()
                .any(|line| line.trim_start().starts_with("d1 src/trusted.rs")),
            "a forged line: {answer:?}"
        );
        assert!(
            answer
                .chars()
                .all(|c| c == '\n' || c == '\t' || crate::support::text::display_safe(&c.to_string())),
            "{answer:?}"
        );
    }

    assert!(answers[0].contains("evil\\n  d1 src/trusted.rs"), "{}", answers[0]);
    assert!(answers[0].contains("\\u{202e}hidden"), "{}", answers[0]);
}

#[test]
fn a_ranking_that_runs_past_its_time_is_given_up_instead_of_finishing() {
    use crate::code::pace::Pace;

    let mut repo = Repo::new();

    repo.write("src/a.rs", "pub fn alpha() { beta(); }\npub fn beta() {}\n");

    let graph = repo.graph();

    assert!(graph.trial(&Pace::new(&|| false, Duration::from_secs(60))).is_ok());

    // a time budget that is already spent, and a cancel
    let spent = Pace::new(&|| false, Duration::ZERO);

    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(graph.trial(&spent), Err("indexing took too long".to_owned()));
    assert_eq!(
        graph.trial(&Pace::new(&|| true, Duration::from_secs(60))),
        Err("cancelled".to_owned())
    );
}

// ---- re-exports ----------------------------------------------------------------------------------------

#[test]
fn a_chain_of_repeated_reexports_is_searched_once_per_module() {
    let mut repo = Repo::new();
    let line = |next: &str| format!("export * from './{next}';\n").repeat(200);

    // five files, each passing everything on to the next 200 times: 200^4 paths for a search without a memory
    repo.write("barrel/a.ts", &line("b"));
    repo.write("barrel/b.ts", &line("c"));
    repo.write("barrel/c.ts", &line("d"));
    repo.write("barrel/d.ts", &line("e"));
    repo.write("barrel/e.ts", "export const q = 1;\n");
    repo.write("lib/z.ts", "export function zzz() {}\n");
    repo.write(
        "app/user.ts",
        "import { zzz } from '../barrel/a';\nexport function run() { zzz(); }\n",
    );

    let started = Instant::now();
    let answer = repo.symbol("zzz");

    // 200^4 paths took minutes; one visit per module is instant
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    // the chain never reaches the file that defines it, so the name alone decides (as for any import the
    // index cannot follow)
    assert!(answer.contains("fn zzz  lib/z.ts:1"), "{answer}");
}

#[test]
fn a_diamond_of_reexports_is_followed_to_the_defining_file() {
    let mut repo = Repo::new();

    repo.write("lib/index.ts", "export * from './left';\nexport * from './right';\n");
    repo.write("lib/left.ts", "export * from './base';\n");
    repo.write("lib/right.ts", "export * from './base';\n");
    repo.write("lib/base.ts", "export function shared() {}\n");
    repo.write("lib/other/base.ts", "export function shared() {}\n");
    repo.write(
        "app/use.ts",
        "import { shared } from '../lib';\nexport function go() { shared(); }\n",
    );

    let answer = repo.symbol("lib/base::shared");

    assert!(answer.contains("app/use.ts:2 go"), "{answer}");
    assert!(!repo.symbol("other/base::shared").contains("app/use.ts"));
}

// ---- namespace imports ---------------------------------------------------------------------------------

#[test]
fn a_call_on_a_namespace_import_reaches_the_function_or_arrow_it_names() {
    let mut repo = Repo::new();

    repo.write(
        "src/a.ts",
        "export function other() {}\nexport const arrow = () => 1;\nexport class Box {}\n",
    );
    repo.write("src/b.ts", "export function other() {}\n");
    repo.write(
        "src/main.ts",
        "import * as A from './a';\nexport function run() { A.other(); A.arrow(); new A.Box(); }\n",
    );

    let other = repo.symbol("src/a.ts::other");
    let arrow = repo.symbol("arrow");

    assert!(sites(&other).contains(&"src/main.ts:2 run"), "sure, not '?': {other}");
    assert!(sites(&arrow).contains(&"src/main.ts:2 run"), "{arrow}");
    assert!(!repo.symbol("src/b.ts::other").contains("src/main.ts"));
}

#[test]
fn a_namespace_call_follows_a_barrel_and_drops_what_the_module_lacks() {
    let mut repo = Repo::new();

    repo.write("src/api/index.ts", "export * from './http';\n");
    repo.write("src/api/http.ts", "export function get() {}\n");
    repo.write("src/other.ts", "export function missing() {}\n");
    repo.write(
        "src/main.ts",
        "import * as api from './api';\nexport function run() { api.get(); api.missing(); }\n",
    );

    assert!(repo.symbol("src/api/http.ts::get").contains("src/main.ts:2 run"));
    assert!(
        !repo.symbol("src/other.ts::missing").contains("src/main.ts"),
        "the namespace has no `missing`"
    );
}

// ---- imports no config explains ------------------------------------------------------------------------

#[test]
fn an_unmapped_alias_resolves_by_name_and_a_package_stays_external() {
    let mut repo = Repo::new();

    // no tsconfig at all: `@/` is an alias the index was never told about
    repo.write(
        "src/lib/utils.ts",
        "export function cn() {}\nexport function once() {}\n",
    );
    repo.write("src/lib/other.ts", "export function once() {}\n");
    repo.write(
        "src/ui/button.ts",
        "import { cn, once } from '@/lib/utils';\nimport { useQuery } from '@tanstack/react-query';\n\
         export function button() { cn(); once(); useQuery(); }\n",
    );

    let cn = repo.symbol("cn");
    let once = repo.symbol("utils.ts::once");

    assert_eq!(sites(&cn), ["src/ui/button.ts:3 button"], "{cn}");
    assert!(once.contains("src/ui/button.ts:3 button ?"), "two `once` fit: {once}");
    // `useQuery` is defined nowhere in the repository, so it was never an edge
    assert!(
        repo.symbol("useQuery")
            .starts_with("No symbol, file or command matches `useQuery`")
    );
}

#[test]
fn a_package_import_matched_to_a_repository_name_is_never_quite_certain() {
    let mut repo = Repo::new();

    repo.write("src/format.ts", "export function format() {}\n");
    repo.write(
        "src/page.ts",
        "import { format } from 'date-fns';\nexport function page() { format(); }\n",
    );

    let answer = repo.symbol("format");

    // by name alone, it may be the package's: listed, but marked
    assert!(sites(&answer).iter().all(|site| site.ends_with('?')), "{answer}");
}

// ---- the map -------------------------------------------------------------------------------------------

#[test]
fn a_focus_is_read_the_way_an_agent_writes_it() {
    let mut repo = Repo::new();

    repo.write("src/api/http.ts", "export function get() {}\n");
    repo.write(
        "src/ui/view.ts",
        "import { get } from '../api/http';\nexport function view() { get(); }\n",
    );
    repo.write("docs/readme.ts", "export function doc() {}\n");

    let root = repo.dir.path().canonicalize().expect("canonical");
    let name = root.file_name().expect("name").to_string_lossy().into_owned();
    let graph = repo.graph();
    let plain = graph.map(2000, Some("src/api"));

    for written in [
        "./src/api".to_string(),
        "src\\api".to_string(),
        format!("{}/src/api", root.display()),
        format!("{name}/src/api"),
        "  src/api  ".to_string(),
    ] {
        assert_eq!(graph.map(2000, Some(&written)), plain, "focus `{written}`");
    }

    assert!(plain.starts_with("src/api/http.ts"), "{plain}");
    assert_eq!(
        graph.map(2000, Some("nowhere/else")),
        "no indexed files under nowhere/else\n"
    );
}

#[test]
fn a_map_of_files_that_use_nothing_of_each_other_still_shows_their_definitions() {
    let mut repo = Repo::new();

    repo.write(
        "src/a.ts",
        "export const LIMIT = 3;\nexport function first() {}\nexport class Zed {}\n",
    );
    repo.write("src/b.rs", "pub fn lone() {}\n");

    let map = repo.graph().map(2000, None);

    assert!(map.contains("src/a.ts\n"), "{map}");
    assert!(map.contains("class Zed"), "types first: {map}");
    assert!(map.contains("function first"), "{map}");
    assert!(map.contains("fn lone"), "{map}");
    assert!(
        map.find("class Zed") < map.find("function first"),
        "types come before functions: {map}"
    );
}

// ---- the size of the graph -----------------------------------------------------------------------------

#[test]
fn a_constant_looking_name_no_constant_carries_is_not_kept_as_a_reference() {
    let mut repo = Repo::new();

    repo.write(
        "src/a.ts",
        "export const KNOWN = 1;\nexport function f(x) { return x === KNOWN || x === MISSING_ONE || x === MISSING_TWO; }\n",
    );

    // only `KNOWN` can resolve; the two others used to be stored and carried until the end
    assert_eq!(repo.graph().counts().refs, 1);
}

#[test]
fn an_aliased_import_of_a_constant_keeps_its_reference() {
    let mut repo = Repo::new();

    repo.write("src/limits.ts", "export const MAX_ROWS = 10;\n");
    repo.write(
        "src/use.ts",
        "import { MAX_ROWS as LIMIT } from './limits';\nexport function f(x) { return x > LIMIT; }\n",
    );

    assert!(repo.symbol("MAX_ROWS").contains("src/use.ts:2 f"));
}

#[test]
fn suggestions_compare_a_bounded_number_of_names_and_skip_names_far_longer_than_the_query() {
    let mut repo = Repo::new();
    let long = "x".repeat(200);
    let mut source = String::from("pub fn fetch_diffs() {}\npub fn get_user() {}\n");

    for n in 0..300 {
        source.push_str(&format!("pub fn get_user_{long}_{n}() {{}}\n"));
    }

    repo.write("a.rs", &source);

    let graph = repo.graph();
    // the names of a query this short that are far longer are no suggestion, whatever the cap
    let answer = graph.suggest("get_use");

    assert!(answer.contains("get_user"), "{answer}");
    assert!(!answer.contains(&long), "{answer}");

    // with room for only a few names, the shortest are the ones compared: the answer is the same every time
    let few = graph.suggest_among("fetch_diff", 3);

    assert!(few.contains("fetch_diffs"), "{few}");
    assert_eq!(few, graph.suggest_among("fetch_diff", 3));
}

#[test]
fn the_names_a_path_question_echoes_are_written_out_like_any_text_from_outside() {
    let mut repo = Repo::new();

    // a folder whose name holds a line break and an escape: the caller may name it in a qualifier
    repo.write(
        "we\nird\u{1b}/lib.rs",
        "pub fn first() { second() }\npub fn second() {}\n",
    );

    let graph = repo.graph();
    let forward = graph.path("we\nird\u{1b}::first", "second", 3);
    let missing = graph.path("we\nird\u{1b}::first", "we\nird\u{1b}::second", 1);
    let reverse = graph.path("second", "we\nird\u{1b}::first", 3);

    for answer in [forward, missing, reverse] {
        assert!(!answer.contains('\u{1b}'), "{answer:?}");
        assert!(
            answer.lines().all(|line| !line.starts_with("ird")),
            "a name forged a line: {answer:?}"
        );
    }
}

#[test]
fn the_estimate_of_a_graph_counts_the_text_of_its_names_not_only_how_many_there_are() {
    let source = |name: &dyn Fn(usize) -> String| -> String {
        (0..200)
            .map(|n| format!("pub fn {}() {{ {}(); }}\n", name(n), name(n + 1)))
            .collect()
    };
    let mut short = Repo::new();
    let mut long = Repo::new();

    short.write("a.rs", &source(&|n| format!("f{n}")));
    long.write("a.rs", &source(&|n| format!("{}{n}", "n".repeat(200))));

    let (short, long) = (short.graph().estimated_bytes(), long.graph().estimated_bytes());

    // 200 definitions and 200 references with 200 more characters each, kept several times over
    assert!(long > short + 200 * 200 * 3, "{short} {long}");
}

// ---- files and strings as the ends of a question, and the line of source under each hop -----------------

/// A small TypeScript app: a UI file calls an api function, which calls a store function.
fn app() -> Repo {
    let repo = Repo::new();

    repo.write(
        "src/ui/panel.ts",
        "import { save } from '../api';\n\nexport function onSave(name: string) {\n  save(name, \"secret-token\");\n}\n\nexport function onClose() {}\n",
    );
    repo.write(
        "src/api.ts",
        "import { store } from './store';\n\nexport function save(name: string, key: string) {\n  return store(name);\n}\n",
    );
    repo.write(
        "src/store.ts",
        "export function store(name: string) {\n  return name;\n}\n",
    );

    repo
}

#[test]
fn a_file_is_an_end_of_a_path_whatever_way_it_is_written_and_the_answer_says_what_it_took_it_for() {
    let mut repo = app();
    let root = repo.dir.path().canonicalize().expect("canonical");
    let absolute = root.join("src/ui/panel.ts").to_string_lossy().into_owned();
    let graph = repo.graph();

    for written in ["src/ui/panel.ts", "./src/ui/panel.ts", "panel.ts", absolute.as_str()] {
        let answer = graph.path(written, "store", 6);

        assert!(answer.contains(": 2 hops"), "{written}: {answer}");
        assert!(
            answer.contains("from: file src/ui/panel.ts: 2 definitions"),
            "{written}: {answer}"
        );
        assert!(answer.contains("    onSave  src/ui/panel.ts:3"), "{written}: {answer}");
    }

    // any definition of the file ends a chain
    let answer = graph.path("save", "src/store.ts", 6);

    assert!(answer.contains("to: file src/store.ts: 1 definition"), "{answer}");
    assert!(answer.contains("  → store  src/store.ts:1"), "{answer}");

    // a name that is a symbol stays a symbol
    assert!(!graph.path("onSave", "store", 6).contains("file "));
}

#[test]
fn every_hop_comes_with_the_line_of_source_that_makes_it_and_a_literal_on_it_is_blanked() {
    let mut repo = app();
    let answer = repo.graph().path("onSave", "store", 6);

    assert!(
        answer.contains("  → save  src/api.ts:3\n        at src/ui/panel.ts:4 | save(name, \"…\");\n"),
        "{answer}"
    );
    assert!(
        answer.contains("  → store  src/store.ts:1\n        at src/api.ts:4 | return store(name);\n"),
        "{answer}"
    );
    assert!(!answer.contains("secret-token"), "{answer}");
}

#[test]
fn the_line_of_a_site_is_left_out_when_the_file_changed_since_it_was_indexed_but_its_place_stays() {
    let mut repo = app();
    let panel = repo.dir.path().join("src/ui/panel.ts");
    let graph = repo.graph();

    assert!(graph.path("onSave", "save", 6).contains("at src/ui/panel.ts:4 | save("));

    // the graph in memory is the one of the old file: another size is another file
    std::fs::write(&panel, "// moved\n\n\n\nsave(name, 1);\n").expect("write");

    let answer = graph.path("onSave", "save", 6);

    assert!(answer.contains("        at src/ui/panel.ts:4\n"), "{answer}");
    assert!(!answer.contains("save(name"), "{answer}");
}

#[test]
fn the_line_of_a_site_is_left_out_when_it_may_sit_inside_a_string_and_cut_to_a_short_line() {
    let mut repo = Repo::new();
    let long = "x".repeat(300);

    repo.write(
        "src/a.ts",
        &format!(
            "export function target() {{}}\nexport function viaTemplate() {{\n  const text = `head ${{target()}} tail`;\n}}\nexport function viaLong() {{\n  target({long}, 1);\n}}\n"
        ),
    );
    repo.write(
        "app/a.py",
        "def target():\n    pass\n\n\ndef via_fstring():\n    return f\"\"\"head {target()} tail\"\"\"\n\n\ndef via_call():\n    return {\"k\": target()}\n\n\ndef plain():\n    target()\n",
    );

    let graph = repo.graph();
    let template = graph.path("viaTemplate", "src/a.ts::target", 3);
    let long = graph.path("viaLong", "src/a.ts::target", 3);
    let fstring = graph.path("via_fstring", "app/a.py::target", 3);
    let dict = graph.path("via_call", "app/a.py::target", 3);
    let plain = graph.path("plain", "app/a.py::target", 3);

    assert!(template.contains("        at src/a.ts:3\n"), "a template: {template}");
    assert!(fstring.contains("        at app/a.py:6\n"), "a triple quote: {fstring}");
    assert!(
        dict.contains("        at app/a.py:10\n"),
        "a brace before the name: {dict}"
    );
    assert!(plain.contains("        at app/a.py:14 | target()\n"), "{plain}");

    let line = long
        .lines()
        .find(|line| line.starts_with("        at "))
        .expect("a site");

    assert!(
        line.trim_start_matches("        at src/a.ts:6 | ").chars().count() <= 120,
        "{line}"
    );
    assert!(line.ends_with('…'), "{line}");
}

#[test]
fn the_top_of_a_file_is_a_start_and_the_chain_says_so() {
    let mut repo = Repo::new();

    repo.write(
        "src/job.ts",
        "export function run() {\n  finish();\n}\n\nfunction finish() {}\n",
    );
    repo.write("src/main.ts", "import { run } from './job';\n\nrun();\n");

    let graph = repo.graph();
    let answer = graph.path("src/main.ts", "finish", 4);

    assert!(answer.starts_with("`src/main.ts` → `finish`: 2 hops\n"), "{answer}");
    assert!(
        answer.contains("    <module>  src/main.ts\n  → run  src/job.ts:1\n        at src/main.ts:3 | run();\n"),
        "{answer}"
    );
    assert!(
        answer.contains("  → finish  src/job.ts:5\n        at src/job.ts:2 | finish();\n"),
        "{answer}"
    );
}

#[test]
fn a_string_that_names_no_symbol_is_an_end_through_the_definitions_that_write_it() {
    let mut repo = Repo::new();

    repo.write(
        "src/events.ts",
        "import { work } from './work';\n\nexport function listenOpened() {\n  listen('form_opened', work);\n}\n",
    );
    repo.write("src/work.ts", "export function work() {}\n");
    repo.write(
        "src-tauri/src/emit.rs",
        "pub fn announce() {\n    emit(\"form_opened\");\n}\n",
    );

    let graph = repo.graph();
    let from = graph.path("form_opened", "work", 4);

    assert!(from.contains(": 1 hop\n"), "{from}");
    assert!(
        from.contains("from: `form_opened` is a string, not a symbol: written in 2 places"),
        "{from}"
    );
    assert!(from.contains("    listenOpened  src/events.ts:3\n"), "{from}");

    // as the end of a chain, and as the subject of the other tools
    assert!(
        graph
            .path("work", "form_opened", 4)
            .contains("to: `form_opened` is a string"),
        "{from}"
    );
    assert!(graph.symbol("form_opened", 5).contains("fn listenOpened"));
    assert!(graph.impact("form_opened", 2).contains("`form_opened`"));
}

#[test]
fn a_symbol_that_several_definitions_share_says_which_ones_a_path_used() {
    let mut repo = Repo::new();

    for name in ["one", "two", "three"] {
        repo.write(
            &format!("src/{name}.ts"),
            &format!("export function same() {{}}\nexport function {name}() {{ same(); }}\n"),
        );
    }

    let answer = repo.graph().path("same", "one", 3);

    assert!(
        answer.contains("from: `same` matches 3 definitions, used: same src/"),
        "{answer}"
    );
}

#[test]
fn symbol_and_impact_take_a_file_and_list_its_most_used_definitions_first() {
    let mut repo = app();
    let graph = repo.graph();
    let symbol = graph.symbol("src/api.ts", 20);

    assert!(
        symbol.starts_with("file src/api.ts: 1 definition\nfn save  src/api.ts:3-5\n"),
        "{symbol}"
    );
    assert!(
        symbol.contains("    src/ui/panel.ts:4 onSave | save(name, \"…\");"),
        "{symbol}"
    );

    let panel = graph.symbol("src/ui/panel.ts", 20);

    assert!(panel.starts_with("file src/ui/panel.ts: 2 definitions\n"), "{panel}");

    let impact = graph.impact("src/store.ts", 3);

    assert!(impact.contains("file src/store.ts: 1 definition"), "{impact}");
    assert!(impact.contains("src/ui/panel.ts"), "{impact}");
}

#[test]
fn text_that_names_nothing_gets_suggestions_as_before() {
    let mut repo = app();
    let graph = repo.graph();

    for answer in [
        graph.path("src/nowhere.ts", "store", 3),
        graph.path("store", "no_such_string", 3),
        graph.symbol("src/nowhere.ts", 3),
        graph.impact("no_such_string", 3),
    ] {
        assert!(answer.starts_with("No symbol, file or command matches `"), "{answer}");
    }

    // with names that are alike, those are suggested
    assert!(
        graph
            .symbol("sav", 3)
            .starts_with("no definition named `sav`. Similar: ")
    );
}

// ---- words as the ends of a question ----------------------------------------------------------------------

/// A doctor command whose chain ends in a probe that runs `harness-memory features`, and a Rust and a TypeScript
/// definition that hold the word `lines` in their names.
fn doctor_repo() -> Repo {
    let repo = Repo::new();

    repo.write(
        "src/command.ts",
        "import { runDoctorSetup } from './setup';\n\nexport function runCommand(argv: string[]) {\n  return argv[0] === \"doctor\" ? runDoctorSetup() : 0;\n}\n",
    );
    repo.write(
        "src/setup.ts",
        "export function runDoctorSetup() {\n  return codeGraphStep();\n}\n\nfunction codeGraphStep() {\n  return probeCodeGraph();\n}\n\nexport function probeCodeGraph() {\n  return execute(\"harness-memory\", [\"features\"]);\n}\n",
    );
    repo.write(
        "src/setup.test.ts",
        "export function doctorFixture() {\n  return \"doctor\";\n}\n",
    );
    repo.write(
        "src/diff.ts",
        "import { invoke } from './ipc';\n\nexport function loadLines(path: string) {\n  return invoke(\"git_file_lines\", { path });\n}\n",
    );
    repo.write(
        "src/ipc.ts",
        "export function invoke(name: string, args: object) {\n  return name;\n}\n",
    );
    repo.write(
        "src-tauri/src/git.rs",
        "pub fn git_file_lines(path: &str) -> usize {\n    path.len()\n}\n",
    );

    repo
}

#[test]
fn words_that_name_no_symbol_stand_for_the_definitions_that_hold_them_and_the_answer_says_so() {
    let mut repo = doctor_repo();
    let answer = repo.graph().path("/useful-skills doctor", "harness-memory features", 6);

    assert!(answer.contains(": 3 hops\n"), "{answer}");
    assert!(
        answer.contains("from: words `doctor` → runCommand src/command.ts:3, runDoctorSetup src/setup.ts:1"),
        "{answer}"
    );
    assert!(answer.contains("(no match: `useful-skills`)"), "{answer}");
    assert!(
        answer.contains("to: words `harness-memory`, `features` → probeCodeGraph src/setup.ts:9\n"),
        "{answer}"
    );

    // the chain starts at the command the question names, not at a helper that is nearer
    assert!(answer.contains("    runCommand  src/command.ts:3\n  → runDoctorSetup  src/setup.ts:1\n"));
    assert!(answer.contains("        at src/command.ts:4 | return argv[0] === \"…\" ? runDoctorSetup() : 0;\n"));
    assert!(
        answer.contains("  → probeCodeGraph  src/setup.ts:9\n        at src/setup.ts:6 | return probeCodeGraph();\n")
    );
}

#[test]
fn a_question_is_read_by_its_content_words_against_names_in_any_case_and_form() {
    let mut repo = doctor_repo();
    let graph = repo.graph();
    let answer = graph.path("the diff view loads the lines of a changed file", "git-file-lines", 4);

    // `lines` is a part of `loadLines` and `git_file_lines`, and `git-file-lines` is the other name of the latter
    assert!(answer.contains(": 1 hop\n"), "{answer}");
    assert!(
        answer.contains("from: words `lines` → loadLines src/diff.ts:3, git_file_lines src-tauri/src/git.rs:1 (no match: `diff`, `view`, `loads`)"),
        "{answer}"
    );
    assert!(
        answer.contains("to: words `git-file-lines` → git_file_lines src-tauri/src/git.rs:1\n"),
        "{answer}"
    );
    assert!(
        answer.contains("  → git_file_lines (by name)  src-tauri/src/git.rs:1\n"),
        "{answer}"
    );

    let exact = graph.symbol("RUNDOCTORSETUP", 5);

    assert!(exact.contains("fn runDoctorSetup  src/setup.ts:1"), "{exact}");
    assert!(graph.impact("doctor", 2).contains("words `doctor` →"));
}

#[test]
fn definitions_that_match_more_words_come_first_and_tests_after_production() {
    let mut repo = doctor_repo();
    let note = repo.graph().path("doctor setup", "probeCodeGraph", 6);

    // `setup` and `doctor` are both parts of runDoctorSetup
    assert!(
        note.contains("from: words `doctor`, `setup` → runDoctorSetup src/setup.ts:1"),
        "{note}"
    );

    let one = repo.graph().symbol("doctor", 5);
    let first = one.find("runDoctorSetup").expect("runDoctorSetup");
    let fixture = one.find("doctorFixture").expect("doctorFixture");

    assert!(first < fixture, "{one}");
}

#[test]
fn text_that_matches_nothing_says_what_to_do_and_keeps_suggestions_when_there_are_some() {
    let mut repo = doctor_repo();
    let graph = repo.graph();
    let none = graph.path("zzqq nothing here", "probeCodeGraph", 4);

    assert_eq!(
        none,
        "No symbol, file or command matches `zzqq nothing here`. Grep once for that text to find the function that handles it, then call code_path from that function.\n"
    );
    assert!(
        graph
            .symbol("zzqq", 3)
            .starts_with("No symbol, file or command matches `zzqq`.")
    );
    assert!(
        graph
            .path("probeCodeGraph", "yyqq", 4)
            .starts_with("No symbol, file or command matches `yyqq`.")
    );

    let typo = graph.symbol("runDoctorSetp", 3);

    assert!(
        typo.starts_with("no definition named `runDoctorSetp`. Similar: runDoctorSetup"),
        "{typo}"
    );
}

#[test]
fn a_symbol_a_file_or_a_string_keeps_its_reading_when_words_could_also_match() {
    let mut repo = doctor_repo();
    let graph = repo.graph();

    assert!(
        graph
            .path("runCommand", "probeCodeGraph", 6)
            .contains("`runCommand` → `probeCodeGraph`: 3 hops\n")
    );
    assert!(
        graph
            .path("src/command.ts", "probeCodeGraph", 6)
            .contains("from: file src/command.ts")
    );
}

// ---- functions passed through objects ----------------------------------------------------------------------

/// Two runners that call `host.run()`, each given its own function by the code that makes it.
fn runners() -> Repo {
    let repo = Repo::new();

    repo.write(
        "src/runner.js",
        "export function makeA(host) {\n  return () => host.run(1);\n}\n\
         export function makeB(host) {\n  return () => host?.run(2);\n}\n",
    );
    repo.write("src/fns.js", "export function fnA() {}\nexport function fnB() {}\n");
    repo.write(
        "src/main.js",
        "import { makeA, makeB } from './runner.js';\nimport { fnA, fnB } from './fns.js';\n\
         export const a = makeA({ run: fnA });\nexport const b = makeB({ run: fnB });\n",
    );

    repo
}

#[test]
fn a_call_through_a_property_means_the_function_put_there_by_the_code_that_makes_the_runner() {
    let mut repo = runners();

    let a = repo.symbol("fnA");
    let b = repo.symbol("fnB");

    assert_eq!(sites(&a), ["src/runner.js:2 makeA ?"], "{a}");
    assert_eq!(sites(&b), ["src/runner.js:5 makeB ?"], "{b}");

    let chain = repo.graph().path("a", "fnA", 4);

    assert!(
        chain.contains("→ makeA") && chain.contains("→ fnA"),
        "followed by code_path: {chain}"
    );
    assert!(chain.contains("at src/runner.js:2 | return () => host.run("), "{chain}");
}

#[test]
fn a_property_put_into_by_other_code_means_it_by_its_name_when_few_functions_are_put_there() {
    let mut repo = Repo::new();

    repo.write(
        "src/use.js",
        "export function go(options) {\n  return options.done(1);\n}\n",
    );
    repo.write(
        "src/fill.js",
        "function finish() {}\nfunction stop() {}\nexport const settings = { done: finish };\n\
         export const more = { done: stop.bind(null) };\n",
    );

    let finish = repo.symbol("finish");

    assert_eq!(sites(&finish), ["src/use.js:2 go ?"], "{finish}");
    assert!(sites(&repo.symbol("stop")).contains(&"src/use.js:2 go ?"));
}

#[test]
fn a_property_that_five_functions_are_put_into_is_too_ambiguous_to_mean_any_of_them() {
    let mut repo = Repo::new();

    repo.write("src/use.js", "export function go(o) { return o.done(1); }\n");

    for n in 0..5 {
        repo.write(
            &format!("src/f{n}.js"),
            &format!("function fn{n}() {{}}\nexport const s{n} = {{ done: fn{n} }};\n"),
        );
    }

    for n in 0..5 {
        let answer = repo.symbol(&format!("fn{n}"));

        assert!(!answer.contains("src/use.js"), "{answer}");
    }
}

#[test]
fn values_of_every_written_shape_are_functions_put_in_the_property() {
    let mut repo = Repo::new();

    repo.write("src/use.js", "export function go(o) { return o.step(1); }\n");
    repo.write(
        "src/fill.js",
        "import * as helpers from './helpers.js';\nfunction viaDefault() {}\nfunction viaCondition() {}\n\
         function viaShorthand() {}\n\
         export const one = { step: (globalThis.x ?? viaDefault) };\n\
         export const two = { step: flag ? viaCondition : undefined };\n\
         export const three = { step: helpers.fromNamespace };\n\
         export const four = { viaShorthand };\n",
    );
    repo.write("src/helpers.js", "export function fromNamespace() {}\n");

    let step = repo.symbol("viaDefault");

    assert_eq!(sites(&step), ["src/use.js:1 go ?"], "{step}");
    assert_eq!(sites(&repo.symbol("viaCondition")), ["src/use.js:1 go ?"]);
    assert_eq!(sites(&repo.symbol("fromNamespace")), ["src/use.js:1 go ?"]);

    // `{ viaShorthand }` is the property `viaShorthand`, not `step`
    assert!(sites(&repo.symbol("viaShorthand")).is_empty());
}

#[test]
fn a_test_double_put_in_a_property_is_never_what_production_code_calls() {
    let mut repo = Repo::new();

    repo.write("src/use.js", "export function go(o) { return o.probe(1); }\n");
    repo.write(
        "src/real.js",
        "export function realProbe() {}\nexport const real = { probe: realProbe };\n",
    );
    repo.write(
        "test/fake.test.js",
        "function fakeProbe() {}\nexport const fake = { probe: fakeProbe };\n",
    );

    assert_eq!(sites(&repo.symbol("realProbe")), ["src/use.js:1 go ?"]);
    assert!(!repo.symbol("fakeProbe").contains("src/use.js"));
}

#[test]
fn a_receiver_that_is_a_class_or_a_module_of_the_repository_is_not_a_property_holder() {
    let mut repo = Repo::new();

    repo.write(
        "src/use.ts",
        "import { Tool } from './tool';\nexport class Host {}\n\
         export function go(h: Host) { h.run(1); Tool.run(2); }\n",
    );
    repo.write("src/tool.ts", "export class Tool {}\n");
    repo.write("src/fill.ts", "function runIt() {}\nexport const s = { run: runIt };\n");

    // `Host` has no `run` and the literal is not passed to anything around the call; `Tool` is an import
    assert!(!repo.symbol("runIt").contains("src/use.ts"));
}

#[test]
fn a_field_of_a_class_that_a_literal_fills_when_the_class_is_made_is_what_a_call_on_it_means() {
    let mut repo = Repo::new();

    repo.write(
        "src/worker.ts",
        "export class Worker {\n  constructor(private options: Options) {}\n  go() {\n    return this.options.done(1);\n  }\n}\n\
         interface Options { done: unknown }\n",
    );
    repo.write(
        "src/main.ts",
        "import { Worker } from './worker';\nfunction finish() {}\nexport const w = new Worker({ done: finish });\n",
    );

    let finish = repo.symbol("finish");

    assert_eq!(sites(&finish), ["src/worker.ts:4 Worker::go ?"], "{finish}");
}

#[test]
fn a_property_name_that_many_calls_carry_is_settled_only_by_the_code_that_passes_the_literal() {
    let mut repo = Repo::new();
    let calls: String = (0..13).map(|n| format!("  o.done({n});\n")).collect();

    repo.write("src/use.js", &format!("export function go(o) {{\n{calls}}}\n"));
    repo.write(
        "src/fill.js",
        "function finish() {}\nexport const settings = { done: finish };\n",
    );
    // the literal is passed to the function that makes the call: that is enough, however common the name
    repo.write("src/own.js", "export function again(o) { return o.done(1); }\n");
    repo.write(
        "src/pass.js",
        "import { again } from './own.js';\nfunction finishAgain() {}\nagain({ done: finishAgain });\n",
    );

    assert!(!repo.symbol("finish").contains("src/use.js"));
    assert_eq!(sites(&repo.symbol("finishAgain")), ["src/own.js:1 again ?"]);
}

#[test]
fn a_call_in_another_language_or_on_a_type_of_the_standard_library_is_never_a_property() {
    let mut repo = Repo::new();

    repo.write(
        "src/fill.js",
        "function finish() {}\nexport const settings = { done: finish };\n",
    );
    repo.write("src/other.rs", "pub fn go() { let o = make(); o.done(1); }\n");
    repo.write(
        "src/map.ts",
        "export function go(m: Map<string, number>) { return m.done(1); }\n",
    );

    assert!(!repo.symbol("finish").contains("src/other.rs"));
    assert!(!repo.symbol("finish").contains("src/map.ts"));
}

/// The `used by` lines of a symbol answer, with their source text.
fn users(answer: &str) -> Vec<&str> {
    answer
        .lines()
        .filter(|line| line.starts_with("    src/") || line.starts_with("    app/"))
        .collect()
}

#[test]
fn a_comment_or_the_rest_of_a_string_never_reaches_the_line_of_a_site() {
    let mut repo = Repo::new();

    repo.write(
        "src/a.rs",
        concat!(
            "pub fn target_fn(s: &str) {}\n",
            "pub fn c1() {\n    target_fn(\"x\"); // prod password is COMMENTSECRET3\n}\n",
            "pub fn c2() {\n    /* api key COMMENTBLOCK4 */ target_fn(\"y\");\n}\n",
            "pub fn c3() {\n    /* \" */ let k = \"RUSTQUOTESECRET5\"; target_fn(k);\n}\n",
            "pub fn c4() {\n    let s = (\"first RUSTMULTISECRET6\n    second RUSTMULTISECRET7\", target_fn(\"z\"));\n}\n",
            "pub fn c5() {\n    let r = (r#\"first RAWSECRET8\n    tail RAWSECRET9\"#, target_fn(\"w\"));\n}\n",
            "pub fn c6() {\n    /* a long note RUSTNOTE10\n       goes on RUSTNOTE11 */ target_fn(\"v\");\n}\n",
        ),
    );
    repo.write(
        "src/b.ts",
        concat!(
            "export function tsTarget(a: string) { return a; }\n",
            "export function t1() {\n  return tsTarget(\"x\") // DB_PASSWORD=TSCOMMENTSECRET6\n}\n",
            "export function t2() {\n  const s = \"line one TSMULTISECRET1 \\\n  line two TSMULTISECRET2\"; tsTarget(s);\n}\n",
            "export function t3() {\n  /* it's */ const k = 'TSQUOTESECRET3'; tsTarget(k);\n}\n",
        ),
    );
    repo.write(
        "app/c.py",
        concat!(
            "def py_target(a): return a\n\n\n",
            "def p1():\n    py_target(\"x\")  # PYCOMMENTSECRET1\n\n\n",
            "def p2():\n    s = \"line one PYMULTISECRET2 \\\n    line two PYMULTISECRET3\"; py_target(s)\n",
        ),
    );

    let graph = repo.graph();
    let answers = [
        graph.symbol("target_fn", 20),
        graph.symbol("tsTarget", 20),
        graph.symbol("py_target", 20),
    ];

    for answer in &answers {
        for secret in ["SECRET", "COMMENTBLOCK", "NOTE1"] {
            assert!(!answer.contains(secret), "{secret} in {answer}");
        }
    }

    let rust = users(&answers[0]).join("\n");

    assert!(rust.contains("src/a.rs:3 c1 | target_fn(\"…\"); // …"), "{rust}");
    assert!(rust.contains("src/a.rs:6 c2 | /* … */ target_fn(\"…\");"), "{rust}");
    assert!(
        rust.contains("src/a.rs:9 c3 | /* … */ let k = \"…\"; target_fn(k);"),
        "{rust}"
    );
    // the end of a string or of a comment that began above is not told from code: the place only
    assert!(rust.lines().any(|line| line.ends_with("src/a.rs:13 c4")), "{rust}");
    assert!(rust.lines().any(|line| line.ends_with("src/a.rs:17 c5")), "{rust}");
    assert!(rust.contains("src/a.rs:21 c6 | /* … */ target_fn(\"…\");"), "{rust}");

    let ts = users(&answers[1]).join("\n");

    assert!(ts.contains("src/b.ts:3 t1 | return tsTarget(\"…\") // …"), "{ts}");
    assert!(ts.lines().any(|line| line.ends_with("src/b.ts:7 t2")), "{ts}");
    assert!(
        ts.contains("src/b.ts:10 t3 | /* … */ const k = \"…\"; tsTarget(k);"),
        "{ts}"
    );

    let python = users(&answers[2]).join("\n");

    assert!(python.contains("app/c.py:5 p1 | py_target(\"…\") # …"), "{python}");
    assert!(python.lines().any(|line| line.ends_with("app/c.py:10 p2")), "{python}");
}

#[test]
fn a_site_is_read_only_when_its_line_is_shown() {
    let mut repo = Repo::new();
    let mut source = String::from("pub fn target_fn() {}\n");

    for n in 0..14 {
        source.push_str(&format!("pub fn user{n:02}() {{\n    target_fn(); // note{n}\n}}\n"));
    }

    repo.write("src/a.rs", &source);

    let answer = repo.symbol("target_fn");
    let shown = answer.lines().filter(|line| line.contains(" | target_fn();")).count();

    assert_eq!(shown, super::render::USERS_WITH_TEXT, "{answer}");
    assert!(
        answer.lines().filter(|line| line.contains("src/a.rs:")).count() > shown,
        "{answer}"
    );
}
