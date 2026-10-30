//! Go, read through the grammar's `tags.scm`: functions, methods, types, constants and calls.

use tree_sitter::{Language, Node, Parser};

use super::tags::{self, Hooks, TagLang};
use super::{Extract, Import, MAX_DEPTH, Ref, Sym, constant_like, first_line, fits, push_bind, text, written_type};

static GO: TagLang = TagLang::new(
    go_language,
    tree_sitter_go::TAGS_QUERY,
    Hooks {
        receiver,
        imports,
        operand,
        enrich,
    },
    &["interpreted_string_literal"],
    "selector_expression",
);

fn go_language() -> Language {
    tree_sitter_go::LANGUAGE.into()
}

/// Reads a Go file.
pub(super) fn extract(parser: &mut Parser, _path: &str, src: &str, out: &mut Extract) {
    tags::extract(parser, &GO, src, out);
}

/// The receiver type of a method: `func (s *Server<T>) Run()` -> `Server`.
fn receiver(node: Node, src: &[u8]) -> Option<String> {
    let receiver = node.child_by_field_name("receiver")?;
    let mut cursor = receiver.walk();
    let param = receiver
        .named_children(&mut cursor)
        .find(|p| p.kind() == "parameter_declaration")?;
    let mut ty = param.child_by_field_name("type")?;

    while matches!(ty.kind(), "pointer_type" | "generic_type") {
        ty = ty.named_child(0)?;
    }

    Some(text(ty, src).to_string())
}

/// Deeper receiver expressions are not followed.
const MAX_EXPR_DEPTH: usize = 12;

/// The receiver expression a node stands for (see `graph/infer.rs`), when it can be told from the syntax.
fn operand(node: Node, src: &[u8]) -> Option<String> {
    expr(node, src, 0)
}

/// [`expr_of`] when the result is short enough to store.
fn expr(node: Node, src: &[u8], depth: usize) -> Option<String> {
    expr_of(node, src, depth).filter(|e| fits(e))
}

fn expr_of(node: Node, src: &[u8], depth: usize) -> Option<String> {
    if depth > MAX_EXPR_DEPTH {
        return None;
    }

    match node.kind() {
        "identifier" => Some(text(node, src).to_string()),
        "selector_expression" => {
            let field = node.child_by_field_name("field")?;
            let operand = expr(node.child_by_field_name("operand")?, src, depth + 1)?;

            Some(format!("{operand}.{}", text(field, src)))
        }
        "call_expression" => {
            let function = node.child_by_field_name("function")?;

            match function.kind() {
                "identifier" => Some(format!("{}()", text(function, src))),
                "selector_expression" => Some(format!("{}()", expr(function, src, depth + 1)?)),
                _ => None,
            }
        }
        "parenthesized_expression" | "unary_expression" => expr(
            node.child_by_field_name("operand").or_else(|| node.named_child(0))?,
            src,
            depth + 1,
        ),
        "composite_literal" => {
            let ty = written_type(text(node.child_by_field_name("type")?, src))?;

            Some(format!(":{ty}"))
        }
        "type_assertion_expression" => {
            let ty = written_type(text(node.child_by_field_name("type")?, src))?;

            Some(format!(":{ty}"))
        }
        "interpreted_string_literal" | "raw_string_literal" => Some(":string".to_string()),
        "int_literal" => Some(":int".to_string()),
        "float_literal" => Some(":float64".to_string()),
        _ => None,
    }
}

/// What a variable initialised with `value` is: the type of a literal, or the expression to follow.
fn initialiser(value: Node, src: &[u8]) -> Option<String> {
    let inner = if value.kind() == "unary_expression" {
        value.child_by_field_name("operand")?
    } else {
        value
    };

    if inner.kind() == "composite_literal" {
        return written_type(text(inner.child_by_field_name("type")?, src));
    }

    expr(value, src, 0).map(|e| format!("={e}"))
}

/// The first type a function returns.
fn result(node: Node, src: &[u8]) -> Option<String> {
    let result = node.child_by_field_name("result")?;

    if result.kind() != "parameter_list" {
        return written_type(text(result, src));
    }

    let mut cursor = result.walk();
    let first = result
        .named_children(&mut cursor)
        .find(|p| p.kind() == "parameter_declaration" || p.kind() == "variadic_parameter_declaration")?;

    written_type(text(first.child_by_field_name("type")?, src))
}

/// Reads what the tags query does not see: return types, parameters, local variables, struct fields and
/// constants.
fn enrich(root: Node, src: &[u8], out: &mut Extract, first_sym: usize) {
    Reader { src, out, first_sym }.node(root, None, 0);
}

struct Reader<'a> {
    src: &'a [u8],
    out: &'a mut Extract,
    first_sym: usize,
}

impl Reader<'_> {
    /// The symbol this file defined with this name on the line of `node`.
    fn sym_at(&self, name: &str, node: Node) -> Option<usize> {
        let line = node.start_position().row + 1;

        (self.first_sym..self.out.syms.len()).find(|i| {
            self.out
                .syms
                .get(*i)
                .is_some_and(|sym| sym.name == name && sym.line == line && !matches!(sym.kind, "const"))
        })
    }

    fn node(&mut self, node: Node, scope: Option<usize>, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }

        let src = self.src;
        let mut scope = scope;

        match node.kind() {
            "function_declaration" | "method_declaration" => {
                let name = node.child_by_field_name("name").map(|n| text(n, src)).unwrap_or("");

                if let Some(sym) = self.sym_at(name, node) {
                    scope = Some(sym);

                    if let Some(def) = self.out.syms.get_mut(sym) {
                        def.ret = result(node, src);
                    }

                    if let Some(receiver) = node.child_by_field_name("receiver") {
                        self.parameters(receiver, scope);
                    }

                    if let Some(parameters) = node.child_by_field_name("parameters") {
                        self.parameters(parameters, scope);
                    }
                }
            }
            "type_spec" => self.fields(node),
            "const_spec" => self.constants(node),
            "short_var_declaration" => self.short_var(node, scope),
            "range_clause" => self.range(node, scope),
            "var_spec" => self.var_spec(node, scope),
            "identifier" => self.mention(node, scope),
            _ => {}
        }

        let mut cursor = node.walk();

        for child in node.children(&mut cursor) {
            self.node(child, scope, depth + 1);
        }
    }

    /// The parameters (or the receiver) of a function: each name with its written type.
    fn parameters(&mut self, list: Node, scope: Option<usize>) {
        let src = self.src;
        let mut cursor = list.walk();

        for param in list.named_children(&mut cursor) {
            let Some(ty) = param
                .child_by_field_name("type")
                .and_then(|ty| written_type(text(ty, src)))
            else {
                continue;
            };
            let mut names = param.walk();

            for name in param.children_by_field_name("name", &mut names) {
                push_bind(self.out, scope, text(name, src), &ty, "param");
            }
        }
    }

    /// The fields of a struct type: `Server.store`.
    fn fields(&mut self, spec: Node) {
        let src = self.src;
        let (Some(name), Some(ty)) = (spec.child_by_field_name("name"), spec.child_by_field_name("type")) else {
            return;
        };

        if ty.kind() != "struct_type" {
            return;
        }

        let owner = text(name, src);

        // the owner is copied into the name of every field
        if !fits(owner) {
            self.out.dropped += 1;

            return;
        }

        let sym = self.sym_at(owner, spec);
        let mut stack = vec![ty];

        while let Some(next) = stack.pop() {
            if next.kind() == "field_declaration" {
                let field_ty = next
                    .child_by_field_name("type")
                    .and_then(|field_ty| written_type(text(field_ty, src)));
                let mut names = next.walk();

                if let Some(field_ty) = field_ty {
                    for field in next.children_by_field_name("name", &mut names) {
                        push_bind(
                            self.out,
                            sym,
                            &format!("{owner}.{}", text(field, src)),
                            &field_ty,
                            "field",
                        );
                    }
                }

                continue;
            }

            let mut walk = next.walk();
            stack.extend(next.named_children(&mut walk));
        }
    }

    /// `const ( A Kind = iota; B )` defines one constant per name.
    fn constants(&mut self, spec: Node) {
        let src = self.src;
        let mut cursor = spec.walk();

        for name in spec.children_by_field_name("name", &mut cursor) {
            self.out.add_sym(Sym {
                name: text(name, src).to_string(),
                kind: "const",
                parent: None,
                line: name.start_position().row + 1,
                end_line: spec.end_position().row + 1,
                sig: first_line(spec, src),
                ret: None,
                test: false,
            });
        }
    }

    /// `x := NewT()`, `t := &T{}`: a variable initialised with something whose type is told.
    fn short_var(&mut self, node: Node, scope: Option<usize>) {
        let src = self.src;
        let (Some(left), Some(right)) = (node.child_by_field_name("left"), node.child_by_field_name("right")) else {
            return;
        };
        let mut a = left.walk();
        let names: Vec<Node> = left.named_children(&mut a).collect();
        let mut b = right.walk();
        let values: Vec<Node> = right.named_children(&mut b).collect();

        // `a, b := f()` gives the first name the first result
        let pairs = if names.len() == values.len() { names.len() } else { 1 };

        for (name, value) in names.iter().zip(&values).take(pairs) {
            if name.kind() == "identifier"
                && let Some(ty) = initialiser(*value, src)
            {
                push_bind(self.out, scope, text(*name, src), &ty, "var");
            }
        }
    }

    /// `for _, item := range items`: the second name is an element of what is ranged over.
    fn range(&mut self, node: Node, scope: Option<usize>) {
        let src = self.src;
        let (Some(left), Some(right)) = (node.child_by_field_name("left"), node.child_by_field_name("right")) else {
            return;
        };
        let mut cursor = left.walk();
        let names: Vec<Node> = left.named_children(&mut cursor).collect();

        if let [_, value] = names.as_slice()
            && value.kind() == "identifier"
            && let Some(ranged) = expr(right, src, 0)
        {
            push_bind(self.out, scope, text(*value, src), &format!("=@{ranged}"), "var");
        }
    }

    /// `var v T`, `var v = NewT()`.
    fn var_spec(&mut self, node: Node, scope: Option<usize>) {
        let src = self.src;
        let ty = node
            .child_by_field_name("type")
            .and_then(|ty| written_type(text(ty, src)))
            .or_else(|| {
                let value = node.child_by_field_name("value")?.named_child(0)?;

                initialiser(value, src)
            });
        let Some(ty) = ty else {
            return;
        };
        let mut cursor = node.walk();

        for name in node.children_by_field_name("name", &mut cursor) {
            push_bind(self.out, scope, text(name, src), &ty, "var");
        }
    }

    /// A constant-looking name used as a value: `case EvtNew:`, `x == ModeFast`.
    fn mention(&mut self, node: Node, scope: Option<usize>) {
        let Some(parent) = node.parent() else {
            return;
        };
        let name = text(node, self.src);

        if !constant_like(name) || super::is_own_name(node) {
            return;
        }

        let skipped = match parent.kind() {
            "call_expression" => parent
                .child_by_field_name("function")
                .is_some_and(|function| function.id() == node.id()),
            "const_spec" | "var_spec" | "parameter_declaration" | "import_spec" | "labeled_statement" => true,
            "short_var_declaration" => false,
            "expression_list" => {
                parent
                    .parent()
                    .is_some_and(|grand| matches!(grand.kind(), "const_spec" | "var_spec"))
                    && parent
                        .parent()
                        .and_then(|grand| grand.child_by_field_name("name"))
                        .is_some_and(|first| first.start_byte() == node.start_byte())
            }
            _ => false,
        };

        if skipped {
            return;
        }

        self.out.add_ref(Ref {
            from: scope,
            name: name.to_string(),
            kind: "use",
            qual: None,
            recv: None,
            line: node.start_position().row + 1,
            handler: super::handled(node, self.src),
        });
    }
}

fn imports(root: Node, src: &[u8], out: &mut Vec<Import>) {
    let mut stack: Vec<Node> = Vec::new();
    let mut cursor = root.walk();

    for declaration in root
        .named_children(&mut cursor)
        .filter(|d| d.kind() == "import_declaration")
    {
        stack.push(declaration);
    }

    while let Some(node) = stack.pop() {
        if node.kind() == "import_spec" {
            let Some(path) = node.child_by_field_name("path") else {
                continue;
            };
            let path = text(path, src).trim_matches(['"', '`']).to_string();
            let local = node
                .child_by_field_name("name")
                .map(|name| text(name, src).to_string())
                .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(&path).to_string());

            out.push(Import {
                local,
                orig: "*".into(),
                source: path,
            });
            continue;
        }

        let mut inner = node.walk();
        stack.extend(node.named_children(&mut inner));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> Extract {
        let mut out = Extract::default();
        extract(&mut Parser::new(), "main.go", src, &mut out);
        out
    }

    #[test]
    fn definitions_methods_imports_and_calls() {
        let out = run(
            "package main\n\nimport (\n\t\"fmt\"\n\tgit \"example.com/app/git\"\n)\n\n\
             type Server struct{}\n\nfunc (s *Server) Run() { helper(); git.Diff(); s.stop(); fmt.Println(\"x\") }\n\
             func helper() {}\n",
        );

        let defs: Vec<(&str, &str, Option<&str>)> = out
            .syms
            .iter()
            .map(|s| (s.name.as_str(), s.kind, s.parent.as_deref()))
            .collect();
        assert!(defs.contains(&("Server", "struct", None)));
        assert!(defs.contains(&("Run", "method", Some("Server"))));
        assert!(defs.contains(&("helper", "fn", None)));

        let imports: Vec<(&str, &str)> = out
            .imports
            .iter()
            .map(|i| (i.local.as_str(), i.source.as_str()))
            .collect();
        assert!(imports.contains(&("fmt", "fmt")));
        assert!(imports.contains(&("git", "example.com/app/git")));

        let calls: Vec<(&str, &str, Option<&str>, bool)> = out
            .refs
            .iter()
            .map(|r| (r.name.as_str(), r.kind, r.qual.as_deref(), r.from.is_some()))
            .collect();
        assert!(calls.contains(&("helper", "call", None, true)));
        assert!(calls.contains(&("Diff", "call", Some("git"), true)));
        assert!(calls.contains(&("stop", "mcall", Some("s"), true)));
    }
}
