//! Full-text search and similar-note lookup.
//!
//! The server folds titles and bodies before they are stored (lowercase, accents removed, split
//! into alphanumeric words, see [`fold`]); the `search` column indexes those words with the
//! `simple` configuration (no stemming, no stopwords), like SQLite's `unicode61 remove_diacritics`
//! tokenizer. Agent text never reaches `to_tsquery` as syntax: it is folded into words, each word
//! is quoted and the words are OR-ed (`'git' | 'cli':*`), so operators, quotes and stray `*`, `!`,
//! `&` or `:` are plain text.
//!
//! Ranking keeps the SQLite order (repository first, then relevance, then the most recently
//! updated), with `ts_rank` instead of BM25: title, tags and body weigh 5 : 3 : 1 and the score is
//! divided by the log of the note's length, so the order of close matches can differ from SQLite.
//! Snippets are cut by the server around the first body word that matches, 24 words long.

use std::collections::HashSet;
use std::path::PathBuf;

use postgres::types::ToSql;
use postgres::{GenericClient, Row};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use super::notes::{VISIBLE, entities_of, kind_from_sql, scope_of};
use super::{KIND_MAX, PATH_MAX, SOURCE_MAX, backend, column, cut, view_params};
use crate::limits::{BODY_MAX, QUERY_MAX, QUERY_TOKENS_MAX, SEARCH_LIMIT_MAX, SNIPPET_MAX, SNIPPET_TOKENS, TITLE_MAX};
use crate::store::{Hit, NoteKind, Provenance, SearchQuery, StoreError, View};
use crate::support::text::STOPWORDS;

/// Candidates examined by [`similar`] before the word-overlap filter.
const SIMILAR_CANDIDATES: i64 = 10;

/// Words of a note used to look for similar notes.
const SIMILAR_TOKENS: usize = 24;

/// `ts_rank` weights for `{D, C, B, A}`: body, (unused), tags, title.
const WEIGHTS: &str = "{0.2, 0.2, 0.6, 1.0}";

/// The columns [`candidate`] reads, with the text cut to the limits (see [`cut`]).
fn hit_columns() -> String {
    format!(
        "n.id, {kind}, n.project_id, {title}, {body}, n.updated_at, {source}, {root}, n.via_override",
        kind = cut("n.kind", KIND_MAX),
        title = cut("n.title", TITLE_MAX),
        body = cut("n.body", BODY_MAX),
        source = cut("n.source", SOURCE_MAX),
        root = cut("n.writer_root", PATH_MAX),
    )
}

/// Row filters besides the view: the note kind, and the checkout root of
/// [`SearchQuery::checkout`].
#[derive(Default)]
struct Filter<'a> {
    kind: Option<&'a str>,
    checkout: Option<&'a str>,
}

/// A matched row with the body, which [`similar`] compares.
struct Candidate {
    hit: Hit,
    body: String,
}

pub(super) fn search(client: &mut impl GenericClient, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
    let text = query.text.trim();
    let length = text.chars().count();

    if length > QUERY_MAX {
        return Err(StoreError::Invalid(format!(
            "query must be at most {QUERY_MAX} characters (got {length}); use a few keywords"
        )));
    }

    let view = view_params(client, &query.view)?;
    let limit = query.limit.clamp(1, SEARCH_LIMIT_MAX);
    let words = words(text, QUERY_TOKENS_MAX);

    let checkout = query.checkout.as_ref().map(|root| root.to_string_lossy().into_owned());
    let filter = Filter {
        kind: query.kind.map(NoteKind::as_str),
        checkout: checkout.as_deref(),
    };

    let rows = if words.is_empty() {
        recent(client, view, &filter, limit)?
    } else {
        let matcher = Matcher::new(&words, true);
        matching(client, &matcher, view, &filter, limit, None)?
    };

    finish(client, rows.into_iter().map(|row| row.hit).collect())
}

pub(super) fn similar(
    client: &mut impl GenericClient,
    view: &View,
    id: i64,
    limit: usize,
) -> Result<Vec<Hit>, StoreError> {
    let sql = format!(
        "SELECT {}, {} FROM notes WHERE id = $1",
        cut("title", TITLE_MAX),
        cut("body", BODY_MAX)
    );
    let row = client.query_opt(&sql, &[&id]).map_err(backend)?;

    let Some(row) = row else {
        return Ok(Vec::new());
    };

    let title: String = column(&row, 0)?;
    let body: String = column(&row, 1)?;

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

    let view = view_params(client, view)?;
    let matcher = Matcher::new(&keys, false);
    let candidates = matching(
        client,
        &matcher,
        view,
        &Filter::default(),
        usize::try_from(SIMILAR_CANDIDATES).unwrap_or(10),
        Some(id),
    )?;
    let needed = keys.len().min(2);

    let similar = candidates
        .into_iter()
        .filter(|candidate| {
            let known: HashSet<String> = words(&format!("{} {}", candidate.hit.title, candidate.body), usize::MAX)
                .into_iter()
                .collect();
            keys.iter().filter(|key| known.contains(*key)).count() >= needed
        })
        .take(limit)
        .map(|candidate| candidate.hit)
        .collect();

    finish(client, similar)
}

/// `text` as stored for the index: its [`words`], separated by single spaces.
pub(super) fn fold(text: &str) -> String {
    words(text, usize::MAX).join(" ")
}

/// The alphanumeric words of `text`, lowercased and without accents, at most `max`.
fn words(text: &str, max: usize) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(max)
        .map(fold_word)
        .filter(|word| !word.is_empty())
        .collect()
}

/// `word` lowercased, with combining marks (accents) dropped after canonical decomposition.
fn fold_word(word: &str) -> String {
    word.nfd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .collect()
}

/// The words of a full-text query and how they match.
struct Matcher<'a> {
    words: &'a [String],
    /// The last word matches as a prefix.
    prefix: bool,
}

impl<'a> Matcher<'a> {
    fn new(words: &'a [String], prefix: bool) -> Self {
        Self { words, prefix }
    }

    /// `'a' | 'b' | 'c'`, with `:*` on the last word when [`Self::prefix`]. The words are
    /// alphanumeric, so quoting them leaves no syntax.
    fn tsquery(&self) -> String {
        let quoted: Vec<String> = self.words.iter().map(|word| format!("'{word}'")).collect();
        let mut query = quoted.join(" | ");

        if self.prefix {
            query.push_str(":*");
        }

        query
    }

    /// True when the folded `word` is one of the query words (or starts with the last one).
    fn matches(&self, word: &str) -> bool {
        let last = self.words.len().saturating_sub(1);

        self.words.iter().enumerate().any(|(index, query)| {
            if self.prefix && index == last {
                word.starts_with(query.as_str())
            } else {
                word == query
            }
        })
    }

    /// About [`SNIPPET_TOKENS`] words of `body` around its first matching word (the start of the
    /// body when no word of it matches), with `…` where it was cut.
    fn snippet(&self, body: &str) -> String {
        let pieces: Vec<&str> = body.split_whitespace().collect();
        let first = pieces
            .iter()
            .position(|piece| words(piece, usize::MAX).iter().any(|word| self.matches(word)))
            .unwrap_or(0);

        let start = first.saturating_sub(SNIPPET_TOKENS / 4);
        let end = (start + SNIPPET_TOKENS).min(pieces.len());
        let mut snippet = pieces[start..end].join(" ");

        if start > 0 {
            snippet.insert(0, '…');
        }

        if end < pieces.len() {
            snippet.push('…');
        }

        snippet
    }
}

/// The condition of [`Filter::checkout`] (`$7` the root): a repository note whose latest write
/// came from that root without `project_path`.
const SAME_CHECKOUT: &str =
    "($7::text IS NULL OR (n.project_id IS NOT NULL AND n.writer_root = $7::text AND NOT n.via_override))";

fn matching(
    client: &mut impl GenericClient,
    matcher: &Matcher<'_>,
    (repo, global): (Option<i64>, bool),
    filter: &Filter<'_>,
    limit: usize,
    exclude: Option<i64>,
) -> Result<Vec<Candidate>, StoreError> {
    let sql = format!(
        "SELECT {}
         FROM notes n, to_tsquery('simple'::regconfig, $1) q
         WHERE n.search @@ q AND {VISIBLE} AND ($4::text IS NULL OR n.kind = $4::text) AND n.id != $6
           AND {SAME_CHECKOUT}
         ORDER BY n.project_id IS NULL, ts_rank('{WEIGHTS}'::float4[], n.search, q, 1) DESC,
                  n.updated_at DESC, n.hits DESC, n.id DESC
         LIMIT $5",
        hit_columns()
    );

    let tsquery = matcher.tsquery();
    let params: [&(dyn ToSql + Sync); 7] = [
        &tsquery,
        &repo,
        &global,
        &filter.kind,
        &limit_param(limit),
        &exclude.unwrap_or(0),
        &filter.checkout,
    ];

    let rows = client.query(&sql, &params).map_err(backend)?;
    rows.iter()
        .map(|row| candidate(row, |body| matcher.snippet(body)))
        .collect()
}

fn recent(
    client: &mut impl GenericClient,
    (repo, global): (Option<i64>, bool),
    filter: &Filter<'_>,
    limit: usize,
) -> Result<Vec<Candidate>, StoreError> {
    // The parameters keep the numbers of `matching`, so VISIBLE and SAME_CHECKOUT fit both.
    let sql = format!(
        "SELECT {}
         FROM notes n
         WHERE $1::text IS NULL AND {VISIBLE} AND ($4::text IS NULL OR n.kind = $4::text)
           AND n.id != $6 AND {SAME_CHECKOUT}
         ORDER BY n.project_id IS NULL, n.updated_at DESC, n.hits DESC, n.id DESC
         LIMIT $5",
        hit_columns()
    );

    let params: [&(dyn ToSql + Sync); 7] = [
        &None::<String>,
        &repo,
        &global,
        &filter.kind,
        &limit_param(limit),
        &0_i64,
        &filter.checkout,
    ];

    let rows = client.query(&sql, &params).map_err(backend)?;
    rows.iter()
        .map(|row| candidate(row, |body| body.chars().take(SNIPPET_MAX).collect()))
        .collect()
}

/// A row of [`hit_columns`], with the snippet `excerpt` cuts from the body.
fn candidate(row: &Row, excerpt: impl Fn(&str) -> String) -> Result<Candidate, StoreError> {
    let kind: String = column(row, 1)?;
    let body: String = column(row, 4)?;
    let root: Option<String> = column(row, 7)?;

    Ok(Candidate {
        hit: Hit {
            id: column(row, 0)?,
            kind: kind_from_sql(&kind),
            scope: scope_of(column(row, 2)?),
            title: column(row, 3)?,
            snippet: shorten(&excerpt(&body)),
            updated_at: column(row, 5)?,
            entities: Vec::new(),
            provenance: Provenance {
                root: root.map(PathBuf::from),
                source: column(row, 6)?,
                via_override: column(row, 8)?,
            },
        },
        body,
    })
}

/// Attaches each hit's entities.
fn finish(client: &mut impl GenericClient, mut hits: Vec<Hit>) -> Result<Vec<Hit>, StoreError> {
    for hit in &mut hits {
        hit.entities = entities_of(client, hit.id)?;
    }

    Ok(hits)
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

fn limit_param(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_drops_accents_case_and_punctuation() {
        assert_eq!(fold("Crème brûlée, CAFÉ"), "creme brulee cafe");
        assert_eq!(fold("business/src/git/diff_parse.rs"), "business src git diff parse rs");
        assert_eq!(fold("'; DROP TABLE notes; --"), "drop table notes");
    }

    #[test]
    fn the_query_is_quoted_words() {
        let words = words("title:near & !x", QUERY_TOKENS_MAX);
        assert_eq!(Matcher::new(&words, true).tsquery(), "'title' | 'near' | 'x':*");
    }

    #[test]
    fn snippets_start_near_the_first_match() {
        let body = format!("{} needle here {}", "lorem ".repeat(100), "dolor ".repeat(100));
        let words = vec!["needle".to_owned()];
        let snippet = Matcher::new(&words, false).snippet(&body);

        assert!(snippet.starts_with('…') && snippet.ends_with('…'), "{snippet}");
        assert!(snippet.contains("needle"), "{snippet}");
        assert_eq!(snippet.split_whitespace().count(), SNIPPET_TOKENS);
    }
}
