//! A question in words as the end of a query: `code_path from="/useful-skills doctor" to="harness-memory features"`.
//!
//! An agent that is asked how a feature works often has no symbol, only the words of the question. A text that
//! is no symbol, file or string of the code is read as words:
//!
//! 1. the text is cut into words (letters, digits, `_` and `-`); short words and filler words are dropped;
//! 2. each word is matched, without regard to case, against the names of the definitions (the whole name, then
//!    a part of it: `doctor` is a part of `runDoctorSetup` and `doctor_services`, `authenticated` of
//!    `createAuthenticationMiddleware` by its stem, `note_search` of `noteSearchTool`) and against the word literals
//!    the code writes (`"doctor"`, `"harness-memory"`, `"git status"`: see `extract::word_like`, and the parts of
//!    a string that names a function, `"git_file_lines"` giving `git`, `file` and `lines`);
//! 3. a word that matches neither also counts when it is a part of the name of the type a member belongs to
//!    (`git` for `GitCli::run`), a part of the path of the file that holds a definition the other words found
//!    (`reading` for `src/reading/Input.go`), or a part of a string that names nothing defined (`'http_polling'`):
//!    such a string is a tag or a key, so it helps a definition hold every word but does not rank it higher (nor
//!    does a word of the type, unless the member's own name holds another word: `GitCli::run` for `git cli run`);
//! 4. production comes before tests and tooling (a top-level `scripts/`, `bench/` or `examples/`), functions and
//!    components before constants, then the definitions that match the most words, then those that carry the word
//!    in their name (not only in a string or a folder), the main definition of a file named like it, those whose
//!    name is the word, those whose folders and file hold the words too, those that write it and those that only
//!    hold it as a part (a word that few definitions hold counts more), then the one that is used in more places,
//!    shorter names first, shorter paths first (the source before its copies).
//!
//! A candidate listing shows the copies of one definition (the same name, signature and lines in a file whose path
//! ends with the other's, as generated `plugins/…` folders make them) once, by the shortest path. A definition
//! that holds the words only by its type or by tags and keys it writes is never the end on its own.
//!
//! The best few are the end, and the answer says which words were read as what, so the caller can see the
//! reading and correct it.
//!
//! Words are a guess, and a path built on a wrong guess costs the caller more than no path. A word that no
//! definition holds, and one that very many hold, tells nothing and is not counted. The end counts as one
//! definition (or the definitions of one name) only when it holds every word that counts and clearly beats the
//! best definition of another name (a stronger match, or it is production code where the other is a test).
//! Otherwise the end is unsure: it keeps the ranked definitions for `code_symbol` and `code_impact`, and
//! `code_path` answers with them as candidates to name.

use std::collections::HashMap;

use super::endpoint::{Endpoint, Reading, Unsure};
use super::{Graph, shown};

/// Definitions a text of words stands for.
const MAX_WORD_ENDS: usize = 8;

/// Words of a text that are looked for.
const MAX_QUERY_WORDS: usize = 6;

/// Definitions a note names.
const NOTE_LISTED: usize = 5;

/// The shortest word looked for.
const MIN_WORD: usize = 3;

/// Definitions of one name that count for a word: a name that hundreds of definitions share (`new`) says nothing
/// about one of them.
const MAX_PER_NAME: usize = 64;

/// Definitions kept per word literal, and places in all: a literal like `"name"` is written everywhere.
const MAX_SYMS_PER_WORD: usize = 32;
const MAX_WORD_PLACES: usize = 100_000;

/// Words of a question that say nothing about the code.
const FILLER: &[&str] = &[
    "the", "and", "for", "from", "with", "when", "how", "what", "which", "that", "this", "into", "then", "its", "are",
    "has", "have", "does", "ask", "asks", "file", "files", "changed", "user", "users", "about", "after", "before",
    "can", "could", "did", "each", "every", "was", "were", "will", "would", "should", "there", "their", "them", "they",
    "than", "these", "those", "via", "while", "who", "why", "where", "you", "your", "our", "not", "but", "all", "any",
    "out", "also",
];

/// A word is common when more than this many definitions hold it, or when it is held by more than a twentieth
/// of the definitions and by more than [`COMMON_FLOOR`] of them (`component` in a UI repository, `get` in an API
/// client): it says nothing about which definition is meant, so it does not have to be held.
const COMMON_ABOVE: usize = 500;
const COMMON_FLOOR: usize = 20;
const COMMON_SHARE: usize = 20;

/// The weight of a word that one definition holds; it falls by two for each doubling of the definitions that hold
/// it.
const RARE: u32 = 20;

/// What a word is to a definition, best first: its name, a literal it writes, a part of its name, a part of the
/// name of the type it belongs to, a part of the path of its file, a part of a string it writes that names nothing
/// the repository defines (`'http_polling'`, `"authenticatedPrincipal"`: a tag or a key, which says little about
/// what the definition does).
const NAME: u8 = 12;
const LITERAL: u8 = 8;
const PART: u8 = 4;
const OWNER: u8 = 3;
const PATH: u8 = 2;
const LOOSE: u8 = 1;

/// True for a match only the type of a definition or a tag or key it writes makes (`OWNER`, `LOOSE`): it helps the
/// definition hold every word and adds to its strength, but never makes a definition the end on its own.
fn weak(tier: u8) -> bool {
    tier == OWNER || tier == LOOSE
}

/// Set on a match whose definition carries the word in its own name (the whole name or a part of it), besides
/// whatever else matched: a name says more than a string the definition writes.
const NAMED: u8 = 0x80;

/// Definitions a candidate listing names, and the ranked ones it picks them from.
const LISTED: usize = 5;
const LISTABLE: usize = 64;

/// Keeps the definition `sym` as a writer of the word literal `literal` (and of each of its words, when it is
/// a command of two parts), within the caps.
pub(super) fn keep(words: &mut HashMap<String, Vec<usize>>, places: &mut usize, literal: &str, sym: usize) {
    let keys = literal
        .split(' ')
        .map(|part| part.trim_start_matches('/'))
        .filter(|part| part.len() >= MIN_WORD);

    for key in std::iter::once(literal).chain(keys) {
        if *places >= MAX_WORD_PLACES {
            return;
        }

        let list = words.entry(key.to_string()).or_default();

        if list.len() < MAX_SYMS_PER_WORD && list.last() != Some(&sym) {
            list.push(sym);
            *places += 1;
        }
    }
}

/// Keeps the definition `sym` as a writer of each part of the string `name` that names a function
/// (`"git_file_lines"` gives `git`, `file` and `lines`), within the caps.
pub(super) fn keep_parts(words: &mut HashMap<String, Vec<usize>>, places: &mut usize, name: &str, sym: usize) {
    for part in segments(name) {
        if part.len() < MIN_WORD || FILLER.contains(&part.as_str()) || *places >= MAX_WORD_PLACES {
            continue;
        }

        let list = words.entry(part).or_default();

        if list.len() < MAX_SYMS_PER_WORD && list.last() != Some(&sym) {
            list.push(sym);
            *places += 1;
        }
    }
}

/// The words of `text` that are looked for: lowercase, at least [`MIN_WORD`] long, not filler, each once.
fn query_words(text: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();

    for word in text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-')) {
        let word = word.trim_matches('-').to_lowercase();

        if word.chars().count() >= MIN_WORD && !FILLER.contains(&word.as_str()) && !words.contains(&word) {
            words.push(word);
        }

        if words.len() == MAX_QUERY_WORDS {
            break;
        }
    }

    words
}

/// The parts of a name in lowercase: `runDoctorSetup`, `run_doctor_setup` and `RunDoctorSetup` give `run`,
/// `doctor`, `setup`; `HTTPServer` gives `http`, `server`.
fn segments(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut part = String::new();

    for (i, &c) in chars.iter().enumerate() {
        if !(c.is_alphanumeric()) {
            if !part.is_empty() {
                parts.push(std::mem::take(&mut part));
            }

            continue;
        }

        let before = i.checked_sub(1).and_then(|at| chars.get(at));
        let after = chars.get(i + 1);
        let lower_to_upper = c.is_uppercase() && before.is_some_and(|b| b.is_lowercase() || b.is_ascii_digit());
        let acronym_end =
            c.is_uppercase() && before.is_some_and(|b| b.is_uppercase()) && after.is_some_and(|a| a.is_lowercase());

        if (lower_to_upper || acronym_end) && !part.is_empty() {
            parts.push(std::mem::take(&mut part));
        }

        part.extend(c.to_lowercase());
    }

    if !part.is_empty() {
        parts.push(part);
    }

    parts
}

/// What a definition is worth as an end, after the words it matches: functions and components lead.
fn kind_rank(kind: &str) -> Option<u8> {
    match kind {
        "fn" | "method" | "member" | "component" | "class" => Some(0),
        "const" | "struct" => Some(1),
        _ => None,
    }
}

/// The word without a trailing `s`, for the plural of a name or a folder (`component` for `src/components`).
fn singular(word: &str) -> &str {
    match word.strip_suffix('s') {
        Some(stem) if stem.chars().count() >= MIN_WORD => stem,
        _ => word,
    }
}

/// The word without the ending of its form, so that `authenticated`, `authenticate` and `authentication` meet, and
/// `entries` with `entry`. The stem keeps at least four letters (five for `-y` and `-ion`, so `ready` stays apart
/// from `read` and `station` from `state`), and a final `s` stays after `s`, `u` and `i` (`process`, `status`,
/// `analysis`).
fn stem(word: &str) -> String {
    const ENDINGS: &[(&str, &str, usize)] = &[
        ("ions", "", 5),
        ("ion", "", 5),
        ("ings", "", 4),
        ("ing", "", 4),
        ("ies", "y", 3),
        ("ied", "y", 3),
        ("ed", "", 4),
        ("es", "", 4),
        ("s", "", 4),
        ("e", "", 4),
        ("y", "", 5),
    ];

    for (ending, then, least) in ENDINGS {
        let Some(stem) = word.strip_suffix(ending) else {
            continue;
        };

        if stem.chars().count() < *least || (*ending == "s" && stem.ends_with(['s', 'u', 'i'])) {
            continue;
        }

        return format!("{stem}{then}");
    }

    word.to_owned()
}

/// A word of several parts (`note_search`, `git-file-lines`, `__init__`) as its parts joined by dashes, the way
/// [`segments`] joins the parts of a name: `note-search`, `init`.
fn dashed(word: &str) -> String {
    word.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// True when `joined` (`-run-doctor-setup-`) holds `word` as one of its parts, in the singular or the plural or
/// another form of it (`authenticated` for the part `authentication`). A word of several parts (`note_search`,
/// `git-file-lines`) is looked for as those parts in a row.
fn has_part(joined: &str, word: &str) -> bool {
    let word = if word.contains(['-', '_']) {
        dashed(word)
    } else {
        word.to_owned()
    };

    if word.contains('-') {
        return [word.as_str(), singular(&word)]
            .into_iter()
            .any(|form| joined.contains(&format!("-{form}-")) || joined.contains(&format!("-{form}s-")));
    }

    let wanted = stem(&word);

    joined.split('-').any(|part| {
        part == word || part == singular(&word) || singular(part) == word || (!part.is_empty() && stem(part) == wanted)
    })
}

/// How each word matches one definition (0 = not at all).
type Tiers = [u8; MAX_QUERY_WORDS];

/// Per definition, the best way each word matches it.
type Matches = HashMap<usize, Tiers>;

impl Graph {
    /// The definitions the words of `query` point at, or nothing.
    pub(super) fn words_endpoint(&self, query: &str) -> Endpoint {
        let words = query_words(query);

        if words.is_empty() {
            return Endpoint::none();
        }

        let mut matches = Matches::new();

        self.match_names(&words, &mut matches);
        self.match_literals(query, &words, &mut matches);
        self.match_paths(&words, &mut matches);

        // a word that few definitions hold says more than one that many do
        let mut holders = [0_usize; MAX_QUERY_WORDS];

        for tiers in matches.values() {
            // a weak match does not make a word held, nor common (`git` of four hundred `GitHub*` members)
            for (holder, tier) in holders.iter_mut().zip(tiers) {
                *holder += usize::from(*tier > 0 && !weak(*tier));
            }
        }

        let weights = holders.map(|count| RARE.saturating_sub(2 * count.max(1).ilog2()).max(1));
        let required = self.required(&words, &holders);
        let agree = self.path_agreement(&words, &matches);
        let rank = |sym: usize, tiers: &Tiers| {
            // a word only a tag or key holds (`LOOSE`) helps the definition hold every word and adds to its
            // strength, but does not rank it above one that is named for a word (`'http_polling'` does not make a
            // panel the HTTP request). A word of its type (`OWNER`) counts like that too, unless the member's own
            // name holds another word: `GitCli::run` for `git cli run` is named for all three, but
            // `HttpPollingConfigStrategy::buildQuery` is not named for `http request`
            let own_name = tiers.iter().any(|tier| tier & NAMED != 0);
            let counts = |tier: u8| tier > 0 && tier != LOOSE && (tier != OWNER || own_name);
            let found = tiers.iter().filter(|tier| counts(**tier)).count();
            // a word the name carries counts twice: it says more than a string or a folder does
            let carried: usize = tiers
                .iter()
                .map(|tier| match *tier {
                    tier if !counts(tier) => 0,
                    tier if tier & NAMED != 0 => 2,
                    _ => 1,
                })
                .sum();
            let strength: u64 = tiers
                .iter()
                .zip(weights)
                .map(|(tier, weight)| u64::from(*tier & !NAMED).saturating_mul(u64::from(weight)))
                .fold(0, u64::saturating_add);
            // a definition named the word (`doctor`) before one that holds it as a part (`runDoctorSetup`), and
            // then the one whose folders and file hold the words too
            let exact = tiers.iter().filter(|tier| **tier & !NAMED == NAME).count();
            let def = self.syms.get(sym);

            (
                def.is_some_and(|d| d.test),
                self.is_tooling(sym),
                def.and_then(|d| kind_rank(&d.kind)),
                std::cmp::Reverse(found),
                std::cmp::Reverse(carried),
                std::cmp::Reverse(self.names_its_file(sym)),
                std::cmp::Reverse(exact),
                std::cmp::Reverse(agree.get(&sym).copied().unwrap_or(0)),
                std::cmp::Reverse(strength),
            )
        };
        let mut ranked: Vec<(usize, Tiers)> = matches.into_iter().collect();

        // of definitions that match alike, the one more places use leads, then the shorter name, then the shorter
        // path (the source before its copies in `plugins/…`)
        ranked.sort_by_cached_key(|(sym, tiers)| {
            let def = self.syms.get(*sym);

            (
                rank(*sym, tiers),
                std::cmp::Reverse(self.incoming.get(*sym).map_or(0, Vec::len)),
                def.map_or(0, |d| d.name.len()),
                def.and_then(|d| self.files.get(d.file)).map_or(0, |f| f.path.len()),
                *sym,
            )
        });
        // the candidates to list come from further down, as copies of one definition are listed once
        let listable: Vec<(usize, Tiers)> = ranked.iter().take(LISTABLE).copied().collect();

        ranked.truncate(MAX_WORD_ENDS);

        let Some((first, first_tiers)) = ranked.first() else {
            return Endpoint::none();
        };

        // the definition that holds every word that counts and clearly beats the next one with another name.
        // Definitions of the lead's name that hold the same words are one end, as a name is (`git_file_lines` in
        // two files), but not one that lacks a word the lead holds (`run` for `GitCli::run` and `git`)
        let lead = self.syms.get(*first);
        let held = |tiers: &Tiers| tiers.map(|tier| tier > 0);
        let lead_held = held(first_tiers);
        let same = |sym: usize, tiers: &Tiers| {
            let def = self.syms.get(sym);

            def.map(|d| (&d.name, d.test, kind_rank(&d.kind))) == lead.map(|d| (&d.name, d.test, kind_rank(&d.kind)))
                && held(tiers) == lead_held
        };
        let top = rank(*first, first_tiers);
        let size = ranked.iter().filter(|(sym, tiers)| same(*sym, tiers)).count();
        let beats = ranked
            .iter()
            .find(|(sym, tiers)| !same(*sym, tiers))
            .is_none_or(|(sym, tiers)| top < rank(*sym, tiers));

        let holds = |tiers: &Tiers| required.iter().all(|&i| tiers.get(i).is_some_and(|tier| *tier > 0));
        let holds_all = holds(first_tiers);
        // the end holds at least one word by more than its type or a tag: `'http_polling'` alone names nothing
        let named = first_tiers.iter().any(|tier| *tier > 0 && !weak(*tier));
        let clear = holds_all && beats && named;
        let unsure = (!clear).then(|| self.unsure(&words, &required, &listable, holds_all));

        // the lead's name first
        if clear {
            ranked.sort_by_key(|(sym, tiers)| !same(*sym, tiers));
        }

        let sure_note = clear.then(|| self.words_note(&words, ranked.get(..size).unwrap_or(&[])));
        let note = self.words_note(&words, &ranked);
        let sure = if clear { size } else { 0 };

        Endpoint {
            reading: Reading::Words,
            syms: ranked.into_iter().map(|(sym, _)| sym).collect(),
            module_refs: Vec::new(),
            note: Some(note),
            unsure,
            sure_note,
            sure,
        }
    }

    /// The words a definition has to hold to be the end: those that some definition holds, less the common ones.
    /// When that leaves none, every word that is held.
    fn required(&self, words: &[String], holders: &[usize; MAX_QUERY_WORDS]) -> Vec<usize> {
        let total = self.syms.len();
        let held = |i: &usize| holders.get(*i).copied().unwrap_or(0);
        let common = |i: &usize| {
            let count = held(i);

            count > COMMON_ABOVE || (count > COMMON_FLOOR && count.saturating_mul(COMMON_SHARE) > total)
        };
        let uncommon: Vec<usize> = (0..words.len()).filter(|i| held(i) > 0 && !common(i)).collect();

        if uncommon.is_empty() {
            return (0..words.len()).filter(|i| held(i) > 0).collect();
        }

        uncommon
    }

    /// True for the main definition of its file: a top-level one named like the file (`ApplicationDashboard` in
    /// `ApplicationDashboard.tsx`, `request` in `request.ts`).
    fn names_its_file(&self, sym: usize) -> bool {
        let Some(def) = self.syms.get(sym).filter(|def| def.parent.is_none()) else {
            return false;
        };
        let Some(file) = self.files.get(def.file) else {
            return false;
        };
        let name = file.path.rsplit('/').next().unwrap_or(&file.path);
        let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
        let plain = |text: &str| -> Vec<char> {
            text.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect()
        };

        plain(&def.name) == plain(stem)
    }

    /// True for a definition in a top-level folder of tooling ([`super::is_tooling_path`]): `request` in
    /// `scripts/check-seo-http.mjs` is not the HTTP request of the app.
    fn is_tooling(&self, sym: usize) -> bool {
        self.syms
            .get(sym)
            .and_then(|def| self.files.get(def.file))
            .is_some_and(|file| super::is_tooling_path(&file.path))
    }

    /// Per matched definition, how many of the words are also parts of the path of its file
    /// (`createAuthenticationMiddleware` in `src/auth/middleware.ts` for `auth middleware`): the folder agrees.
    fn path_agreement(&self, words: &[String], matches: &Matches) -> HashMap<usize, u8> {
        let mut per_file: HashMap<usize, u8> = HashMap::new();

        matches
            .keys()
            .filter_map(|&sym| {
                let file = self.syms.get(sym)?.file;
                let count = *per_file.entry(file).or_insert_with(|| {
                    let path = self.files.get(file).map_or("", |f| f.path.as_str());
                    let stem = path.rsplit_once('.').map_or(path, |(stem, _)| stem);
                    let joined = format!("-{}-", segments(stem).join("-"));
                    let held = words
                        .iter()
                        .filter(|word| has_part(&joined, &word.replace('-', "")))
                        .count();

                    u8::try_from(held).unwrap_or(u8::MAX)
                });

                Some((sym, count))
            })
            .collect()
    }

    /// The candidates of an end that words do not settle: the best few with the words each holds.
    fn unsure(&self, words: &[String], required: &[usize], ranked: &[(usize, Tiers)], holds_all: bool) -> Unsure {
        let quote = |list: &[&String]| -> String {
            list.iter()
                .map(|word| shown(word).into_owned())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let all: Vec<&String> = required.iter().filter_map(|i| words.get(*i)).collect();
        let skipped: Vec<&String> = (0..words.len())
            .filter(|i| !required.contains(i))
            .filter_map(|i| words.get(i))
            .collect();
        let why = if holds_all {
            "several definitions hold all of"
        } else {
            "no definition holds all of"
        };
        // the copies of a definition take one place, shown by the copy with the shortest path: a copy has the same
        // name, signature and lines in a file whose path ends with the other's (`doctor.js:148` and
        // `plugins/claude/doctor.js:148`, as a generated folder makes them)
        let copies = |a: usize, b: usize| {
            let (a_loc, b_loc) = (self.loc(a), self.loc(b));
            let (short, long) = if a_loc.len() <= b_loc.len() {
                (a_loc, b_loc)
            } else {
                (b_loc, a_loc)
            };

            let body = |sym: usize| self.syms.get(sym).map(|def| (&def.sig, def.end_line));

            self.label(a) == self.label(b)
                && body(a) == body(b)
                && (short == long || long.ends_with(&format!("/{short}")))
        };
        // per listed place: the copy shown and the ranked definitions it stands for
        let mut groups: Vec<(usize, &Tiers)> = Vec::new();

        for (sym, tiers) in ranked {
            if let Some(group) = groups.iter_mut().find(|(shown, _)| copies(*shown, *sym)) {
                if self.loc(*sym).len() < self.loc(group.0).len() {
                    *group = (*sym, tiers);
                }
            } else if groups.len() < LISTED {
                groups.push((*sym, tiers));
            }
        }

        let lines: Vec<String> = groups
            .iter()
            .map(|(sym, tiers)| {
                let held: Vec<&String> = words
                    .iter()
                    .zip(tiers.iter())
                    .filter(|(_, tier)| **tier > 0)
                    .map(|(word, _)| word)
                    .collect();

                format!("{}  ({})", self.place(*sym), quote(&held))
            })
            .collect();

        // no word is held by more than a type, a tag or a key: say so rather than list no words
        if all.is_empty() {
            return Unsure {
                why: format!("only types, tags or keys hold {}", quote(&skipped)),
                lines,
            };
        }

        let ignored = if skipped.is_empty() {
            String::new()
        } else {
            format!(" (not counted, none or too many hold them: {})", quote(&skipped))
        };

        Unsure {
            why: format!("{why} {}{ignored}", quote(&all)),
            lines,
        }
    }

    /// Matches the words against the names of the definitions.
    fn match_names(&self, words: &[String], matches: &mut Matches) {
        // the word without its dashes and underscores finds the names that hold it, whatever separates their parts,
        // and its stem the names that hold another form of it
        let flat: Vec<String> = words
            .iter()
            .map(|word| singular(&word.replace(['-', '_'], "")).to_owned())
            .collect();
        let stems: Vec<String> = words
            .iter()
            .zip(&flat)
            .map(|(word, flat)| {
                if word.contains(['-', '_']) {
                    flat.clone()
                } else {
                    stem(word)
                }
            })
            .collect();
        // a name in lowercase without its underscores, as the words are compared with it
        let lowered = |name: &str| -> String {
            name.chars()
                .filter(|c| *c != '_')
                .flat_map(char::to_lowercase)
                .collect()
        };
        // how the word `i` matches `name`: the whole name, a part of it, or not at all
        let tier_of = |name: &str, lower: &str, i: usize, parts: &mut Option<String>| -> u8 {
            let wanted = flat.get(i).map_or("", String::as_str);
            let gate = stems.get(i).map_or(wanted, String::as_str);

            if !lower.contains(gate) {
                return 0;
            }

            if lower == wanted || lower.strip_suffix('s') == Some(wanted) {
                return NAME;
            }

            let joined = parts.get_or_insert_with(|| format!("-{}-", segments(name).join("-")));

            if words.get(i).is_some_and(|word| has_part(joined, word)) {
                PART
            } else {
                0
            }
        };

        for (name, syms) in &self.by_name {
            let lower = lowered(name);
            let mut parts: Option<String> = None;

            for i in 0..words.len() {
                let tier = tier_of(name, &lower, i, &mut parts);

                if tier > 0 {
                    self.credit(syms.iter().take(MAX_PER_NAME), i, tier | NAMED, matches);
                }
            }
        }

        // a member holds the words of the type it belongs to, weaker than its own name (`GitCli::run` holds
        // `git`): the name a caller writes it with (`git.run(…)`) says what it is part of
        let mut owners: HashMap<&str, (Tiers, usize)> = HashMap::new();

        for (sym, def) in self.syms.iter().enumerate() {
            let Some(owner) = def.parent.as_deref() else {
                continue;
            };
            // only a member a word may end at uses up the cap of its type (fields do not)
            if kind_rank(&def.kind).is_none() {
                continue;
            }

            let (tiers, credited) = owners.entry(owner).or_insert_with(|| {
                let lower = lowered(owner);
                let mut parts: Option<String> = None;
                let mut tiers: Tiers = [0; MAX_QUERY_WORDS];

                for (i, tier) in tiers.iter_mut().enumerate().take(words.len()) {
                    *tier = if tier_of(owner, &lower, i, &mut parts) > 0 {
                        OWNER
                    } else {
                        0
                    };
                }

                (tiers, 0)
            });

            if tiers.iter().all(|tier| *tier == 0) || *credited >= MAX_PER_NAME {
                continue;
            }

            *credited += 1;

            for (i, tier) in tiers.iter().enumerate() {
                if *tier > 0 {
                    self.credit(std::iter::once(&sym), i, *tier, matches);
                }
            }
        }
    }

    /// Matches the words, and the whole text, against the word literals the code writes.
    fn match_literals(&self, query: &str, words: &[String], matches: &mut Matches) {
        for (i, word) in words.iter().enumerate() {
            if let Some(syms) = self.words.get(word) {
                self.credit(syms.iter(), i, LITERAL, matches);
            }

            if let Some(syms) = self.loose_words.get(word) {
                self.credit(syms.iter(), i, LOOSE, matches);
            }
        }

        // a command written whole (`"/useful-skills doctor"`) is every word of the text at once
        let whole = query.trim().to_lowercase();

        if let Some(syms) = self.words.get(&whole) {
            for i in 0..words.len() {
                self.credit(syms.iter(), i, LITERAL, matches);
            }
        }
    }

    /// Matches the words that matched a definition by no name or literal against the parts of the path of its
    /// file: a definition one word found is more likely the one when the file is named for another word. A word of
    /// several parts (`useful-skills`) counts only as one part there: as parts in a row it would match the folders
    /// of generated copies (`plugins/…/harness-useful-skills/`).
    fn match_paths(&self, words: &[String], matches: &mut Matches) {
        let mut parts: HashMap<usize, String> = HashMap::new();

        for (sym, tiers) in matches.iter_mut() {
            let Some(file) = self.syms.get(*sym).map(|def| def.file) else {
                continue;
            };

            if tiers.iter().take(words.len()).all(|tier| *tier > 0) {
                continue;
            }

            let joined = parts.entry(file).or_insert_with(|| {
                let path = self.files.get(file).map_or("", |f| f.path.as_str());
                let stem = path.rsplit_once('.').map_or(path, |(stem, _)| stem);

                format!("-{}-", segments(stem).join("-"))
            });

            for (word, tier) in words.iter().zip(tiers.iter_mut()) {
                if *tier == 0 && has_part(joined, &word.replace('-', "")) {
                    *tier = PATH;
                }
            }
        }
    }

    /// Notes that `word` (number `i`) matches the definitions `syms` the way `tier` says.
    fn credit<'a>(&self, syms: impl Iterator<Item = &'a usize>, i: usize, tier: u8, matches: &mut Matches) {
        for &sym in syms {
            let allowed = self.syms.get(sym).is_some_and(|def| kind_rank(&def.kind).is_some());

            if allowed && let Some(slot) = matches.entry(sym).or_default().get_mut(i) {
                let named = (*slot | tier) & NAMED;

                *slot = (*slot & !NAMED).max(tier & !NAMED) | named;
            }
        }
    }

    /// `words `a`, `b` → name path:line, …`, and the words that matched nothing.
    fn words_note(&self, words: &[String], ranked: &[(usize, Tiers)]) -> String {
        let used = |i: usize, strong: bool| {
            ranked
                .iter()
                .any(|(_, tiers)| tiers.get(i).is_some_and(|tier| *tier > 0 && !(strong && weak(*tier))))
        };
        let (read, lost): (Vec<usize>, Vec<usize>) = (0..words.len()).partition(|i| used(*i, false));
        // a word the listed definitions hold only by their type or a tag or key they write
        let weakly: Vec<usize> = read.iter().copied().filter(|i| !used(*i, true)).collect();
        let quote = |list: &[usize]| -> String {
            list.iter()
                .filter_map(|i| words.get(*i))
                .map(|word| format!("`{}`", shown(word)))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let places: Vec<String> = ranked
            .iter()
            .take(NOTE_LISTED)
            .map(|(sym, _)| self.place(*sym))
            .collect();
        let more = ranked.len().saturating_sub(NOTE_LISTED);
        let mut note = format!("words {} → {}", quote(&read), places.join(", "));

        if more > 0 {
            note.push_str(&format!(", … {more} more"));
        }

        if !weakly.is_empty() {
            note.push_str(&format!(" (only by a type, tag or key: {})", quote(&weakly)));
        }

        if !lost.is_empty() {
            note.push_str(&format!(" (no match: {})", quote(&lost)));
        }

        note
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{MAX_SYMS_PER_WORD, MAX_WORD_PLACES, has_part, keep, keep_parts, query_words, segments, stem};

    #[test]
    fn the_forms_of_a_word_meet_and_short_or_unlike_words_do_not() {
        assert_eq!(stem("authenticated"), stem("authentication"));
        assert_eq!(stem("authenticate"), stem("authentication"));
        assert_eq!(stem("entries"), stem("entry"));
        assert_eq!(stem("listing"), "list");
        assert_eq!(stem("processes"), stem("process"));
        assert_eq!(stem("status"), "status");
        // a stem keeps four letters: `app` is not `apply`
        assert_eq!(stem("used"), "used");
        // `-y` and `-ion` need a stem of five: `ready` is not `read`, `station` not `state`, `story` not `store`
        assert_ne!(stem("ready"), stem("read"));
        assert_ne!(stem("station"), stem("state"));
        assert_ne!(stem("story"), stem("store"));
        assert_eq!(stem("queries"), stem("query"));
        assert!(has_part("-init-", "__init__"));
        assert!(!has_part("-apply-patch-", "app"));
        assert!(has_part("-create-authentication-middleware-", "authenticated"));
        assert!(has_part("-register-entry-routes-", "entries"));
        assert!(has_part("-note-search-tool-", "note_search"));
        assert!(!has_part("-note-tool-search-", "note_search"));
    }

    #[test]
    fn names_split_at_separators_and_case_changes() {
        assert_eq!(segments("runDoctorSetup"), ["run", "doctor", "setup"]);
        assert_eq!(segments("git_file_lines"), ["git", "file", "lines"]);
        assert_eq!(segments("HTTPServer"), ["http", "server"]);
        assert_eq!(segments("sha256Of"), ["sha256", "of"]);
    }

    #[test]
    fn a_literal_is_kept_for_few_definitions_and_a_command_under_each_of_its_words() {
        let mut words = HashMap::new();
        let mut places = 0;

        for sym in 0..MAX_SYMS_PER_WORD + 10 {
            keep(&mut words, &mut places, "doctor", sym);
            keep(&mut words, &mut places, "doctor", sym);
        }

        keep(&mut words, &mut places, "/useful-skills doctor", 7);
        keep_parts(&mut words, &mut places, "git_file_lines", 7);

        assert_eq!(words.get("doctor").map(Vec::len), Some(MAX_SYMS_PER_WORD));
        assert_eq!(words.get("/useful-skills doctor"), Some(&vec![7]));
        assert_eq!(words.get("useful-skills"), Some(&vec![7]));
        assert_eq!(words.get("lines"), Some(&vec![7]));
        // `file` is filler
        assert!(!words.contains_key("file"));

        let mut full = MAX_WORD_PLACES;

        keep(&mut words, &mut full, "another", 1);
        keep_parts(&mut words, &mut full, "another_one", 1);

        assert!(!words.contains_key("another"));
        assert!(!words.contains_key("one"));
    }

    #[test]
    fn a_question_keeps_its_content_words() {
        assert_eq!(
            query_words("the diff view asks for the full lines of a changed file"),
            ["diff", "view", "full", "lines"]
        );
        assert_eq!(query_words("/useful-skills doctor"), ["useful-skills", "doctor"]);
        assert_eq!(query_words("Doctor doctor"), ["doctor"]);
        assert!(query_words("a of to").is_empty());
    }
}
