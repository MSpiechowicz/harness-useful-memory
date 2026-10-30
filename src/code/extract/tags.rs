//! The generic path: any grammar that ships a `tags.scm` (`@definition.*`, `@reference.*`, `@name`).
//!
//! A language supplies its grammar, its tags query, how to find the type that owns a method and how to
//! read its imports. The query is compiled once per language ([`TagLang::compiled`]).

use std::collections::HashSet;
use std::ops::ControlFlow;
use std::sync::OnceLock;

use tree_sitter::{Language, Node, Parser, Query, QueryCursor, QueryCursorOptions, StreamingIterator};

use super::{
    DROPPED, Extract, Import, MAX_DEPTH, Ref, Sym, first_line, ident_like, in_credential_binding, is_own_name,
    out_of_time, text, word_like,
};

/// The scans below look at the file's clock every this many steps.
const CHECK_EVERY: usize = 4096;

/// What a language adds to the generic path.
pub(super) struct Hooks {
    /// The type that owns a method definition node.
    pub receiver: fn(Node, &[u8]) -> Option<String>,
    /// The imports of a file, from its root node.
    pub imports: fn(Node, &[u8], &mut Vec<Import>),
    /// The receiver expression a node stands for (see `graph/infer.rs`).
    pub operand: fn(Node, &[u8]) -> Option<String>,
    /// Reads what the tags query does not see (bindings, return types, constants) once the definitions
    /// and references are in `out`; the last argument is the index of the first symbol of this file.
    pub enrich: fn(Node, &[u8], &mut Extract, usize),
}

/// One language read through its tags query.
pub(super) struct TagLang {
    pub language: fn() -> Language,
    pub query: &'static str,
    pub hooks: Hooks,
    /// The string literal node kinds that may name a symbol.
    pub string_kinds: &'static [&'static str],
    /// The node kind whose `operand`.`field` is a qualified use of an imported package (`pkg.Func`).
    pub selector_kind: &'static str,
    /// One compiled query per language, never shared between languages.
    compiled: OnceLock<Option<Query>>,
}

impl TagLang {
    pub(super) const fn new(
        language: fn() -> Language,
        query: &'static str,
        hooks: Hooks,
        string_kinds: &'static [&'static str],
        selector_kind: &'static str,
    ) -> Self {
        Self {
            language,
            query,
            hooks,
            string_kinds,
            selector_kind,
            compiled: OnceLock::new(),
        }
    }

    /// The compiled tags query; `None` when the grammar's query does not compile.
    fn compiled(&self) -> Option<&Query> {
        self.compiled
            .get_or_init(|| Query::new(&(self.language)(), self.query).ok())
            .as_ref()
    }
}

fn def_kind_from_tag(tag: &str, node: Node) -> Option<&'static str> {
    Some(match tag {
        "definition.function" => "fn",
        "definition.method" => "method",
        "definition.class" => "class",
        "definition.interface" => "interface",
        "definition.constant" => "const",
        "definition.macro" => "macro",
        "definition.type" => match node.child_by_field_name("type").map(|ty| ty.kind()) {
            Some("struct_type") => "struct",
            Some("interface_type") => "interface",
            _ => "type",
        },
        _ => return None,
    })
}

/// The innermost range of `ranges` (sorted by start, properly nested or disjoint) that holds each of the
/// sorted `positions`.
fn enclosing(ranges: &[(usize, usize, usize)], positions: &[usize]) -> Vec<Option<usize>> {
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut next = 0;
    let mut result = Vec::with_capacity(positions.len());

    for &at in positions {
        while let Some(&(start, end, sym)) = ranges.get(next) {
            if start > at {
                break;
            }

            stack.push((end, sym));
            next += 1;
        }

        while stack.last().is_some_and(|(end, _)| *end <= at) {
            stack.pop();
        }

        result.push(stack.last().map(|(_, sym)| *sym));
    }

    result
}

/// Reads `src` with the language's tags query.
pub(super) fn extract(parser: &mut Parser, lang: &'static TagLang, src: &str, out: &mut Extract) {
    let language = (lang.language)();
    let Some(tree) = super::parse(parser, &language, src) else {
        return;
    };
    let Some(query) = lang.compiled() else {
        return;
    };

    let bytes = src.as_bytes();
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();

    // no match starts deeper than the walks of the other languages go (the parent of a node costs its depth)
    cursor.set_max_start_depth(u32::try_from(MAX_DEPTH).ok());
    let mut ranges: Vec<(usize, usize, usize)> = Vec::new(); // (start, end, symbol)
    let mut pending: Vec<(usize, Ref)> = Vec::new();
    // the references made on a value or a package: `x.f()` / `pkg.F()`
    let mut selected: HashSet<usize> = HashSet::new();
    let first_sym = out.syms.len();
    // the query stops with the file's time, as the parse does
    let mut give_up = |_: &tree_sitter::QueryCursorState| {
        if out_of_time() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let mut matches = cursor.matches_with_options(
        query,
        tree.root_node(),
        bytes,
        QueryCursorOptions::new().progress_callback(&mut give_up),
    );

    while let Some(found) = matches.next() {
        let mut name = None;
        let mut role = None;

        for capture in found.captures() {
            let Some(capture_name) = names.get(capture.index as usize).copied() else {
                continue;
            };

            if capture_name == "name" {
                name = Some(capture.node);
            }

            if capture_name.starts_with("definition.") || capture_name.starts_with("reference.") {
                role = Some((capture_name, capture.node));
            }
        }

        let (Some(name), Some((tag, node))) = (name, role) else {
            continue;
        };
        let line = name.start_position().row + 1;

        if let Some(kind) = def_kind_from_tag(tag, node) {
            let sym = out.add_sym(Sym {
                name: text(name, bytes).to_string(),
                kind,
                parent: if kind == "method" {
                    (lang.hooks.receiver)(node, bytes)
                } else {
                    None
                },
                line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                sig: first_line(node, bytes),
                ret: None,
                test: false,
            });

            // what is inside a definition that was not kept belongs to the one around it, or to none
            if sym != DROPPED {
                ranges.push((node.start_byte(), node.end_byte(), sym));
            }
        } else if tag == "reference.call" || tag == "reference.type" {
            // the `name` of a qualified type (`events.Kind`) is a use, the `name` of a type spec a definition
            if tag == "reference.type"
                && is_own_name(name)
                && name.parent().is_some_and(|p| p.kind() != "qualified_type")
            {
                continue;
            }

            // `pkg.Func()` / `x.Method()` / `pkg.Type`: the identifier next to the name is the qualifier
            let qual = name.parent().and_then(|parent| {
                let mut walk = parent.walk();
                let sibling = parent
                    .named_children(&mut walk)
                    .find(|x| x.id() != name.id() && x.kind().ends_with("identifier"));

                sibling.map(|x| text(x, bytes).to_string())
            });
            let kind = if tag == "reference.type" { "type" } else { "call" };
            let selector = name.parent().filter(|parent| parent.kind() == lang.selector_kind);
            let recv = selector
                .and_then(|parent| parent.child_by_field_name("operand"))
                .and_then(|operand| (lang.hooks.operand)(operand, bytes));

            if tag == "reference.call" && selector.is_some() {
                selected.insert(name.start_byte());
            }

            pending.push((
                name.start_byte(),
                Ref {
                    from: None,
                    name: text(name, bytes).to_string(),
                    kind,
                    qual,
                    recv,
                    line,
                    handler: false,
                },
            ));
        }
    }

    let mut read_imports = Vec::new();

    (lang.hooks.imports)(tree.root_node(), bytes, &mut read_imports);

    for import in read_imports {
        out.add_import(import);
    }

    let imported: HashSet<String> = out.imports.iter().map(|import| import.local.clone()).collect();

    // `pkg.Func` used as a value (callbacks, tables): the tags query only sees calls; string literals may
    // name a symbol
    let seen: HashSet<usize> = pending.iter().map(|(at, _)| *at).collect();
    let mut stack = vec![(tree.root_node(), 0_usize)];
    let mut steps = 0_usize;

    while let Some((node, depth)) = stack.pop() {
        steps += 1;

        // finding the parent of a node costs its depth: nothing this deep is looked at
        if depth > MAX_DEPTH {
            continue;
        }

        if steps.is_multiple_of(CHECK_EVERY) && out_of_time() {
            out.dropped += 1;

            break;
        }

        if lang.string_kinds.contains(&node.kind()) {
            let value = text(node, bytes).trim_matches('"');

            let kind = if ident_like(value) {
                Some("str")
            } else if word_like(value) && !in_credential_binding(node, bytes) {
                Some("word")
            } else {
                None
            };

            if let Some(kind) = kind {
                pending.push((
                    node.start_byte(),
                    Ref {
                        from: None,
                        name: value.to_string(),
                        kind,
                        qual: None,
                        recv: None,
                        line: node.start_position().row + 1,
                        handler: false,
                    },
                ));
            }
        }

        if node.kind() == lang.selector_kind
            && let (Some(operand), Some(field)) =
                (node.child_by_field_name("operand"), node.child_by_field_name("field"))
        {
            let qual = text(operand, bytes);

            if operand.kind() == "identifier" && imported.contains(qual) && !seen.contains(&field.start_byte()) {
                pending.push((
                    field.start_byte(),
                    Ref {
                        from: None,
                        name: text(field, bytes).to_string(),
                        kind: "call",
                        qual: Some(qual.to_string()),
                        recv: None,
                        line: field.start_position().row + 1,
                        // `case events.EvtNew:` mentions a constant of the package, which reads like a call
                        handler: super::handled(field, bytes),
                    },
                ));
            }
        }

        let mut walk = node.walk();
        stack.extend(node.named_children(&mut walk).map(|child| (child, depth + 1)));
    }

    ranges.sort_unstable();
    pending.sort_by_key(|(at, _)| *at);

    let positions: Vec<usize> = pending.iter().map(|(at, _)| *at).collect();
    let owners = enclosing(&ranges, &positions);

    for ((at, mut reference), owner) in pending.into_iter().zip(owners) {
        reference.from = owner.filter(|sym| *sym >= first_sym);

        if reference.kind == "call" && selected.contains(&at) {
            // `pkg.F()` names a package; anything else before the dot (a variable, a field, a call result)
            // is a value: a method call
            let package = reference.qual.as_deref().is_some_and(|qual| imported.contains(qual));

            if package {
                reference.recv = None;
            } else {
                reference.kind = "mcall";
            }
        } else if reference.kind == "call" && reference.qual.as_deref().is_some_and(|qual| !imported.contains(qual)) {
            reference.kind = "mcall";
        }

        if reference.kind != "mcall" {
            reference.recv = None;
        }

        out.add_ref(reference);
    }

    (lang.hooks.enrich)(tree.root_node(), bytes, out, first_sym);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_innermost_enclosing_range_wins() {
        let ranges = [(0, 100, 0), (10, 20, 1), (50, 60, 2)];
        let got = enclosing(&ranges, &[5, 15, 25, 55, 100, 150]);

        assert_eq!(got, vec![Some(0), Some(1), Some(0), Some(2), None, None]);
    }
}
