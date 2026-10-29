//! Python resolution: a package is a folder, a module is `a/b.py` or `a/b/__init__.py`.
//!
//! An absolute import (`import a.b`, `from a.b import c`) is looked up under every source root (`pyroot`
//! packages: the repository root, `src/`, and the folder above each top-level `__init__.py` chain), plus
//! the importing file's own folder when that folder is not a package (a script, a test folder). A module
//! no root holds (the standard library, site-packages) stays unresolved. A relative import is resolved
//! against the importing file's folder: one dot is that folder, each further dot goes one folder up.
//!
//! `x.f()` where `x` is an imported module resolves to that module's file, `Class.m()` and `self.m()` to
//! the method of the class (the extractor writes the enclosing class as the qualifier of `self`/`cls`).
//! A name imported from a package folder that defines it in a submodule (`from pkg import Client`
//! re-exported by `pkg/__init__.py`) resolves to the definitions below that folder.

use super::{Graph, Imp, MAX_CANDIDATES, Ref, Rules};

pub(super) struct PythonRules;

pub(super) static RULES: PythonRules = PythonRules;

impl Rules for PythonRules {
    fn before(&self, graph: &Graph, r: &Ref, import: Option<&Imp>) -> Option<Vec<usize>> {
        let import = import?;
        let (dots, segs) = parse_module(&import.source);

        // an import of the standard library or of site-packages
        graph.py_module(r.file, dots, &segs).is_none().then(Vec::new)
    }

    fn qualified(&self, graph: &Graph, r: &Ref, qual: &str, cands: &mut Vec<usize>) -> Option<Vec<usize>> {
        let mut parts = qual.split('.');
        let head = parts.next()?;
        let import = graph.imports.get(&(r.file, head.to_string()))?;

        // the dotted path the qualifier stands for: the import's module, its name, then the rest
        let (dots, mut segs) = parse_module(&import.source);

        if import.orig != "*" {
            segs.push(import.orig.as_str());
        }

        segs.extend(parts);

        // the longest prefix that is a module; what follows names a class
        for take in (0..=segs.len()).rev() {
            if take == 0 && dots == 0 {
                break;
            }

            let Some(files) = graph.py_module(r.file, dots, segs.get(..take).unwrap_or(&[])) else {
                continue;
            };

            if files.is_empty() {
                continue;
            }

            let mut found = graph.py_pool(&files, cands);

            if let Some(class) = segs.get(take..).and_then(<[&str]>::last) {
                found.retain(|s| {
                    graph
                        .syms
                        .get(*s)
                        .is_some_and(|sym| sym.parent.as_deref() == Some(*class))
                });
            }

            return (!found.is_empty()).then_some(found);
        }

        Some(Vec::new()) // no module of the repository
    }

    fn imported(&self, graph: &Graph, r: &Ref, import: &Imp, cands: &[usize]) -> Option<Vec<usize>> {
        let (dots, segs) = parse_module(&import.source);
        let files = graph.py_module(r.file, dots, &segs)?;
        let found = graph.py_pool(&files, cands);

        (!found.is_empty()).then_some(found)
    }

    fn package_of(&self, path: &str) -> Option<String> {
        Some(dir_of(path).to_string())
    }

    fn bare_call_skips_methods(&self) -> bool {
        true
    }

    fn external(&self, graph: &Graph, file: usize, import: &Imp) -> bool {
        let (dots, segs) = parse_module(&import.source);

        graph.py_module(file, dots, &segs).is_none()
    }

    fn is_builtin(&self, name: &str) -> bool {
        BUILTINS.contains(&name)
    }
}

/// The builtin functions and types.
const BUILTINS: &[&str] = &[
    "abs",
    "all",
    "any",
    "ascii",
    "bin",
    "bool",
    "breakpoint",
    "bytearray",
    "bytes",
    "callable",
    "chr",
    "classmethod",
    "compile",
    "complex",
    "delattr",
    "dict",
    "dir",
    "divmod",
    "enumerate",
    "eval",
    "exec",
    "filter",
    "float",
    "format",
    "frozenset",
    "getattr",
    "globals",
    "hasattr",
    "hash",
    "help",
    "hex",
    "id",
    "input",
    "int",
    "isinstance",
    "issubclass",
    "iter",
    "len",
    "list",
    "locals",
    "map",
    "max",
    "memoryview",
    "min",
    "next",
    "object",
    "oct",
    "open",
    "ord",
    "pow",
    "print",
    "property",
    "range",
    "repr",
    "reversed",
    "round",
    "set",
    "setattr",
    "slice",
    "sorted",
    "staticmethod",
    "str",
    "sum",
    "super",
    "tuple",
    "type",
    "vars",
    "zip",
];

/// `..m.n` -> (2, [m, n]); `.` -> (1, []); `a.b` -> (0, [a, b]).
fn parse_module(source: &str) -> (usize, Vec<&str>) {
    let rest = source.trim_start_matches('.');

    (
        source.len() - rest.len(),
        rest.split('.').filter(|seg| !seg.is_empty()).collect(),
    )
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn join(dir: &str, rest: &str) -> String {
    if dir.is_empty() {
        rest.to_string()
    } else {
        format!("{dir}/{rest}")
    }
}

impl Graph {
    /// A Python module -> its files. `dots` leading dots make it relative to the folder of file `from`.
    /// `None` = an absolute module no source root holds (external); a relative one always answers.
    fn py_module(&self, from: usize, dots: usize, segs: &[&str]) -> Option<Vec<usize>> {
        let dir = dir_of(&self.files.get(from)?.path);

        if dots > 0 {
            let mut base = dir;

            for _ in 1..dots {
                if base.is_empty() {
                    return None; // above the repository
                }

                base = dir_of(base);
            }

            return Some(self.py_files(base, segs));
        }

        let mut roots: Vec<&str> = self.packages.pyroots.iter().map(String::as_str).collect();

        // a script or a test folder: its own folder is on the import path
        if !self.path_idx.contains_key(&join(dir, "__init__.py")) {
            roots.push(dir);
        }

        let mut found: Vec<usize> = Vec::new();

        for root in roots {
            for file in self.py_files(root, segs) {
                if !found.contains(&file) {
                    found.push(file);
                }
            }
        }

        (!found.is_empty()).then_some(found)
    }

    /// `base/a/b.py` and `base/a/b/__init__.py` (`base/__init__.py` for no segments).
    fn py_files(&self, base: &str, segs: &[&str]) -> Vec<usize> {
        let path = join(base, &segs.join("/"));
        let candidates = if segs.is_empty() {
            vec![join(base, "__init__.py")]
        } else {
            vec![format!("{path}.py"), format!("{path}/__init__.py")]
        };

        candidates
            .iter()
            .filter_map(|file| self.path_idx.get(file).copied())
            .collect()
    }

    /// The candidates defined in `files`; when none is, those a package `__init__.py` among them
    /// re-exports, that is any below its folder (when that is few enough to be signal).
    fn py_pool(&self, files: &[usize], cands: &[usize]) -> Vec<usize> {
        let inside: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|s| self.syms.get(*s).is_some_and(|sym| files.contains(&sym.file)))
            .collect();

        if !inside.is_empty() {
            return inside;
        }

        let dirs: Vec<String> = files
            .iter()
            .filter_map(|f| self.files.get(*f))
            .filter_map(|f| f.path.strip_suffix("/__init__.py"))
            .map(|dir| format!("{dir}/"))
            .collect();
        let below: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|s| {
                self.file_of(*s)
                    .is_some_and(|f| dirs.iter().any(|dir| f.path.starts_with(dir.as_str())))
            })
            .collect();

        if below.len() > MAX_CANDIDATES {
            Vec::new()
        } else {
            below
        }
    }
}
