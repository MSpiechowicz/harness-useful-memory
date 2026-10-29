//! TypeScript, JavaScript and single-file-component resolution: relative specifiers, `tsconfig` path
//! aliases, namespace imports and default imports of components.
//!
//! A specifier that is not relative and matches no alias is most likely an npm package and stays external,
//! unless the repository defines the imported name: then it is an alias no config declares (or a workspace
//! package) and the name decides, as for any name in reach (marked `?` when several definitions fit).

use std::collections::{HashMap, HashSet, VecDeque};

use super::{Graph, Imp, MAX_SCAN, Ref, Rules};

/// How many `export ... from` links a name is followed through.
const MAX_REEXPORT_DEPTH: usize = 4;

/// The modules one search through re-exports visits at most.
const MAX_REEXPORT_VISITS: usize = 256;

/// The `export * from` and the modules per exported name one file keeps; the rest is not followed.
const MAX_STAR_REEXPORTS: usize = 256;
const MAX_NAMED_SOURCES: usize = 8;

/// What a file passes on with `export ... from`, without duplicates (a hostile file repeats one line a
/// thousand times to multiply the search).
#[derive(Default)]
pub(super) struct Reexports {
    /// Exported name -> the module specifiers it comes from.
    named: HashMap<String, Vec<String>>,
    /// The specifiers of `export * from`.
    stars: Vec<String>,
}

impl Reexports {
    /// Records `export { .. as exported } from source` (`exported` is `*` for `export * from source`).
    pub(super) fn add(&mut self, exported: &str, source: String) {
        let list = if exported == "*" {
            &mut self.stars
        } else {
            self.named.entry(exported.to_string()).or_default()
        };
        let cap = if exported == "*" {
            MAX_STAR_REEXPORTS
        } else {
            MAX_NAMED_SOURCES
        };

        if list.len() < cap && !list.contains(&source) {
            list.push(source);
        }
    }

    /// The specifiers `name` may come from.
    fn sources(&self, name: &str) -> impl Iterator<Item = &String> {
        self.named.get(name).into_iter().flatten().chain(self.stars.iter())
    }
}

pub(super) struct TsRules;

pub(super) static RULES: TsRules = TsRules;

/// Globals every JavaScript runtime has.
const BUILTINS: &[&str] = &[
    "parseInt",
    "parseFloat",
    "isNaN",
    "isFinite",
    "setTimeout",
    "setInterval",
    "clearTimeout",
    "clearInterval",
    "queueMicrotask",
    "structuredClone",
    "fetch",
    "require",
    "String",
    "Number",
    "Boolean",
    "Symbol",
    "BigInt",
    "Array",
    "Object",
    "Promise",
    "Error",
    "Date",
    "RegExp",
    "Map",
    "Set",
    "WeakMap",
    "WeakSet",
    "URL",
    "encodeURIComponent",
    "decodeURIComponent",
    "encodeURI",
    "decodeURI",
    "atob",
    "btoa",
    "alert",
    "confirm",
    "requestAnimationFrame",
    "cancelAnimationFrame",
];

const COMPONENT_EXTENSIONS: &[&str] = &[".svelte", ".vue", ".astro"];

impl Rules for TsRules {
    fn before(&self, graph: &Graph, r: &Ref, import: Option<&Imp>) -> Option<Vec<usize>> {
        let import = import?;

        let Some(modules) = graph.ts_module(r.file, &import.source) else {
            // an external package, unless the repository defines what is imported: then the name decides
            if graph.defines_import(r, import) {
                return None;
            }

            return Some(Vec::new());
        };

        // a default import of a component under another name: the module's component
        if let [module] = modules.as_slice()
            && r.kind != "type"
            && graph
                .files
                .get(*module)
                .is_some_and(|f| COMPONENT_EXTENSIONS.iter().any(|ext| f.path.ends_with(ext)))
        {
            let component = graph.file_syms.get(*module).and_then(|syms| {
                syms.iter()
                    .copied()
                    .find(|s| graph.syms.get(*s).is_some_and(|sym| sym.kind == "component"))
            });

            return Some(component.into_iter().collect());
        }

        None
    }

    fn qualified(&self, graph: &Graph, r: &Ref, qual: &str, cands: &mut Vec<usize>) -> Option<Vec<usize>> {
        let import = graph.imports.get(&(r.file, qual.to_string()))?;

        let Some(modules) = graph.ts_module(r.file, &import.source) else {
            // an npm package, or an alias no config declares: the name decides
            return bare(&import.source).then(Vec::new);
        };
        let inside: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|s| graph.syms.get(*s).is_some_and(|sym| modules.contains(&sym.file)))
            .collect();

        if !inside.is_empty() {
            return Some(inside);
        }

        // `ns.name()` on `import * as ns from`: an export the module has (or passes on), or nothing
        (import.orig == "*").then(|| graph.reexported(&modules, &r.name, cands))
    }

    fn bare_call_skips_methods(&self) -> bool {
        true
    }

    fn external(&self, graph: &Graph, file: usize, import: &Imp) -> bool {
        graph.ts_module(file, &import.source).is_none()
    }

    fn name_only(&self, graph: &Graph, file: usize, import: &Imp) -> bool {
        bare(&import.source) && graph.ts_module(file, &import.source).is_none()
    }

    fn is_builtin(&self, name: &str) -> bool {
        BUILTINS.contains(&name)
    }

    fn imported(&self, graph: &Graph, r: &Ref, import: &Imp, cands: &[usize]) -> Option<Vec<usize>> {
        let modules = graph.ts_module(r.file, &import.source)?;
        let inside: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|s| graph.syms.get(*s).is_some_and(|sym| modules.contains(&sym.file)))
            .collect();

        if !inside.is_empty() {
            return Some(inside);
        }

        // re-exported (`export { x } from`, `export * from`): follow it to the file that defines the name
        let followed = graph.reexported(&modules, &import.orig, cands);

        if !followed.is_empty() {
            return Some(followed);
        }

        // re-exported through an index: anything under that directory
        let dir = modules
            .first()
            .and_then(|module| graph.files.get(*module))
            .and_then(|f| f.path.rsplit_once('/'))
            .map(|(dir, _)| dir.to_string())?;
        let below: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|s| graph.file_of(*s).is_some_and(|f| f.path.starts_with(&dir)))
            .collect();

        (!below.is_empty()).then_some(below)
    }
}

/// True for a specifier that names a package (`react`, `@scope/pkg/sub`), not an alias or a path (`@/x`,
/// `~/x`, `#x`, `$lib/x`).
fn bare(source: &str) -> bool {
    match source.strip_prefix('@') {
        Some(scoped) => scoped.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()),
        None => source.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()),
    }
}

impl Graph {
    /// True when the repository defines what `import` brings in under the name `r` uses it by.
    fn defines_import(&self, r: &Ref, import: &Imp) -> bool {
        if import.orig == "*" {
            return false;
        }

        let Some(list) = self.by_name.get(import.orig.as_str()) else {
            return false;
        };
        let window = list.len().min(MAX_SCAN);

        self.spend(window)
            && list
                .iter()
                .take(window)
                .any(|s| self.candidate(r, "ts", false, false, *s))
    }

    /// The candidates defined in `modules` or, following `export ... from` of those files, in the modules
    /// they pass `name` on from (at most [`MAX_REEXPORT_DEPTH`] links deep). Every module is visited once,
    /// at most [`MAX_REEXPORT_VISITS`] of them per search, each visit paid from the scan budget: a barrel
    /// chain written to multiply the search ends with the name unresolved.
    fn reexported(&self, modules: &[usize], name: &str, cands: &[usize]) -> Vec<usize> {
        let mut found: Vec<usize> = Vec::new();
        let mut seen: HashSet<usize> = modules.iter().copied().collect();
        let mut queue: VecDeque<(usize, usize)> = modules.iter().map(|module| (*module, 0)).collect();
        let mut visits = 0_usize;

        while let Some((module, depth)) = queue.pop_front() {
            let Some(passed) = self.reexports.get(&module) else {
                continue;
            };

            if depth >= MAX_REEXPORT_DEPTH {
                continue;
            }

            visits += 1;

            if visits > MAX_REEXPORT_VISITS || !self.spend(1 + passed.stars.len()) {
                break;
            }

            for source in passed.sources(name) {
                let Some(targets) = self.ts_module(module, source) else {
                    continue;
                };
                let inside: Vec<usize> = cands
                    .iter()
                    .copied()
                    .filter(|s| self.syms.get(*s).is_some_and(|sym| targets.contains(&sym.file)))
                    .collect();

                if !inside.is_empty() {
                    found.extend(inside);

                    continue;
                }

                for target in targets {
                    if seen.insert(target) {
                        queue.push_back((target, depth + 1));
                    }
                }
            }
        }

        found.sort_unstable();
        found.dedup();

        found
    }
}
