//! Python: classes, functions, methods, module constants, calls, base classes, annotated types, imports
//! and string names.
//!
//! A function whose nearest enclosing definition is a class (also through a decorator) is a `method` of
//! that class; a function nested in a function stays a `fn`.
//!
//! Imports keep the module as the dotted path written in the source. A relative import keeps its leading
//! dots (`..m`) and is resolved against the importing file's folder when the graph is built
//! (`graph/python.rs`), so the stored rows do not depend on where the file lives.
//!
//! | source                   | local | orig | source |
//! |--------------------------|-------|------|--------|
//! | `import a.b.c`           | `a`   | `*`  | `a`    |
//! | `import a.b.c as x`      | `x`   | `*`  | `a.b.c`|
//! | `from a.b import c`      | `c`   | `c`  | `a.b`  |
//! | `from a.b import c as d` | `d`   | `c`  | `a.b`  |
//! | `from . import x`        | `x`   | `x`  | `.`    |
//! | `from ..m import y`      | `y`   | `y`  | `..m`  |
//!
//! `from m import *` brings no names.

use std::collections::HashSet;

use tree_sitter::{Node, Parser};

use super::{
    DROPPED, Extract, Import, MAX_DEPTH, Placement, constant_like, fits, ident_like, push_base, push_bind, push_def,
    push_mcall, push_ref, push_ref_recv, push_use, push_word, text, written_type,
};

/// Reads a Python file.
pub(super) fn extract(parser: &mut Parser, _path: &str, src: &str, out: &mut Extract) {
    let Some(tree) = super::parse(parser, &tree_sitter_python::LANGUAGE.into(), src) else {
        return;
    };

    Walk {
        src: src.as_bytes(),
        out,
    }
    .node(tree.root_node(), Scope::default(), 0);

    method_calls(out);
    super::ts::keep_function_values(out);
}

/// Where the walk is: the enclosing definition and the classes around it.
#[derive(Clone, Copy, Default)]
struct Scope<'a> {
    /// Index of the enclosing definition in [`Extract::syms`].
    current: Option<usize>,
    /// The class whose body the node sits directly in: a function here is a method.
    owner: Option<&'a str>,
    /// The nearest enclosing class, also from inside its methods (what `self` is).
    class: Option<&'a str>,
    /// The class whose body the node sits directly in is an enumeration: its names are members.
    members: bool,
}

struct Walk<'a> {
    src: &'a [u8],
    out: &'a mut Extract,
}

impl<'a> Walk<'a> {
    fn node(&mut self, node: Node, scope: Scope<'a>, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }

        let mut inner = scope;

        match node.kind() {
            "import_statement" => {
                self.import(node);

                return;
            }
            "import_from_statement" => {
                self.import_from(node);

                return;
            }
            "class_definition" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let class = text(name, self.src);
                    let place = self.place();
                    let sym = push_def(self.out, node, "class", name, None, &place);

                    // a class that is not kept owns nothing: its name is not copied into every member
                    if sym == DROPPED {
                        return;
                    }

                    self.bases(node, sym);

                    push_bind(self.out, Some(sym), "self", class, "param");
                    push_bind(self.out, Some(sym), "cls", class, "param");

                    inner = Scope {
                        current: Some(sym),
                        owner: Some(class),
                        class: Some(class),
                        members: is_enumeration(node, self.src),
                    };
                }
            }
            "function_definition" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let kind = if scope.owner.is_some() { "method" } else { "fn" };
                    let place = self.place();
                    let sym = push_def(self.out, node, kind, name, scope.owner, &place);

                    if sym == DROPPED {
                        return;
                    }

                    inner.current = Some(sym);
                    inner.owner = None;
                    inner.members = false;

                    if let Some(def) = self.out.syms.get_mut(sym) {
                        def.ret = node
                            .child_by_field_name("return_type")
                            .and_then(|ty| written_type(text(ty, self.src)));
                    }
                }
            }
            "typed_parameter" | "typed_default_parameter" => self.parameter(node, scope),
            "for_statement" => self.for_loop(node, scope),
            "with_item" => self.with_item(node, scope),
            "identifier" => self.mention(node, scope),
            "assignment" => {
                self.assignment(node, scope);

                if scope.members
                    && let Some(name) = member_name(node, self.src)
                {
                    let place = self.place();

                    push_def(self.out, node, "enum_member", name, scope.owner, &place);
                } else if let Some(name) = module_constant(node, self.src) {
                    let place = self.place();

                    inner.current = Some(push_def(self.out, node, "const", name, None, &place));
                }
            }
            "call" => self.call(node, scope),
            "decorator" => self.decorator(node, scope),
            "string" => self.string(node, scope),
            "type" => self.annotation(node, scope),
            _ => {}
        }

        let mut cursor = node.walk();

        for child in node.children(&mut cursor) {
            self.node(child, inner, depth + 1);
        }
    }

    /// `request: Request`, `auth: Auth | None = None`.
    fn parameter(&mut self, node: Node, scope: Scope) {
        let name = if node.kind() == "typed_default_parameter" {
            node.child_by_field_name("name")
        } else {
            node.named_child(0)
        };
        let Some(name) = name.filter(|n| n.kind() == "identifier") else {
            return;
        };
        let ty = node
            .child_by_field_name("type")
            .and_then(|ty| written_type(text(ty, self.src)));

        if let Some(ty) = ty {
            push_bind(self.out, scope.current, text(name, self.src), &ty, "param");
        }
    }

    /// `for item in items`.
    fn for_loop(&mut self, node: Node, scope: Scope) {
        let Some(name) = node.child_by_field_name("left").filter(|n| n.kind() == "identifier") else {
            return;
        };

        if let Some(iterated) = node
            .child_by_field_name("right")
            .and_then(|right| expr(right, self.src, 0))
        {
            push_bind(
                self.out,
                scope.current,
                text(name, self.src),
                &format!("=@{iterated}"),
                "var",
            );
        }
    }

    /// `with Client() as client`: the name is what the expression is (a class called is its instance).
    fn with_item(&mut self, node: Node, scope: Scope) {
        let Some(pattern) = node.child_by_field_name("value").filter(|v| v.kind() == "as_pattern") else {
            return;
        };
        let alias = pattern
            .child_by_field_name("alias")
            .and_then(|alias| match alias.kind() {
                "identifier" => Some(alias),
                _ => alias.named_child(0).filter(|inner| inner.kind() == "identifier"),
            });

        if let (Some(value), Some(alias)) = (pattern.named_child(0), alias)
            && let Some(ty) = initialiser(value, self.src)
        {
            push_bind(self.out, scope.current, text(alias, self.src), &ty, "var");
        }
    }

    /// `x = T()`, `x: T = ...`, `self.x = param`, and class-level `x: T`.
    fn assignment(&mut self, node: Node, scope: Scope) {
        let (Some(left), right) = (node.child_by_field_name("left"), node.child_by_field_name("right")) else {
            return;
        };
        let ty = node
            .child_by_field_name("type")
            .and_then(|ty| written_type(text(ty, self.src)))
            .or_else(|| right.and_then(|right| initialiser(right, self.src)));
        let Some(ty) = ty else {
            return;
        };

        match left.kind() {
            "identifier" if scope.owner.is_some() => {
                if let Some(class) = scope.class {
                    let field = format!("{class}.{}", text(left, self.src));

                    push_bind(self.out, scope.current, &field, &ty, "field");
                }
            }
            "identifier" => push_bind(self.out, scope.current, text(left, self.src), &ty, "var"),
            "attribute" => {
                let object = left.child_by_field_name("object");
                let attribute = left.child_by_field_name("attribute");

                if let (Some(object), Some(attribute), Some(class)) = (object, attribute, scope.class)
                    && object.kind() == "identifier"
                    && matches!(text(object, self.src), "self" | "cls")
                {
                    let field = format!("{class}.{}", text(attribute, self.src));

                    push_bind(self.out, scope.current, &field, &ty, "field");
                }
            }
            _ => {}
        }
    }

    /// A constant-looking name used as a value: `x == MODE_FAST`, `Color.RED`.
    fn mention(&mut self, node: Node, scope: Scope) {
        let Some(parent) = node.parent() else {
            return;
        };
        let name = text(node, self.src);
        let line = node.start_position().row + 1;
        let passed = parent.kind() == "argument_list" && parent.parent().is_some_and(|call| call.kind() == "call")
            || parent.kind() == "keyword_argument"
                && parent
                    .child_by_field_name("value")
                    .is_some_and(|value| value.id() == node.id());

        // a function passed as a value: `map(clean, rows)`, `Thread(target=work)`
        if passed {
            push_ref(self.out, scope.current, name, "fnref", None, line);
        }

        match parent.kind() {
            "attribute" => {
                // `Color.RED`: the member, qualified by the class
                let is_member = parent
                    .child_by_field_name("attribute")
                    .is_some_and(|attribute| attribute.id() == node.id());
                let grand_calls = parent.parent().is_some_and(|grand| {
                    grand.kind() == "call"
                        && grand
                            .child_by_field_name("function")
                            .is_some_and(|f| f.id() == parent.id())
                });

                if is_member && !grand_calls && constant_like(name) {
                    let qual = parent
                        .child_by_field_name("object")
                        .and_then(|object| dotted(object, self.src, 0));

                    push_use(
                        self.out,
                        scope.current,
                        name,
                        qual,
                        node,
                        self.src,
                        node.start_position().row + 1,
                    );
                }
            }
            // `case Color.RED:` names a constant through its class, where an event is handled
            "dotted_name" if constant_like(name) && super::handled(node, self.src) => {
                let mut cursor = parent.walk();
                let owners: Vec<&str> = parent
                    .named_children(&mut cursor)
                    .take_while(|part| part.id() != node.id())
                    .map(|part| text(part, self.src))
                    .collect();

                if !owners.is_empty() {
                    push_use(
                        self.out,
                        scope.current,
                        name,
                        Some(owners.join(".")),
                        node,
                        self.src,
                        node.start_position().row + 1,
                    );
                }
            }
            "call"
            | "parameters"
            | "default_parameter"
            | "typed_parameter"
            | "typed_default_parameter"
            | "class_definition"
            | "function_definition"
            | "decorator"
            | "aliased_import"
            | "dotted_name"
            | "type" => {}
            _ => {
                let screaming = name.len() >= 2
                    && name
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                    && name.starts_with(|c: char| c.is_ascii_uppercase());
                let in_annotation = std::iter::successors(Some(parent), |p| p.parent()).any(|p| p.kind() == "type");

                if screaming && !in_annotation && !is_call_target(node) && !super::is_own_name(node) {
                    push_use(
                        self.out,
                        scope.current,
                        name,
                        None,
                        node,
                        self.src,
                        node.start_position().row + 1,
                    );
                }
            }
        }
    }

    fn place(&self) -> Placement<'a> {
        Placement {
            src: self.src,
            line_off: 0,
            test: false,
        }
    }

    /// `import a.b.c`, `import a.b as x`.
    fn import(&mut self, node: Node) {
        let mut cursor = node.walk();

        for name in node.children_by_field_name("name", &mut cursor) {
            match name.kind() {
                "dotted_name" => {
                    let full = compact(text(name, self.src));
                    let head = full.split('.').next().unwrap_or("").to_string();

                    self.push_import(head.clone(), "*".into(), head);
                }
                "aliased_import" => {
                    let (Some(module), Some(alias)) =
                        (name.child_by_field_name("name"), name.child_by_field_name("alias"))
                    else {
                        continue;
                    };

                    self.push_import(
                        text(alias, self.src).to_string(),
                        "*".into(),
                        compact(text(module, self.src)),
                    );
                }
                _ => {}
            }
        }
    }

    /// `from a.b import c, d as e`, `from . import x`, `from ..m import y`.
    fn import_from(&mut self, node: Node) {
        let Some(module) = node.child_by_field_name("module_name") else {
            return;
        };
        let source = compact(text(module, self.src));
        let mut cursor = node.walk();

        // copied into every name of the statement
        if !fits(&source) {
            self.out.dropped += 1;

            return;
        }

        for name in node.children_by_field_name("name", &mut cursor) {
            match name.kind() {
                "dotted_name" => {
                    let orig = compact(text(name, self.src));

                    self.push_import(orig.clone(), orig, source.clone());
                }
                "aliased_import" => {
                    let (Some(orig), Some(alias)) =
                        (name.child_by_field_name("name"), name.child_by_field_name("alias"))
                    else {
                        continue;
                    };

                    self.push_import(
                        text(alias, self.src).to_string(),
                        compact(text(orig, self.src)),
                        source.clone(),
                    );
                }
                _ => {}
            }
        }
    }

    fn push_import(&mut self, local: String, orig: String, source: String) {
        if local.is_empty() || source.is_empty() {
            return;
        }

        self.out.add_import(Import { local, orig, source });
    }

    /// Base classes are type references made by the class itself.
    fn bases(&mut self, node: Node, class: usize) {
        let Some(bases) = node.child_by_field_name("superclasses") else {
            return;
        };
        let derived = self.out.syms.get(class).map(|sym| sym.name.clone()).unwrap_or_default();
        let mut cursor = bases.walk();

        for base in bases.named_children(&mut cursor) {
            let line = base.start_position().row + 1;

            match base.kind() {
                "identifier" => {
                    push_ref(self.out, Some(class), text(base, self.src), "type", None, line);
                    push_base(self.out, &derived, text(base, self.src), line);
                }
                "attribute" => {
                    if let (Some(object), Some(name)) = (
                        base.child_by_field_name("object"),
                        base.child_by_field_name("attribute"),
                    ) {
                        let qual = dotted(object, self.src, 0);

                        push_ref(self.out, Some(class), text(name, self.src), "type", qual, line);
                        push_base(self.out, &derived, text(name, self.src), line);
                    }
                }
                // `Base[int]`: the base is what is subscripted
                "subscript" => {
                    if let Some(value) = base.child_by_field_name("value") {
                        let name = match value.kind() {
                            "identifier" => Some(text(value, self.src)),
                            "attribute" => value.child_by_field_name("attribute").map(|n| text(n, self.src)),
                            _ => None,
                        };

                        if let Some(name) = name {
                            push_base(self.out, &derived, name, line);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// `f()`, `x.f()`, `self.m()`, `pkg.mod.f()`.
    fn call(&mut self, node: Node, scope: Scope) {
        let Some(function) = node.child_by_field_name("function") else {
            return;
        };

        match function.kind() {
            "identifier" => {
                let line = function.start_position().row + 1;

                push_ref(self.out, scope.current, text(function, self.src), "call", None, line);
            }
            "attribute" => {
                let (Some(object), Some(name)) = (
                    function.child_by_field_name("object"),
                    function.child_by_field_name("attribute"),
                ) else {
                    return;
                };
                let line = name.start_position().row + 1;
                let name = text(name, self.src);

                let recv = expr(object, self.src, 0);

                if object.kind() == "identifier"
                    && matches!(text(object, self.src), "self" | "cls")
                    && let Some(class) = scope.class
                {
                    push_mcall(self.out, scope.current, name, Some(class.to_string()), recv, line);
                } else if let Some(qual) = dotted(object, self.src, 0) {
                    // a module, a class or a variable: told apart once the imports are known
                    push_ref_recv(self.out, scope.current, name, "call", Some(qual), recv, line);
                } else {
                    push_mcall(self.out, scope.current, name, None, recv, line);
                }
            }
            _ => {}
        }
    }

    /// `@decorator` and `@pkg.decorator` call the decorator (`@decorator(...)` is an ordinary call).
    fn decorator(&mut self, node: Node, scope: Scope) {
        let Some(expression) = node.named_child(0) else {
            return;
        };
        let line = expression.start_position().row + 1;

        match expression.kind() {
            "identifier" => push_ref(self.out, scope.current, text(expression, self.src), "call", None, line),
            "attribute" => {
                if let (Some(object), Some(name)) = (
                    expression.child_by_field_name("object"),
                    expression.child_by_field_name("attribute"),
                ) {
                    let qual = dotted(object, self.src, 0);
                    let kind = if qual.is_some() { "call" } else { "mcall" };

                    push_ref(self.out, scope.current, text(name, self.src), kind, qual, line);
                }
            }
            _ => {}
        }
    }

    /// A plain string that looks like a code name. Docstrings and f-strings with parts are skipped.
    fn string(&mut self, node: Node, scope: Scope) {
        if node
            .parent()
            .is_some_and(|parent| parent.kind() == "expression_statement")
        {
            return;
        }

        let mut cursor = node.walk();
        let mut content = None;

        for child in node.children(&mut cursor) {
            match child.kind() {
                "interpolation" => return,
                "string_content" if content.is_none() => content = Some(child),
                "string_content" => return,
                _ => {}
            }
        }

        let Some(content) = content else {
            return;
        };
        let value = text(content, self.src);

        if ident_like(value) {
            push_ref(
                self.out,
                scope.current,
                value,
                "str",
                None,
                node.start_position().row + 1,
            );
        } else {
            push_word(self.out, scope.current, value, node, self.src);
        }
    }

    /// The class names in a written annotation (`x: Client`, `-> list[Response]`, `m.Request`). Lowercase
    /// names are builtins or modules.
    fn annotation(&mut self, node: Node, scope: Scope) {
        // `Optional[Optional[...` a hundred thousand deep: finding the parent of a node costs its depth, so
        // the names this deep are not looked at
        let mut stack = vec![(node, 0_usize)];

        while let Some((next, depth)) = stack.pop() {
            if next.kind() == "string" || depth > MAX_DEPTH {
                continue;
            }

            if next.kind() == "identifier" {
                self.type_name(next, scope);
            }

            let mut cursor = next.walk();
            stack.extend(next.named_children(&mut cursor).map(|child| (child, depth + 1)));
        }
    }

    fn type_name(&mut self, node: Node, scope: Scope) {
        let name = text(node, self.src);

        if !name.starts_with(|c: char| c.is_ascii_uppercase()) {
            return;
        }

        let mut qual = None;

        if let Some(parent) = node
            .parent()
            .filter(|p| matches!(p.kind(), "attribute" | "member_type"))
        {
            let first = parent.named_child(0);

            if first.is_some_and(|first| first.id() == node.id()) {
                return; // the qualifier of `m.Request`, not the type
            }

            qual = first
                .filter(|first| first.kind() == "identifier")
                .map(|first| text(first, self.src).to_string());
        }

        push_ref(
            self.out,
            scope.current,
            name,
            "type",
            qual,
            node.start_position().row + 1,
        );
    }
}

/// Deeper receiver expressions are not followed.
const MAX_EXPR_DEPTH: usize = 12;

/// The receiver expression a node stands for (see `graph/infer.rs`), when it can be told from the syntax
/// and is short enough to store.
fn expr(node: Node, src: &[u8], depth: usize) -> Option<String> {
    expr_of(node, src, depth).filter(|e| fits(e))
}

fn expr_of(node: Node, src: &[u8], depth: usize) -> Option<String> {
    if depth > MAX_EXPR_DEPTH {
        return None;
    }

    match node.kind() {
        "identifier" => Some(text(node, src).to_string()),
        "attribute" => {
            let object = expr(node.child_by_field_name("object")?, src, depth + 1)?;

            Some(format!(
                "{object}.{}",
                text(node.child_by_field_name("attribute")?, src)
            ))
        }
        "call" => {
            let function = node.child_by_field_name("function")?;

            match function.kind() {
                "identifier" => Some(format!("{}()", text(function, src))),
                "attribute" => Some(format!("{}()", expr(function, src, depth + 1)?)),
                _ => None,
            }
        }
        "await" | "parenthesized_expression" => expr(node.named_child(0)?, src, depth + 1),
        "string" | "concatenated_string" => {
            let bytes = node
                .child(0)
                .is_some_and(|start| start.kind() == "string_start" && text(start, src).starts_with(['b', 'B']));

            Some(if bytes { ":bytes" } else { ":str" }.to_string())
        }
        "list" | "list_comprehension" => Some(":list".to_string()),
        "dictionary" | "dictionary_comprehension" => Some(":dict".to_string()),
        "set" | "set_comprehension" => Some(":set".to_string()),
        "tuple" => Some(":tuple".to_string()),
        "integer" => Some(":int".to_string()),
        "float" => Some(":float".to_string()),
        "true" | "false" => Some(":bool".to_string()),
        _ => None,
    }
}

/// What a variable initialised with `value` is: the expression to follow (a class called is its instance).
fn initialiser(value: Node, src: &[u8]) -> Option<String> {
    expr(value, src, 0).map(|e| format!("={e}"))
}

/// True for a class that lists `Enum`, `IntEnum`, `Flag`... among its bases.
fn is_enumeration(class: Node, src: &[u8]) -> bool {
    let Some(bases) = class.child_by_field_name("superclasses") else {
        return false;
    };
    let mut cursor = bases.walk();

    bases.named_children(&mut cursor).any(|base| {
        let name = text(base, src).rsplit('.').next().unwrap_or("");

        name.ends_with("Enum") || name.ends_with("Flag")
    })
}

/// The name of `NAME = value` in the body of an enumeration.
fn member_name<'t>(node: Node<'t>, src: &[u8]) -> Option<Node<'t>> {
    let left = node.child_by_field_name("left").filter(|l| l.kind() == "identifier")?;
    let name = text(left, src);

    node.parent()
        .and_then(|statement| statement.parent())
        .filter(|block| block.kind() == "block")
        .and(Some(left))
        .filter(|_| !name.starts_with('_'))
}

/// True when `node` is the called name of a call.
fn is_call_target(node: Node) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "call"
            && parent
                .child_by_field_name("function")
                .is_some_and(|function| function.id() == node.id())
    })
}

/// The name of a module-level `NAME = value` (not a dunder like `__all__`).
fn module_constant<'t>(node: Node<'t>, src: &[u8]) -> Option<Node<'t>> {
    let statement = node.parent().filter(|p| p.kind() == "expression_statement")?;

    statement.parent().filter(|p| p.kind() == "module")?;

    let left = node.child_by_field_name("left").filter(|l| l.kind() == "identifier")?;
    let name = text(left, src);

    (!(name.starts_with("__") && name.ends_with("__"))).then_some(left)
}

/// `a`, `a.b.c` -> the dotted text; `None` when the expression is anything else (a call, a subscript).
fn dotted(node: Node, src: &[u8], depth: usize) -> Option<String> {
    if depth > 8 {
        return None;
    }

    match node.kind() {
        "identifier" => Some(text(node, src).to_string()),
        "attribute" => {
            let object = dotted(node.child_by_field_name("object")?, src, depth + 1)?;
            let name = text(node.child_by_field_name("attribute")?, src);

            Some(format!("{object}.{name}"))
        }
        _ => None,
    }
}

/// A module path without the whitespace or line continuations the source may hold inside it.
fn compact(path: &str) -> String {
    path.chars().filter(|c| !c.is_whitespace() && *c != '\\').collect()
}

/// A qualified call whose first name is not imported is a call on a value: a method call.
fn method_calls(out: &mut Extract) {
    let imported: HashSet<&str> = out.imports.iter().map(|import| import.local.as_str()).collect();

    for reference in &mut out.refs {
        if reference.kind != "call" {
            continue;
        }

        let Some(qual) = &reference.qual else {
            continue;
        };

        if !imported.contains(qual.split('.').next().unwrap_or(qual)) {
            reference.kind = "mcall";
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> Extract {
        let mut out = Extract::default();
        extract(&mut Parser::new(), "app/mod.py", src, &mut out);
        out
    }

    fn defs(out: &Extract) -> Vec<(&str, &str, Option<&str>)> {
        out.syms
            .iter()
            .map(|s| (s.name.as_str(), s.kind, s.parent.as_deref()))
            .collect()
    }

    fn refs(out: &Extract) -> Vec<(&str, &str, Option<&str>, Option<&str>)> {
        out.refs
            .iter()
            .map(|r| {
                (
                    r.name.as_str(),
                    r.kind,
                    r.qual.as_deref(),
                    r.from.and_then(|f| out.syms.get(f)).map(|s| s.name.as_str()),
                )
            })
            .collect()
    }

    fn imports(out: &Extract) -> Vec<(&str, &str, &str)> {
        out.imports
            .iter()
            .map(|i| (i.local.as_str(), i.orig.as_str(), i.source.as_str()))
            .collect()
    }

    #[test]
    fn methods_get_their_class_and_nested_functions_stay_functions() {
        let out = run("MAX_RETRIES = 3\n__all__ = ['Client']\n\n\
             class Client(Base):\n    def send(self):\n        def inner():\n            pass\n        return inner()\n\n\
             \x20   class Meta:\n        def hook(self):\n            pass\n\n\
             def helper():\n    pass\n");
        let found = defs(&out);

        assert!(found.contains(&("MAX_RETRIES", "const", None)), "{found:?}");
        assert!(!found.iter().any(|d| d.0 == "__all__"), "{found:?}");
        assert!(found.contains(&("Client", "class", None)));
        assert!(found.contains(&("send", "method", Some("Client"))));
        assert!(found.contains(&("inner", "fn", None)));
        assert!(found.contains(&("Meta", "class", None)));
        assert!(found.contains(&("hook", "method", Some("Meta"))));
        assert!(found.contains(&("helper", "fn", None)));
    }

    #[test]
    fn decorated_and_async_methods_are_methods() {
        let out = run("class Api:\n    @staticmethod\n    def make():\n        pass\n\n\
             \x20   @property\n    @cached(ttl=3)\n    async def value(self):\n        pass\n\n\
             @route('/x')\ndef view():\n    pass\n");
        let found = defs(&out);
        let calls = refs(&out);

        assert!(found.contains(&("make", "method", Some("Api"))), "{found:?}");
        assert!(found.contains(&("value", "method", Some("Api"))), "{found:?}");
        assert!(found.contains(&("view", "fn", None)), "{found:?}");
        assert!(
            calls.contains(&("staticmethod", "call", None, Some("Api"))),
            "{calls:?}"
        );
        assert!(calls.contains(&("cached", "call", None, Some("Api"))), "{calls:?}");
        assert!(calls.contains(&("route", "call", None, None)), "{calls:?}");
    }

    #[test]
    fn imports_keep_the_module_as_written() {
        let out = run(
            "import os, a.b.c\nimport a.b as x\nfrom a.b import c, d as e\nfrom . import f\n\
             from ..m import (y,\n    z as w)\nfrom .m import *\nfrom __future__ import annotations\n",
        );
        let found = imports(&out);

        assert!(found.contains(&("os", "*", "os")), "{found:?}");
        assert!(found.contains(&("a", "*", "a")), "{found:?}");
        assert!(found.contains(&("x", "*", "a.b")), "{found:?}");
        assert!(found.contains(&("c", "c", "a.b")), "{found:?}");
        assert!(found.contains(&("e", "d", "a.b")), "{found:?}");
        assert!(found.contains(&("f", "f", ".")), "{found:?}");
        assert!(found.contains(&("y", "y", "..m")), "{found:?}");
        assert!(found.contains(&("w", "z", "..m")), "{found:?}");
        assert_eq!(found.len(), 8, "a wildcard and __future__ bring no names: {found:?}");
    }

    #[test]
    fn calls_tell_modules_from_values_and_self_from_the_class() {
        let out = run("import pkg.mod as m\nimport os.path\n\n\
             class Client:\n    def send(self, request):\n        self.build()\n        cls_obj = Client.make()\n\
             \x20       self.session.get()\n        m.run()\n        os.path.join()\n        request.finish()\n\
             \x20       make()().again()\n\n    def build(self):\n        pass\n");
        let calls = refs(&out);

        assert!(
            calls.contains(&("build", "mcall", Some("Client"), Some("send"))),
            "{calls:?}"
        );
        assert!(
            calls.contains(&("make", "mcall", Some("Client"), Some("send"))),
            "{calls:?}"
        );
        assert!(
            calls.contains(&("get", "mcall", Some("self.session"), Some("send"))),
            "{calls:?}"
        );
        assert!(calls.contains(&("run", "call", Some("m"), Some("send"))), "{calls:?}");
        assert!(
            calls.contains(&("join", "call", Some("os.path"), Some("send"))),
            "{calls:?}"
        );
        assert!(
            calls.contains(&("finish", "mcall", Some("request"), Some("send"))),
            "{calls:?}"
        );
        assert!(calls.contains(&("again", "mcall", None, Some("send"))), "{calls:?}");
        assert!(calls.contains(&("make", "call", None, Some("send"))), "{calls:?}");
    }

    #[test]
    fn base_classes_annotations_and_strings_are_references() {
        let out = run(
            "class Client(Base, pkg.Mixin, metaclass=Meta):\n    def send(self, r: Request, *, t: m.Timeout = None) -> list[Response]:\n        return route('handle_request', f'not_{r}', \"two words\")\n",
        );
        let found = refs(&out);

        assert!(found.contains(&("Base", "type", None, Some("Client"))), "{found:?}");
        assert!(
            found.contains(&("Mixin", "type", Some("pkg"), Some("Client"))),
            "{found:?}"
        );
        assert!(found.contains(&("Request", "type", None, Some("send"))), "{found:?}");
        assert!(
            found.contains(&("Timeout", "type", Some("m"), Some("send"))),
            "{found:?}"
        );
        assert!(found.contains(&("Response", "type", None, Some("send"))), "{found:?}");
        assert!(
            found.contains(&("handle_request", "str", None, Some("send"))),
            "{found:?}"
        );
        assert!(
            !found.iter().any(|r| r.0 == "Meta" || r.0 == "list" || r.0 == "m"),
            "{found:?}"
        );
        assert_eq!(found.iter().filter(|r| r.1 == "str").count(), 1, "{found:?}");
    }

    #[test]
    fn docstrings_are_not_string_links() {
        let out = run("def f():\n    \"\"\"some_helper_name\"\"\"\n");

        assert!(refs(&out).iter().all(|r| r.1 != "str"));
    }

    #[test]
    fn malformed_python_does_not_panic() {
        let deep = format!("x = {}1{}\n", "(".repeat(20_000), ")".repeat(20_000));
        let junk = [
            "",
            "\0\0\0",
            "def (((((",
            "class :\n  def\n",
            "from import\nimport\nfrom . import\n",
            "@\n@@\ndef",
            "    indented\n  odd\n",
            "\u{feff}\u{1F600}",
            deep.as_str(),
        ];

        for source in junk {
            let _ = run(source);
        }
    }

    /// The `base` references of a file as (base, derived).
    fn bases(out: &Extract) -> Vec<(&str, &str)> {
        out.refs
            .iter()
            .filter(|r| r.kind == "base")
            .map(|r| (r.name.as_str(), r.qual.as_deref().unwrap_or("")))
            .collect()
    }

    #[test]
    fn a_class_records_each_base_it_lists() {
        let out = run(
            "class A(Base, mixins.Mixin, Generic[T], metaclass=Meta):\n    pass\n\nclass B(a.Other[int]):\n    pass\n\nclass C:\n    pass\n",
        );

        assert_eq!(
            bases(&out),
            [("Base", "A"), ("Mixin", "A"), ("Generic", "A"), ("Other", "B")]
        );
        // the base is still a type reference of the class
        assert!(out.refs.iter().any(|r| r.kind == "type" && r.name == "Base"));
    }

    #[test]
    fn a_with_statement_binds_the_name_to_what_it_opens() {
        let out = run("def f():\n    with Client() as client, open(p) as handle:\n        client.go()\n");
        let binds: Vec<(&str, &str)> = out.binds.iter().map(|b| (b.name.as_str(), b.ty.as_str())).collect();

        assert!(binds.contains(&("client", "=Client()")), "{binds:?}");
    }
}
