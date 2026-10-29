//! The text the agent reads: the ranked map, one symbol with its uses and users, and the impact of a change.
//!
//! Every list is capped, tests are summarised per file, and every impact entry reads `caller → callee`
//! (an agent invents chains otherwise).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Write;

use super::endpoint::Reading;
use super::{Graph, TYPE_KINDS, shown, tokens};

/// Signatures listed per file in the map.
const MAP_SIGNATURES: usize = 6;

/// Definitions shown for one symbol query.
const MAX_DEFINITIONS: usize = 4;

/// Uses of a definition that come with the line of source that makes them, and sites of its users that do. The
/// rest are listed by place only.
const USES_WITH_TEXT: usize = 6;
pub(super) const USERS_WITH_TEXT: usize = 10;

/// Similar names suggested when nothing matches.
const MAX_SUGGESTIONS: usize = 12;

/// The most names compared with the query for a suggestion (the shortest first, so the answer does not depend
/// on the order of a hash map). A repository can hold hundreds of thousands of names of up to 256 bytes.
const MAX_SUGGEST_CANDIDATES: usize = 50_000;

/// A name this much longer than the query (twice its length and more) is not a suggestion for it.
const SUGGEST_EXTRA_LENGTH: usize = 8;

/// Production files listed with their callers in an impact answer (the direct callers always are).
const MAX_IMPACT_FILES: usize = 40;

/// Files listed with no caller names after the detailed ones in an impact answer.
const MAX_IMPACT_BARE: usize = 60;

/// Names listed per file in an impact answer.
const MAX_IMPACT_NAMES: usize = 8;

/// Test files listed in an impact answer.
const MAX_IMPACT_TESTS: usize = 10;

/// An edge weaker than this is not followed by `impact`.
const MIN_FOLLOW_WEIGHT: f32 = 0.5;

/// Longest query used to look for similar names.
const MAX_SUGGEST_QUERY: usize = 64;

impl Graph {
    /// The most central source files with the signatures other code depends on, within `budget` tokens.
    /// `focus` is a path prefix that centres the ranking on one area (see [`Graph::focus_prefix`]). Test
    /// files are left out. When no definition is used by another file, each file lists its top definitions
    /// instead.
    pub fn map(&self, budget: usize, focus: Option<&str>) -> String {
        if self.files.is_empty() {
            return "No source files are indexed in this folder.\n".to_string();
        }

        let wanted = focus.map(str::trim).filter(|wanted| !wanted.is_empty());
        let prefix = match wanted {
            Some(wanted) => match self.focus_prefix(wanted) {
                Some(prefix) => Some(prefix),
                None => return format!("no indexed files under {}\n", shown(wanted)),
            },
            None => None,
        };
        let ranks = self.rank(prefix.as_deref());
        let linked = ranks.syms.iter().any(|rank| *rank > 0.0);
        let score = |f: usize| ranks.files.get(f).copied().unwrap_or(0.0);
        let mut order: Vec<usize> = (0..self.files.len())
            .filter(|f| self.files.get(*f).is_some_and(|file| !file.test))
            .collect();

        order.sort_by(|a, b| {
            score(*b).total_cmp(&score(*a)).then_with(|| {
                self.files
                    .get(*a)
                    .map(|f| &f.path)
                    .cmp(&self.files.get(*b).map(|f| &f.path))
            })
        });

        let mut out = String::new();

        for file in order {
            let Some(entry) = self.files.get(file) else {
                continue;
            };
            let sym_rank = |s: usize| ranks.syms.get(s).copied().unwrap_or(0.0);
            let mut symbols: Vec<usize> = self
                .file_syms
                .get(file)
                .map(|list| {
                    list.iter()
                        .copied()
                        .filter(|s| (!linked || sym_rank(*s) > 0.0) && self.syms.get(*s).is_some_and(|sym| !sym.test))
                        .collect()
                })
                .unwrap_or_default();

            if linked {
                symbols.sort_by(|a, b| sym_rank(*b).total_cmp(&sym_rank(*a)).then_with(|| a.cmp(b)));
            } else {
                symbols.sort_by_key(|s| self.definition_order(*s));
            }

            let mut block = format!("{}\n", shown(&entry.path));

            for s in symbols.iter().take(MAP_SIGNATURES) {
                let Some(sym) = self.syms.get(*s) else {
                    continue;
                };
                let owner = sym
                    .parent
                    .as_ref()
                    .map(|p| format!("{}: ", shown(p)))
                    .unwrap_or_default();

                let _ = writeln!(block, "  {owner}{}", shown(&sym.sig));
            }

            if !out.is_empty() && tokens(&out) + tokens(&block) > budget {
                break;
            }

            out += &block;
        }

        out
    }

    /// How a definition is placed among those of its file when nothing ranks them: top-level before members,
    /// types before functions before constants, then by line.
    fn definition_order(&self, sym: usize) -> (bool, u8, i64, usize) {
        let Some(def) = self.syms.get(sym) else {
            return (true, u8::MAX, 0, sym);
        };
        let rank = if TYPE_KINDS.contains(&def.kind.as_str()) {
            0
        } else if def.kind == "fn" {
            1
        } else if matches!(def.kind.as_str(), "const" | "static") {
            2
        } else {
            3
        };

        (def.parent.is_some(), rank, def.line, sym)
    }

    /// The path prefix `focus` means among the indexed files, or `None` when no file is under it. Written
    /// by an agent, it may use `\`, start with `./` or carry the project root (absolute, or by its folder
    /// name); the first reading that matches a file is the one.
    pub(super) fn focus_prefix(&self, focus: &str) -> Option<String> {
        self.readings(focus)
            .into_iter()
            .find(|prefix| self.files.iter().any(|file| file.path.starts_with(prefix.as_str())))
    }

    /// The paths a text written by an agent may mean, most literal first: with `/` for `\`, without a leading
    /// `./`, and without the project root (absolute, or by its folder name).
    pub(super) fn readings(&self, written: &str) -> Vec<String> {
        let mut written = written.replace('\\', "/");

        while let Some(rest) = written.strip_prefix("./") {
            written = rest.to_string();
        }

        let root = self.root.replace('\\', "/");
        let root = root.trim_end_matches('/');
        let folder = root.rsplit('/').next().unwrap_or("");
        let mut readings = vec![written.clone()];

        if let Some(inside) = written.strip_prefix(root)
            && !root.is_empty()
            && (inside.is_empty() || inside.starts_with('/'))
        {
            readings.push(inside.trim_start_matches('/').to_string());
        }

        if let Some(inside) = written.strip_prefix(folder)
            && !folder.is_empty()
            && (inside.is_empty() || inside.starts_with('/'))
        {
            readings.push(inside.trim_start_matches('/').to_string());
        }

        readings
    }

    /// Where `query` is defined (`name`, `Type::name`, `module::name` or `path/fragment::name`), what it
    /// uses and who uses it. At most `limit` entries per list.
    pub fn symbol(&self, query: &str, limit: usize) -> String {
        let cap = limit.clamp(1, 50);
        let query = query.trim();
        let end = self.endpoint(query, usize::MAX);
        let reading = end.reading;
        let mut defs = end.syms;

        if defs.is_empty() {
            return self.suggest(query);
        }

        // a file or a string stands for many definitions: the most used ones come first (words are ranked)
        if matches!(reading, Reading::File | Reading::Text) {
            defs.sort_by_key(|sym| {
                (
                    self.syms.get(*sym).is_some_and(|s| s.test),
                    std::cmp::Reverse(self.incoming.get(*sym).map_or(0, Vec::len)),
                    *sym,
                )
            });
        }

        let mut out = String::new();
        let qualify = format!(
            "showing {MAX_DEFINITIONS}{}",
            if matches!(reading, Reading::File | Reading::Text) {
                " most used"
            } else {
                ""
            }
        );

        match end.note {
            Some(note) if reading != Reading::Symbol => {
                let tail = if defs.len() > MAX_DEFINITIONS {
                    format!(", {qualify} (qualify as Type::name or path::name)")
                } else {
                    String::new()
                };

                let _ = writeln!(out, "{note}{tail}");
            }
            _ if defs.len() > MAX_DEFINITIONS => {
                let _ = writeln!(
                    out,
                    "{} definitions match `{}`, {qualify} (qualify as Type::name or path::name)",
                    defs.len(),
                    shown(query)
                );
            }
            _ => {}
        }

        for &def in defs.iter().take(MAX_DEFINITIONS) {
            let Some(sym) = self.syms.get(def) else {
                continue;
            };

            let _ = writeln!(
                out,
                "{} {}  {}-{}\n  {}",
                sym.kind,
                self.label(def),
                self.loc(def),
                sym.end_line,
                shown(&sym.sig)
            );

            self.write_uses(&mut out, def, cap);
            self.write_users(&mut out, def, cap);
        }

        out
    }

    /// What `def` uses: resolved, unique targets, the first ones with the line of source that uses them.
    fn write_uses(&self, out: &mut String, def: usize, cap: usize) {
        let mut seen = HashSet::new();
        let mut uses: Vec<(usize, f32, usize)> = Vec::new();

        for &ri in self.outgoing.get(def).map(Vec::as_slice).unwrap_or(&[]) {
            for (target, weight) in self.res.get(ri).map(Vec::as_slice).unwrap_or(&[]) {
                if *target != def && seen.insert(*target) {
                    uses.push((*target, *weight, ri));
                }
            }
        }

        if uses.is_empty() {
            return;
        }

        let more = uses.len().saturating_sub(cap);
        let list: Vec<(usize, f32, usize)> = uses.into_iter().take(cap).collect();
        let mark = |weight: f32| if weight < 1.0 { "?" } else { "" };

        let _ = writeln!(out, "  uses:");

        for (target, weight, ri) in list.iter().take(USES_WITH_TEXT) {
            let _ = writeln!(
                out,
                "    {}{} {}  at {}",
                self.label(*target),
                mark(*weight),
                self.loc(*target),
                self.site_with_text(*ri)
            );
        }

        if list.len() > USES_WITH_TEXT {
            let rest: Vec<String> = list
                .iter()
                .skip(USES_WITH_TEXT)
                .map(|(target, weight, _)| format!("{}{} {}", self.label(*target), mark(*weight), self.loc(*target)))
                .collect();
            let tail = if more > 0 {
                format!(", … {more} more")
            } else {
                String::new()
            };

            let _ = writeln!(out, "    also: {}{tail}", rest.join(", "));
        } else if more > 0 {
            let _ = writeln!(out, "    also: … {more} more");
        }
    }

    /// The references to `def`, one per (file, caller): the one whose resolution is the most certain, then
    /// the first line. Each comes with its weight.
    fn users(&self, def: usize) -> Vec<(usize, f32)> {
        let mut best: HashMap<(usize, Option<usize>), (usize, f32)> = HashMap::new();

        for &ri in self.incoming.get(def).map(Vec::as_slice).unwrap_or(&[]) {
            let Some(r) = self.refs.get(ri) else {
                continue;
            };
            let weight = self.weight(ri, def);
            let better = |held: &(usize, f32)| {
                let held_line = self.refs.get(held.0).map_or(i64::MAX, |h| h.line);

                weight > held.1 || (weight == held.1 && r.line < held_line)
            };
            let slot = best.entry((r.file, r.from)).or_insert((ri, weight));

            if better(slot) {
                *slot = (ri, weight);
            }
        }

        let mut users: Vec<(usize, f32)> = best.into_values().collect();

        users.sort_by_key(|(ri, _)| *ri);

        users
    }

    /// Who uses `def`: production sites listed, tests summarised per file.
    fn write_users(&self, out: &mut String, def: usize, cap: usize) {
        let mut production: Vec<(String, usize)> = Vec::new();
        let mut tests: BTreeMap<&str, usize> = BTreeMap::new();

        for (ri, weight) in self.users(def) {
            let Some(r) = self.refs.get(ri) else {
                continue;
            };
            let Some(file) = self.files.get(r.file) else {
                continue;
            };

            if file.test || r.from.and_then(|s| self.syms.get(s)).is_some_and(|sym| sym.test) {
                *tests.entry(file.path.as_str()).or_default() += 1;
            } else {
                let who = r.from.map_or_else(|| "<module>".to_string(), |s| self.label(s));
                let tag = if r.kind == "str" { " (by name)" } else { "" };
                let mark = if weight < 1.0 { " ?" } else { "" };

                production.push((format!("{}:{} {who}{tag}{mark}", shown(&file.path), r.line), ri));
            }
        }

        production.sort();

        let test_total: usize = tests.values().sum();
        let test_note = if tests.is_empty() {
            String::new()
        } else {
            format!(" + {test_total} test(s)")
        };

        let _ = writeln!(out, "  used by {} site(s){test_note}:", production.len());

        for (i, (site, ri)) in production.iter().take(cap).enumerate() {
            // the line is read only for the sites that show it
            match (i < USERS_WITH_TEXT).then(|| self.site_text(*ri)).flatten() {
                Some(text) => {
                    let _ = writeln!(out, "    {site} | {text}");
                }
                None => {
                    let _ = writeln!(out, "    {site}");
                }
            }
        }

        if production.len() > cap {
            let _ = writeln!(out, "    … {} more", production.len() - cap);
        }

        if !tests.is_empty() {
            let list: Vec<String> = tests
                .iter()
                .take(6)
                .map(|(path, count)| format!("{} ({count})", shown(path)))
                .collect();
            let more = if tests.len() > 6 {
                format!(", … {} more files", tests.len() - 6)
            } else {
                String::new()
            };

            let _ = writeln!(out, "    tests: {}{more}", list.join(", "));
        }
    }

    /// Everything that transitively uses `query`, up to `depth` caller hops; ambiguous edges are not
    /// followed. Entries read `caller → what it calls`.
    pub fn impact(&self, query: &str, depth: usize) -> String {
        let depth = depth.clamp(1, 4);
        let query = query.trim();
        let end = self.endpoint(query, usize::MAX);
        let (reading, note) = (end.reading, end.note);
        let start = end.syms;

        if start.is_empty() {
            return self.suggest(query);
        }

        let mut dist: HashMap<usize, usize> = start.iter().map(|s| (*s, 0)).collect();
        // how each caller was reached: (callee, through a string name)
        let mut via: HashMap<usize, (usize, bool)> = HashMap::new();
        // (file, depth, callee) for references made outside any definition
        let mut module_users: Vec<(usize, usize, usize)> = Vec::new();
        let mut skipped = 0_usize;
        let mut queue: VecDeque<usize> = start.iter().copied().collect();

        while let Some(sym) = queue.pop_front() {
            let here = dist.get(&sym).copied().unwrap_or(0);

            if here >= depth {
                continue;
            }

            for &ri in self.incoming.get(sym).map(Vec::as_slice).unwrap_or(&[]) {
                if self.weight(ri, sym) < MIN_FOLLOW_WEIGHT {
                    skipped += 1;
                    continue;
                }

                let Some(r) = self.refs.get(ri) else {
                    continue;
                };

                match r.from {
                    Some(caller) => {
                        if let std::collections::hash_map::Entry::Vacant(slot) = dist.entry(caller) {
                            slot.insert(here + 1);
                            via.insert(caller, (sym, r.kind == "str"));
                            queue.push_back(caller);
                        }
                    }
                    None => module_users.push((r.file, here + 1, sym)),
                }
            }
        }

        // per (is test, file): the shallowest depth and the entries
        let mut by_file: BTreeMap<(bool, usize), (usize, Vec<String>)> = BTreeMap::new();

        // where the symbol is defined (d0) is not affected by a change to it: listed, not counted
        let mut definitions: BTreeMap<&str, Vec<String>> = BTreeMap::new();

        for (sym, d) in &dist {
            let Some(sym_data) = self.syms.get(*sym) else {
                continue;
            };

            if *d == 0 {
                if let Some(file) = self.files.get(sym_data.file) {
                    definitions
                        .entry(file.path.as_str())
                        .or_default()
                        .push(self.label(*sym));
                }

                continue;
            }

            let entry = by_file
                .entry((sym_data.test, sym_data.file))
                .or_insert_with(|| (*d, Vec::new()));

            entry.0 = entry.0.min(*d);
            entry.1.push(match via.get(sym) {
                Some((callee, by_name)) => format!(
                    "{} → {}{}",
                    self.label(*sym),
                    if *by_name { "by name " } else { "" },
                    self.label(*callee)
                ),
                None => self.label(*sym),
            });
        }

        for (file, d, callee) in module_users {
            let is_test = self.files.get(file).is_some_and(|f| f.test);

            by_file
                .entry((is_test, file))
                .or_insert_with(|| (d, Vec::new()))
                .1
                .push(format!("<module> → {}", self.label(callee)));
        }

        let mut rows: Vec<(usize, &str, Vec<String>, bool)> = by_file
            .into_iter()
            .filter_map(|((is_test, file), (d, names))| {
                self.files.get(file).map(|f| (d, f.path.as_str(), names, is_test))
            })
            .collect();

        rows.sort();

        let (test_rows, production_rows): (Vec<_>, Vec<_>) = rows.into_iter().partition(|row| row.3);
        let mut out = format!(
            "`{}`: {} production file(s), {} test file(s) within {depth} caller hop(s). Entries read `caller → what it calls`",
            shown(query),
            production_rows.len(),
            test_rows.len()
        );

        if skipped > 0 {
            let _ = write!(out, " ({skipped} ambiguous name matches not followed)");
        }

        out.push('\n');

        // what a file or a string stood for
        if reading != Reading::Symbol
            && let Some(note) = note
        {
            let _ = writeln!(out, "{note}");
        }

        let defined = definitions.len();

        for (path, mut labels) in definitions.into_iter().take(MAX_IMPACT_FILES) {
            labels.sort();
            labels.dedup();
            labels.truncate(MAX_IMPACT_NAMES);

            let _ = writeln!(out, "  d0 {}  [defined here: {}]", shown(path), labels.join(", "));
        }

        if defined > MAX_IMPACT_FILES {
            let _ = writeln!(out, "  … {} more files define it", defined - MAX_IMPACT_FILES);
        }

        // The direct callers are never cut. Deeper files get the rest of the detailed lines, then one bare
        // line each, and what is still left is only counted per depth.
        let direct = production_rows.iter().filter(|row| row.0 <= 1).count();
        let detailed = direct.max(MAX_IMPACT_FILES);
        let bare = detailed + MAX_IMPACT_BARE;
        let mut omitted: BTreeMap<usize, usize> = BTreeMap::new();

        for (i, (d, path, mut names, _)) in production_rows.into_iter().enumerate() {
            if i < detailed {
                names.sort();
                names.dedup();

                let more = names.len().saturating_sub(MAX_IMPACT_NAMES);
                names.truncate(MAX_IMPACT_NAMES);

                let tail = if more > 0 { format!(", +{more}") } else { String::new() };

                let _ = writeln!(out, "  d{d} {}  [{}{tail}]", shown(path), names.join(", "));
            } else if i < bare {
                let _ = writeln!(out, "  d{d} {}", shown(path));
            } else {
                *omitted.entry(d).or_default() += 1;
            }
        }

        if !omitted.is_empty() {
            let counts: Vec<String> = omitted.iter().map(|(d, n)| format!("{n} at d{d}")).collect();

            let _ = writeln!(
                out,
                "  … not listed: {} (ask code_impact about a caller above to see who depends on it)",
                counts.join(", ")
            );
        }

        if !test_rows.is_empty() {
            let list: Vec<String> = test_rows
                .iter()
                .take(MAX_IMPACT_TESTS)
                .map(|row| format!("{} ({})", shown(row.1), row.2.len()))
                .collect();
            let more = if test_rows.len() > MAX_IMPACT_TESTS {
                format!(", … {} more", test_rows.len() - MAX_IMPACT_TESTS)
            } else {
                String::new()
            };

            let _ = writeln!(out, "  tests: {}{more}", list.join(", "));
        }

        out
    }

    /// The reply for a name that matches nothing: similar names.
    pub(super) fn suggest(&self, query: &str) -> String {
        self.suggest_among(query, MAX_SUGGEST_CANDIDATES)
    }

    /// [`Graph::suggest`], comparing at most `most` names.
    pub(super) fn suggest_among(&self, query: &str, most: usize) -> String {
        let name = query.rsplit("::").next().unwrap_or(query);
        let wanted: String = name.chars().take(MAX_SUGGEST_QUERY).collect::<String>().to_lowercase();
        let max = (wanted.len() / 4).clamp(1, 3);
        let longest = wanted.len() * 2 + SUGGEST_EXTRA_LENGTH;

        // the length is checked before anything is copied or compared
        let mut candidates: Vec<&String> = self
            .by_name
            .keys()
            .filter(|candidate| candidate.len() <= longest)
            .collect();

        if candidates.len() > most {
            candidates.sort_unstable_by(|a, b| (a.len(), *a).cmp(&(b.len(), *b)));
            candidates.truncate(most);
        }

        let mut names: Vec<(usize, &String)> = candidates
            .into_iter()
            .filter_map(|candidate| {
                let lower = candidate.to_lowercase();

                if lower.contains(&wanted) {
                    Some((0, candidate))
                } else if lower.len().abs_diff(wanted.len()) <= max {
                    let distance = levenshtein(&lower, &wanted);

                    (distance <= max).then_some((distance, candidate))
                } else {
                    None
                }
            })
            .collect();

        names.sort_by(|a, b| (a.0, a.1.len(), a.1).cmp(&(b.0, b.1.len(), b.1)));

        let list: Vec<std::borrow::Cow<str>> = names
            .iter()
            .take(MAX_SUGGESTIONS)
            .map(|(_, name)| shown(name))
            .collect();

        if list.is_empty() {
            return format!(
                "No symbol, file or command matches `{}`. Grep once for that text to find the function that handles it, then call code_path from that function.\n",
                shown(query)
            );
        }

        format!("no definition named `{}`. Similar: {}\n", shown(query), list.join(", "))
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();

    for (i, ca) in a.chars().enumerate() {
        let mut current = vec![i + 1];

        for (j, cb) in b.iter().enumerate() {
            let substitute = previous.get(j).copied().unwrap_or(0) + usize::from(ca != *cb);
            let delete = previous.get(j + 1).copied().unwrap_or(0) + 1;
            let insert = current.get(j).copied().unwrap_or(0) + 1;

            current.push(substitute.min(delete).min(insert));
        }

        previous = current;
    }

    previous.get(b.len()).copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::levenshtein;

    #[test]
    fn edit_distance_counts_single_edits() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("same", "same"), 0);
    }
}
