//! `harness-memory push`: copies the local SQLite memory into a team database.
//!
//! The notes (repository and global) and relations of the local database are written to the team
//! store through [`MemoryStore`], so any backend can receive it. [`Options`] can narrow that to one
//! repository and leave the global scope or every repository out. A note whose title already
//! exists in the same scope of the team store is compared with it: the same note is
//! [`Action::Identical`], a team note changed later is [`Action::TeamNewer`], and both stay as they
//! are, so pushing again adds nothing. A newer local note with the team's text only changes its
//! kind, tags or title ([`Action::Updated`]). A newer local note with another text is a conflict:
//! [`Options::overwrite`] says whether it replaces the teammate's text ([`Action::Overwritten`]) or
//! the team's text stays ([`Action::Kept`]). Every note is listed in [`Summary::changes`]; a note
//! whose writing failed is marked [`Change::failed`] and left out of the totals. A relation that
//! already exists in the team store as it would be written is left alone. A relation whose note
//! failed is not written either: it fails too, as a [`Failure`] of that note (its local id), in a
//! dry run as well.
//!
//! A team note hidden on this computer (`hidden`, read strictly by the caller: a push stops when
//! the list cannot be read) is never written over, nor counted as copied: a local note whose
//! action would copy it there (any but [`Action::Kept`] and [`Action::TeamNewer`]) fails instead,
//! naming `harness-memory notes unhide`, and stays on this computer. A dry run fails it as well,
//! so the preview lists the same failures as the push. The relations of a local note whose team
//! note is hidden and left as it is ([`Action::Kept`], [`Action::TeamNewer`]) are not linked to it:
//! they are counted in [`Summary::links_unchanged`], as the note is counted as left alone.
//!
//! With [`Options::delete_copied`] the local notes the team store now holds as they are here are
//! deleted from the local database after the push. A note whose writing failed stays, and so does
//! one the team store already had (any action but [`Action::Added`]) when the team's note lacks
//! some of its entities: only an added note takes the local entities along. When any relation
//! could not be written, nothing is deleted (the notes it belongs to would lose it), and that is
//! reported as a failure; the relation of a failed note does not count, as its note stays. A note
//! that changed (or went) after the push read it stays as well, marked [`Change::changed_since`].
//!
//! What the trait cannot carry stays behind: the team store stamps its own creation and update
//! times, revisions and hit counts start fresh, and an updated note keeps the entities it had
//! (only added notes get the local ones; at most [`ENTITIES_PER_NOTE`] of them).

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::store::pending::Entry;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::limits::{ENTITIES_PER_NOTE, NEIGHBORS_LIMIT_MAX};
use crate::store::split::hidden_title_error;
use crate::store::{Forget, Link, MemoryStore, Note, NoteDraft, NoteKind, NotePatch, ProjectKey, Provenance};
use crate::store::{SqliteStore, StoreError, Target, View, clean};

/// The label of the global scope in [`Change::scope`] and [`Summary::projects`].
pub const GLOBAL: &str = "global";

/// Which conflicts (a newer local note whose text differs from the team's) a push overwrites.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Overwrite {
    /// Every conflict, as `push` does without `--overwrite`.
    #[default]
    Default,
    /// None: the team's text stays (`--overwrite none`).
    None,
    /// Every conflict (`--overwrite all`).
    All,
    /// Only the conflicts of these local note ids; any other conflict is kept.
    Ids(BTreeSet<i64>),
}

impl Overwrite {
    /// True when a conflict on the local note (or script) `local_id` is overwritten.
    pub fn takes(&self, local_id: i64) -> bool {
        match self {
            Self::Default | Self::All => true,
            Self::None => false,
            Self::Ids(ids) => ids.contains(&local_id),
        }
    }
}

/// What to push.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Count and list only; write nothing.
    pub dry_run: bool,
    /// Only the notes and relations of this repository key (`git:…` or `path:…`); `None` for
    /// every repository.
    pub repo: Option<String>,
    /// Leave the global notes and relations out.
    pub skip_global: bool,
    /// Leave every repository's notes and relations out (they stay on this computer, see
    /// [`crate::backend::Routing`]).
    pub skip_repositories: bool,
    /// Which conflicts replace the team's text.
    pub overwrite: Overwrite,
    /// After a push that is not a dry run, delete the local notes the team store holds as they are
    /// here (see [`Action::copies`]), unless writing them failed or they changed meanwhile.
    pub delete_copied: bool,
}

impl Options {
    /// True when the scope `target` is pushed (by `push` and `scripts push` alike).
    pub fn takes(&self, target: &Target) -> bool {
        match target {
            Target::Global => !self.skip_global,
            Target::Repo(_) if self.skip_repositories => false,
            Target::Repo(key) => self.repo.as_ref().is_none_or(|repo| *repo == key.key),
        }
    }
}

/// A local note as pushed.
#[derive(Debug, Clone)]
struct LocalNote {
    id: i64,
    target: Target,
    kind: NoteKind,
    title: String,
    body: String,
    tags: Vec<String>,
    entities: Vec<String>,
    provenance: Provenance,
    updated_at: i64,
    /// Bumped by every write through the store, so a change within the same second shows.
    revision: i64,
}

/// A local relation as pushed.
#[derive(Debug, Clone)]
struct LocalLink {
    target: Target,
    link: Link,
}

/// What a push did to one note in the team store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The note was not there and was added.
    Added,
    /// The team's note had the same text; its kind, tags or title were replaced by the newer local
    /// ones.
    Updated,
    /// The team's note had another text, which the newer local one replaced.
    Overwritten,
    /// The team's note had another text and the local note was newer, but [`Options::overwrite`]
    /// left the team's text in place.
    Kept,
    /// The team's note was the same as the local one.
    Identical,
    /// The team's note differed and was changed after the local one, so it stayed.
    TeamNewer,
}

impl Action {
    /// `add`, `update`, `overwrite`, `keep`, `identical` or `teamNewer`, as `push --json` writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "add",
            Self::Updated => "update",
            Self::Overwritten => "overwrite",
            Self::Kept => "keep",
            Self::Identical => "identical",
            Self::TeamNewer => "teamNewer",
        }
    }

    /// True when the team store holds the note as it is here after the action, so the local copy
    /// can go ([`Options::delete_copied`]).
    pub fn copies(self) -> bool {
        matches!(self, Self::Added | Self::Updated | Self::Overwritten | Self::Identical)
    }
}

/// One local note a push took, and what it did (or, for a dry run, would do) with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The note's id in the local database.
    pub local_id: i64,
    /// The repository key, or [`GLOBAL`].
    pub scope: String,
    /// The repository key; `None` for a global note.
    pub repo: Option<String>,
    pub title: String,
    pub action: Action,
    /// True when the local note was newer and its text differed from the team's: an overwrite
    /// candidate, whether [`Options::overwrite`] took it ([`Action::Overwritten`]) or not
    /// ([`Action::Kept`]).
    pub conflict: bool,
    /// The note's id in the team store; `None` for a note a dry run would add, or whose adding
    /// failed.
    pub team_id: Option<i64>,
    /// Unix seconds.
    pub local_updated_at: i64,
    /// Unix seconds; `None` when the team store had no such note.
    pub team_updated_at: Option<i64>,
    /// True when [`Options::delete_copied`] deleted the local note.
    pub deleted: bool,
    /// True when [`Options::delete_copied`] would have deleted the local note, but it changed (or
    /// went) after the push read it, so it stayed.
    pub changed_since: bool,
    /// True when adding or changing the team's note failed (see [`Summary::failed`]); the note
    /// keeps its [`Change::action`] but is left out of the totals.
    pub failed: bool,
}

/// A note or relation a push could not write, or a local note it could not delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The local note's id; `None` for a relation or the local database as a whole.
    pub local_id: Option<i64>,
    /// What and why.
    pub message: String,
    /// True when the note was copied but deleting it from the local database failed.
    pub deleting: bool,
}

/// What a push did (or, for a dry run, would do).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// Notes added; this and the other note totals leave out the notes whose writing failed.
    pub added: usize,
    /// Notes whose kind, tags or title were replaced; the team's text was the same.
    pub updated: usize,
    /// Notes whose team text was replaced by a newer, different local text.
    pub overwritten: usize,
    /// Conflicts whose team text stayed ([`Action::Kept`]).
    pub kept: usize,
    /// Notes the team store already had as they are here.
    pub identical: usize,
    /// Notes the team changed after the local ones, so they stayed.
    pub team_newer: usize,
    /// Relations written to the team store.
    pub links_added: usize,
    /// Relations already in the team store as they would be written, so left alone, and those of
    /// a note whose team note is hidden on this computer and was left as it is.
    pub links_unchanged: usize,
    /// Added notes that had more than [`ENTITIES_PER_NOTE`] entities, of which only the first
    /// were linked.
    pub entities_cut: usize,
    /// Every local note the push took, in push order.
    pub changes: Vec<Change>,
    /// The repository keys (and [`GLOBAL`]) whose notes or relations were pushed or skipped.
    pub projects: BTreeSet<String>,
    /// Notes and relations that could not be written, and local notes that could not be deleted.
    pub failed: Vec<Failure>,
    /// How many local notes [`Options::delete_copied`] deleted; `None` when it was not asked for
    /// or the push was a dry run.
    pub deleted: Option<usize>,
}

impl Summary {
    /// Notes left as they were in the team store: [`Summary::identical`] and
    /// [`Summary::team_newer`].
    pub fn skipped(&self) -> usize {
        self.identical + self.team_newer
    }
}

/// Pushes the local database at `local` into `remote` as `options` say, never writing over the
/// team notes `hidden` on this computer (their ids in `remote`).
///
/// The local database is migrated first like the server would (it is never created: a missing
/// file is an error), then read through a read-only connection.
pub fn push(
    local: &Path,
    remote: &mut dyn MemoryStore,
    hidden: &[i64],
    options: &Options,
) -> Result<Summary, StoreError> {
    push_ids(local, remote, hidden, options, None)
}

/// Like [`push`], but only the local notes `only` names (an id, scope and title that all match), with
/// their relations: the offline fallback copies what it saved while the team database was
/// unreachable (see [`crate::store::pending`]).
pub fn push_only(
    local: &Path,
    remote: &mut dyn MemoryStore,
    hidden: &[i64],
    options: &Options,
    only: &[Entry],
) -> Result<Summary, StoreError> {
    push_ids(local, remote, hidden, options, Some(only))
}

fn push_ids(
    local: &Path,
    remote: &mut dyn MemoryStore,
    hidden: &[i64],
    options: &Options,
    only: Option<&[Entry]>,
) -> Result<Summary, StoreError> {
    let (notes, links) = read_local(local)?;
    let mut summary = Summary::default();
    let mut remote_ids: HashMap<i64, i64> = HashMap::new();
    let mut lacking: HashSet<i64> = HashSet::new();
    let mut failed_notes: HashSet<i64> = HashSet::new();
    let mut left_hidden: HashSet<i64> = HashSet::new();

    let chosen = |note: &LocalNote| {
        only.is_none_or(|entries| {
            entries.iter().any(|entry| {
                entry.id == note.id && entry.name == note.title && entry.scope == scope_label(&note.target)
            })
        })
    };
    let chosen_ids: HashSet<i64> = notes.iter().filter(|note| chosen(note)).map(|note| note.id).collect();

    for note in notes.iter().filter(|note| options.takes(&note.target) && chosen(note)) {
        summary.projects.insert(scope_label(&note.target));

        match push_note(remote, hidden, note, options, &mut summary) {
            // A kept or team-newer note whose team note is hidden here: its relations stay off it.
            Ok(Some(pushed)) if pushed.hidden => {
                left_hidden.insert(note.id);
            }
            Ok(Some(pushed)) => {
                remote_ids.insert(note.id, pushed.team_id);

                if pushed.lacks_entities {
                    lacking.insert(note.id);
                }
            }
            Ok(None) => {}
            Err(error) => {
                failed_notes.insert(note.id);
                uncount(&mut summary, note.id);
                summary.failed.push(Failure {
                    local_id: Some(note.id),
                    message: format!("note {} \"{}\": {error}", note.id, note.title),
                    deleting: false,
                });
            }
        }
    }

    let mut relations_failed = false;

    for local_link in links.iter().filter(|link| {
        options.takes(&link.target) && only.is_none_or(|_| link.link.note_id.is_some_and(|id| chosen_ids.contains(&id)))
    }) {
        summary.projects.insert(scope_label(&local_link.target));

        let note_id = local_link.link.note_id;

        // Without its note the relation would reach the team detached from it: it fails with it.
        if let Some(id) = note_id.filter(|id| failed_notes.contains(id)) {
            summary.failed.push(Failure {
                local_id: Some(id),
                message: format!(
                    "relation \"{} {} {}\": its note {id} was not pushed",
                    local_link.link.from, local_link.link.relation, local_link.link.to
                ),
                deleting: false,
            });
            continue;
        }

        // The team's note was left as it is and is hidden here, so its relations are left too.
        if note_id.is_some_and(|id| left_hidden.contains(&id)) {
            summary.links_unchanged += 1;
            continue;
        }

        let mapped = note_id.map(|id| remote_ids.get(&id).copied());
        let link = Link {
            note_id: mapped.flatten(),
            ..local_link.link.clone()
        };

        // In a dry run a note that would be added has no team id yet, so its relation would change.
        let awaits_note = matches!(mapped, Some(None));
        let exists = match link_exists(remote, &local_link.target, &link) {
            Ok(exists) => exists && !(options.dry_run && awaits_note),
            Err(error) => {
                summary.failed.push(relation_failure(&link, &error));
                relations_failed = true;
                continue;
            }
        };

        if exists {
            summary.links_unchanged += 1;
            continue;
        }

        if options.dry_run {
            summary.links_added += 1;
            continue;
        }

        match remote.link(&local_link.target, &link) {
            Ok(_) => summary.links_added += 1,
            Err(error) => {
                summary.failed.push(relation_failure(&link, &error));
                relations_failed = true;
            }
        }
    }

    if options.delete_copied && !options.dry_run {
        let deleted = if relations_failed {
            summary.failed.push(Failure {
                local_id: None,
                message: "the copied notes were not deleted from this computer, because a relation could not be \
                          pushed. Push again"
                    .to_owned(),
                deleting: true,
            });
            0
        } else {
            delete_copied(local, &notes, &lacking, &mut summary)
        };

        summary.deleted = Some(deleted);
    }

    Ok(summary)
}

/// A note the team store holds after [`push_note`].
struct Pushed {
    team_id: i64,
    /// True when the team's note lacks some of the local note's entities (which only adding a note
    /// takes along).
    lacks_entities: bool,
    /// True when the team's note is hidden on this computer (and so was left as it is).
    hidden: bool,
}

/// Adds, replaces or leaves one note; the team store's note when it has one. A team note `hidden`
/// on this computer that the note would copy to fails it, in a dry run too.
fn push_note(
    remote: &mut dyn MemoryStore,
    hidden: &[i64],
    note: &LocalNote,
    options: &Options,
    summary: &mut Summary,
) -> Result<Option<Pushed>, StoreError> {
    let existing = remote.find_by_title(&note.target, &note.title)?;

    let Some(existing) = existing else {
        let index = record(summary, note, Action::Added, false, None);

        if note.entities.len() > ENTITIES_PER_NOTE {
            summary.entities_cut += 1;
        }

        if options.dry_run {
            return Ok(None);
        }

        let draft = NoteDraft {
            title: note.title.clone(),
            body: note.body.clone(),
            kind: note.kind,
            tags: note.tags.clone(),
            entities: note.entities.iter().take(ENTITIES_PER_NOTE).cloned().collect(),
            provenance: note.provenance.clone(),
        };

        let saved = remote.remember(&note.target, &draft)?;
        summary.changes[index].team_id = Some(saved.id);
        return Ok(Some(Pushed {
            team_id: saved.id,
            lacks_entities: false,
            hidden: false,
        }));
    };

    let (action, conflict) = compare(note, &existing, &options.overwrite);
    record(summary, note, action, conflict, Some(&existing));

    let is_hidden = hidden.contains(&existing.id);

    if action.copies() && is_hidden {
        return Err(hidden_title_error(existing.id));
    }

    let writes = matches!(action, Action::Updated | Action::Overwritten);
    let pushed = Pushed {
        team_id: existing.id,
        lacks_entities: lacks_entities(note, &existing),
        hidden: is_hidden,
    };

    if options.dry_run || !writes {
        return Ok(Some(pushed));
    }

    let patch = NotePatch {
        title: Some(note.title.clone()),
        body: Some(note.body.clone()),
        append: None,
        kind: Some(note.kind),
        tags: Some(note.tags.clone()),
        provenance: note.provenance.clone(),
    };

    remote.update(&View::around(&note.target), existing.id, &patch)?;
    Ok(Some(pushed))
}

/// True when the team's `existing` note lacks an entity of the local `note`.
fn lacks_entities(note: &LocalNote, existing: &Note) -> bool {
    let theirs: HashSet<String> = existing.entities.iter().map(|name| clean::normalize(name)).collect();
    note.entities
        .iter()
        .any(|name| !theirs.contains(&clean::normalize(name)))
}

/// What pushing `note` over the team's `existing` note with the same title does, and whether it
/// is a conflict (the local note is newer and its text differs).
fn compare(note: &LocalNote, existing: &Note, overwrite: &Overwrite) -> (Action, bool) {
    let same = existing.title == note.title
        && existing.body == note.body
        && existing.kind == note.kind
        && existing.tags == note.tags;

    if same {
        return (Action::Identical, false);
    }

    if note.updated_at <= existing.updated_at {
        return (Action::TeamNewer, false);
    }

    if existing.body == note.body {
        return (Action::Updated, false);
    }

    if overwrite.takes(note.id) {
        (Action::Overwritten, true)
    } else {
        (Action::Kept, true)
    }
}

/// Counts `action` for `note` (against the team's `existing` note, if any) and lists it in
/// [`Summary::changes`]; its index there.
fn record(summary: &mut Summary, note: &LocalNote, action: Action, conflict: bool, existing: Option<&Note>) -> usize {
    *total(summary, action) += 1;

    let repo = match &note.target {
        Target::Repo(key) => Some(key.key.clone()),
        Target::Global => None,
    };

    summary.changes.push(Change {
        local_id: note.id,
        scope: scope_label(&note.target),
        repo,
        title: note.title.clone(),
        action,
        conflict,
        team_id: existing.map(|team| team.id),
        local_updated_at: note.updated_at,
        team_updated_at: existing.map(|team| team.updated_at),
        deleted: false,
        changed_since: false,
        failed: false,
    });

    summary.changes.len() - 1
}

/// The total of `summary` that counts `action`.
fn total(summary: &mut Summary, action: Action) -> &mut usize {
    match action {
        Action::Added => &mut summary.added,
        Action::Updated => &mut summary.updated,
        Action::Overwritten => &mut summary.overwritten,
        Action::Kept => &mut summary.kept,
        Action::Identical => &mut summary.identical,
        Action::TeamNewer => &mut summary.team_newer,
    }
}

/// Marks the change of the local note `local_id` failed and takes it out of its total (writing it
/// failed after it was counted); nothing when it was not recorded yet.
fn uncount(summary: &mut Summary, local_id: i64) {
    let Some(change) = summary
        .changes
        .iter_mut()
        .rev()
        .find(|change| change.local_id == local_id)
    else {
        return;
    };

    change.failed = true;
    let action = change.action;

    let count = total(summary, action);
    *count = count.saturating_sub(1);
}

/// Deletes from the local database at `path` the `notes` that [`Summary::changes`] says the team
/// store now holds as they are here, leaving any whose writing failed, whose entities the team's
/// note lacks (`lacking`), or that changed since they were read; how many went. What cannot be
/// deleted is added to [`Summary::failed`].
fn delete_copied(path: &Path, notes: &[LocalNote], lacking: &HashSet<i64>, summary: &mut Summary) -> usize {
    let mut store = match SqliteStore::open(path) {
        Ok(store) => store,
        Err(error) => {
            summary.failed.push(Failure {
                local_id: None,
                message: format!("the copied notes were not deleted from this computer: {error}"),
                deleting: true,
            });
            return 0;
        }
    };

    let failed: HashSet<i64> = summary.failed.iter().filter_map(|failure| failure.local_id).collect();
    let read: HashMap<i64, &LocalNote> = notes.iter().map(|note| (note.id, note)).collect();
    let mut errors = Vec::new();
    let mut deleted = 0;

    for change in &mut summary.changes {
        let kept = !change.action.copies()
            || change.failed
            || failed.contains(&change.local_id)
            || lacking.contains(&change.local_id);

        if kept {
            continue;
        }

        let Some(note) = read.get(&change.local_id).copied() else {
            continue;
        };

        match forget_unchanged(&mut store, note) {
            Ok(true) => {
                change.deleted = true;
                deleted += 1;
            }
            Ok(false) => change.changed_since = true,
            Err(error) => errors.push(Failure {
                local_id: Some(change.local_id),
                message: format!(
                    "note {} \"{}\" was copied but not deleted from this computer: {error}",
                    change.local_id, change.title
                ),
                deleting: true,
            }),
        }
    }

    summary.failed.extend(errors);
    deleted
}

/// Deletes the local note as the push `read` it unless it changed since (a server may have written
/// it meanwhile, or deleted it); true when it went, false when it changed. `updated_at` has whole
/// seconds, so the revision and the text are compared as well.
fn forget_unchanged(store: &mut SqliteStore, read: &LocalNote) -> Result<bool, StoreError> {
    let view = View::around(&read.target);
    let current = store.peek(&view, &[read.id])?;

    let unchanged = current.first().is_some_and(|note| {
        note.updated_at == read.updated_at
            && note.revision == read.revision
            && note.title == read.title
            && note.body == read.body
    });

    if !unchanged {
        return Ok(false);
    }

    let forgotten = store.forget(&view, &read.target, &Forget::Note(read.id))?;
    Ok(forgotten > 0)
}

/// True when the scope `target` of the team store already holds `link` as writing it would leave
/// it: the same entities and label, and its note (a relation without a note keeps the one it has).
/// Only the relations [`MemoryStore::neighbors`] lists for the first entity are looked at, so one
/// past [`NEIGHBORS_LIMIT_MAX`] counts as missing and is written again.
fn link_exists(remote: &mut dyn MemoryStore, target: &Target, link: &Link) -> Result<bool, StoreError> {
    // An invalid relation is left to `link`, which reports why.
    let Ok(wanted) = clean::link(link) else {
        return Ok(false);
    };

    let view = match target {
        Target::Repo(key) => View {
            repo: Some(key.clone()),
            global: false,
        },
        Target::Global => View {
            repo: None,
            global: true,
        },
    };

    let around = match remote.neighbors(&view, &wanted.from, 1, NEIGHBORS_LIMIT_MAX) {
        Ok(around) => around,
        Err(StoreError::NotFound(_)) => return Ok(false),
        Err(error) => return Err(error),
    };

    let (from, to) = (clean::normalize(&wanted.from), clean::normalize(&wanted.to));

    Ok(around.edges.iter().any(|edge| {
        clean::normalize(&edge.from) == from
            && edge.relation == wanted.relation
            && clean::normalize(&edge.to) == to
            && wanted.note_id.is_none_or(|id| edge.note_id == Some(id))
    }))
}

fn relation_failure(link: &Link, error: &StoreError) -> Failure {
    Failure {
        local_id: None,
        message: format!("relation \"{} {} {}\": {error}", link.from, link.relation, link.to),
        deleting: false,
    }
}

/// The repository key of `target`, or [`GLOBAL`].
pub(crate) fn scope_label(target: &Target) -> String {
    match target {
        Target::Repo(key) => key.key.clone(),
        Target::Global => GLOBAL.to_owned(),
    }
}

/// The notes (oldest first) and relations of the local database.
fn read_local(path: &Path) -> Result<(Vec<LocalNote>, Vec<LocalLink>), StoreError> {
    if !path.is_file() {
        return Err(StoreError::NotFound(format!(
            "there is no local memory database at {}",
            path.display()
        )));
    }

    // Opening migrates an older file, and reports a newer one.
    let store = SqliteStore::open(path)?;
    if store.is_newer() {
        return Err(StoreError::NewerSchema);
    }
    drop(store);

    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    let mut entities: HashMap<i64, Vec<String>> = HashMap::new();
    let mut statement = conn.prepare(
        "SELECT ne.note_id, e.name FROM note_entities ne JOIN entities e ON e.id = ne.entity_id
         ORDER BY ne.note_id, e.name_norm",
    )?;
    let mut rows = statement.query([])?;

    while let Some(row) = rows.next()? {
        entities.entry(row.get(0)?).or_default().push(row.get(1)?);
    }

    let mut statement = conn.prepare(
        "SELECT n.id, p.key, p.root_path, p.remote, n.kind, n.title, n.body, n.tags, n.source, n.writer_root,
                n.via_override, n.updated_at, n.revision
         FROM notes n LEFT JOIN projects p ON p.id = n.project_id
         ORDER BY n.id",
    )?;
    let notes = statement
        .query_map([], |row| {
            let id: i64 = row.get(0)?;
            let kind: String = row.get(4)?;
            let tags: String = row.get(7)?;
            let root: Option<String> = row.get(9)?;

            Ok(LocalNote {
                id,
                target: target(row.get(1)?, row.get(2)?, row.get(3)?),
                kind: NoteKind::parse(&kind).unwrap_or(NoteKind::Note),
                title: row.get(5)?,
                body: row.get(6)?,
                tags: tags.split_whitespace().map(str::to_owned).collect(),
                entities: entities.remove(&id).unwrap_or_default(),
                provenance: Provenance {
                    root: root.map(PathBuf::from),
                    source: row.get(8)?,
                    via_override: row.get(10)?,
                },
                updated_at: row.get(11)?,
                revision: row.get(12)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut statement = conn.prepare(
        "SELECT p.key, p.root_path, p.remote, s.name, r.rel, d.name, r.note_id
         FROM relations r
         JOIN entities s ON s.id = r.src
         JOIN entities d ON d.id = r.dst
         LEFT JOIN projects p ON p.id = r.project_id
         ORDER BY r.id",
    )?;
    let links = statement
        .query_map([], |row| {
            Ok(LocalLink {
                target: target(row.get(0)?, row.get(1)?, row.get(2)?),
                link: Link {
                    from: row.get(3)?,
                    relation: row.get(4)?,
                    to: row.get(5)?,
                    note_id: row.get(6)?,
                },
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok((notes, links))
}

/// The scope of a row from its project's key, root and remote (all `None` for global).
fn target(key: Option<String>, root: Option<String>, remote: Option<String>) -> Target {
    match key {
        Some(key) => Target::Repo(ProjectKey {
            key,
            root: PathBuf::from(root.unwrap_or_default()),
            remote,
        }),
        None => Target::Global,
    }
}
