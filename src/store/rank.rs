//! The keyword ranking every backend shares.
//!
//! A backend finds a pool of candidates with its own index (FTS5, `tsvector`, the `$text` index
//! or the `terms_en` stems) ordered by its own relevance. This module decides what of the pool is
//! an answer and in which order, the same way for every backend:
//!
//! 1. The query is cut into words, the stopwords ([`is_query_stopword`]) are dropped and the rest
//!    are reduced to their Snowball English stems ([`Terms`]). A query of `n` distinct stems wants
//!    a note that has at least `ceil(n / 2)` of them in its title, body, tags or entity names. The
//!    last word also matches as a prefix.
//! 2. The candidates that are left keep the relevance their backend gave them (BM25, `ts_rank`; for
//!    MongoDB, the relevance computed here, see below), which is what makes a rare word count more
//!    than a common one. A candidate whose relevance is below [`Floor`] times the best kept
//!    candidate's is dropped, so a long tail of weak matches does not follow one strong note. The
//!    rest get a *share*: their relevance over the best one's.
//! 3. A backend's relevance only compares notes of one database, and the team database and the
//!    local file are merged by score, so the score also holds a *quality* that depends on the note
//!    and the query alone: the words of the query that a field has, `weight * frequency * (0.5 *
//!    count / words + 0.5)` per distinct word (MongoDB's `$text` formula, each repeat counting
//!    half the one before) over the title (5), tags and entity names (3) and body (1), taken
//!    against what a lone title word per query term would give (`r = relevance / (5 * n)`, then
//!    `r / (1 + r)`, so it stays under 1). A candidate without relevance from its backend is
//!    shared by this same text relevance.
//! 4. The score is share times quality, times [`REPO_BOOST`] for a note of this repository, times
//!    [`CONTEXT_BOOST`] when a query word is the name of one of the note's entities or a segment
//!    of a path in its body or entities. Repository and global notes share this one order, and
//!    a best answer with a weak match still scores low, so lists from two databases merge by it.
//! 5. Ties go to the more recently updated note, then the one read more often, then the higher id.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashSet};

use rust_stemmers::{Algorithm, Stemmer};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::limits::QUERY_TOKENS_MAX;
use crate::support::text::is_query_stopword;

/// Multiplies the score of a note of the repository the search is for (a global note is 1.0): an
/// equally relevant repository note wins, a clearly more relevant global one still beats a weak
/// repository one.
pub(crate) const REPO_BOOST: f64 = 1.25;

/// Multiplies the score of a note that a query word names (an entity or a path segment).
pub(crate) const CONTEXT_BOOST: f64 = 1.15;

/// The candidates fetched per wanted result: the filters and the stem count drop some of them.
const POOL_FACTOR: usize = 4;

/// The fewest candidates fetched, so a search for one note still has a pool to filter.
const POOL_MIN: usize = 32;

/// The most candidates fetched.
const POOL_MAX: usize = 100;

/// How many candidates a backend fetches to answer `limit` results.
pub(crate) fn pool_size(limit: usize) -> usize {
    limit.saturating_mul(POOL_FACTOR).clamp(POOL_MIN, POOL_MAX)
}

/// A relative relevance floor: candidates below this share of the best kept one are dropped.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Floor(pub f64);

/// The weight of a word in a title, in tags and entity names, and in a body (the SQLite store's
/// 5 : 3 : 1).
const TITLE_WEIGHT: f64 = 5.0;
const TAGS_WEIGHT: f64 = 3.0;
const BODY_WEIGHT: f64 = 1.0;

/// One word of a query.
pub(crate) struct Term {
    /// The word as typed, lowercased with its accents: what a backend's own index folds itself.
    pub raw: String,
    /// The word without accents (see [`fold`]).
    word: String,
    stem: String,
}

/// The words of a query that are left to match: its distinct stems, stopwords dropped.
pub(crate) struct Terms {
    asked: bool,
    terms: Vec<Term>,
}

impl Terms {
    /// The terms of `text`, at most [`QUERY_TOKENS_MAX`].
    pub(crate) fn parse(text: &str) -> Self {
        let stemmer = stemmer();
        let mut terms: Vec<Term> = Vec::new();
        let mut asked = false;

        for raw in text.split(|character: char| !character.is_alphanumeric()) {
            if raw.is_empty() {
                continue;
            }

            asked = true;

            let word = fold(raw);

            if word.is_empty() || is_query_stopword(&word) || terms.len() >= QUERY_TOKENS_MAX {
                continue;
            }

            let stem = stemmer.stem(&word).into_owned();

            if terms.iter().any(|term| term.stem == stem) {
                continue;
            }

            terms.push(Term {
                raw: raw.to_lowercase(),
                word,
                stem,
            });
        }

        Self { asked, terms }
    }

    /// True when the query had any word at all (an empty query lists the recent notes).
    pub(crate) fn asked(&self) -> bool {
        self.asked
    }

    /// True when no word is left to match (the query was only stopwords): nothing answers it.
    pub(crate) fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Term> {
        self.terms.iter()
    }

    /// The words as typed, lowercased, in query order.
    pub(crate) fn raws(&self) -> Vec<String> {
        self.terms.iter().map(|term| term.raw.clone()).collect()
    }

    /// How many terms a note has to match: half of them, rounded up.
    pub(crate) fn needed(&self) -> usize {
        self.terms.len().div_ceil(2)
    }

    /// Calls `found` with the index of each term that the folded `word` matches: the same stem, or
    /// for the last term also a word or stem it starts.
    fn each_match(&self, stemmer: &Stemmer, word: &str, mut found: impl FnMut(usize)) {
        let stem = stemmer.stem(word);
        let last = self.terms.len().saturating_sub(1);

        for (index, term) in self.terms.iter().enumerate() {
            let prefix = index == last && (word.starts_with(&term.word) || stem.starts_with(&term.stem));

            if *stem == *term.stem || prefix {
                found(index);
            }
        }
    }

    /// True when the folded `word` matches any term.
    pub(crate) fn matches(&self, stemmer: &Stemmer, word: &str) -> bool {
        let mut any = false;
        self.each_match(stemmer, word, |_| any = true);

        any
    }
}

/// The Snowball English stemmer, the algorithm of the `english` text search configurations.
pub(crate) fn stemmer() -> Stemmer {
    Stemmer::create(Algorithm::English)
}

/// `text` lowercased and without accents; also the letters that do not decompose (`ł`, `đ`, `ø`,
/// `ı`, `ß`, `æ`, `œ`) as the Latin letters an index without accents folds them to.
pub(crate) fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let letters = text
        .nfd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase);

    for character in letters {
        match character {
            'ł' => out.push('l'),
            'đ' | 'ð' => out.push('d'),
            'ø' => out.push('o'),
            'ı' => out.push('i'),
            'ß' => out.push_str("ss"),
            'æ' => out.push_str("ae"),
            'œ' => out.push_str("oe"),
            other => out.push(other),
        }
    }

    out
}

/// The [`fold`]ed alphanumeric words of `text`.
pub(crate) fn folded_words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(fold)
        .filter(|word| !word.is_empty())
}

/// The text of a candidate that a query is matched against.
pub(crate) struct Subject<'a> {
    pub title: &'a str,
    pub body: &'a str,
    /// The tags, separated by spaces.
    pub tags: &'a str,
    pub entities: &'a [String],
}

/// A candidate of a backend's pool.
pub(crate) struct Entry<'a> {
    /// The backend's relevance, higher is better and never negative (BM25 is negated first); `None`
    /// to use the relevance of the text (see the module docs).
    pub relevance: Option<f64>,
    /// A note of the repository the search is for.
    pub repo: bool,
    pub subject: Subject<'a>,
    pub updated_at: i64,
    pub hits: i64,
    pub id: i64,
}

/// A kept candidate: its index in the pool, its score in thousandths and the boost that score
/// includes (see [`boost_milli`]).
pub(crate) struct Ranked {
    pub index: usize,
    pub milli: u32,
    pub boost_milli: u32,
}

/// What a subject has of a query.
struct Reading {
    matched: usize,
    relevance: f64,
    named: bool,
}

/// The candidates of `entries` that answer `terms`, best first, at most `limit`. See the module
/// docs for what counts and how the score is made.
pub(crate) fn rank(terms: &Terms, entries: &[Entry<'_>], floor: Floor, limit: usize) -> Vec<Ranked> {
    let stemmer = stemmer();
    let needed = terms.needed();
    let reference = TITLE_WEIGHT * terms.terms.len() as f64;

    let mut kept: Vec<(usize, Reading, f64)> = Vec::new();

    for (index, entry) in entries.iter().enumerate() {
        let reading = read(terms, &stemmer, &entry.subject);

        if reading.matched >= needed {
            let relevance = entry.relevance.unwrap_or(reading.relevance);
            kept.push((index, reading, relevance));
        }
    }

    let best = kept.iter().map(|(_, _, relevance)| *relevance).fold(0.0, f64::max);

    let mut ranked: Vec<Ranked> = Vec::new();

    for (index, reading, relevance) in kept {
        let share = if best > 0.0 { relevance / best } else { 1.0 };

        if share < floor.0 {
            continue;
        }

        let strength = reading.relevance / reference;
        let boost = boost(entries[index].repo, reading.named);
        let score = share * strength / (1.0 + strength) * boost;

        ranked.push(Ranked {
            index,
            milli: (score * 1000.0).round() as u32,
            boost_milli: (boost * 1000.0).round() as u32,
        });
    }

    ranked.sort_by_key(|item| {
        let entry = &entries[item.index];
        (
            Reverse(item.milli),
            Reverse(entry.updated_at),
            Reverse(entry.hits),
            Reverse(entry.id),
        )
    });
    ranked.truncate(limit);

    ranked
}

/// The factor a note scores times: [`REPO_BOOST`] for a note of the repository the search is for,
/// [`CONTEXT_BOOST`] when a query word names it.
fn boost(repo: bool, named: bool) -> f64 {
    let mut boost = 1.0;

    if repo {
        boost *= REPO_BOOST;
    }

    if named {
        boost *= CONTEXT_BOOST;
    }

    boost
}

/// The boost of `subject` for `terms` in thousandths, the one [`rank`] applies: for a note that
/// did not come from a keyword search (a hybrid search finds it by its vector).
pub(crate) fn boost_milli(terms: &Terms, subject: &Subject<'_>, repo: bool) -> u32 {
    let named = {
        let context = context_words(subject);
        terms.iter().any(|term| context.contains(&term.word))
    };

    (boost(repo, named) * 1000.0).round() as u32
}

/// How many terms `subject` has (its title, body, tags and entity names), how relevant it is, and
/// whether a term is the name of one of its entities or a segment of a path in its body or
/// entities.
fn read(terms: &Terms, stemmer: &Stemmer, subject: &Subject<'_>) -> Reading {
    let mut found = vec![false; terms.terms.len()];
    let entities = subject.entities.join(" ");
    let fields = [
        (subject.title, TITLE_WEIGHT),
        (subject.tags, TAGS_WEIGHT),
        (entities.as_str(), TAGS_WEIGHT),
        (subject.body, BODY_WEIGHT),
    ];

    let relevance = fields
        .into_iter()
        .map(|(text, weight)| field_relevance(terms, stemmer, text, weight, &mut found))
        .sum();
    let context = context_words(subject);

    Reading {
        matched: found.iter().filter(|found| **found).count(),
        relevance,
        named: terms.iter().any(|term| context.contains(&term.word)),
    }
}

/// The relevance of one field `text`: per distinct word that matches a term, `weight * frequency *
/// (0.5 * count / words + 0.5)`, where each repeat of a word counts half the one before. Marks
/// the terms it matched in `found`.
fn field_relevance(terms: &Terms, stemmer: &Stemmer, text: &str, weight: f64, found: &mut [bool]) -> f64 {
    // Sorted, so the sum (and with it a tie) never depends on a hash order.
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut total = 0_u32;

    for word in folded_words(text) {
        *counts.entry(word).or_default() += 1;
        total += 1;
    }

    let mut relevance = 0.0;

    for (word, count) in counts {
        let mut matched = false;
        terms.each_match(stemmer, &word, |index| {
            found[index] = true;
            matched = true;
        });

        if !matched {
            continue;
        }

        let mut frequency = 0.0;
        let mut repeat = 1.0;

        for _ in 0..count {
            frequency += repeat;
            repeat /= 2.0;
        }

        relevance += weight * frequency * (0.5 * f64::from(count) / f64::from(total) + 0.5);
    }

    relevance
}

/// The folded names of the entities of `subject` and the segments of the paths in its body and
/// entities (a segment, and a file name without its extension).
fn context_words(subject: &Subject<'_>) -> HashSet<String> {
    let mut context: HashSet<String> = HashSet::new();

    for entity in subject.entities {
        context.insert(fold(entity.trim()));
        path_segments(entity, &mut context);
    }

    for token in subject.body.split_whitespace() {
        path_segments(token, &mut context);
    }

    context
}

/// Adds the segments of `token` to `into` when it is a path (has a `/` or `\`).
fn path_segments(token: &str, into: &mut HashSet<String>) {
    if !token.contains(['/', '\\']) {
        return;
    }

    for segment in token.split(['/', '\\']) {
        let segment = segment.trim_matches(|character: char| !character.is_alphanumeric());

        if segment.is_empty() {
            continue;
        }

        into.insert(fold(segment));

        if let Some((stem, _)) = segment.split_once('.') {
            into.insert(fold(stem));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject<'a>(title: &'a str, body: &'a str, entities: &'a [String]) -> Subject<'a> {
        Subject {
            title,
            body,
            tags: "",
            entities,
        }
    }

    const FLOOR: Floor = Floor(0.05);

    fn entry(id: i64, subject: Subject<'_>) -> Entry<'_> {
        Entry {
            relevance: None,
            repo: false,
            subject,
            updated_at: 0,
            hits: 0,
            id,
        }
    }

    fn ids(ranked: &[Ranked], entries: &[Entry<'_>]) -> Vec<i64> {
        ranked.iter().map(|item| entries[item.index].id).collect()
    }

    #[test]
    fn stopwords_and_repeated_stems_are_dropped() {
        let terms = Terms::parse("What is the cache and the caching of invalidation?");
        assert_eq!(terms.raws(), ["cache", "invalidation"]);
        assert_eq!(terms.needed(), 1);
        assert!(terms.asked());

        let only_stopwords = Terms::parse("what is the");
        assert!(only_stopwords.asked() && only_stopwords.is_empty());

        let blank = Terms::parse(" *-\" ");
        assert!(!blank.asked() && blank.is_empty());
    }

    #[test]
    fn half_of_the_stems_round_up() {
        let needed = |text: &str| Terms::parse(text).needed();

        assert_eq!(needed("zebra"), 1);
        assert_eq!(needed("zebra migration"), 1);
        assert_eq!(needed("zebra migration plan"), 2);
        assert_eq!(needed("kubernetes helm chart deployment"), 2);
    }

    #[test]
    fn a_note_needs_half_of_the_stems() {
        let terms = Terms::parse("zebra migration plan");
        let entries = [
            entry(1, subject("Migration plan", "for the schema", &[])),
            entry(2, subject("Migration", "of rows", &[])),
            entry(3, subject("Zebra", "crossing the migration", &[])),
        ];

        let ranked = rank(&terms, &entries, FLOOR, 10);
        assert_eq!(ids(&ranked, &entries), [1, 3]);
    }

    #[test]
    fn stems_match_across_inflections_and_accents() {
        let terms = Terms::parse("cache invalidation");
        let entries = [entry(1, subject("Invalidating the caching layer", "", &[]))];
        assert_eq!(rank(&terms, &entries, FLOOR, 10).len(), 1);

        let polish = Terms::parse("zazolc gesla");
        let entries = [entry(1, subject("Zażółć gęślą jaźń encoding", "", &[]))];
        assert_eq!(rank(&polish, &entries, FLOOR, 10).len(), 1);
    }

    #[test]
    fn the_last_word_matches_as_a_prefix() {
        let terms = Terms::parse("routines sched");
        let entries = [entry(1, subject("Scheduler", "", &[]))];
        assert_eq!(rank(&terms, &entries, FLOOR, 10).len(), 1);

        let first = Terms::parse("sched routines");
        assert!(
            rank(&first, &entries, FLOOR, 10).is_empty(),
            "only the last word is a prefix"
        );
    }

    #[test]
    fn entity_names_count_as_text() {
        let terms = Terms::parse("zebra migration");
        let entities = vec!["zebra".to_owned()];
        let entries = [entry(1, subject("Plan", "", &entities))];
        assert_eq!(rank(&terms, &entries, FLOOR, 10).len(), 1);
    }

    #[test]
    fn a_title_outweighs_a_body_and_a_lone_weak_answer_still_scores_low() {
        let terms = Terms::parse("webkit");
        let title = [entry(1, subject("webkit", "", &[]))];
        let body = [entry(1, subject("sidebar", "mentions webkit once", &[]))];

        // Each is the best answer of its own database, which says nothing about the other one.
        let alone = |entries: &[Entry<'_>]| rank(&terms, entries, FLOOR, 10)[0].milli;
        assert!(alone(&title) > 3 * alone(&body));

        // A backend's relevance only compares candidates of one database.
        let mut strong = entry(1, subject("webkit", "", &[]));
        strong.relevance = Some(10.0);
        let mut weak = entry(2, subject("webkit", "", &[]));
        weak.relevance = Some(2.0);

        let entries = [strong, weak];
        let ranked = rank(&terms, &entries, FLOOR, 10);
        assert_eq!(ids(&ranked, &entries), [1, 2]);
        assert!(ranked[0].milli > 3 * ranked[1].milli);
    }

    #[test]
    fn the_repository_boost_decides_equal_notes_and_not_clear_differences() {
        let terms = Terms::parse("webkit");
        let mut repo = entry(1, subject("webkit", "", &[]));
        repo.repo = true;
        let global = entry(2, subject("webkit", "", &[]));

        let entries = [global, repo];
        assert_eq!(ids(&rank(&terms, &entries, FLOOR, 10), &entries), [1, 2]);

        let mut weak = entry(1, subject("sidebar", "mentions webkit once", &[]));
        weak.repo = true;
        let strong = entry(2, subject("webkit rendering webkit", "webkit webkit webkit", &[]));

        let entries = [weak, strong];
        assert_eq!(ids(&rank(&terms, &entries, FLOOR, 10), &entries), [2, 1]);
    }

    #[test]
    fn a_query_word_naming_an_entity_or_a_path_segment_boosts_the_note() {
        let terms = Terms::parse("sqlite");
        let entities = vec!["SQLite".to_owned()];
        let named = entry(1, subject("sqlite", "", &entities));
        let path = entry(2, subject("sqlite", "see src/store/sqlite/search.rs", &[]));
        let plain = entry(3, subject("sqlite", "", &[]));

        let entries = [plain, path, named];
        // The entity also counts as text, so it outranks the path.
        let ranked = rank(&terms, &entries, FLOOR, 10);
        assert_eq!(ids(&ranked, &entries), [1, 2, 3]);
        assert!(ranked[1].milli > ranked[2].milli);

        let file = Terms::parse("search");
        let entries = [entry(1, subject("search", "src/store/sqlite/search.rs", &[]))];
        let plain = [entry(1, subject("search", "src store sqlite search rs", &[]))];
        assert!(rank(&file, &entries, FLOOR, 10)[0].milli > rank(&file, &plain, FLOOR, 10)[0].milli);
    }

    #[test]
    fn a_weak_tail_under_the_floor_is_dropped() {
        let terms = Terms::parse("webkit");
        let lost = format!("webkit {}", "lorem ".repeat(200));
        let entries = [
            entry(1, subject("webkit webkit webkit", "webkit webkit webkit", &[])),
            entry(2, subject("sidebar", &lost, &[])),
            entry(3, subject("webkit", "", &[])),
        ];

        // Note 2 has less than a twentieth of note 1's relevance.
        assert_eq!(ids(&rank(&terms, &entries, FLOOR, 10), &entries), [1, 3]);
    }

    #[test]
    fn ties_go_to_the_recent_then_the_often_read_then_the_higher_id() {
        let terms = Terms::parse("webkit");
        let mut old = entry(1, subject("webkit", "", &[]));
        old.updated_at = 1;
        let mut recent = entry(2, subject("webkit", "", &[]));
        recent.updated_at = 2;
        let mut read = entry(3, subject("webkit", "", &[]));
        read.updated_at = 1;
        read.hits = 5;
        let mut newer = entry(4, subject("webkit", "", &[]));
        newer.updated_at = 1;

        let entries = [old, recent, read, newer];
        assert_eq!(ids(&rank(&terms, &entries, FLOOR, 10), &entries), [2, 3, 4, 1]);
        assert_eq!(rank(&terms, &entries, FLOOR, 2).len(), 2);
    }
}
