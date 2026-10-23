//! Hybrid `note_search`: the notes closest in meaning to the query, found by the embedder
//! ([`crate::embed`]), fused with the keyword results ([`crate::store::fuse`]).
//!
//! It runs when embedding is on (`embedder.toml` in the data folder) and the query has a word
//! that is not a stopword. The embedder is asked for the notes of the physical stores the view
//! covers ([`crate::store::MemoryStore::vector_targets`]) and keeps the ones that clearly answer
//! the query ([`closest`]); the store then
//! loads those notes under the view, kind, checkout and hidden-note rules of a search
//! ([`crate::store::MemoryStore::hits_for`]), so a note the keyword search could not return is never returned by
//! its vector either. An embedder that is not running or not ready costs the search nothing but
//! the vector hits: the keyword results are returned, with [`UNAVAILABLE`] on the first such
//! search of a session.
//!
//! The same module keeps the vectors current from this side: a saved or changed note is sent to
//! the embedder at once and a deleted one forgotten, best effort and with short timeouts (the
//! embedder's own pass finds what was missed).

use std::path::PathBuf;

use super::Server;
use crate::backend::{Place, Selection};
use crate::embed::protocol::{NoteInput, Target, VectorHit};
use crate::embed::{self, EmbedderClient};
use crate::limits::{QUERY_MAX, SEARCH_LIMIT_MAX};
use crate::store::fuse::fuse;
use crate::store::rank::Terms;
use crate::store::split::LOCAL_ID_OFFSET;
use crate::store::{Hit, Note, NoteOrigin, Scope, SearchQuery, StoreError, VectorTarget, View};
use crate::support::Env;

/// The line a search starts with when embedding is on and the embedder did not answer.
pub(super) const UNAVAILABLE: &str = "(vector search unavailable: keyword results only)";

/// The least cosine a note has to answer a query. Measured with the int8 multilingual MiniLM on
/// 1000 notes of similar vocabulary: a query that has an answer reaches 0.45 (two words, a
/// synonym) to 0.85 with its best note, and one that has none reaches 0.2 to 0.45.
const COSINE_FLOOR: f32 = 0.40;

/// How far above its background a note has to stand. The background is the cosine of the least
/// close of the [`VECTOR_DEPTH`] notes asked for: a query about a topic the notes do not cover
/// reaches 0.45 when many notes share its vocabulary, but all of them reach about 0.4, while the
/// best answer of a query rises 0.10 to 0.45 above its background.
const MARGIN: f32 = 0.08;

/// The fewest notes asked for that tell a background: with fewer in view, the floor alone decides.
const BACKGROUND_MIN: usize = 10;

/// How many notes the embedder is asked for.
const VECTOR_DEPTH: usize = SEARCH_LIMIT_MAX;

/// The embedder of this computer and the keys it names the stores by.
#[derive(Debug, Clone)]
pub struct Vectors {
    /// The data folder, `<data>/harness-memory`, whose `embedder.toml` says whether it is on.
    root: PathBuf,
    client: EmbedderClient,
    /// The key of the local file and of the team database; `None` when there is none.
    local: Option<String>,
    team: Option<String>,
}

impl Vectors {
    /// The embedder of the data folder `root`, naming the local file `local` and the team
    /// database `team`.
    pub fn new(root: PathBuf, local: Option<String>, team: Option<String>) -> Self {
        Self {
            client: EmbedderClient::new(&root, &std::env::temp_dir()),
            root,
            local,
            team,
        }
    }

    /// The embedder for the stores `selection` names; `None` when the data folder is unknown.
    pub fn for_selection(env: &Env, selection: &Selection) -> Option<Self> {
        let path = crate::database_path(env)?;
        let root = path.parent()?.to_path_buf();

        Some(Self::new(
            root,
            Some(embed::store_key_local(&path)),
            embed::store_key_team(&selection.backend),
        ))
    }

    /// Whether embedding is on. Read on every call, so turning it on or off takes effect at once.
    fn enabled(&self) -> bool {
        embed::enabled(&self.root)
    }

    fn key(&self, place: Place) -> Option<&str> {
        match place {
            Place::Local => self.local.as_deref(),
            Place::Remote => self.team.as_deref(),
        }
    }

    /// The store and its own id for note `id` as its store shows it.
    fn locate(&self, origin: NoteOrigin, id: i64) -> Option<(&str, i64)> {
        match origin {
            NoteOrigin::Single => self.key(Place::Local).map(|key| (key, id)),
            NoteOrigin::Local => self.key(Place::Local).map(|key| (key, id - LOCAL_ID_OFFSET)),
            NoteOrigin::Team { .. } => self.key(Place::Remote).map(|key| (key, id)),
        }
    }

    /// Tells the embedder about `note` (shown as `note.id` by a store that says `origin`).
    pub fn upsert(&self, origin: NoteOrigin, note: &Note) {
        if !self.enabled() {
            return;
        }

        let Some((store, id)) = self.locate(origin, note.id) else {
            return;
        };

        let input = NoteInput {
            id,
            project: note.project.clone(),
            title: note.title.clone(),
            body: note.body.clone(),
            tags: note.tags.clone(),
        };

        let _ = self.client.upsert(store, &[input]);
    }

    /// Tells the embedder note `id` (shown by a store that says `origin`) is gone.
    pub fn forget(&self, origin: NoteOrigin, id: i64) {
        if !self.enabled() {
            return;
        }

        if let Some((store, id)) = self.locate(origin, id) {
            let _ = self.client.forget(store, &[id]);
        }
    }
}

/// The notes of a search.
pub(super) struct Found {
    pub hits: Vec<Hit>,
    /// A line to show before the notes.
    pub notice: Option<&'static str>,
}

/// Answers `query`: a keyword search, fused with the vector search when that is on.
pub(super) fn search(server: &mut Server, query: &SearchQuery) -> Result<Found, StoreError> {
    let limit = query.limit.clamp(1, SEARCH_LIMIT_MAX);
    let text = query.text.trim();

    let vectors = server.vectors.clone().filter(|vectors| {
        let askable = !Terms::parse(text).is_empty() && text.chars().count() <= QUERY_MAX;
        askable && vectors.enabled()
    });

    let Some(vectors) = vectors else {
        return keyword_only(server, query, None);
    };

    let targets = server.store.vector_targets(&query.view);
    let wire: Vec<(Target, i64)> = targets.iter().filter_map(|target| wire_of(&vectors, target)).collect();

    if wire.is_empty() {
        return keyword_only(server, query, None);
    }

    let asked: Vec<Target> = wire.iter().map(|(target, _)| target.clone()).collect();

    // The floor is applied here ([`closest`]): it needs the notes below it to tell a background.
    let closest = match vectors.client.search(text, &asked, VECTOR_DEPTH, -1.0) {
        Ok(found) => closest(found),
        Err(_) => {
            let notice = (!server.vector_notice).then_some(UNAVAILABLE);
            server.vector_notice = true;

            return keyword_only(server, query, notice);
        }
    };

    let wide = SearchQuery {
        limit: SEARCH_LIMIT_MAX,
        ..query.clone()
    };
    let keyword = server.store.search(&wide)?;

    // The ids as the store shows them, the closest first.
    let shown: Vec<i64> = closest
        .iter()
        .filter_map(|hit| {
            let (_, offset) = wire.iter().find(|(target, _)| target.store == hit.store)?;
            Some(hit.id + offset)
        })
        .collect();

    let missing: Vec<i64> = shown
        .iter()
        .copied()
        .filter(|id| !keyword.iter().any(|hit| hit.id == *id))
        .collect();

    // A store that cannot load them leaves the keyword results alone.
    let loaded = server.store.hits_for(query, &missing).unwrap_or_default();

    let vector: Vec<Hit> = shown
        .iter()
        .filter_map(|id| keyword.iter().chain(&loaded).find(|hit| hit.id == *id).cloned())
        .collect();

    Ok(Found {
        hits: fuse(keyword, vector, limit),
        notice: None,
    })
}

/// The notes of `found` (closest first, [`VECTOR_DEPTH`] at most) that answer the query: at least
/// [`COSINE_FLOOR`], and [`MARGIN`] above the background when [`BACKGROUND_MIN`] notes tell one.
fn closest(found: Vec<VectorHit>) -> Vec<VectorHit> {
    let background = found
        .last()
        .filter(|_| found.len() >= BACKGROUND_MIN)
        .map(|hit| hit.cosine);
    let least = background.map_or(COSINE_FLOOR, |background| COSINE_FLOOR.max(background + MARGIN));

    found.into_iter().filter(|hit| hit.cosine >= least).collect()
}

/// The keyword search of `query` alone.
fn keyword_only(server: &mut Server, query: &SearchQuery, notice: Option<&'static str>) -> Result<Found, StoreError> {
    Ok(Found {
        hits: server.store.search(query)?,
        notice,
    })
}

/// What the embedder is asked for `target`, and what its ids are shown with.
fn wire_of(vectors: &Vectors, target: &VectorTarget) -> Option<(Target, i64)> {
    let store = vectors.key(target.place)?;

    Some((
        Target {
            store: store.to_owned(),
            projects: target.projects.clone(),
        },
        target.id_offset,
    ))
}

/// Sends note `id` as it is stored now to the embedder (after a save or an update).
pub(super) fn refresh(server: &mut Server, id: i64) {
    let Some(vectors) = server.vectors.clone().filter(Vectors::enabled) else {
        return;
    };

    let Ok(notes) = server.store.peek(&View::every_repo(), &[id]) else {
        return;
    };

    if let Some(note) = notes.first() {
        vectors.upsert(server.store.origin(id, note.scope), note);
    }
}

/// Tells the embedder note `id` was deleted.
pub(super) fn forgot(server: &mut Server, id: i64) {
    if let Some(vectors) = &server.vectors {
        vectors.forget(server.store.origin(id, Scope::Global), id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosines(values: &[f32]) -> Vec<VectorHit> {
        values
            .iter()
            .enumerate()
            .map(|(index, cosine)| VectorHit {
                store: "local:test".to_owned(),
                id: index as i64,
                cosine: *cosine,
            })
            .collect()
    }

    fn kept(values: &[f32]) -> usize {
        closest(cosines(values)).len()
    }

    /// A note at `top` over a background of `rest`, twenty notes in all.
    fn crowd(top: &[f32], rest: f32) -> Vec<f32> {
        top.iter()
            .copied()
            .chain(std::iter::repeat_n(rest, 20 - top.len()))
            .collect()
    }

    #[test]
    fn a_note_standing_out_of_its_background_answers() {
        // A synonym of two words: 0.445 over a background of 0.25.
        assert_eq!(kept(&crowd(&[0.445], 0.25)), 1);
        assert_eq!(kept(&crowd(&[0.66, 0.46], 0.38)), 2);
    }

    #[test]
    fn a_topic_the_notes_do_not_cover_has_no_answer_however_high_the_cosines_are() {
        // Many notes share the vocabulary: the best reaches 0.45, but so do twenty others at 0.40.
        assert_eq!(kept(&crowd(&[0.453, 0.450], 0.394)), 0);
        assert_eq!(kept(&crowd(&[0.30, 0.30], 0.24)), 0);
    }

    #[test]
    fn a_few_notes_in_view_are_judged_by_the_floor_alone() {
        assert_eq!(kept(&[0.43, 0.41, 0.2]), 2);
        assert_eq!(kept(&[0.39, 0.2]), 0);
    }
}
