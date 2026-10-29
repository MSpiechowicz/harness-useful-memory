//! Go resolution: a package is a directory, its import path comes from `go.mod`.

use super::{Graph, Imp, Ref, Rules};

pub(super) struct GoRules;

pub(super) static RULES: GoRules = GoRules;

/// The predeclared functions and conversions.
const BUILTINS: &[&str] = &[
    "append", "cap", "clear", "close", "complex", "copy", "delete", "imag", "len", "make", "max", "min", "new",
    "panic", "print", "println", "real", "recover", "string", "int", "int8", "int16", "int32", "int64", "uint",
    "uint8", "uint16", "uint32", "uint64", "uintptr", "float32", "float64", "byte", "rune", "bool", "error",
];

impl Rules for GoRules {
    fn qualified(&self, graph: &Graph, r: &Ref, qual: &str, cands: &mut Vec<usize>) -> Option<Vec<usize>> {
        let import = graph.imports.get(&(r.file, qual.to_string()))?;

        let Some(modules) = graph.go_package(&import.source) else {
            return Some(Vec::new()); // the standard library or an external module
        };
        let inside: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|s| graph.syms.get(*s).is_some_and(|sym| modules.contains(&sym.file)))
            .collect();

        (!inside.is_empty()).then_some(inside)
    }

    fn package_of(&self, path: &str) -> Option<String> {
        Some(
            path.rsplit_once('/')
                .map(|(dir, _)| dir.to_string())
                .unwrap_or_default(),
        )
    }

    fn own_package_only(&self) -> bool {
        true
    }

    fn bare_call_skips_methods(&self) -> bool {
        true
    }

    fn external(&self, graph: &Graph, _file: usize, import: &Imp) -> bool {
        graph.go_package(&import.source).is_none()
    }

    fn is_builtin(&self, name: &str) -> bool {
        BUILTINS.contains(&name)
    }
}
