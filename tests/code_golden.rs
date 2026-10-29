//! Golden tests of the code index: hand-verified facts about who uses what, over the small fixtures in
//! `tests/fixtures/code/` and, when present, the pinned public repositories (`bench/fetch.sh`) and the private
//! ones of `bench/local.toml`.
//!
//! The expectations are facts, not snapshots. Each one was derived by reading the code or running `git grep`
//! on the repository, never by copying what the index printed. They live next to the repository they describe:
//!
//! - `tests/fixtures/code/<lang>/golden.toml` for the fixtures,
//! - `bench/golden/<repo>.toml` for the public repositories,
//! - `bench/local/golden/<name>.toml` for the private ones (gitignored).
//!
//! An entry with `pending = "H3: why"` (or `"FINDING: why"`) is a fact the index does not get right yet. It is
//! skipped by the green tests and run by the `pending_*` tests, which are `#[ignore]`d: run them with
//! `cargo test --test code_golden -- --ignored` and remove `pending` from every entry that now passes.
//!
//! Formats (all lists are optional):
//!
//! ```toml
//! [[symbol]]                       # `Graph::symbol(query)`
//! query = "pricing::total"
//! defined = "src/pricing.rs:7"     # the output names this location
//! used_by = ["src/orders.rs:13 Order::cost"]       # a site line starts with this (a trailing " ?" is allowed)
//! sure_used_by = ["src/orders.rs:13 Order::cost"]  # a site line equals this (not marked ambiguous)
//! not_used_by = ["src/report.rs"]  # no site line starts with this
//! uses = ["add_tax"]               # the `uses:` list names this
//! not_uses = ["URL::join"]         # the `uses:` list does not name this
//! tests = ["tests/foo.py"]         # the tests line names this file
//!
//! [[impact]]                       # `Graph::impact(query, depth)`
//! query = "pricing::total"
//! depth = 2
//! files = ["src/orders.rs"]        # production files listed
//! not_files = ["src/report.rs"]
//! contains = ["Order::cost → total"]  # raw substrings of the output
//!
//! [[map]]                          # `Graph::map(budget, focus)`
//! budget = 1500
//! top = 10                         # only the first `top` files count
//! files = ["src/util/chars.go"]    # must be among them
//! not_files = ["src/tmux.go"]      # must not be among them
//!
//! [[path]]                         # `Graph::path(from, to, max_depth)`
//! from = "DiffPanel"
//! to = "git/mod::validate_paths"
//! max_depth = 6                    # default 6
//! hops = 4                         # the answer says `: 4 hops`
//! through = ["git_file_lines (by name)", "file_lines"]  # lines start with these, in this order
//! not_through = ["Dialogs::len"]   # no line starts with these
//! reverse = true                   # the chain is the other way round (the answer says so)
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use harness_memory::code::{CodeIndex, Graph};
use serde::Deserialize;

/// Tokens asked of `Graph::map` when an entry gives no budget.
const DEFAULT_BUDGET: usize = 1500;

/// Sites listed per symbol (the most `Graph::symbol` accepts).
const SITE_LIMIT: usize = 50;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Golden {
    symbol: Vec<SymbolFact>,
    impact: Vec<ImpactFact>,
    map: Vec<MapFact>,
    path: Vec<PathFact>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SymbolFact {
    query: String,
    pending: Option<String>,
    /// How the fact was checked, for the reader.
    note: Option<String>,
    defined: Option<String>,
    used_by: Vec<String>,
    sure_used_by: Vec<String>,
    not_used_by: Vec<String>,
    uses: Vec<String>,
    not_uses: Vec<String>,
    tests: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ImpactFact {
    query: String,
    depth: Option<usize>,
    pending: Option<String>,
    note: Option<String>,
    files: Vec<String>,
    not_files: Vec<String>,
    contains: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct MapFact {
    budget: Option<usize>,
    focus: Option<String>,
    top: Option<usize>,
    pending: Option<String>,
    note: Option<String>,
    files: Vec<String>,
    not_files: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PathFact {
    from: String,
    to: String,
    max_depth: Option<usize>,
    pending: Option<String>,
    note: Option<String>,
    hops: Option<usize>,
    through: Vec<String>,
    not_through: Vec<String>,
    reverse: bool,
}

/// One checked statement, for the report.
struct Outcome {
    label: String,
    pending: Option<String>,
    error: Option<String>,
}

/// The result of running one golden file over one repository.
#[derive(Default)]
struct Report {
    outcomes: Vec<Outcome>,
}

impl Report {
    fn total(&self) -> usize {
        self.outcomes.len()
    }

    fn active(&self) -> usize {
        self.outcomes.iter().filter(|o| o.pending.is_none()).count()
    }

    fn pending(&self) -> usize {
        self.total() - self.active()
    }

    fn push(&mut self, label: String, pending: &Option<String>, error: Option<String>) {
        self.outcomes.push(Outcome {
            label,
            pending: pending.clone(),
            error,
        });
    }

    /// The failed expectations that are not pending.
    fn failures(&self) -> Vec<String> {
        self.outcomes
            .iter()
            .filter(|o| o.pending.is_none())
            .filter_map(|o| o.error.as_ref().map(|e| format!("{}: {e}", o.label)))
            .collect()
    }
}

// ---- parsing the rendered text -------------------------------------------------------------------------

/// The "used by" site lines of a symbol answer, trimmed and without the source line after a ` | `:
/// `path:line caller [(by name)] [?]`.
fn sites(text: &str) -> Vec<&str> {
    let mut in_users = false;

    text.lines()
        .filter_map(|line| {
            let indent = line.len() - line.trim_start().len();

            // a heading ("  used by", "  uses:") starts a section
            if indent < 4 {
                in_users = line.starts_with("  used by");

                return None;
            }

            let site = line.trim().split(" | ").next().unwrap_or_default();

            (in_users && indent == 4 && !site.starts_with("tests:") && !site.starts_with('…')).then_some(site)
        })
        .collect()
}

/// What a symbol answer lists under `uses:` (every definition's, joined): `name path:line`, without the place
/// of the call after `  at `.
fn uses_lines(text: &str) -> String {
    let mut in_uses = false;
    let mut found: Vec<&str> = Vec::new();

    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();

        if indent < 4 {
            in_uses = line.starts_with("  uses:");

            continue;
        }

        if in_uses && indent == 4 {
            let used = line.trim().trim_start_matches("also: ");

            found.push(used.split("  at ").next().unwrap_or_default());
        }
    }

    found.join(", ")
}

/// The production files of an impact answer.
fn impact_files(text: &str) -> Vec<&str> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let depth = words.next()?;

            if depth.len() == 2 && depth.starts_with('d') && depth[1..].chars().all(|c| c.is_ascii_digit()) {
                words.next()
            } else {
                None
            }
        })
        .collect()
}

/// The files of a map answer, best first.
fn map_files(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with(' '))
        .collect()
}

// ---- checking facts ----------------------------------------------------------------------------------

fn check_symbol(graph: &Graph, fact: &SymbolFact, report: &mut Report) {
    let text = graph.symbol(&fact.query, SITE_LIMIT);
    let found = sites(&text);
    let uses = uses_lines(&text);
    let q = &fact.query;
    let ok = |label: String, pass: bool, why: &dyn Fn() -> String, report: &mut Report| {
        report.push(label, &fact.pending, (!pass).then(why));
    };

    if text.starts_with("no definition named") || text.starts_with("No symbol, file or command matches") {
        report.push(
            format!("symbol {q}: defined"),
            &fact.pending,
            Some(text.trim().to_string()),
        );

        return;
    }

    if let Some(location) = &fact.defined {
        ok(
            format!("symbol {q}: defined at {location}"),
            text.contains(&format!("  {location}")) || text.contains(&format!(" {location}-")),
            &|| format!("not defined there in\n{text}"),
            report,
        );
    }

    for wanted in &fact.used_by {
        let hit = found.iter().any(|site| site.starts_with(wanted.as_str()));

        ok(
            format!("symbol {q}: used by {wanted}"),
            hit,
            &|| format!("missing from the sites\n{}", found.join("\n")),
            report,
        );
    }

    for wanted in &fact.sure_used_by {
        let hit = found.iter().any(|site| site == wanted);

        ok(
            format!("symbol {q}: surely used by {wanted}"),
            hit,
            &|| format!("no unmarked site equals it\n{}", found.join("\n")),
            report,
        );
    }

    for unwanted in &fact.not_used_by {
        let hit = found.iter().find(|site| site.starts_with(unwanted.as_str()));

        ok(
            format!("symbol {q}: not used by {unwanted}"),
            hit.is_none(),
            &|| format!("listed as a site: {}", hit.copied().unwrap_or_default()),
            report,
        );
    }

    for wanted in &fact.uses {
        ok(
            format!("symbol {q}: uses {wanted}"),
            uses.contains(wanted.as_str()),
            &|| format!("missing from `uses: {uses}`"),
            report,
        );
    }

    for unwanted in &fact.not_uses {
        ok(
            format!("symbol {q}: does not use {unwanted}"),
            !uses.contains(unwanted.as_str()),
            &|| format!("listed in `uses: {uses}`"),
            report,
        );
    }

    for wanted in &fact.tests {
        let line = text
            .lines()
            .find(|l| l.trim_start().starts_with("tests:"))
            .unwrap_or_default();

        ok(
            format!("symbol {q}: tested in {wanted}"),
            line.contains(wanted.as_str()),
            &|| format!("not in `{line}`"),
            report,
        );
    }
}

fn check_impact(graph: &Graph, fact: &ImpactFact, report: &mut Report) {
    let depth = fact.depth.unwrap_or(2);
    let text = graph.impact(&fact.query, depth);
    let files = impact_files(&text);
    let q = &fact.query;

    if text.starts_with("no definition named") || text.starts_with("No symbol, file or command matches") {
        report.push(
            format!("impact {q}: defined"),
            &fact.pending,
            Some(text.trim().to_string()),
        );

        return;
    }

    for wanted in &fact.files {
        let hit = files.contains(&wanted.as_str());

        report.push(
            format!("impact {q} d{depth}: reaches {wanted}"),
            &fact.pending,
            (!hit).then(|| format!("not among the files\n{}", files.join("\n"))),
        );
    }

    for unwanted in &fact.not_files {
        let hit = files.contains(&unwanted.as_str());

        report.push(
            format!("impact {q} d{depth}: does not reach {unwanted}"),
            &fact.pending,
            hit.then(|| "listed".to_string()),
        );
    }

    for wanted in &fact.contains {
        report.push(
            format!("impact {q} d{depth}: shows `{wanted}`"),
            &fact.pending,
            (!text.contains(wanted.as_str())).then(|| format!("missing from\n{text}")),
        );
    }
}

fn check_map(graph: &Graph, fact: &MapFact, report: &mut Report) {
    let text = graph.map(fact.budget.unwrap_or(DEFAULT_BUDGET), fact.focus.as_deref());
    let files = map_files(&text);
    let top: Vec<&str> = files.iter().copied().take(fact.top.unwrap_or(files.len())).collect();
    let scope = fact.top.map_or_else(|| "map".to_string(), |n| format!("map top {n}"));

    for wanted in &fact.files {
        report.push(
            format!("{scope}: has {wanted}"),
            &fact.pending,
            (!top.contains(&wanted.as_str())).then(|| format!("not among\n{}", top.join("\n"))),
        );
    }

    for unwanted in &fact.not_files {
        report.push(
            format!("{scope}: lacks {unwanted}"),
            &fact.pending,
            top.contains(&unwanted.as_str()).then(|| {
                let place = top.iter().position(|f| f == unwanted).map_or(0, |i| i + 1);

                format!("listed at place {place}")
            }),
        );
    }
}

/// The symbol lines of a path answer, without the arrow: `label [(by name)]  path:line`. The notes under the head
/// and the `at` line of a hop are not among them.
fn chain_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.starts_with("  ") && !line.starts_with("      "))
        .map(|line| line.trim_start().trim_start_matches("→ ").trim_start())
        .collect()
}

fn check_path(graph: &Graph, fact: &PathFact, report: &mut Report) {
    let text = graph.path(&fact.from, &fact.to, fact.max_depth.unwrap_or(6));
    let lines = chain_lines(&text);
    let label = format!("path {} -> {}", fact.from, fact.to);
    let head = text.lines().next().unwrap_or_default();

    if let Some(hops) = fact.hops {
        let wanted = format!(": {hops} {}", if hops == 1 { "hop" } else { "hops" });

        report.push(
            format!("{label}: {hops} hop(s)"),
            &fact.pending,
            (!head.ends_with(&wanted)).then(|| format!("the answer is\n{text}")),
        );
    }

    let reversed = head.starts_with("no chain from");
    let none = head.starts_with("no chain within")
        || head.starts_with("no definition")
        || head.starts_with("No symbol, file or command matches");

    report.push(
        format!("{label}: {}", if fact.reverse { "reverse" } else { "forward" }),
        &fact.pending,
        (none || reversed != fact.reverse).then(|| format!("the answer is\n{text}")),
    );

    let mut at = 0;

    for wanted in &fact.through {
        let found = lines
            .iter()
            .enumerate()
            .skip(at)
            .find(|(_, line)| line.starts_with(&format!("{wanted}  ")));

        if let Some((i, _)) = found {
            at = i + 1;
        }

        report.push(
            format!("{label}: through {wanted}"),
            &fact.pending,
            found
                .is_none()
                .then(|| format!("not on the chain, in that order\n{text}")),
        );
    }

    for unwanted in &fact.not_through {
        let hit = lines.iter().find(|line| line.starts_with(&format!("{unwanted}  ")));

        report.push(
            format!("{label}: not through {unwanted}"),
            &fact.pending,
            hit.map(|line| format!("on the chain: {line}")),
        );
    }
}

// ---- running a repository -------------------------------------------------------------------------------

/// Indexes `root` into a throwaway database and checks every fact of `golden`.
fn run(root: &Path, golden: &Path) -> Report {
    let text = fs::read_to_string(golden).unwrap_or_else(|e| panic!("read {}: {e}", golden.display()));
    let facts: Golden = toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", golden.display()));
    let data = tempfile::tempdir().expect("data dir");
    let mut index = CodeIndex::open(&data.path().join("code.sqlite3")).expect("open the index");
    let root = root
        .canonicalize()
        .unwrap_or_else(|e| panic!("canonicalize {}: {e}", root.display()));
    let (graph, _) = index.graph(&root, &|| false).expect("index the repository");
    let mut report = Report::default();

    for fact in &facts.symbol {
        check_symbol(graph, fact, &mut report);
    }

    for fact in &facts.impact {
        check_impact(graph, fact, &mut report);
    }

    for fact in &facts.map {
        check_map(graph, fact, &mut report);
    }

    for fact in &facts.path {
        check_path(graph, fact, &mut report);
    }

    report
}

/// Prints the counts and returns every broken expectation that is not pending.
fn green_failures(name: &str, report: &Report) -> Vec<String> {
    eprintln!(
        "golden {name}: {} expectations, {} active, {} pending",
        report.total(),
        report.active(),
        report.pending()
    );

    report
        .failures()
        .into_iter()
        .map(|failure| format!("[{name}] {failure}"))
        .collect()
}

/// Fails with every broken expectation that is not pending.
fn assert_green(name: &str, report: &Report) {
    let failures = green_failures(name, report);

    assert!(
        failures.is_empty(),
        "{} of {} expectations of {name} failed:\n\n{}",
        failures.len(),
        report.active(),
        failures.join("\n\n")
    );
}

/// For the ignored tests: prints which pending expectations now pass and returns those that still fail.
fn pending_failures(name: &str, report: &Report) -> Vec<String> {
    let mut by_reason: BTreeMap<&str, (usize, usize)> = BTreeMap::new();

    for outcome in report.outcomes.iter().filter(|o| o.pending.is_some()) {
        let entry = by_reason
            .entry(outcome.pending.as_deref().unwrap_or_default())
            .or_default();

        if outcome.error.is_some() {
            entry.0 += 1;
        } else {
            entry.1 += 1;
            eprintln!("pending {name}: NOW PASSES, remove `pending`: {}", outcome.label);
        }
    }

    for (reason, (failing, passing)) in &by_reason {
        eprintln!("pending {name}: {failing} failing, {passing} passing: {reason}");
    }

    report
        .outcomes
        .iter()
        .filter(|o| o.pending.is_some())
        .filter_map(|o| o.error.as_ref().map(|e| format!("[{name}] {}: {e}", o.label)))
        .collect()
}

/// For the ignored tests: fails while any pending expectation still fails, and says which ones now pass.
fn assert_pending(name: &str, report: &Report) {
    let failing = pending_failures(name, report);

    assert!(
        failing.is_empty(),
        "{} pending expectations of {name} still fail",
        failing.len()
    );
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

// ---- the fixtures ---------------------------------------------------------------------------------------

fn fixture(name: &str) -> (PathBuf, PathBuf) {
    let root = manifest_dir().join("tests/fixtures/code").join(name);
    let golden = root.join("golden.toml");

    (root, golden)
}

fn fixture_report(name: &str) -> Report {
    let (root, golden) = fixture(name);

    run(&root, &golden)
}

/// Everything the index says about a fixture: the whole map, then the answer to every symbol and impact query
/// of its golden file, as one insta snapshot (the full output of a small repository).
fn fixture_output(name: &str) -> String {
    let (root, golden) = fixture(name);
    let text = fs::read_to_string(&golden).expect("read golden.toml");
    let facts: Golden = toml::from_str(&text).expect("parse golden.toml");
    let data = tempfile::tempdir().expect("data dir");
    let mut index = CodeIndex::open(&data.path().join("code.sqlite3")).expect("open the index");
    let (graph, _) = index
        .graph(&root.canonicalize().expect("canonical"), &|| false)
        .expect("index");
    let mut out = format!("== map\n{}", graph.map(4000, None));
    let mut symbols: Vec<&str> = facts.symbol.iter().map(|f| f.query.as_str()).collect();

    symbols.sort_unstable();
    symbols.dedup();

    for query in symbols {
        out += &format!("\n== symbol {query}\n{}", graph.symbol(query, SITE_LIMIT));
    }

    let mut impacts: Vec<(&str, usize)> = facts
        .impact
        .iter()
        .map(|f| (f.query.as_str(), f.depth.unwrap_or(2)))
        .collect();

    impacts.sort_unstable();
    impacts.dedup();

    for (query, depth) in impacts {
        out += &format!("\n== impact {query} d{depth}\n{}", graph.impact(query, depth));
    }

    for fact in &facts.path {
        out += &format!(
            "\n== path {} -> {}\n{}",
            fact.from,
            fact.to,
            graph.path(&fact.from, &fact.to, fact.max_depth.unwrap_or(6))
        );
    }

    out
}

macro_rules! fixture_tests {
    ($($test:ident, $pending:ident, $snapshot:ident => $name:literal;)*) => {$(
        #[test]
        fn $test() {
            assert_green($name, &fixture_report($name));
        }

        #[test]
        #[ignore = "pending expectations of the fixture: run with --ignored once the resolver improves"]
        fn $pending() {
            assert_pending($name, &fixture_report($name));
        }

        #[test]
        fn $snapshot() {
            insta::assert_snapshot!(concat!($name, "_output"), fixture_output($name));
        }
    )*};
}

fixture_tests! {
    fixture_rust, pending_fixture_rust, snapshot_fixture_rust => "rust";
    fixture_ts, pending_fixture_ts, snapshot_fixture_ts => "ts";
    fixture_react_tsx, pending_fixture_react_tsx, snapshot_fixture_react_tsx => "react-tsx";
    fixture_svelte, pending_fixture_svelte, snapshot_fixture_svelte => "svelte";
    fixture_vue, pending_fixture_vue, snapshot_fixture_vue => "vue";
    fixture_astro, pending_fixture_astro, snapshot_fixture_astro => "astro";
    fixture_go, pending_fixture_go, snapshot_fixture_go => "go";
    fixture_python, pending_fixture_python, snapshot_fixture_python => "python";
    fixture_ipc, pending_fixture_ipc, snapshot_fixture_ipc => "ipc";
    fixture_vite, pending_fixture_vite, snapshot_fixture_vite => "vite";
    fixture_sveltekit, pending_fixture_sveltekit, snapshot_fixture_sveltekit => "sveltekit";
    fixture_ts_namespace, pending_fixture_ts_namespace, snapshot_fixture_ts_namespace => "ts-namespace";
    fixture_esm_js, pending_fixture_esm_js, snapshot_fixture_esm_js => "esm-js";
}

// ---- the public repositories ----------------------------------------------------------------------------

/// The report over a pinned public repository, or `None` (with a message) when it was not fetched.
fn public_report(name: &str) -> Option<Report> {
    let root = manifest_dir().join("target/bench-repos").join(name);

    if !root.join(".git").exists() {
        eprintln!(
            "SKIPPED golden {name}: {} is missing, run bench/fetch.sh",
            root.display()
        );

        return None;
    }

    Some(run(
        &root,
        &manifest_dir().join("bench/golden").join(format!("{name}.toml")),
    ))
}

macro_rules! public_tests {
    ($($test:ident, $pending:ident => $name:literal;)*) => {$(
        #[test]
        fn $test() {
            if let Some(report) = public_report($name) {
                assert_green($name, &report);
            }
        }

        #[test]
        #[ignore = "pending expectations of a public repository: run with --ignored once the resolver improves"]
        fn $pending() {
            if let Some(report) = public_report($name) {
                assert_pending($name, &report);
            }
        }
    )*};
}

public_tests! {
    golden_fzf, pending_golden_fzf => "fzf";
    golden_vitepress, pending_golden_vitepress => "vitepress";
    golden_httpx, pending_golden_httpx => "httpx";
}

// ---- the private repositories ----------------------------------------------------------------------------

/// `name = "/absolute/path"` pairs of `bench/local.toml` (gitignored), or `None` when the file is missing.
fn local_repos() -> Option<BTreeMap<String, PathBuf>> {
    let text = fs::read_to_string(manifest_dir().join("bench/local.toml")).ok()?;
    let table: BTreeMap<String, String> = toml::from_str(&text).expect("parse bench/local.toml");

    Some(
        table
            .into_iter()
            .map(|(name, path)| (name, PathBuf::from(path)))
            .collect(),
    )
}

/// One report per private repository that exists on this computer and has a golden file.
fn local_reports() -> Vec<(String, Report)> {
    let Some(repos) = local_repos() else {
        eprintln!("SKIPPED golden local: bench/local.toml is missing (see bench/local.toml.example)");

        return Vec::new();
    };
    let mut reports = Vec::new();

    for (name, root) in repos {
        let golden = manifest_dir().join("bench/local/golden").join(format!("{name}.toml"));

        if !root.is_dir() || !golden.is_file() {
            eprintln!(
                "SKIPPED golden local {name}: {} or {} is missing",
                root.display(),
                golden.display()
            );

            continue;
        }

        let report = run(&root, &golden);

        reports.push((name, report));
    }

    reports
}

#[test]
fn golden_local() {
    let failures: Vec<String> = local_reports()
        .iter()
        .flat_map(|(name, report)| green_failures(name, report))
        .collect();

    assert!(
        failures.is_empty(),
        "{} private expectations failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
#[ignore = "pending expectations of the private repositories: run with --ignored once the resolver improves"]
fn pending_golden_local() {
    let failing: Vec<String> = local_reports()
        .iter()
        .flat_map(|(name, report)| pending_failures(name, report))
        .collect();

    assert!(
        failing.is_empty(),
        "{} pending private expectations still fail",
        failing.len()
    );
}
