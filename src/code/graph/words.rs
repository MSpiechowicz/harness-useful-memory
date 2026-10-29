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
//! 3. production comes before tests and functions and components before constants, then the definitions that
//!    match the most words, then those whose name is the word, those that write it and those that only hold it
//!    as a part (a word that few definitions hold counts more), shorter names first.
//!
//! The best few are the end, and the answer says which words were read as what, so the caller can see the
//! reading and correct it.

use std::collections::HashMap;

use super::endpoint::{Endpoint, Reading};
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
    "has", "have", "does", "ask", "asks", "file", "files", "request", "requests", "changed", "user", "users",
];

/// The weight of a word that one definition holds; it falls by two for each doubling of the definitions that hold
/// it.
const RARE: u32 = 20;

/// What a word is to a definition, best first: its name, a literal it writes, a part of its name.
const NAME: u8 = 3;
const LITERAL: u8 = 2;
const PART: u8 = 1;

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

/// Per definition, the best way each word matches it (0 = not at all).
type Matches = HashMap<usize, [u8; MAX_QUERY_WORDS]>;

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

        // a word that few definitions hold says more than one that many do
        let mut holders = [0_usize; MAX_QUERY_WORDS];

        for tiers in matches.values() {
            for (holder, tier) in holders.iter_mut().zip(tiers) {
                *holder += usize::from(*tier > 0);
            }
        }

        let weights = holders.map(|count| (RARE - 2 * count.max(1).ilog2()).max(1));
        let mut ranked: Vec<(usize, [u8; MAX_QUERY_WORDS])> = matches.into_iter().collect();

        ranked.sort_by_cached_key(|(sym, tiers)| {
            let found = tiers.iter().filter(|tier| **tier > 0).count();
            let strength: u32 = tiers
                .iter()
                .zip(weights)
                .map(|(tier, weight)| u32::from(*tier) * weight)
                .sum();
            let def = self.syms.get(*sym);

            (
                def.is_some_and(|d| d.test),
                def.and_then(|d| kind_rank(&d.kind)),
                std::cmp::Reverse(found),
                std::cmp::Reverse(strength),
                def.map_or(0, |d| d.name.len()),
                *sym,
            )
        });
        ranked.truncate(MAX_WORD_ENDS);

        if ranked.is_empty() {
            return Endpoint::none();
        }

        let note = self.words_note(&words, &ranked);

        Endpoint {
            reading: Reading::Words,
            syms: ranked.into_iter().map(|(sym, _)| sym).collect(),
            module_refs: Vec::new(),
            note: Some(note),
        }
    }

    /// Matches the words against the names of the definitions.
    fn match_names(&self, words: &[String], matches: &mut Matches) {
        // the word without its dashes finds the names that hold it, whatever separates their parts
        let flat: Vec<String> = words.iter().map(|word| word.replace('-', "")).collect();

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

                let tier = if lower == wanted {
                    NAME
                } else {
                    let joined = parts.get_or_insert_with(|| format!("-{}-", segments(name).join("-")));

                    if joined.contains(&format!("-{word}-")) { PART } else { 0 }
                };

                if tier > 0 {
                    self.credit(syms.iter().take(MAX_PER_NAME), i, tier, matches);
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

    /// Notes that `word` (number `i`) matches the definitions `syms` the way `tier` says.
    fn credit<'a>(&self, syms: impl Iterator<Item = &'a usize>, i: usize, tier: u8, matches: &mut Matches) {
        for &sym in syms {
            let allowed = self.syms.get(sym).is_some_and(|def| kind_rank(&def.kind).is_some());

            if allowed && let Some(slot) = matches.entry(sym).or_default().get_mut(i) {
                *slot = (*slot).max(tier);
            }
        }
    }

    /// `words `a`, `b` → name path:line, …`, and the words that matched nothing.
    fn words_note(&self, words: &[String], ranked: &[(usize, [u8; MAX_QUERY_WORDS])]) -> String {
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
