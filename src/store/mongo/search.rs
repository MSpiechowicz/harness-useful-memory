//! Full-text search and similar-note lookup over the `$text` index (the module docs of [`super`]
//! list how the ranking differs from SQLite's FTS5).
//!
//! Agent text never reaches `$search` as syntax: it is split into alphanumeric words joined by
//! spaces, so quotes and `-` (phrase and negation in `$text`) cannot appear. The prefix pattern
//! of the last word is built from the same alphanumeric word, escaped.

use std::collections::{HashMap, HashSet};

use mongodb::bson::{Bson, Document, doc};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use super::notes::{entities_of, kind_of, provenance_of};
use super::{Ctx, Fail, NOTES, cut, int, note_fields, scope_of, strings, text, visible};
use crate::limits::{BODY_MAX, QUERY_MAX, QUERY_TOKENS_MAX, SEARCH_LIMIT_MAX, SNIPPET_MAX, SNIPPET_TOKENS, TITLE_MAX};
use crate::store::{Hit, NoteKind, Provenance, SearchQuery, StoreError, View};
use crate::support::text::STOPWORDS;

/// Candidates read from each of the word query and the prefix query.
const CANDIDATES: i64 = 100;

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

pub(super) fn search(ctx: &mut Ctx<'_>, query: &SearchQuery) -> Result<Vec<Hit>, Fail> {
    let text = query.text.trim();
    let length = text.chars().count();

    if length > QUERY_MAX {
        return Err(Fail::Store(StoreError::Invalid(format!(
            "query must be at most {QUERY_MAX} characters (got {length}); use a few keywords"
        ))));
    }

    let limit = query.limit.clamp(1, SEARCH_LIMIT_MAX);
    let words = words(text, QUERY_TOKENS_MAX);
    let filter = filter(query);

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
    let mut candidates = matching(ctx, filter.clone(), &words, None, CANDIDATES)?;
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

    // The repository first, then relevance, then the most recently updated.
    candidates.sort_by(|a, b| {
        a.project
            .is_none()
            .cmp(&b.project.is_none())
            .then(b.score.total_cmp(&a.score))
            .then(b.updated_at.cmp(&a.updated_at))
            .then(b.hits.cmp(&a.hits))
            .then(b.id.cmp(&a.id))
    });
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

pub(super) fn similar(ctx: &mut Ctx<'_>, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, Fail> {
    let fields = doc! { "title": cut("$title", TITLE_MAX), "body": cut("$body", BODY_MAX) };
    let Some(source) = ctx.find_one(NOTES, doc! { "_id": id }, fields)? else {
        return Ok(Vec::new());
    };

    let title = text(&source, "title").unwrap_or_default();
    let body = text(&source, "body").unwrap_or_default();

    let mut keys: Vec<String> = Vec::new();
    let title_words = words(&title, SIMILAR_TOKENS)
        .into_iter()
        .filter(|word| word.chars().count() >= 3);
    let body_words = words(&body, usize::MAX)
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

    let candidates = matching(ctx, visible(view), &keys, Some(id), SIMILAR_CANDIDATES)?;
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

/// The view, kind and checkout filters of `query`.
fn filter(query: &SearchQuery) -> Document {
    let mut filter = visible(&query.view);

    if let Some(kind) = query.kind {
        filter.insert("kind", kind.as_str());
    }

    if let Some(root) = &query.checkout {
        let same_checkout = doc! {
            "project": { "$ne": Bson::Null },
            "writer_root": root.to_string_lossy().into_owned(),
            "via_override": false,
        };
        filter = doc! { "$and": [filter, same_checkout] };
    }

    filter
}

/// The notes matching any of `words` (whole words) under `filter`, best first, with `textScore`.
fn matching(
    ctx: &mut Ctx<'_>,
    mut filter: Document,
    words: &[String],
    exclude: Option<i64>,
    limit: i64,
) -> Result<Vec<Candidate>, Fail> {
    filter.insert("$text", doc! { "$search": words.join(" ") });

    if let Some(id) = exclude {
        filter.insert("_id", doc! { "$ne": id });
    }

    // Fields are cut before the sort, so the sort never holds a whole oversized document.
    let mut fields = note_fields();
    fields.insert("score", doc! { "$meta": "textScore" });

    let pipeline = vec![
        doc! { "$match": filter },
        doc! { "$project": fields },
        doc! { "$sort": { "project": -1, "score": -1, "updated_at": -1, "hits": -1, "_id": -1 } },
        doc! { "$limit": limit },
    ];

    let found = ctx.aggregate(NOTES, pipeline)?;
    Ok(found.iter().map(Candidate::new).collect())
}

/// MongoDB's text score of the words of `candidate` that start with `prefix` and are longer
/// (equal words are in `textScore` already): per field, `weight * frequency * (0.5 * count /
/// words + 0.5)`, where each repeat of a word counts half the one before.
fn prefix_score(candidate: &Candidate, prefix: &str) -> f64 {
    let tags = candidate.tags.join(" ");
    let fields = [
        (candidate.title.as_str(), TITLE_WEIGHT),
        (candidate.body.as_str(), BODY_WEIGHT),
        (tags.as_str(), TAGS_WEIGHT),
    ];

    let mut score = 0.0;

    for (field, weight) in fields {
        let words = folded_words(field);
        let total = words.len() as f64;
        let mut counts: HashMap<&str, i32> = HashMap::new();

        for word in &words {
            if word.len() > prefix.len() && word.starts_with(prefix) {
                *counts.entry(word.as_str()).or_default() += 1;
            }
        }

        for count in counts.into_values() {
            let frequency: f64 = (0..count).map(|repeat| 0.5_f64.powi(repeat)).sum();
            score += weight * frequency * (0.5 * f64::from(count) / total + 0.5);
        }
    }

    score
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
