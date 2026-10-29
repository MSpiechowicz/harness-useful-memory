//! TypeScript, JavaScript, JSX and TSX: definitions, calls, `new`, JSX components, imports and string names.
//!
//! Files are read with the TSX grammar (so JSX works in `.js` too) except `.ts`, which uses the plain
//! TypeScript grammar. [`extract_block`] parses a script block of a single-file component.

use tree_sitter::{Language, Node, Parser};

use super::{
    Extract, Import, MAX_DEPTH, Placement, constant_like, ident_like, is_own_name, push_bind, push_def, push_mcall,
    push_ref, push_ref_recv, push_word, text, written_type,
};

/// Reads a TS/JS file.
pub(super) fn extract(parser: &mut Parser, path: &str, src: &str, out: &mut Extract) {
    let plain = path.to_ascii_lowercase().ends_with(".ts");
    let language: Language = if plain {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    } else {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    };

    extract_block(parser, &language, src, 0, None, !plain, out);
    keep_function_values(out);
}

/// Drops the function-value references that cannot resolve: a name passed as an argument only means a
/// function when this file defines it or imports it. The same holds for the value of a function kept in a
/// property (`prop`): `fn`, or `ns.fn` where `ns` or `fn` is known.
pub(super) fn keep_function_values(out: &mut Extract) {
    let known: std::collections::HashSet<&str> = out
        .syms
        .iter()
        .filter(|sym| matches!(sym.kind, "fn" | "class" | "component"))
        .map(|sym| sym.name.as_str())
        .chain(out.imports.iter().map(|import| import.local.as_str()))
        .collect();
    let keep: Vec<bool> = out
        .refs
        .iter()
        .map(|r| match r.kind {
            "fnref" => known.contains(r.name.as_str()),
            "prop" => r.qual.as_deref().is_some_and(|value| {
                let first = value.split('.').next().unwrap_or(value);
                let last = value.rsplit('.').next().unwrap_or(value);

                known.contains(last) || known.contains(first)
            }),
            _ => true,
        })
        .collect();
    let mut flags = keep.into_iter();

    out.refs.retain(|_| flags.next().unwrap_or(true));
}

/// Parses `src` as TypeScript (a `.ts` file, or a script block that starts `line_off` lines into its
/// file); references made outside any definition belong to `owner`. `jsx` turns on component references.
pub(super) fn extract_block(
    parser: &mut Parser,
    language: &Language,
    src: &str,
    line_off: usize,
    owner: Option<usize>,
    jsx: bool,
    out: &mut Extract,
) {
    let Some(tree) = super::parse(parser, language, src) else {
        return;
    };

    Walk {
        src: src.as_bytes(),
        line_off,
        jsx,
        sfc: owner.is_some(),
        class_sym: None,
        out,
    }
    .node(tree.root_node(), owner, None, 0);
}

/// The TypeScript grammar, for script blocks.
pub(super) fn typescript() -> Language {
    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
}

fn def_kind(node: Node) -> Option<&'static str> {
    match node.kind() {
        "function_declaration" | "generator_function_declaration" => Some("fn"),
        "class_declaration" | "abstract_class_declaration" => Some("class"),
        "method_definition" | "method_signature" | "abstract_method_signature" => Some("method"),
        "interface_declaration" => Some("interface"),
        "type_alias_declaration" => Some("type"),
        "enum_declaration" => Some("enum"),
        "public_field_definition" => {
            let value = node.child_by_field_name("value").map(|value| value.kind());

            matches!(value, Some("arrow_function" | "function_expression" | "function")).then_some("method")
        }
        "variable_declarator" => {
            let value = node.child_by_field_name("value").map(|value| value.kind());

            if matches!(value, Some("arrow_function" | "function_expression" | "function")) {
                return Some("fn");
            }

            let at_top = node
                .parent()
                .and_then(|declaration| declaration.parent())
                .is_some_and(|outer| {
                    outer.kind() == "program"
                        || (outer.kind() == "export_statement"
                            && outer.parent().is_some_and(|program| program.kind() == "program"))
                });

            at_top.then_some("const")
        }
        _ => None,
    }
}

/// `(name, qualifier, is_member_call)` of the callee of a call.
fn callee(node: Node, src: &[u8]) -> Option<(String, Option<String>, bool)> {
    match node.kind() {
        "identifier" => Some((text(node, src).to_string(), None, false)),
        "member_expression" => {
            let name = text(node.child_by_field_name("property")?, src).to_string();
            let object = node.child_by_field_name("object")?;
            // `Foo.bar()` on an identifier: keep it as a qualifier (namespace import, class static)
            let qual = (object.kind() == "identifier").then(|| text(object, src).to_string());

            Some((name, qual, true))
        }
        _ => None,
    }
}

/// Whether the module path of an import or re-export is short enough to keep: an over-long one is
/// dropped (and counted) instead of being copied into every name the statement brings in.
fn source_fits(node: Node, src: &[u8]) -> bool {
    node.child_by_field_name("source")
        .is_none_or(|source| super::fits(text(source, src).trim_matches(['\'', '"'])))
}

fn collect_import(node: Node, src: &[u8], out: &mut Vec<Import>) {
    let Some(source) = node.child_by_field_name("source") else {
        return;
    };
    let source = text(source, src).trim_matches(['\'', '"']);

    // An over-long module path would be copied into every name it brings in.
    if !super::fits(source) {
        return;
    }

    let source = source.to_string();
    let mut cursor = node.walk();

    for clause in node.named_children(&mut cursor).filter(|c| c.kind() == "import_clause") {
        let mut inner = clause.walk();

        for part in clause.named_children(&mut inner) {
            match part.kind() {
                "identifier" => {
                    // default import: the component/module itself
                    let local = text(part, src).to_string();

                    out.push(Import {
                        local: local.clone(),
                        orig: local,
                        source: source.clone(),
                    });
                }
                "named_imports" => {
                    let mut specs = part.walk();

                    for spec in part
                        .named_children(&mut specs)
                        .filter(|s| s.kind() == "import_specifier")
                    {
                        let Some(name) = spec.child_by_field_name("name") else {
                            continue;
                        };
                        let orig = text(name, src).to_string();
                        let local = spec
                            .child_by_field_name("alias")
                            .map_or_else(|| orig.clone(), |alias| text(alias, src).to_string());

                        out.push(Import {
                            local,
                            orig,
                            source: source.clone(),
                        });
                    }
                }
                "namespace_import" => {
                    let mut names = part.walk();
                    let id = part.named_children(&mut names).find(|n| n.kind() == "identifier");

                    if let Some(id) = id {
                        out.push(Import {
                            local: text(id, src).to_string(),
                            orig: "*".into(),
                            source: source.clone(),
                        });
                    }
                }
                _ => {}
            }
        }
    }
}

/// `export * from './x'` and `export { a, b as c } from './x'`: kept as imports whose local name is
/// `export:<exported name>` (`export:*` for all), so the graph can follow a name through a re-export.
fn collect_reexport(node: Node, src: &[u8], out: &mut Vec<Import>) {
    let Some(source) = node.child_by_field_name("source") else {
        return;
    };
    let source = text(source, src).trim_matches(['\'', '"']);

    // An over-long module path would be copied into every name it brings in.
    if !super::fits(source) {
        return;
    }

    let source = source.to_string();
    let mut cursor = node.walk();

    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "export_clause" => {
                let mut specs = child.walk();

                for spec in child
                    .named_children(&mut specs)
                    .filter(|s| s.kind() == "export_specifier")
                {
                    let Some(name) = spec.child_by_field_name("name") else {
                        continue;
                    };
                    let orig = text(name, src).to_string();
                    let exported = spec
                        .child_by_field_name("alias")
                        .map_or_else(|| orig.clone(), |alias| text(alias, src).to_string());

                    out.push(Import {
                        local: format!("export:{exported}"),
                        orig,
                        source: source.clone(),
                    });
                }
            }
            "namespace_export" => {}
            _ => {}
        }
    }

    // `export * from`: no clause, a `*` token
    let mut tokens = node.walk();

    if !node.children(&mut tokens).any(|c| c.kind() == "export_clause") {
        out.push(Import {
            local: "export:*".to_string(),
            orig: "*".to_string(),
            source,
        });
    }
}

/// `const Page = lazy(() => import('./Page').then((m) => ({ default: m.Page })))`: the local name is the
/// component of that module, so it is an import, not a definition. `None` for anything else.
fn lazy_import(declarator: Node, src: &[u8]) -> Option<Import> {
    let local = text(
        declarator
            .child_by_field_name("name")
            .filter(|n| n.kind() == "identifier")?,
        src,
    );
    let call = declarator
        .child_by_field_name("value")
        .filter(|v| v.kind() == "call_expression")?;
    let function = call.child_by_field_name("function")?;
    let lazy = match function.kind() {
        "identifier" => text(function, src) == "lazy",
        "member_expression" => text(function.child_by_field_name("property")?, src) == "lazy",
        _ => false,
    };

    if !lazy {
        return None;
    }

    let loader = call
        .child_by_field_name("arguments")?
        .named_child(0)
        .filter(|a| a.kind() == "arrow_function")?
        .child_by_field_name("body")?;
    let mut body = loader;
    let mut orig = local.to_string();

    // `import(..).then((m) => ({ default: m.Name }))`
    if body.kind() == "call_expression"
        && let Some(member) = body
            .child_by_field_name("function")
            .filter(|f| f.kind() == "member_expression")
        && text(member.child_by_field_name("property")?, src) == "then"
    {
        let callback = body.child_by_field_name("arguments")?.named_child(0)?;
        let mut stack = vec![callback];

        while let Some(next) = stack.pop() {
            if next.kind() == "pair"
                && text(next.child_by_field_name("key")?, src) == "default"
                && let Some(value) = next
                    .child_by_field_name("value")
                    .filter(|v| v.kind() == "member_expression")
            {
                orig = text(value.child_by_field_name("property")?, src).to_string();
                break;
            }

            let mut cursor = next.walk();
            stack.extend(next.named_children(&mut cursor));
        }

        body = member.child_by_field_name("object")?;
    }

    let import_call = body.kind() == "call_expression"
        && body
            .child_by_field_name("function")
            .is_some_and(|f| f.kind() == "import");

    if !import_call {
        return None;
    }

    let source = body.child_by_field_name("arguments")?.named_child(0)?;

    if source.kind() != "string" {
        return None;
    }

    Some(Import {
        local: local.to_string(),
        orig,
        source: text(source, src).trim_matches(['\'', '"']).to_string(),
    })
}

struct Walk<'a> {
    src: &'a [u8],
    line_off: usize,
    jsx: bool,
    /// Inside a single-file component: a top-level `const x = f()` runs for the component, it is no definition.
    sfc: bool,
    /// The class or interface whose body is being read.
    class_sym: Option<usize>,
    out: &'a mut Extract,
}

/// Deeper receiver expressions are not followed.
const MAX_EXPR_DEPTH: usize = 12;

/// The receiver expression a node stands for (see `graph/infer.rs`), when it can be told from the syntax.
fn expr(node: Node, src: &[u8], depth: usize) -> Option<String> {
    if depth > MAX_EXPR_DEPTH {
        return None;
    }

    match node.kind() {
        "identifier" => Some(text(node, src).to_string()),
        "this" => Some("this".to_string()),
        "member_expression" => {
            let property = node.child_by_field_name("property")?;

            if property.kind() != "property_identifier" {
                return None;
            }

            let object = expr(node.child_by_field_name("object")?, src, depth + 1)?;

            Some(format!("{object}.{}", text(property, src)))
        }
        "call_expression" => {
            let function = node.child_by_field_name("function")?;

            match function.kind() {
                "identifier" => Some(format!("{}()", text(function, src))),
                "member_expression" => Some(format!("{}()", expr(function, src, depth + 1)?)),
                _ => None,
            }
        }
        "await_expression" | "parenthesized_expression" | "non_null_expression" => {
            expr(node.named_child(0)?, src, depth + 1)
        }
        "new_expression" => {
            let ty = written_type(text(node.child_by_field_name("constructor")?, src))?;

            Some(format!(":{ty}"))
        }
        "as_expression" | "satisfies_expression" => {
            let ty = written_type(text(node.named_child(1)?, src))?;

            Some(format!(":{ty}"))
        }
        "string" | "template_string" => Some(":string".to_string()),
        "number" => Some(":number".to_string()),
        "true" | "false" => Some(":boolean".to_string()),
        "array" => Some(":Array".to_string()),
        "regex" => Some(":RegExp".to_string()),
        _ => None,
    }
}

/// What a variable or field initialised with `value` is: the class of a `new`, or the expression to follow.
fn initialiser(value: Node, src: &[u8]) -> Option<String> {
    let inner = match value.kind() {
        "await_expression" | "parenthesized_expression" => value.named_child(0)?,
        _ => value,
    };

    if inner.kind() == "new_expression" {
        return written_type(text(inner.child_by_field_name("constructor")?, src));
    }

    expr(inner, src, 0).map(|e| format!("={e}"))
}

/// The written type of the annotation field `type` of a declaration.
fn annotated(node: Node, src: &[u8]) -> Option<String> {
    written_type(text(node.child_by_field_name("type")?, src))
}

/// The written return type of a function-like node.
fn returns(node: Node, src: &[u8]) -> Option<String> {
    written_type(text(node.child_by_field_name("return_type")?, src))
}

/// True when an identifier stands for a value there: not a declared name, a callee, a parameter, a tag or
/// the object of a member access.
fn value_position(node: Node, parent: Node) -> bool {
    if is_own_name(node) {
        return false;
    }

    match parent.kind() {
        "call_expression" => !parent
            .child_by_field_name("function")
            .is_some_and(|function| function.id() == node.id()),
        "new_expression"
        | "member_expression"
        | "required_parameter"
        | "optional_parameter"
        | "import_specifier"
        | "export_specifier"
        | "namespace_import"
        | "import_clause"
        | "jsx_opening_element"
        | "jsx_closing_element"
        | "jsx_self_closing_element"
        | "labeled_statement"
        | "rest_pattern"
        | "array_pattern"
        | "object_pattern"
        | "shorthand_property_identifier_pattern"
        | "type_query" => false,
        _ => true,
    }
}

/// The type an object literal is declared to have: `const x: T = {..}`, `{..} as T`, or the return type of
/// the function it is returned from.
fn object_type(object: Node, src: &[u8]) -> Option<String> {
    let mut at = object.parent()?;

    while at.kind() == "parenthesized_expression" {
        at = at.parent()?;
    }

    match at.kind() {
        "as_expression" | "satisfies_expression" => written_type(text(at.named_child(1)?, src)),
        "variable_declarator" => annotated(at, src),
        "return_statement" => {
            let mut up = at.parent();

            while let Some(candidate) = up {
                if matches!(
                    candidate.kind(),
                    "function_declaration"
                        | "generator_function_declaration"
                        | "method_definition"
                        | "arrow_function"
                        | "function_expression"
                        | "function"
                ) {
                    return returns(candidate, src);
                }

                up = candidate.parent();
            }

            None
        }
        "arrow_function" => returns(at, src),
        _ => None,
    }
}

/// Property values kept per object literal property (`key: a ?? b ? c : d` is three).
const MAX_PROP_VALUES: usize = 4;

/// The functions a property value may hold, as written: `fn`, `ns.fn`, `a ?? fn`, `cond ? a : b`,
/// `fn.bind(x)`, any of them in parentheses or after a cast. A value that starts at `this` is a method, which
/// a function value never is.
fn function_values(value: Node, src: &[u8], depth: usize, found: &mut Vec<String>) {
    if depth > MAX_EXPR_DEPTH || found.len() >= MAX_PROP_VALUES {
        return;
    }

    match value.kind() {
        "identifier" => found.push(text(value, src).to_string()),
        "member_expression" => {
            if let Some(chain) =
                expr(value, src, 0).filter(|chain| !chain.starts_with("this") && !chain.contains(['(', ':']))
            {
                found.push(chain);
            }
        }
        "parenthesized_expression" | "as_expression" | "satisfies_expression" | "non_null_expression" => {
            if let Some(inner) = value.named_child(0) {
                function_values(inner, src, depth + 1, found);
            }
        }
        "binary_expression" => {
            let operator = value
                .child_by_field_name("operator")
                .map(|operator| text(operator, src));

            if matches!(operator, Some("??" | "||"))
                && let (Some(left), Some(right)) =
                    (value.child_by_field_name("left"), value.child_by_field_name("right"))
            {
                function_values(left, src, depth + 1, found);
                function_values(right, src, depth + 1, found);
            }
        }
        "ternary_expression" => {
            for field in ["consequence", "alternative"] {
                if let Some(branch) = value.child_by_field_name(field) {
                    function_values(branch, src, depth + 1, found);
                }
            }
        }
        "call_expression" => {
            // `fn.bind(this)`
            if let Some(member) = value
                .child_by_field_name("function")
                .filter(|function| function.kind() == "member_expression")
                && member
                    .child_by_field_name("property")
                    .is_some_and(|property| text(property, src) == "bind")
                && let Some(object) = member.child_by_field_name("object")
            {
                function_values(object, src, depth + 1, found);
            }
        }
        _ => {}
    }
}

/// The function an object literal is passed to as an argument (`create({ .. })`, `new Runner({ .. })`,
/// `Runner.create({ .. })`): its name as written, or its last name for a member.
fn passed_to(object: Node, src: &[u8]) -> Option<String> {
    let mut at = object.parent()?;

    while matches!(
        at.kind(),
        "parenthesized_expression" | "as_expression" | "satisfies_expression" | "non_null_expression"
    ) {
        at = at.parent()?;
    }

    if at.kind() != "arguments" {
        return None;
    }

    let call = at.parent()?;
    let callee = match call.kind() {
        "call_expression" => call.child_by_field_name("function")?,
        "new_expression" => call.child_by_field_name("constructor")?,
        _ => return None,
    };

    match callee.kind() {
        "identifier" => Some(text(callee, src).to_string()),
        "member_expression" => Some(text(callee.child_by_field_name("property")?, src).to_string()),
        _ => None,
    }
}

impl Walk<'_> {
    fn node(&mut self, node: Node, current: Option<usize>, ctx: Option<&str>, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }

        let src = self.src;
        let line = node.start_position().row + self.line_off + 1;
        if node.kind() == "variable_declarator"
            && let Some(import) = lazy_import(node, src)
        {
            self.out.imports.push(import);
            return;
        }

        let enclosing = current;
        let saved_class = self.class_sym;
        let mut current = current;
        let mut owner: Option<String> = None;

        if let Some(kind) = def_kind(node)
            && let Some(name) = node.child_by_field_name("name")
            && matches!(name.kind(), "identifier" | "property_identifier" | "type_identifier")
            && !(kind == "const" && self.sfc)
        {
            let place = Placement {
                src,
                line_off: self.line_off,
                test: false,
            };
            // a method of an object literal belongs to no class around it
            let parent = if node.parent().is_some_and(|p| p.kind() == "object") {
                None
            } else {
                ctx.map(str::to_string)
            };
            let sym = push_def(self.out, node, kind, name, parent.as_deref(), &place);

            current = Some(sym);
            self.set_return(sym, node);

            if matches!(kind, "class" | "interface" | "enum") {
                owner = Some(text(name, src).to_string());
            }

            if matches!(kind, "class" | "interface") {
                self.class_sym = Some(sym);
            }

            if kind == "class" {
                push_bind(self.out, Some(sym), "this", text(name, src), "param");
            }
        }

        if node.kind() == "import_statement" {
            if source_fits(node, src) {
                collect_import(node, src, &mut self.out.imports);
            } else {
                self.out.dropped += 1;
            }
            return;
        }

        if node.kind() == "export_statement" && node.child_by_field_name("source").is_some() {
            if source_fits(node, src) {
                collect_reexport(node, src, &mut self.out.imports);
            } else {
                self.out.dropped += 1;
            }
            return;
        }

        self.reference(node, current, line);
        self.binding(node, enclosing, current, ctx);

        let ctx_next = owner.as_deref().or(ctx);
        let mut cursor = node.walk();

        for child in node.children(&mut cursor) {
            self.node(child, current, ctx_next, depth + 1);
        }

        self.class_sym = saved_class;
    }

    /// Records the return type of the definition `sym` made from `node`.
    fn set_return(&mut self, sym: usize, node: Node) {
        let source = match node.kind() {
            "variable_declarator" => node.child_by_field_name("value"),
            _ => Some(node),
        };
        let ret = source.and_then(|function| returns(function, self.src));

        if let Some(def) = self.out.syms.get_mut(sym) {
            def.ret = ret;
        }
    }

    /// The written types a node declares: variables, parameters, fields, members and enum members.
    fn binding(&mut self, node: Node, enclosing: Option<usize>, current: Option<usize>, ctx: Option<&str>) {
        let src = self.src;

        match node.kind() {
            "variable_declarator" => {
                let Some(name) = node.child_by_field_name("name").filter(|n| n.kind() == "identifier") else {
                    return;
                };
                let ty = annotated(node, src).or_else(|| {
                    node.child_by_field_name("value")
                        .and_then(|value| initialiser(value, src))
                });

                if let Some(ty) = ty {
                    push_bind(self.out, enclosing, text(name, src), &ty, "var");
                }
            }
            "for_in_statement" => {
                let Some(name) = node.child_by_field_name("left").filter(|n| n.kind() == "identifier") else {
                    return;
                };

                if let Some(iterated) = node.child_by_field_name("right").and_then(|r| expr(r, src, 0)) {
                    push_bind(self.out, current, text(name, src), &format!("=@{iterated}"), "var");
                }
            }
            "required_parameter" | "optional_parameter" => {
                let Some(name) = node.child_by_field_name("pattern").filter(|n| n.kind() == "identifier") else {
                    return;
                };
                let Some(ty) = annotated(node, src) else {
                    return;
                };

                push_bind(self.out, current, text(name, src), &ty, "param");

                // `constructor(private api: Api)` also declares the field
                let mut cursor = node.walk();
                let declares_field = node
                    .children(&mut cursor)
                    .any(|child| matches!(child.kind(), "accessibility_modifier" | "readonly"));

                if declares_field && let (Some(class), Some(owner)) = (self.class_sym, ctx) {
                    push_bind(
                        self.out,
                        Some(class),
                        &format!("{owner}.{}", text(name, src)),
                        &ty,
                        "field",
                    );
                }
            }
            "public_field_definition" => {
                let (Some(class), Some(owner), Some(name)) = (self.class_sym, ctx, node.child_by_field_name("name"))
                else {
                    return;
                };
                let ty = annotated(node, src).or_else(|| {
                    node.child_by_field_name("value")
                        .and_then(|value| initialiser(value, src))
                });

                if let Some(ty) = ty {
                    push_bind(
                        self.out,
                        Some(class),
                        &format!("{owner}.{}", text(name, src)),
                        &ty,
                        "field",
                    );
                }
            }
            "property_signature" => {
                let (Some(class), Some(owner), Some(name)) = (self.class_sym, ctx, node.child_by_field_name("name"))
                else {
                    return;
                };
                let annotation = node.child_by_field_name("type").and_then(|t| t.named_child(0));

                if let Some(function) = annotation.filter(|t| t.kind() == "function_type") {
                    self.member(node, name, owner, function);
                } else if let Some(ty) = annotated(node, src) {
                    push_bind(
                        self.out,
                        Some(class),
                        &format!("{owner}.{}", text(name, src)),
                        &ty,
                        "field",
                    );
                }
            }
            "pair" => {
                let (Some(key), Some(value)) = (node.child_by_field_name("key"), node.child_by_field_name("value"))
                else {
                    return;
                };

                if key.kind() != "property_identifier" {
                    return;
                }

                if matches!(value.kind(), "arrow_function" | "function_expression" | "function") {
                    if let Some(object) = node.parent() {
                        let owner = object_type(object, src);

                        self.member(node, key, owner.as_deref().unwrap_or(""), value);
                    }
                } else {
                    let mut values = Vec::new();

                    function_values(value, src, 0, &mut values);
                    self.properties(node, text(key, src), &values, current);
                }
            }
            "shorthand_property_identifier" => {
                let name = text(node, src);

                self.properties(node, name, &[name.to_string()], current);
            }
            "enum_assignment" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.enum_member(node, name, ctx);
                }
            }
            "property_identifier" if node.parent().is_some_and(|p| p.kind() == "enum_body") => {
                self.enum_member(node, node, ctx);
            }
            _ => {}
        }
    }

    /// Notes that property `key` of the object literal around `node` may hold each of `values` (a `prop`
    /// reference: the value as written in `qual`, the function the object is passed to in `recv`). Whether a
    /// value is a function is told when the graph is loaded.
    fn properties(&mut self, node: Node, key: &str, values: &[String], from: Option<usize>) {
        let via = node.parent().and_then(|object| passed_to(object, self.src));
        let line = node.start_position().row + self.line_off + 1;

        for value in values {
            push_ref_recv(self.out, from, key, "prop", Some(value.clone()), via.clone(), line);
        }
    }

    /// A member of an interface or an object literal that holds a function: a method named `name` of `owner`.
    /// It never becomes the scope of the references inside it, and a string never names it.
    fn member(&mut self, node: Node, name: Node, owner: &str, function: Node) {
        let place = Placement {
            src: self.src,
            line_off: self.line_off,
            test: false,
        };
        let owner = (!owner.is_empty()).then_some(owner);
        let sym = push_def(self.out, node, "member", name, owner, &place);

        if let Some(def) = self.out.syms.get_mut(sym) {
            def.ret = returns(function, self.src);
        }
    }

    fn enum_member(&mut self, node: Node, name: Node, owner: Option<&str>) {
        let place = Placement {
            src: self.src,
            line_off: self.line_off,
            test: false,
        };

        push_def(self.out, node, "enum_member", name, owner, &place);
    }

    /// The reference a node itself makes, if any.
    fn reference(&mut self, node: Node, from: Option<usize>, line: usize) {
        let src = self.src;

        match node.kind() {
            "type_identifier" if !is_own_name(node) => {
                let qual = node
                    .parent()
                    .filter(|parent| parent.kind() == "nested_type_identifier")
                    .and_then(|parent| parent.child_by_field_name("module"))
                    .map(|module| text(module, src).to_string());

                push_ref(self.out, from, text(node, src), "type", qual, line);
            }
            "call_expression" => {
                let Some(function) = node.child_by_field_name("function") else {
                    return;
                };

                if let Some((name, qual, member)) = callee(function, src) {
                    if member {
                        let recv = function
                            .child_by_field_name("object")
                            .and_then(|object| expr(object, src, 0));

                        push_mcall(self.out, from, &name, qual, recv, line);
                    } else {
                        push_ref(self.out, from, &name, "call", qual, line);
                    }
                }
            }
            // React/JSX: <Foo /> and <Foo.Bar> are component uses (lowercase tags are DOM elements)
            "jsx_opening_element" | "jsx_self_closing_element" if self.jsx => {
                if let Some(name) = node.child_by_field_name("name") {
                    match name.kind() {
                        "identifier" if text(name, src).starts_with(|c: char| c.is_uppercase()) => {
                            push_ref(self.out, from, text(name, src), "component", None, line);
                        }
                        "member_expression" => {
                            if let Some((member, qual, _)) = callee(name, src) {
                                push_ref(self.out, from, &member, "component", qual, line);
                            }
                        }
                        _ => {}
                    }
                }
            }
            // names passed as strings: IPC commands, event names, routes, registries
            // a function passed as a value (`app.get(path, handler)`, `rows.map(format)`), or a constant
            "identifier" => {
                let Some(parent) = node.parent() else {
                    return;
                };
                let name = text(node, src);

                // also an element of an array: a table of steps or handlers (`[["Facts", factsStep]]`)
                if matches!(parent.kind(), "arguments" | "jsx_expression" | "array") {
                    push_ref(self.out, from, name, "fnref", None, line);
                }

                if constant_like(name) && value_position(node, parent) {
                    push_ref(self.out, from, name, "use", None, line);
                }
            }
            "member_expression" => {
                // `Color.Red`, `limits.MAX_ROWS`: a constant named through its owner
                let (Some(object), Some(property)) =
                    (node.child_by_field_name("object"), node.child_by_field_name("property"))
                else {
                    return;
                };
                let name = text(property, src);

                if object.kind() == "identifier"
                    && property.kind() == "property_identifier"
                    && constant_like(name)
                    && !node.parent().is_some_and(|p| {
                        p.kind() == "call_expression"
                            && p.child_by_field_name("function").is_some_and(|f| f.id() == node.id())
                    })
                {
                    push_ref(self.out, from, name, "use", Some(text(object, src).to_string()), line);
                }
            }
            "string" => {
                let value = text(node, src).trim_matches(['\'', '"']);

                if ident_like(value) {
                    push_ref(self.out, from, value, "str", None, line);
                } else if !node.parent().is_some_and(|parent| parent.kind().starts_with("jsx_")) {
                    // a class name or other markup attribute is no command
                    push_word(self.out, from, value, node, src);
                }
            }
            "new_expression" => {
                if let Some((name, qual, _)) = node.child_by_field_name("constructor").and_then(|c| callee(c, src)) {
                    push_ref(self.out, from, &name, "new", qual, line);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(path: &str, src: &str) -> Extract {
        let mut out = Extract::default();
        extract(&mut Parser::new(), path, src, &mut out);
        out
    }

    #[test]
    fn components_calls_imports_and_strings() {
        let out = run(
            "src/App.tsx",
            "import Foo, { bar as baz } from './x';\nimport * as ns from '$lib/y';\n\
             export function App() { baz(); ns.run(); return <Foo.Bar a={invoke('git_file_lines')} /> }\n",
        );

        let imports: Vec<(&str, &str, &str)> = out
            .imports
            .iter()
            .map(|i| (i.local.as_str(), i.orig.as_str(), i.source.as_str()))
            .collect();
        assert_eq!(
            imports,
            vec![("Foo", "Foo", "./x"), ("baz", "bar", "./x"), ("ns", "*", "$lib/y")]
        );

        let refs: Vec<(&str, &str, Option<&str>)> = out
            .refs
            .iter()
            .map(|r| (r.name.as_str(), r.kind, r.qual.as_deref()))
            .collect();
        assert!(refs.contains(&("baz", "call", None)));
        assert!(refs.contains(&("run", "mcall", Some("ns"))));
        assert!(refs.contains(&("Bar", "component", Some("Foo"))));
        assert!(refs.contains(&("git_file_lines", "str", None)));
        assert!(out.syms.iter().any(|s| s.name == "App" && s.kind == "fn"));
    }

    #[test]
    fn plain_typescript_has_no_jsx_components() {
        let out = run("a.ts", "const x = a < B > (c);\nclass K { m() {} }\nnew K();\n");

        assert!(out.refs.iter().all(|r| r.kind != "component"));
        assert!(
            out.syms
                .iter()
                .any(|s| s.name == "m" && s.parent.as_deref() == Some("K"))
        );
        assert!(out.refs.iter().any(|r| r.kind == "new" && r.name == "K"));
    }

    #[test]
    fn functions_put_in_object_properties_are_props_with_the_function_they_are_passed_to() {
        let out = run(
            "a.js",
            "import { runIt, other } from './x';\nimport * as ns from './y';\nfunction local() {}\n\
             export const runner = create({ run: runIt, fallback: svc.cb ?? local, bound: other.bind(null), \
             shared: ns.member, name: 'x', count: 3, this_: this.method });\n\
             const plain = { local };\n",
        );
        let props: Vec<(&str, &str, Option<&str>)> = out
            .refs
            .iter()
            .filter(|r| r.kind == "prop")
            .map(|r| (r.name.as_str(), r.qual.as_deref().unwrap_or(""), r.recv.as_deref()))
            .collect();

        assert_eq!(
            props,
            vec![
                ("run", "runIt", Some("create")),
                ("fallback", "local", Some("create")),
                ("bound", "other", Some("create")),
                ("shared", "ns.member", Some("create")),
                ("local", "local", None),
            ]
        );
    }
}
