//! Receiver types from what the code writes down: a method call `x.m()` only means the methods of the
//! type of `x`.
//!
//! The extractors keep the written types in three places: `binds` (a variable, parameter or field with its
//! annotation, or `=expr` when only an initialiser says what it is), `symbols.ret` (the return type of a
//! function) and `refs.recv` (the receiver of a method call as a small expression).
//!
//! A receiver expression is a chain of segments joined by `.`. The first segment is `:Type` (a literal or
//! `new Type`), `name()` (a call), `Type::name()` (a path call) or `name` (a variable or an imported module);
//! the next ones are `field` or `method()`. [`Graph::receiver_type`] follows the chain: a variable through
//! its scope, its file and, in Go, its package; a call through the return type of what it resolves to; a
//! field through the written type of the field of the type before it. Anything it cannot follow is
//! unknown, and the call keeps the old behaviour of matching by name.

use std::collections::HashMap;

use rusqlite::Connection;

use super::{File, Graph, MAX_SETTLED, Pace, Ref, Sym, TOO_LARGE, TYPE_KINDS, db, poll, rules};

/// The type of a value that comes from outside the repository (a standard library call, an external
/// package): no definition of the repository can be its method.
const EXTERN: &str = "<extern>";

/// The most written types a graph holds: the same cap the store keeps while writing.
use crate::code::store::MAX_BINDS;

/// Chains longer than this, and `=expr` bindings nested deeper, are unknown.
const MAX_DEPTH: usize = 6;

/// A written type of a field, and where it was written.
struct Field {
    ty: String,
    file: usize,
    scope: Option<usize>,
}

/// What a table entry or a string costs besides its text, in bytes, for an estimate of memory.
const ENTRY_BYTES: usize = 64;

/// The written types of a repository.
#[derive(Default)]
pub(super) struct Types {
    /// Per (file, scope symbol; `None` = the file): name -> written types.
    vars: HashMap<(usize, Option<usize>), HashMap<String, Vec<String>>>,
    /// Per (language, owner type, field).
    fields: HashMap<(&'static str, String, String), Vec<Field>>,
    /// Go package-level variables: (package, name) -> (file, written type).
    package_vars: HashMap<(String, String), Vec<(usize, String)>>,
    /// Per symbol: the innermost symbol of the same file that contains it.
    outer: Vec<Option<usize>>,
}

impl Types {
    /// About how many bytes the written types take: their text, which a hostile file makes as long as the
    /// storage allows, and a fixed amount per entry.
    pub(super) fn estimated_bytes(&self) -> usize {
        let mut bytes = self.outer.len().saturating_mul(16);

        for names in self.vars.values() {
            for (name, types) in names {
                bytes = bytes
                    .saturating_add(ENTRY_BYTES + name.len())
                    .saturating_add(types.iter().map(|ty| ENTRY_BYTES + ty.len()).sum::<usize>());
            }
        }

        for ((_, owner, field), list) in &self.fields {
            bytes = bytes
                .saturating_add(ENTRY_BYTES + owner.len() + field.len())
                .saturating_add(list.iter().map(|found| ENTRY_BYTES + found.ty.len()).sum::<usize>());
        }

        for ((package, name), list) in &self.package_vars {
            bytes = bytes
                .saturating_add(ENTRY_BYTES + package.len() + name.len())
                .saturating_add(list.iter().map(|(_, ty)| ENTRY_BYTES + ty.len()).sum::<usize>());
        }

        bytes
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn load(
        conn: &Connection,
        repo: i64,
        file_index: &HashMap<i64, usize>,
        sym_index: &HashMap<i64, usize>,
        files: &[File],
        syms: &[Sym],
        file_syms: &[Vec<usize>],
        pace: &Pace,
    ) -> Result<Types, String> {
        let mut types = Types {
            outer: containers(syms, file_syms),
            ..Types::default()
        };
        let mut statement = conn
            .prepare("SELECT file_id, scope_symbol, name, type, kind FROM binds WHERE repo_id = ?1")
            .map_err(db)?;
        let rows = statement
            .query_map([repo], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(db)?;

        for (count, row) in rows.enumerate() {
            poll(pace, count)?;

            if count >= MAX_BINDS {
                return Err(TOO_LARGE.into());
            }

            let (file_id, scope, name, ty, kind) = row.map_err(db)?;
            let Some(&file) = file_index.get(&file_id) else {
                continue;
            };
            let Some(entry) = files.get(file) else {
                continue;
            };
            let scope = scope.and_then(|id| sym_index.get(&id).copied());

            if kind == "field" {
                if let Some((owner, field)) = name.split_once('.') {
                    types
                        .fields
                        .entry((entry.lang, owner.to_string(), field.to_string()))
                        .or_default()
                        .push(Field { ty, file, scope });
                }

                continue;
            }

            if entry.lang == "go" && scope.is_none() {
                types
                    .package_vars
                    .entry((entry.pkg.clone(), name.clone()))
                    .or_default()
                    .push((file, ty.clone()));
            }

            types
                .vars
                .entry((file, scope))
                .or_default()
                .entry(name)
                .or_default()
                .push(ty);
        }

        Ok(types)
    }
}

/// Per symbol: the innermost other symbol of its file whose lines contain its own.
fn containers(syms: &[Sym], file_syms: &[Vec<usize>]) -> Vec<Option<usize>> {
    let mut outer = vec![None; syms.len()];

    for list in file_syms {
        let mut order: Vec<usize> = list.clone();

        order.sort_by_key(|s| {
            syms.get(*s).map_or((0, std::cmp::Reverse(0), *s), |sym| {
                (sym.line, std::cmp::Reverse(sym.end_line), *s)
            })
        });

        let mut stack: Vec<usize> = Vec::new();

        for s in order {
            let Some(sym) = syms.get(s) else {
                continue;
            };

            while stack
                .last()
                .is_some_and(|top| syms.get(*top).is_none_or(|t| t.end_line < sym.line))
            {
                stack.pop();
            }

            if let Some(slot) = outer.get_mut(s) {
                *slot = stack.last().copied();
            }

            stack.push(s);
        }
    }

    outer
}

/// The type name without what iterating it yields: `slice<Foo>` -> `slice`.
fn base(ty: &str) -> &str {
    ty.split('<').next().unwrap_or(ty)
}

/// What iterating a type yields: `slice<Foo>` -> `Foo`.
fn element(ty: &str) -> Option<String> {
    let inner = ty.split_once('<')?.1.strip_suffix('>')?;

    (!inner.is_empty()).then(|| inner.to_string())
}

/// The only value of `values`, when they all agree.
fn agreed<T: PartialEq + Clone>(values: &[T]) -> Option<T> {
    let first = values.first()?;

    values.iter().all(|value| value == first).then(|| first.clone())
}

impl Graph {
    /// The type name of the receiver of the method call `r`, when its written types say.
    pub(super) fn receiver_type(&self, r: &Ref) -> Option<String> {
        let recv = r.recv.as_deref()?;

        self.eval(r.file, r.from, recv, 0).map(|ty| base(&ty).to_string())
    }

    /// The type of the chain `expr` read in `file` from inside `scope`.
    fn eval(&self, file: usize, scope: Option<usize>, expr: &str, depth: usize) -> Option<String> {
        if depth > MAX_DEPTH {
            return None;
        }

        // `@chain`: what iterating the chain yields
        if let Some(inner) = expr.strip_prefix('@') {
            let iterated = self.eval(file, scope, inner, depth + 1)?;

            return if iterated == EXTERN {
                Some(iterated)
            } else {
                element(&iterated)
            };
        }

        let lang = self.files.get(file)?.lang;
        let mut parts = expr.split('.');
        let first = parts.next()?;
        let mut module: Option<&str> = None;
        let mut ty: String;

        if let Some(literal) = first.strip_prefix(':') {
            ty = literal.to_string();
        } else if let Some(call) = first.strip_suffix("()") {
            ty = match call.rsplit_once("::") {
                Some((qual, name)) => self.call_or_extern(file, Some(qual), name)?,
                None => self.call_or_extern(file, None, call)?,
            };
        } else if let Some(found) = self.variable(file, scope, first, depth) {
            ty = found;
        } else if self.imports.contains_key(&(file, first.to_string())) {
            module = Some(first);
            ty = String::new();
        } else {
            return None;
        }

        for part in parts {
            if ty == EXTERN {
                continue;
            }

            if let Some(imported) = module.take() {
                let name = part.strip_suffix("()")?;

                ty = self.call_or_extern(file, Some(imported), name)?;
            } else if let Some(name) = part.strip_suffix("()") {
                ty = self.method_type(lang, base(&ty), name)?;
            } else {
                // a field, or a property / getter written as a method
                ty = self
                    .field_type(lang, base(&ty), part, depth)
                    .or_else(|| self.method_type(lang, base(&ty), part))?;
            }
        }

        (module.is_none() && !ty.is_empty()).then_some(ty)
    }

    /// The written type of the variable `name` as `scope` sees it: the nearest scope outwards, then the
    /// file, then (Go) the package, then an imported constant of another file.
    fn variable(&self, file: usize, scope: Option<usize>, name: &str, depth: usize) -> Option<String> {
        let mut at = scope;

        loop {
            if let Some(written) = self.types.vars.get(&(file, at)).and_then(|vars| vars.get(name)) {
                return self.written(file, at, written, depth);
            }

            match at {
                Some(inner) => at = self.types.outer.get(inner).copied().flatten(),
                None => break,
            }
        }

        let entry = self.files.get(file)?;

        if entry.lang == "go" {
            let found = self.types.package_vars.get(&(entry.pkg.clone(), name.to_string()))?;
            let (other, _) = found.first()?;
            let tys: Vec<String> = found.iter().map(|(_, ty)| ty.clone()).collect();

            return self.written(*other, None, &tys, depth);
        }

        if self.imports.contains_key(&(file, name.to_string())) {
            let pseudo = Ref {
                file,
                from: None,
                name: name.to_string(),
                kind: "use".into(),
                qual: None,
                recv: None,
                line: 0,
                handler: false,
            };
            let targets = self.resolve_ref(&pseudo).targets;
            let target = agreed(&targets.iter().map(|(sym, _)| *sym).collect::<Vec<_>>())?;
            let sym = self.syms.get(target)?;
            let written = self.types.vars.get(&(sym.file, None))?.get(&sym.name)?;

            return self.written(sym.file, None, written, depth);
        }

        None
    }

    /// The type a binding states: a written type, or what the expression after `=` evaluates to.
    fn written(&self, file: usize, scope: Option<usize>, tys: &[String], depth: usize) -> Option<String> {
        let ty = agreed(tys)?;

        match ty.strip_prefix('=') {
            Some(expr) => self.eval(file, scope, expr, depth + 1),
            None => Some(ty),
        }
    }

    /// The result type of a call, or [`EXTERN`] when the callee is outside the repository.
    fn call_or_extern(&self, file: usize, qual: Option<&str>, name: &str) -> Option<String> {
        if let Some(ty) = self.call_type(file, qual, name) {
            return Some(ty);
        }

        self.external_call(file, qual, name).then(|| EXTERN.to_string())
    }

    /// True when a call to `name` (through `qual`) can only reach code outside the repository: an
    /// external package, the standard library, a type the repository does not define, a builtin.
    fn external_call(&self, file: usize, qual: Option<&str>, name: &str) -> bool {
        let Some(entry) = self.files.get(file) else {
            return false;
        };
        let Some(rules) = rules(entry.lang) else {
            return false;
        };
        let Some(qual) = qual else {
            return rules.is_builtin(name) && !self.by_name.contains_key(name);
        };
        let head = qual.split([':', '.']).next().unwrap_or(qual);

        if let Some(import) = self.imports.get(&(file, head.to_string())) {
            return rules.external(self, file, import);
        }

        let last = qual.rsplit([':', '.']).next().unwrap_or(qual);

        if last.starts_with(|c: char| c.is_uppercase()) {
            return !self.defines_type(entry.lang, last);
        }

        entry.lang == "rust"
            && self
                .rust_path(file, &qual.split("::").collect::<Vec<_>>(), &[])
                .is_none()
    }

    /// The type of the value a call to `name` returns, resolving the callee like any other call.
    fn call_type(&self, file: usize, qual: Option<&str>, name: &str) -> Option<String> {
        let pseudo = Ref {
            file,
            from: None,
            name: name.to_string(),
            kind: "call".into(),
            qual: qual.map(str::to_string),
            recv: None,
            line: 0,
            handler: false,
        };
        let targets = self.resolve_ref(&pseudo).targets;
        let tys: Vec<Option<String>> = targets.iter().map(|(sym, _)| self.result_of(*sym)).collect();

        agreed(&tys)?
    }

    /// What calling `sym` yields: the class it constructs, or its written return type.
    fn result_of(&self, sym: usize) -> Option<String> {
        let def = self.syms.get(sym)?;

        if TYPE_KINDS.contains(&def.kind.as_str()) {
            return Some(def.name.clone());
        }

        let ret = def.ret.as_deref()?;

        match (ret, def.parent.as_deref()) {
            ("Self" | "this", Some(owner)) => Some(owner.to_string()),
            _ => Some(ret.to_string()),
        }
    }

    /// The return type of the method `name` of type `ty`.
    fn method_type(&self, lang: &str, ty: &str, name: &str) -> Option<String> {
        let methods = self.by_owner.get(&(name.to_string(), ty.to_string()))?;
        let tys: Vec<Option<String>> = methods
            .iter()
            .copied()
            .filter(|s| {
                self.syms
                    .get(*s)
                    .is_some_and(|sym| matches!(sym.kind.as_str(), "method" | "member" | "fn"))
                    && self.lang_of_sym(*s) == lang
            })
            .take(MAX_SETTLED + 1)
            .map(|s| self.result_of(s))
            .collect();

        agreed(&tys)?
    }

    /// The written type of the field `field` of the type `ty`.
    fn field_type(&self, lang: &'static str, ty: &str, field: &str, depth: usize) -> Option<String> {
        let found = self.types.fields.get(&(lang, ty.to_string(), field.to_string()))?;
        let first = found.first()?;
        let tys: Vec<String> = found.iter().map(|f| f.ty.clone()).collect();

        self.written(first.file, first.scope, &tys, depth)
    }
}
