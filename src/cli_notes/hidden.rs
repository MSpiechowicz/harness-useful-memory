//! `notes hide`, `notes unhide`, and the hidden team notes `notes list --hidden` and
//! `notes get --hidden` show (see the parent module docs). The hidden list is the local file's
//! ([`crate::store::hidden`]); the notes themselves are read from the team database directly, past
//! the store that leaves them out.

use std::process::ExitCode;

use serde_json::json;

use super::{Opened, not_found, usage};
use crate::backend;
use crate::cli;
use crate::limits::GET_IDS_MAX;
use crate::store::listing::{compare, page_limit};
use crate::store::split::is_local_id;
use crate::store::{
    HiddenNote, LangFilter, MemoryStore, Note, NoteListQuery, NotePage, NoteRow, Scope, StoreError, clean,
};

/// `notes hide <id>`.
pub(super) fn hide(args: &[Option<&str>]) -> ExitCode {
    let Some((id, repos, json)) = cli::id_options(args) else {
        return usage();
    };

    let Some(mut opened) = Opened::open(&repos) else {
        return ExitCode::FAILURE;
    };

    if let Some(refused) = refusal(&opened, id) {
        return refused;
    }

    match hide_note(&mut opened, id) {
        Ok(Some(title)) => {
            cli::print_either(
                json,
                json!({ "hidden": id }),
                &format!("hid note {id} {}", clean::line(&title)),
            );
            ExitCode::SUCCESS
        }
        Ok(None) => not_found(id),
        Err(error) => opened.failed(&error),
    }
}

/// `notes unhide <id>`.
pub(super) fn unhide(args: &[Option<&str>]) -> ExitCode {
    let Some((id, repos, json)) = cli::id_options(args) else {
        return usage();
    };

    let Some(mut opened) = Opened::open(&repos) else {
        return ExitCode::FAILURE;
    };

    if let Some(refused) = refusal(&opened, id) {
        return refused;
    }

    let Some(hidden) = opened.hidden.as_mut() else {
        return not_found(id);
    };

    match hidden.unhide(id) {
        Ok(true) => {
            cli::print_either(json, json!({ "unhidden": id }), &format!("unhid note {id}"));
            ExitCode::SUCCESS
        }
        Ok(false) => not_found(id),
        Err(error) => opened.failed(&error),
    }
}

/// Why `id` cannot be hidden or shown again here, printed (exit 1); `None` when it can be.
fn refusal(opened: &Opened, id: i64) -> Option<ExitCode> {
    if opened.hidden.is_none() {
        eprintln!(
            "harness-memory: no team notes are read here (no team database is set, or repository_notes and \
             global_notes are both \"local\"), so there is no team note to hide"
        );
        return Some(ExitCode::FAILURE);
    }

    if is_local_id(id) {
        eprintln!("harness-memory: note {id} is saved on this computer, delete it instead");
        return Some(ExitCode::FAILURE);
    }

    None
}

/// Hides the visible team note `id` (one already hidden stays so); its title, `None` when there
/// is no such note.
fn hide_note(opened: &mut Opened, id: i64) -> Result<Option<String>, StoreError> {
    if let Some(known) = in_view(opened)?.into_iter().find(|hidden| hidden.id == id) {
        return Ok(Some(known.label));
    }

    let Some(note) = opened.note(id)? else {
        return Ok(None);
    };

    if let Some(hidden) = opened.hidden.as_mut() {
        hidden.hide(&note)?;
    }

    Ok(Some(note.title))
}

/// The hidden team notes of the view: a scope read from the team database, the repository of the
/// view for repository notes.
pub(super) fn in_view(opened: &mut Opened) -> Result<Vec<HiddenNote>, StoreError> {
    let Some(hidden) = opened.hidden.as_mut() else {
        return Ok(Vec::new());
    };

    let listed = hidden.list();
    let reads_team = |scope| crate::open::notes_place(&opened.selection, scope).reads_team();
    let repo = opened.view.repo.as_ref().map(|key| key.key.as_str());
    let every = opened.view.sees_every_repo();

    Ok(listed
        .into_iter()
        .filter(|note| reads_team(note.scope))
        .filter(|note| match note.scope {
            Scope::Repo => note
                .project
                .as_deref()
                .is_some_and(|project| every || Some(project) == repo),
            Scope::Global => opened.view.global,
        })
        .collect())
}

/// How many team notes of the view are hidden, as the store leaves them out: the rows of notes
/// the team database no longer has (or gave their id to a newer note) leave the list first when a
/// prune is due ([`crate::store::HiddenNotes::prune_if_due`], which the store's reads run too).
pub(super) fn count(opened: &mut Opened) -> Result<usize, StoreError> {
    let due = opened.hidden.as_mut().is_some_and(|hidden| hidden.prune_due());

    if due {
        let mut team = team_store(opened)?;

        if let Some(hidden) = opened.hidden.as_mut() {
            hidden.prune_if_due(team.as_mut())?;
        }
    }

    Ok(in_view(opened)?.len())
}

/// The hidden team note `id` of the view, read from the team database; `None` when it is not
/// hidden, or gone (then it leaves the hidden list).
pub(super) fn note(opened: &mut Opened, id: i64) -> Result<Option<Note>, StoreError> {
    let wanted: Vec<HiddenNote> = in_view(opened)?.into_iter().filter(|hidden| hidden.id == id).collect();
    Ok(read(opened, &wanted)?.into_iter().next())
}

/// One page of the hidden team notes of the view that `query` keeps: filtered, counted, sorted and
/// cut as a store lists notes (the text matches the title, the body or the tags, ignoring case).
pub(super) fn page(opened: &mut Opened, query: &NoteListQuery) -> Result<NotePage, StoreError> {
    let entries = in_view(opened)?;
    let notes = read(opened, &entries)?;

    let text = query.text.trim().to_lowercase();
    let entity = query.entity.as_deref().map(clean::normalize);
    let kept: Vec<Note> = notes
        .into_iter()
        .filter(|note| text.is_empty() || contains(note, &text))
        .filter(|note| LangFilter::keeps(query.lang, note.lang))
        .filter(|note| {
            entity
                .as_ref()
                .is_none_or(|wanted| note.entities.iter().any(|name| clean::normalize(name) == *wanted))
        })
        .collect();

    let counted: Vec<_> = kept.iter().map(|note| (note.kind, note.scope, 1)).collect();
    let mut rows: Vec<NoteRow> = kept
        .into_iter()
        .filter(|note| query.kind.is_none_or(|kind| kind == note.kind))
        .filter(|note| query.scope.is_none_or(|scope| scope == note.scope))
        .map(NoteRow::from)
        .collect();

    rows.sort_by(|a, b| compare(query, a, b));
    let rows = rows.into_iter().skip(query.offset).take(page_limit(query)).collect();
    Ok(NotePage::new(counted, query, rows))
}

/// True when the title, the body or a tag of `note` contains the lowercase `text`.
fn contains(note: &Note, text: &str) -> bool {
    let fields = [note.title.as_str(), note.body.as_str()];
    fields.iter().any(|field| field.to_lowercase().contains(text))
        || note.tags.iter().any(|tag| tag.to_lowercase().contains(text))
}

/// The team notes of `hidden`, read from the team database in the view without counting a hit; a
/// note the team database no longer has (or gave its id to a newer note) leaves the hidden list.
fn read(opened: &mut Opened, hidden: &[HiddenNote]) -> Result<Vec<Note>, StoreError> {
    if hidden.is_empty() {
        return Ok(Vec::new());
    }

    let mut team = team_store(opened)?;
    let mut found: Vec<Note> = Vec::new();
    let mut gone: Vec<i64> = Vec::new();

    for chunk in hidden.chunks(GET_IDS_MAX) {
        let ids: Vec<i64> = chunk.iter().map(|hidden| hidden.id).collect();
        let notes = team.peek(&opened.view, &ids)?;

        for entry in chunk {
            match notes.iter().find(|note| note.id == entry.id) {
                Some(note) if note.created_at == entry.team_created_at => found.push(note.clone()),
                _ => gone.push(entry.id),
            }
        }
    }

    if let Some(list) = opened.hidden.as_mut().filter(|_| !gone.is_empty()) {
        list.remove(&gone);
    }

    Ok(found)
}

/// The team database itself, past the store that leaves the hidden notes out.
fn team_store(opened: &Opened) -> Result<Box<dyn MemoryStore>, StoreError> {
    match backend::open(&opened.selection.backend) {
        Some(Ok(team)) => Ok(team.store),
        Some(Err(message)) => Err(StoreError::Backend(message)),
        None => Err(StoreError::Invalid("no team database is set".to_owned())),
    }
}
