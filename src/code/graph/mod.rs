//! The in-memory graph of one repository: names resolved to definitions through imports, modules and
//! packages, then ranked and rendered.
//!
//! Resolution is by name, imports and modules, not a compiler: it is framework agnostic and an unresolved
//! name (an external crate or package) stays unresolved. The order for one reference:
//!
//! 1. the same file, for unqualified names only;
//! 2. the qualifier (`Type::method` through the owning type, then the language's module rules);
//! 3. the explicit import (the language's rules);
//! 4. the same package, then non-test definitions;
//! 5. more than 8 candidates is too ambiguous and is dropped.
//!
//! The index holds untrusted code, so every step is bounded: a name with more definitions in reach than the
//! caps below is ambiguous and makes no edge (a hostile file of a hundred thousand `fn x(){x()}` lines
//! would otherwise make a hundred thousand squared edges), the work of all the lookups together has a
//! budget, the edges of the graph are capped, and the load polls the caller's cancellation and time budget.
//!
//! A method call is first narrowed by the written type of its receiver (see `infer`), and a function passed
//! as a value only means a function of the same file or an imported one. A method call that nothing else
//! settles may mean a function kept in a property of an object literal (`{ run: runIt }`, then `host.run()`):
//! see `Graph::resolve_prop`.
//!
//! Method calls only resolve to methods, types to type kinds, macros to macros. A string literal that
//! names a function or component defined in another file is a weak `(by name)` link (see
//! `Graph::resolve_str`).
//!
//! A language joins by implementing `Rules` in its own module and listing it in `rules()`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::Connection;

use super::pace::Pace;
use super::packages::normalize;
use super::store::db;
use super::walk::TOO_LARGE;

mod endpoint;
mod go;
mod infer;
mod path;
mod props;
mod python;
mod rank;
mod render;
mod rust;
mod site;
mod site_scan;
#[cfg(test)]
mod tests;
mod ts;
mod words;

/// More candidates than this make a reference too ambiguous to be signal.
const MAX_CANDIDATES: usize = 8;

/// A step that settles a reference by a narrow rule (the same file, the owning type, an import) may name at
/// most this many definitions. More is an ambiguous name and makes no edge.
const MAX_SETTLED: usize = 64;

/// A name with more definitions than this is not scanned for the wide steps (package, import and module
/// rules), only looked up by file and owner.
const MAX_SCAN: usize = 1_024;

/// The definitions all the scans of a graph may visit together. A repository that spends it ends up with
/// the remaining references unresolved, the same on every load.
const SCAN_BUDGET: usize = 120_000_000;

/// The edges (reference to definition) a graph keeps.
const MAX_EDGES: usize = 4_000_000;

/// The most definitions and references a graph holds: past them the repository is too large. A graph costs
/// about 500 bytes per reference (measured on a 2,600 file repository with 250,000 of them), so a full one
/// is about half a gigabyte. References that cannot resolve (a constant no definition carries) are not
/// loaded, so they do not count.
pub(super) const MAX_SYMS: usize = 500_000;
pub(super) const MAX_REFS: usize = 1_000_000;

/// How many rows are read or references resolved between two polls of the pace.
const POLL_EVERY: usize = 2_048;

/// The most a method call weighs when only its name, not its receiver, tells which method it is.
const NAME_ONLY_WEIGHT: f32 = 0.95;

/// A string link keeps at most this many targets.
const MAX_STRING_TARGETS: usize = 3;

/// The most places where strings that name no definition are kept, in all and per string. They are no edges:
/// an agent may name one (an event) as the end of a path.
const MAX_MENTIONS: usize = 50_000;
const MAX_MENTIONS_PER_NAME: usize = 32;

/// The length of text that the per-item numbers of [`Graph::estimated_bytes`] already hold.
const SHORT_TEXT: usize = 24;

/// Symbol kinds that are types.
const TYPE_KINDS: &[&str] = &[
    "struct",
    "enum",
    "union",
    "trait",
    "type",
    "interface",
    "class",
    "component",
];

struct File {
    path: String,
    /// The stamp the index holds for the file (`files.mtime_ns`, `files.size`): a source line is shown only while
    /// the file on disk still has it.
    mtime_ns: i64,
    size: i64,
    /// The language (`files.lang`), one of the names in [`rules`].
    lang: &'static str,
    test: bool,
    /// The crate (Rust), the directory (Go) or the first path component.
    pkg: String,
    /// Rust module path inside its crate: `business/src/git/file_lines.rs` -> `[git, file_lines]`.
    segs: Vec<String>,
}

struct Sym {
    file: usize,
    name: String,
    kind: String,
    parent: Option<String>,
    line: i64,
    end_line: i64,
    sig: String,
    /// The written return type, reduced to a type name.
    ret: Option<String>,
    /// In a test file or a `#[cfg(test)]` module.
    test: bool,
}

struct Ref {
    file: usize,
    from: Option<usize>,
    name: String,
    kind: String,
    qual: Option<String>,
    /// The receiver expression of a method call (see `infer`).
    recv: Option<String>,
    line: i64,
}

/// Where a string that names no definition is written.
struct Mention {
    file: usize,
    /// The definition the string is written in; `None` for module-level code.
    from: Option<usize>,
    line: i64,
}

struct Imp {
    orig: String,
    source: String,
}

/// The manifests of the repository.
#[derive(Default)]
struct Packages {
    /// Rust crate name -> folder.
    crates: HashMap<String, String>,
    /// TypeScript alias prefix -> folder, longest prefix first.
    aliases: Vec<(String, String)>,
    /// Go module path -> folder.
    gomods: Vec<(String, String)>,
    /// Python source roots (folders absolute imports start from), shallowest first.
    pyroots: Vec<String>,
}

/// How many files, definitions and references a graph holds, and how many references resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub files: usize,
    pub symbols: usize,
    pub refs: usize,
    pub resolved: usize,
}

/// The resolved graph of one repository. Built by [`Graph::load`]; queried by `map`, `symbol` and `impact`.
pub struct Graph {
    /// The repository's absolute root.
    root: String,
    files: Vec<File>,
    syms: Vec<Sym>,
    refs: Vec<Ref>,
    /// Per reference: the resolved symbols with their weight (1.0 = unique).
    res: Vec<Vec<(usize, f32)>>,
    /// Per symbol: the references resolving to it.
    incoming: Vec<Vec<usize>>,
    /// Per symbol: the references made from inside it.
    outgoing: Vec<Vec<usize>>,
    by_name: HashMap<String, Vec<usize>>,
    file_syms: Vec<Vec<usize>>,
    path_idx: HashMap<String, usize>,
    imports: HashMap<(usize, String), Imp>,
    /// Per file: what it passes on with `export ... from`.
    reexports: HashMap<usize, ts::Reexports>,
    packages: Packages,
    /// Strings that name no definition of the repository, with where they are written.
    loose: HashMap<String, Vec<Mention>>,
    /// Word literals (see `words`): a short plain word or command written in code, with the definitions that
    /// write it.
    words: HashMap<String, Vec<usize>>,
    /// Per Rust crate: the module names its files define.
    pkg_mods: HashMap<String, HashSet<String>>,
    /// Written types of variables, parameters and fields.
    types: infer::Types,
    /// Per reference: a method call matched by its name alone (no receiver type, no qualifier), which
    /// counts little in the ranking.
    soft: Vec<bool>,
    resolved: usize,
    /// Per (file, name): its definitions of that name.
    by_file_name: HashMap<(usize, String), Vec<usize>>,
    /// Per (name, owner): the definitions of that name inside that type.
    by_owner: HashMap<(String, String), Vec<usize>>,
    /// The (language, name) of every type defined.
    type_defs: HashSet<(&'static str, String)>,
    /// Per Go package directory: its files.
    go_dirs: HashMap<String, Vec<usize>>,
    /// Per property name: the functions object literals put there.
    props: HashMap<String, props::Prop>,
    /// What the scans may still visit (see [`SCAN_BUDGET`]).
    scan_left: AtomicUsize,
}

/// The language-specific steps of resolution. A step answers `Some(targets)` to settle the reference
/// (an empty list drops it) or `None` to let the next step try.
trait Rules: Sync {
    /// Settles a reference before candidates are looked up (an import of a single-file component, or of an
    /// external package).
    fn before(&self, _graph: &Graph, _r: &Ref, _import: Option<&Imp>) -> Option<Vec<usize>> {
        None
    }

    /// The qualifier step after the owning type was tried; it may narrow `cands` and let the next step go on.
    fn qualified(&self, _graph: &Graph, _r: &Ref, _qual: &str, _cands: &mut Vec<usize>) -> Option<Vec<usize>> {
        None
    }

    /// The explicit import step.
    fn imported(&self, _graph: &Graph, _r: &Ref, _import: &Imp, _cands: &[usize]) -> Option<Vec<usize>> {
        None
    }

    /// The package a file belongs to, when it is not the crate or first-folder default.
    fn package_of(&self, _path: &str) -> Option<String> {
        None
    }

    /// True when an unqualified name can only come from the file's own package (Go).
    fn own_package_only(&self) -> bool {
        false
    }

    /// True when a call without a qualifier cannot mean a method (Python: a method needs `obj.m()`).
    fn bare_call_skips_methods(&self) -> bool {
        false
    }

    /// True when the import brings in code from outside the repository.
    fn external(&self, _graph: &Graph, _file: usize, _import: &Imp) -> bool {
        false
    }

    /// True when the import's source is a package the repository does not hold and the name was matched to a
    /// definition of the repository by its name alone: never quite certain.
    fn name_only(&self, _graph: &Graph, _file: usize, _import: &Imp) -> bool {
        false
    }

    /// True when a call without a qualifier to this name is the language's own builtin, unless the
    /// repository defines it in the same package.
    fn is_builtin(&self, _name: &str) -> bool {
        false
    }
}

/// The outcome of resolving one reference.
struct Resolved {
    targets: Vec<(usize, f32)>,
    /// A method call matched by its name alone.
    soft: bool,
}

impl Resolved {
    fn none() -> Self {
        Self::sure(Vec::new())
    }

    fn sure(targets: Vec<(usize, f32)>) -> Self {
        Self { targets, soft: false }
    }
}

/// The rules of a language by its `files.lang` name.
fn rules(lang: &str) -> Option<&'static dyn Rules> {
    match lang {
        "rust" => Some(&rust::RULES),
        "ts" => Some(&ts::RULES),
        "go" => Some(&go::RULES),
        "python" => Some(&python::RULES),
        _ => None,
    }
}

/// The static name of a language, so files can hold it without allocating.
fn lang_name(lang: &str) -> Option<&'static str> {
    super::extract::LANGS
        .iter()
        .map(|spec| spec.name)
        .find(|name| *name == lang)
}

/// True for a path that holds tests (by folder or file name).
fn is_test(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let folders = path.rsplit_once('/').map_or("", |(folders, _)| folders);
    let in_folder = |wanted: &str| folders.split('/').any(|folder| folder == wanted);

    in_folder("tests")
        || in_folder("test")
        || in_folder("__tests__")
        || in_folder("__mocks__")
        || in_folder("e2e")
        || path.contains(".test.")
        || path.contains(".spec.")
        || path.ends_with("/tests.rs")
        || stem.ends_with("_test")
        || stem.ends_with("_spec")
        || (name.starts_with("test_") && name.ends_with(".py"))
        || name == "conftest.py"
        || path.contains("/mock")
}

/// Splits a file into its crate (the one whose folder is the longest prefix, else the first path
/// component) and its module segments.
fn locate(path: &str, crates: &HashMap<String, String>) -> (String, Vec<String>) {
    let (pkg, inner) = crates
        .iter()
        .filter(|(_, dir)| {
            dir.is_empty()
                || path
                    .strip_prefix(dir.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
        .max_by(|a, b| a.1.len().cmp(&b.1.len()).then_with(|| b.0.cmp(a.0)))
        .map(|(name, dir)| {
            let inner = if dir.is_empty() {
                path
            } else {
                path.get(dir.len() + 1..).unwrap_or(path)
            };

            (name.clone(), inner.to_string())
        })
        .unwrap_or_else(|| (path.split('/').next().unwrap_or("").to_string(), path.to_string()));

    let stem = inner.strip_suffix(".rs").unwrap_or(&inner);
    let mut segs: Vec<String> = stem.split('/').map(str::to_string).collect();

    if matches!(segs.first().map(String::as_str), Some("src" | "tests")) {
        segs.remove(0);
    }

    if matches!(segs.last().map(String::as_str), Some("mod" | "lib" | "main")) {
        segs.pop();
    }

    (pkg, segs)
}

/// Spreads one unit of weight over the candidates; more than [`MAX_SETTLED`] are too ambiguous to be one.
fn weigh(cands: Vec<usize>) -> Vec<(usize, f32)> {
    if cands.is_empty() || cands.len() > MAX_SETTLED {
        return Vec::new();
    }

    let weight = 1.0 / cands.len() as f32;

    cands.into_iter().map(|sym| (sym, weight)).collect()
}

/// The weight of a method call matched by its name alone: shared out like [`weigh`], and never quite
/// certain, so the answer marks it with `?` even when only one method has that name.
fn weigh_soft(cands: Vec<usize>) -> Vec<(usize, f32)> {
    weigh(cands)
        .into_iter()
        .map(|(sym, weight)| (sym, weight.min(NAME_ONLY_WEIGHT)))
        .collect()
}

impl Graph {
    /// Reads the repository `repo` from the database and resolves every reference.
    pub fn load(conn: &Connection, repo: i64, pace: &Pace) -> Result<Graph, String> {
        let packages = load_packages(conn, repo)?;
        let root: String = conn
            .query_row("SELECT root FROM repos WHERE id = ?1", [repo], |row| row.get(0))
            .map_err(db)?;

        let mut files = Vec::new();
        let mut file_index: HashMap<i64, usize> = HashMap::new();
        let mut statement = conn
            .prepare("SELECT id, path, lang, mtime_ns, size FROM files WHERE repo_id = ?1 ORDER BY path")
            .map_err(db)?;
        let rows = statement
            .query_map([repo], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(db)?;

        for (count, row) in rows.enumerate() {
            poll(pace, count)?;

            let (id, path, lang, mtime_ns, size) = row.map_err(db)?;
            let Some(lang) = lang_name(&lang) else {
                continue;
            };
            let (mut pkg, segs) = locate(&path, &packages.crates);

            if let Some(own) = rules(lang).and_then(|rules| rules.package_of(&path)) {
                pkg = own;
            }

            file_index.insert(id, files.len());
            files.push(File {
                test: is_test(&path),
                path,
                mtime_ns,
                size,
                lang,
                pkg,
                segs,
            });
        }

        let mut pkg_mods: HashMap<String, HashSet<String>> = HashMap::new();

        for file in files.iter().filter(|file| file.lang == "rust") {
            pkg_mods
                .entry(file.pkg.clone())
                .or_default()
                .extend(file.segs.iter().cloned());
        }

        let path_idx = files
            .iter()
            .enumerate()
            .map(|(i, file)| (file.path.clone(), i))
            .collect();

        let mut syms = Vec::new();
        let mut sym_index: HashMap<i64, usize> = HashMap::new();
        let mut statement = conn
            .prepare("SELECT id, file_id, name, kind, parent, line, end_line, sig, ret, test FROM symbols WHERE repo_id = ?1 ORDER BY id")
            .map_err(db)?;
        let rows = statement
            .query_map([repo], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, bool>(9)?,
                ))
            })
            .map_err(db)?;

        for (count, row) in rows.enumerate() {
            poll(pace, count)?;

            if syms.len() >= MAX_SYMS {
                return Err(TOO_LARGE.into());
            }

            let (id, file_id, name, kind, parent, line, end_line, sig, ret, test) = row.map_err(db)?;
            let Some(&file) = file_index.get(&file_id) else {
                continue;
            };
            let in_test_file = files.get(file).is_some_and(|f| f.test);

            sym_index.insert(id, syms.len());
            syms.push(Sym {
                file,
                name,
                kind,
                parent,
                line,
                end_line,
                sig,
                ret,
                test: test || in_test_file,
            });
        }

        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut file_syms = vec![Vec::new(); files.len()];

        for (i, sym) in syms.iter().enumerate() {
            by_name.entry(sym.name.clone()).or_default().push(i);

            if let Some(list) = file_syms.get_mut(sym.file) {
                list.push(i);
            }
        }

        let mut imports = HashMap::new();
        let mut reexports: HashMap<usize, ts::Reexports> = HashMap::new();
        let mut statement = conn
            .prepare("SELECT file_id, local, orig, source FROM imports WHERE repo_id = ?1")
            .map_err(db)?;
        let rows = statement
            .query_map([repo], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(db)?;

        for (count, row) in rows.enumerate() {
            poll(pace, count)?;

            if imports.len() + reexports.len() >= MAX_REFS {
                return Err(TOO_LARGE.into());
            }

            let (file_id, local, orig, source) = row.map_err(db)?;
            let Some(&file) = file_index.get(&file_id) else {
                continue;
            };

            match local.strip_prefix("export:") {
                Some(exported) => reexports.entry(file).or_default().add(exported, source),
                None => {
                    imports.insert((file, local), Imp { orig, source });
                }
            }
        }

        // the names a `use` reference can resolve to
        let valued: HashSet<&str> = syms
            .iter()
            .filter(|sym| matches!(sym.kind.as_str(), "const" | "static" | "enum_member"))
            .map(|sym| sym.name.as_str())
            .collect();

        let mut refs = Vec::new();
        let mut prop_rows: Vec<props::PropRow> = Vec::new();
        let mut loose: HashMap<String, Vec<Mention>> = HashMap::new();
        let mut mentions = 0_usize;
        let mut words: HashMap<String, Vec<usize>> = HashMap::new();
        let mut word_places = 0_usize;
        let mut statement = conn
            .prepare(
                "SELECT file_id, from_symbol, name, kind, qual, recv, line FROM refs WHERE repo_id = ?1 ORDER BY id",
            )
            .map_err(db)?;
        let rows = statement
            .query_map([repo], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(db)?;

        for (count, row) in rows.enumerate() {
            poll(pace, count)?;

            if refs.len() + prop_rows.len() >= MAX_REFS {
                return Err(TOO_LARGE.into());
            }

            let (file_id, from, name, kind, qual, recv, line) = row.map_err(db)?;
            let Some(&file) = file_index.get(&file_id) else {
                continue;
            };

            // a word literal is no reference either: it only tells which definition a word of a question may mean
            if kind == "word" {
                if let Some(sym) = from.and_then(|id| sym_index.get(&id).copied()) {
                    words::keep(&mut words, &mut word_places, &name, sym);
                }

                continue;
            }

            // a function kept in a property is no reference: it only tells what a call through the property
            // may mean
            if kind == "prop" {
                if let Some(value) = qual {
                    prop_rows.push(props::PropRow {
                        file,
                        key: name,
                        value,
                        via: recv,
                    });
                }

                continue;
            }

            // the parts of a string that names a function (`"git_file_lines"`) are words the definition writes
            if kind == "str"
                && let Some(sym) = from.and_then(|id| sym_index.get(&id).copied())
            {
                words::keep_parts(&mut words, &mut word_places, &name, sym);
            }

            // a string that names nothing the repository defines can never resolve: it stays in the index file
            // (the definition may be added later) but not in the edges of the graph. Where it is written is
            // kept, within a cap, for a query that names it
            if kind == "str" && !by_name.contains_key(&name) {
                if mentions < MAX_MENTIONS {
                    let places = loose.entry(name).or_default();

                    if places.len() < MAX_MENTIONS_PER_NAME {
                        places.push(Mention {
                            file,
                            from: from.and_then(|id| sym_index.get(&id).copied()),
                            line,
                        });
                        mentions += 1;
                    }
                }

                continue;
            }

            // a constant-looking name that no constant of the repository carries (under the name it is
            // imported from, if it is imported) could never resolve: it is no reference
            if kind == "use" {
                let written = imports
                    .get(&(file, name.clone()))
                    .filter(|import| import.orig != "*")
                    .map_or(name.as_str(), |import| import.orig.as_str());

                if !valued.contains(written) {
                    continue;
                }
            }

            refs.push(Ref {
                file,
                from: from.and_then(|id| sym_index.get(&id).copied()),
                name,
                kind,
                qual,
                recv,
                line,
            });
        }

        let types = infer::Types::load(conn, repo, &file_index, &sym_index, &files, &syms, &file_syms, pace)?;
        let symbol_count = syms.len();

        let mut by_file_name: HashMap<(usize, String), Vec<usize>> = HashMap::new();
        let mut by_owner: HashMap<(String, String), Vec<usize>> = HashMap::new();
        let mut type_defs: HashSet<(&'static str, String)> = HashSet::new();

        for (i, sym) in syms.iter().enumerate() {
            by_file_name.entry((sym.file, sym.name.clone())).or_default().push(i);

            if let Some(owner) = &sym.parent {
                by_owner.entry((sym.name.clone(), owner.clone())).or_default().push(i);
            }

            if TYPE_KINDS.contains(&sym.kind.as_str())
                && let Some(file) = files.get(sym.file)
            {
                type_defs.insert((file.lang, sym.name.clone()));
            }
        }

        let mut go_dirs: HashMap<String, Vec<usize>> = HashMap::new();

        for (i, file) in files.iter().enumerate().filter(|(_, file)| file.lang == "go") {
            go_dirs.entry(file.pkg.clone()).or_default().push(i);
        }

        let mut graph = Graph {
            root,
            files,
            syms,
            refs,
            res: Vec::new(),
            incoming: vec![Vec::new(); symbol_count],
            outgoing: vec![Vec::new(); symbol_count],
            by_name,
            file_syms,
            path_idx,
            imports,
            reexports,
            packages,
            loose,
            words,
            pkg_mods,
            types,
            soft: Vec::new(),
            resolved: 0,
            by_file_name,
            by_owner,
            type_defs,
            go_dirs,
            props: HashMap::new(),
            scan_left: AtomicUsize::new(SCAN_BUDGET),
        };

        graph.bind_props(prop_rows, pace)?;
        graph.link(pace)?;

        Ok(graph)
    }

    /// Resolves every reference: code references first, then string names, which prefer entry points
    /// (definitions without direct callers in their own language). The edges are capped: past
    /// [`MAX_EDGES`] the remaining references stay unresolved.
    fn link(&mut self, pace: &Pace) -> Result<(), String> {
        let mut res: Vec<Vec<(usize, f32)>> = Vec::with_capacity(self.refs.len());
        let mut soft: Vec<bool> = Vec::with_capacity(self.refs.len());
        let mut edges = 0_usize;

        for ri in 0..self.refs.len() {
            poll(pace, ri)?;

            let settled = if edges >= MAX_EDGES || self.is_string(ri) {
                Resolved::none()
            } else {
                self.resolve(ri)
            };

            edges += settled.targets.len();

            res.push(settled.targets);
            soft.push(settled.soft);
        }

        let mut callers = vec![0_usize; self.syms.len()];

        for (ri, targets) in res.iter().enumerate() {
            for (sym, _) in targets {
                if self.lang_of_ref(ri) == self.lang_of_sym(*sym)
                    && let Some(count) = callers.get_mut(*sym)
                {
                    *count += 1;
                }
            }
        }

        for ri in 0..self.refs.len() {
            poll(pace, ri)?;

            if self.is_string(ri) {
                let targets = self.resolve_str(ri, &callers);

                edges += targets.len();

                if edges <= MAX_EDGES
                    && let Some(slot) = res.get_mut(ri)
                {
                    *slot = targets;
                }
            }
        }

        for (ri, targets) in res.iter().enumerate() {
            if !targets.is_empty() {
                self.resolved += 1;
            }

            for (sym, _) in targets {
                if let Some(list) = self.incoming.get_mut(*sym) {
                    list.push(ri);
                }
            }

            if let Some(from) = self.refs.get(ri).and_then(|r| r.from)
                && let Some(list) = self.outgoing.get_mut(from)
            {
                list.push(ri);
            }
        }

        self.res = res;
        self.soft = soft;

        Ok(())
    }

    fn is_string(&self, ri: usize) -> bool {
        self.refs.get(ri).is_some_and(|r| r.kind == "str")
    }

    fn lang_of_ref(&self, ri: usize) -> &str {
        self.refs
            .get(ri)
            .and_then(|r| self.files.get(r.file))
            .map_or("", |f| f.lang)
    }

    fn lang_of_sym(&self, sym: usize) -> &str {
        self.syms
            .get(sym)
            .and_then(|s| self.files.get(s.file))
            .map_or("", |f| f.lang)
    }

    /// The file of a symbol.
    fn file_of(&self, sym: usize) -> Option<&File> {
        self.syms.get(sym).and_then(|s| self.files.get(s.file))
    }

    /// How many edges (a reference and one definition it resolves to) the graph holds.
    #[cfg(test)]
    pub(crate) fn edges(&self) -> usize {
        self.res.iter().map(Vec::len).sum()
    }

    /// How many files, definitions and references the graph holds.
    pub fn counts(&self) -> Counts {
        Counts {
            files: self.files.len(),
            symbols: self.syms.len(),
            refs: self.refs.len(),
            resolved: self.resolved,
        }
    }

    /// About how many bytes the graph takes in memory: 500 per reference (what a graph of 2,600 files and 250k
    /// references measured, resolution and indexes included), 300 per definition and 200 per file, plus the
    /// text of the strings beyond the 24 bytes of a short name that those numbers hold. A name is kept by the
    /// definition and again by each index that is keyed by it (three or four copies), so a hostile file with
    /// names at the storage cap (256 bytes) costs several times what its rows say. Only the server's cache of
    /// graphs and the worker's record of the graph's size read it.
    pub fn estimated_bytes(&self) -> usize {
        let long = |text: &str| text.len().saturating_sub(SHORT_TEXT);
        let long_opt = |text: &Option<String>| text.as_deref().map_or(0, long);
        let mut text = 0_usize;

        for file in &self.files {
            text += 4 * long(&file.path) + long(&file.pkg) + file.segs.iter().map(|seg| long(seg)).sum::<usize>();
        }

        for sym in &self.syms {
            text += 4 * long(&sym.name) + 3 * long_opt(&sym.parent) + long(&sym.sig) + long_opt(&sym.ret);
        }

        for r in &self.refs {
            text += 2 * long(&r.name) + long_opt(&r.qual) + long_opt(&r.recv);
        }

        for ((_, local), import) in &self.imports {
            text += long(local) + long(&import.orig) + long(&import.source);
        }

        // a function kept in a property
        for (key, prop) in &self.props {
            text += long(key)
                + prop
                    .bound
                    .iter()
                    .map(|b| 160 + 8 * (b.targets.len() + b.via.len()))
                    .sum::<usize>();
        }

        // a string that names nothing is kept with its places
        for (name, places) in &self.loose {
            text += long(name) + places.len() * 48;
        }

        // a word literal is kept with the definitions that write it
        for (word, syms) in &self.words {
            text += 64 + long(word) + syms.len() * 8;
        }

        self.refs
            .len()
            .saturating_mul(500)
            .saturating_add(self.syms.len().saturating_mul(300))
            .saturating_add(self.files.len().saturating_mul(200))
            .saturating_add(text)
            .saturating_add(self.types.estimated_bytes())
    }

    /// Runs the whole-graph ranking that `code_map` starts from, and drops it: a worker process does it
    /// within its limits, so a repository whose ranking would exhaust them is found there.
    pub fn trial(&self, pace: &Pace) -> Result<(), String> {
        let ranks = self.rank_paced(None, &|| pace.check())?;

        std::hint::black_box(ranks.files.len() + ranks.syms.len());

        Ok(())
    }

    fn kind_ok(ref_kind: &str, sym: &Sym) -> bool {
        let kind = sym.kind.as_str();

        match ref_kind {
            "macro" => kind == "macro",
            "mcall" => matches!(kind, "method" | "member") || (sym.parent.is_some() && kind == "fn"),
            "type" => TYPE_KINDS.contains(&kind),
            "component" => matches!(kind, "component" | "class" | "fn" | "const"),
            "str" => matches!(kind, "fn" | "method" | "class" | "component"),
            "use" => matches!(kind, "const" | "static" | "enum_member"),
            "fnref" => matches!(kind, "fn" | "class" | "component"),
            _ => !matches!(kind, "type" | "interface" | "trait" | "macro"),
        }
    }

    fn resolve(&self, ri: usize) -> Resolved {
        let Some(r) = self.refs.get(ri) else {
            return Resolved::none();
        };
        let settled = self.resolve_ref(r);

        if r.kind == "mcall" && settled.targets.is_empty() {
            return self.resolve_prop(r).unwrap_or(settled);
        }

        settled
    }

    /// True when `s` can be what `r` means, by language and kind alone. `namespace` says `r` calls a member
    /// of a namespace import (`import * as ns from`), which is an export of that module, a function or a
    /// class, not a method.
    fn candidate(&self, r: &Ref, lang: &str, skip_methods: bool, namespace: bool, s: usize) -> bool {
        self.lang_of_sym(s) == lang
            && self.syms.get(s).is_some_and(|sym| {
                let fits = if namespace {
                    sym.parent.is_none() && matches!(sym.kind.as_str(), "fn" | "const" | "class")
                } else {
                    Self::kind_ok(&r.kind, sym)
                };

                fits && !(skip_methods && sym.kind == "method")
                    && !(sym.kind == "enum_member" && r.kind != "use" && r.qual.is_none())
            })
    }

    /// True when `qual` is a namespace import (`import * as qual from`) of `file`.
    fn is_namespace(&self, file: usize, qual: Option<&str>) -> bool {
        qual.is_some_and(|qual| {
            self.imports
                .get(&(file, qual.trim().to_string()))
                .is_some_and(|import| import.orig == "*")
        })
    }

    /// Takes `n` from what the scans may still visit; false when the budget is spent.
    fn spend(&self, n: usize) -> bool {
        self.scan_left
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| left.checked_sub(n))
            .is_ok()
    }

    /// The members of `list` that pass `keep`, for a narrow step: `None` when there are more than
    /// [`MAX_SETTLED`] of them or the list is longer than [`MAX_SCAN`] (ambiguous: the members past the
    /// window are not known, so a part of the list must not pass for all of it), or the scan budget is spent.
    fn settled(&self, list: Option<&Vec<usize>>, keep: impl Fn(usize) -> bool) -> Option<Vec<usize>> {
        let list = list.map_or(&[][..], Vec::as_slice);

        if list.len() > MAX_SCAN || !self.spend(list.len()) {
            return None;
        }

        let found: Vec<usize> = list
            .iter()
            .copied()
            .filter(|s| keep(*s))
            .take(MAX_SETTLED + 1)
            .collect();

        (found.len() <= MAX_SETTLED).then_some(found)
    }

    /// Every member of `list` that passes `keep`, for the wide steps: `None` when the list is longer than
    /// [`MAX_SCAN`] or the scan budget is spent.
    fn scanned(&self, list: &[usize], keep: impl Fn(usize) -> bool) -> Option<Vec<usize>> {
        if list.len() > MAX_SCAN || !self.spend(list.len()) {
            return None;
        }

        Some(list.iter().copied().filter(|s| keep(*s)).collect())
    }

    fn resolve_ref(&self, r: &Ref) -> Resolved {
        let Some(file) = self.files.get(r.file) else {
            return Resolved::none();
        };
        let Some(rules) = rules(file.lang) else {
            return Resolved::none();
        };

        let import = self.imports.get(&(r.file, r.name.clone()));
        let name = import
            .filter(|i| i.orig != "*")
            .map_or(r.name.as_str(), |i| i.orig.as_str());

        if let Some(settled) = rules.before(self, r, import) {
            return Resolved::sure(weigh(settled));
        }

        let Some(named) = self.by_name.get(name) else {
            return Resolved::none();
        };

        let bare_call = r.kind == "call" && r.qual.is_none();
        let skip_methods = rules.bare_call_skips_methods() && bare_call;
        let namespace = r.kind == "mcall" && self.is_namespace(r.file, r.qual.as_deref());
        let keep = |s: usize| self.candidate(r, file.lang, skip_methods, namespace, s);

        // 0. the receiver's written type: a method call only means the methods of that type
        let mut owned: Option<Vec<usize>> = None;

        if r.kind == "mcall"
            && let Some(ty) = self.receiver_type(r)
        {
            let Some(own) = self.settled(self.by_owner.get(&(name.to_string(), ty.clone())), keep) else {
                return Resolved::none();
            };

            if !own.is_empty() {
                owned = Some(own);
            } else if !self.defines_type(file.lang, &ty) {
                return Resolved::none(); // a method of a type the repository does not define
            }
        }

        let typed = owned.is_some();

        // 1. the same file wins for unqualified names (a qualified `git::x()` means another module)
        let same = match &owned {
            Some(own) => own
                .iter()
                .copied()
                .filter(|s| self.syms.get(*s).is_some_and(|sym| sym.file == r.file))
                .collect(),
            None => {
                let Some(same) = self.settled(self.by_file_name.get(&(r.file, name.to_string())), keep) else {
                    return Resolved::none();
                };

                same
            }
        };

        if !same.is_empty() && r.qual.is_none() {
            return Resolved::sure(weigh(same));
        }

        // 2. the qualifier: Type::method, module::item, namespace.item
        let qual = r.qual.as_deref().map(str::trim);

        if !typed && let Some(qual) = qual {
            let last = qual.rsplit("::").next().unwrap_or(qual);
            let Some(by_parent) = self.settled(self.by_owner.get(&(name.to_string(), last.to_string())), keep) else {
                return Resolved::none();
            };

            if !by_parent.is_empty() {
                return Resolved::sure(weigh(by_parent));
            }
        }

        // the wide steps look at every definition of the name that is in reach
        let Some(mut cands) = owned.or_else(|| self.scanned(named, keep)) else {
            return Resolved::none();
        };

        if cands.is_empty() {
            return Resolved::none();
        }

        if !typed
            && let Some(qual) = qual
            && let Some(settled) = rules.qualified(self, r, qual, &mut cands)
        {
            return Resolved::sure(weigh(settled));
        }

        // 3. the explicit import
        if !typed
            && let Some(import) = import
            && let Some(settled) = rules.imported(self, r, import, &cands)
        {
            return Resolved::sure(weigh(settled));
        }

        // a function passed as a value only means the one written or imported here
        if r.kind == "fnref" {
            return Resolved::none();
        }

        // 4. the same package, then non-test definitions
        let same_package: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|s| self.file_of(*s).is_some_and(|f| f.pkg == file.pkg))
            .collect();

        if bare_call && same_package.is_empty() && rules.is_builtin(name) {
            return Resolved::none(); // `len(x)`, `append(x)`: the language's own, nothing of the repository
        }

        if !same_package.is_empty() {
            cands = same_package;
        } else if rules.own_package_only() && r.kind != "mcall" {
            return Resolved::none(); // unqualified Go names only come from the own package (or builtins)
        }

        if !file.test {
            let production: Vec<usize> = cands
                .iter()
                .copied()
                .filter(|s| self.file_of(*s).is_some_and(|f| !f.test))
                .collect();

            // production code never means a test double it did not import
            if production.is_empty() {
                return Resolved::none();
            }

            cands = production;
        }

        // 5. too ambiguous to be signal
        if cands.len() > MAX_CANDIDATES {
            return Resolved::none();
        }

        let soft = r.kind == "mcall" && !typed;
        let by_name_only = import.is_some_and(|import| rules.name_only(self, r.file, import));

        Resolved {
            targets: if soft || by_name_only {
                weigh_soft(cands)
            } else {
                weigh(cands)
            },
            soft,
        }
    }

    /// True when a type named `name` is defined in a file of `lang`.
    fn defines_type(&self, lang: &str, name: &str) -> bool {
        lang_name(lang).is_some_and(|lang| self.type_defs.contains(&(lang, name.to_string())))
    }

    /// A string literal naming a function or component defined elsewhere: a weak, framework-agnostic link
    /// (IPC commands, event handlers, route tables). It must cross languages or hit a definition without
    /// direct callers; ties go to entry points; at most 3 candidates.
    fn resolve_str(&self, ri: usize, callers: &[usize]) -> Vec<(usize, f32)> {
        let Some(r) = self.refs.get(ri) else {
            return Vec::new();
        };
        let Some(all) = self.by_name.get(&r.name) else {
            return Vec::new();
        };
        let lang = self.lang_of_ref(ri);
        let Some(mut cands) = self.scanned(all, |s| {
            self.syms
                .get(s)
                .is_some_and(|sym| sym.file != r.file && Self::kind_ok("str", sym))
        }) else {
            return Vec::new(); // a name defined too often to mean one of them
        };

        let other: Vec<usize> = cands.iter().copied().filter(|s| self.lang_of_sym(*s) != lang).collect();
        let crosses = !other.is_empty();

        if crosses {
            cands = other;
        }

        let entries: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|s| callers.get(*s).copied().unwrap_or(0) == 0)
            .collect();

        if !entries.is_empty() {
            cands = entries;
        } else if !crosses {
            return Vec::new(); // same language and already called directly: a data key, not a by-name link
        }

        if !self.files.get(r.file).is_some_and(|f| f.test) {
            let production: Vec<usize> = cands
                .iter()
                .copied()
                .filter(|s| self.syms.get(*s).is_some_and(|sym| !sym.test))
                .collect();

            if !production.is_empty() {
                cands = production;
            }
        }

        if cands.is_empty() || cands.len() > MAX_STRING_TARGETS {
            return Vec::new();
        }

        weigh(cands)
    }

    /// `path:line` of a symbol.
    fn loc(&self, sym: usize) -> String {
        match self.syms.get(sym) {
            Some(s) => format!(
                "{}:{}",
                shown(self.files.get(s.file).map_or("", |f| f.path.as_str())),
                s.line
            ),
            None => String::new(),
        }
    }

    /// `Type::name` or `name`.
    fn label(&self, sym: usize) -> String {
        match self.syms.get(sym) {
            Some(Sym {
                parent: Some(parent),
                name,
                ..
            }) => format!("{}::{}", shown(parent), shown(name)),
            Some(s) => shown(&s.name).into_owned(),
            None => String::new(),
        }
    }

    /// The weight of the resolution of reference `ri` to symbol `sym` (1.0 when unknown).
    fn weight(&self, ri: usize, sym: usize) -> f32 {
        self.res
            .get(ri)
            .and_then(|targets| targets.iter().find(|t| t.0 == sym))
            .map_or(1.0, |t| t.1)
    }

    /// Definitions a query names: `name`, `Type::name`, `module::name` or `path/fragment::name`; the most
    /// used non-test ones first.
    fn find(&self, query: &str) -> Vec<usize> {
        let split = query
            .rsplit_once("::")
            .or_else(|| query.rsplit_once('.').filter(|(qual, _)| !qual.contains('/')));
        let (qual, name) = match split {
            Some((qual, name)) => (Some(qual), name),
            None => (None, query),
        };
        let mut hits: Vec<usize> = self.by_name.get(name).cloned().unwrap_or_default();

        if let Some(qual) = qual {
            hits.retain(|s| {
                let Some(sym) = self.syms.get(*s) else {
                    return false;
                };
                let Some(file) = self.files.get(sym.file) else {
                    return false;
                };

                sym.parent.as_deref() == Some(qual)
                    || file.path.contains(qual)
                    || qual
                        .split("::")
                        .all(|seg| file.segs.iter().any(|x| x == seg) || file.pkg == seg)
            });
        }

        hits.sort_by_key(|s| {
            (
                self.file_of(*s).is_some_and(|f| f.test),
                std::cmp::Reverse(self.incoming.get(*s).map_or(0, Vec::len)),
                *s,
            )
        });

        hits
    }

    /// TS module specifier -> files it can mean. `None` = not found among the repository's relative paths and
    /// aliases, so most likely an external package.
    fn ts_module(&self, from: usize, source: &str) -> Option<Vec<usize>> {
        let dir = self.files.get(from)?.path.rsplit_once('/').map_or("", |(dir, _)| dir);

        if source.starts_with('.') {
            return Some(self.ts_files(&normalize(&format!("{dir}/{source}"))));
        }

        // several configs may define the same alias for their own folders (a monorepo with an `@/*` in
        // each app): the first that leads to a file wins
        let mut matched = false;

        for (prefix, target) in &self.packages.aliases {
            let under = source == prefix
                || source
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|rest| rest.starts_with('/'));

            if !under {
                continue;
            }

            matched = true;

            let found = self.ts_files(&normalize(&format!(
                "{target}/{}",
                source.get(prefix.len()..).unwrap_or("")
            )));

            if !found.is_empty() {
                return Some(found);
            }
        }

        matched.then(Vec::new)
    }

    /// The files a module path (without extension, or with `.js` for a `.ts`) names.
    fn ts_files(&self, base: &str) -> Vec<usize> {
        let base = base.trim_end_matches(".js");

        [
            "",
            ".ts",
            ".tsx",
            ".js",
            ".jsx",
            ".svelte",
            ".vue",
            ".astro",
            ".svelte.ts",
            "/index.ts",
            "/index.tsx",
            "/index.js",
            "/index.svelte.ts",
        ]
        .iter()
        .filter_map(|ext| self.path_idx.get(&format!("{base}{ext}")).copied())
        .collect()
    }

    /// Go import path -> files of that package directory. `None` = standard library or external module.
    fn go_package(&self, source: &str) -> Option<Vec<usize>> {
        let (module, dir) = self.packages.gomods.iter().find(|(module, _)| {
            source == module
                || source
                    .strip_prefix(module.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })?;
        let dir = normalize(&format!("{dir}/{}", source.get(module.len()..).unwrap_or("")));

        Some(self.go_dirs.get(&dir).cloned().unwrap_or_default())
    }
}

/// Polls the pace once every [`POLL_EVERY`] steps.
/// The resident memory of this process, in bytes, where the system says (Linux). A worker compares it before
/// and after it loads a graph to record how much memory the graph took.
pub fn resident_bytes() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let kib: usize = status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()?;

        Some(kib.saturating_mul(1024))
    }

    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn poll(pace: &Pace, count: usize) -> Result<(), String> {
    if count.is_multiple_of(POLL_EVERY) {
        pace.check()?;
    }

    Ok(())
}

fn load_packages(conn: &Connection, repo: i64) -> Result<Packages, String> {
    let mut packages = Packages::default();
    let mut statement = conn
        .prepare("SELECT kind, name, dir FROM packages WHERE repo_id = ?1 ORDER BY kind, name, dir")
        .map_err(db)?;
    let rows = statement
        .query_map([repo], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(db)?;

    for row in rows {
        let (kind, name, dir) = row.map_err(db)?;

        match kind.as_str() {
            "crate" => {
                packages.crates.insert(name, dir);
            }
            "gomod" => packages.gomods.push((name, dir)),
            "alias" => packages.aliases.push((name, dir)),
            "pyroot" => packages.pyroots.push(dir),
            _ => {}
        }
    }

    packages
        .aliases
        .sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
    packages
        .pyroots
        .sort_by_key(|dir| (dir.matches('/').count(), dir.clone()));

    Ok(packages)
}

/// Text from the repository as the agent reads it: a name, a path or a signature with a line break, an escape
/// or a bidirectional control in it is written out (`\n`, `\u{1b}`), so a repository cannot forge lines of an
/// answer or hide text in it.
pub(super) fn shown(text: &str) -> std::borrow::Cow<'_, str> {
    crate::support::text::display_escaped(text)
}

/// A rough token count: one per four bytes.
pub fn tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}
