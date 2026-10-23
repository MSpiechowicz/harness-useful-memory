//! Full-text search and similar-note lookup over the `$text` index, or on Amazon DocumentDB over
//! the indexed `terms` with the score computed here (the module docs of [`super`] list how the
//! ranking differs from SQLite's FTS5 and between the two).
//!
//! Agent text never reaches `$search` as syntax: it is split into alphanumeric words joined by
//! spaces, so quotes and `-` (phrase and negation in `$text`) cannot appear. The prefix pattern
//! of the last word is built from the same alphanumeric word, escaped.
//!
//! A search reads a pool of candidates and [`rank`] decides which of them answer the query and in
//! which order (stopwords, half of the stems, one order for repository and global notes), the
//! same way as for every backend. The pool is the union of the notes the `$text` index finds
//! (where there is one: exact words, scored by the server), the notes that have a stem of a query
//! word in `terms_en` (ranked by how many of the stems they have), the notes with a word or stem
//! the last query word starts and, on DocumentDB or without the extensions, the notes with one of
//! the words in `terms`. Every candidate is then scored from its own text, whichever index found it
//! (the formula of `$text`, over stems, in [`rank`]), so the notes an older server wrote, without
//! `terms_en`, rank like the others. The similar-note lookup keeps its own index, query and order.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use mongodb::bson::{Bson, Document, doc};
use rust_stemmers::{Algorithm, Stemmer};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use super::notes::{entities_of, kind_of, provenance_of};
use super::{Ctx, Fail, NOTES, cut, error_code, int, note_fields, scope_of, strings, text, visible};
use crate::limits::{BODY_MAX, QUERY_MAX, SEARCH_LIMIT_MAX, SNIPPET_MAX, SNIPPET_TOKENS, TITLE_MAX};
use crate::store::listing::checked_exclude;
use crate::store::rank::{self, Floor, Subject, Terms};
use crate::store::{Hit, NoteKind, Provenance, SearchQuery, StoreError, View};
use crate::support::text::STOPWORDS;

/// Candidates read from each of the word query and the prefix query.
const CANDIDATES: i64 = 100;

/// The server error of a `$text` query without a text index.
const INDEX_NOT_FOUND: i32 = 27;

/// Candidates examined by [`similar`] before the word-overlap filter.
const SIMILAR_CANDIDATES: i64 = 10;

/// Words of a note used to look for similar notes.
const SIMILAR_TOKENS: usize = 24;

/// The fields of the text index with their weights (the SQLite store's 5 : 1 : 3).
const TITLE_WEIGHT: f64 = 5.0;
const BODY_WEIGHT: f64 = 1.0;
const TAGS_WEIGHT: f64 = 3.0;

/// The least relevance, as a share of the best candidate's, that an answer has. The relevance is the
/// text one of [`rank`] (title 5, tags 3, body 1, no weight for rarity), which spreads wider than
/// BM25 and `ts_rank`: a body-only match of a word that is also in a title can be a twentieth.
const FLOOR: Floor = Floor(0.05);

/// The recency order of the recent notes (repository first, as `null` sorts before any key).
fn recent_order() -> Document {
    doc! { "project": -1, "updated_at": -1, "hits": -1, "_id": -1 }
}

/// The recency order of a pool of candidates, which any scope may fill.
fn pool_order() -> Document {
    doc! { "updated_at": -1, "hits": -1, "_id": -1 }
}

/// A matched note.
struct Candidate {
    id: i64,
    project: Option<String>,
    kind: NoteKind,
    title: String,
    body: String,
    tags: Vec<String>,
    updated_at: i64,
    hits: i64,
    norms: Vec<String>,
    provenance: Provenance,
    /// The order of the similar notes (see [`word_score`]); a search scores in [`rank::rank`].
    score: f64,
    /// The search score in thousandths once ranked.
    milli: u32,
}

impl Candidate {
    fn new(document: &Document) -> Self {
        Self {
            id: int(document, "_id"),
            project: text(document, "project"),
            kind: kind_of(document),
            title: text(document, "title").unwrap_or_default(),
            body: text(document, "body").unwrap_or_default(),
            tags: strings(document, "tags"),
            updated_at: int(document, "updated_at"),
            hits: int(document, "hits"),
            norms: strings(document, "entities"),
            provenance: provenance_of(document),
            score: 0.0,
            milli: 0,
        }
    }
}

/// Searches with the text index (`text_index`) or, on Amazon DocumentDB, without; `extensions` when
/// the database has `terms_en`.
pub(super) fn search(
    ctx: &mut Ctx<'_>,
    query: &SearchQuery,
    text_index: bool,
    extensions: bool,
) -> Result<Vec<Hit>, Fail> {
    let text = query.text.trim();
    let length = text.chars().count();

    if length > QUERY_MAX {
        return Err(Fail::Store(StoreError::Invalid(format!(
            "query must be at most {QUERY_MAX} characters (got {length}); use a few keywords"
        ))));
    }

    let limit = query.limit.clamp(1, SEARCH_LIMIT_MAX);
    let terms = Terms::parse(text);
    let filter = filter(query)?;

    if !terms.asked() {
        let found = ctx.find(
            NOTES,
            filter,
            recent_order(),
            i64::try_from(limit).unwrap_or(1),
            note_fields(),
        )?;

        let rows = found
            .iter()
            .map(|document| {
                let candidate = Candidate::new(document);
                let start: String = candidate.body.chars().take(SNIPPET_MAX).collect();
                (candidate, shorten(&start))
            })
            .collect();

        return finish(ctx, rows);
    }

    // A query of stopwords alone has nothing to look for.
    if terms.is_empty() {
        return Ok(Vec::new());
    }

    let size = i64::try_from(rank::pool_size(limit)).unwrap_or(CANDIDATES);
    let pool = pool(ctx, filter, &terms, (text_index, extensions), size)?;

    ranked(ctx, &terms, pool, limit)
}

/// The notes that may answer `terms`, each once (see the module docs for where they come from),
/// at most `size` from each source.
fn pool(
    ctx: &mut Ctx<'_>,
    filter: Document,
    terms: &Terms,
    (text_index, extensions): (bool, bool),
    size: i64,
) -> Result<Vec<Candidate>, Fail> {
    let words = terms.raws();
    let mut pool: Vec<Candidate> = Vec::new();
    let mut known: HashSet<i64> = HashSet::new();

    let mut add = |found: Vec<Candidate>| {
        for candidate in found {
            if known.insert(candidate.id) {
                pool.push(candidate);
            }
        }
    };

    if text_index {
        let order = doc! { "score": -1, "updated_at": -1, "hits": -1, "_id": -1 };
        add(by_text_index(ctx, filter.clone(), &words, size, order)?);
    }

    // Notes an older server wrote have no stems yet: their words are found through `terms`, which
    // is the only index of DocumentDB without the extensions.
    if !text_index || !extensions {
        let wanted = folded_terms(&words);
        add(by_words(ctx, filter.clone(), &wanted, size)?);
    }

    if extensions {
        let stems = english_terms(&folded_terms(&words));
        add(by_stems(ctx, filter.clone(), &stems, size)?);
    }

    add(by_prefix(ctx, filter, terms, extensions, size)?);

    Ok(pool)
}

/// The best `limit` of `pool` that answer `terms`, scored by [`rank::rank`] from their own text (the
/// formula of `$text`, over stems), with the snippets cut around the words that matched.
fn ranked(ctx: &mut Ctx<'_>, terms: &Terms, pool: Vec<Candidate>, limit: usize) -> Result<Vec<Hit>, Fail> {
    let stemmer = rank::stemmer();

    let tags: Vec<String> = pool.iter().map(|candidate| candidate.tags.join(" ")).collect();
    let entries: Vec<rank::Entry<'_>> = pool
        .iter()
        .zip(&tags)
        .map(|(candidate, tags)| rank::Entry {
            relevance: None,
            repo: candidate.project.is_some(),
            subject: Subject {
                title: &candidate.title,
                body: &candidate.body,
                tags,
                entities: &candidate.norms,
            },
            updated_at: candidate.updated_at,
            hits: candidate.hits,
            id: candidate.id,
        })
        .collect();
    let ranking = rank::rank(terms, &entries, FLOOR, limit);
    drop(entries);

    let mut slots: Vec<Option<Candidate>> = pool.into_iter().map(Some).collect();
    let mut rows: Vec<(Candidate, String)> = Vec::with_capacity(ranking.len());

    for item in ranking {
        if let Some(mut candidate) = slots[item.index].take() {
            candidate.milli = item.milli;
            let snippet = snippet(&candidate.body, |word| terms.matches(&stemmer, word));
            rows.push((candidate, snippet));
        }
    }

    finish(ctx, rows)
}

pub(super) fn similar(
    ctx: &mut Ctx<'_>,
    view: &View,
    id: i64,
    limit: usize,
    text_index: bool,
) -> Result<Vec<Hit>, Fail> {
    let fields = doc! { "title": cut("$title", TITLE_MAX), "body": cut("$body", BODY_MAX) };
    let Some(source) = ctx.find_one(NOTES, doc! { "_id": id }, fields)? else {
        return Ok(Vec::new());
    };

    let title = text(&source, "title").unwrap_or_default();
    let body = text(&source, "body").unwrap_or_default();

    resembling(ctx, view, (&title, &body), Some(id), limit, text_index)
}

/// Up to `limit` notes in `view` resembling a note of `title` and `body`, leaving out note `skip`.
pub(super) fn resembling(
    ctx: &mut Ctx<'_>,
    view: &View,
    (title, body): (&str, &str),
    skip: Option<i64>,
    limit: usize,
    text_index: bool,
) -> Result<Vec<Hit>, Fail> {
    let mut keys: Vec<String> = Vec::new();
    let title_words = words(title, SIMILAR_TOKENS)
        .into_iter()
        .filter(|word| word.chars().count() >= 3);
    let body_words = words(body, usize::MAX)
        .into_iter()
        .filter(|word| word.chars().count() >= 4);

    for word in title_words.chain(body_words) {
        if keys.len() >= SIMILAR_TOKENS {
            break;
        }

        if !STOPWORDS.contains(&word.as_str()) && !keys.contains(&word) {
            keys.push(word);
        }
    }

    if keys.is_empty() {
        return Ok(Vec::new());
    }

    let candidates = matching(ctx, visible(view), &keys, skip, SIMILAR_CANDIDATES, text_index)?;
    let needed = keys.len().min(2);

    let wanted: HashSet<String> = keys.iter().map(|key| fold(key)).collect();

    let rows = candidates
        .into_iter()
        .filter(|candidate| {
            let known: HashSet<String> = words(&format!("{} {}", candidate.title, candidate.body), usize::MAX)
                .into_iter()
                .collect();
            keys.iter().filter(|key| known.contains(*key)).count() >= needed
        })
        .take(limit)
        .map(|candidate| {
            let snippet = snippet(&candidate.body, |word| wanted.contains(word));
            (candidate, snippet)
        })
        .collect();

    finish(ctx, rows)
}

/// The folded words of a note's title, body and tags (each once), which the prefix query reads.
pub(super) fn terms(title: &str, body: &str, tags: &[String]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();

    for field in [title, body, &tags.join(" ")] {
        for word in folded_words(field) {
            if seen.insert(word.clone()) {
                out.push(word);
            }
        }
    }

    out
}

/// The Snowball English stems of `terms` (the words [`terms`] returns), each once: what the English
/// keyword search matches the stems of a query against.
pub(super) fn english_terms(terms: &[String]) -> Vec<String> {
    let stemmer = Stemmer::create(Algorithm::English);
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();

    for word in terms {
        let stem = stemmer.stem(word).into_owned();

        if seen.insert(stem.clone()) {
            out.push(stem);
        }
    }

    out
}

/// The view, kind, checkout and left-out ids filters of `query`.
fn filter(query: &SearchQuery) -> Result<Document, StoreError> {
    let mut filter = visible(&query.view);

    if let Some(kind) = query.kind {
        filter.insert("kind", kind.as_str());
    }

    let exclude = checked_exclude(&query.exclude)?;

    if !exclude.is_empty() {
        filter.insert("_id", doc! { "$nin": exclude });
    }

    if let Some(root) = &query.checkout {
        let same_checkout = doc! {
            "project": { "$ne": Bson::Null },
            "writer_root": root.to_string_lossy().into_owned(),
            "via_override": false,
        };
        filter = doc! { "$and": [filter, same_checkout] };
    }

    Ok(filter)
}

/// The notes matching any of `words` (whole words) under `filter`, best first, at most `limit`:
/// through the text index when there is one, else through the `terms`.
fn matching(
    ctx: &mut Ctx<'_>,
    mut filter: Document,
    words: &[String],
    exclude: Option<i64>,
    limit: i64,
    text_index: bool,
) -> Result<Vec<Candidate>, Fail> {
    if let Some(id) = exclude {
        filter.insert("_id", doc! { "$ne": id });
    }

    if text_index {
        let order = doc! { "project": -1, "score": -1, "updated_at": -1, "hits": -1, "_id": -1 };
        by_text_index(ctx, filter, words, limit, order)
    } else {
        by_terms(ctx, filter, words, limit)
    }
}

/// The order of similar notes: the repository first, then relevance, then the most recently
/// updated (then hits, then id). A search ranks with [`rank::rank`] instead.
fn similar_order(a: &Candidate, b: &Candidate) -> Ordering {
    a.project
        .is_none()
        .cmp(&b.project.is_none())
        .then(b.score.total_cmp(&a.score))
        .then(b.updated_at.cmp(&a.updated_at))
        .then(b.hits.cmp(&a.hits))
        .then(b.id.cmp(&a.id))
}

/// [`matching`] with `$text`, scored by `textScore` and sorted by `order`.
fn by_text_index(
    ctx: &mut Ctx<'_>,
    mut filter: Document,
    words: &[String],
    limit: i64,
    order: Document,
) -> Result<Vec<Candidate>, Fail> {
    filter.insert("$text", doc! { "$search": words.join(" ") });

    // Fields are cut before the sort, so the sort never holds a whole oversized document.
    let mut fields = note_fields();
    fields.insert("score", doc! { "$meta": "textScore" });

    let pipeline = vec![
        doc! { "$match": filter },
        doc! { "$project": fields },
        doc! { "$sort": order },
        doc! { "$limit": limit },
    ];

    let found = ctx.aggregate(NOTES, pipeline).map_err(|fail| match fail {
        Fail::Db(error) if error_code(&error) == Some(INDEX_NOT_FOUND) => Fail::Store(StoreError::Backend(
            "the notes text index is missing, which happens when the database was set up for Amazon DocumentDB: \
             set database_kind = \"documentdb\""
                .to_owned(),
        )),
        other => other,
    })?;

    Ok(found.iter().map(Candidate::new).collect())
}

/// [`matching`] without a text index (Amazon DocumentDB): the [`CANDIDATES`] most recently
/// updated notes holding one of the folded `words` in their `terms`, scored by [`word_score`].
fn by_terms(ctx: &mut Ctx<'_>, filter: Document, words: &[String], limit: i64) -> Result<Vec<Candidate>, Fail> {
    let wanted = folded_terms(words);
    let mut candidates = by_words(ctx, filter, &wanted, CANDIDATES)?;

    for candidate in &mut candidates {
        candidate.score = word_score(candidate, &wanted);
    }

    candidates.sort_by(similar_order);
    candidates.truncate(usize::try_from(limit).unwrap_or(0));

    Ok(candidates)
}

/// The folded words of `words`, each once.
fn folded_terms(words: &[String]) -> Vec<String> {
    let mut wanted: Vec<String> = Vec::new();

    for word in words.iter().flat_map(|word| folded_words(word)) {
        if !wanted.contains(&word) {
            wanted.push(word);
        }
    }

    wanted
}

/// The `size` most recently updated notes under `filter` that hold one of the folded `wanted`
/// words in their `terms`, unscored.
fn by_words(ctx: &mut Ctx<'_>, mut filter: Document, wanted: &[String], size: i64) -> Result<Vec<Candidate>, Fail> {
    filter.insert("terms", doc! { "$in": wanted });

    let found = ctx.find(NOTES, filter, pool_order(), size, note_fields())?;

    Ok(found.iter().map(Candidate::new).collect())
}

/// The `size` notes under `filter` that have the most of the `stems` in `terms_en`, then the most
/// recently updated: the notes of a word's other forms (`caching` for `cache`), and the ones that
/// have several of the words come first, which is what keeps the half-of-the-stems rule from
/// missing them behind a flood of notes with one common word.
fn by_stems(ctx: &mut Ctx<'_>, mut filter: Document, stems: &[String], size: i64) -> Result<Vec<Candidate>, Fail> {
    filter.insert("terms_en", doc! { "$in": stems });

    // Fields are cut before the sort, like in `by_text_index`.
    let mut fields = note_fields();
    fields.insert(
        "matched",
        doc! { "$size": { "$setIntersection": ["$terms_en", stems] } },
    );

    let pipeline = vec![
        doc! { "$match": filter },
        doc! { "$project": fields },
        doc! { "$sort": { "matched": -1, "updated_at": -1, "hits": -1, "_id": -1 } },
        doc! { "$limit": size },
    ];

    let found = ctx.aggregate(NOTES, pipeline)?;

    Ok(found.iter().map(Candidate::new).collect())
}

/// The `size` most recently updated notes under `filter` with a word the last query word starts
/// (in `terms`) or, with the extensions, a stem the last query stem starts (in `terms_en`, which
/// finds `scheduling` for `schedule`).
fn by_prefix(
    ctx: &mut Ctx<'_>,
    filter: Document,
    terms: &Terms,
    extensions: bool,
    size: i64,
) -> Result<Vec<Candidate>, Fail> {
    let Some(last) = terms.iter().last() else {
        return Ok(Vec::new());
    };

    let word = fold(&last.raw);
    let mut clauses = vec![doc! { "terms": { "$regex": format!("^{}", regex::escape(&word)) } }];

    if extensions {
        let stem = english_terms(&[word]).into_iter().next().unwrap_or_default();
        clauses.push(doc! { "terms_en": { "$regex": format!("^{}", regex::escape(&stem)) } });
    }

    let by_prefix = doc! { "$and": [filter, { "$or": clauses }] };
    let found = ctx.find(NOTES, by_prefix, pool_order(), size, note_fields())?;

    Ok(found.iter().map(Candidate::new).collect())
}

/// MongoDB's text score of `candidate` for the folded `words` (see [`field_score`]).
fn word_score(candidate: &Candidate, words: &[String]) -> f64 {
    score_fields(candidate, |word| words.iter().any(|wanted| wanted == word))
}

/// The sum of [`field_score`] over the title, body and tags of `candidate`, with their weights.
fn score_fields(candidate: &Candidate, matches: impl Fn(&str) -> bool) -> f64 {
    let tags = candidate.tags.join(" ");
    let fields = [
        (candidate.title.as_str(), TITLE_WEIGHT),
        (candidate.body.as_str(), BODY_WEIGHT),
        (tags.as_str(), TAGS_WEIGHT),
    ];

    fields
        .into_iter()
        .map(|(field, weight)| field_score(&folded_words(field), weight, &matches))
        .sum()
}

/// MongoDB's text score of one field (its folded `words`) for the words `matches` accepts: per
/// distinct matched word, `weight * frequency * (0.5 * count / words + 0.5)`, where each repeat
/// of a word counts half the one before.
fn field_score(words: &[String], weight: f64, matches: impl Fn(&str) -> bool) -> f64 {
    let total = words.len() as f64;
    let mut counts: HashMap<&str, i32> = HashMap::new();

    for word in words {
        if matches(word) {
            *counts.entry(word.as_str()).or_default() += 1;
        }
    }

    counts
        .into_values()
        .map(|count| {
            let frequency: f64 = (0..count).map(|repeat| 0.5_f64.powi(repeat)).sum();
            weight * frequency * (0.5 * f64::from(count) / total + 0.5)
        })
        .sum()
}

/// Up to [`SNIPPET_TOKENS`] words of `body` from just before the first token with a folded word
/// that `matches`, on one line; the start of the body when nothing matches.
fn snippet(body: &str, matches: impl Fn(&str) -> bool) -> String {
    let tokens: Vec<&str> = body.split_whitespace().collect();
    let matches = |token: &&str| folded_words(token).iter().any(|part| matches(part));

    let start = tokens
        .iter()
        .position(matches)
        .map_or(0, |at| at.saturating_sub(SNIPPET_TOKENS / 4));
    let end = (start + SNIPPET_TOKENS).min(tokens.len());

    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }

    out.push_str(&tokens[start..end].join(" "));

    if end < tokens.len() {
        out.push('…');
    }

    shorten(&out)
}

/// Attaches each hit's entities.
fn finish(ctx: &mut Ctx<'_>, rows: Vec<(Candidate, String)>) -> Result<Vec<Hit>, Fail> {
    let mut hits: Vec<Hit> = Vec::with_capacity(rows.len());

    for (candidate, snippet) in rows {
        let entities = entities_of(ctx, candidate.project.as_deref(), &candidate.norms)?;

        hits.push(Hit {
            id: candidate.id,
            kind: candidate.kind,
            scope: scope_of(candidate.project.as_deref()),
            title: candidate.title,
            snippet,
            updated_at: candidate.updated_at,
            entities,
            provenance: candidate.provenance,
            score_milli: candidate.milli,
        });
    }

    Ok(hits)
}

/// Lowercased alphanumeric words of `text`, at most `max`.
fn words(text: &str, max: usize) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(max)
        .map(str::to_lowercase)
        .collect()
}

/// The words of `text` lowercased and without diacritics.
fn folded_words(text: &str) -> Vec<String> {
    words(&fold(text), usize::MAX)
}

/// `text` lowercased, with combining marks (diacritics) removed after canonical decomposition.
fn fold(text: &str) -> String {
    text.nfd()
        .filter(|character| !is_combining_mark(*character))
        .collect::<String>()
        .to_lowercase()
}

/// The snippet on one line, at most [`SNIPPET_MAX`] characters.
fn shorten(snippet: &str) -> String {
    let line = snippet.split_whitespace().collect::<Vec<_>>().join(" ");

    if line.chars().count() <= SNIPPET_MAX {
        return line;
    }

    let mut cut: String = line.chars().take(SNIPPET_MAX - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(title: &str, body: &str, tags: &[&str]) -> Candidate {
        Candidate {
            id: 1,
            project: Some("git:example.com/acme/widget".to_owned()),
            kind: NoteKind::Note,
            title: title.to_owned(),
            body: body.to_owned(),
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            updated_at: 0,
            hits: 0,
            norms: Vec::new(),
            provenance: Provenance::default(),
            score: 0.0,
            milli: 0,
        }
    }

    fn score(candidate: &Candidate, words: &[&str]) -> f64 {
        let words: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
        word_score(candidate, &words)
    }

    #[test]
    fn a_word_weighs_five_in_the_title_three_in_the_tags_and_one_in_the_body() {
        // One matched word in a one-word field: weight * 1 * (0.5 * 1 / 1 + 0.5).
        assert_eq!(score(&note("webkit", "", &[]), &["webkit"]), 5.0);
        assert_eq!(score(&note("", "", &["webkit"]), &["webkit"]), 3.0);
        assert_eq!(score(&note("", "webkit", &[]), &["webkit"]), 1.0);

        // Every field at once adds up.
        assert_eq!(score(&note("webkit", "webkit", &["webkit"]), &["webkit"]), 9.0);
        assert_eq!(score(&note("gtk", "qt", &["cairo"]), &["webkit"]), 0.0);
    }

    #[test]
    fn longer_fields_and_repeats_count_less() {
        // Half a word's weight depends on how much of the field it is.
        assert_eq!(score(&note("webkit flags", "", &[]), &["webkit"]), 5.0 * 0.75);

        // A repeat counts half the one before: 1 + 0.5, over a field of two words.
        assert_eq!(score(&note("", "webkit webkit", &[]), &["webkit"]), 1.5);

        // Each matched word scores on its own.
        assert_eq!(score(&note("webkit flags", "", &[]), &["webkit", "flags"]), 7.5);
    }

    #[test]
    fn words_are_matched_after_folding_case_and_diacritics() {
        assert_eq!(score(&note("Café Menu", "", &[]), &["cafe"]), 3.75);
        assert_eq!(
            score(&note("", "Crème brûlée", &["Résumé"]), &["creme", "resume"]),
            0.75 + 3.0
        );
        assert_eq!(score(&note("cafeteria", "", &[]), &["cafe"]), 0.0, "whole words only");
    }

    #[test]
    fn similar_notes_rank_the_repository_first_then_the_score_then_the_most_recent() {
        let mut global = note("a", "", &[]);
        global.project = None;
        global.score = 10.0;

        let mut weak = note("b", "", &[]);
        weak.score = 1.0;
        weak.updated_at = 9;

        let mut strong = note("c", "", &[]);
        strong.score = 2.0;

        let mut tied = note("d", "", &[]);
        tied.score = 2.0;
        tied.updated_at = 5;

        let mut ranked = [global, weak, strong, tied];
        ranked.sort_by(similar_order);

        let titles: Vec<&str> = ranked.iter().map(|candidate| candidate.title.as_str()).collect();
        assert_eq!(titles, ["d", "c", "b", "a"]);
    }
}
