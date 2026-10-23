//! Full-text search and similar-note lookup.
//!
//! Agent text never reaches `MATCH` as syntax: it is split into alphanumeric words, each word is
//! double-quoted and the words are OR-ed (`"git" OR "cli"*`), so FTS5 operators, column filters,
//! quotes and stray `*` or `-` are plain text. Bodies, titles and tags are weighted 1 : 5 : 3.
//!
//! A search reads a pool of candidates by BM25 from the English index (`notes_fts_en`, Porter
//! stems) or, in a database without the extensions, from `notes_fts`, and [`rank`] decides which
//! of them answer the query and in which order (stopwords, half of the stems, one order for
//! repository and global notes). The similar-note lookup keeps its own query and order.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::listing::not_excluded;
use super::notes::{VISIBLE, kind_from_sql, scope_of};
use super::sql::{Sql, Value, args};
use super::view_params;
use crate::limits::{QUERY_MAX, SEARCH_LIMIT_MAX, SNIPPET_MAX, SNIPPET_TOKENS};
use crate::store::listing::excluded_json;
use crate::store::rank::{self, Entry, Floor, Subject, Terms};
use crate::store::{Hit, NoteKind, Provenance, Scope, SearchQuery, StoreError, View};
use crate::support::text::STOPWORDS;

/// Candidates examined by [`similar`] before the word-overlap filter.
const SIMILAR_CANDIDATES: usize = 10;

/// Words of a note used to look for similar notes.
const SIMILAR_TOKENS: usize = 24;

/// The provenance columns of a row, read by [`collect`] after the body.
const PROVENANCE: &str = "n.source, n.writer_root, n.via_override";

/// Row filters besides the view: the note kind, the checkout root of [`SearchQuery::checkout`] and
/// the ids [`SearchQuery::exclude`] leaves out (a JSON array, `None` for none).
#[derive(Default)]
struct Filter<'a> {
    kind: Option<&'a str>,
    checkout: Option<&'a str>,
    exclude: Option<&'a str>,
}

impl Filter<'_> {
    /// The JSON array of the ids left out (`[]` for none).
    fn excluded(&self) -> &str {
        self.exclude.unwrap_or("[]")
    }
}

/// The condition of [`Filter::checkout`] (`?7` the root): a repository note whose latest write came
/// from that root without `project_path`.
const SAME_CHECKOUT: &str = "(?7 IS NULL OR (n.project_id IS NOT NULL AND n.writer_root = ?7 AND n.via_override = 0))";

/// The least relevance, as a share of the best candidate's BM25, that an answer has. BM25 weighs a
/// rare word far above a common one, so a note that only has the common word of a query falls
/// well under a tenth of the note that has the rare one.
const FLOOR: Floor = Floor(0.1);

/// A matched row with the body, which [`similar`] compares.
struct Candidate {
    hit: Hit,
    body: String,
    /// Read for a search (see [`pool`]); empty and zero for the other queries.
    tags: String,
    hits: i64,
    relevance: f64,
}

pub(super) fn search(db: &mut dyn Sql, query: &SearchQuery, extensions: bool) -> Result<Vec<Hit>, StoreError> {
    let text = query.text.trim();
    let length = text.chars().count();

    if length > QUERY_MAX {
        return Err(StoreError::Invalid(format!(
            "query must be at most {QUERY_MAX} characters (got {length}); use a few keywords"
        )));
    }

    let (repo, global) = view_params(db, &query.view)?;
    let limit = query.limit.clamp(1, SEARCH_LIMIT_MAX);
    let kind = query.kind.map(NoteKind::as_str);
    let terms = Terms::parse(text);

    let checkout = query.checkout.as_ref().map(|root| root.to_string_lossy().into_owned());
    let exclude = excluded_json(&query.exclude)?;
    let filter = Filter {
        kind,
        checkout: checkout.as_deref(),
        exclude: Some(&exclude),
    };

    if !terms.asked() {
        let rows = recent(db, (repo, global), &filter, limit)?;
        return finish(db, rows.into_iter().map(|row| row.hit).collect());
    }

    // A query of stopwords alone has nothing to look for.
    if terms.is_empty() {
        return Ok(Vec::new());
    }

    ranked(db, &terms, (repo, global), &filter, limit, extensions)
}

/// The best `limit` answers to `terms`: a pool by BM25, ranked by [`rank::rank`], entities attached.
fn ranked(
    db: &mut dyn Sql,
    terms: &Terms,
    view: (Option<i64>, bool),
    filter: &Filter<'_>,
    limit: usize,
    extensions: bool,
) -> Result<Vec<Hit>, StoreError> {
    let table = if extensions { "notes_fts_en" } else { "notes_fts" };
    let pool = pool(
        db,
        table,
        &fts_query(&terms.raws(), true),
        view,
        filter,
        rank::pool_size(limit),
    )?;

    let ids: Vec<i64> = pool.iter().map(|candidate| candidate.hit.id).collect();
    let mut entities = entities_by_id(db, &ids)?;

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
pub(super) fn by_ids(db: &mut dyn Sql, query: &SearchQuery, ids: &[i64]) -> Result<Vec<Hit>, StoreError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let (repo, global) = view_params(db, &query.view)?;
    let checkout = query.checkout.as_ref().map(|root| root.to_string_lossy().into_owned());
    let exclude = excluded_json(&query.exclude)?;
    let filter = Filter {
        kind: query.kind.map(NoteKind::as_str),
        checkout: checkout.as_deref(),
        exclude: Some(&exclude),
    };
    let wanted: Vec<String> = ids.iter().map(i64::to_string).collect();

    // The parameters keep the numbers of `recent`, so VISIBLE, SAME_CHECKOUT and the exclusion fit
    // both.
    let sql = format!(
        "SELECT n.id, n.kind, n.project_id, n.title, substr(n.body, 1, {SNIPPET_MAX}), n.updated_at, n.body,
                {PROVENANCE}
         FROM notes n
         WHERE n.id IN (SELECT value FROM json_each(?1)) AND {VISIBLE} AND (?4 IS NULL OR n.kind = ?4)
               AND {SAME_CHECKOUT} AND {}
         LIMIT ?5",
        not_excluded(8)
    );

    let found = collect(
        db,
        &sql,
        args![
            format!("[{}]", wanted.join(",")),
            repo,
            global,
            filter.kind,
            limit_param(ids.len()),
            None::<i64>,
            filter.checkout,
            filter.excluded()
        ],
        false,
    )?;

    let found_ids: Vec<i64> = found.iter().map(|candidate| candidate.hit.id).collect();
    let mut entities = entities_by_id(db, &found_ids)?;
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

pub(super) fn similar(db: &mut dyn Sql, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
    let text: Option<(String, String)> = db.row("SELECT title, body FROM notes WHERE id = ?1", args![id], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?;

    let Some((title, body)) = text else {
        return Ok(Vec::new());
    };

    resembling(db, view, (&title, &body), Some(id), limit)
}

/// Up to `limit` notes in `view` resembling a note of `title` and `body`, leaving out note `skip`.
pub(super) fn resembling(
    db: &mut dyn Sql,
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

    let params = view_params(db, view)?;
    let candidates = matching(
        db,
        &fts_query(&keys, false),
        params,
        &Filter::default(),
        SIMILAR_CANDIDATES,
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

    finish(db, similar)
}

/// Lowercased alphanumeric words of `text`, at most `max`.
fn words(text: &str, max: usize) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(max)
        .map(str::to_lowercase)
        .collect()
}

/// `"a" OR "b" OR "c"`, with a prefix `*` on the last word when `prefix`.
fn fts_query(words: &[String], prefix: bool) -> String {
    let quoted: Vec<String> = words.iter().map(|word| format!("\"{word}\"")).collect();
    let mut query = quoted.join(" OR ");

    if prefix {
        query.push('*');
    }

    query
}

fn matching(
    db: &mut dyn Sql,
    fts: &str,
    (repo, global): (Option<i64>, bool),
    filter: &Filter<'_>,
    limit: usize,
    exclude: Option<i64>,
) -> Result<Vec<Candidate>, StoreError> {
    let sql = format!(
        "SELECT n.id, n.kind, n.project_id, n.title, snippet(notes_fts, 1, '', '', '…', {SNIPPET_TOKENS}),
                n.updated_at, n.body, {PROVENANCE}
         FROM notes_fts JOIN notes n ON n.id = notes_fts.rowid
         WHERE notes_fts MATCH ?1 AND {VISIBLE} AND (?4 IS NULL OR n.kind = ?4) AND n.id != ?6 AND {SAME_CHECKOUT}
               AND {}
         ORDER BY n.project_id IS NULL, bm25(notes_fts, 5.0, 1.0, 3.0), n.updated_at DESC, n.hits DESC, n.id DESC
         LIMIT ?5",
        not_excluded(8)
    );

    collect(
        db,
        &sql,
        args![
            fts,
            repo,
            global,
            filter.kind,
            limit_param(limit),
            exclude.unwrap_or(0),
            filter.checkout,
            filter.excluded()
        ],
        false,
    )
}

/// Up to `limit` notes of `table` (`notes_fts_en` or `notes_fts`) matching `fts`, best BM25 first,
/// each with its relevance (BM25 negated, so higher is better), reads and tags for [`rank`].
fn pool(
    db: &mut dyn Sql,
    table: &str,
    fts: &str,
    (repo, global): (Option<i64>, bool),
    filter: &Filter<'_>,
    limit: usize,
) -> Result<Vec<Candidate>, StoreError> {
    let sql = format!(
        "SELECT n.id, n.kind, n.project_id, n.title, snippet({table}, 1, '', '', '…', {SNIPPET_TOKENS}),
                n.updated_at, n.body, {PROVENANCE}, bm25({table}, 5.0, 1.0, 3.0), n.hits, n.tags
         FROM {table} JOIN notes n ON n.id = {table}.rowid
         WHERE {table} MATCH ?1 AND {VISIBLE} AND (?4 IS NULL OR n.kind = ?4) AND n.id != ?6 AND {SAME_CHECKOUT}
               AND {}
         ORDER BY bm25({table}, 5.0, 1.0, 3.0), n.updated_at DESC, n.hits DESC, n.id DESC
         LIMIT ?5",
        not_excluded(8)
    );

    collect(
        db,
        &sql,
        args![
            fts,
            repo,
            global,
            filter.kind,
            limit_param(limit),
            0_i64,
            filter.checkout,
            filter.excluded()
        ],
        true,
    )
}

fn recent(
    db: &mut dyn Sql,
    (repo, global): (Option<i64>, bool),
    filter: &Filter<'_>,
    limit: usize,
) -> Result<Vec<Candidate>, StoreError> {
    let sql = format!(
        "SELECT n.id, n.kind, n.project_id, n.title, substr(n.body, 1, {SNIPPET_MAX}), n.updated_at, n.body,
                {PROVENANCE}
         FROM notes n
         WHERE ?1 IS NULL AND {VISIBLE} AND (?4 IS NULL OR n.kind = ?4) AND {SAME_CHECKOUT} AND {}
         ORDER BY n.project_id IS NULL, n.updated_at DESC, n.hits DESC, n.id DESC
         LIMIT ?5",
        not_excluded(8)
    );

    collect(
        db,
        &sql,
        args![
            None::<String>,
            repo,
            global,
            filter.kind,
            limit_param(limit),
            None::<i64>,
            filter.checkout,
            filter.excluded()
        ],
        false,
    )
}

/// The rows of `sql` as candidates; `scored` when it also selects the BM25, the reads and the tags
/// after the provenance (see [`pool`]).
fn collect(db: &mut dyn Sql, sql: &str, args: &[Value], scored: bool) -> Result<Vec<Candidate>, StoreError> {
    let rows = db.query(sql, args)?;
    let candidates = rows.iter().map(|row| {
        let kind: String = row.get(1)?;
        let snippet: String = row.get(4)?;
        let root: Option<String> = row.get(8)?;

        let (relevance, hits, tags) = if scored {
            // BM25 is zero or negative, the more negative the better.
            (-row.get::<f64>(10)?.min(0.0), row.get(11)?, row.get(12)?)
        } else {
            (0.0, 0, String::new())
        };

        Ok(Candidate {
            hit: Hit {
                id: row.get(0)?,
                kind: kind_from_sql(&kind),
                scope: scope_of(row.get(2)?),
                title: row.get(3)?,
                snippet: shorten(&snippet),
                updated_at: row.get(5)?,
                entities: Vec::new(),
                provenance: Provenance {
                    root: root.map(PathBuf::from),
                    source: row.get(7)?,
                    via_override: row.get(9)?,
                },
                score_milli: 0,
                boost_milli: Hit::NO_BOOST,
            },
            body: row.get(6)?,
            tags,
            hits,
            relevance,
        })
    });

    candidates.collect()
}

/// Attaches each hit's entities, alphabetically, in one query for all hits (one round trip to a
/// remote server, not one per hit).
fn finish(db: &mut dyn Sql, mut hits: Vec<Hit>) -> Result<Vec<Hit>, StoreError> {
    let ids: Vec<i64> = hits.iter().map(|hit| hit.id).collect();
    let mut entities = entities_by_id(db, &ids)?;

    for hit in &mut hits {
        hit.entities = entities.remove(&hit.id).unwrap_or_default();
    }

    Ok(hits)
}

/// The entity names of each note of `ids` that has some, alphabetically, in one query.
fn entities_by_id(db: &mut dyn Sql, ids: &[i64]) -> Result<HashMap<i64, Vec<String>>, StoreError> {
    let mut found: HashMap<i64, Vec<String>> = HashMap::new();

    if ids.is_empty() {
        return Ok(found);
    }

    let list: Vec<String> = ids.iter().map(i64::to_string).collect();
    let rows = db.query(
        "SELECT ne.note_id, e.name FROM note_entities ne JOIN entities e ON e.id = ne.entity_id
         WHERE ne.note_id IN (SELECT value FROM json_each(?1)) ORDER BY e.name_norm",
        args![format!("[{}]", list.join(","))],
    )?;

    for row in &rows {
        found.entry(row.get(0)?).or_default().push(row.get(1)?);
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
