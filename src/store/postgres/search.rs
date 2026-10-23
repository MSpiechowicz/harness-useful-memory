//! Full-text search and similar-note lookup.
//!
//! The server folds titles and bodies before they are stored (lowercase, accents removed, split
//! into alphanumeric words, see [`fold`]); the `search` column indexes those words with the
//! `simple` configuration (no stemming, no stopwords), like SQLite's `unicode61 remove_diacritics`
//! tokenizer. Agent text never reaches `to_tsquery` as syntax: it is folded into words, each word
//! is quoted and the words are OR-ed (`'git' | 'cli':*`), so operators, quotes and stray `*`, `!`,
//! `&` or `:` are plain text.
//!
//! A search reads a pool of candidates by `ts_rank` from the English column (`search_en`, stemmed,
//! stopwords dropped: `to_tsquery('english', ...)`) or, in a database without the extensions, from
//! the `simple` one, and [`rank`] decides which of them answer the query and in which order
//! (stopwords, half of the stems, one order for repository and global notes), the same way as for
//! SQLite. `ts_rank` weighs title, tags and body 5 : 3 : 1 and divides the score by the log of the
//! note's length, so the order of close matches can differ from SQLite. The similar-note lookup
//! keeps the `simple` column, its query and its order.
//! Snippets are cut by the server around the first body word that matches, 24 words long.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use postgres::types::ToSql;
use postgres::{GenericClient, Row};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use super::notes::{VISIBLE, kind_from_sql, scope_of};
use super::{KIND_MAX, PATH_MAX, SOURCE_MAX, backend, column, cut, view_params};
use crate::limits::ENTITY_MAX;
use crate::limits::{BODY_MAX, QUERY_MAX, SEARCH_LIMIT_MAX, SNIPPET_MAX, SNIPPET_TOKENS, TAG_MAX, TAGS_MAX, TITLE_MAX};
use crate::store::listing::checked_exclude;
use crate::store::rank::{self, Entry, Floor, Subject, Terms};
use crate::store::{Hit, NoteKind, Provenance, Scope, SearchQuery, StoreError, View};
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

/// Row filters besides the view: the note kind, the checkout root of [`SearchQuery::checkout`] and
/// the ids [`SearchQuery::exclude`] leaves out.
#[derive(Default)]
struct Filter<'a> {
    kind: Option<&'a str>,
    checkout: Option<&'a str>,
    exclude: Vec<i64>,
}

/// The least relevance, as a share of the best candidate's `ts_rank`, that an answer has. `ts_rank`
/// weighs a title word 1.0 and a body word 0.2 before dividing by the length, so a body-only match
/// of a word that is also in a title lands around a fifth and passes.
const FLOOR: Floor = Floor(0.1);

/// The longest tag text read for ranking: [`TAGS_MAX`] tags of [`TAG_MAX`] characters and their
/// separators.
const TAGS_TEXT_MAX: usize = TAGS_MAX * (TAG_MAX + 1);

/// A matched row with the body, which [`similar`] compares.
struct Candidate {
    hit: Hit,
    body: String,
    /// Read for a search (see [`pool`]); empty and zero for the other queries.
    tags: String,
    hits: i64,
    relevance: f64,
}

pub(super) fn search(
    client: &mut impl GenericClient,
    query: &SearchQuery,
    extensions: bool,
) -> Result<Vec<Hit>, StoreError> {
    let text = query.text.trim();
    let length = text.chars().count();

    if length > QUERY_MAX {
        return Err(StoreError::Invalid(format!(
            "query must be at most {QUERY_MAX} characters (got {length}); use a few keywords"
        )));
    }

    let view = view_params(client, &query.view)?;
    let limit = query.limit.clamp(1, SEARCH_LIMIT_MAX);
    let terms = Terms::parse(text);

    let checkout = query.checkout.as_ref().map(|root| root.to_string_lossy().into_owned());
    let filter = Filter {
        kind: query.kind.map(NoteKind::as_str),
        checkout: checkout.as_deref(),
        exclude: checked_exclude(&query.exclude)?.to_vec(),
    };

    if !terms.asked() {
        let rows = recent(client, view, &filter, limit)?;
        return finish(client, rows.into_iter().map(|row| row.hit).collect());
    }

    // A query of stopwords alone has nothing to look for.
    if terms.is_empty() {
        return Ok(Vec::new());
    }

    ranked(client, &terms, view, &filter, limit, extensions)
}

/// The best `limit` answers to `terms`: a pool by `ts_rank`, ranked by [`rank::rank`], entities
/// attached.
fn ranked(
    client: &mut impl GenericClient,
    terms: &Terms,
    view: (Option<i64>, bool),
    filter: &Filter<'_>,
    limit: usize,
    extensions: bool,
) -> Result<Vec<Hit>, StoreError> {
    let pool = pool(client, terms, view, filter, rank::pool_size(limit), extensions)?;

    let ids: Vec<i64> = pool.iter().map(|candidate| candidate.hit.id).collect();
    let mut entities = entities_by_id(client, &ids)?;

    let entries: Vec<Entry<'_>> = pool
        .iter()
        .map(|candidate| Entry {
            relevance: Some(candidate.relevance),
            repo: candidate.hit.scope == Scope::Repo,
            subject: Subject {
                title: &candidate.hit.title,
                body: &candidate.body,
                tags: &candidate.tags,
                entities: entities.get(&candidate.hit.id).map_or(&[], Vec::as_slice),
            },
            updated_at: candidate.hit.updated_at,
            hits: candidate.hits,
            id: candidate.hit.id,
        })
        .collect();
    let ranking = rank::rank(terms, &entries, FLOOR, limit);
    drop(entries);

    let mut slots: Vec<Option<Candidate>> = pool.into_iter().map(Some).collect();
    let mut hits: Vec<Hit> = Vec::with_capacity(ranking.len());

    for item in ranking {
        if let Some(candidate) = slots[item.index].take() {
            hits.push(Hit {
                entities: entities.remove(&candidate.hit.id).unwrap_or_default(),
                score_milli: item.milli,
                boost_milli: item.boost_milli,
                ..candidate.hit
            });
        }
    }

    Ok(hits)
}

/// The notes among `ids` that `query` may see, with the boost of its words (see
/// [`crate::store::MemoryStore::hits_for`]).
pub(super) fn by_ids(
    client: &mut impl GenericClient,
    query: &SearchQuery,
    ids: &[i64],
) -> Result<Vec<Hit>, StoreError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let (repo, global) = view_params(client, &query.view)?;
    let checkout = query.checkout.as_ref().map(|root| root.to_string_lossy().into_owned());
    let kind = query.kind.map(NoteKind::as_str);
    let exclude = checked_exclude(&query.exclude)?.to_vec();

    // The parameters keep the numbers of `pool`, so VISIBLE, SAME_CHECKOUT and NOT_EXCLUDED fit
    // both.
    let sql = format!(
        "SELECT {} FROM notes n
         WHERE n.id = ANY($1::bigint[]) AND {VISIBLE} AND ($4::text IS NULL OR n.kind = $4::text)
           AND n.id != $6 AND {SAME_CHECKOUT} AND {NOT_EXCLUDED}
         LIMIT $5",
        hit_columns()
    );

    let wanted = ids.to_vec();
    let params: [&(dyn ToSql + Sync); 8] = [
        &wanted,
        &repo,
        &global,
        &kind,
        &limit_param(ids.len()),
        &0_i64,
        &checkout,
        &exclude,
    ];

    let rows = client.query(&sql, &params).map_err(backend)?;
    let found = rows
        .iter()
        .map(|row| candidate(row, |body| body.chars().take(SNIPPET_MAX).collect()))
        .collect::<Result<Vec<_>, _>>()?;

    let found_ids: Vec<i64> = found.iter().map(|candidate| candidate.hit.id).collect();
    let mut entities = entities_by_id(client, &found_ids)?;
    let terms = Terms::parse(&query.text);

    Ok(found
        .into_iter()
        .map(|candidate| {
            let entities = entities.remove(&candidate.hit.id).unwrap_or_default();
            let subject = Subject {
                title: &candidate.hit.title,
                body: &candidate.body,
                tags: "",
                entities: &entities,
            };
            let boost_milli = rank::boost_milli(&terms, &subject, candidate.hit.scope == Scope::Repo);

            Hit {
                entities,
                boost_milli,
                ..candidate.hit
            }
        })
        .collect())
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

    resembling(client, view, (&title, &body), Some(id), limit)
}

/// Up to `limit` notes in `view` resembling a note of `title` and `body`, leaving out note `skip`.
pub(super) fn resembling(
    client: &mut impl GenericClient,
    view: &View,
    (title, body): (&str, &str),
    skip: Option<i64>,
    limit: usize,
) -> Result<Vec<Hit>, StoreError> {
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

    let view = view_params(client, view)?;
    let matcher = Matcher::new(&keys, false);
    let candidates = matching(
        client,
        &matcher,
        view,
        &Filter::default(),
        usize::try_from(SIMILAR_CANDIDATES).unwrap_or(10),
        skip,
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

/// The condition of [`Filter::exclude`] (`$8` the ids).
const NOT_EXCLUDED: &str = "n.id <> ALL($8::bigint[])";

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
           AND {SAME_CHECKOUT} AND {NOT_EXCLUDED}
         ORDER BY n.project_id IS NULL, ts_rank('{WEIGHTS}'::float4[], n.search, q, 1) DESC,
                  n.updated_at DESC, n.hits DESC, n.id DESC
         LIMIT $5",
        hit_columns()
    );

    let tsquery = matcher.tsquery();
    let params: [&(dyn ToSql + Sync); 8] = [
        &tsquery,
        &repo,
        &global,
        &filter.kind,
        &limit_param(limit),
        &exclude.unwrap_or(0),
        &filter.checkout,
        &filter.exclude,
    ];

    let rows = client.query(&sql, &params).map_err(backend)?;
    rows.iter()
        .map(|row| candidate(row, |body| matcher.snippet(body)))
        .collect()
}

/// Up to `limit` notes matching `terms`, best `ts_rank` first (the English column with `extensions`,
/// else the `simple` one), each with its relevance, reads and tags for [`rank`].
fn pool(
    client: &mut impl GenericClient,
    terms: &Terms,
    (repo, global): (Option<i64>, bool),
    filter: &Filter<'_>,
    limit: usize,
    extensions: bool,
) -> Result<Vec<Candidate>, StoreError> {
    let (config, index) = if extensions {
        ("english", "search_en")
    } else {
        ("simple", "search")
    };

    // The words are folded like the indexed text, then stemmed by `to_tsquery` itself.
    let words: Vec<String> = terms
        .iter()
        .map(|term| fold_word(&term.raw))
        .filter(|word| !word.is_empty())
        .collect();
    let matcher = Matcher::new(&words, true);

    let sql = format!(
        "SELECT {}, n.hits, {}, ts_rank('{WEIGHTS}'::float4[], n.{index}, q, 1)::float8
         FROM notes n, to_tsquery('{config}'::regconfig, $1) q
         WHERE n.{index} @@ q AND {VISIBLE} AND ($4::text IS NULL OR n.kind = $4::text) AND n.id != $6
           AND {SAME_CHECKOUT} AND {NOT_EXCLUDED}
         ORDER BY ts_rank('{WEIGHTS}'::float4[], n.{index}, q, 1) DESC, n.updated_at DESC, n.hits DESC, n.id DESC
         LIMIT $5",
        hit_columns(),
        cut("n.tags", TAGS_TEXT_MAX)
    );

    let tsquery = matcher.tsquery();
    let params: [&(dyn ToSql + Sync); 8] = [
        &tsquery,
        &repo,
        &global,
        &filter.kind,
        &limit_param(limit),
        &0_i64,
        &filter.checkout,
        &filter.exclude,
    ];

    let rows = client.query(&sql, &params).map_err(backend)?;
    let mut pool = Vec::with_capacity(rows.len());

    for row in &rows {
        let mut found = candidate(row, |body| matcher.snippet(body))?;
        found.hits = column(row, 9)?;
        found.tags = column(row, 10)?;
        found.relevance = column::<f64>(row, 11)?.max(0.0);
        pool.push(found);
    }

    Ok(pool)
}

fn recent(
    client: &mut impl GenericClient,
    (repo, global): (Option<i64>, bool),
    filter: &Filter<'_>,
    limit: usize,
) -> Result<Vec<Candidate>, StoreError> {
    // The parameters keep the numbers of `matching`, so VISIBLE, SAME_CHECKOUT and NOT_EXCLUDED fit
    // both.
    let sql = format!(
        "SELECT {}
         FROM notes n
         WHERE $1::text IS NULL AND {VISIBLE} AND ($4::text IS NULL OR n.kind = $4::text)
           AND n.id != $6 AND {SAME_CHECKOUT} AND {NOT_EXCLUDED}
         ORDER BY n.project_id IS NULL, n.updated_at DESC, n.hits DESC, n.id DESC
         LIMIT $5",
        hit_columns()
    );

    let params: [&(dyn ToSql + Sync); 8] = [
        &None::<String>,
        &repo,
        &global,
        &filter.kind,
        &limit_param(limit),
        &0_i64,
        &filter.checkout,
        &filter.exclude,
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
            score_milli: 0,
            boost_milli: Hit::NO_BOOST,
        },
        body,
        tags: String::new(),
        hits: 0,
        relevance: 0.0,
    })
}

/// Attaches each hit's entities.
fn finish(client: &mut impl GenericClient, mut hits: Vec<Hit>) -> Result<Vec<Hit>, StoreError> {
    let ids: Vec<i64> = hits.iter().map(|hit| hit.id).collect();
    let mut entities = entities_by_id(client, &ids)?;

    for hit in &mut hits {
        hit.entities = entities.remove(&hit.id).unwrap_or_default();
    }

    Ok(hits)
}

/// The entity names of each note of `ids` that has some, alphabetically (by code point, like
/// SQLite), in one query.
fn entities_by_id(client: &mut impl GenericClient, ids: &[i64]) -> Result<HashMap<i64, Vec<String>>, StoreError> {
    let mut found: HashMap<i64, Vec<String>> = HashMap::new();

    if ids.is_empty() {
        return Ok(found);
    }

    let sql = format!(
        "SELECT ne.note_id, {} FROM note_entities ne JOIN entities e ON e.id = ne.entity_id
         WHERE ne.note_id = ANY($1::bigint[]) ORDER BY e.name_norm COLLATE \"C\"",
        cut("e.name", ENTITY_MAX)
    );

    let rows = client.query(&sql, &[&ids]).map_err(backend)?;

    for row in &rows {
        found.entry(column(row, 0)?).or_default().push(column(row, 1)?);
    }

    Ok(found)
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
        let words = words("title:near & !x", usize::MAX);
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
