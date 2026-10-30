//! A question in words as the end of a query: `code_path from="/useful-skills doctor" to="harness-memory features"`.
//!
//! An agent that is asked how a feature works often has no symbol, only the words of the question. A text that
//! is no symbol, file or string of the code is read as words:
//!
//! 1. the text is cut into words (letters, digits, `_` and `-`); short words and filler words are dropped;
//! 2. each word is matched, without regard to case, against the names of the definitions (the whole name, then
//!    a part of it: `doctor` is a part of `runDoctorSetup` and `doctor_services`) and against the word literals
//!    the code writes (`"doctor"`, `"harness-memory"`, `"git status"`: see `extract::word_like`, and the parts of
//!    a string that names a function, `"git_file_lines"` giving `git`, `file` and `lines`);
//! 3. a word that matches neither also counts when it is a part of the path of the file that holds a definition
//!    the other words found (`reading` for `src/reading/Input.go`);
//! 4. production comes before tests and functions and components before constants, then the definitions that
//!    match the most words, then those that carry the word in their name (not only in a string or a folder), the
//!    main definition of a file named like it, those whose name is the word, those that write it and those that
//!    only hold it as a part (a word that few definitions hold counts more), then the one that is used in more
//!    places, shorter names first.
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

/// What a word is to a definition, best first: its name, a literal it writes, a part of its name.
const NAME: u8 = 6;
const LITERAL: u8 = 4;
const PART: u8 = 2;
const PATH: u8 = 1;

/// Set on a match whose definition carries the word in its own name (the whole name or a part of it), besides
/// whatever else matched: a name says more than a string the definition writes.
const NAMED: u8 = 0x80;

/// Definitions a candidate listing names.
const LISTED: usize = 5;

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

/// True when `joined` (`-run-doctor-setup-`) holds `word` as one of its parts, in the singular or the plural.
fn has_part(joined: &str, word: &str) -> bool {
    [word, singular(word)]
        .into_iter()
        .any(|form| joined.contains(&format!("-{form}-")) || joined.contains(&format!("-{form}s-")))
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
            for (holder, tier) in holders.iter_mut().zip(tiers) {
                *holder += usize::from(*tier > 0);
            }
        }

        let weights = holders.map(|count| RARE.saturating_sub(2 * count.max(1).ilog2()).max(1));
        let required = self.required(&words, &holders);
        let rank = |sym: usize, tiers: &Tiers| {
            let found = tiers.iter().filter(|tier| **tier > 0).count();
            // a word the name carries counts twice: it says more than a string or a folder does
            let carried: usize = tiers
                .iter()
                .map(|tier| match *tier {
                    0 => 0,
                    tier if tier & NAMED != 0 => 2,
                    _ => 1,
                })
                .sum();
            let strength: u64 = tiers
                .iter()
                .zip(weights)
                .map(|(tier, weight)| u64::from(*tier & !NAMED).saturating_mul(u64::from(weight)))
                .fold(0, u64::saturating_add);
            let def = self.syms.get(sym);

            (
                def.is_some_and(|d| d.test),
                def.and_then(|d| kind_rank(&d.kind)),
                std::cmp::Reverse(found),
                std::cmp::Reverse(carried),
                std::cmp::Reverse(self.names_its_file(sym)),
                std::cmp::Reverse(strength),
            )
        };
        let mut ranked: Vec<(usize, Tiers)> = matches.into_iter().collect();

        // of definitions that match alike, the one more places use leads, then the shorter name
        ranked.sort_by_cached_key(|(sym, tiers)| {
            (
                rank(*sym, tiers),
                std::cmp::Reverse(self.incoming.get(*sym).map_or(0, Vec::len)),
                self.syms.get(*sym).map_or(0, |d| d.name.len()),
                *sym,
            )
        });
        ranked.truncate(MAX_WORD_ENDS);

        let Some((first, first_tiers)) = ranked.first() else {
            return Endpoint::none();
        };

        // the definition that holds every word that counts and clearly beats the next one with another name.
        // Definitions of the lead's name are one end, as a name is (`git_file_lines` in two files)
        let lead = self.syms.get(*first);
        let same = |sym: usize| {
            let def = self.syms.get(sym);

            def.map(|d| (&d.name, d.test, kind_rank(&d.kind))) == lead.map(|d| (&d.name, d.test, kind_rank(&d.kind)))
        };
        let top = rank(*first, first_tiers);
        let size = ranked.iter().filter(|(sym, _)| same(*sym)).count();
        let beats = ranked
            .iter()
            .find(|(sym, _)| !same(*sym))
            .is_none_or(|(sym, tiers)| top < rank(*sym, tiers));

        let holds = |tiers: &Tiers| required.iter().all(|&i| tiers.get(i).is_some_and(|tier| *tier > 0));
        let holds_all = holds(first_tiers);
        let clear = holds_all && beats;
        let unsure = (!clear).then(|| self.unsure(&words, &required, &ranked, holds_all));

        // the lead's name first
        if clear {
            ranked.sort_by_key(|(sym, _)| !same(*sym));
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
        let lines: Vec<String> = ranked
            .iter()
            .take(LISTED)
            .map(|(sym, tiers)| {
                let held: Vec<&String> = words
                    .iter()
                    .zip(tiers)
                    .filter(|(_, tier)| **tier > 0)
                    .map(|(word, _)| word)
                    .collect();

                format!("{}  ({})", self.place(*sym), quote(&held))
            })
            .collect();

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
        // the word without its dashes finds the names that hold it, whatever separates their parts
        let flat: Vec<String> = words
            .iter()
            .map(|word| singular(&word.replace('-', "")).to_owned())
            .collect();

        for (name, syms) in &self.by_name {
            let lower: String = name
                .chars()
                .filter(|c| *c != '_')
                .flat_map(char::to_lowercase)
                .collect();
            let mut parts: Option<String> = None;

            for (i, word) in words.iter().enumerate() {
                let wanted = flat.get(i).map_or("", String::as_str);

                if !lower.contains(wanted) {
                    continue;
                }

                let tier = if lower == wanted || lower.strip_suffix('s') == Some(wanted) {
                    NAME
                } else {
                    let joined = parts.get_or_insert_with(|| format!("-{}-", segments(name).join("-")));

                    if has_part(joined, word) { PART } else { 0 }
                };

                if tier > 0 {
                    self.credit(syms.iter().take(MAX_PER_NAME), i, tier | NAMED, matches);
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
    /// file: a definition one word found is more likely the one when the file is named for another word.
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
        let used = |i: usize| {
            ranked
                .iter()
                .any(|(_, tiers)| tiers.get(i).is_some_and(|tier| *tier > 0))
        };
        let (read, lost): (Vec<usize>, Vec<usize>) = (0..words.len()).partition(|i| used(*i));
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

        if !lost.is_empty() {
            note.push_str(&format!(" (no match: {})", quote(&lost)));
        }

        note
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{MAX_SYMS_PER_WORD, MAX_WORD_PLACES, keep, keep_parts, query_words, segments};

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
