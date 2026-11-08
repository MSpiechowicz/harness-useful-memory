//! The changes to team notes made while the team database was unreachable.
//!
//! With the offline fallback (see [`crate::backend::Selection::offline_fallback`]) a note saved for
//! a scope set to `remote` is kept on this computer and copied later ([`crate::store::pending`]),
//! but a change to a note the team database already holds cannot be made on a copy: the note is not
//! here. Such a change is kept instead as an [`Update`] in `pending-updates.json` next to the local
//! database, in the order it was made, and applied to the team note when the database is back
//! ([`crate::store::SplitStore`] replays it, one process at a time).
//!
//! An update keeps what is needed to apply it as the person made it: the note, the part of the
//! repository and global scopes it was made in (the view), when it was made, the revision the person
//! last read when the caller knew it ([`Update::base_revision`]), the cleaned changes and who made
//! them. A title and a body that replace the note's are masked as the store masks them, and the
//! text to append is masked on its own (no secret is kept at rest in the file). It is masked again
//! as one with the text it is joined to when it is applied, so a secret split across the join is
//! still caught.
//!
//! Before an update is sent to the team database it is marked on its entry ([`Applying`]) with the
//! revision of the note it was read at. A process that stops after the write and before the entry
//! is taken off finds the mark at the next replay: only an update that carries one can already be in
//! the note, so an update that was never started is applied even when the note ends with the same
//! text.
//!
//! The file is private to the owner (`0600`), written through a unique temporary file and changed
//! under an advisory lock on `pending-updates.json.lock` ([`FileLock`]), so two servers adding or
//! removing an update at once lose none. A change that would not fit in the file, and a file that
//! cannot be read, are refused with a message instead of dropping what is queued or overwriting it.
//! `pending-sync.json` (the list of notes saved offline) is not touched: an older harness-memory,
//! which knows nothing of this file, goes on copying what it lists.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::store::{Author, NoteKind, NotePatch, ProjectKey, Provenance, StoreError, View, clean};
use crate::support::private::{FileLock, private_folder, read_private, write_atomic};
use crate::support::redact::redact_text;

/// What a change that was queued is told to say, in every reply that reports it.
pub const QUEUED_MESSAGE: &str = "saved on this computer, applied when the remote database is back";

/// The file name of the queue, next to the local database.
const FILE: &str = "pending-updates.json";

/// The file whose lock says that a process is applying the queue.
const REPLAY_FILE: &str = "pending-updates.replay";

/// The most bytes read from the file.
const MAX_BYTES: usize = 4 * 1024 * 1024;

/// The most bytes written to it, below [`MAX_BYTES`] so that what was written can always be read.
const MAX_WRITE_BYTES: usize = 3 * 1024 * 1024;

/// The changes of an update, as the person gave them; `None` keeps a field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Text added to the end of the body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append: Option<String>,
    /// The name of the kind ([`NoteKind::as_str`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

/// Who made the change ([`Provenance`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Writer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub via_override: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Author>,
}

/// The repository of the view a change was made in ([`ProjectKey`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    pub key: String,
    /// The work-tree it was made in; kept so that saving a note of this repository on this computer
    /// does not overwrite the root the local database knows with an empty one.
    #[serde(default)]
    pub root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

impl Repo {
    /// The repository as a project key.
    pub fn project_key(&self) -> ProjectKey {
        ProjectKey {
            key: self.key.clone(),
            root: PathBuf::from(&self.root),
            remote: self.remote.clone(),
        }
    }
}

/// One change to a team note, waiting for the team database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Update {
    /// Its place in the queue: later changes have larger numbers.
    pub seq: u64,
    /// The team database it is meant for ([`crate::backend::Backend::team_key`]).
    pub team: String,
    /// The note's id in the team database.
    pub id: i64,
    /// The repository of the view the change was made in; `None` without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<Repo>,
    /// Whether that view included the global notes.
    #[serde(default)]
    pub global: bool,
    /// Unix seconds when the change was made.
    pub at: i64,
    /// The revision of the note the person last read; `None` when the caller did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<i64>,
    pub patch: Patch,
    pub writer: Writer,
    /// Set just before the update is sent to the team database: the note as it was read then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applying: Option<Applying>,
}

/// The note as the replay read it just before it sent an update: what a later replay needs to tell
/// an update that went through (its process stopped before the entry was taken off) from one that
/// was never sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applying {
    /// The revision the note was at.
    pub revision: i64,
    /// Unix seconds when the note was last changed, as read.
    pub updated_at: i64,
}

impl Update {
    /// The change `patch` makes to team note `id` in `view` of the team database `team` at `at`
    /// (Unix seconds); it fails the way the store fails an invalid patch. [`Queue::add`] numbers it.
    pub fn new(team: &str, id: i64, view: &View, patch: &NotePatch, at: i64) -> Result<Self, StoreError> {
        let cleaned = clean::patch(patch)?;
        let provenance = &patch.provenance;

        Ok(Self {
            seq: 0,
            team: team.to_owned(),
            id,
            repo: view.repo.as_ref().map(|repo| Repo {
                key: repo.key.clone(),
                root: repo.root.to_string_lossy().into_owned(),
                remote: repo.remote.clone(),
            }),
            global: view.global,
            at,
            base_revision: patch.expected_revision,
            patch: Patch {
                title: cleaned.title,
                body: cleaned.body.as_deref().map(redact_text),
                append: cleaned.append.as_deref().map(redact_text),
                kind: cleaned.kind.map(|kind| kind.as_str().to_owned()),
                tags: cleaned.tags,
            },
            writer: Writer {
                root: provenance.root.as_ref().map(|root| root.to_string_lossy().into_owned()),
                source: provenance.source.clone(),
                via_override: provenance.via_override,
                author: provenance.author.clone(),
            },
            applying: None,
        })
    }

    /// The view the change was made in.
    pub fn view(&self) -> View {
        View {
            repo: self.repo.as_ref().map(Repo::project_key),
            global: self.global,
        }
    }

    /// True when the change replaces the note's title, body, kind or tags (rather than only adding
    /// text to its body), which must not overwrite what someone else wrote meanwhile.
    pub fn replaces(&self) -> bool {
        let patch = &self.patch;

        patch.title.is_some() || patch.body.is_some() || patch.kind.is_some() || patch.tags.is_some()
    }

    /// The change as a patch for the store, writing as the person who made it and, when given, only
    /// to the note at revision `expected`. `None` when the kind it holds is not one (the file was
    /// edited).
    pub fn note_patch(&self, expected: Option<i64>) -> Option<NotePatch> {
        let kind = match &self.patch.kind {
            Some(name) => Some(NoteKind::parse(name)?),
            None => None,
        };

        Some(NotePatch {
            title: self.patch.title.clone(),
            body: self.patch.body.clone(),
            append: self.patch.append.clone(),
            kind,
            tags: self.patch.tags.clone(),
            provenance: self.provenance(),
            expected_revision: expected,
        })
    }

    /// Who made the change.
    pub fn provenance(&self) -> Provenance {
        Provenance {
            root: self.writer.root.as_ref().map(PathBuf::from),
            source: self.writer.source.clone(),
            via_override: self.writer.via_override,
            author: self.writer.author.clone(),
            original_added_by: None,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct List {
    #[serde(default)]
    updates: Vec<Update>,
}

/// The queue of changes, kept in a file.
#[derive(Debug, Clone)]
pub struct Queue {
    folder: PathBuf,
    path: PathBuf,
    /// The most bytes written to the file ([`MAX_WRITE_BYTES`]).
    limit: usize,
}

impl Queue {
    /// The queue kept next to the local database `database`.
    pub fn beside(database: &Path) -> Self {
        let path = database.with_file_name(FILE);
        let folder = path.parent().map(Path::to_path_buf).unwrap_or_default();

        Self {
            folder,
            path,
            limit: MAX_WRITE_BYTES,
        }
    }

    /// Holds the lock of the file while a change reads it and writes it back.
    fn locked(&self) -> Result<FileLock, String> {
        FileLock::exclusive(&self.path, "list of changes waiting for the team database")
    }

    /// The queue as it is; nothing queued when there is no file. A file that cannot be read or
    /// understood is an error, never an empty queue: the next change would write over it.
    fn read(&self) -> Result<List, String> {
        let bytes = match read_private(&self.path, MAX_BYTES) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(List::default()),
            Err(error) => return Err(error),
        };

        serde_json::from_slice(&bytes).map_err(|_| format!("{FILE} is not a list of changes this version understands"))
    }

    /// Writes `list`, refusing more than `limit` bytes: [`Queue::add`] leaves room below
    /// [`MAX_BYTES`], so the marks and revisions the replay adds to what is listed always fit.
    fn write_within(&self, list: &List, limit: usize) -> Result<(), String> {
        if list.updates.is_empty() {
            return match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(format!("removing the list of changes waiting failed: {error}")),
            };
        }

        let bytes = serde_json::to_vec(list).map_err(|error| error.to_string())?;

        if bytes.len() > limit {
            return Err(format!(
                "too many changes are waiting for the team database to keep another ({} MB at most): connect to it \
                 so they are applied, then repeat the change",
                limit.div_ceil(1024 * 1024)
            ));
        }

        write_atomic(
            &self.folder,
            &self.path,
            &bytes,
            "pending-updates-",
            "list of changes waiting for the team database",
        )
    }

    /// The changes meant for the team database `team`, in the order they were made.
    pub fn entries(&self, team: &str) -> Result<Vec<Update>, String> {
        let list = self.read()?;

        Ok(list.updates.into_iter().filter(|update| update.team == team).collect())
    }

    /// Adds `update` at the end, numbering it. Nothing is added, with the reason as the error, when
    /// the file is full or cannot be read.
    pub fn add(&self, mut update: Update) -> Result<(), String> {
        // The first change may be made before anything made the data folder (nothing was saved here).
        private_folder(&self.folder, "data folder")?;

        let _lock = self.locked()?;
        let mut list = self.read()?;

        update.seq = list
            .updates
            .iter()
            .map(|waiting| waiting.seq)
            .max()
            .map_or(1, |last| last + 1);
        list.updates.push(update);

        self.write_within(&list, self.limit)
    }

    /// Takes the update numbered `seq` off the queue (the file goes when nothing is left).
    pub fn remove(&self, seq: u64) -> Result<(), String> {
        let _lock = self.locked()?;
        let mut list = self.read()?;
        let before = list.updates.len();
        list.updates.retain(|update| update.seq != seq);

        if list.updates.len() == before {
            return Ok(());
        }

        self.write_within(&list, MAX_BYTES)
    }

    /// Marks the update numbered `seq` as being applied to the note as it was read (`applying`), under
    /// the lock, before it is sent. Nothing is marked, with the reason as the error, when the file
    /// cannot be read or written: the update must not be sent then.
    pub fn mark_applying(&self, seq: u64, applying: Applying) -> Result<(), String> {
        let _lock = self.locked()?;
        let mut list = self.read()?;

        let Some(update) = list.updates.iter_mut().find(|update| update.seq == seq) else {
            return Ok(());
        };

        if update.applying == Some(applying) {
            return Ok(());
        }

        update.applying = Some(applying);

        self.write_within(&list, MAX_BYTES)
    }

    /// Tells the changes still waiting for team note `id` of the team database `team` that the note
    /// went from revision `from` to `to` (its next one: the caller checks that nothing was written in
    /// between): the replay has just written it, so a later change of the same person follows it and is
    /// not a teammate's change to fall out with. `read_at` is when the note was last changed as the
    /// replay read it, before the write.
    ///
    /// A change made against `from` follows it. A change that named no revision does when the note
    /// had not been changed since that change was made (`read_at` is not later), else someone else
    /// changed the note after it, which it must not hide. A change made against another revision
    /// keeps it: that one was read before something else changed the note.
    pub fn rebase(&self, team: &str, id: i64, from: i64, to: i64, read_at: i64) -> Result<(), String> {
        let _lock = self.locked()?;
        let mut list = self.read()?;
        let mut changed = false;

        for update in list
            .updates
            .iter_mut()
            .filter(|update| update.team == team && update.id == id)
        {
            let follows = match update.base_revision {
                Some(base) => base == from,
                None => read_at <= update.at,
            };

            if follows && update.base_revision != Some(to) {
                update.base_revision = Some(to);
                changed = true;
            }
        }

        if !changed {
            return Ok(());
        }

        self.write_within(&list, MAX_BYTES)
    }

    /// The right to apply the queue, when no other process holds it: held until dropped. A process
    /// that gets none leaves the queue to the one that has it.
    pub fn replaying(&self) -> Result<Option<FileLock>, String> {
        FileLock::try_exclusive(
            &self.folder.join(REPLAY_FILE),
            "list of changes waiting for the team database",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(team: &str, id: i64, text: &str) -> Update {
        let patch = NotePatch {
            append: Some(text.to_owned()),
            ..NotePatch::default()
        };

        Update::new(team, id, &View::around(&crate::store::Target::Global), &patch, 100).expect("update")
    }

    #[test]
    fn updates_are_numbered_and_listed_in_order_for_their_team_database() {
        let dir = tempfile::tempdir().expect("dir");
        let queue = Queue::beside(&dir.path().join("memory.sqlite3"));

        queue.add(update("a", 1, "first")).expect("add");
        queue.add(update("b", 2, "other team")).expect("add");
        queue.add(update("a", 1, "second")).expect("add");

        let waiting = queue.entries("a").expect("entries");
        let appended: Vec<_> = waiting.iter().map(|update| update.patch.append.as_deref()).collect();
        assert_eq!(appended, [Some("first"), Some("second")]);
        assert!(waiting[0].seq < waiting[1].seq);

        queue.remove(waiting[0].seq).expect("remove");
        assert_eq!(queue.entries("a").expect("entries").len(), 1);
        assert_eq!(queue.entries("b").expect("entries").len(), 1);

        queue.remove(waiting[1].seq).expect("remove");
        queue
            .remove(queue.entries("b").expect("entries")[0].seq)
            .expect("remove");
        assert!(!dir.path().join(FILE).exists(), "the file goes with the last update");
    }

    #[test]
    fn a_file_that_cannot_be_read_is_not_written_over() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join(FILE), "not json").expect("write");
        let queue = Queue::beside(&dir.path().join("memory.sqlite3"));

        assert!(queue.entries("a").is_err());
        assert!(queue.add(update("a", 1, "x")).is_err(), "refused, not written over");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(FILE)).expect("read"),
            "not json"
        );
    }

    #[test]
    fn a_full_queue_refuses_the_next_change_and_keeps_what_it_holds() {
        let dir = tempfile::tempdir().expect("dir");
        let mut queue = Queue::beside(&dir.path().join("memory.sqlite3"));
        queue.limit = 100 * 1024;
        // The longest text a note takes, in characters of four bytes each.
        let big = "\u{1F600}".repeat(crate::limits::BODY_MAX);

        let mut added = 0;
        let error = loop {
            match queue.add(update("a", 1, &big)) {
                Ok(()) => added += 1,
                Err(error) => break error,
            }

            assert!(added < 100, "the queue never filled");
        };

        assert!(error.contains("too many changes"), "{error}");
        assert_eq!(queue.entries("a").expect("entries").len(), added, "nothing was dropped");
    }

    #[test]
    fn a_full_queue_can_still_be_marked_and_rebased_by_the_replay() {
        let dir = tempfile::tempdir().expect("dir");
        let mut queue = Queue::beside(&dir.path().join("memory.sqlite3"));
        let big = "\u{1F600}".repeat(crate::limits::BODY_MAX);

        queue.add(update("a", 1, &big)).expect("first");
        queue.limit = std::fs::metadata(&queue.path)
            .expect("size")
            .len()
            .try_into()
            .expect("fits");

        let first = queue.entries("a").expect("entries").remove(0).seq;

        queue
            .mark_applying(
                first,
                Applying {
                    revision: 1,
                    updated_at: 1,
                },
            )
            .expect("the mark fits past what add accepts");
        queue.rebase("a", 1, 1, 2, 1).expect("the rebase fits too");
        queue.remove(first).expect("remove");
    }

    #[test]
    fn only_one_process_applies_the_queue_at_a_time() {
        let dir = tempfile::tempdir().expect("dir");
        let queue = Queue::beside(&dir.path().join("memory.sqlite3"));

        let first = queue.replaying().expect("lock").expect("free");
        assert!(queue.replaying().expect("lock").is_none(), "taken");

        drop(first);
        assert!(queue.replaying().expect("lock").is_some(), "free again");
    }

    fn replacement(base: Option<i64>, at: i64) -> Update {
        let patch = NotePatch {
            body: Some("new".to_owned()),
            expected_revision: base,
            ..NotePatch::default()
        };

        Update::new("a", 1, &View::around(&crate::store::Target::Global), &patch, at).expect("update")
    }

    fn bases(queue: &Queue) -> Vec<Option<i64>> {
        queue
            .entries("a")
            .expect("entries")
            .iter()
            .map(|update| update.base_revision)
            .collect()
    }

    #[test]
    fn a_rebase_moves_a_change_made_against_the_revision_or_against_none_until_the_note_was_last_changed() {
        let dir = tempfile::tempdir().expect("dir");
        let queue = Queue::beside(&dir.path().join("memory.sqlite3"));

        queue.add(replacement(Some(1), 100)).expect("add");
        queue.add(replacement(Some(7), 100)).expect("add");
        queue.add(replacement(None, 100)).expect("add");
        queue.add(replacement(None, 50)).expect("add");

        // The note was last changed at 90 as the replay read it: the change made at 50 is older than
        // that, so something else changed the note after it.
        queue.rebase("a", 1, 1, 2, 90).expect("rebase");

        assert_eq!(bases(&queue), [Some(2), Some(7), Some(2), None]);
    }

    #[test]
    fn a_mark_is_kept_in_the_file_for_the_update_it_names_alone() {
        let dir = tempfile::tempdir().expect("dir");
        let queue = Queue::beside(&dir.path().join("memory.sqlite3"));
        queue.add(update("a", 1, "first")).expect("add");
        queue.add(update("a", 1, "second")).expect("add");

        let waiting = queue.entries("a").expect("entries");
        assert!(waiting.iter().all(|update| update.applying.is_none()));

        let read = Applying {
            revision: 3,
            updated_at: 90,
        };
        queue.mark_applying(waiting[1].seq, read).expect("mark");

        let marked: Vec<_> = queue
            .entries("a")
            .expect("entries")
            .iter()
            .map(|update| update.applying)
            .collect();
        assert_eq!(marked, [None, Some(read)]);

        // An older file without the field still reads.
        let text = std::fs::read_to_string(dir.path().join(FILE)).expect("read");
        assert_eq!(text.matches("applying").count(), 1);
    }

    #[test]
    fn an_appended_text_is_masked_when_it_is_queued() {
        // Built at run time: a token written out would trip secret scanners on the source.
        let token = format!("ghp_{}", "a".repeat(36));

        let queued = update("a", 1, &format!("key {token}"));

        let append = queued.patch.append.expect("append");
        assert!(!append.contains(&token), "{append}");
        assert!(append.starts_with("key "), "{append}");
    }

    #[test]
    fn an_update_keeps_its_view_revision_and_writer_and_masks_what_replaces_text() {
        // Built at run time: a token written out would trip secret scanners on the source.
        let token = format!("ghp_{}", "a".repeat(36));
        let patch = NotePatch {
            body: Some(format!("token {token}")),
            kind: Some(NoteKind::Fix),
            provenance: Provenance {
                source: Some("client".to_owned()),
                author: Author::new("Ada", Some("client")),
                ..Provenance::default()
            },
            expected_revision: Some(4),
            ..NotePatch::default()
        };
        let view = View {
            repo: Some(ProjectKey {
                key: "git:example.com/o/r".to_owned(),
                root: PathBuf::from("work-tree"),
                remote: Some("example.com/o/r".to_owned()),
            }),
            global: true,
        };

        let update = Update::new("a", 7, &view, &patch, 100).expect("update");
        assert!(!update.patch.body.as_deref().expect("body").contains(&token), "masked");
        assert_eq!(update.view(), view);
        assert_eq!(update.base_revision, Some(4));
        assert!(update.replaces());

        let back = update.note_patch(Some(9)).expect("patch");
        assert_eq!(back.expected_revision, Some(9));
        assert_eq!(back.kind, Some(NoteKind::Fix));
        assert_eq!(back.provenance.author, Author::new("Ada", Some("client")));
        assert_eq!(back.provenance.source.as_deref(), Some("client"));
    }
}
