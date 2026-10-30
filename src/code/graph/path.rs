//! `code_path`: the shortest chain of uses from one symbol to another, one hop per line.
//!
//! The chain follows resolved edges of weight 0.5 or more (an ambiguous name match is not followed): a call,
//! a render, a function passed as a value, or a string that names a function (marked `(by name)`). A
//! constant or enum member shared by a few definitions is a bridge: the definition that mentions it and
//! the one that handles it (a `case` label or an equality test on it, see `Ref::handler`) are linked through
//! it, which is how an event reaches its consumer. A constant passed as an argument is no handler. Type
//! mentions are no flow and are not followed. Chains through production code win; test code is used only
//! when no production chain exists. When nothing leads from the first symbol to the second, the other
//! direction is tried.
//!
//! A call on a base class, interface or trait method also links to the overrides below it (see `dispatch`). Those
//! links are weak: a chain through one is looked for only when no chain of strong links exists, and says
//! `via subclass/impl`.
//!
//! An end made of plain words is a guess. It is used only when one definition holds every word that counts and
//! clearly beats the others (see `words`), and the answer then says the ends were guessed. When words fit several
//! definitions, or none holds them all, the answer lists the candidates to name instead of a chain. When there is no chain, the answer
//! says how far the search got from the first end and which calls it could not follow there.
//!
//! An end may also be a file of the repository (all its definitions, and for a start the code at the top of
//! the file) or a string that names no symbol (see `endpoint`). Each hop comes with the line of source that
//! makes the link (see `site`), so the caller does not have to open the file to see the call.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write;

use super::dispatch::Overrides;
use super::endpoint::{Endpoint, Reading};
use super::{Graph, shown};

/// A shared constant with more users than this links too many unrelated definitions to be a bridge.
const MAX_BRIDGE_USERS: usize = 12;

/// Definitions of each end that are tried.
const MAX_ENDS: usize = 4;

/// Unresolved calls named at the point where a search stopped.
const MAX_STUCK: usize = 6;

/// An edge weaker than this is not followed.
const MIN_WEIGHT: f32 = 0.5;

/// The start of a chain that begins with the code at the top of a file, which is no symbol.
const MODULE: usize = usize::MAX;

/// One step of a chain: the symbol reached and the reference that led there (`None` for the start). The first
/// step of a chain from the top of a file has the reference of that code.
struct Step {
    sym: usize,
    via: Option<usize>,
    /// Reached through an override of what the reference calls (`via subclass/impl`).
    weak: bool,
}

/// How a symbol was reached: from which symbol, through which reference, and whether through an override.
type Seen = HashMap<usize, (usize, Option<usize>, bool)>;

/// What a symbol leads to: the symbol, the reference that makes the link and whether it is an override.
type Next = (usize, usize, bool);

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

        // words that fit no one definition are not guessed
        if start.unsure.is_some() || goal.unsure.is_some() {
            return unsure_answer(&[("from", &shown_from, &start), ("to", &shown_to, &goal)]);
        }

        let (start, goal) = (settled(start), settled(goal));
        let head = format!("`{shown_from}` → `{shown_to}`");
        let notes = notes(&start, &goal);
        let overrides = Overrides::default();

        // the same name twice is the same symbol: no hop leads there
        if from == to
            && start.reading == Reading::Symbol
            && let Some(&same) = start.syms.first()
        {
            return self.render(
                &head,
                &notes,
                &[Step {
                    sym: same,
                    via: None,
                    weak: false,
                }],
            );
        }

        if let Some(chain) = self.nearest(&start, &goal, max, &overrides) {
            return self.render(&head, &notes, &chain);
        }

        if let Some(chain) = self.nearest(&goal, &start, max, &overrides) {
            let head = format!("no chain from `{shown_from}` to `{shown_to}`, reverse: `{shown_to}` → `{shown_from}`");

            return self.render(&head, &notes, &chain);
        }

        // both names reach one definition (`f` and `m::f`): it is its own chain
        if let Some(&same) = start.syms.iter().find(|sym| goal.syms.contains(sym)) {
            return self.render(
                &head,
                &notes,
                &[Step {
                    sym: same,
                    via: None,
                    weak: false,
                }],
            );
        }

        let reached = self.progress(&start, &goal, max, &overrides);

        format!(
            "no chain within {max} hops between `{shown_from}` and `{shown_to}` (tried both directions)\n{notes}{reached}"
        )
    }

    /// The shortest chain from `start` to any definition of `goal`: through production code if there is one,
    /// and through links of calls before links of overrides.
    fn nearest(&self, start: &Endpoint, goal: &Endpoint, max: usize, overrides: &Overrides) -> Option<Vec<Step>> {
        let ways = [(false, false), (false, true), (true, false), (true, true)];

        ways.into_iter()
            .find_map(|(tests, weak)| self.search(start, goal, max, (tests, weak), overrides))
    }

    /// One search, over production code or also tests, over strong links or also overrides.
    fn search(
        &self,
        start: &Endpoint,
        goal: &Endpoint,
        max: usize,
        (tests, weak): (bool, bool),
        overrides: &Overrides,
    ) -> Option<Vec<Step>> {
        let goals: HashSet<usize> = goal.syms.iter().copied().collect();
        let (mut seen, mut queue) = self.seeded(start, tests);

        while let Some((sym, hops)) = queue.pop_front() {
            if goals.contains(&sym) && hops > 0 {
                return Some(Self::unwind(sym, &seen));
            }

            if hops >= max {
                continue;
            }

            for (next, via, over) in self.steps(sym, tests, weak, overrides) {
                if seen.contains_key(&next) {
                    continue;
                }

                seen.insert(next, (sym, Some(via), over));
                queue.push_back((next, hops + 1));
            }
        }

        None
    }

    /// The symbols a search from `start` begins with, and the code at the top of a file leads to what it uses
    /// in one hop.
    fn seeded(&self, start: &Endpoint, tests: bool) -> (Seen, VecDeque<(usize, usize)>) {
        let mut seen: Seen = HashMap::new();
        let mut queue: VecDeque<(usize, usize)> = VecDeque::new();

        for &sym in &start.syms {
            seen.insert(sym, (sym, None, false));
            queue.push_back((sym, 0));
        }

        for &ri in &start.module_refs {
            if let Some(r) = self.refs.get(ri)
                && r.kind != "type"
            {
                for &(target, weight) in self.res.get(ri).map(Vec::as_slice).unwrap_or(&[]) {
                    let allowed = tests || self.syms.get(target).is_some_and(|def| !def.test);

                    if weight >= MIN_WEIGHT && allowed && !seen.contains_key(&target) {
                        seen.insert(target, (MODULE, Some(ri), false));
                        queue.push_back((target, 1));
                    }
                }
            }
        }

        (seen, queue)
    }

    /// Where a search from `start` got to when no chain reaches `goal`: the definition it reached that is
    /// likeliest the way on (one that calls a name of the goal and was not followed, else the furthest one with
    /// calls that were not followed), the way there, and those calls.
    fn progress(&self, start: &Endpoint, goal: &Endpoint, max: usize, overrides: &Overrides) -> String {
        let (mut seen, mut queue) = self.seeded(start, false);
        let mut reached: Vec<(usize, usize)> = queue.iter().copied().collect();
        let wanted: HashSet<&str> = goal
            .syms
            .iter()
            .filter_map(|sym| self.syms.get(*sym))
            .map(|def| def.name.as_str())
            .collect();

        while let Some((sym, hops)) = queue.pop_front() {
            if hops >= max {
                continue;
            }

            for (next, via, over) in self.steps(sym, false, true, overrides) {
                if let Entry::Vacant(slot) = seen.entry(next) {
                    slot.insert((sym, Some(via), over));
                    queue.push_back((next, hops + 1));
                    reached.push((next, hops + 1));
                }
            }
        }

        // the point to report: calls to the goal's names first, then any calls not followed, then the furthest
        let best = reached
            .iter()
            .map(|&(sym, hops)| {
                let stuck = self.unfollowed(sym);
                let toward = stuck.iter().any(|(name, _)| wanted.contains(name.as_str()));

                (toward, !stuck.is_empty(), hops, std::cmp::Reverse(sym), stuck)
            })
            .max_by(|a, b| (a.0, a.1, a.2, &a.3).cmp(&(b.0, b.1, b.2, &b.3)));
        let Some((_, _, hops, std::cmp::Reverse(at), stuck)) = best else {
            return String::new();
        };
        let chain = Self::unwind(at, &seen);
        let labels: Vec<String> = chain.iter().map(|step| self.label(step.sym)).collect();
        let way = match labels.as_slice() {
            [first, _, _, _, .., last] => format!("{first} → … → {last}"),
            _ => labels.join(" → "),
        };
        let unit = if hops == 1 { "hop" } else { "hops" };
        let mut out = if hops == 0 {
            format!("nothing leads out of {way} {}\n", self.loc(at))
        } else {
            format!(
                "got as far as {way} ({hops} {unit}, {} reached) {}\n",
                reached.len(),
                self.loc(at)
            )
        };

        if hops >= max {
            out.push_str("  stopped at max_depth: raise it to go on\n");
        }

        if !stuck.is_empty() {
            let names: Vec<String> = stuck
                .iter()
                .take(MAX_STUCK)
                .map(|(name, defs)| format!("{} ({defs} defs)", shown(name)))
                .collect();
            let more = stuck.len().saturating_sub(MAX_STUCK);
            let tail = if more > 0 {
                format!(", … {more} more")
            } else {
                String::new()
            };

            let _ = writeln!(
                out,
                "  not followed from {}: {}{tail}",
                self.label(at),
                names.join(", ")
            );
        }

        out
    }

    /// The calls made in `sym` that no strong link follows although the repository defines the name: the name
    /// and how many definitions it has. Calls of code outside the repository are not among them.
    fn unfollowed(&self, sym: usize) -> Vec<(String, usize)> {
        let mut names: Vec<(String, usize)> = Vec::new();
        let mut listed: HashSet<&str> = HashSet::new();

        for &ri in self.outgoing.get(sym).map(Vec::as_slice).unwrap_or(&[]) {
            let Some(r) = self.refs.get(ri) else {
                continue;
            };

            if !matches!(r.kind.as_str(), "call" | "mcall" | "new" | "component") {
                continue;
            }

            let followed = self
                .res
                .get(ri)
                .is_some_and(|targets| targets.iter().any(|(_, weight)| *weight >= MIN_WEIGHT));
            let defs = self.by_name.get(&r.name).map_or(0, Vec::len);

            if !followed && defs > 0 && listed.insert(r.name.as_str()) {
                names.push((r.name.clone(), defs));
            }
        }

        names
    }

    /// The chain that led to `end`.
    fn unwind(end: usize, seen: &Seen) -> Vec<Step> {
        let mut chain = Vec::new();
        let mut at = end;

        while let Some(&(before, via, weak)) = seen.get(&at) {
            chain.push(Step { sym: at, via, weak });

            if via.is_none() {
                break;
            }

            at = before;
        }

        chain.reverse();

        chain
    }

    /// What `sym` leads to: the definitions it uses with enough certainty and, for a shared constant, the
    /// other definitions that handle it (a `case` label or an equality test on it, not a mere mention). With `weak`, also the overrides of the methods it calls. Each comes
    /// with the reference that makes the link, strong links first.
    fn steps(&self, sym: usize, tests: bool, weak: bool, overrides: &Overrides) -> Vec<Next> {
        let mut steps: Vec<Next> = Vec::new();
        let mut widened: HashSet<usize> = HashSet::new();
        let allowed = |s: usize| tests || self.syms.get(s).is_some_and(|def| !def.test);

        for &ri in self.outgoing.get(sym).map(Vec::as_slice).unwrap_or(&[]) {
            let Some(r) = self.refs.get(ri) else {
                continue;
            };

            if r.kind == "type" {
                continue;
            }

            for &(target, weight) in self.res.get(ri).map(Vec::as_slice).unwrap_or(&[]) {
                if weight < MIN_WEIGHT {
                    continue;
                }

                if target != sym && allowed(target) {
                    steps.push((target, ri, false));
                }

                // a method called a thousand times in `sym` has its overrides looked up once
                if weak && widened.insert(target) {
                    for over in overrides.of(self, target) {
                        if over != sym && allowed(over) {
                            steps.push((over, ri, true));
                        }
                    }
                }
            }
        }

        if self.is_shared_constant(sym) {
            for &ri in self.incoming.get(sym).map(Vec::as_slice).unwrap_or(&[]) {
                let Some(r) = self.refs.get(ri).filter(|r| r.handler) else {
                    continue;
                };
                let Some(user) = r.from else {
                    continue;
                };

                if user != sym && allowed(user) && self.weight(ri, sym) >= MIN_WEIGHT {
                    steps.push((user, ri, false));
                }
            }
        }

        // a direct use is better evidence than a name in a string, and a call than an override of it
        steps.sort_by_key(|(target, ri, over)| {
            (*over, self.refs.get(*ri).is_some_and(|r| r.kind == "str"), *target, *ri)
        });
        steps.dedup_by_key(|(target, _, _)| *target);

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

    /// ` (via subclass/impl of Base::method)` for a hop that is an override of what the reference calls.
    fn override_tag(&self, step: &Step) -> String {
        let base = step
            .via
            .and_then(|ri| self.res.get(ri))
            .and_then(|targets| {
                targets
                    .iter()
                    .find(|(target, _)| self.overrides(*target).contains(&step.sym))
            })
            .map(|(target, _)| self.label(*target));

        match base {
            Some(base) => format!(" (via subclass/impl of {base})"),
            None => " (via subclass/impl)".to_owned(),
        }
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
            let tag = if step.weak {
                self.override_tag(step)
            } else if by_name {
                " (by name)".to_owned()
            } else {
                String::new()
            };
            let arrow = if step.via.is_none() && i == 0 { "  " } else { "→ " };

            let _ = writeln!(out, "  {arrow}{}{tag}  {}", self.label(step.sym), self.loc(step.sym));

            if let Some(ri) = step.via {
                let _ = writeln!(out, "        at {}", self.site_with_text(ri));
            }
        }

        out
    }
}

/// The end as `code_path` uses it. Words that one definition alone holds are that definition.
fn settled(mut end: Endpoint) -> Endpoint {
    if end.reading == Reading::Words {
        end.syms.truncate(end.sure.max(1));

        if let Some(note) = end.sure_note.take() {
            end.note = Some(note);
        }
    }

    end
}

/// The reply to words that fit no one definition: the candidates of each end that is unsure, to name in a
/// next call.
fn unsure_answer(ends: &[(&str, &str, &Endpoint)]) -> String {
    let mut out = String::from("the words do not name one definition, so no chain is guessed\n");

    for (side, text, end) in ends {
        let Some(unsure) = &end.unsure else {
            continue;
        };

        let _ = writeln!(out, "{side} `{text}`: {}. Closest:", unsure.why);

        for line in &unsure.lines {
            let _ = writeln!(out, "  {line}");
        }
    }

    out.push_str("Call code_path again with one of these names (or Type::name) as the end.\n");

    out
}

/// The lines under the head that say what each end was taken for, when it is not a plain name.
fn notes(start: &Endpoint, goal: &Endpoint) -> String {
    let mut out = String::new();

    for (side, end) in [("from", start), ("to", goal)] {
        if let Some(note) = &end.note {
            out.push_str(&format!("{side}: {note}\n"));
        }
    }

    if start.reading == Reading::Words || goal.reading == Reading::Words {
        out.push_str("endpoints guessed from words: check the first and last hop\n");
    }

    out
}
