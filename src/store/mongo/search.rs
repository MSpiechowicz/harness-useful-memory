//! Full-text search and similar-note lookup over the `$text` index, or on Amazon DocumentDB over
//! the indexed `terms` with the score computed here (the module docs of [`super`] list how the
//! ranking differs from SQLite's FTS5 and between the two).
//!
//! Agent text never reaches `$search` as syntax: it is split into alphanumeric words joined by
//! spaces, so quotes and `-` (phrase and negation in `$text`) cannot appear. The prefix pattern
//! of the last word is built from the same alphanumeric word, escaped.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use mongodb::bson::{Bson, Document, doc};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use super::notes::{entities_of, kind_of, provenance_of};
use super::{Ctx, Fail, NOTES, cut, error_code, int, note_fields, scope_of, strings, text, visible};
use crate::limits::{BODY_MAX, QUERY_MAX, QUERY_TOKENS_MAX, SEARCH_LIMIT_MAX, SNIPPET_MAX, SNIPPET_TOKENS, TITLE_MAX};
use crate::store::listing::checked_exclude;
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

/// The recency order (repository first, as `null` sorts before any key).
fn recent_order() -> Document {
    doc! { "project": -1, "updated_at": -1, "hits": -1, "_id": -1 }
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
    score: f64,
}

impl Candidate {
    fn new(document: &Document) -> Self {
        let score = match document.get("score") {
            Some(Bson::Double(score)) => *score,
            _ => 0.0,
        };

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
            score,
        }
    }
}

/// Searches with the text index (`text_index`) or, on Amazon DocumentDB, over the `terms`.
pub(super) fn search(ctx: &mut Ctx<'_>, query: &SearchQuery, text_index: bool) -> Result<Vec<Hit>, Fail> {
    let text = query.text.trim();
    let length = text.chars().count();

    if length > QUERY_MAX {
        return Err(Fail::Store(StoreError::Invalid(format!(
            "query must be at most {QUERY_MAX} characters (got {length}); use a few keywords"
        ))));
    }

    let limit = query.limit.clamp(1, SEARCH_LIMIT_MAX);
    let words = words(text, QUERY_TOKENS_MAX);
    let filter = filter(query)?;

    let Some(last) = words.last() else {
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
    };

    let prefix = fold(last);
    let mut candidates = matching(ctx, filter.clone(), &words, None, CANDIDATES, text_index)?;
    let known: HashSet<i64> = candidates.iter().map(|candidate| candidate.id).collect();

    let mut by_prefix = filter;
    by_prefix.insert("terms", doc! { "$regex": format!("^{}", regex::escape(&prefix)) });
    let extra = ctx.find(NOTES, by_prefix, recent_order(), CANDIDATES, note_fields())?;

    for document in &extra {
        let candidate = Candidate::new(document);

        if !known.contains(&candidate.id) {
            candidates.push(candidate);
        }
    }

    for candidate in &mut candidates {
        candidate.score += prefix_score(candidate, &prefix);
    }

    candidates.sort_by(rank);
    candidates.truncate(limit);

    let rows = candidates
        .into_iter()
        .map(|candidate| {
            let snippet = snippet(&candidate.body, &words, Some(&prefix));
            (candidate, snippet)
        })
        .collect();

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
            let snippet = snippet(&candidate.body, &keys, None);
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
        by_text_index(ctx, filter, words, limit)
    } else {
        by_terms(ctx, filter, words, limit)
    }
}

/// The search order: the repository first, then relevance, then the most recently updated (then
/// hits, then id).
fn rank(a: &Candidate, b: &Candidate) -> Ordering {
    a.project
        .is_none()
        .cmp(&b.project.is_none())
        .then(b.score.total_cmp(&a.score))
        .then(b.updated_at.cmp(&a.updated_at))
        .then(b.hits.cmp(&a.hits))
        .then(b.id.cmp(&a.id))
}

/// [`matching`] with `$text`, scored by `textScore`.
fn by_text_index(
    ctx: &mut Ctx<'_>,
    mut filter: Document,
    words: &[String],
    limit: i64,
) -> Result<Vec<Candidate>, Fail> {
    filter.insert("$text", doc! { "$search": words.join(" ") });

    // Fields are cut before the sort, so the sort never holds a whole oversized document.
    let mut fields = note_fields();
    fields.insert("score", doc! { "$meta": "textScore" });

    let pipeline = vec![
        doc! { "$match": filter },
        doc! { "$project": fields },
        doc! { "$sort": { "project": -1, "score": -1, "updated_at": -1, "hits": -1, "_id": -1 } },
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
fn by_terms(ctx: &mut Ctx<'_>, mut filter: Document, words: &[String], limit: i64) -> Result<Vec<Candidate>, Fail> {
    let mut wanted: Vec<String> = Vec::new();

    for word in words.iter().flat_map(|word| folded_words(word)) {
        if !wanted.contains(&word) {
            wanted.push(word);
        }
    }

    filter.insert("terms", doc! { "$in": wanted.clone() });

    let found = ctx.find(NOTES, filter, recent_order(), CANDIDATES, note_fields())?;
    let mut candidates: Vec<Candidate> = found.iter().map(Candidate::new).collect();

    for candidate in &mut candidates {
        candidate.score = word_score(candidate, &wanted);
    }

    candidates.sort_by(rank);
    candidates.truncate(usize::try_from(limit).unwrap_or(0));

    Ok(candidates)
}

/// MongoDB's text score of `candidate` for the folded `words` (see [`field_score`]).
fn word_score(candidate: &Candidate, words: &[String]) -> f64 {
    score_fields(candidate, |word| words.iter().any(|wanted| wanted == word))
}

/// MongoDB's text score of the words of `candidate` that start with `prefix` and are longer
/// (equal words are in `textScore` already): per field, `weight * frequency * (0.5 * count /
/// words + 0.5)`, where each repeat of a word counts half the one before.
fn prefix_score(candidate: &Candidate, prefix: &str) -> f64 {
    score_fields(candidate, |word| word.len() > prefix.len() && word.starts_with(prefix))
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

/// Up to [`SNIPPET_TOKENS`] words of `body` from just before the first word matching `words`
/// (or starting with `prefix`), on one line; the start of the body when nothing matches.
fn snippet(body: &str, words: &[String], prefix: Option<&str>) -> String {
    let tokens: Vec<&str> = body.split_whitespace().collect();
    let wanted: HashSet<String> = words.iter().map(|word| fold(word)).collect();

    let matches = |token: &&str| {
        folded_words(token)
            .iter()
            .any(|part| wanted.contains(part) || prefix.is_some_and(|prefix| part.starts_with(prefix)))
    };

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
    fn the_prefix_score_leaves_out_the_word_itself() {
        let candidate = note("hard hardening", "", &[]);
        assert_eq!(prefix_score(&candidate, "hard"), 3.75);
        assert_eq!(score(&candidate, &["hard"]), 3.75);
    }

    #[test]
    fn the_repository_ranks_first_then_the_score_then_the_most_recent() {
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
        ranked.sort_by(rank);

        let titles: Vec<&str> = ranked.iter().map(|candidate| candidate.title.as_str()).collect();
        assert_eq!(titles, ["d", "c", "b", "a"]);
    }
}
