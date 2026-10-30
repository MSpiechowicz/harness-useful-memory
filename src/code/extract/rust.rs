//! Rust: definitions, calls, macros, types, `use` declarations and string names.

use tree_sitter::{Node, Parser};

use super::{
    DROPPED, Extract, Import, MAX_DEPTH, MAX_STR, Placement, constant_like, fits, ident_like, is_own_name, last_seg,
    push_base, push_bind, push_def, push_mcall, push_ref, push_use, push_word, text, written_type,
};

/// Reads a Rust file.
pub(super) fn extract(parser: &mut Parser, _path: &str, src: &str, out: &mut Extract) {
    let Some(tree) = super::parse(parser, &tree_sitter_rust::LANGUAGE.into(), src) else {
        return;
    };

    Walk {
        src: src.as_bytes(),
        out,
        in_test: false,
    }
    .node(tree.root_node(), None, None, 0);
}

fn def_kind(node: Node) -> Option<&'static str> {
    match node.kind() {
        "function_item" | "function_signature_item" => Some("fn"),
        "struct_item" => Some("struct"),
        "enum_item" => Some("enum"),
        "union_item" => Some("union"),
        "trait_item" => Some("trait"),
        "type_item" => Some("type"),
        "const_item" => Some("const"),
        "static_item" => Some("static"),
        "macro_definition" => Some("macro"),
        _ => None,
    }
}

/// `(name, qualifier, is_method_call)` of the callee of a call expression.
fn callee(node: Node, src: &[u8], ctx: Option<&str>, depth: usize) -> Option<(String, Option<String>, bool)> {
    if depth > 8 {
        return None;
    }

    match node.kind() {
        "identifier" => Some((text(node, src).to_string(), None, false)),
        "scoped_identifier" => {
            let name = text(node.child_by_field_name("name")?, src).to_string();
            let qual = node.child_by_field_name("path").map(|path| {
                let path = text(path, src);

                if path == "Self" {
                    ctx.unwrap_or(path).to_string()
                } else {
                    path.to_string()
                }
            });

            Some((name, qual, false))
        }
        "field_expression" => Some((text(node.child_by_field_name("field")?, src).to_string(), None, true)),
        "generic_function" => callee(node.child_by_field_name("function")?, src, ctx, depth + 1),
        _ => None,
    }
}

/// `prefix::path`, or `None` when the result would be too long to store (a long prefix is not copied into
/// every name of a `use` list).
fn join(prefix: &str, path: &str) -> Option<String> {
    if prefix.len().saturating_add(path.len()).saturating_add(2) > MAX_STR {
        return None;
    }

    if prefix.is_empty() {
        Some(path.to_string())
    } else {
        Some(format!("{prefix}::{path}"))
    }
}

/// Collects the names a `use` declaration brings in.
fn collect_use(node: Node, src: &[u8], prefix: &str, out: &mut Extract, depth: usize) {
    if depth > 32 {
        return;
    }

    match node.kind() {
        "identifier" | "scoped_identifier" | "crate" | "self" | "super" => {
            let Some(full) = join(prefix, text(node, src)) else {
                out.dropped += 1;

                return;
            };
            let name = last_seg(&full);

            if name == "self" {
                // `use foo::{self}` imports the module `foo`
                let module = full.trim_end_matches("::self").to_string();
                let local = last_seg(&module);

                out.add_import(Import {
                    local: local.clone(),
                    orig: local,
                    source: module,
                });
            } else {
                out.add_import(Import {
                    local: name.clone(),
                    orig: name,
                    source: full,
                });
            }
        }
        "use_as_clause" => {
            let (Some(path), Some(alias)) = (node.child_by_field_name("path"), node.child_by_field_name("alias"))
            else {
                return;
            };
            let Some(full) = join(prefix, text(path, src)) else {
                out.dropped += 1;

                return;
            };

            out.add_import(Import {
                local: text(alias, src).to_string(),
                orig: last_seg(&full),
                source: full,
            });
        }
        "scoped_use_list" => {
            let path = node.child_by_field_name("path").map_or("", |path| text(path, src));

            if let Some(list) = node.child_by_field_name("list") {
                let Some(prefix) = join(prefix, path) else {
                    out.dropped += 1;

                    return;
                };
                let mut cursor = list.walk();

                for child in list.named_children(&mut cursor) {
                    collect_use(child, src, &prefix, out, depth + 1);
                }
            }
        }
        "use_list" => {
            let mut cursor = node.walk();

            for child in node.named_children(&mut cursor) {
                collect_use(child, src, prefix, out, depth + 1);
            }
        }
        _ => {} // use_wildcard: nothing named
    }
}

/// True when an attribute marks a test: `#[test]`, `#[tokio::test]`, `#[rstest]`, `#[cfg(test)]`.
fn test_attribute(attribute: &str) -> bool {
    let mut body = String::new();
    let mut in_string = false;

    for c in attribute
        .trim()
        .trim_start_matches('#')
        .trim_start_matches('[')
        .trim_end_matches(']')
        .chars()
    {
        if c == '"' {
            in_string = !in_string;
        } else if !in_string && !c.is_whitespace() {
            body.push(c);
        }
    }

    if let Some(condition) = body.strip_prefix("cfg(") {
        return !condition.contains("not(")
            && condition
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|word| word == "test");
    }

    let path = body.split(['(', '=']).next().unwrap_or("");
    let last = path.rsplit("::").next().unwrap_or(path);

    last == "test" || last == "rstest" || last.ends_with("_test")
}

fn has_test_attr(node: Node, src: &[u8]) -> bool {
    let mut previous = node.prev_named_sibling();

    while let Some(sibling) =
        previous.filter(|s| matches!(s.kind(), "attribute_item" | "line_comment" | "block_comment"))
    {
        if sibling.kind() == "attribute_item" && test_attribute(text(sibling, src)) {
            return true;
        }

        previous = sibling.prev_named_sibling();
    }

    false
}

/// Deeper receiver expressions are not followed.
const MAX_EXPR_DEPTH: usize = 12;

/// The type `ty` as written, with `Self` replaced by the type being implemented.
fn typed(node: Node, src: &[u8], ctx: Option<&str>) -> Option<String> {
    let ty = written_type(text(node, src))?;

    match (ty.as_str(), ctx) {
        ("Self", Some(owner)) => Some(owner.to_string()),
        _ => Some(ty),
    }
}

/// The receiver expression a node stands for (see `graph/infer.rs`), when it can be told from the syntax
/// and is short enough to store.
fn expr(node: Node, src: &[u8], ctx: Option<&str>, depth: usize) -> Option<String> {
    expr_of(node, src, ctx, depth).filter(|e| fits(e))
}

fn expr_of(node: Node, src: &[u8], ctx: Option<&str>, depth: usize) -> Option<String> {
    if depth > MAX_EXPR_DEPTH {
        return None;
    }

    match node.kind() {
        "identifier" | "self" => Some(text(node, src).to_string()),
        "field_expression" => {
            let field = node
                .child_by_field_name("field")
                .filter(|f| f.kind() == "field_identifier")?;
            let value = expr(node.child_by_field_name("value")?, src, ctx, depth + 1)?;

            Some(format!("{value}.{}", text(field, src)))
        }
        "call_expression" => {
            let function = node.child_by_field_name("function")?;

            match function.kind() {
                "identifier" => Some(format!("{}()", text(function, src))),
                "scoped_identifier" => {
                    let name = text(function.child_by_field_name("name")?, src);
                    let path = text(function.child_by_field_name("path")?, src);
                    let path = if path == "Self" { ctx? } else { path };

                    path.chars()
                        .all(|c| c.is_alphanumeric() || matches!(c, '_' | ':'))
                        .then(|| format!("{path}::{name}()"))
                }
                "field_expression" => Some(format!("{}()", expr(function, src, ctx, depth + 1)?)),
                "generic_function" => {
                    let inner = function.child_by_field_name("function")?;

                    match inner.kind() {
                        "identifier" => Some(format!("{}()", text(inner, src))),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        "reference_expression" | "parenthesized_expression" | "await_expression" => expr(
            node.child_by_field_name("value").or_else(|| node.named_child(0))?,
            src,
            ctx,
            depth + 1,
        ),
        "struct_expression" => {
            let ty = typed(node.child_by_field_name("name")?, src, ctx)?;

            Some(format!(":{ty}"))
        }
        "string_literal" | "raw_string_literal" => Some(":str".to_string()),
        "integer_literal" | "float_literal" => Some(":number".to_string()),
        "boolean_literal" => Some(":bool".to_string()),
        "array_expression" => Some(":slice".to_string()),
        _ => None,
    }
}

/// What a variable or field initialised with `value` is.
fn initialiser(value: Node, src: &[u8], ctx: Option<&str>) -> Option<String> {
    if value.kind() == "struct_expression" {
        return typed(value.child_by_field_name("name")?, src, ctx);
    }

    expr(value, src, ctx, 0).map(|e| format!("={e}"))
}

struct Walk<'a> {
    src: &'a [u8],
    out: &'a mut Extract,
    in_test: bool,
}

impl Walk<'_> {
    fn node(&mut self, node: Node, current: Option<usize>, ctx: Option<&str>, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }

        let src = self.src;
        let line = node.start_position().row + 1;
        let was_test = self.in_test;
        let enclosing = current;
        let mut current = current;
        let mut owner: Option<String> = None;

        if matches!(node.kind(), "mod_item" | "function_item") && has_test_attr(node, src) {
            self.in_test = true;
        }

        if let Some(kind) = def_kind(node)
            && let Some(name) = node.child_by_field_name("name")
            && matches!(name.kind(), "identifier" | "type_identifier")
        {
            let place = Placement {
                src,
                line_off: 0,
                test: self.in_test,
            };

            let sym = push_def(self.out, node, kind, name, ctx, &place);

            // nothing inside a definition that was not kept is read
            if sym == DROPPED {
                self.in_test = was_test;

                return;
            }

            current = Some(sym);

            if matches!(kind, "fn") {
                self.set_return(sym, node, ctx);
            }

            if kind == "trait" {
                owner = Some(text(name, src).to_string());
            }
        }

        match node.kind() {
            "impl_item" => {
                // `impl<T> Foo<T>` -> Foo
                owner = node.child_by_field_name("type").map(|ty| {
                    let ty = if ty.kind() == "generic_type" {
                        ty.child_by_field_name("type").unwrap_or(ty)
                    } else {
                        ty
                    };

                    last_seg(text(ty, src))
                });

                // the owner is copied into every method of the block
                if owner.as_deref().is_some_and(|owner| !fits(owner)) {
                    self.out.dropped += 1;
                    self.in_test = was_test;

                    return;
                }

                // `impl Trait for Type`: the type implements the trait
                if let (Some(owner), Some(tr)) = (owner.as_deref(), node.child_by_field_name("trait")) {
                    let tr = if tr.kind() == "generic_type" {
                        tr.child_by_field_name("type").unwrap_or(tr)
                    } else {
                        tr
                    };

                    push_base(self.out, owner, &last_seg(text(tr, src)), line);
                }
            }
            "use_declaration" => {
                if let Some(argument) = node.child_by_field_name("argument") {
                    collect_use(argument, src, "", self.out, 0);
                }

                self.in_test = was_test;
                return;
            }
            _ => {}
        }

        self.reference(node, current, ctx, line);
        self.binding(node, enclosing, current, ctx);

        let ctx_next = owner.as_deref().or(ctx);
        let mut cursor = node.walk();

        for child in node.children(&mut cursor) {
            self.node(child, current, ctx_next, depth + 1);
        }

        self.in_test = was_test;
    }

    /// Records the return type of the function `sym`.
    fn set_return(&mut self, sym: usize, node: Node, ctx: Option<&str>) {
        let ret = node
            .child_by_field_name("return_type")
            .and_then(|ty| typed(ty, self.src, ctx));

        if let Some(def) = self.out.syms.get_mut(sym) {
            def.ret = ret;
        }
    }

    /// The written types a node declares: `self`, parameters, `let`s, constants, fields and enum variants.
    fn binding(&mut self, node: Node, enclosing: Option<usize>, current: Option<usize>, ctx: Option<&str>) {
        let src = self.src;

        match node.kind() {
            "self_parameter" => {
                if let Some(owner) = ctx {
                    push_bind(self.out, current, "self", owner, "param");
                }
            }
            "parameter" => {
                let Some(name) = node.child_by_field_name("pattern").filter(|n| n.kind() == "identifier") else {
                    return;
                };

                if let Some(ty) = node.child_by_field_name("type").and_then(|ty| typed(ty, src, ctx)) {
                    push_bind(self.out, current, text(name, src), &ty, "param");
                }
            }
            "let_declaration" => {
                let Some(name) = node.child_by_field_name("pattern").filter(|n| n.kind() == "identifier") else {
                    return;
                };
                let ty = node
                    .child_by_field_name("type")
                    .and_then(|ty| typed(ty, src, ctx))
                    .or_else(|| {
                        node.child_by_field_name("value")
                            .and_then(|value| initialiser(value, src, ctx))
                    });

                if let Some(ty) = ty {
                    push_bind(self.out, current, text(name, src), &ty, "let");
                }
            }
            "for_expression" => {
                let Some(name) = node.child_by_field_name("pattern").filter(|n| n.kind() == "identifier") else {
                    return;
                };

                if let Some(iterated) = node.child_by_field_name("value").and_then(|v| expr(v, src, ctx, 0)) {
                    push_bind(self.out, current, text(name, src), &format!("=@{iterated}"), "let");
                }
            }
            "const_item" | "static_item" => {
                let (Some(name), Some(ty)) = (
                    node.child_by_field_name("name"),
                    node.child_by_field_name("type").and_then(|ty| typed(ty, src, ctx)),
                ) else {
                    return;
                };

                push_bind(self.out, enclosing, text(name, src), &ty, "var");
            }
            "field_declaration" => {
                let (Some(owner), Some(name)) = (
                    current
                        .and_then(|sym| self.out.syms.get(sym))
                        .map(|sym| sym.name.clone()),
                    node.child_by_field_name("name"),
                ) else {
                    return;
                };

                if let Some(ty) = node
                    .child_by_field_name("type")
                    .and_then(|ty| typed(ty, src, Some(&owner)))
                {
                    push_bind(self.out, current, &format!("{owner}.{}", text(name, src)), &ty, "field");
                }
            }
            "enum_variant" => {
                let Some(name) = node.child_by_field_name("name") else {
                    return;
                };
                let owner = current
                    .and_then(|sym| self.out.syms.get(sym))
                    .map(|sym| sym.name.clone());
                let place = Placement {
                    src,
                    line_off: 0,
                    test: self.in_test,
                };

                push_def(self.out, node, "enum_member", name, owner.as_deref(), &place);
            }
            _ => {}
        }
    }

    /// Calls, macros and constants inside the token tree of a macro invocation, which the grammar leaves
    /// unparsed: `ident (..)` is a call, `a::b (..)` a qualified one, `x . m (..)` a method call.
    fn tokens(&mut self, tree: Node, from: Option<usize>, ctx: Option<&str>, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }

        let src = self.src;
        let mut cursor = tree.walk();
        let kids: Vec<Node> = tree.children(&mut cursor).collect();

        for (i, kid) in kids.iter().enumerate() {
            if kid.kind() == "token_tree" {
                self.tokens(*kid, from, ctx, depth + 1);
                continue;
            }

            if kid.kind() != "identifier" {
                continue;
            }

            let name = text(*kid, src);
            let line = kid.start_position().row + 1;
            let next = kids.get(i + 1);
            let opens_call = next.is_some_and(|n| n.kind() == "token_tree" && text(*n, src).starts_with('('));
            let before = i.checked_sub(1).and_then(|j| kids.get(j));

            if next.is_some_and(|n| n.kind() == "!") && kids.get(i + 2).is_some_and(|n| n.kind() == "token_tree") {
                push_ref(self.out, from, name, "macro", None, line);
            } else if before.is_some_and(|b| b.kind() == ".") {
                if opens_call {
                    let recv = i
                        .checked_sub(2)
                        .and_then(|j| kids.get(j))
                        .filter(|r| r.kind() == "identifier")
                        .map(|r| text(*r, src).to_string());

                    push_mcall(self.out, from, name, None, recv, line);
                }
            } else {
                let qual = path_before(&kids, i, src, ctx);

                if opens_call {
                    push_ref(self.out, from, name, "call", qual, line);
                } else if constant_like(name) {
                    push_ref(self.out, from, name, "use", qual, line);
                }
            }
        }
    }

    /// The reference a node itself makes, if any.
    fn reference(&mut self, node: Node, from: Option<usize>, ctx: Option<&str>, line: usize) {
        let src = self.src;

        match node.kind() {
            "call_expression" => {
                let Some(function) = node.child_by_field_name("function") else {
                    return;
                };

                if let Some((name, qual, method)) = callee(function, src, ctx, 0) {
                    if method {
                        let recv = function
                            .child_by_field_name("value")
                            .and_then(|value| expr(value, src, ctx, 0));

                        push_mcall(self.out, from, &name, qual, recv, line);
                    } else {
                        push_ref(self.out, from, &name, "call", qual, line);
                    }
                }
            }
            "macro_invocation" => {
                if let Some(mac) = node.child_by_field_name("macro") {
                    let mac = if mac.kind() == "scoped_identifier" {
                        mac.child_by_field_name("name").unwrap_or(mac)
                    } else {
                        mac
                    };

                    push_ref(self.out, from, text(mac, src), "macro", None, line);
                }

                let mut cursor = node.walk();

                for tree in node.children(&mut cursor).filter(|c| c.kind() == "token_tree") {
                    self.tokens(tree, from, ctx, 0);
                }
            }
            // the cheap test first: most identifiers are not constant-looking
            "identifier" if constant_like(text(node, src)) => {
                if node
                    .parent()
                    .is_some_and(|p| !matches!(p.kind(), "token_tree" | "scoped_identifier"))
                    && !is_own_name(node)
                    && !is_callee(node)
                {
                    push_use(self.out, from, text(node, src), None, node, src);
                }
            }
            "scoped_identifier"
                if node.parent().is_some_and(|p| p.kind() != "scoped_identifier") && !is_callee(node) =>
            {
                let Some(name) = node.child_by_field_name("name").map(|n| text(n, src)) else {
                    return;
                };

                if constant_like(name) {
                    let path = node.child_by_field_name("path").map(|p| text(p, src));
                    let qual = match (path, ctx) {
                        (Some("Self"), Some(owner)) => Some(owner.to_string()),
                        (Some(path), _) => Some(path.to_string()),
                        _ => None,
                    };

                    push_use(self.out, from, name, qual, node, src);
                }
            }
            "type_identifier" if !is_own_name(node) => {
                // `tauri::State` -> qualifier `tauri`
                let qual = node
                    .parent()
                    .filter(|parent| parent.kind() == "scoped_type_identifier")
                    .and_then(|parent| parent.child_by_field_name("path"))
                    .map(|path| text(path, src).to_string());

                push_ref(self.out, from, text(node, src), "type", qual, line);
            }
            "string_literal" => {
                let value = text(node, src).trim_matches(['\'', '"']);

                if ident_like(value) {
                    push_ref(self.out, from, value, "str", None, line);
                } else {
                    push_word(self.out, from, value, node, src);
                }
            }
            _ => {}
        }
    }
}

/// The most segments of the path written before a name in a macro's tokens.
const MAX_PATH_SEGMENTS: usize = 16;

/// The `a::b` path written before token `at` of a token tree (`Self` becomes the implemented type).
fn path_before(kids: &[Node], at: usize, src: &[u8], ctx: Option<&str>) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut j = at;

    // the nearest segments only: a path of a hundred thousand `a::` would be read again for every name after it
    while j >= 2
        && parts.len() < MAX_PATH_SEGMENTS
        && kids.get(j - 1).is_some_and(|k| k.kind() == "::")
        && kids
            .get(j - 2)
            .is_some_and(|k| matches!(k.kind(), "identifier" | "crate" | "self" | "super"))
    {
        parts.push(kids.get(j - 2).map_or("", |k| text(*k, src)));
        j -= 2;
    }

    if parts.is_empty() {
        return None;
    }

    parts.reverse();

    let path = parts.join("::");

    match (path.as_str(), ctx) {
        ("Self", Some(owner)) => Some(owner.to_string()),
        _ => Some(path),
    }
}

/// True when `node` is the function of a call (the call reference covers it).
fn is_callee(node: Node) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "call_expression"
            && parent
                .child_by_field_name("function")
                .is_some_and(|function| function.id() == node.id())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> Extract {
        let mut out = Extract::default();
        extract(&mut Parser::new(), "src/lib.rs", src, &mut out);
        out
    }

    #[test]
    fn test_attributes_are_recognised() {
        assert!(test_attribute("#[test]"));
        assert!(test_attribute("#[tokio::test(flavor = \"multi_thread\")]"));
        assert!(test_attribute("#[cfg(test)]"));
        assert!(test_attribute("#[cfg(all(test, unix))]"));
        assert!(!test_attribute("#[cfg(not(test))]"));
        assert!(!test_attribute("#[derive(Debug)]"));
        assert!(!test_attribute("#[cfg(feature = \"test\")]"));
    }

    #[test]
    fn definitions_calls_and_test_modules() {
        let out = run(
            "pub fn run() { helper(); git::diff(); x.go(); Env::new(); println!(\"x\"); }\n\
             fn helper() {}\n\
             struct Env;\n\
             impl Env { fn new() -> Self { Env } fn go(&self) {} }\n\
             #[cfg(test)]\nmod tests { #[test] fn works() { super::helper(); } }\n",
        );

        let names: Vec<(&str, &str, Option<&str>, bool)> = out
            .syms
            .iter()
            .map(|s| (s.name.as_str(), s.kind, s.parent.as_deref(), s.test))
            .collect();

        assert!(names.contains(&("run", "fn", None, false)));
        assert!(names.contains(&("new", "fn", Some("Env"), false)));
        assert!(names.contains(&("works", "fn", None, true)));

        let calls: Vec<(&str, &str, Option<&str>)> = out
            .refs
            .iter()
            .filter(|r| matches!(r.kind, "call" | "mcall" | "macro"))
            .map(|r| (r.name.as_str(), r.kind, r.qual.as_deref()))
            .collect();

        assert!(calls.contains(&("helper", "call", None)));
        assert!(calls.contains(&("diff", "call", Some("git"))));
        assert!(calls.contains(&("go", "mcall", None)));
        assert!(calls.contains(&("new", "call", Some("Env"))));
        assert!(calls.contains(&("println", "macro", None)));
    }

    #[test]
    fn use_declarations_expand_lists_and_aliases() {
        let out = run("use crate::git::{self, diff as d, status::Status};\nuse serde_json::Value;\n");
        let mut got: Vec<(&str, &str, &str)> = out
            .imports
            .iter()
            .map(|i| (i.local.as_str(), i.orig.as_str(), i.source.as_str()))
            .collect();
        got.sort_unstable();

        assert_eq!(
            got,
            vec![
                ("Status", "Status", "crate::git::status::Status"),
                ("Value", "Value", "serde_json::Value"),
                ("d", "diff", "crate::git::diff"),
                ("git", "git", "crate::git"),
            ]
        );
    }

    #[test]
    fn an_impl_of_a_trait_records_the_trait_for_the_type() {
        let out = run(
            "trait Sink { fn write(&self); }\nstruct F;\nimpl Sink for F { fn write(&self) {} }\nimpl<T> a::Wrap<T> for Vec<T> {}\nimpl F { fn own(&self) {} }\n",
        );
        let bases: Vec<(&str, &str)> = out
            .refs
            .iter()
            .filter(|r| r.kind == "base")
            .map(|r| (r.name.as_str(), r.qual.as_deref().unwrap_or("")))
            .collect();

        assert_eq!(bases, [("Sink", "F"), ("Wrap", "Vec")]);
    }
}
