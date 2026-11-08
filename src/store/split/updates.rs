//! The changes to team notes made while the team database was unreachable, in a
//! [`SplitStore`](super::SplitStore) with the offline fallback (see [`crate::store::pending_updates`]
//! for the queue itself).
//!
//! [`SplitStore::queue_update`] keeps a change to a team note instead of applying it. Once the team
//! database is back, [`SplitStore::replay_offline_updates`] applies the queue in order, one process
//! at a time, and decides for each change what the team note allows:
//!
//! - A change that only adds text to the body is applied whatever happened to the note since, unless
//!   it was sent before (its entry says so, [`Applying`]), the note has been written since, and the
//!   body ends with that text: it was applied before the process stopped. A change that was never
//!   sent is applied even when the body ends with the same text.
//! - A change that replaces the title, body, kind or tags is applied only to a note that was not
//!   changed since: at the revision the caller read when it said so, else not updated later than the
//!   change was made. A change the note already has is dropped.
//! - Anything else, a note that was changed or deleted meanwhile, a note the person hid since or
//!   whose scope no longer saves to the team database (see [`SplitStore::team_writes`]), or a write the database refuses, loses nothing: the
//!   text of the change is saved as a note on this computer ("title (offline change)"), which is not
//!   listed as waiting for the team database, so it shows among the notes kept here for the person to
//!   decide, and the change leaves the queue. The text is the change's alone for a note the person
//!   hid, and is cut (with a note of it) when it is longer than a note can be.
//!
//! A database that cannot be reached, or any other failure that may pass, stops the replay and keeps
//! the rest in order, to be tried again after a while. One call applies at most [`REPLAY_MAX`]
//! changes, so that no tool call is held up for long, and the rest follows at the next call.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Fallback, Instant, RETRY_COPY, Saved, Scope, SplitStore, StoreError, View, narrow_by};
use super::{note_missing, offline_refusal};
use crate::limits::{BODY_MAX, TITLE_MAX};
use crate::store::pending_updates::{Applying, Update};
use crate::store::{HiddenNotes, Note, NoteDraft, NoteKind, NotePatch, ProjectKey, Target, clean};
use crate::support::redact::redact_text;

/// How many titles a change saved as a note on this computer tries before it gives up on one that
/// is not taken.
const TITLE_TRIES: usize = 50;

/// What the notes the queue could not apply are called.
const OFFLINE_CHANGE: &str = "offline change";

/// How many queued changes one call applies: the rest is left for the next call.
const REPLAY_MAX: usize = 100;

/// Ends the text of a change that was cut to fit in a note.
const CUT_MARK: &str = "\n[cut: the offline change was too long for a note]";

/// How the replay of a call ended.
enum Replay {
    /// Nothing is left to apply.
    Finished,
    /// [`REPLAY_MAX`] changes were applied and more wait: the next call goes on, without a delay.
    More,
    /// The rest has to be tried again after a while.
    Failed,
}

/// What the replay did with one queued change.
enum Applied {
    /// The team note has the change now, or had it already: it leaves the queue.
    Done,
    /// The change was saved as a note on this computer: it leaves the queue.
    Kept,
    /// The change could not be kept as a note either (the text was refused): it leaves the queue,
    /// as it would stop everything behind it for good.
    Dropped,
    /// Nothing was done, and the queue stops here to be tried again later.
    Later,
}

/// What the team note's state allows for a queued change.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Apply it.
    Apply,
    /// The note has it already.
    Present,
    /// The note changed since, or the change cannot be made to it: keep the text as a note here.
    Changed,
}

impl SplitStore {
    /// Keeps the change `patch` makes to team note `id` while the team database cannot be reached,
    /// to apply it when it is back; the reply says it is queued. The patch is checked as the store
    /// checks it, and the note is treated as the online path treats it as far as it can be told
    /// without reading it: a hidden note is not found, and a team note of a `remote+local` scope that
    /// saves to this computer is read-only (it is refused when the scope it is in cannot be told apart
    /// from one).
    pub(super) fn queue_update(&mut self, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError> {
        let Some(team_view) = narrow_by(view, |scope| self.configured(scope).reads_team()) else {
            return Err(note_missing(id));
        };

        if self.hidden_ids_strict()?.contains(&id) {
            return Err(note_missing(id));
        }

        let in_view = |scope: Scope| match scope {
            Scope::Repo => team_view.repo.is_some(),
            Scope::Global => team_view.global,
        };
        let read_only = [Scope::Repo, Scope::Global]
            .into_iter()
            .any(|scope| in_view(scope) && self.team_read_only(scope));

        if read_only {
            return Err(offline_refusal(id, "changed"));
        }

        let Some(fallback) = &self.fallback else {
            return Err(offline_refusal(id, "changed"));
        };

        // As the online path checks it: a body and a text to append that together are too long are
        // refused now, as they could never be applied.
        clean::patched_body("", &clean::patch(patch)?)?;

        let update = Update::new(&fallback.team, id, &team_view, patch, now())?;

        fallback
            .updates
            .add(update)
            .map_err(|message| StoreError::Backend(fallback.secrets.scrub(&message)))?;

        Ok(Saved {
            id,
            created: false,
            revision: 0,
            scope: if team_view.repo.is_some() {
                Scope::Repo
            } else {
                Scope::Global
            },
            project: team_view.repo.as_ref().map(|repo| repo.key.clone()),
            queued: true,
            not_listed: false,
        })
    }

    /// Applies the queued changes to the team database, which is reachable; what could not be
    /// applied stays queued and is tried again after a while.
    pub(super) fn replay_offline_updates(&mut self) {
        let Some(mut fallback) = self.fallback.take() else {
            return;
        };

        match self.apply_queue(&fallback) {
            Replay::Finished => {}
            Replay::More => fallback.sync_due = true,
            Replay::Failed => {
                fallback.sync_due = true;
                fallback.retry_at = Instant::now() + RETRY_COPY;
            }
        }

        self.fallback = Some(fallback);
    }

    /// Applies the queue in order, at most [`REPLAY_MAX`] changes.
    fn apply_queue(&mut self, fallback: &Fallback) -> Replay {
        let secrets = &fallback.secrets;

        let waiting = match fallback.updates.entries(&fallback.team) {
            Ok(waiting) => waiting,
            Err(message) => {
                eprintln!("harness-memory: {}", secrets.scrub(&message));
                return Replay::Finished;
            }
        };

        if waiting.is_empty() {
            return Replay::Finished;
        }

        // One process applies the queue at a time: another one is at it now, or will be.
        let _replaying = match fallback.updates.replaying() {
            Ok(Some(replaying)) => replaying,
            Ok(None) => return Replay::Failed,
            Err(message) => {
                eprintln!("harness-memory: {}", secrets.scrub(&message));
                return Replay::Failed;
            }
        };

        let mut applied = 0;
        let mut kept = 0;
        let mut dropped = 0;
        let mut replay = Replay::Finished;

        // Read again for every change: the one before may have rebased it.
        loop {
            let update = match fallback.updates.entries(&fallback.team) {
                Ok(left) => match left.into_iter().next() {
                    Some(update) => update,
                    None => break,
                },
                Err(message) => {
                    eprintln!("harness-memory: {}", secrets.scrub(&message));
                    replay = Replay::Failed;
                    break;
                }
            };

            if applied + kept + dropped >= REPLAY_MAX {
                replay = Replay::More;
                break;
            }

            match self.apply_update(fallback, &update) {
                Applied::Done => applied += 1,
                Applied::Kept => kept += 1,
                Applied::Dropped => dropped += 1,
                Applied::Later => {
                    replay = Replay::Failed;
                    break;
                }
            }

            if let Err(message) = fallback.updates.remove(update.seq) {
                eprintln!("harness-memory: {}", secrets.scrub(&message));
                replay = Replay::Failed;
                break;
            }
        }

        if applied > 0 {
            eprintln!("harness-memory: applied {applied} change(s) made offline to team notes");
        }

        if kept > 0 {
            eprintln!(
                "harness-memory: {kept} change(s) made offline could not be applied to the team note (it changed or went meanwhile), so the text is saved as note(s) on this computer"
            );
        }

        if dropped > 0 {
            eprintln!(
                "harness-memory: {dropped} change(s) made offline could not be applied to the team note nor kept as a note on this computer, and were dropped"
            );
        }

        replay
    }

    /// Applies one queued change as the module docs say.
    fn apply_update(&mut self, fallback: &Fallback, update: &Update) -> Applied {
        let hidden = match self.hidden.as_mut().map(HiddenNotes::try_ids) {
            Some(Ok(hidden)) => hidden,
            Some(Err(error)) => return self.stopped(fallback, &error),
            None => Vec::new(),
        };

        let view = update.view();

        let current = match self.team.peek(&view, &[update.id]) {
            Ok(notes) => notes.into_iter().next(),
            Err(error) => return self.stopped(fallback, &error),
        };

        let Some(current) = current else {
            return self.keep_as_note(fallback, update, None);
        };

        // A note the person hid since is not shown: nothing of it goes into a note on this computer.
        if hidden.contains(&current.id) {
            return self.keep_as_note(fallback, update, None);
        }

        // A note a scope that no longer saves to the team database holds (the team's notes are
        // read-only there, or kept on this computer only) is not changed.
        if !self.team_writes(current.scope) {
            return self.keep_as_note(fallback, update, Some(&current));
        }

        match verdict(update, &current) {
            Verdict::Present => {
                self.follow_earlier_write(fallback, update, &current);
                return Applied::Done;
            }
            Verdict::Changed => return self.keep_as_note(fallback, update, Some(&current)),
            Verdict::Apply => {}
        }

        // A replacement is made only to the revision that was read, so a teammate's change between
        // the read and the write is a conflict, not an overwrite.
        let expected = update.replaces().then_some(current.revision);

        let Some(patch) = update.note_patch(expected) else {
            return self.keep_as_note(fallback, update, Some(&current));
        };

        // Written before the change is sent: a process that stops after the write finds it, and so
        // knows that the change may be in the note already.
        let read = Applying {
            revision: current.revision,
            updated_at: current.updated_at,
        };

        if let Err(message) = fallback.updates.mark_applying(update.seq, read) {
            eprintln!("harness-memory: {}", fallback.secrets.scrub(&message));
            return Applied::Later;
        }

        match self.team.update(&view, current.id, &patch) {
            Ok(saved) => {
                // The changes still waiting for this note were made after this one by the same
                // person: they follow the revision just written, and a teammate's change after it
                // is what they fall out with. Another revision than the next one says that someone
                // else wrote in between, and then they do not follow it.
                if saved.revision == current.revision + 1 {
                    self.rebase_after(fallback, update, &read, saved.revision);
                }

                Applied::Done
            }
            Err(StoreError::NotFound(_)) => self.keep_as_note(fallback, update, None),
            Err(StoreError::Stale(_) | StoreError::Invalid(_) | StoreError::Conflict(_)) => {
                self.keep_as_note(fallback, update, Some(&current))
            }
            Err(error) => self.stopped(fallback, &error),
        }
    }

    /// An update already in the team note (a change it has, or text it ends with, as [`verdict`]
    /// found): when its entry says it was sent at revision R and the note is now at R + 1, the write
    /// was its own and the process stopped before it rebased the changes behind it, so that is done now.
    fn follow_earlier_write(&self, fallback: &Fallback, update: &Update, current: &Note) {
        let Some(read) = update.applying else {
            return;
        };

        if current.revision == read.revision + 1 {
            self.rebase_after(fallback, update, &read, current.revision);
        }
    }

    /// Tells the changes waiting behind `update` that the note went from the revision of `read` to
    /// `to`.
    fn rebase_after(&self, fallback: &Fallback, update: &Update, read: &Applying, to: i64) {
        let rebased = fallback
            .updates
            .rebase(&fallback.team, update.id, read.revision, to, read.updated_at);

        if let Err(message) = rebased {
            eprintln!("harness-memory: {}", fallback.secrets.scrub(&message));
        }
    }

    /// Stops the replay because of `error`: nothing was lost, and the rest is tried again later. A
    /// team database that cannot be reached is served from this computer again from now on.
    fn stopped(&mut self, fallback: &Fallback, error: &StoreError) -> Applied {
        if let StoreError::Unreachable(reason) = error {
            self.team.connection_lost(reason, true);
            self.go_down();
        }

        eprintln!(
            "harness-memory: changes made offline to team notes were not applied yet: {}",
            fallback.secrets.scrub(&error.to_string())
        );

        Applied::Later
    }

    /// Saves the text of `update` as a note on this computer (the team note `current` it was for
    /// is gone when `None`, or its text is not to be copied), not listed as waiting for the team
    /// database.
    ///
    /// A text that is refused never keeps the change in the queue, as everything behind it would wait
    /// for good: a text that is too long is cut, and any other refusal drops the change. The messages
    /// say nothing of the text.
    fn keep_as_note(&mut self, fallback: &Fallback, update: &Update, current: Option<&Note>) -> Applied {
        match self.save_as_note(update, current) {
            Ok(false) => Applied::Kept,
            Ok(true) => {
                eprintln!(
                    "harness-memory: the change made offline to note {} was too long for a note, so the note kept on this computer has the start of it",
                    update.id
                );
                Applied::Kept
            }
            Err(StoreError::Invalid(_)) => {
                eprintln!(
                    "harness-memory: the change made offline to note {} was refused as a note on this computer",
                    update.id
                );
                Applied::Dropped
            }
            Err(error) => {
                eprintln!(
                    "harness-memory: the change made offline to note {} could not be kept as a note on this computer: {}",
                    update.id,
                    fallback.secrets.scrub(&error.to_string())
                );
                Applied::Later
            }
        }
    }

    /// Saves the note that keeps `update`; whether its text was cut to fit.
    fn save_as_note(&mut self, update: &Update, current: Option<&Note>) -> Result<bool, StoreError> {
        let target = note_target(update, current);
        let title = self.free_title(&target, &note_title(update, current), update.seq)?;
        let (body, cut) = fit(note_body(update, current));

        let draft = NoteDraft {
            title,
            body,
            kind: note_kind(update, current),
            tags: update
                .patch
                .tags
                .clone()
                .or_else(|| current.map(|note| note.tags.clone()))
                .unwrap_or_default(),
            entities: Vec::new(),
            provenance: update.provenance(),
        };

        self.local.remember(&target, &draft)?;

        Ok(cut)
    }

    /// `wanted`, or the first of `wanted 2`, `wanted 3`, ... that no note of this computer's scope
    /// `target` has: saving under a title a note has would replace that note, and an earlier
    /// change kept the same way must stay. When that many are taken, the number of the change in
    /// the queue (`seq`) tells it apart, so a change is never refused for its title.
    fn free_title(&mut self, target: &Target, wanted: &str, seq: u64) -> Result<String, StoreError> {
        // Room for the number after the title.
        let base: String = wanted.chars().take(TITLE_MAX - 4).collect();

        for number in 1..=TITLE_TRIES {
            let candidate = if number == 1 {
                wanted.to_owned()
            } else {
                format!("{} {number}", base.trim_end())
            };

            if self.local.find_by_title(target, &candidate)?.is_none() {
                return Ok(candidate);
            }
        }

        let suffix = format!(" {seq}");
        let room = TITLE_MAX.saturating_sub(suffix.chars().count());
        let title: String = wanted.chars().take(room).collect();

        Ok(format!("{}{suffix}", title.trim_end()))
    }
}

/// `body`, or its start with [`CUT_MARK`] when it is longer than a note's body can be; whether it
/// was cut.
fn fit(body: String) -> (String, bool) {
    if body.chars().count() <= BODY_MAX {
        return (body, false);
    }

    let room = BODY_MAX - CUT_MARK.chars().count();
    let mut kept: String = body.chars().take(room).collect();
    kept.push_str(CUT_MARK);

    (kept, true)
}

/// What the team note `current` allows for `update`.
fn verdict(update: &Update, current: &Note) -> Verdict {
    let Some(patch) = update.note_patch(None) else {
        return Verdict::Changed;
    };

    let Ok(cleaned) = clean::patch(&patch) else {
        return Verdict::Changed;
    };

    if !update.replaces() {
        let Some(append) = update.patch.append.as_deref() else {
            return Verdict::Present;
        };

        // Only a change that was sent before, and a note written since, can have it already: the
        // same text at the end of the note says nothing about a change that was never sent.
        let sent = update.applying.is_some_and(|read| current.revision > read.revision);
        let ends = current.body.ends_with(append) || current.body.ends_with(&redact_text(append));

        return if sent && ends { Verdict::Present } else { Verdict::Apply };
    }

    // What the change would leave: a note that already is that has nothing to apply.
    let Ok(body) = clean::patched_body(&current.body, &cleaned) else {
        return Verdict::Changed;
    };

    let same = cleaned.title.as_ref().is_none_or(|title| *title == current.title)
        && body == current.body
        && cleaned.kind.is_none_or(|kind| kind == current.kind)
        && cleaned.tags.as_ref().is_none_or(|tags| *tags == current.tags);

    if same {
        return Verdict::Present;
    }

    let unchanged = match update.base_revision {
        Some(base) => current.revision == base,
        None => current.updated_at <= update.at,
    };

    if unchanged { Verdict::Apply } else { Verdict::Changed }
}

/// The scope of this computer's note that keeps `update`: the team note's, else the one it was made in.
fn note_target(update: &Update, current: Option<&Note>) -> Target {
    let key = match current {
        Some(note) => note.project.as_deref(),
        None => update.repo.as_ref().map(|repo| repo.key.as_str()),
    };

    let Some(key) = key else {
        return Target::Global;
    };

    // The work-tree the change was made in, so the project row here keeps a real root.
    let known = update.repo.as_ref().filter(|repo| repo.key == key);
    let root = match known {
        Some(repo) => PathBuf::from(&repo.root),
        None => update.writer.root.as_ref().map(PathBuf::from).unwrap_or_default(),
    };

    Target::Repo(ProjectKey {
        key: key.to_owned(),
        root,
        remote: known.and_then(|repo| repo.remote.clone()),
    })
}

/// The title of the note that keeps `update`: the team note's title and ` (offline change)`, or
/// `Offline change to note N` when the team note is gone.
fn note_title(update: &Update, current: Option<&Note>) -> String {
    let Some(note) = current else {
        return format!("Offline change to note {}", update.id);
    };

    let suffix = format!(" ({OFFLINE_CHANGE})");
    let room = TITLE_MAX.saturating_sub(suffix.chars().count());
    let title: String = note.title.chars().take(room).collect();

    format!("{}{suffix}", title.trim_end())
}

/// The text of the note that keeps `update`: what the change would have made the body (the team
/// note's with the change), or, for a note that is gone, the text the change gives (a change of
/// title, kind or tags alone is written out).
fn note_body(update: &Update, current: Option<&Note>) -> String {
    let patch = &update.patch;

    if let Some(note) = current
        && let Some(made) = update
            .note_patch(None)
            .and_then(|patch| clean::patch(&patch).ok())
            .and_then(|cleaned| clean::patched_body(&note.body, &cleaned).ok())
    {
        return made;
    }

    let mut parts: Vec<String> = Vec::new();

    if let Some(body) = &patch.body {
        parts.push(body.clone());
    }

    if let Some(append) = &patch.append {
        parts.push(append.clone());
    }

    if parts.is_empty() {
        if let Some(title) = &patch.title {
            parts.push(format!("title: {title}"));
        }

        if let Some(kind) = &patch.kind {
            parts.push(format!("kind: {kind}"));
        }

        if let Some(tags) = &patch.tags {
            parts.push(format!("tags: {}", tags.join(" ")));
        }
    }

    parts.join("\n")
}

/// The kind of the note that keeps `update`: the one the change gives, else the team note's.
fn note_kind(update: &Update, current: Option<&Note>) -> NoteKind {
    update
        .patch
        .kind
        .as_deref()
        .and_then(NoteKind::parse)
        .or_else(|| current.map(|note| note.kind))
        .unwrap_or(NoteKind::Note)
}

/// Unix seconds now.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}
