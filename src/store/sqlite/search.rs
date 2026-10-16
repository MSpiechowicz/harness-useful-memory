//! Full-text search and similar-note lookup.
//!
//! Agent text never reaches `MATCH` as syntax: it is split into alphanumeric words, each word is
//! double-quoted and the words are OR-ed (`"git" OR "cli"*`), so FTS5 operators, column filters,
//! quotes and stray `*` or `-` are plain text. Bodies, titles and tags are weighted 1 : 5 : 3.

use std::collections::HashSet;
use std::path::PathBuf;

use rusqlite::{Connection, params};

use super::notes::{VISIBLE, entities_of, kind_from_sql, scope_of};
use super::view_params;
use crate::limits::{QUERY_MAX, QUERY_TOKENS_MAX, SEARCH_LIMIT_MAX, SNIPPET_MAX, SNIPPET_TOKENS};
use crate::store::{Hit, NoteKind, Provenance, SearchQuery, StoreError, View};
use crate::support::text::STOPWORDS;

/// Candidates examined by [`similar`] before the word-overlap filter.
const SIMILAR_CANDIDATES: usize = 10;

/// Words of a note used to look for similar notes.
const SIMILAR_TOKENS: usize = 24;

/// The provenance columns of a row, read by [`collect`] after the body.
const PROVENANCE: &str = "n.source, n.writer_root, n.via_override";

/// Row filters besides the view: the note kind, and the checkout root of
/// [`SearchQuery::checkout`].
#[derive(Default)]
struct Filter<'a> {
    kind: Option<&'a str>,
    checkout: Option<&'a str>,
}

/// The condition of [`Filter::checkout`] (`?7` the root): a repository note whose latest write came
/// from that root without `project_path`.
const SAME_CHECKOUT: &str = "(?7 IS NULL OR (n.project_id IS NOT NULL AND n.writer_root = ?7 AND n.via_override = 0))";

/// A matched row with the body, which [`similar`] compares.
struct Candidate {
    hit: Hit,
    body: String,
}

pub(super) fn search(conn: &Connection, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
    let text = query.text.trim();
    let length = text.chars().count();

    if length > QUERY_MAX {
        return Err(StoreError::Invalid(format!(
            "query must be at most {QUERY_MAX} characters (got {length}); use a few keywords"
        )));
    }

    let (repo, global) = view_params(conn, &query.view)?;
    let limit = query.limit.clamp(1, SEARCH_LIMIT_MAX);
    let kind = query.kind.map(NoteKind::as_str);
    let words = words(text, QUERY_TOKENS_MAX);

    let checkout = query.checkout.as_ref().map(|root| root.to_string_lossy().into_owned());
    let filter = Filter {
        kind,
        checkout: checkout.as_deref(),
    };

    let rows = if words.is_empty() {
        recent(conn, (repo, global), &filter, limit)?
    } else {
        matching(conn, &fts_query(&words, true), (repo, global), &filter, limit, None)?
    };

    finish(conn, rows.into_iter().map(|row| row.hit).collect())
}

pub(super) fn similar(conn: &Connection, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
    let text: Option<(String, String)> = rusqlite::OptionalExtension::optional(conn.query_row(
        "SELECT title, body FROM notes WHERE id = ?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ))?;

    let Some((title, body)) = text else {
        return Ok(Vec::new());
    };

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

    let params = view_params(conn, view)?;
    let candidates = matching(
        conn,
        &fts_query(&keys, false),
        params,
        &Filter::default(),
        SIMILAR_CANDIDATES,
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

    finish(conn, similar)
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
    conn: &Connection,
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
         ORDER BY n.project_id IS NULL, bm25(notes_fts, 5.0, 1.0, 3.0), n.updated_at DESC, n.hits DESC, n.id DESC
         LIMIT ?5"
    );

    collect(
        conn,
        &sql,
        params![
            fts,
            repo,
            global,
            filter.kind,
            limit_param(limit),
            exclude.unwrap_or(0),
            filter.checkout
        ],
    )
}

fn recent(
    conn: &Connection,
    (repo, global): (Option<i64>, bool),
    filter: &Filter<'_>,
    limit: usize,
) -> Result<Vec<Candidate>, StoreError> {
    let sql = format!(
        "SELECT n.id, n.kind, n.project_id, n.title, substr(n.body, 1, {SNIPPET_MAX}), n.updated_at, n.body,
                {PROVENANCE}
         FROM notes n
         WHERE ?1 IS NULL AND {VISIBLE} AND (?4 IS NULL OR n.kind = ?4) AND {SAME_CHECKOUT}
         ORDER BY n.project_id IS NULL, n.updated_at DESC, n.hits DESC, n.id DESC
         LIMIT ?5"
    );

    collect(
        conn,
        &sql,
        params![
            None::<String>,
            repo,
            global,
            filter.kind,
            limit_param(limit),
            None::<i64>,
            filter.checkout
        ],
    )
}

fn collect(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Candidate>, StoreError> {
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map(params, |row| {
        let kind: String = row.get(1)?;
        let snippet: String = row.get(4)?;
        let root: Option<String> = row.get(8)?;

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
            },
            body: row.get(6)?,
        })
    })?;

    Ok(rows.collect::<Result<_, _>>()?)
}

/// Attaches each hit's entities.
fn finish(conn: &Connection, mut hits: Vec<Hit>) -> Result<Vec<Hit>, StoreError> {
    for hit in &mut hits {
        hit.entities = entities_of(conn, hit.id)?;
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
