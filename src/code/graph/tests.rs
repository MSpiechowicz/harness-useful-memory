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

#[test]
fn a_map_ranks_what_the_code_depends_on_and_leaves_out_copies_and_tooling_last() {
    let mut repo = Repo::new();

    // `helper` is used by three tests only, `core` by the app: the tests do not make helper central
    repo.write("src/helper.ts", "export function helper() {\n  return 1;\n}\n");
    repo.write("src/core.ts", "export function core() {\n  return 2;\n}\n");
    repo.write(
        "src/app.ts",
        "import { core } from './core';\n\nexport function app() {\n  return core();\n}\n",
    );

    for name in ["one", "two", "three"] {
        repo.write(
            &format!("test/{name}.test.ts"),
            "import { helper } from '../src/helper';\n\nexport function check() {\n  return helper();\n}\n",
        );
    }

    // a generated copy of core and a build script that uses core
    repo.write("plugins/web/src/core.ts", "export function core() {\n  return 2;\n}\n");
    repo.write(
        "scripts/build.ts",
        "import { core } from '../src/core';\n\nexport function build() {\n  return core();\n}\n",
    );

    let map = repo.graph().map(4000, None);
    let core = map.find("src/core.ts\n").expect("core");
    let helper = map.find("src/helper.ts\n").expect("helper");
    let script = map.find("scripts/build.ts\n").expect("build");

    assert!(core < helper, "{map}");
    assert!(helper < script, "the code before its tooling: {map}");
    assert!(!map.contains("plugins/web/src/core.ts\n"), "{map}");
    assert!(
        map.contains("(1 copy of other files left out, such as plugins/web/src/core.ts)\n"),
        "{map}"
    );
}

#[test]
fn a_map_focused_on_tooling_follows_what_it_uses_and_empty_files_are_no_copies() {
    let mut repo = Repo::new();

    repo.write("src/core.ts", "export function core() {\n  return 2;\n}\n");
    // alpha comes first by path: only the script's use can put core before it
    repo.write("src/alpha.ts", "export function alpha() {\n  return 3;\n}\n");
    repo.write(
        "scripts/build.ts",
        "import { core } from '../src/core';\n\nexport function build() {\n  return core();\n}\n",
    );
    repo.write("app/utils/__init__.py", "");
    repo.write("utils/__init__.py", "");

    // the script's use of core counts under its own focus: core leads the code
    let map = repo.graph().map(4000, Some("scripts"));
    let core = map.find("src/core.ts\n").expect("core");
    let alpha = map.find("src/alpha.ts\n").expect("alpha");

    assert!(core < alpha, "{map}");
    assert!(!map.contains("left out"), "two empty files are no copies: {map}");
}

#[test]
fn a_bundle_built_from_the_sources_is_not_indexed() {
    let mut repo = Repo::new();

    repo.write("src/core.ts", "export function core() {\n  return 2;\n}\n");
    repo.write(
        "src/app.ts",
        "import { core } from './core';\n\nexport function app() {\n  return core();\n}\n",
    );
    // what esbuild or bun writes when the sources are bundled into one committed file
    repo.write(
        "extension.js",
        "var __create = Object.create;\nvar __defProp = Object.defineProperty;\nvar __commonJS = (cb, mod) => cb;\n\nfunction core() {\n  return 2;\n}\nfunction app() {\n  return core();\n}\n",
    );

    let graph = repo.graph();
    let symbol = graph.symbol("core", 5);

    assert!(symbol.contains("src/core.ts"), "{symbol}");
    assert!(!symbol.contains("extension.js"), "{symbol}");
    assert!(
        !graph.map(4000, None).contains("extension.js"),
        "the map leaves the bundle out"
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
fn a_lowercase_constant_an_astro_page_imports_is_used_by_the_page() {
    let mut repo = Repo::new();

    repo.write(
        "src/data/site.ts",
        "export const siteContent: Record<string, string> = { en: 'a' };\n",
    );
    repo.write(
        "src/pages/index.astro",
        "---\nimport { siteContent } from \"../data/site\";\n\nconst content = siteContent.en;\n---\n<h1>{content}</h1>\n",
    );

    let answer = repo.symbol("siteContent");

    assert_eq!(sites(&answer), vec!["src/pages/index.astro:4 index"], "{answer}");
}

#[test]
fn a_function_an_astro_template_calls_is_used_by_the_component_at_the_template_line() {
    let mut repo = Repo::new();

    repo.write(
        "src/i18n.ts",
        "export function alternatePath(locale: string) {\n  return `/${locale}/`;\n}\n",
    );
    repo.write(
        "src/components/SiteHeader.astro",
        "---\nimport { alternatePath } from \"../i18n\";\nconst { locale } = Astro.props;\n---\n<header>\n  <a href={alternatePath(locale)}>other language</a>\n  <p>alternatePath is not shown {locale}</p>\n</header>\n",
    );

    let answer = repo.symbol("alternatePath");

    assert_eq!(
        sites(&answer),
        vec!["src/components/SiteHeader.astro:6 SiteHeader"],
        "{answer}"
    );
}

#[test]
fn a_function_a_svelte_template_calls_is_used_by_the_component() {
    let mut repo = Repo::new();

    repo.write(
        "src/format.ts",
        "export function format(x: number) {\n  return `${x}`;\n}\n",
    );
    repo.write(
        "src/Row.svelte",
        "<script lang=\"ts\">\n  import { format } from './format';\n  export let x = 1;\n</script>\n\n<p>{format(x)}</p>\n",
    );

    let answer = repo.symbol("format");

    assert_eq!(sites(&answer), vec!["src/Row.svelte:6 Row"], "{answer}");
}

#[test]
fn functions_a_vue_template_calls_in_text_and_directives_are_used_by_the_component() {
    let mut repo = Repo::new();

    repo.write(
        "src/format.ts",
        "export function format(x: number) {\n  return `${x}`;\n}\nexport function label(x: number) {\n  return `${x}`;\n}\n",
    );
    repo.write(
        "src/Row.vue",
        "<script setup lang=\"ts\">\nimport { format, label } from './format';\n</script>\n\n<template>\n  <p :title=\"label(x)\">\n    {{ format(x) }}\n  </p>\n</template>\n",
    );

    let format = repo.symbol("format");
    let label = repo.symbol("label");

    assert_eq!(sites(&format), vec!["src/Row.vue:7 Row"], "{format}");
    assert_eq!(sites(&label), vec!["src/Row.vue:6 Row"], "{label}");
}

#[test]
fn a_name_a_component_does_not_import_is_not_used_by_its_template() {
    let mut repo = Repo::new();

    repo.write(
        "src/format.ts",
        "export function format(x: number) {\n  return `${x}`;\n}\n",
    );
    repo.write("src/other.ts", "export function other() {\n  return 1;\n}\n");
    repo.write(
        "src/Row.svelte",
        "<script>\n  import { other } from './other';\n</script>\n\n<p>{format(1)} {other()}</p>\n<p>format(2)</p>\n<p>{'format(3)'}</p>\n",
    );
    repo.write(
        "src/Row.vue",
        "<script setup>\nimport { other } from './other';\n</script>\n\n<template>\n  <p :title=\"format(1)\">format(2) {{ other() }} {{ 'format(3)' }}</p>\n</template>\n",
    );

    let format = repo.symbol("format");
    let other = repo.symbol("other");

    assert!(sites(&format).is_empty(), "{format}");
    assert_eq!(
        sites(&other),
        vec!["src/Row.svelte:5 Row", "src/Row.vue:6 Row"],
        "{other}"
    );
}

#[test]
fn a_lowercase_constant_is_used_where_it_is_imported_as_a_value_an_alias_or_a_namespace() {
    let mut repo = Repo::new();

    repo.write(
        "src/data/site.ts",
        "export const siteContent = { en: 'a' };\nexport const fallback = { en: 'b' };\n",
    );
    repo.write(
        "src/plain.ts",
        "import { siteContent } from './data/site';\nexport function read() { return siteContent.en; }\n",
    );
    repo.write(
        "src/aliased.ts",
        "import { siteContent as content } from './data/site';\nexport function pick() { return content.en; }\n",
    );
    repo.write(
        "src/spread.ts",
        "import { siteContent } from './data/site';\nexport function all() { return { ...siteContent, siteContent }; }\n",
    );
    repo.write(
        "src/namespaced.ts",
        "import * as data from './data/site';\nexport function other() { return data.fallback; }\n",
    );

    let site = repo.symbol("siteContent");

    assert!(site.contains("src/plain.ts:2 read"), "{site}");
    assert!(site.contains("src/aliased.ts:2 pick"), "{site}");
    assert!(site.contains("src/spread.ts:2 all"), "{site}");
    assert!(!site.contains("src/namespaced.ts"), "{site}");
    assert!(
        repo.symbol("fallback").contains("src/namespaced.ts:2 other"),
        "a member of a namespace import is a use of that export"
    );
}

#[test]
fn a_lowercase_name_that_is_not_imported_is_no_reference() {
    let mut repo = Repo::new();

    repo.write("src/data/site.ts", "export const siteContent = { en: 'a' };\n");
    repo.write(
        "src/local.ts",
        "export function read(siteContent2) { const siteContent = { en: 'b' }; return siteContent.en + siteContent2; }\n",
    );

    let answer = repo.symbol("siteContent");

    assert!(sites(&answer).is_empty(), "{answer}");
}

#[test]
fn an_imported_lowercase_constant_resolves_to_the_module_it_is_imported_from() {
    let mut repo = Repo::new();

    repo.write("src/data/site.ts", "export const siteContent = { en: 'a' };\n");
    repo.write("src/other/site.ts", "export const siteContent = { en: 'b' };\n");
    repo.write(
        "src/page.ts",
        "import { siteContent } from './data/site';\nexport function read() { return siteContent.en; }\n",
    );

    let answer = repo.symbol("siteContent");
    let sections: Vec<&str> = answer.split("const siteContent ").skip(1).collect();
    let used = |path: &str| {
        sections
            .iter()
            .find(|section| section.trim_start().starts_with(path))
            .is_some_and(|section| section.contains("src/page.ts:2 read"))
    };

    assert_eq!(sections.len(), 2, "{answer}");
    assert!(used("src/data/site.ts"), "{answer}");
    assert!(!used("src/other/site.ts"), "{answer}");
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
fn words_that_one_definition_alone_holds_are_its_end_and_the_chain_says_it_was_guessed() {
    let mut repo = doctor_repo();
    let answer = repo.graph().path("doctor setup", "harness-memory features", 6);

    assert!(answer.contains(": 2 hops\n"), "{answer}");
    assert!(
        answer.contains("from: words `doctor`, `setup` → runDoctorSetup src/setup.ts:1\n"),
        "{answer}"
    );
    assert!(
        answer.contains("to: words `harness-memory`, `features` → probeCodeGraph src/setup.ts:9\n"),
        "{answer}"
    );
    assert!(
        answer.contains("endpoints guessed from words: check the first and last hop\n"),
        "{answer}"
    );
    assert!(answer.contains("    runDoctorSetup  src/setup.ts:1\n  → codeGraphStep  src/setup.ts:5\n"));
    assert!(
        answer.contains("  → probeCodeGraph  src/setup.ts:9\n        at src/setup.ts:6 | return probeCodeGraph();\n"),
        "{answer}"
    );

    // a name is no guess
    let plain = repo.graph().path("runDoctorSetup", "probeCodeGraph", 6);

    assert!(!plain.contains("guessed"), "{plain}");
}

#[test]
fn words_that_several_definitions_hold_get_candidates_to_name_and_a_word_nobody_holds_is_not_counted() {
    let mut repo = doctor_repo();
    let answer = repo.graph().path("/useful-skills doctor", "harness-memory features", 6);

    // nothing holds `useful-skills`, so it is not counted: the definition that carries `doctor` in its name leads
    // the three that hold it, and the answer says which word found nothing
    assert!(answer.contains(": 2 hops\n"), "{answer}");
    assert!(
        answer.contains("from: words `doctor` → runDoctorSetup src/setup.ts:1 (no match: `useful-skills`)\n"),
        "{answer}"
    );
    assert!(
        answer.contains("endpoints guessed from words: check the first and last hop\n"),
        "{answer}"
    );

    // two definitions that hold every word tie: both are listed
    let mut shop = Repo::new();

    shop.write(
        "src/billing.ts",
        "export function renderInvoice(n: number) {\n  return formatTotal(n);\n}\n\nexport function renderReceipt(n: number) {\n  return formatTotal(n);\n}\n\nexport function formatTotal(n: number) {\n  return n;\n}\n",
    );

    let tie = shop.graph().path("render", "formatTotal", 6);

    assert!(
        tie.contains("several definitions hold all of render. Closest:"),
        "{tie}"
    );
    assert!(tie.contains("renderInvoice src/billing.ts:1  (render)"), "{tie}");
    assert!(tie.contains("renderReceipt src/billing.ts:5  (render)"), "{tie}");
    assert!(!tie.contains("→"), "{tie}");

    // the same name in two files is one end, as a name is
    shop.write(
        "src/other.ts",
        "export function formatTotal(n: number) {\n  return n + 1;\n}\n",
    );

    let same = shop.graph().path("renderInvoice", "format total", 6);

    assert!(
        same.contains("to: words `format`, `total` → formatTotal src/billing.ts:9, formatTotal src/other.ts:1"),
        "{same}"
    );
    assert!(same.contains(": 1 hop\n"), "{same}");
}

#[test]
fn a_word_held_by_thousands_of_definitions_is_not_counted_and_the_rare_word_leads() {
    let mut repo = Repo::new();
    let mut items = String::new();

    // `get` is part of three thousand names: its weight used to underflow past 2048 holders
    for n in 0..3_000 {
        items.push_str(&format!("export function get_item{n}() {{\n  return {n};\n}}\n"));
    }

    repo.write("src/items.ts", &items);
    repo.write(
        "src/widget.ts",
        "export function make_widget() {\n  return 1;\n}\n\nexport function get_widget() {\n  return make_widget();\n}\n",
    );

    let answer = repo.graph().path("get widget", "make_widget", 6);

    // `get` is too common to count, `widget` is held by two definitions and get_widget holds both words
    assert!(
        answer.contains("from: words `get`, `widget` → get_widget src/widget.ts:5\n"),
        "{answer}"
    );
    assert!(answer.contains(": 1 hop\n"), "{answer}");

    // a common word alone is still an end: it is all there is to go by
    let common = repo.graph().impact("get", 1);

    assert!(common.contains("words `get` →"), "{common}");
}

#[test]
fn request_is_a_word_of_a_question_and_a_folder_of_the_file_counts_for_the_definition() {
    let mut repo = Repo::new();

    repo.write(
        "src/lib/api/request.ts",
        "export function request(url: string) {\n  return url;\n}\n",
    );
    repo.write(
        "src/net/sender.ts",
        "export function sendRequest(url: string) {\n  return url;\n}\n",
    );
    repo.write(
        "src/client.ts",
        "import { request } from './lib/api/request';\n\nexport function load() {\n  return request('x');\n}\n",
    );

    let answer = repo.graph().path("load", "api request", 4);

    assert!(
        answer.contains("to: words `api`, `request` → request src/lib/api/request.ts:1\n"),
        "{answer}"
    );
    assert!(answer.contains(": 1 hop\n"), "{answer}");

    // a word that no definition holds does not veto the others
    let unheld = repo.graph().path("load", "api request quux", 4);

    assert!(
        unheld.contains("to: words `api`, `request` → request src/lib/api/request.ts:1 (no match: `quux`)\n"),
        "{unheld}"
    );

    // the singular finds the plural folder
    let plural = repo.graph().path("load", "request lib apis", 4);

    assert!(plural.contains("→ request src/lib/api/request.ts:1"), "{plural}");
}

#[test]
fn a_word_that_names_the_file_of_a_definition_counts_for_it() {
    let mut repo = doctor_repo();
    let answer = repo
        .graph()
        .path("the diff view loads the lines of a changed file", "git-file-lines", 4);

    // nothing holds `view`, so it is not counted: loadLines alone holds `diff` (its file), `loads` and `lines`
    assert!(answer.contains(": 1 hop\n"), "{answer}");
    assert!(
        answer.contains("from: words `diff`, `loads`, `lines` → loadLines src/diff.ts:3 (no match: `view`)\n"),
        "{answer}"
    );

    let answer = repo.graph().path("diff lines", "git-file-lines", 4);

    assert!(answer.contains(": 1 hop\n"), "{answer}");
    assert!(
        answer.contains("from: words `diff`, `lines` → loadLines src/diff.ts:3\n"),
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

    let exact = repo.graph().symbol("RUNDOCTORSETUP", 5);

    assert!(exact.contains("fn runDoctorSetup  src/setup.ts:1"), "{exact}");
    assert!(repo.graph().impact("doctor", 2).contains("words `doctor` →"));
}

#[test]
fn a_word_finds_another_form_of_it_and_a_word_with_an_underscore_its_parts() {
    let mut repo = Repo::new();

    repo.write(
        "src/auth/middleware.ts",
        "export function createAuthenticationMiddleware() {\n  return 1;\n}\n",
    );
    repo.write("src/tools.ts", "export function noteSearchTool() {\n  return 2;\n}\n");
    repo.write(
        "src/app.ts",
        "import { createAuthenticationMiddleware } from './auth/middleware';\nimport { noteSearchTool } from './tools';\n\nexport function createApp() {\n  return createAuthenticationMiddleware() + noteSearchTool();\n}\n",
    );

    // `authenticated` meets the part `authentication`
    let forms = repo.graph().path("createApp", "authenticated middleware", 4);

    assert!(
        forms.contains(
            "to: words `authenticated`, `middleware` → createAuthenticationMiddleware src/auth/middleware.ts:1\n"
        ),
        "{forms}"
    );
    assert!(forms.contains(": 1 hop\n"), "{forms}");

    // `note_search` is the parts `note` and `search` in a row
    let parts = repo.graph().path("createApp", "note_search tool", 4);

    assert!(
        parts.contains("to: words `note_search`, `tool` → noteSearchTool src/tools.ts:1\n"),
        "{parts}"
    );
}

#[test]
fn tooling_comes_after_the_code_and_copies_of_a_definition_are_listed_once() {
    let mut repo = Repo::new();

    repo.write(
        "src/lib/api/request.ts",
        "export function request(url: string) {\n  return url;\n}\n",
    );
    repo.write(
        "scripts/check-http.mjs",
        "export function sendRequest(url) {\n  return url;\n}\n",
    );

    // the script holds `http` in its file name, but the request of the code leads
    let http = repo.graph().path("http request", "request", 4);
    let code = http.find("request src/lib/api/request.ts:1").expect("request");
    let script = http.find("sendRequest scripts/check-http.mjs:1").expect("sendRequest");

    assert!(code < script, "{http}");

    let mut skills = Repo::new();
    let format = "export function formatDoctor(report) {\n  return report;\n}\n";

    skills.write("plugins/claude/doctor.js", format);
    skills.write("plugins/opencode/doctor.js", format);
    skills.write("doctor.js", format);
    skills.write(
        "doctor-setup.js",
        "export function doctorServices() {\n  return 1;\n}\n",
    );

    // three copies of formatDoctor take one place, shown by the source
    let doctor = skills.graph().path("doctor", "doctorServices", 4);

    assert!(doctor.contains("  formatDoctor doctor.js:1  (doctor)\n"), "{doctor}");
    assert!(
        doctor.contains("  doctorServices doctor-setup.js:1  (doctor)\n"),
        "{doctor}"
    );
    assert!(!doctor.contains("plugins/"), "{doctor}");
}

#[test]
fn a_member_holds_the_words_of_its_type_and_a_tag_string_ranks_no_definition_higher() {
    let mut rust = Repo::new();

    rust.write(
        "src/cli.rs",
        "pub struct GitCli;\n\nimpl GitCli {\n    pub fn run(&self) {}\n}\n",
    );
    rust.write("src/other.rs", "pub fn run() {}\n");
    rust.write("src/main.rs", "pub fn start() {\n    other::run();\n}\n");

    // `git` is a part of the type of GitCli::run, so it alone holds both words: the other `run` is not the end
    let owner = rust.graph().path("start", "run git", 4);

    assert!(
        owner.contains("to: words `run`, `git` → GitCli::run src/cli.rs:4 (only by a type, tag or key: `git`)\n"),
        "{owner}"
    );
    assert!(!owner.contains("run src/other.rs:1"), "{owner}");

    // the type counts as a match once the member's own name holds a word: GitCli::run is named for all of
    // `git cli run`, a free function named for two of them is not the end
    rust.write("src/action.rs", "pub fn run_cli_action() {}\n");

    let typed = rust.graph().path("start", "git cli run", 4);

    assert!(typed.contains("→ GitCli::run src/cli.rs:4"), "{typed}");
    assert!(!typed.contains("run_cli_action src/action.rs:1,"), "{typed}");

    let mut ui = Repo::new();

    ui.write(
        "src/lib/api/request.ts",
        "export function request(url: string) {\n  return url;\n}\n",
    );
    ui.write(
        "src/panel.tsx",
        "export function Panel(config: object) {\n  return config.type === 'http_polling' && 'request' in config;\n}\n",
    );

    // `http` of the tag `'http_polling'` neither lifts the panel above the request the name says nor counts as held
    let tag = ui.graph().path("http request", "Panel", 4);

    assert!(
        tag.contains("from: words `request` → request src/lib/api/request.ts:1 (no match: `http`)\n"),
        "{tag}"
    );

    // the panel holds `http` and `polling` only by its tag: that names no end, it is only a candidate
    let only = ui.graph().path("http polling", "request", 4);

    assert!(only.contains("the words do not name one definition"), "{only}");
    assert!(
        only.contains("only types, tags or keys hold http, polling. Closest:"),
        "{only}"
    );
    assert!(only.contains("Panel src/panel.tsx:1"), "{only}");
}

#[test]
fn only_copies_share_a_place_and_a_nested_scripts_folder_is_code() {
    let mut repo = Repo::new();
    let get = "export function GET() {\n  return 1;\n}\n";

    repo.write("app/users/route.ts", get);
    repo.write("app/orders/route.ts", get);
    repo.write(
        "src/lib/api/request.ts",
        "export function request() {\n  return 2;\n}\n",
    );
    repo.write(
        "src/scripts/check-http.ts",
        "export function sendRequest() {\n  return 3;\n}\n",
    );

    repo.write("src/total.ts", "export function total() {\n  return 4;\n}\n");

    // two route files of the same name in different folders are two definitions, not copies: both are listed
    let routes = repo.graph().path("get total", "request", 4);

    assert!(routes.contains("  GET app/users/route.ts:1"), "{routes}");
    assert!(routes.contains("  GET app/orders/route.ts:1"), "{routes}");

    // `src/scripts/` is a folder of the code: its sendRequest holds `http` by its file and is the end
    let http = repo.graph().path("http request", "GET", 4);

    assert!(
        http.contains("from: words `http`, `request` → sendRequest src/scripts/check-http.ts:1"),
        "{http}"
    );
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

// ---- package.function ends -----------------------------------------------------------------------------------

#[test]
fn a_module_qualifier_means_the_function_of_that_module_not_a_method_of_the_same_name() {
    let mut repo = Repo::new();

    repo.write(
        "src/utils.ts",
        "export function formatDate(d: Date): string {\n  return d.toString();\n}\n",
    );
    repo.write(
        "src/fmt.ts",
        "export class Fmt {\n  formatDate(d: Date): string {\n    return d.toString();\n  }\n}\n",
    );
    repo.write("web/__init__.py", "from ._api import fetch\n");
    repo.write("web/_api.py", "def fetch(url):\n    return url\n");
    repo.write(
        "web/_client.py",
        "class Client:\n    def fetch(self, url):\n        return url\n",
    );
    repo.write("Cargo.toml", "[package]\nname = \"net\"\nversion = \"0.1.0\"\n");
    repo.write("src/http.rs", "pub fn send() {}\n");
    repo.write(
        "src/client.rs",
        "pub struct Client;\n\nimpl Client {\n    pub fn send(&self) {}\n}\n",
    );

    let graph = repo.graph();

    let one = graph.symbol("utils.formatDate", 5);
    assert!(one.contains("fn formatDate  src/utils.ts:1"), "{one}");
    assert!(!one.contains("Fmt::formatDate"), "{one}");

    let python = graph.symbol("web.fetch", 5);
    assert!(python.contains("fn fetch  web/_api.py:1"), "{python}");
    assert!(!python.contains("Client::fetch"), "{python}");

    let rust = graph.symbol("http::send", 5);
    assert!(rust.contains("fn send  src/http.rs:1"), "{rust}");
    assert!(!rust.contains("Client::send"), "{rust}");

    // a type as the qualifier still means its method
    assert!(
        graph
            .symbol("Fmt.formatDate", 5)
            .contains("method Fmt::formatDate  src/fmt.ts:2")
    );
    assert!(
        graph
            .symbol("Client.fetch", 5)
            .contains("Client::fetch  web/_client.py:2")
    );
    assert!(
        graph
            .symbol("Client::send", 5)
            .contains("Client::send  src/client.rs:4")
    );
}

// ---- calls on a base reach the overrides -----------------------------------------------------------------

/// A base class with `count` subclasses that override `run`, a function that calls `run` on a base, and `finish`.
fn hierarchy(count: usize) -> Repo {
    let repo = Repo::new();
    let mut classes = String::from("class Base:\n    def run(self):\n        pass\n\n");

    for i in 0..count {
        classes.push_str(&format!(
            "class Sub{i:02}(Base):\n    def run(self):\n        finish{i:02}()\n\ndef finish{i:02}():\n    pass\n\n"
        ));
    }

    repo.write("app/classes.py", &classes);
    repo.write(
        "app/main.py",
        "from classes import Base\n\ndef go(base: Base):\n    base.run()\n",
    );

    repo
}

#[test]
fn a_call_on_a_base_method_reaches_the_overrides_and_a_few_only() {
    let mut repo = hierarchy(30);
    let graph = repo.graph();
    let first = graph.path("go", "Sub00.run", 3);

    assert!(first.contains(": 1 hop\n"), "{first}");
    assert!(
        first.contains("  → Sub00::run (via subclass/impl of Base::run)  app/classes.py:"),
        "{first}"
    );
    assert!(first.contains("        at app/main.py:4 | base.run()"), "{first}");

    // the fan-out of one call is capped, so a base with hundreds of subclasses stays cheap
    let last = graph.path("go", "Sub29.run", 3);

    assert!(
        last.starts_with("no chain within 3 hops between `go` and `Sub29.run`"),
        "{last}"
    );

    // and the override is no caller: the weak link is no edge of the graph
    assert!(graph.symbol("Sub00.run", 5).contains("used by 0 site(s)"));
}

#[test]
fn a_chain_of_strong_links_wins_over_a_shorter_one_through_an_override() {
    let mut repo = Repo::new();

    repo.write(
        "app/chain.py",
        "class A:\n    def run(self):\n        pass\n\nclass B(A):\n    def run(self):\n        finish()\n\n\
         def go(a: A):\n    a.run()\n    helper()\n\ndef helper():\n    middle()\n\ndef middle():\n    finish()\n\ndef finish():\n    pass\n",
    );

    let answer = repo.graph().path("go", "finish", 6);

    // go -> B.run -> finish is shorter, but only through an override
    assert!(answer.contains(": 3 hops\n"), "{answer}");
    assert!(answer.contains("  → helper  app/chain.py"), "{answer}");
    assert!(!answer.contains("via subclass/impl"), "{answer}");

    // with no strong chain the override is used
    let weak = repo.graph().path("go", "B.run", 6);

    assert!(weak.contains("(via subclass/impl of A::run)"), "{weak}");
}

#[test]
fn a_chain_through_an_override_goes_down_more_than_one_level_of_subclasses() {
    let mut repo = Repo::new();

    repo.write(
        "src/shapes.ts",
        "export interface Shape { area(): number }\nexport class Poly implements Shape { area(): number { return 1; } }\n\
         export class Square extends Poly { area(): number { return side(); } }\nfunction side(): number { return 2; }\n",
    );
    repo.write(
        "src/use.ts",
        "import { Shape } from './shapes';\n\nexport function measure(s: Shape): number {\n  return s.area();\n}\n",
    );

    let answer = repo.graph().path("measure", "side", 4);

    assert!(answer.contains(": 2 hops\n"), "{answer}");
    assert!(
        answer.contains("  → Square::area (via subclass/impl of Shape::area)"),
        "{answer}"
    );
}

// ---- no chain ------------------------------------------------------------------------------------------------

#[test]
fn a_missing_chain_says_how_far_the_search_got_and_which_calls_it_did_not_follow() {
    let mut repo = Repo::new();

    repo.write(
        "src/flow.ts",
        "export function start() {\n  return middle();\n}\n\nfunction middle() {\n  return send(1);\n}\n\nexport function target() {\n  return 1;\n}\n",
    );
    repo.write("src/a.ts", "export function send(n: number) { return n; }\n");
    repo.write("src/b.ts", "export function send(n: number) { return n; }\n");
    repo.write("src/c.ts", "export function send(n: number) { return n; }\n");

    let graph = repo.graph();
    let answer = graph.path("start", "target", 4);

    assert!(
        answer.starts_with("no chain within 4 hops between `start` and `target` (tried both directions)\n"),
        "{answer}"
    );
    assert!(
        answer.contains("got as far as start → middle (1 hop, 2 reached) src/flow.ts:5\n"),
        "{answer}"
    );
    assert!(
        answer.contains("  not followed from middle: send (3 defs)\n"),
        "{answer}"
    );
    assert!(answer.lines().count() <= 4, "{answer}");

    // a definition with nothing that leads out
    let none = graph.path("target", "start", 4);

    assert!(none.contains("nothing leads out of target src/flow.ts:9\n"), "{none}");

    // a search that ran into the depth limit says so
    let short = graph.path("start", "target", 1);

    assert!(short.contains("stopped at max_depth: raise it to go on\n"), "{short}");
}
