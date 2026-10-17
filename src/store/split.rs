//! One scope in the team database, the other on this computer.
//!
//! [`SplitStore`] serves the MCP tools from two stores: the team database holds one scope
//! (repository or global notes) and the local SQLite file the other, as
//! [`crate::backend::Routing`] says. A write goes to the store of its scope; a read that covers both
//! scopes asks both and merges the results.
//!
//! **Note ids.** Both stores number their notes from 1, so the local store's ids are shown with
//! [`LOCAL_ID_OFFSET`] added (local note 7 is note 1000000007) and team ids as they are, the same
//! ids teammates see. An id at or above the offset is read as a local one, any other as a team
//! one; a team id that reaches the offset cannot be told apart and is reported as an error rather
//! than routed to the wrong store. Relations only point at notes of their own scope, so a note id
//! given with a link has to belong to the store of the link's scope.
//!
//! **Merging.** A search lists the repository notes first, as a single store does, so the
//! repository store's hits come first and the global store's fill up to the limit. The relations
//! around an entity list the repository store's first too (a single store orders them by
//! distance across both scopes). Similar notes come only from the store of the note they resemble.
//! A list of notes merges the two stores' sorted pages: a page at offset N reads the first N rows
//! of both stores (in pages of [`LIST_PAGE_MAX`]) to find where it starts.
//!
//! Each store only sees its own scope: notes of the other scope left in it (written before the
//! routing changed) are not shown.

use std::cmp::Ordering;
use std::collections::VecDeque;

use crate::limits::{GET_IDS_MAX, LIST_PAGE_MAX, NEIGHBOR_NOTES_MAX, NEIGHBORS_LIMIT_MAX, SEARCH_LIMIT_MAX};
use crate::store::listing::{compare, directed, page_limit};
use crate::store::{
    Edge, Forget, Forgotten, Hit, Link, MemoryStore, Neighborhood, Note, NoteDraft, NoteListQuery, NotePage, NotePatch,
    NoteRow, NoteSort, Saved, Scope, SearchQuery, StoreError, Target, View,
};

/// Added to the local store's note ids, so they never equal a team database id.
pub const LOCAL_ID_OFFSET: i64 = 1_000_000_000;

/// The team database for one scope, the local SQLite store for the other. See the module docs.
pub struct SplitStore {
    team: Box<dyn MemoryStore>,
    local: Box<dyn MemoryStore>,
    /// The scope kept in `local`; `team` keeps the other one.
    local_scope: Scope,
}

impl std::fmt::Debug for SplitStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SplitStore")
            .field("local_scope", &self.local_scope)
            .finish_non_exhaustive()
    }
}

/// Which of the two stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Team,
    Local,
}

impl SplitStore {
    /// Keeps `local_scope` in `local` and the other scope in `team`.
    pub fn new(team: Box<dyn MemoryStore>, local: Box<dyn MemoryStore>, local_scope: Scope) -> Self {
        Self {
            team,
            local,
            local_scope,
        }
    }

    fn side_of(&self, scope: Scope) -> Side {
        if scope == self.local_scope {
            Side::Local
        } else {
            Side::Team
        }
    }

    fn side_of_target(&self, target: &Target) -> Side {
        match target {
            Target::Repo(_) => self.side_of(Scope::Repo),
            Target::Global => self.side_of(Scope::Global),
        }
    }

    /// The store of the repository scope, then the one of the global scope.
    fn order(&self) -> [Side; 2] {
        [self.side_of(Scope::Repo), self.side_of(Scope::Global)]
    }

    /// The order of `query` across both stores: by place, local before team ascending, for
    /// [`NoteSort::Place`] (each store orders its own notes by id then), else as one store orders.
    fn compare(&self, query: &NoteListQuery, a: &NoteRow, b: &NoteRow) -> Ordering {
        if query.sort != NoteSort::Place {
            return compare(query, a, b);
        }

        let team = |row: &NoteRow| self.side_of(row.scope) == Side::Team;
        directed(query, team(a).cmp(&team(b)).then(a.id.cmp(&b.id)))
    }

    fn store(&mut self, side: Side) -> &mut dyn MemoryStore {
        match side {
            Side::Team => self.team.as_mut(),
            Side::Local => self.local.as_mut(),
        }
    }

    /// The part of `view` the store `side` holds; `None` when it holds none of it.
    fn narrow(&self, view: &View, side: Side) -> Option<View> {
        let repo_side = self.side_of(Scope::Repo);

        if side == repo_side {
            view.repo.clone().map(|repo| View {
                repo: Some(repo),
                global: false,
            })
        } else {
            view.global.then_some(View {
                repo: None,
                global: true,
            })
        }
    }

    /// The notes among `ids` visible in `view`, in the order asked, counting a hit on each when
    /// `count` ([`MemoryStore::get`]) and none otherwise ([`MemoryStore::peek`]).
    fn read_notes(&mut self, view: &View, ids: &[i64], count: bool) -> Result<Vec<Note>, StoreError> {
        if ids.is_empty() || ids.len() > GET_IDS_MAX {
            return Err(StoreError::Invalid(format!("ids must list 1-{GET_IDS_MAX} note ids")));
        }

        let mut found: Vec<Note> = Vec::new();

        for side in [Side::Team, Side::Local] {
            let Some(narrowed) = self.narrow(view, side) else {
                continue;
            };

            let wanted: Vec<i64> = ids
                .iter()
                .map(|id| decode(*id))
                .filter(|(of, _)| *of == side)
                .map(|(_, raw)| raw)
                .collect();

            if wanted.is_empty() {
                continue;
            }

            let store = self.store(side);
            let read = if count {
                store.get(&narrowed, &wanted)?
            } else {
                store.peek(&narrowed, &wanted)?
            };

            for note in read {
                found.push(Note {
                    id: encode(side, note.id)?,
                    ..note
                });
            }
        }

        // In the order asked, each note once.
        let mut ordered: Vec<Note> = Vec::with_capacity(found.len());

        for id in ids {
            if ordered.iter().any(|note| note.id == *id) {
                continue;
            }

            if let Some(index) = found.iter().position(|note| note.id == *id) {
                ordered.push(found.swap_remove(index));
            }
        }

        Ok(ordered)
    }
}

/// The id shown for note `raw` of the store `side`.
fn encode(side: Side, raw: i64) -> Result<i64, StoreError> {
    let shown = match side {
        Side::Team if (..LOCAL_ID_OFFSET).contains(&raw) => Some(raw),
        Side::Team => None,
        Side::Local => raw.checked_add(LOCAL_ID_OFFSET),
    };

    shown.ok_or_else(|| {
        StoreError::Backend(format!(
            "note id {raw} is too large to tell the team database's notes from this computer's"
        ))
    })
}

/// The store and its own id for the shown note id `id`.
fn decode(id: i64) -> (Side, i64) {
    if id >= LOCAL_ID_OFFSET {
        (Side::Local, id - LOCAL_ID_OFFSET)
    } else {
        (Side::Team, id)
    }
}

/// `error` of the store `side` with its own note ids replaced by the shown ones: a conflict's id,
/// and `raw` (the id the call was given as `shown`) in a not-found message.
fn reword(error: StoreError, side: Side, ids: Option<(i64, i64)>) -> StoreError {
    match error {
        StoreError::Conflict(raw) => StoreError::Conflict(encode(side, raw).unwrap_or(raw)),
        StoreError::NotFound(message) => match ids {
            Some((raw, shown)) => {
                StoreError::NotFound(message.replace(&format!("note {raw} not"), &format!("note {shown} not")))
            }
            None => StoreError::NotFound(message),
        },
        other => other,
    }
}

/// The error of a note id no visible scope holds, as the stores word it.
fn note_missing(id: i64) -> StoreError {
    StoreError::NotFound(format!("note {id} not found in this repository or the global scope"))
}

fn encode_hits(side: Side, hits: Vec<Hit>) -> Result<Vec<Hit>, StoreError> {
    hits.into_iter()
        .map(|hit| {
            Ok(Hit {
                id: encode(side, hit.id)?,
                ..hit
            })
        })
        .collect()
}

/// One store's rows of a merged list: read [`LIST_PAGE_MAX`] at a time, ids shown.
struct Cursor {
    side: Side,
    view: View,
    rows: VecDeque<NoteRow>,
    /// Rows read so far.
    read: usize,
    /// True once the store has no more rows.
    done: bool,
}

impl Cursor {
    fn new(side: Side, view: View) -> Self {
        Self {
            side,
            view,
            rows: VecDeque::new(),
            read: 0,
            done: false,
        }
    }

    /// Reads the next rows of `query` from `store` (which is the store of [`Self::side`]).
    fn fetch(&mut self, store: &mut dyn MemoryStore, query: &NoteListQuery) -> Result<NotePage, StoreError> {
        let part = store.list_notes(&NoteListQuery {
            view: self.view.clone(),
            offset: self.read,
            limit: LIST_PAGE_MAX,
            ..query.clone()
        })?;

        self.read += part.rows.len();
        self.done = part.rows.len() < LIST_PAGE_MAX;

        for row in &part.rows {
            self.rows.push_back(NoteRow {
                id: encode(self.side, row.id)?,
                ..row.clone()
            });
        }

        Ok(part)
    }
}

/// The view no store holds anything of: asked of one store only to check the other arguments.
const NOTHING: View = View {
    repo: None,
    global: false,
};

impl MemoryStore for SplitStore {
    fn remember(&mut self, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError> {
        let side = self.side_of_target(target);
        let saved = self
            .store(side)
            .remember(target, draft)
            .map_err(|error| reword(error, side, None))?;

        Ok(Saved {
            id: encode(side, saved.id)?,
            ..saved
        })
    }

    fn update(&mut self, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError> {
        let (side, raw) = decode(id);
        let Some(view) = self.narrow(view, side) else {
            return Err(note_missing(id));
        };

        let saved = self
            .store(side)
            .update(&view, raw, patch)
            .map_err(|error| reword(error, side, Some((raw, id))))?;

        Ok(Saved { id, ..saved })
    }

    fn get(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        self.read_notes(view, ids, true)
    }

    fn peek(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        self.read_notes(view, ids, false)
    }

    fn list_notes(&mut self, query: &NoteListQuery) -> Result<NotePage, StoreError> {
        let mut cursors: Vec<Cursor> = self
            .order()
            .into_iter()
            .filter_map(|side| self.narrow(&query.view, side).map(|view| Cursor::new(side, view)))
            .collect();

        if cursors.is_empty() {
            let [first, _] = self.order();
            cursors.push(Cursor::new(first, NOTHING));
        }

        let mut page = NotePage::new([], query, Vec::new());

        for cursor in &mut cursors {
            let part = cursor.fetch(self.store(cursor.side), query)?;
            page = NotePage::new(page.counted.into_iter().chain(part.counted), query, Vec::new());
        }

        let limit = page_limit(query);
        let mut skipped = 0;

        while page.rows.len() < limit {
            for cursor in &mut cursors {
                if cursor.rows.is_empty() && !cursor.done {
                    cursor.fetch(self.store(cursor.side), query)?;
                }
            }

            let next = cursors
                .iter()
                .enumerate()
                .filter_map(|(index, cursor)| cursor.rows.front().map(|row| (index, row)))
                .min_by(|(_, a), (_, b)| self.compare(query, a, b))
                .map(|(index, _)| index);

            let Some(row) = next.and_then(|index| cursors[index].rows.pop_front()) else {
                break;
            };

            if skipped < query.offset {
                skipped += 1;
            } else {
                page.rows.push(row);
            }
        }

        Ok(page)
    }

    fn search(&mut self, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
        let limit = query.limit.clamp(1, SEARCH_LIMIT_MAX);
        let mut hits: Vec<Hit> = Vec::new();
        let mut asked = false;

        for side in self.order() {
            let Some(view) = self.narrow(&query.view, side) else {
                continue;
            };

            // A checkout search leaves the global notes out.
            if view.global && query.checkout.is_some() {
                continue;
            }

            if hits.len() >= limit {
                break;
            }

            let part = SearchQuery {
                view,
                limit: limit - hits.len(),
                ..query.clone()
            };
            hits.extend(encode_hits(side, self.store(side).search(&part)?)?);
            asked = true;
        }

        if !asked {
            let [first, _] = self.order();
            let part = SearchQuery {
                view: NOTHING,
                ..query.clone()
            };
            return encode_hits(first, self.store(first).search(&part)?);
        }

        Ok(hits)
    }

    fn similar(&mut self, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
        let (side, raw) = decode(id);
        let Some(view) = self.narrow(view, side) else {
            return Ok(Vec::new());
        };

        let hits = self.store(side).similar(&view, raw, limit)?;
        encode_hits(side, hits)
    }

    fn link(&mut self, target: &Target, link: &Link) -> Result<Edge, StoreError> {
        let side = self.side_of_target(target);

        let ids = match link.note_id {
            Some(id) => {
                let (note_side, raw) = decode(id);

                if note_side != side {
                    return Err(StoreError::NotFound(format!("note {id} not found in this scope")));
                }

                Some((raw, id))
            }
            None => None,
        };

        let own = Link {
            note_id: ids.map(|(raw, _)| raw),
            ..link.clone()
        };
        let edge = self
            .store(side)
            .link(target, &own)
            .map_err(|error| reword(error, side, ids))?;

        Ok(Edge {
            note_id: edge.note_id.map(|raw| encode(side, raw)).transpose()?,
            ..edge
        })
    }

    fn neighbors(&mut self, view: &View, entity: &str, depth: u8, limit: usize) -> Result<Neighborhood, StoreError> {
        let limit = limit.clamp(1, NEIGHBORS_LIMIT_MAX);
        let mut parts: Vec<(Side, Neighborhood)> = Vec::new();
        let mut missing: Option<StoreError> = None;

        for side in self.order() {
            let Some(view) = self.narrow(view, side) else {
                continue;
            };

            match self.store(side).neighbors(&view, entity, depth, limit) {
                Ok(part) => parts.push((side, part)),
                Err(error @ StoreError::NotFound(_)) => missing = missing.or(Some(error)),
                Err(error) => return Err(error),
            }
        }

        if parts.is_empty() {
            if let Some(error) = missing {
                return Err(error);
            }

            let [first, _] = self.order();
            return self.store(first).neighbors(&NOTHING, entity, depth, limit);
        }

        let mut merged = Neighborhood::default();
        let mut total = 0usize;

        for (side, part) in parts {
            total += part.edges.len() + part.more_edges;

            for edge in part.edges {
                merged.edges.push(Edge {
                    note_id: edge.note_id.map(|raw| encode(side, raw)).transpose()?,
                    ..edge
                });
            }

            for (raw, title) in part.notes {
                merged.notes.push((encode(side, raw)?, title));
            }
        }

        merged.edges.truncate(limit);
        merged.notes.truncate(NEIGHBOR_NOTES_MAX);
        merged.more_edges = total.saturating_sub(merged.edges.len());
        Ok(merged)
    }

    fn forget(&mut self, view: &View, target: &Target, what: &Forget) -> Result<usize, StoreError> {
        let Forget::Note(id) = what else {
            let side = self.side_of_target(target);
            return self.store(side).forget(view, target, what);
        };

        let (side, raw) = decode(*id);
        let Some(view) = self.narrow(view, side) else {
            return Err(note_missing(*id));
        };

        self.store(side)
            .forget(&view, target, &Forget::Note(raw))
            .map_err(|error| reword(error, side, Some((raw, *id))))
    }

    /// Each store deletes the scope it holds; a failure in the second leaves the first's deletes.
    fn forget_notes(&mut self, view: &View) -> Result<Forgotten, StoreError> {
        let mut forgotten = Forgotten::default();

        for side in [Side::Team, Side::Local] {
            let Some(view) = self.narrow(view, side) else {
                continue;
            };

            forgotten = forgotten + self.store(side).forget_notes(&view)?;
        }

        Ok(forgotten)
    }

    fn find_by_title(&mut self, target: &Target, title: &str) -> Result<Option<Note>, StoreError> {
        let side = self.side_of_target(target);

        match self.store(side).find_by_title(target, title)? {
            Some(note) => Ok(Some(Note {
                id: encode(side, note.id)?,
                ..note
            })),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_never_collide() {
        for raw in [1, 2, 999_999_999] {
            let team = encode(Side::Team, raw).expect("team id");
            let local = encode(Side::Local, raw).expect("local id");

            assert_ne!(team, local);
            assert_eq!(decode(team), (Side::Team, raw));
            assert_eq!(decode(local), (Side::Local, raw));
        }

        assert_eq!(encode(Side::Local, 7).expect("local id"), 1_000_000_007);
        assert!(
            encode(Side::Team, LOCAL_ID_OFFSET).is_err(),
            "a team id at the offset is refused"
        );
        assert!(encode(Side::Local, i64::MAX).is_err(), "no overflow");
    }

    #[test]
    fn not_found_messages_show_the_shown_id() {
        let error = reword(
            StoreError::NotFound("note 7 not found in this repository or the global scope".to_owned()),
            Side::Local,
            Some((7, 1_000_000_007)),
        );
        assert_eq!(
            error.to_string(),
            "note 1000000007 not found in this repository or the global scope"
        );

        let conflict = reword(StoreError::Conflict(3), Side::Local, None);
        assert!(matches!(conflict, StoreError::Conflict(1_000_000_003)));
    }
}
