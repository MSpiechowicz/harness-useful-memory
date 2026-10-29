//! `code_path`: the shortest chain of uses from one symbol to another, one hop per line.
//!
//! The chain follows resolved edges of weight 0.5 or more (an ambiguous name match is not followed): a call,
//! a render, a function passed as a value, or a string that names a function (marked `(by name)`). A
//! constant or enum member shared by a few definitions is a bridge: the definition that mentions it and
//! the one that handles it are linked through it, which is how an event reaches its consumer. Type
//! mentions are no flow and are not followed. Chains through production code win; test code is used only
//! when no production chain exists. When nothing leads from the first symbol to the second, the other
//! direction is tried.
//!
//! An end may also be a file of the repository (all its definitions, and for a start the code at the top of
//! the file) or a string that names no symbol (see `endpoint`). Each hop comes with the line of source that
//! makes the link (see `site`), so the caller does not have to open the file to see the call.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write;

use super::endpoint::{Endpoint, Reading};
use super::{Graph, shown};

/// A shared constant with more users than this links too many unrelated definitions to be a bridge.
const MAX_BRIDGE_USERS: usize = 12;

/// Definitions of each end that are tried.
const MAX_ENDS: usize = 4;

/// The hops more that a better ranked start of words may need than the nearest one.
const WORDS_SLACK: usize = 1;

/// An edge weaker than this is not followed.
const MIN_WEIGHT: f32 = 0.5;

/// The start of a chain that begins with the code at the top of a file, which is no symbol.
const MODULE: usize = usize::MAX;

/// One step of a chain: the symbol reached and the reference that led there (`None` for the start). The first
/// step of a chain from the top of a file has the reference of that code.
struct Step {
    sym: usize,
    via: Option<usize>,
}

/// How a symbol was reached: from which symbol, through which reference.
type Seen = HashMap<usize, (usize, Option<usize>)>;

impl Graph {
    /// The shortest chain of at most `max_depth` hops from `from` to `to`, or from `to` to `from` when
    /// there is none that way, or a message saying there is no chain.
    pub fn path(&self, from: &str, to: &str, max_depth: usize) -> String {
        let max = max_depth.clamp(1, 10);
        let (from, to) = (from.trim(), to.trim());
        let start = self.endpoint(from, MAX_ENDS);
        let goal = self.endpoint(to, MAX_ENDS);

        if start.is_empty() {
            return self.suggest(from);
        }

        if goal.syms.is_empty() {
            return self.suggest(to);
        }

        // the names are the caller's text: written out like any text of the repository
        let (shown_from, shown_to) = (shown(from), shown(to));
        let head = format!("`{shown_from}` → `{shown_to}`");
        let notes = notes(&start, &goal);

        // the same name twice is the same symbol: no hop leads there
        if from == to
            && start.reading == Reading::Symbol
            && let Some(&same) = start.syms.first()
        {
            return self.render(&head, &notes, &[Step { sym: same, via: None }]);
        }

        if let Some(chain) = self.shortest(&start, &goal, max) {
            return self.render(&head, &notes, &chain);
        }

        if let Some(chain) = self.shortest(&goal, &start, max) {
            let head = format!("no chain from `{shown_from}` to `{shown_to}`, reverse: `{shown_to}` → `{shown_from}`");

            return self.render(&head, &notes, &chain);
        }

        // both names reach one definition (`f` and `m::f`): it is its own chain
        if let Some(&same) = start.syms.iter().find(|sym| goal.syms.contains(sym)) {
            return self.render(&head, &notes, &[Step { sym: same, via: None }]);
        }

        format!("no chain within {max} hops between `{shown_from}` and `{shown_to}` (tried both directions)\n{notes}")
    }

    /// The shortest chain from `start` to any definition of `goal`: through production code if there is one.
    ///
    /// The definitions that words stand for are ranked, and the best one a chain leads from is the better start
    /// even when another is a hop or two nearer (the command a question names, not the helper it calls).
    fn shortest(&self, start: &Endpoint, goal: &Endpoint, max: usize) -> Option<Vec<Step>> {
        if start.reading != Reading::Words {
            return self.nearest(start, goal, max);
        }

        // one search per start, best ranked first. A definition that stands for both ends has the others to reach
        let mut chains: Vec<Vec<Step>> = Vec::new();

        for &sym in &start.syms {
            let rest: Vec<usize> = goal.syms.iter().copied().filter(|end| *end != sym).collect();

            if rest.is_empty() {
                continue;
            }

            if let Some(chain) = self.nearest(&words_end(vec![sym]), &words_end(rest), max) {
                chains.push(chain);
            }
        }

        let hops = |chain: &Vec<Step>| chain.iter().filter(|step| step.via.is_some()).count();
        let nearest = chains.iter().map(hops).min()?;

        chains.into_iter().find(|chain| hops(chain) <= nearest + WORDS_SLACK)
    }

    fn nearest(&self, start: &Endpoint, goal: &Endpoint, max: usize) -> Option<Vec<Step>> {
        self.search(start, goal, max, false)
            .or_else(|| self.search(start, goal, max, true))
    }

    fn search(&self, start: &Endpoint, goal: &Endpoint, max: usize, tests: bool) -> Option<Vec<Step>> {
        let goals: HashSet<usize> = goal.syms.iter().copied().collect();
        let mut seen: Seen = HashMap::new();
        let mut queue: VecDeque<(usize, usize)> = VecDeque::new();

        for &sym in &start.syms {
            seen.insert(sym, (sym, None));
            queue.push_back((sym, 0));
        }

        // the code at the top of a file leads to what it uses in one hop
        for &ri in &start.module_refs {
            if let Some(r) = self.refs.get(ri)
                && r.kind != "type"
            {
                for &(target, weight) in self.res.get(ri).map(Vec::as_slice).unwrap_or(&[]) {
                    let allowed = tests || self.syms.get(target).is_some_and(|def| !def.test);

                    if weight >= MIN_WEIGHT && allowed && !seen.contains_key(&target) {
                        seen.insert(target, (MODULE, Some(ri)));
                        queue.push_back((target, 1));
                    }
                }
            }
        }

        while let Some((sym, hops)) = queue.pop_front() {
            if goals.contains(&sym) && hops > 0 {
                return Some(Self::unwind(sym, &seen));
            }

            if hops >= max {
                continue;
            }

            for (next, via) in self.steps(sym, tests) {
                if seen.contains_key(&next) {
                    continue;
                }

                seen.insert(next, (sym, Some(via)));
                queue.push_back((next, hops + 1));
            }
        }

        None
    }

    /// The chain that led to `end`.
    fn unwind(end: usize, seen: &Seen) -> Vec<Step> {
        let mut chain = Vec::new();
        let mut at = end;

        while let Some(&(before, via)) = seen.get(&at) {
            chain.push(Step { sym: at, via });

            if via.is_none() {
                break;
            }

            at = before;
        }

        chain.reverse();

        chain
    }

    /// What `sym` leads to: the definitions it uses with enough certainty and, for a shared constant, the
    /// other definitions that mention it. Each comes with the reference that makes the link.
    fn steps(&self, sym: usize, tests: bool) -> Vec<(usize, usize)> {
        let mut steps: Vec<(usize, usize)> = Vec::new();
        let allowed = |s: usize| tests || self.syms.get(s).is_some_and(|def| !def.test);

        for &ri in self.outgoing.get(sym).map(Vec::as_slice).unwrap_or(&[]) {
            let Some(r) = self.refs.get(ri) else {
                continue;
            };

            if r.kind == "type" {
                continue;
            }

            for &(target, weight) in self.res.get(ri).map(Vec::as_slice).unwrap_or(&[]) {
                if weight >= MIN_WEIGHT && target != sym && allowed(target) {
                    steps.push((target, ri));
                }
            }
        }

        if self.is_shared_constant(sym) {
            for &ri in self.incoming.get(sym).map(Vec::as_slice).unwrap_or(&[]) {
                let Some(user) = self.refs.get(ri).and_then(|r| r.from) else {
                    continue;
                };

                if user != sym && allowed(user) && self.weight(ri, sym) >= MIN_WEIGHT {
                    steps.push((user, ri));
                }
            }
        }

        // a direct use is better evidence than a name in a string
        steps.sort_by_key(|(target, ri)| (self.refs.get(*ri).is_some_and(|r| r.kind == "str"), *target, *ri));
        steps.dedup_by_key(|(target, _)| *target);

        steps
    }

    /// A constant or enum member that a few definitions mention.
    fn is_shared_constant(&self, sym: usize) -> bool {
        let Some(def) = self.syms.get(sym) else {
            return false;
        };

        if !matches!(def.kind.as_str(), "const" | "static" | "enum_member") {
            return false;
        }

        let users: HashSet<usize> = self
            .incoming
            .get(sym)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .filter_map(|ri| self.refs.get(*ri).and_then(|r| r.from))
            .collect();

        (2..=MAX_BRIDGE_USERS).contains(&users.len())
    }

    /// `head: N hops`, the notes on the ends, then one line per symbol on the chain with where it is defined and
    /// under each hop the line of source that makes it.
    fn render(&self, head: &str, notes: &str, chain: &[Step]) -> String {
        let hops = chain.iter().filter(|step| step.via.is_some()).count();
        let unit = if hops == 1 { "hop" } else { "hops" };
        let mut out = format!("{head}: {hops} {unit}\n{notes}");

        // a chain that begins with the code at the top of a file
        if let Some(first) = chain.first().and_then(|step| step.via).and_then(|ri| self.refs.get(ri)) {
            let file = self.files.get(first.file).map_or("", |file| file.path.as_str());

            let _ = writeln!(out, "    <module>  {}", shown(file));
        }

        for (i, step) in chain.iter().enumerate() {
            let by_name = step
                .via
                .and_then(|ri| self.refs.get(ri))
                .is_some_and(|r| r.kind == "str");
            let tag = if by_name { " (by name)" } else { "" };
            let arrow = if step.via.is_none() && i == 0 { "  " } else { "→ " };

            let _ = writeln!(out, "  {arrow}{}{tag}  {}", self.label(step.sym), self.loc(step.sym));

            if let Some(ri) = step.via {
                let _ = writeln!(out, "        at {}", self.site_with_text(ri));
            }
        }

        out
    }
}

/// An end made of the definitions `syms` that words stood for.
fn words_end(syms: Vec<usize>) -> Endpoint {
    Endpoint {
        reading: Reading::Words,
        syms,
        module_refs: Vec::new(),
        note: None,
    }
}

/// The lines under the head that say what each end was taken for, when it is not a plain name.
fn notes(start: &Endpoint, goal: &Endpoint) -> String {
    let mut out = String::new();

    for (side, end) in [("from", start), ("to", goal)] {
        if let Some(note) = &end.note {
            out.push_str(&format!("{side}: {note}\n"));
        }
    }

    out
}
