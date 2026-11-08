//! `harness-memory pull`: copies team notes to this computer for the scopes set to `remote+local`,
//! the other way round from [`crate::push`]. The team database is only read, never written.
//!
//! A run is either a dry run, which lists what the team database holds in the scopes taken, or a
//! copy of the team notes [`Options::ids`] names: nothing is ever copied by default
//! ([`pull`] refuses a copy without ids). Every team read (the pages of the list, the notes asked
//! for, their relations) is done before the first write, so a team database that cannot be reached
//! fails the run with nothing copied; only this computer's file is written after that, one note at
//! a time, and what fails there is listed in [`Summary::failed`] while the other notes are still
//! copied.
//!
//! A team note is matched with the local note of the same title in the same scope and repository
//! ([`MemoryStore::find_by_title`]). No such note: [`Action::Add`], the note is copied as a note of
//! its own (the team's stays where it is, and each copy is its own local row). A local note with the
//! same text (the team's body as a local save would store it) is [`Action::Identical`], one with
//! another text [`Action::LocalDiffers`]. Neither is ever written, so pulling again adds nothing.
//!
//! A team note hidden on this computer (`hidden`, read strictly by the caller) is never listed nor
//! copied, even when its id is given: that id fails. An id that names no team note, or one outside
//! the scopes taken ([`Options::refusal`]), fails too and copies nothing.
//!
//! What travels with a copy: the kind, tags and text (masked and limited as in any local save), up
//! to [`ENTITIES_PER_NOTE`] entities, the writing client and who added and last changed the
//! note (not the writer's checkout, so the copy is a row of this checkout), and the relations the
//! team database holds for the note that touch one of its entities (read per entity, at most
//! [`NEIGHBORS_LIMIT_MAX`] relations each, so relations past that are left behind). A relation this
//! computer has already, between the same entities, is left as it is.
//! What the trait cannot carry stays behind: this computer stamps its own creation and update times
//! and starts the revision and the hit counters fresh. A pulled note is not listed among the notes
//! that wait for the team database (`pending-sync.json`): it came from there.
//!
//! The bodies of the team notes are read only where they are needed: for a note this computer has
//! one of that title for, to tell identical from different, and for the notes copied, a few notes
//! per read ([`GET_IDS_MAX`]). The list itself is read a page at a time.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::limits::{ENTITIES_PER_NOTE, GET_IDS_MAX, LIST_PAGE_MAX, NEIGHBORS_LIMIT_MAX};
use crate::push::link_exists;
use crate::store::{
    AuthorFilter, Authors, Link, MemoryStore, Note, NoteDraft, NoteKind, NoteListQuery, NoteRow, NoteSort, ProjectKey,
    Provenance, Scope, SqliteStore, StoreError, Target, View, clean,
};

/// What to pull.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// List only; write nothing.
    pub dry_run: bool,
    /// Only the notes of this repository key (`git:…` or `path:…`); the global notes are not
    /// narrowed by it. `None` for every repository.
    pub repo: Option<String>,
    /// Why repository notes are not taken (a scope that is not `remote+local`); `None` takes them.
    pub repository_left: Option<String>,
    /// Why global notes are not taken (a scope that is not `remote+local`, or `--no-global`).
    pub global_left: Option<String>,
    /// The team ids to copy. Without them only a dry run is possible.
    pub ids: Option<Vec<i64>>,
}

impl Options {
    /// Why a team entry of the repository `project` (`None`: the global scope) is not taken, if so.
    pub fn refusal(&self, project: Option<&str>) -> Option<String> {
        let Some(key) = project else {
            return self.global_left.clone();
        };

        if let Some(reason) = &self.repository_left {
            return Some(reason.clone());
        }

        match &self.repo {
            Some(repo) if repo != key => Some(format!("it belongs to {key}, and --repo names {repo}")),
            _ => None,
        }
    }

    /// The scope (and repository) a read of the team database is narrowed to.
    pub(crate) fn team_view(&self) -> View {
        match &self.repo {
            Some(key) => View {
                repo: Some(key_only(key)),
                global: self.global_left.is_none(),
            },
            None => View::every_repo(),
        }
    }

    /// The scope a list of the team database is narrowed to: none when both are taken.
    pub(crate) fn scope(&self) -> Option<Scope> {
        match (self.repository_left.is_some(), self.global_left.is_some()) {
            (true, false) => Some(Scope::Global),
            (false, true) => Some(Scope::Repo),
            _ => None,
        }
    }
}

/// What a pull did (or, for a dry run, would do) with one team entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// This computer has none of that title: it is copied.
    Add,
    /// This computer has one of that title with the same text.
    Identical,
    /// This computer has one of that title with another text. It stays as it is.
    LocalDiffers,
}

impl Action {
    /// `add`, `identical` or `localDiffers`, as `pull --json` writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Identical => "identical",
            Self::LocalDiffers => "localDiffers",
        }
    }
}

/// One team entry a pull took, and what it did (or, for a dry run, would do) with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The entry's id in the team database.
    pub team_id: i64,
    /// Its id on this computer: the copy, or the entry that was there; `None` for a dry run that
    /// would add it, or a copy that failed.
    pub local_id: Option<i64>,
    /// The repository key; `None` for a global entry.
    pub repo: Option<String>,
    /// A note's title, a script's name.
    pub title: String,
    pub action: Action,
    /// Unix seconds.
    pub team_updated_at: i64,
    /// Unix seconds; `None` when this computer has no such entry.
    pub local_updated_at: Option<i64>,
    /// True when the copy failed (see [`Summary::failed`]); the entry keeps its action but is left
    /// out of the totals.
    pub failed: bool,
}

/// An entry or relation a pull could not copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The team id of the entry; `None` when it names none.
    pub team_id: Option<i64>,
    /// What and why.
    pub message: String,
    /// True when the entry itself failed (not copied, or not taken), false for one of its
    /// relations, which did not stop the entry from being copied.
    pub entry: bool,
}

/// What a pull did (or, for a dry run, would do).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// Entries copied (or a dry run would copy); this and the other totals leave out the entries
    /// that failed.
    pub added: usize,
    /// Entries this computer holds with the same text.
    pub identical: usize,
    /// Entries this computer holds with another text.
    pub local_differs: usize,
    /// Every entry the pull took, in the order it read them.
    pub changes: Vec<Change>,
    /// Entries and relations that could not be copied, and ids that name no entry that can be.
    pub failed: Vec<Failure>,
}

impl Summary {
    /// How many entries failed: the ids that name none that can be copied, and the entries whose
    /// copy failed.
    pub fn failed_entries(&self) -> usize {
        let ids: BTreeSet<Option<i64>> = self
            .failed
            .iter()
            .filter(|failure| failure.entry)
            .map(|failure| failure.team_id)
            .collect();

        ids.len()
    }

    /// Lists `change` and counts its action.
    pub(crate) fn record(&mut self, change: Change) -> usize {
        match change.action {
            Action::Add => self.added += 1,
            Action::Identical => self.identical += 1,
            Action::LocalDiffers => self.local_differs += 1,
        }

        self.changes.push(change);
        self.changes.len() - 1
    }

    /// Marks the change at `index` failed and takes it out of its total.
    pub(crate) fn uncount(&mut self, index: usize) {
        let change = &mut self.changes[index];
        change.failed = true;

        match change.action {
            Action::Add => self.added = self.added.saturating_sub(1),
            Action::Identical => self.identical = self.identical.saturating_sub(1),
            Action::LocalDiffers => self.local_differs = self.local_differs.saturating_sub(1),
        }
    }

    /// Notes that the entry `team_id` could not be copied (or taken).
    pub(crate) fn fail(&mut self, team_id: i64, message: String) {
        self.failed.push(Failure {
            team_id: Some(team_id),
            message,
            entry: true,
        });
    }
}

/// A team note as pulled.
struct Team {
    id: i64,
    project: Option<String>,
    kind: NoteKind,
    title: String,
    /// `None` until read (a list leaves the bodies in the database).
    body: Option<String>,
    tags: Vec<String>,
    entities: Vec<String>,
    provenance: Provenance,
    updated_at: i64,
}

impl Team {
    fn of_row(row: NoteRow) -> Self {
        Self {
            id: row.id,
            project: row.project,
            kind: row.kind,
            title: row.title,
            body: None,
            tags: row.tags,
            entities: row.entities,
            provenance: copied_provenance(row.provenance, row.authors),
            updated_at: row.updated_at,
        }
    }

    fn of_note(note: Note) -> Self {
        Self {
            id: note.id,
            project: note.project,
            kind: note.kind,
            title: note.title,
            body: Some(note.body),
            tags: note.tags,
            entities: note.entities,
            provenance: copied_provenance(note.provenance, note.authors),
            updated_at: note.updated_at,
        }
    }

    fn change(&self, action: Action, local: Option<&Note>) -> Change {
        Change {
            team_id: self.id,
            local_id: local.map(|note| note.id),
            repo: self.project.clone(),
            title: self.title.clone(),
            action,
            team_updated_at: self.updated_at,
            local_updated_at: local.map(|note| note.updated_at),
            failed: false,
        }
    }
}

/// The provenance a copy keeps: the client that wrote it, and the people who added and last changed
/// the original (the copy is added by whoever added it there). The checkout and the
/// `project_path` mark of the original are not kept: they name a teammate's folder, and a copy
/// that held them would refuse every later same-title save from this checkout. The copy has no
/// root, so it is a row of any checkout here.
pub(crate) fn copied_provenance(provenance: Provenance, authors: Authors) -> Provenance {
    Provenance {
        root: None,
        via_override: false,
        author: authors.updated_by,
        original_added_by: Some(authors.added_by),
        ..provenance
    }
}

/// Pulls from the team store `team` into the local file at `local` as `options` say, never taking
/// the team notes `hidden` on this computer (their ids in `team`).
///
/// The local file is migrated first like the server would; a dry run never creates it, and a copy
/// creates it only when a note is to be written. A copy without [`Options::ids`] is refused.
pub fn pull(
    local: &Path,
    team: &mut dyn MemoryStore,
    hidden: &[i64],
    options: &Options,
) -> Result<Summary, StoreError> {
    if !options.dry_run && options.ids.is_none() {
        return Err(StoreError::Invalid(
            "pull copies only the notes it is given: list them with --dry-run, then name them with --ids".to_owned(),
        ));
    }

    let hidden: HashSet<i64> = hidden.iter().copied().collect();
    let mut summary = Summary::default();
    let mut local_store = open_existing(local)?;

    let teams = match &options.ids {
        Some(ids) => asked_for(team, &hidden, options, ids, &mut summary)?,
        None => listed(team, &hidden, options)?,
    };

    let found = match local_store.as_mut() {
        Some(store) => find_local(store, &teams)?,
        None => vec![None; teams.len()],
    };

    let mut teams = teams;
    read_missing_bodies(team, &mut teams, &found)?;

    let mut adds: Vec<(usize, usize)> = Vec::new();

    for (index, (note, seen)) in teams.iter().zip(&found).enumerate() {
        let action = match (seen, note.body.as_deref()) {
            (None, _) => Action::Add,
            (Some(local), Some(body)) if same_text(&local.body, body) => Action::Identical,
            (Some(_), Some(_)) => Action::LocalDiffers,
            (Some(_), None) => {
                summary.fail(
                    note.id,
                    format!("team note {} \"{}\" disappeared while it was read", note.id, note.title),
                );
                continue;
            }
        };

        let at = summary.record(note.change(action, seen.as_ref()));

        if action == Action::Add {
            adds.push((index, at));
        }
    }

    if options.dry_run || adds.is_empty() {
        return Ok(summary);
    }

    // Every read of the team database comes before the first write here.
    let mut cache = HashMap::new();
    let mut planned: Vec<(usize, usize, Vec<Link>)> = Vec::new();

    for (index, at) in adds {
        let links = relations_of(team, &teams[index], &mut cache)?;
        planned.push((index, at, links));
    }

    let mut store = match local_store {
        Some(store) => store,
        None => open_for_writing(local)?,
    };
    let projects = known_projects(local);

    for (index, at, links) in planned {
        copy_note(&mut store, &projects, &teams[index], at, &links, &mut summary);
    }

    Ok(summary)
}

/// The local file at `path` when there is one (migrated, never created); `None` otherwise.
fn open_existing(path: &Path) -> Result<Option<SqliteStore>, StoreError> {
    if !path.is_file() {
        return Ok(None);
    }

    open_for_writing(path).map(Some)
}

/// The local file at `path`, created when missing.
fn open_for_writing(path: &Path) -> Result<SqliteStore, StoreError> {
    let store = SqliteStore::open(path)?;

    if store.is_newer() {
        return Err(StoreError::NewerSchema);
    }

    Ok(store)
}

/// The team notes `ids` name, in the order given. An id that is hidden on this computer, names no
/// note, or names one outside the scopes taken fails (in `summary`) and is left out; no hidden
/// note is read.
fn asked_for(
    team: &mut dyn MemoryStore,
    hidden: &HashSet<i64>,
    options: &Options,
    ids: &[i64],
    summary: &mut Summary,
) -> Result<Vec<Team>, StoreError> {
    let mut seen = HashSet::new();
    let mut wanted: Vec<i64> = Vec::new();

    for id in ids.iter().copied().filter(|id| seen.insert(*id)) {
        if hidden.contains(&id) {
            summary.fail(
                id,
                format!(
                    "team note {id} is hidden on this computer, so it is not copied: show it again with \
                     harness-memory notes unhide {id}"
                ),
            );
            continue;
        }

        wanted.push(id);
    }

    let mut found: HashMap<i64, Note> = HashMap::new();

    for chunk in wanted.chunks(GET_IDS_MAX) {
        for note in team.peek(&View::every_repo(), chunk)? {
            found.insert(note.id, note);
        }
    }

    let mut teams = Vec::new();

    for id in wanted {
        let Some(note) = found.remove(&id) else {
            summary.fail(id, format!("team note {id} was not found in the team database"));
            continue;
        };

        if let Some(reason) = options.refusal(note.project.as_deref()) {
            summary.fail(id, format!("team note {id} \"{}\" is not taken: {reason}", note.title));
            continue;
        }

        teams.push(Team::of_note(note));
    }

    Ok(teams)
}

/// Every team note of the scopes taken that is not hidden on this computer, a page of the list at
/// a time, oldest change first.
fn listed(team: &mut dyn MemoryStore, hidden: &HashSet<i64>, options: &Options) -> Result<Vec<Team>, StoreError> {
    let view = options.team_view();
    let mut teams = Vec::new();
    let mut offset = 0;

    loop {
        let page = team.list_notes(&NoteListQuery {
            view: view.clone(),
            text: String::new(),
            kind: None,
            scope: options.scope(),
            place: None,
            entity: None,
            lang: None,
            useful: None,
            author: AuthorFilter::Any,
            want_authors: false,
            sort: NoteSort::Updated,
            descending: false,
            offset,
            limit: LIST_PAGE_MAX,
            exclude: Vec::new(),
        })?;

        offset += page.rows.len();
        let empty = page.rows.is_empty();

        for row in page.rows {
            if hidden.contains(&row.id) || options.refusal(row.project.as_deref()).is_some() {
                continue;
            }

            teams.push(Team::of_row(row));
        }

        if empty || offset >= page.total {
            return Ok(teams);
        }
    }
}

/// The local note of the same title in the same scope and repository, per team note.
fn find_local(store: &mut SqliteStore, teams: &[Team]) -> Result<Vec<Option<Note>>, StoreError> {
    teams
        .iter()
        .map(|note| store.find_by_title(&key_target(note.project.as_deref()), &note.title))
        .collect()
}

/// Reads the bodies of the team notes that this computer has one of the same title for, a few notes
/// per read.
fn read_missing_bodies(
    team: &mut dyn MemoryStore,
    teams: &mut [Team],
    found: &[Option<Note>],
) -> Result<(), StoreError> {
    let wanted: Vec<i64> = teams
        .iter()
        .zip(found)
        .filter(|(note, seen)| note.body.is_none() && seen.is_some())
        .map(|(note, _)| note.id)
        .collect();

    let mut bodies: HashMap<i64, String> = HashMap::new();

    for chunk in wanted.chunks(GET_IDS_MAX) {
        for note in team.peek(&View::every_repo(), chunk)? {
            bodies.insert(note.id, note.body);
        }
    }

    // A note this computer has no match for needs no body to be listed as added.
    for note in teams.iter_mut().filter(|note| note.body.is_none()) {
        note.body = bodies.remove(&note.id);
    }

    Ok(())
}

/// True when the local note body `local` is what a local save of the team's `body` stores.
fn same_text(local: &str, body: &str) -> bool {
    match clean::body(body) {
        Ok(stored) => local == stored,
        Err(_) => local == body,
    }
}

/// The relations the team database holds for `note` that touch one of its entities, read once per
/// entity of a scope (`cache`).
fn relations_of(
    team: &mut dyn MemoryStore,
    note: &Team,
    cache: &mut HashMap<(Option<String>, String), Vec<Link>>,
) -> Result<Vec<Link>, StoreError> {
    let view = match &note.project {
        Some(key) => View {
            repo: Some(key_only(key)),
            global: false,
        },
        None => View {
            repo: None,
            global: true,
        },
    };

    let mut links: Vec<Link> = Vec::new();

    for entity in note.entities.iter().take(ENTITIES_PER_NOTE) {
        let key = (note.project.clone(), clean::normalize(entity));

        if !cache.contains_key(&key) {
            let around = match team.neighbors(&view, entity, 1, NEIGHBORS_LIMIT_MAX) {
                Ok(around) => around.edges,
                Err(StoreError::NotFound(_)) => Vec::new(),
                Err(error) => return Err(error),
            };

            let edges = around
                .into_iter()
                .filter(|edge| edge.note_id.is_some())
                .map(|edge| Link {
                    from: edge.from,
                    relation: edge.relation,
                    to: edge.to,
                    note_id: edge.note_id,
                })
                .collect();

            cache.insert(key.clone(), edges);
        }

        for edge in &cache[&key] {
            let ours = edge.note_id == Some(note.id);
            let known = links
                .iter()
                .any(|link| (&link.from, &link.relation, &link.to) == (&edge.from, &edge.relation, &edge.to));

            if ours && !known {
                links.push(edge.clone());
            }
        }
    }

    Ok(links)
}

/// Writes the copy of `note` and its `links` to this computer's file; failures go to `summary`.
fn copy_note(
    store: &mut SqliteStore,
    projects: &HashMap<String, ProjectKey>,
    note: &Team,
    at: usize,
    links: &[Link],
    summary: &mut Summary,
) {
    let target = write_target(projects, note.project.as_deref());

    let draft = NoteDraft {
        title: note.title.clone(),
        body: note.body.clone().unwrap_or_default(),
        kind: note.kind,
        tags: note.tags.clone(),
        entities: note.entities.iter().take(ENTITIES_PER_NOTE).cloned().collect(),
        provenance: note.provenance.clone(),
    };

    let saved = match store.remember(&target, &draft) {
        Ok(saved) if saved.created => saved,
        Ok(_) => {
            summary.uncount(at);
            summary.fail(
                note.id,
                format!(
                    "team note {} \"{}\": a note of that title was saved on this computer meanwhile and was changed \
                     by the copy",
                    note.id, note.title
                ),
            );
            return;
        }
        Err(error) => {
            summary.uncount(at);
            summary.fail(note.id, format!("team note {} \"{}\": {error}", note.id, note.title));
            return;
        }
    };

    summary.changes[at].local_id = Some(saved.id);

    for link in links {
        if let Err(error) = write_link(store, &target, saved.id, link) {
            summary.failed.push(Failure {
                team_id: Some(note.id),
                message: format!(
                    "team note {}: relation \"{} {} {}\": {error}",
                    note.id, link.from, link.relation, link.to
                ),
                entry: false,
            });
        }
    }
}

/// Links `link` to the local note `local_id` unless this computer has that relation already.
fn write_link(store: &mut SqliteStore, target: &Target, local_id: i64, link: &Link) -> Result<(), StoreError> {
    let unattached = Link {
        note_id: None,
        ..link.clone()
    };

    if link_exists(store, target, &unattached)? {
        return Ok(());
    }

    store
        .link(
            target,
            &Link {
                note_id: Some(local_id),
                ..link.clone()
            },
        )
        .map(|_| ())
}

/// The scope a read looks in for the repository `project` (`None`: global); only the key matters.
pub(crate) fn key_target(project: Option<&str>) -> Target {
    match project {
        Some(key) => Target::Repo(key_only(key)),
        None => Target::Global,
    }
}

/// A repository named by its key alone, which is all a read needs.
pub(crate) fn key_only(key: &str) -> ProjectKey {
    ProjectKey {
        key: key.to_owned(),
        root: PathBuf::new(),
        remote: None,
    }
}

/// The repositories this computer's file knows, with the root and remote it holds for them: a copy
/// into such a repository must not change them.
pub(crate) fn known_projects(path: &Path) -> HashMap<String, ProjectKey> {
    let mut known = HashMap::new();
    let opened = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX);

    let Ok(conn) = opened else {
        return known;
    };

    let Ok(mut statement) = conn.prepare("SELECT key, root_path, remote FROM projects") else {
        return known;
    };

    let rows = statement.query_map([], |row| {
        let root: String = row.get(1)?;

        Ok(ProjectKey {
            key: row.get(0)?,
            root: PathBuf::from(root),
            remote: row.get(2)?,
        })
    });

    for key in rows.into_iter().flatten().flatten() {
        known.insert(key.key.clone(), key);
    }

    known
}

/// The scope a copy is written to: the repository as this computer knows it, else as its key says
/// (`git:host/owner/repo` has that remote, `path:/folder` that root).
pub(crate) fn write_target(projects: &HashMap<String, ProjectKey>, project: Option<&str>) -> Target {
    let Some(key) = project else {
        return Target::Global;
    };

    if let Some(known) = projects.get(key) {
        return Target::Repo(known.clone());
    }

    Target::Repo(ProjectKey {
        key: key.to_owned(),
        root: key.strip_prefix("path:").map(PathBuf::from).unwrap_or_default(),
        remote: key.strip_prefix("git:").map(str::to_owned),
    })
}
