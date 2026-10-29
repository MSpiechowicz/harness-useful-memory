//! Rust resolution: crate and module paths (`crate::git::diff`, a crate named in `Cargo.toml`).

use super::{Graph, Imp, Ref, Rules};

pub(super) struct RustRules;

pub(super) static RULES: RustRules = RustRules;

impl Rules for RustRules {
    fn qualified(&self, graph: &Graph, r: &Ref, qual: &str, cands: &mut Vec<usize>) -> Option<Vec<usize>> {
        let last = qual.rsplit("::").next().unwrap_or(qual);

        if last.starts_with(|c: char| c.is_uppercase()) {
            return Some(Vec::new()); // a method of a type we don't define (Vec::new, Some, ...)
        }

        // expand an imported module: `use crate::git; git::diff()`
        let head = qual.split("::").next().unwrap_or(qual);
        let full = match graph.imports.get(&(r.file, head.to_string())) {
            Some(import) => format!("{}{}", import.source, qual.get(head.len()..).unwrap_or("")),
            None => qual.to_string(),
        };
        let segs: Vec<&str> = full.split("::").map(str::trim).collect();

        match graph.rust_path(r.file, &segs, cands) {
            None => Some(Vec::new()),
            Some(narrowed) if !narrowed.is_empty() => {
                *cands = narrowed;
                None
            }
            Some(_) => None,
        }
    }

    fn external(&self, graph: &Graph, file: usize, import: &Imp) -> bool {
        let segs: Vec<&str> = import.source.split("::").map(str::trim).collect();
        let module = segs.get(..segs.len().saturating_sub(1)).unwrap_or(&[]);

        graph.rust_path(file, module, &[]).is_none()
    }

    fn is_builtin(&self, name: &str) -> bool {
        matches!(
            name,
            "drop" | "Some" | "Ok" | "Err" | "Box" | "panic" | "todo" | "unreachable"
        )
    }

    fn imported(&self, graph: &Graph, r: &Ref, import: &Imp, cands: &[usize]) -> Option<Vec<usize>> {
        let segs: Vec<&str> = import.source.split("::").map(str::trim).collect();
        let module = segs.get(..segs.len().saturating_sub(1)).unwrap_or(&[]);

        match graph.rust_path(r.file, module, cands) {
            None => Some(Vec::new()),
            Some(narrowed) if !narrowed.is_empty() => Some(narrowed),
            Some(_) => None,
        }
    }
}

impl Graph {
    /// A Rust path (without the item name) -> the candidates narrowed to its crate and module. `None` =
    /// external (`std::`, `serde_json::`, ...).
    pub(super) fn rust_path(&self, from: usize, segs: &[&str], cands: &[usize]) -> Option<Vec<usize>> {
        let Some(first) = segs.first() else {
            return Some(cands.to_vec());
        };
        let here = &self.files.get(from)?.pkg;
        let (pkg, rest): (String, &[&str]) = match *first {
            "crate" | "self" | "super" => (here.clone(), segs.get(1..).unwrap_or(&[])),
            name if self.packages.crates.contains_key(name) => (name.to_string(), segs.get(1..).unwrap_or(&[])),
            name if self.pkg_mods.get(here).is_some_and(|mods| mods.contains(name)) => (here.clone(), segs),
            _ => return None,
        };

        let in_pkg: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|s| self.file_of(*s).is_some_and(|f| f.pkg == pkg))
            .collect();
        let score = |s: &usize| {
            self.file_of(*s).map_or(0, |f| {
                rest.iter().filter(|seg| f.segs.iter().any(|x| x == **seg)).count()
            })
        };
        let best = in_pkg.iter().map(score).max().unwrap_or(0);

        Some(in_pkg.into_iter().filter(|s| score(s) == best).collect())
    }
}
