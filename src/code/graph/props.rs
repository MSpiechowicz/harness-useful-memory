//! Functions passed through objects: `createRunner({ run: runIt })` and later `host.run(..)`.
//!
//! An object literal property whose value is a function (`key: fn`, `{ fn }`, `key: a ?? fn`, `key: fn.bind(x)`,
//! `key: ns.fn`) is stored as a `prop` reference. It is no edge of the graph. A method call that nothing else
//! settles (`host.key(..)`, `host?.key(..)`, `this.host.key(..)`) may mean the functions bound to `key`: a weak
//! link (`?`) that `code_path` still follows, see [`Graph::resolve_prop`].

use std::collections::HashMap;

use super::super::pace::Pace;
use super::{Graph, Ref, Resolved, poll};

/// A method call whose name is a property that object literals fill with functions (`{ run: runIt }`) means
/// them only when at most this many different functions are put there.
const MAX_PROP_TARGETS: usize = 4;

/// Bindings of one property name that are kept: past it the name is too common to mean anything.
const MAX_PROP_BINDINGS: usize = 64;

/// A property name that more method calls than this carry is too common (`get`, `run`, `map`) for its name alone
/// to say which function a call means: only a literal passed to code around the call settles it then.
const MAX_PROP_CALLS: usize = 12;

/// The weight of a call through a property: when the object literal that holds the function is passed to a
/// function that contains the call, and when only the property name tells. Both are below 1 (the answer marks
/// them `?`) and at least what `code_path` follows.
const PROP_DIRECT_WEIGHT: f32 = 0.75;
const PROP_NAME_WEIGHT: f32 = 0.55;

/// What object literals put in the property of one name.
#[derive(Default)]
pub(super) struct Prop {
    pub(super) bound: Vec<Bound>,
    /// How many method calls in the repository carry the name, whatever their receiver.
    calls: usize,
}

/// A function kept in a property of an object literal (`{ key: fn }`).
pub(super) struct Bound {
    pub(super) file: usize,
    /// The functions the property may hold.
    pub(super) targets: Vec<usize>,
    /// The definitions the object literal is passed to as an argument (`create({ key: fn })`).
    pub(super) via: Vec<usize>,
}

/// A `prop` reference as stored, before its value is resolved.
pub(super) struct PropRow {
    pub(super) file: usize,
    pub(super) key: String,
    /// The value as written: `fn` or `ns.fn`.
    pub(super) value: String,
    /// The name of the function the object literal is passed to.
    pub(super) via: Option<String>,
}

impl Graph {
    /// Resolves the functions that object literals keep in properties (`{ run: runIt }`): the value as a
    /// function passed as a value (the same file or an imported one, or a member of a namespace import), and
    /// the function the literal is passed to as a call.
    pub(super) fn bind_props(&mut self, rows: Vec<PropRow>, pace: &Pace) -> Result<(), String> {
        let mut props: HashMap<String, Prop> = HashMap::new();

        for (count, row) in rows.into_iter().enumerate() {
            poll(pace, count)?;

            if props
                .get(&row.key)
                .is_some_and(|prop| prop.bound.len() >= MAX_PROP_BINDINGS)
            {
                continue;
            }

            let targets = self.function_value(row.file, &row.value);

            if targets.is_empty() {
                continue;
            }

            let via = row
                .via
                .map(|name| {
                    let call = Ref {
                        file: row.file,
                        from: None,
                        name,
                        kind: "call".to_string(),
                        qual: None,
                        recv: None,
                        line: 0,
                        handler: false,
                    };

                    self.resolve_ref(&call)
                        .targets
                        .into_iter()
                        .map(|(sym, _)| sym)
                        .collect()
                })
                .unwrap_or_default();

            props.entry(row.key).or_default().bound.push(Bound {
                file: row.file,
                targets,
                via,
            });
        }

        for r in self.refs.iter().filter(|r| r.kind == "mcall") {
            if let Some(prop) = props.get_mut(&r.name) {
                prop.calls += 1;
            }
        }

        self.props = props;

        Ok(())
    }

    /// The functions a property value written in `file` (`fn`, `ns.fn`) may be.
    fn function_value(&self, file: usize, value: &str) -> Vec<usize> {
        let (qual, name) = match value.rsplit_once('.') {
            Some((qual, name)) => (Some(qual), name),
            None => (None, value),
        };
        let member = qual.filter(|qual| self.is_namespace(file, Some(qual)));
        let value = Ref {
            file,
            from: None,
            name: name.to_string(),
            kind: if member.is_some() { "mcall" } else { "fnref" }.to_string(),
            qual: member.map(str::to_string),
            recv: None,
            line: 0,
            handler: false,
        };
        let mut targets: Vec<usize> = self
            .resolve_ref(&value)
            .targets
            .into_iter()
            .map(|(sym, _)| sym)
            .collect();

        targets.sort_unstable();
        targets.dedup();

        targets
    }

    /// A method call that nothing else settles may mean a function that an object literal put in a property of
    /// that name (`createRunner({ run: runIt })`, then `host.run()`): a weak link, only when the receiver is not
    /// a module or class of the repository and at most [`MAX_PROP_TARGETS`] different functions are put there.
    /// Literals passed to a function that contains the call are preferred (and the only ones for a receiver of
    /// a type the repository defines), and test code only counts for test code.
    pub(super) fn resolve_prop(&self, r: &Ref) -> Option<Resolved> {
        let prop = self.props.get(&r.name)?;
        let file = self.files.get(r.file)?;

        if let Some(qual) = r.qual.as_deref().map(str::trim)
            && (self.imports.contains_key(&(r.file, qual.to_string())) || self.defines_type(file.lang, qual))
        {
            return None; // a module or a class: its own members are what the call means
        }

        // a receiver of a type the repository does not define (`Map`, `URLSearchParams`) has the methods of
        // that type. One the repository defines has no such method (nothing else settled the call): the
        // property is a field of it, which only an object literal passed to code around the call can fill
        let typed = match self.receiver_type(r) {
            Some(ty) if self.defines_type(file.lang, &ty) => true,
            Some(_) => return None,
            None => false,
        };

        // code of another language, and test code for production code, never means these
        let usable: Vec<&Bound> = prop
            .bound
            .iter()
            .filter(|bound| {
                self.files
                    .get(bound.file)
                    .is_some_and(|f| f.lang == file.lang && (file.test || !f.test))
            })
            .collect();
        let mut all: Vec<usize> = usable.iter().flat_map(|bound| bound.targets.iter().copied()).collect();

        all.sort_unstable();
        all.dedup();

        if all.is_empty() || all.len() > MAX_PROP_TARGETS {
            return None;
        }

        let mut direct: Vec<usize> = usable
            .iter()
            .filter(|bound| bound.via.iter().any(|via| self.contains(*via, r)))
            .flat_map(|bound| bound.targets.iter().copied())
            .collect();

        direct.sort_unstable();
        direct.dedup();

        let (targets, weight) = if !direct.is_empty() {
            (direct, PROP_DIRECT_WEIGHT)
        } else if typed || prop.calls > MAX_PROP_CALLS {
            return None;
        } else {
            (all, PROP_NAME_WEIGHT)
        };

        Some(Resolved {
            targets: targets.into_iter().map(|sym| (sym, weight)).collect(),
            soft: true,
        })
    }

    /// True when the reference `r` is written inside the definition `sym`.
    fn contains(&self, sym: usize, r: &Ref) -> bool {
        r.from == Some(sym)
            || self
                .syms
                .get(sym)
                .is_some_and(|def| def.file == r.file && def.line <= r.line && r.line <= def.end_line)
    }
}
