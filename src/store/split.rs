//! Notes read from the team database and this computer, as the routing says.
//!
//! [`SplitStore`] serves the MCP tools and `harness-memory notes` from two stores, the team
//! database and the local SQLite file, routed per note scope by [`NotesPlace`] (see
//! [`crate::backend::Routing`]): `remote` reads and writes the team database, `local` the local
//! file, `remote+local` reads both and writes the local file. A write goes to the store its scope
//! saves to; a read that covers both stores asks both and merges the results.
//!
//! **Read-only team notes.** In a `remote+local` scope the team notes are read-only:
//! `note_update`, `note_delete` and `entity_link` with a team note id are refused
//! ([`read_only_error`]), naming `harness-memory notes hide`. In a `remote` scope they stay
//! editable.
//!
//! **Hidden team notes.** With [`HiddenNotes`] (read on every call) the team notes hidden on this
//! computer are left out of every read, in `remote` and `remote+local` scopes alike: a hidden id is
//! not found, lists and searches leave it out of their rows and counts, similar notes and the notes
//! around an entity skip it. Deleting every note never reaches past the store a scope writes to,
//! nor a hidden team note: it deletes what the list shows. Saving a note in a `remote` scope under
//! the title of a hidden team note is refused ([`hidden_title_error`]) rather than changing the
//! note nobody sees. The hidden list is matched by id alone, so at most once per
//! [`crate::store::hidden::PRUNE_EVERY`] (across processes) the rows of team notes that are gone,
//! or whose id the team database gave to a newer note (SQLite and libSQL reuse ids), are dropped
//! ([`HiddenNotes::prune_if_due`]). A read that cannot read the hidden list hides nothing, but a
//! change to the team notes (deleting every note, changing or deleting one, linking one, saving
//! one) fails instead ([`HiddenNotes::try_ids`]).
//!
//! **Note ids.** Both stores number their notes from 1, so the local store's ids are shown with
//! [`LOCAL_ID_OFFSET`] added (local note 7 is note 1000000007) and team ids as they are, the same
//! ids teammates see. An id at or above the offset is read as a local one, any other as a team
//! one; a team id that reaches the offset cannot be told apart and is reported as an error rather
//! than routed to the wrong store. Relations only point at notes of their own store, so a note id
//! given with a link has to belong to the store the link's scope writes to.
//!
//! **Merging.** A search lists the repository notes first, as a single store does: per scope, the
//! hits of the stores reading it take turns, up to the limit. The relations around an entity list
//! the repository store's first too (a single store orders them by distance across both scopes).
//! Similar notes come from the store of the note they resemble, and in a `remote+local` scope from
//! the other store too, taking turns. A list of notes merges the two stores' sorted pages: a page at
//! offset N reads the first N rows of both stores (in pages of [`LIST_PAGE_MAX`]) to find where it
//! starts. A list of the notes kept in one place reads the page from that place's store alone; the
//! other store only counts its notes (the list's counts per place).
//!
//! Each store only sees the scopes it is read for: notes of another scope left in it (written
//! before the routing changed) are not shown.

use std::cmp::Ordering;
use std::collections::VecDeque;

use crate::backend::{NotesPlace, Place};
use crate::limits::{GET_IDS_MAX, LIST_PAGE_MAX, NEIGHBOR_NOTES_MAX, NEIGHBORS_LIMIT_MAX, SEARCH_LIMIT_MAX};
use crate::store::listing::{compare, directed, page_limit, per_place};
use crate::store::{
    Edge, Forget, Forgotten, HiddenNotes, Hit, Link, MemoryStore, Neighborhood, Note, NoteDraft, NoteListQuery,
    NoteOrigin, NotePage, NotePatch, NoteRow, NoteSort, Saved, Scope, SearchQuery, StoreError, Target, View,
};

/// Added to the local store's note ids, so they never equal a team database id.
pub const LOCAL_ID_OFFSET: i64 = 1_000_000_000;

/// The team database and the local SQLite store, routed per scope. See the module docs.
pub struct SplitStore {
    team: Box<dyn MemoryStore>,
    local: Box<dyn MemoryStore>,
    repo: NotesPlace,
    global: NotesPlace,
    hidden: Option<HiddenNotes>,
}

impl std::fmt::Debug for SplitStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SplitStore")
            .field("repo", &self.repo)
            .field("global", &self.global)
            .field("hidden", &self.hidden)
            .finish_non_exhaustive()
    }
}

/// Which of the two stores (shared with [`crate::store::SplitScripts`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Team,
    Local,
}

impl Side {
    /// Where the store keeps what it holds.
    pub(crate) fn place(self) -> Place {
        match self {
            Self::Team => Place::Remote,
            Self::Local => Place::Local,
        }
    }
}

/// The note similar notes are looked for next to: one of the store asked, or the title and body of
/// a note of the other store.
#[derive(Clone, Copy)]
enum Source<'a> {
    Own(i64),
    Text((&'a str, &'a str)),
}

impl SplitStore {
    /// Keeps `local_scope` in `local` and the other scope in `team`, hiding nothing.
    pub fn new(team: Box<dyn MemoryStore>, local: Box<dyn MemoryStore>, local_scope: Scope) -> Self {
        let place = |scope: Scope| {
            if scope == local_scope {
                NotesPlace::Local
            } else {
                NotesPlace::Remote
            }
        };

        Self::routed(team, local, place(Scope::Repo), place(Scope::Global), None)
    }

    /// Routes repository notes by `repo` and global notes by `global`, leaving out the team notes
    /// `hidden` hides (none without it).
    pub fn routed(
        team: Box<dyn MemoryStore>,
        local: Box<dyn MemoryStore>,
        repo: NotesPlace,
        global: NotesPlace,
        hidden: Option<HiddenNotes>,
    ) -> Self {
        Self {
            team,
            local,
            repo,
            global,
            hidden,
        }
    }

    fn place(&self, scope: Scope) -> NotesPlace {
        match scope {
            Scope::Repo => self.repo,
            Scope::Global => self.global,
        }
    }

    /// True when the store `side` is read for the notes of `scope`.
    fn reads(&self, scope: Scope, side: Side) -> bool {
        match side {
            Side::Team => self.place(scope).reads_team(),
            Side::Local => self.place(scope).reads_local(),
        }
    }

    /// The store new notes of `scope` go to.
    fn write_side(&self, scope: Scope) -> Side {
        match self.place(scope).writes() {
            Place::Remote => Side::Team,
            Place::Local => Side::Local,
        }
    }

    /// The stores read for some scope, the ones of the repository scope first (this computer's
    /// before the team's when both are).
    fn order(&self) -> Vec<Side> {
        let mut sides: Vec<Side> = Vec::new();

        for scope in [Scope::Repo, Scope::Global] {
            for side in [Side::Local, Side::Team] {
                if self.reads(scope, side) && !sides.contains(&side) {
                    sides.push(side);
                }
            }
        }

        sides
    }

    /// The store asked to check the arguments when no store is read for the view.
    fn first(&self) -> Side {
        self.order().first().copied().unwrap_or(Side::Team)
    }

    /// The order of `query` across both stores: by place, local before team ascending, for
    /// [`NoteSort::Place`] (each store orders its own notes by id then), else as one store orders.
    fn compare(query: &NoteListQuery, a: &NoteRow, b: &NoteRow) -> Ordering {
        if query.sort != NoteSort::Place {
            return compare(query, a, b);
        }

        let team = |row: &NoteRow| decode(row.id).0 == Side::Team;
        directed(query, team(a).cmp(&team(b)).then(a.id.cmp(&b.id)))
    }

    fn store(&mut self, side: Side) -> &mut dyn MemoryStore {
        match side {
            Side::Team => self.team.as_mut(),
            Side::Local => self.local.as_mut(),
        }
    }

    /// The part of `view` the store `side` is read for; `None` when it is read for none of it.
    fn narrow(&self, view: &View, side: Side) -> Option<View> {
        narrow_by(view, |scope| self.reads(scope, side))
    }

    /// The part of `view` whose notes the store `side` saves; `None` when it saves none of it.
    fn narrow_writes(&self, view: &View, side: Side) -> Option<View> {
        narrow_by(view, |scope| self.write_side(scope) == side)
    }

    /// The ids of the team notes hidden on this computer (none without [`HiddenNotes`]), the list
    /// pruned first when it is due (see the module docs).
    fn hidden_ids(&mut self) -> Result<Vec<i64>, StoreError> {
        let Some(hidden) = self.hidden.as_mut() else {
            return Ok(Vec::new());
        };

        // A team database that cannot be read fails the call itself, so a failed prune waits for
        // the next turn.
        let _ = hidden.prune_if_due(self.team.as_mut());

        Ok(hidden.ids())
    }

    /// The ids [`Self::hidden_ids`] reads, for a change to the team notes: an error when the
    /// hidden list cannot be read, rather than none hidden.
    fn hidden_ids_strict(&mut self) -> Result<Vec<i64>, StoreError> {
        let Some(hidden) = self.hidden.as_mut() else {
            return Ok(Vec::new());
        };

        let _ = hidden.prune_if_due(self.team.as_mut());
        hidden.try_ids()
    }

    /// Refuses to save `title` in the team scope `target` when a team note with that title is
    /// hidden on this computer (saving would change it unseen).
    fn check_hidden_title(&mut self, target: &Target, title: &str) -> Result<(), StoreError> {
        let hidden = self.hidden_ids_strict()?;

        if hidden.is_empty() {
            return Ok(());
        }

        match self.team.find_by_title(target, title)? {
            Some(note) if hidden.contains(&note.id) => Err(hidden_title_error(note.id)),
            _ => Ok(()),
        }
    }

    /// The ids `side` leaves out of its reads: the hidden ones for the team store, and those of
    /// `asked` (shown ids) that are its own.
    fn excluded(&mut self, side: Side, asked: &[i64]) -> Result<Vec<i64>, StoreError> {
        let hidden = match side {
            Side::Team => self.hidden_ids()?,
            Side::Local => Vec::new(),
        };

        Ok(with_asked(hidden, side, asked))
    }

    /// Refuses a change to the team note `raw` (shown as `id`) visible in `view` (the team store's
    /// part) when it is hidden (not found) or its scope is `remote+local` (read-only).
    fn check_team_write(&mut self, view: &View, raw: i64, id: i64) -> Result<(), StoreError> {
        if self.hidden_ids_strict()?.contains(&raw) {
            return Err(note_missing(id));
        }

        let (repo, global) = (self.repo, self.global);
        let read_only = move |scope: Scope| {
            let place = if scope == Scope::Repo { repo } else { global };
            place == NotesPlace::RemoteLocal
        };
        let in_view = (view.repo.is_some() && read_only(Scope::Repo)) || (view.global && read_only(Scope::Global));

        if !in_view {
            return Ok(());
        }

        let Some(note) = self.team.peek(view, &[raw])?.into_iter().next() else {
            return Err(note_missing(id));
        };

        if read_only(note.scope) {
            return Err(read_only_error(id, note.scope));
        }

        Ok(())
    }

    /// The notes among `ids` visible in `view`, in the order asked, counting a hit on each when
    /// `count` ([`MemoryStore::get`]) and none otherwise ([`MemoryStore::peek`]).
    fn read_notes(&mut self, view: &View, ids: &[i64], count: bool) -> Result<Vec<Note>, StoreError> {
        if ids.is_empty() || ids.len() > GET_IDS_MAX {
            return Err(StoreError::Invalid(format!("ids must list 1-{GET_IDS_MAX} note ids")));
        }

        let hidden = self.hidden_ids()?;
        let mut found: Vec<Note> = Vec::new();

        for side in [Side::Team, Side::Local] {
            let Some(narrowed) = self.narrow(view, side) else {
                continue;
            };

            let wanted: Vec<i64> = ids
                .iter()
                .map(|id| decode(*id))
                .filter(|(of, raw)| *of == side && !(side == Side::Team && hidden.contains(raw)))
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

    /// The page of `query` the store `side` lists alone, for `view` and leaving out `exclude`, ids
    /// shown.
    fn page_of(
        &mut self,
        side: Side,
        query: &NoteListQuery,
        view: View,
        exclude: Vec<i64>,
    ) -> Result<NotePage, StoreError> {
        let mut page = self.store(side).list_notes(&NoteListQuery {
            view,
            place: None,
            exclude,
            ..query.clone()
        })?;

        for row in &mut page.rows {
            row.id = encode(side, row.id)?;
        }

        Ok(page)
    }

    /// Up to `limit` hits of `side` for `query` in `view`, the hidden ones left out, ids shown.
    fn search_side(
        &mut self,
        side: Side,
        query: &SearchQuery,
        view: View,
        limit: usize,
    ) -> Result<Vec<Hit>, StoreError> {
        let part = SearchQuery {
            view,
            limit,
            exclude: self.excluded(side, &query.exclude)?,
            ..query.clone()
        };

        encode_hits(side, self.store(side).search(&part)?)
    }

    /// Up to `limit` notes in `view` of the store `side` resembling `source`, the hidden ones left
    /// out, ids shown.
    fn similar_side(
        &mut self,
        side: Side,
        view: &View,
        source: Source<'_>,
        limit: usize,
    ) -> Result<Vec<Hit>, StoreError> {
        let excluded = self.excluded(side, &[])?;
        let asked = limit.saturating_add(excluded.len());

        let hits = match source {
            Source::Own(raw) => self.store(side).similar(view, raw, asked)?,
            Source::Text(text) => self.store(side).similar_text(view, text, asked)?,
        };

        let kept: Vec<Hit> = hits
            .into_iter()
            .filter(|hit| !excluded.contains(&hit.id))
            .take(limit)
            .collect();
        encode_hits(side, kept)
    }
}

/// `ids` (the store `side`'s own) with those of `asked` (shown ids) that are `side`'s, each once.
fn with_asked(mut ids: Vec<i64>, side: Side, asked: &[i64]) -> Vec<i64> {
    for (of, raw) in asked.iter().map(|id| decode(*id)) {
        if of == side && !ids.contains(&raw) {
            ids.push(raw);
        }
    }

    ids
}

/// The scopes of `view` that `keep` keeps; `None` when it keeps none.
pub(crate) fn narrow_by(view: &View, keep: impl Fn(Scope) -> bool) -> Option<View> {
    let repo = view.repo.clone().filter(|_| keep(Scope::Repo));
    let global = view.global && keep(Scope::Global);

    (repo.is_some() || global).then_some(View { repo, global })
}

/// The scope `target` writes to.
pub(crate) fn target_scope(target: &Target) -> Scope {
    match target {
        Target::Repo(_) => Scope::Repo,
        Target::Global => Scope::Global,
    }
}

/// The other store.
fn other(side: Side) -> Side {
    match side {
        Side::Team => Side::Local,
        Side::Local => Side::Team,
    }
}

/// The id shown for note `raw` of the store `side`.
fn encode(side: Side, raw: i64) -> Result<i64, StoreError> {
    encode_as(side, raw, "note")
}

/// The id shown for the `noun` (note, script) `raw` of the store `side`: the local store's with
/// [`LOCAL_ID_OFFSET`] added, the team's as it is (an error when it reaches the offset).
pub(crate) fn encode_as(side: Side, raw: i64, noun: &str) -> Result<i64, StoreError> {
    let shown = match side {
        Side::Team if (..LOCAL_ID_OFFSET).contains(&raw) => Some(raw),
        Side::Team => None,
        Side::Local => raw.checked_add(LOCAL_ID_OFFSET),
    };

    shown.ok_or_else(|| {
        StoreError::Backend(format!(
            "{noun} id {raw} is too large to tell the team database's {noun}s from this computer's"
        ))
    })
}

/// The store and its own id for the shown id `id` (of a note or a script).
pub(crate) fn decode(id: i64) -> (Side, i64) {
    if id >= LOCAL_ID_OFFSET {
        (Side::Local, id - LOCAL_ID_OFFSET)
    } else {
        (Side::Team, id)
    }
}

/// True when the shown note or script id `id` is one of this computer's ([`LOCAL_ID_OFFSET`] or
/// above).
pub fn is_local_id(id: i64) -> bool {
    decode(id).0 == Side::Local
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

/// The refusal to save a note under the title of the hidden team note `id`.
pub fn hidden_title_error(id: i64) -> StoreError {
    StoreError::Invalid(format!(
        "a hidden team note has this title (note {id}): unhide it with harness-memory notes unhide {id}, or pick \
         another title"
    ))
}

/// The refusal to change team note `id` of a `remote+local` scope.
pub fn read_only_error(id: i64, scope: Scope) -> StoreError {
    let key = match scope {
        Scope::Repo => "repository_notes",
        Scope::Global => "global_notes",
    };

    StoreError::Invalid(format!(
        "note {id} is a team note and read-only here: {key} is \"remote+local\", which reads the team's notes \
         and saves new notes on this computer only. Save a new note instead, or stop seeing this one with \
         harness-memory notes hide {id} (Hide on this computer in Harness Desktop)"
    ))
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

/// `parts` taking turns (the first of each, then the second of each, …), at most `limit`.
fn take_turns(parts: Vec<Vec<Hit>>, limit: usize) -> Vec<Hit> {
    let mut queues: Vec<VecDeque<Hit>> = parts.into_iter().map(VecDeque::from).collect();
    let mut merged: Vec<Hit> = Vec::new();

    while merged.len() < limit && queues.iter().any(|queue| !queue.is_empty()) {
        for queue in &mut queues {
            if merged.len() >= limit {
                break;
            }

            if let Some(hit) = queue.pop_front() {
                merged.push(hit);
            }
        }
    }

    merged
}

/// One store's rows of a merged list: read [`LIST_PAGE_MAX`] at a time, ids shown.
struct Cursor {
    side: Side,
    view: View,
    /// The ids the store leaves out (the hidden ones, for the team store).
    exclude: Vec<i64>,
    rows: VecDeque<NoteRow>,
    /// Rows read so far.
    read: usize,
    /// True once the store has no more rows.
    done: bool,
}

impl Cursor {
    fn new(side: Side, view: View, exclude: Vec<i64>) -> Self {
        Self {
            side,
            view,
            exclude,
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
            place: None,
            exclude: self.exclude.clone(),
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
        let side = self.write_side(target_scope(target));

        if side == Side::Team {
            self.check_hidden_title(target, &draft.title)?;
        }

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

        if side == Side::Team {
            self.check_team_write(&view, raw, id)?;
        }

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
        let mut cursors: Vec<Cursor> = Vec::new();
        let mut places: Vec<(Place, usize)> = Vec::new();

        for side in self.order() {
            let Some(view) = self.narrow(&query.view, side) else {
                continue;
            };

            let exclude = self.excluded(side, &query.exclude)?;

            if query.place.is_none_or(|place| place == side.place()) {
                cursors.push(Cursor::new(side, view, exclude));
            } else {
                // The store of the other place is not listed, only counted.
                let counted = NoteListQuery {
                    offset: 0,
                    limit: 1,
                    ..query.clone()
                };
                let total = self.page_of(side, &counted, view, exclude)?.total;
                places.push((side.place(), total));
            }
        }

        if cursors.is_empty() {
            let page = self.page_of(self.first(), query, NOTHING, Vec::new())?;
            return Ok(NotePage {
                places: per_place(places),
                ..page
            });
        }

        // One store for the whole view pages in the database, as a single store does.
        if let [only] = cursors.as_slice() {
            let page = self.page_of(only.side, query, only.view.clone(), only.exclude.clone())?;
            places.push((only.side.place(), page.total));

            return Ok(NotePage {
                places: per_place(places),
                ..page
            });
        }

        let mut page = NotePage::new([], query, Vec::new());

        for cursor in &mut cursors {
            let part = cursor.fetch(self.store(cursor.side), query)?;
            places.push((cursor.side.place(), part.total));
            page = NotePage::new(page.counted.into_iter().chain(part.counted), query, Vec::new());
        }

        page.places = per_place(places);

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
                .min_by(|(_, a), (_, b)| Self::compare(query, a, b))
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
        let readers: Vec<(Side, View)> = self
            .order()
            .into_iter()
            .filter_map(|side| self.narrow(&query.view, side).map(|view| (side, view)))
            .collect();

        // One store for the whole view ranks it as a single store does.
        if let [(side, view)] = readers.as_slice() {
            return self.search_side(*side, query, view.clone(), limit);
        }

        let mut hits: Vec<Hit> = Vec::new();
        let mut asked = false;

        for scope in [Scope::Repo, Scope::Global] {
            // A checkout search leaves the global notes out.
            if scope == Scope::Global && query.checkout.is_some() {
                continue;
            }

            let Some(view) = narrow_by(&query.view, |kept| kept == scope) else {
                continue;
            };

            if hits.len() >= limit {
                break;
            }

            let room = limit - hits.len();
            let mut parts: Vec<Vec<Hit>> = Vec::new();

            for side in [Side::Local, Side::Team] {
                if self.reads(scope, side) {
                    parts.push(self.search_side(side, query, view.clone(), room)?);
                    asked = true;
                }
            }

            hits.extend(take_turns(parts, room));
        }

        if !asked {
            return self.search_side(self.first(), query, NOTHING, limit);
        }

        Ok(hits)
    }

    fn similar(&mut self, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
        let (side, raw) = decode(id);

        if side == Side::Team && self.hidden_ids()?.contains(&raw) {
            return Ok(Vec::new());
        }

        let Some(own) = self.narrow(view, side) else {
            return Ok(Vec::new());
        };

        let mut parts = vec![self.similar_side(side, &own, Source::Own(raw), limit)?];

        // In a `remote+local` scope the other store holds notes of the same scope too.
        let shared = narrow_by(view, |scope| self.place(scope) == NotesPlace::RemoteLocal);

        if let Some(shared) = shared {
            let note = self.store(side).peek(&own, &[raw])?.into_iter().next();
            let note = note.filter(|note| self.place(note.scope) == NotesPlace::RemoteLocal);

            if let Some(note) = note {
                let source = Source::Text((&note.title, &note.body));
                parts.push(self.similar_side(other(side), &shared, source, limit)?);
            }
        }

        Ok(take_turns(parts, limit))
    }

    fn link(&mut self, target: &Target, link: &Link) -> Result<Edge, StoreError> {
        let scope = target_scope(target);
        let side = self.write_side(scope);

        let ids = match link.note_id {
            Some(id) => {
                let (note_side, raw) = decode(id);
                let team_note = note_side == Side::Team;

                if note_side != side && team_note && self.reads(scope, Side::Team) {
                    return Err(read_only_error(id, scope));
                }

                if note_side != side || (team_note && self.hidden_ids_strict()?.contains(&raw)) {
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

            let first = self.first();
            return self.store(first).neighbors(&NOTHING, entity, depth, limit);
        }

        let hidden = self.hidden_ids()?;
        let shown = |side: Side, raw: i64| !(side == Side::Team && hidden.contains(&raw));
        let mut merged = Neighborhood::default();
        let mut total = 0usize;

        for (side, part) in parts {
            total += part.edges.len() + part.more_edges;

            for edge in part.edges {
                let note_id = edge.note_id.filter(|raw| shown(side, *raw));

                merged.edges.push(Edge {
                    note_id: note_id.map(|raw| encode(side, raw)).transpose()?,
                    ..edge
                });
            }

            for (raw, title) in part.notes {
                if shown(side, raw) {
                    merged.notes.push((encode(side, raw)?, title));
                }
            }
        }

        merged.edges.truncate(limit);
        merged.notes.truncate(NEIGHBOR_NOTES_MAX);
        merged.more_edges = total.saturating_sub(merged.edges.len());
        Ok(merged)
    }

    fn forget(&mut self, view: &View, target: &Target, what: &Forget) -> Result<usize, StoreError> {
        let Forget::Note(id) = what else {
            let side = self.write_side(target_scope(target));
            return self.store(side).forget(view, target, what);
        };

        let (side, raw) = decode(*id);
        let Some(view) = self.narrow(view, side) else {
            return Err(note_missing(*id));
        };

        if side == Side::Team {
            self.check_team_write(&view, raw, *id)?;
        }

        self.store(side)
            .forget(&view, target, &Forget::Note(raw))
            .map_err(|error| reword(error, side, Some((raw, *id))))
    }

    /// Each store deletes the scopes it saves (a `remote+local` scope's team notes stay), and the
    /// team store leaves the hidden notes too (nothing is deleted there when the hidden list cannot
    /// be read); a failure in the second leaves the first's deletes.
    fn forget_notes(&mut self, view: &View, exclude: &[i64]) -> Result<Forgotten, StoreError> {
        let mut forgotten = Forgotten::default();

        for side in [Side::Team, Side::Local] {
            let Some(view) = self.narrow_writes(view, side) else {
                continue;
            };

            let hidden = match side {
                Side::Team => self.hidden_ids_strict()?,
                Side::Local => Vec::new(),
            };
            let left = with_asked(hidden, side, exclude);
            forgotten = forgotten + self.store(side).forget_notes(&view, &left)?;
        }

        Ok(forgotten)
    }

    fn find_by_title(&mut self, target: &Target, title: &str) -> Result<Option<Note>, StoreError> {
        let side = self.write_side(target_scope(target));

        match self.store(side).find_by_title(target, title)? {
            Some(note) => Ok(Some(Note {
                id: encode(side, note.id)?,
                ..note
            })),
            None => Ok(None),
        }
    }

    fn origin(&self, id: i64, scope: Scope) -> NoteOrigin {
        match decode(id).0 {
            Side::Local => NoteOrigin::Local,
            Side::Team => NoteOrigin::Team {
                read_only: self.place(scope) == NotesPlace::RemoteLocal,
            },
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

    #[test]
    fn hits_take_turns_up_to_the_limit() {
        let hit = |id: i64| Hit {
            id,
            kind: crate::store::NoteKind::Note,
            scope: Scope::Repo,
            title: String::new(),
            snippet: String::new(),
            updated_at: 0,
            entities: Vec::new(),
            provenance: crate::store::Provenance::default(),
        };
        let ids = |hits: Vec<Hit>| hits.into_iter().map(|hit| hit.id).collect::<Vec<_>>();

        let parts = || vec![vec![hit(1), hit(2), hit(3)], vec![hit(10)]];
        assert_eq!(ids(take_turns(parts(), 3)), [1, 10, 2]);
        assert_eq!(ids(take_turns(parts(), 9)), [1, 10, 2, 3]);
        assert!(take_turns(Vec::new(), 3).is_empty());
    }
}
