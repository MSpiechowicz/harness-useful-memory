//! Calls on a base class, interface or trait method also reach the overrides.
//!
//! `transport.handle_request(request)` with `transport: BaseTransport` calls whichever subclass the object is.
//! Resolution stops at the written type, so the chain of a path would end at the base method and never reach
//! `HTTPTransport.handle_request`. The extractors keep `X extends/implements/derives from Y` as `base`
//! references (Python base classes, TS `extends` and `implements`, Rust `impl Trait for Type`), and this
//! module follows them downward: the overrides of a method are the methods of the same name in the types
//! below its owner. Go interfaces are satisfied without being named, so Go has none.
//!
//! These links are weak. They are no edge of the graph (no caller list, rank or impact reads them): only
//! `code_path` follows them, after the strong ones, and says so. Both the types below a base and the
//! overrides of a method are capped, and a search works out the overrides of a method once, so a hostile
//! repository stays linear.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};

use super::Graph;

/// Types that derive directly from one base, and links kept in all.
const MAX_DERIVED_PER_BASE: usize = 64;
const MAX_LINKS: usize = 200_000;

/// Types below a base that are visited for one method.
const MAX_SUBTYPES: usize = 64;

/// Overrides one method keeps.
const MAX_OVERRIDES: usize = 8;

/// What each type is a base of, per language.
#[derive(Default)]
pub(super) struct Bases {
    derived: HashMap<(&'static str, String), Vec<String>>,
    links: usize,
}

impl Bases {
    /// Notes that `derived` extends or implements `base`, within the caps.
    pub(super) fn keep(&mut self, lang: &'static str, base: String, derived: String) {
        if self.links >= MAX_LINKS {
            return;
        }

        let list = self.derived.entry((lang, base)).or_default();

        if list.len() < MAX_DERIVED_PER_BASE && !list.contains(&derived) {
            list.push(derived);
            self.links += 1;
        }
    }

    /// About how many bytes the links take.
    pub(super) fn estimated_bytes(&self) -> usize {
        self.derived
            .iter()
            .map(|((_, base), list)| 64 + base.len() + list.iter().map(|d| 32 + d.len()).sum::<usize>())
            .sum()
    }
}

/// Overrides read from the list of one type (a class redefined in thousands of files has thousands of them).
const MAX_PER_TYPE: usize = 16;

/// The overrides of the methods one query has asked for. A search asks for the same method once per call that
/// reaches it, which can be thousands of times.
#[derive(Default)]
pub(super) struct Overrides {
    known: RefCell<HashMap<usize, Vec<usize>>>,
}

impl Overrides {
    /// [`Graph::overrides`] of `method`, worked out once.
    pub(super) fn of(&self, graph: &Graph, method: usize) -> Vec<usize> {
        if let Some(found) = self.known.borrow().get(&method) {
            return found.clone();
        }

        let found = graph.overrides(method);

        self.known.borrow_mut().insert(method, found.clone());

        found
    }
}

impl Graph {
    /// The methods that override `method` in the types below its owner, production code first, at most
    /// [`MAX_OVERRIDES`]. The work is bounded: the types visited, the methods read per type and the production
    /// overrides looked for are capped, and the walk ends once enough are found.
    pub(super) fn overrides(&self, method: usize) -> Vec<usize> {
        let Some(def) = self.syms.get(method) else {
            return Vec::new();
        };
        let Some(owner) = def.parent.as_deref() else {
            return Vec::new();
        };

        if !matches!(def.kind.as_str(), "method" | "member" | "fn") {
            return Vec::new();
        }

        let Some(lang) = super::lang_name(self.lang_of_sym(method)) else {
            return Vec::new();
        };
        let mut seen: HashSet<&str> = HashSet::from([owner]);
        let mut queue: VecDeque<&str> = VecDeque::from([owner]);
        let mut found: Vec<usize> = Vec::new();
        let mut production = 0_usize;

        'walk: while let Some(base) = queue.pop_front() {
            let below = self
                .bases
                .derived
                .get(&(lang, base.to_string()))
                .map_or(&[][..], Vec::as_slice);

            for ty in below {
                if seen.len() > MAX_SUBTYPES || !seen.insert(ty.as_str()) {
                    continue;
                }

                queue.push_back(ty.as_str());

                let defined = self
                    .by_owner
                    .get(&(def.name.clone(), ty.clone()))
                    .map_or(&[][..], Vec::as_slice);

                for &over in defined.iter().take(MAX_PER_TYPE) {
                    if over == method || self.lang_of_sym(over) != lang {
                        continue;
                    }

                    found.push(over);
                    production += usize::from(self.syms.get(over).is_some_and(|s| !s.test));

                    if production >= MAX_OVERRIDES {
                        break 'walk;
                    }
                }
            }
        }

        found.sort_by_key(|sym| (self.syms.get(*sym).is_some_and(|s| s.test), *sym));
        found.dedup();
        found.truncate(MAX_OVERRIDES);

        found
    }
}
