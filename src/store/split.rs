//! Notes read from the team database and this computer, as the routing says.
//!
//! [`SplitStore`] serves the MCP tools and `harness-memory notes` from two stores, the team
//! database and the local SQLite file, routed per note scope by [`NotesPlace`] (see
//! [`crate::backend::Routing`]): `remote` reads and writes the team database, `local` the local
//! file, `remote+local` reads both and writes the place [`SplitStore::saving_to`] names
//! (`remote_local_saves`, the team database by default). A write goes to the store its scope saves
//! to; a read that covers both stores asks both and merges the results.
//!
//! **Read-only team notes.** In a `remote+local` scope that saves to this computer the team notes are
//! read-only: `note_update`, `note_delete` and `entity_link` with a team note id are refused
//! ([`read_only_error`]), naming `harness-memory notes hide`. In a `remote` scope, and in a
//! `remote+local` scope that saves to the team database, they stay editable.
//!
//! **Picking the place.** In a `remote+local` scope [`MemoryStore::remember_to`] saves a note to
//! the place the user picked (`notes save --place`), whatever `remote_local_saves` says: `local`
//! straight to this computer, `remote` straight to the team database with the hidden title check and
//! the store's own rules. Neither falls back, and neither records anything as waiting: a note the
//! person saved here stays here, and an unreachable team database fails a save to it.
//!
//! **Notes kept here.** In a `remote+local` scope that saves to the team database, a save without a
//! place under a title a note on this computer already has (the person kept it here, or an agent
//! saved it offline) updates that note where it is, online or not, and lists nothing as waiting: only
//! a genuinely new title follows `remote_local_saves`, and only the row an offline save created is
//! listed. A scope set to `remote` lists whatever it saves offline. `--place local` takes the note it
//! saves off the list if it was on it.
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
//! **Merging.** A search for words ranks the hits of the stores by score ([`Hit::score_milli`]),
//! repository and global notes together, as a single store does (see `store::rank` for
//! what the score is and why scores of two databases only roughly compare). The recent notes of an
//! empty query list the repository first: per scope, the hits of the stores reading it take
//! turns, up to the limit. The relations around an entity list the repository store's first too (a single store orders them by distance across both scopes).
//! Similar notes come from the store of the note they resemble, and in a `remote+local` scope from
//! the other store too, taking turns. A list of notes merges the two stores' sorted pages: a page at
//! offset N reads the first N rows of both stores (in pages of [`LIST_PAGE_MAX`]) to find where it
//! starts. A list of the notes kept in one place reads the page from that place's store alone; the
//! other store only counts its notes (the list's counts per place). The list's author facet is the
//! sum of the facets of the stores listed.
//!
//! Each store only sees the scopes it is read for: notes of another scope left in it (written
//! before the routing changed) are not shown.
//!
//! **Offline fallback.** With [`SplitStore::with_offline_fallback`], scopes that read the team
//! database are served from this computer while it is unreachable, whether it could not be opened or
//! its connection was lost during a call (`SplitStore::attempt`: the call is made once more,
//! now locally). In a scope that saves to the team database (`remote`, or `remote+local` with
//! `remote_local_saves` `remote`: [`SplitStore::team_writes`]) what is saved meanwhile waits in
//! [`crate::store::pending`] to be copied back, and a change to a note the team database holds waits
//! in [`crate::store::pending_updates`] to be applied (`updates`); deleting or linking such a note is
//! refused, saying nothing was changed. Only what is listed as waiting is copied: a note a person
//! kept on this computer (`--place local`, or a scope that saves here) is not listed and stays.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::backend::{NotesPlace, Place, Secrets};
use crate::limits::{GET_IDS_MAX, LIST_PAGE_MAX, NEIGHBOR_NOTES_MAX, NEIGHBORS_LIMIT_MAX, SEARCH_LIMIT_MAX};
use crate::push::{GLOBAL, Options, push_only, scope_label};
use crate::store::listing::{compare, directed, page_limit, per_place};
use crate::store::pending::{Entry, Kind, Pending};
use crate::store::pending_updates::Queue;
use crate::store::rank::Terms;
use crate::store::{
    AuthorFacet, Countable, Edge, Forget, Forgotten, HiddenNotes, Hit, Link, MemoryStore, Neighborhood, Note,
    NoteDraft, NoteListQuery, NoteOrigin, NotePage, NotePatch, NoteRow, NoteSort, Saved, Scope, SearchQuery,
    StoreError, Target, VectorTarget, View, clean,
};

mod updates;

/// How long a copy of the notes saved offline that failed is not tried again.
const RETRY_COPY: Duration = Duration::from_secs(60);

/// Added to the local store's note ids, so they never equal a team database id.
pub const LOCAL_ID_OFFSET: i64 = 1_000_000_000;

/// The team database and the local SQLite store, routed per scope. See the module docs.
pub struct SplitStore {
    team: Box<dyn MemoryStore>,
    local: Box<dyn MemoryStore>,
    repo: NotesPlace,
    global: NotesPlace,
    /// Where new notes of a scope routed `remote+local` are saved.
    saves: Place,
    hidden: Option<HiddenNotes>,
    fallback: Option<Fallback>,
    /// True while the offline fallback serves the scopes that read the team database from this
    /// computer.
    down: bool,
}

/// The offline fallback of a [`SplitStore`], see [`SplitStore::with_offline_fallback`].
struct Fallback {
    /// The notes saved on this computer for a scope set to `remote`.
    pending: Pending,
    /// The changes to team notes made while the team database was unreachable, waiting to be
    /// applied to it.
    updates: Queue,
    /// The local database those notes are copied from.
    database: PathBuf,
    /// The team database the notes are meant for ([`crate::backend::Backend::team_key`]).
    team: String,
    /// Masks the database credentials out of what is printed.
    secrets: Secrets,
    /// Whether the pending notes are copied when the team database is next reachable.
    sync_due: bool,
    /// Not before this time: a copy that failed is tried again after a while.
    retry_at: Instant,
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
            saves: Place::default(),
            hidden,
            fallback: None,
            down: false,
        }
    }

    /// The store saving new notes of a scope routed `remote+local` to `saves` (`remote_local_saves`;
    /// the team database without this). The team's notes of such a scope are editable only when
    /// that is the team database.
    pub fn saving_to(mut self, saves: Place) -> Self {
        self.saves = saves;
        self
    }

    /// The store falling back to this computer while the team database cannot be opened: the
    /// scopes that read the team database are then read and saved in the local database `database`
    /// (the team notes are not shown), the notes saved for a scope set to `remote` are listed in
    /// [`Pending`], and they are copied to the team database when it is reachable again (at the
    /// next call, or when a later server starts). A change to a team note made meanwhile is kept in
    /// the [`Queue`] and applied then too. A team database that was reached and is lost
    /// during a call is told apart as well: the call is made again from this computer
    /// (`attempt`). Without the fallback an unreachable team database fails the calls that
    /// need it.
    pub fn with_offline_fallback(mut self, database: &Path, team: String, secrets: Secrets) -> Self {
        self.fallback = Some(Fallback {
            pending: Pending::beside(database),
            updates: Queue::beside(database),
            database: database.to_owned(),
            team,
            secrets,
            sync_due: true,
            retry_at: Instant::now(),
        });
        self
    }

    /// Where the notes of `scope` are kept as the settings say.
    fn configured(&self, scope: Scope) -> NotesPlace {
        match scope {
            Scope::Repo => self.repo,
            Scope::Global => self.global,
        }
    }

    /// True when new notes of `scope` go to the team database and its notes can be changed from here:
    /// a scope set to `remote`, or to `remote+local` while the new notes are saved to the team
    /// database. Only then do notes saved offline wait for the team database.
    fn team_writes(&self, scope: Scope) -> bool {
        self.configured(scope).writes_team(self.saves)
    }

    /// True when the team's notes of `scope` can be read but not changed from here.
    fn team_read_only(&self, scope: Scope) -> bool {
        self.configured(scope).team_read_only(self.saves)
    }

    /// Where the notes of `scope` are read and saved now: the local database alone while the
    /// offline fallback is serving.
    fn place(&self, scope: Scope) -> NotesPlace {
        match self.configured(scope) {
            NotesPlace::Remote | NotesPlace::RemoteLocal if self.down => NotesPlace::Local,
            place => place,
        }
    }

    /// True when some scope reads the team database and the offline fallback serves meanwhile.
    fn falls_back(&self) -> bool {
        self.fallback.is_some()
            && [Scope::Repo, Scope::Global]
                .into_iter()
                .any(|scope| self.configured(scope).reads_team())
    }

    /// True when some scope of `view` reads the team database.
    fn reads_team_in(&self, view: &View) -> bool {
        narrow_by(view, |scope| self.configured(scope).reads_team()).is_some()
    }

    /// Serves from this computer from now on: the team database is not reachable.
    fn go_down(&mut self) {
        if !self.down {
            self.down = true;
            eprintln!(
                "harness-memory: the team database is unreachable: notes are read and saved on this computer \
                 until it is back"
            );
        }

        if let Some(fallback) = self.fallback.as_mut() {
            fallback.sync_due = true;
        }
    }

    /// Runs `call`; when the team database turns out to be unreachable during it (a connection that
    /// worked when the call began and dropped since), the connection is dropped, so the calls
    /// until the cooldown ends fail at once, and with the offline fallback the call is made once
    /// more, now served from this computer (and recorded for the team database as an offline
    /// call is). Without the fallback the call fails, with what to change when the store knows,
    /// and the next call connects again. A failure of the database that answered is not repeated.
    fn attempt<T>(&mut self, call: impl Fn(&mut Self) -> Result<T, StoreError>) -> Result<T, StoreError> {
        let reason = match call(self) {
            // Already serving from this computer: the team database is not what failed.
            Err(StoreError::Unreachable(reason)) if !self.down => reason,
            other => return other,
        };

        let falls_back = self.falls_back();
        let message = self.team.connection_lost(&reason, falls_back);

        if !falls_back {
            return Err(StoreError::Unreachable(message));
        }

        self.go_down();
        call(self)
    }

    /// Decides before a call whether the team database is reachable (a failed open is not tried
    /// again for a while), and copies the notes saved offline, then applies the changes queued
    /// meanwhile, when it is back.
    fn settle(&mut self) {
        if !self.falls_back() {
            return;
        }

        let up = self.team.ready();

        if !up {
            self.go_down();
            return;
        }

        self.down = false;

        let Some(fallback) = self.fallback.as_mut() else {
            return;
        };

        if fallback.sync_due && Instant::now() >= fallback.retry_at {
            fallback.sync_due = false;
            self.copy_offline_notes();
            self.replay_offline_updates();
        }
    }

    /// Copies the notes saved offline to the team database; the ones that were not copied stay
    /// listed and are tried again after a while. A note the team database now holds as it is here
    /// is deleted from this computer, while one it holds differently (a teammate's newer text, or a
    /// text the team kept) stays here for the person to decide, as `harness-memory push` leaves it.
    fn copy_offline_notes(&mut self) {
        let Some(mut fallback) = self.fallback.take() else {
            return;
        };

        // An entry whose note is gone from this computer (deleted, discarded, or removed after a
        // copy that stopped before it was taken off) waits for nothing.
        let listed = fallback.pending.entries(Kind::Notes, &fallback.team);
        let listed = self.without_gone_notes(&fallback, listed);

        // Only what is meant for this team database and for a scope that still saves there.
        let waiting: Vec<Entry> = listed
            .into_iter()
            .filter(|entry| {
                let scope = if entry.scope == GLOBAL {
                    Scope::Global
                } else {
                    Scope::Repo
                };
                self.team_writes(scope)
            })
            .collect();

        if !waiting.is_empty() && !self.push_offline_notes(&fallback, &waiting) {
            fallback.sync_due = true;
            fallback.retry_at = Instant::now() + RETRY_COPY;
        }

        self.fallback = Some(fallback);
    }

    /// `entries` without those whose local note is gone, which are taken off the list too. An entry
    /// whose note is there under another title stays, and so does everything when the notes cannot
    /// be asked: nothing is dropped unseen. One read of up to [`GET_IDS_MAX`] notes per few entries.
    fn without_gone_notes(&mut self, fallback: &Fallback, entries: Vec<Entry>) -> Vec<Entry> {
        let wanted: BTreeSet<i64> = entries.iter().map(|entry| entry.id).collect();
        let wanted: Vec<i64> = wanted.into_iter().collect();
        let mut here: BTreeSet<i64> = BTreeSet::new();

        for chunk in wanted.chunks(GET_IDS_MAX) {
            match self.local.peek(&View::every_repo(), chunk) {
                Ok(notes) => here.extend(notes.iter().map(|note| note.id)),
                Err(_) => return entries,
            }
        }

        let (kept, gone): (Vec<Entry>, Vec<Entry>) = entries.into_iter().partition(|entry| here.contains(&entry.id));

        if !gone.is_empty()
            && let Err(error) = fallback.pending.remove(Kind::Notes, &gone)
        {
            eprintln!("harness-memory: {}", fallback.secrets.scrub(&error));
        }

        kept
    }

    /// Copies `waiting` to the team database; false when something has to be tried again.
    fn push_offline_notes(&mut self, fallback: &Fallback, waiting: &[Entry]) -> bool {
        let secrets = &fallback.secrets;

        let hidden = match self.hidden.as_mut().map(HiddenNotes::try_ids) {
            Some(Ok(hidden)) => hidden,
            Some(Err(error)) => {
                eprintln!(
                    "harness-memory: notes saved offline were not copied to the team database: {}",
                    secrets.scrub(&error.to_string())
                );
                return false;
            }
            None => Vec::new(),
        };

        let options = Options {
            skip_global: !self.team_writes(Scope::Global),
            skip_repositories: !self.team_writes(Scope::Repo),
            delete_copied: true,
            ..Options::default()
        };

        let summary = match push_only(&fallback.database, self.team.as_mut(), &hidden, &options, waiting) {
            Ok(summary) => summary,
            Err(error) => {
                eprintln!(
                    "harness-memory: notes saved offline were not copied to the team database: {}",
                    secrets.scrub(&error.to_string())
                );
                return false;
            }
        };

        // A note that failed stays listed, and so does everything when a relation failed (it
        // names no note); one that is gone from this computer leaves the list.
        let mut kept: BTreeSet<i64> = summary
            .changes
            .iter()
            .filter(|change| change.failed)
            .map(|change| change.local_id)
            .collect();
        kept.extend(summary.failed.iter().filter_map(|failure| failure.local_id));
        let relation_failed = summary.failed.iter().any(|failure| failure.local_id.is_none());

        // Only a note the push took leaves the list: one whose title was changed behind the list's
        // back stays listed rather than being dropped unseen.
        let matched: BTreeSet<i64> = summary.changes.iter().map(|change| change.local_id).collect();

        let done: Vec<Entry> = waiting
            .iter()
            .filter(|entry| !relation_failed && matched.contains(&entry.id) && !kept.contains(&entry.id))
            .cloned()
            .collect();

        if let Err(error) = fallback.pending.remove(Kind::Notes, &done) {
            eprintln!("harness-memory: {}", secrets.scrub(&error));
        }

        let copied = summary
            .changes
            .iter()
            .filter(|change| !change.failed && change.action.copies())
            .count();

        if copied > 0 {
            eprintln!("harness-memory: copied {copied} note(s) saved offline to the team database");
        }

        for failure in &summary.failed {
            eprintln!("harness-memory: {}", secrets.scrub(&failure.message));
        }

        kept.is_empty() && !relation_failed
    }

    /// Keeps the pending entry of the local note `raw` matching it after its title became `title`
    /// while the team database was still unreachable.
    fn rename_offline(&self, saved: &Saved, raw: i64, title: &str) {
        let Some(fallback) = &self.fallback else {
            return;
        };

        let scope = saved.project.clone().unwrap_or_else(|| GLOBAL.to_owned());
        let name = clean::title(title).unwrap_or_else(|_| title.to_owned());

        if let Err(error) = fallback.pending.rename(Kind::Notes, &fallback.team, raw, &scope, &name) {
            eprintln!("harness-memory: {}", fallback.secrets.scrub(&error));
        }
    }

    /// Gives the entry of the local note `id` saved in the scope of `target` the title `title`, when the
    /// note is listed as waiting for the team database.
    fn rename_listed(&self, target: &Target, id: i64, title: &str) {
        let Some(fallback) = &self.fallback else {
            return;
        };

        let name = clean::title(title).unwrap_or_else(|_| title.to_owned());

        if let Err(error) = fallback
            .pending
            .rename(Kind::Notes, &fallback.team, id, &scope_label(target), &name)
        {
            eprintln!("harness-memory: {}", fallback.secrets.scrub(&error));
        }
    }

    /// Lists the local note `id` titled `title`, saved in the scope of `target` (which saves to the team database)
    /// while the team database was unreachable, as waiting to be copied (a note listed already stays
    /// listed once). False when the list could not be changed: the note stays here, unlisted.
    fn record_offline(&self, target: &Target, id: i64, title: &str) -> bool {
        let Some(fallback) = &self.fallback else {
            return true;
        };

        let entry = Entry {
            team: fallback.team.clone(),
            id,
            scope: scope_label(target),
            name: clean::title(title).unwrap_or_else(|_| title.to_owned()),
        };

        if let Err(error) = fallback.pending.add(Kind::Notes, entry) {
            eprintln!(
                "harness-memory: the note was saved here but could not be listed for copying: {}",
                fallback.secrets.scrub(&error)
            );

            return false;
        }

        true
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
        match self.place(scope).writes(self.saves) {
            Place::Remote => Side::Team,
            Place::Local => Side::Local,
        }
    }

    /// The stores some scope saves new notes to, the team database first: where the English notes
    /// setting lives.
    fn settings_sides(&self) -> Vec<Side> {
        let writes = |side: Side| {
            [Scope::Repo, Scope::Global]
                .into_iter()
                .any(|scope| self.write_side(scope) == side)
        };

        [Side::Team, Side::Local]
            .into_iter()
            .filter(|side| writes(*side))
            .collect()
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

    /// The shown ids `ids` that a counter may reach, as [`ids_by_side`] splits them. A counter
    /// never settles ([`Self::settle`] may open the team database or copy the notes saved
    /// offline, which no counter may wait for): the call that queued it did. The team notes are
    /// left out while the team database is down, hidden on this computer, or while the hidden list
    /// cannot be read (a hidden note must not be counted as shown). The list is read as it is, never
    /// pruned ([`Self::hidden_ids_unpruned`]).
    fn countable_by_side(&mut self, ids: &[i64]) -> Vec<(Side, Vec<i64>)> {
        let mut sides = ids_by_side(ids);

        if self.down {
            sides.retain(|(side, _)| *side != Side::Team);
        }

        if !sides.iter().any(|(side, _)| *side == Side::Team) {
            return sides;
        }

        match self.hidden_ids_unpruned() {
            Ok(hidden) => {
                for (side, raw) in &mut sides {
                    if *side == Side::Team {
                        raw.retain(|id| !hidden.contains(id));
                    }
                }
            }
            Err(_) => sides.retain(|(side, _)| *side != Side::Team),
        }

        sides.retain(|(_, raw)| !raw.is_empty());
        sides
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
        // the next turn. A connection that dropped is the call's to handle ([`Self::attempt`]); one
        // that is down is not read at all.
        if !self.down {
            lost_connection(hidden.prune_if_due(self.team.as_mut()))?;
        }

        Ok(hidden.ids())
    }

    /// The ids [`Self::hidden_ids`] reads, for a change to the team notes: an error when the
    /// hidden list cannot be read, rather than none hidden.
    fn hidden_ids_strict(&mut self) -> Result<Vec<i64>, StoreError> {
        let Some(hidden) = self.hidden.as_mut() else {
            return Ok(Vec::new());
        };

        if !self.down {
            lost_connection(hidden.prune_if_due(self.team.as_mut()))?;
        }

        hidden.try_ids()
    }

    /// The ids [`Self::hidden_ids_strict`] reads, without pruning the list: a prune reads the team
    /// database with the long timeouts of a call, which no counter may wait for. The call that
    /// queued the counter pruned already, and the next reply does again.
    fn hidden_ids_unpruned(&mut self) -> Result<Vec<i64>, StoreError> {
        match self.hidden.as_mut() {
            Some(hidden) => hidden.try_ids(),
            None => Ok(Vec::new()),
        }
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
    /// part) when it is hidden (not found) or its scope keeps the team's notes read-only
    /// (`remote+local` saving to this computer).
    fn check_team_write(&mut self, view: &View, raw: i64, id: i64) -> Result<(), StoreError> {
        if self.hidden_ids_strict()?.contains(&raw) {
            return Err(note_missing(id));
        }

        let (repo_read_only, global_read_only) = (self.team_read_only(Scope::Repo), self.team_read_only(Scope::Global));
        let read_only = |scope: Scope| {
            if scope == Scope::Repo {
                repo_read_only
            } else {
                global_read_only
            }
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

    /// The best `limit` hits of `query` over `readers` (each store with the part of the view it is
    /// read for), merged by score: one store's notes of any scope rank as a single store ranks them,
    /// and the stores compare by [`Hit::score_milli`], then the most recently updated, then the
    /// higher id.
    fn search_ranked(
        &mut self,
        readers: &[(Side, View)],
        query: &SearchQuery,
        limit: usize,
    ) -> Result<Vec<Hit>, StoreError> {
        let mut hits: Vec<Hit> = Vec::new();

        for (side, view) in readers {
            hits.extend(self.search_side(*side, query, view.clone(), limit)?);
        }

        hits.sort_by_key(|hit| (Reverse(hit.score_milli), Reverse(hit.updated_at), Reverse(hit.id)));
        hits.truncate(limit);

        Ok(hits)
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

/// `pruned` as a failed prune of the hidden list leaves it: only a team database that cannot be
/// reached is an error, which fails the call that asked.
fn lost_connection(pruned: Result<usize, StoreError>) -> Result<(), StoreError> {
    match pruned {
        Err(error @ StoreError::Unreachable(_)) => Err(error),
        _ => Ok(()),
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

/// The shown note ids `ids` as each store's own, the team database's first; a store none of them
/// belongs to is left out.
fn ids_by_side(ids: &[i64]) -> Vec<(Side, Vec<i64>)> {
    [Side::Team, Side::Local]
        .into_iter()
        .map(|side| {
            let own: Vec<i64> = ids
                .iter()
                .map(|id| decode(*id))
                .filter(|(of, _)| *of == side)
                .map(|(_, raw)| raw)
                .collect();
            (side, own)
        })
        .filter(|(_, own)| !own.is_empty())
        .collect()
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

/// The refusal to change team note `id` (`done`: `deleted`, `linked`, `changed`) while the team
/// database is unreachable: the note cannot be read, so nothing was changed, and it is not known to
/// be gone.
pub(crate) fn offline_refusal(id: i64, done: &str) -> StoreError {
    StoreError::Unreachable(format!(
        "the team database is unreachable, so note {id} was not {done} and nothing was changed: this needs the \
         team database. Try again when it is back"
    ))
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

/// The refusal to change team note `id` of a `remote+local` scope that saves to this computer.
pub fn read_only_error(id: i64, scope: Scope) -> StoreError {
    let key = match scope {
        Scope::Repo => "repository_notes",
        Scope::Global => "global_notes",
    };

    StoreError::Invalid(format!(
        "note {id} is a team note and read-only here: {key} is \"remote+local\" and remote_local_saves is \
         \"local\", which reads the team's notes and saves new notes on this computer only. Save a new note \
         instead, or stop seeing this one with harness-memory notes hide {id} (Hide on this computer in \
         Harness Desktop)"
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
    /// True once the first batch was read: only that one asks the store who added the notes.
    started: bool,
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
            started: false,
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
            // The facet counts the whole list, so one batch is enough.
            want_authors: query.want_authors && !self.started,
            ..query.clone()
        })?;

        self.started = true;
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

impl SplitStore {
    fn remember_once(&mut self, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError> {
        self.settle();
        let scope = target_scope(target);
        let mut side = self.write_side(scope);

        // A note kept on this computer under this title is updated where it is, online or not.
        if self.kept_here(scope, side, target, &draft.title)? {
            side = Side::Local;
        }

        if side == Side::Team {
            self.check_hidden_title(target, &draft.title)?;
        }

        let saved = self
            .store(side)
            .remember(target, draft)
            .map_err(|error| reword(error, side, None))?;

        // Listed whether the save made the row or replaced one already here (a leftover of an earlier
        // offline save, say): what was saved last for a scope that saves to the team database is meant
        // for it. The list is a set, so a row listed already is not listed twice. A scope routed
        // `remote+local` lists only the row the save made: one that was here already is a person's own.
        let mut not_listed = false;

        // A save that replaced a row listed as waiting may have changed its title (the letter case
        // is not part of the match), so the entry follows it: the push finds a row by its exact name.
        if side == Side::Local && !saved.created && self.team_writes(scope) {
            self.rename_listed(target, saved.id, &draft.title);
        }

        if side == Side::Local && self.down && self.team_writes(scope) && self.lists_save(scope, saved.created) {
            not_listed = !self.record_offline(target, saved.id, &draft.title);
        }

        Ok(Saved {
            id: encode(side, saved.id)?,
            not_listed,
            ..saved
        })
    }

    /// True when a save without a picked place updates the note this computer keeps under `title`
    /// instead of going to the team database: the scope is routed `remote+local`, the save would
    /// go to the team database (`side`), and a note of that title is here. The store's own rules
    /// then apply to it, such as the conflict of a same-title save from another checkout.
    fn kept_here(&mut self, scope: Scope, side: Side, target: &Target, title: &str) -> Result<bool, StoreError> {
        if side != Side::Team || self.configured(scope) != NotesPlace::RemoteLocal {
            return Ok(false);
        }

        Ok(self.local.find_by_title(target, title)?.is_some())
    }

    /// True when a save on this computer while the team database was unreachable is listed as waiting:
    /// in a scope set to `remote` always (what is here is meant for the team database), in one routed
    /// `remote+local` only when the save created the row.
    fn lists_save(&self, scope: Scope, created: bool) -> bool {
        created || self.configured(scope) == NotesPlace::Remote
    }

    /// Takes the local note `id` saved in the scope of `target` off the list of what waits for the team
    /// database: the person kept it here.
    fn unlist_offline(&self, target: &Target, id: i64) -> Result<(), StoreError> {
        let Some(fallback) = &self.fallback else {
            return Ok(());
        };

        fallback
            .pending
            .forget(Kind::Notes, &fallback.team, id, &scope_label(target))
            .map_err(|error| {
                StoreError::Backend(format!(
                    "the note was saved on this computer, but could not be taken off the list of what waits for \
                     the team database, so it may still be copied there: {}",
                    fallback.secrets.scrub(&error)
                ))
            })
    }

    /// Saves the note to this computer whatever the routing of its scope says: the place a person
    /// picked. It records nothing as waiting, so the note stays here, also while the team database
    /// is unreachable, and a note listed as waiting is taken off the list.
    fn remember_local(&mut self, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError> {
        let saved = self
            .local
            .remember(target, draft)
            .map_err(|error| reword(error, Side::Local, None))?;

        // Off the list before anything settles: a settle copies what waits to the team database and
        // deletes the row here, which would send the old text there and drop the person's own.
        self.unlist_offline(target, saved.id)?;
        self.settle();

        Ok(Saved {
            id: encode(Side::Local, saved.id)?,
            ..saved
        })
    }

    /// Saves the note to the team database whatever the routing of its scope says, with the rules of
    /// a save there: the hidden title check, then the store's own. It never serves this computer
    /// and records nothing as waiting, so it takes no part in the offline fallback.
    fn remember_remote(&mut self, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError> {
        self.check_hidden_title(target, &draft.title)?;

        let saved = self
            .team
            .remember(target, draft)
            .map_err(|error| reword(error, Side::Team, None))?;

        Ok(Saved {
            id: encode(Side::Team, saved.id)?,
            ..saved
        })
    }

    fn update_once(&mut self, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError> {
        self.settle();
        let (side, raw) = decode(id);

        // The note cannot be read now, so the change waits for the team database.
        if side == Side::Team && self.down {
            return self.queue_update(view, id, patch);
        }

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

        if side == Side::Local
            && self.down
            && let Some(title) = &patch.title
        {
            self.rename_offline(&saved, raw, title);
        }

        Ok(Saved { id, ..saved })
    }

    fn get_once(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        self.settle();
        self.read_notes(view, ids, true)
    }

    fn peek_once(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        self.settle();
        self.read_notes(view, ids, false)
    }

    fn list_notes_once(&mut self, query: &NoteListQuery) -> Result<NotePage, StoreError> {
        self.settle();
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
                    want_authors: false,
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
        let mut authors = AuthorFacet::default();

        for cursor in &mut cursors {
            let part = cursor.fetch(self.store(cursor.side), query)?;
            places.push((cursor.side.place(), part.total));
            authors.merge(part.authors);
            page = NotePage::new(page.counted.into_iter().chain(part.counted), query, Vec::new());
        }

        page.places = per_place(places);
        page.authors = authors;

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

    fn search_once(&mut self, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
        self.settle();
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

        // A search for words is ranked by score across scopes and stores; only the recent notes
        // of an empty query list the repository first.
        if Terms::parse(&query.text).asked() && !readers.is_empty() {
            return self.search_ranked(&readers, query, limit);
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

    /// The notes among `ids` (shown ids) each store reads for the view of `query`, the hidden
    /// team notes left out.
    fn hits_for_once(&mut self, query: &SearchQuery, ids: &[i64]) -> Result<Vec<Hit>, StoreError> {
        self.settle();
        let mut hits: Vec<Hit> = Vec::new();

        for side in [Side::Team, Side::Local] {
            let Some(view) = self.narrow(&query.view, side) else {
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

            let part = SearchQuery {
                view,
                exclude: self.excluded(side, &query.exclude)?,
                ..query.clone()
            };

            hits.extend(encode_hits(side, self.store(side).hits_for(&part, &wanted)?)?);
        }

        Ok(hits)
    }

    fn similar_once(&mut self, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
        self.settle();
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

    fn link_once(&mut self, target: &Target, link: &Link) -> Result<Edge, StoreError> {
        self.settle();
        let scope = target_scope(target);
        let side = self.write_side(scope);

        let ids = match link.note_id {
            Some(id) => {
                let (note_side, raw) = decode(id);
                let team_note = note_side == Side::Team;

                if team_note && self.down && self.configured(scope).reads_team() {
                    return Err(offline_refusal(id, "linked"));
                }

                if note_side != side && team_note && self.reads(scope, Side::Team) {
                    return Err(read_only_error(id, scope));
                }

                let reads_both = self.reads(scope, Side::Team) && self.reads(scope, Side::Local);

                if note_side != side && !team_note && reads_both {
                    return Err(StoreError::NotFound(format!(
                        "note {id} is kept on this computer, and a link in this scope is saved in the team \
                         database, so it cannot point at it"
                    )));
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

    fn neighbors_once(
        &mut self,
        view: &View,
        entity: &str,
        depth: u8,
        limit: usize,
    ) -> Result<Neighborhood, StoreError> {
        self.settle();
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

    fn forget_once(&mut self, view: &View, target: &Target, what: &Forget) -> Result<usize, StoreError> {
        self.settle();
        let Forget::Note(id) = what else {
            let side = self.write_side(target_scope(target));
            return self.store(side).forget(view, target, what);
        };

        let (side, raw) = decode(*id);

        // The note cannot be read now, so it is not known to be gone: nothing is deleted.
        if side == Side::Team && self.down && self.reads_team_in(view) {
            return Err(offline_refusal(*id, "deleted"));
        }

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

    /// Each store deletes the scopes it saves (the team notes of a `remote+local` scope that saves to
    /// this computer stay, and so do the notes on this computer of one that saves to the team
    /// database), and the team store leaves the hidden notes too (nothing is deleted there when the
    /// hidden list cannot be read); a failure in the second leaves the first's deletes.
    fn forget_notes_once(&mut self, view: &View, exclude: &[i64]) -> Result<Forgotten, StoreError> {
        self.settle();
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

    fn find_by_title_once(&mut self, target: &Target, title: &str) -> Result<Option<Note>, StoreError> {
        self.settle();
        let side = self.write_side(target_scope(target));

        match self.store(side).find_by_title(target, title)? {
            Some(note) => Ok(Some(Note {
                id: encode(side, note.id)?,
                ..note
            })),
            None => Ok(None),
        }
    }

    /// The setting of the store new notes go to: the team database when a scope saves there, else
    /// this computer's file.
    fn english_notes_once(&mut self) -> Result<bool, StoreError> {
        self.settle();
        let side = self.settings_sides().first().copied().unwrap_or(Side::Local);
        self.store(side).english_notes()
    }

    /// Stores the setting in every store a scope saves to.
    fn set_english_notes_once(&mut self, on: bool) -> Result<(), StoreError> {
        self.settle();
        for side in self.settings_sides() {
            self.store(side).set_english_notes(on)?;
        }

        Ok(())
    }
}

impl MemoryStore for SplitStore {
    fn remember(&mut self, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError> {
        self.attempt(|this| this.remember_once(target, draft))
    }

    fn remember_to(&mut self, place: Place, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError> {
        let scope = target_scope(target);

        if self.configured(scope) != NotesPlace::RemoteLocal {
            return Err(StoreError::Invalid(
                "a place can be picked only for notes routed \"remote+local\"".to_owned(),
            ));
        }

        match place {
            Place::Local => self.remember_local(target, draft),
            Place::Remote => match self.remember_remote(target, draft) {
                Err(StoreError::Unreachable(reason)) => {
                    let message = self.team.connection_lost(&reason, false);

                    Err(StoreError::Unreachable(format!(
                        "nothing was saved, not even on this computer: {message}"
                    )))
                }
                other => other,
            },
        }
    }

    fn update(&mut self, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError> {
        self.attempt(|this| this.update_once(view, id, patch))
    }

    fn get(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        self.attempt(|this| this.get_once(view, ids))
    }

    fn peek(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        self.attempt(|this| this.peek_once(view, ids))
    }

    fn list_notes(&mut self, query: &NoteListQuery) -> Result<NotePage, StoreError> {
        self.attempt(|this| this.list_notes_once(query))
    }

    fn search(&mut self, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
        self.attempt(|this| this.search_once(query))
    }

    /// One target per store read for the view, the local one with its id offset.
    fn vector_targets(&mut self, view: &View) -> Vec<VectorTarget> {
        let mut targets: Vec<VectorTarget> = Vec::new();

        for side in self.order() {
            let Some(narrowed) = self.narrow(view, side) else {
                continue;
            };

            let offset = match side {
                Side::Team => 0,
                Side::Local => LOCAL_ID_OFFSET,
            };

            targets.extend(VectorTarget::of_view(side.place(), &narrowed, offset));
        }

        targets
    }

    fn hits_for(&mut self, query: &SearchQuery, ids: &[i64]) -> Result<Vec<Hit>, StoreError> {
        self.attempt(|this| this.hits_for_once(query, ids))
    }

    fn countable(&mut self, view: &View, ids: &[i64]) -> Countable {
        self.settle();

        let mut found = Countable::default();
        // The strict list: a hidden list that cannot be read hides nothing, so no team note is
        // counted on its word (a hidden note must never be counted).
        let hidden = self.hidden_ids_strict();

        for side in [Side::Team, Side::Local] {
            let own: Vec<(i64, i64)> = ids
                .iter()
                .map(|id| (*id, decode(*id)))
                .filter(|(_, (of, _))| *of == side)
                .map(|(shown, (_, raw))| (shown, raw))
                .collect();

            if own.is_empty() {
                continue;
            }

            // A scope this side is not read for is not visible: checked, and not found.
            let Some(narrowed) = self.narrow(view, side) else {
                continue;
            };

            let wanted: Vec<(i64, i64)> = match (side, &hidden) {
                (Side::Team, Err(_)) => {
                    found.unchecked.extend(own.iter().map(|(shown, _)| *shown));
                    continue;
                }
                (Side::Team, Ok(hidden)) => own.into_iter().filter(|(_, raw)| !hidden.contains(raw)).collect(),
                (Side::Local, _) => own,
            };

            for chunk in wanted.chunks(GET_IDS_MAX) {
                let raws: Vec<i64> = chunk.iter().map(|(_, raw)| *raw).collect();

                match self.store(side).peek(&narrowed, &raws) {
                    Ok(notes) => found.visible.extend(
                        chunk
                            .iter()
                            .filter(|(_, raw)| notes.iter().any(|note| note.id == *raw))
                            .map(|(shown, _)| *shown),
                    ),
                    Err(_) => found.unchecked.extend(chunk.iter().map(|(shown, _)| *shown)),
                }
            }
        }

        found
    }

    fn record_shown(&mut self, ids: &[i64]) -> Result<(), StoreError> {
        // A counter is not a change to the note: it also counts a read-only team note. Each side is
        // counted on its own, so a team database that is down does not stop the local counts.
        for (side, raw) in self.countable_by_side(ids) {
            let _ = self.store(side).record_shown(&raw);
        }

        Ok(())
    }

    fn record_useful(&mut self, ids: &[i64], at: i64) -> Result<(), StoreError> {
        for (side, raw) in self.countable_by_side(ids) {
            let _ = self.store(side).record_useful(&raw, at);
        }

        Ok(())
    }

    fn similar(&mut self, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
        self.attempt(|this| this.similar_once(view, id, limit))
    }

    fn link(&mut self, target: &Target, link: &Link) -> Result<Edge, StoreError> {
        self.attempt(|this| this.link_once(target, link))
    }

    fn neighbors(&mut self, view: &View, entity: &str, depth: u8, limit: usize) -> Result<Neighborhood, StoreError> {
        self.attempt(|this| this.neighbors_once(view, entity, depth, limit))
    }

    fn forget(&mut self, view: &View, target: &Target, what: &Forget) -> Result<usize, StoreError> {
        self.attempt(|this| this.forget_once(view, target, what))
    }

    fn forget_notes(&mut self, view: &View, exclude: &[i64]) -> Result<Forgotten, StoreError> {
        self.attempt(|this| this.forget_notes_once(view, exclude))
    }

    fn find_by_title(&mut self, target: &Target, title: &str) -> Result<Option<Note>, StoreError> {
        self.attempt(|this| this.find_by_title_once(target, title))
    }

    fn english_notes(&mut self) -> Result<bool, StoreError> {
        self.attempt(|this| this.english_notes_once())
    }

    fn set_english_notes(&mut self, on: bool) -> Result<(), StoreError> {
        self.attempt(|this| this.set_english_notes_once(on))
    }

    fn origin(&self, id: i64, scope: Scope) -> NoteOrigin {
        match decode(id).0 {
            Side::Local => NoteOrigin::Local,
            Side::Team => NoteOrigin::Team {
                read_only: self.place(scope).team_read_only(self.saves),
            },
        }
    }

    fn ready(&mut self) -> bool {
        self.settle();
        !self.down
    }

    fn offline(&self) -> bool {
        self.down
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
            score_milli: 0,
            boost_milli: Hit::NO_BOOST,
        };
        let ids = |hits: Vec<Hit>| hits.into_iter().map(|hit| hit.id).collect::<Vec<_>>();

        let parts = || vec![vec![hit(1), hit(2), hit(3)], vec![hit(10)]];
        assert_eq!(ids(take_turns(parts(), 3)), [1, 10, 2]);
        assert_eq!(ids(take_turns(parts(), 9)), [1, 10, 2, 3]);
        assert!(take_turns(Vec::new(), 3).is_empty());
    }
}
