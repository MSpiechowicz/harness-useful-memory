//! Notes: create-or-update by title, patch, read (counting hits) and delete.
//!
//! [`prepare`] and [`prepare_patch`] clean and check the agent's text before any request is sent.

use std::collections::HashSet;
use std::path::PathBuf;

use mongodb::bson::{Bson, Document, doc};

use super::search::terms;
use super::{Ctx, ENTITIES, Fail, NOTES, RELATIONS, graph, int, now, project_of, scope_of, scope_value, strings};
use super::{PATH_MAX, cut, flag, id_only, note_fields, number, text, visible};
use crate::limits::{BODY_MAX, ENTITY_MAX, GET_IDS_MAX};
use crate::store::{
    Forgotten, Note, NoteDraft, NoteKind, NotePatch, Provenance, Saved, StoreError, Target, View, clean,
};
use crate::support::redact::redact_text;

/// A [`NoteDraft`] cleaned and checked.
pub(super) struct CleanDraft {
    title: String,
    title_norm: String,
    body: String,
    kind: NoteKind,
    tags: Vec<String>,
    entities: Vec<String>,
    writer: Writer,
}

/// A [`NotePatch`] cleaned and checked; `body` and `append` are not masked yet (see [`update`]).
pub(super) struct CleanPatch {
    title: Option<String>,
    body: Option<String>,
    append: Option<String>,
    kind: Option<NoteKind>,
    tags: Option<Vec<String>>,
    writer: Writer,
}

/// A [`Provenance`] as stored; an unknown writer is stored as such (`null` root and source).
struct Writer {
    root: Option<String>,
    source: Option<String>,
    via_override: bool,
}

impl Writer {
    fn new(provenance: &Provenance) -> Self {
        Self {
            root: provenance.root.as_ref().map(|root| root.to_string_lossy().into_owned()),
            source: clean::source(provenance.source.as_deref()),
            via_override: provenance.via_override,
        }
    }

    /// The writer fields of a note document.
    fn fields(&self) -> Document {
        doc! {
            "source": self.source.clone(),
            "writer_root": self.root.clone(),
            "via_override": self.via_override,
        }
    }
}

/// A stored note with the normalized names of its entities.
pub(super) struct Stored {
    pub note: Note,
    pub norms: Vec<String>,
}

pub(super) fn prepare(draft: &NoteDraft) -> Result<CleanDraft, StoreError> {
    let title = clean::title(&draft.title)?;

    Ok(CleanDraft {
        title_norm: clean::normalize(&title),
        title,
        body: clean::body(&draft.body)?,
        kind: draft.kind,
        tags: clean::tags(&draft.tags)?,
        entities: clean::entities(&draft.entities)?,
        writer: Writer::new(&draft.provenance),
    })
}

pub(super) fn prepare_patch(patch: &NotePatch) -> Result<CleanPatch, StoreError> {
    let empty = patch.title.is_none()
        && patch.body.is_none()
        && patch.append.is_none()
        && patch.kind.is_none()
        && patch.tags.is_none();

    if empty {
        return Err(StoreError::Invalid(
            "nothing to change: give title, body, append, kind or tags".to_owned(),
        ));
    }

    Ok(CleanPatch {
        title: patch.title.as_deref().map(clean::title).transpose()?,
        body: patch.body.as_deref().map(clean::body_unmasked).transpose()?,
        append: patch.append.as_deref().map(clean::body_unmasked).transpose()?,
        kind: patch.kind,
        tags: patch.tags.as_deref().map(clean::tags).transpose()?,
        writer: Writer::new(&patch.provenance),
    })
}

pub(super) fn check_ids(ids: &[i64]) -> Result<(), StoreError> {
    if ids.is_empty() || ids.len() > GET_IDS_MAX {
        return Err(StoreError::Invalid(format!("ids must list 1-{GET_IDS_MAX} note ids")));
    }

    Ok(())
}

pub(super) fn remember(ctx: &mut Ctx<'_>, target: &Target, draft: &CleanDraft) -> Result<Saved, Fail> {
    let project = project_of(target);
    let scope = scope_value(project.as_deref());
    let now = now();

    let existing = ctx.find_one(
        NOTES,
        doc! { "project": scope.clone(), "title_norm": &draft.title_norm },
        doc! {
            "_id": number("$_id"),
            "writer_root": cut("$writer_root", PATH_MAX),
            "via_override": flag("$via_override"),
            "revision": number("$revision"),
        },
    )?;

    let writer = &draft.writer;
    let mut fields = doc! {
        "title": &draft.title,
        "body": &draft.body,
        "kind": draft.kind.as_str(),
        "tags": &draft.tags,
        "terms": terms(&draft.title, &draft.body, &draft.tags),
        "updated_at": now,
    };
    fields.extend(writer.fields());

    let (id, created, revision) = match existing {
        Some(note) => {
            let id = int(&note, "_id");
            let known_root = text(&note, "writer_root");
            let known_override = note.get_bool("via_override").unwrap_or(false);

            // Any folder can claim a repository's remote, and `project_path` can name any
            // checkout: a same-title write from another checkout, or through `project_path`, must
            // replace the note on purpose, by id (see the SQLite store).
            let other_checkout = known_root.is_some() && known_root != writer.root;

            if project.is_some() && (other_checkout || known_override || writer.via_override) {
                return Err(Fail::Store(StoreError::Conflict(id)));
            }

            let revision = int(&note, "revision");
            let changed = ctx.update_one(
                NOTES,
                doc! { "_id": id, "revision": revision },
                doc! { "$set": fields, "$inc": { "revision": 1_i64 } },
            )?;

            if changed == 0 {
                return Err(Fail::Retry);
            }

            (id, false, revision + 1)
        }
        None => {
            let id = ctx.next_note_id()?;
            let mut note = doc! {
                "_id": id,
                "project": scope.clone(),
                "title_norm": &draft.title_norm,
                "created_at": now,
                "revision": 1_i64,
                "hits": 0_i64,
                "entities": Vec::<String>::new(),
            };
            note.extend(fields);
            ctx.insert_one(NOTES, note)?;

            (id, true, 1)
        }
    };

    if !draft.entities.is_empty() {
        let mut norms: Vec<String> = Vec::new();

        for name in &draft.entities {
            norms.push(graph::ensure_entity(ctx, project.as_deref(), name)?.norm);
        }

        ctx.update_one(
            NOTES,
            doc! { "_id": id },
            doc! { "$addToSet": { "entities": { "$each": norms } } },
        )?;
    }

    Ok(Saved {
        id,
        created,
        revision,
        scope: scope_of(project.as_deref()),
        project,
    })
}

/// Applies `patch` at the revision it read. The body is masked after `append` is joined to it, so
/// a secret split across the two is masked as a whole (see [`clean::join_masked`]).
pub(super) fn update(ctx: &mut Ctx<'_>, view: &View, id: i64, patch: &CleanPatch) -> Result<Saved, Fail> {
    let Some(Stored { note, .. }) = load(ctx, view, id)? else {
        return Err(Fail::Store(not_found(id)));
    };

    let title = patch.title.clone().unwrap_or_else(|| note.title.clone());
    let title_norm = clean::normalize(&title);
    ensure_title_free(ctx, &note, &title_norm)?;

    let body = patch.body.as_deref().unwrap_or(&note.body);
    let body = match &patch.append {
        Some(append) => clean::join_masked(body, append),
        None => redact_text(body),
    };

    let length = body.chars().count();
    if length > BODY_MAX {
        return Err(Fail::Store(StoreError::Invalid(format!(
            "body would be {length} characters (most {BODY_MAX}); shorten it with body instead of append"
        ))));
    }

    let kind = patch.kind.unwrap_or(note.kind);
    let tags = patch.tags.clone().unwrap_or_else(|| note.tags.clone());

    let mut fields = doc! {
        "title": &title,
        "title_norm": &title_norm,
        "terms": terms(&title, &body, &tags),
        "body": body,
        "kind": kind.as_str(),
        "tags": tags,
        "updated_at": now(),
    };
    fields.extend(patch.writer.fields());

    let changed = ctx.update_one(
        NOTES,
        doc! { "_id": id, "revision": note.revision },
        doc! { "$set": fields, "$inc": { "revision": 1_i64 } },
    )?;

    if changed == 0 {
        return Err(Fail::Retry);
    }

    Ok(Saved {
        id,
        created: false,
        revision: note.revision + 1,
        scope: note.scope,
        project: note.project,
    })
}

/// The visible notes among `ids`, each read as it was before this read's hit (when `count`).
pub(super) fn get(ctx: &mut Ctx<'_>, view: &View, ids: &[i64], count: bool) -> Result<Vec<Note>, Fail> {
    let mut seen: HashSet<i64> = HashSet::new();
    let mut found: Vec<Note> = Vec::new();

    for &id in ids {
        if !seen.insert(id) {
            continue;
        }

        let read = if count {
            let mut filter = visible(view);
            filter.insert("_id", id);

            ctx.find_one_and_update(
                NOTES,
                filter,
                doc! { "$inc": { "hits": 1_i64 } },
                false,
                false,
                note_fields(),
            )?
            .as_ref()
            .map(stored)
        } else {
            load(ctx, view, id)?
        };

        if let Some(Stored { mut note, norms }) = read {
            note.entities = entities_of(ctx, note.project.as_deref(), &norms)?;
            found.push(note);
        }
    }

    Ok(found)
}

pub(super) fn forget(ctx: &mut Ctx<'_>, view: &View, id: i64) -> Result<usize, Fail> {
    let Some(Stored { note, norms }) = load(ctx, view, id)? else {
        return Err(Fail::Store(not_found(id)));
    };

    ctx.delete_one(NOTES, doc! { "_id": id })?;
    ctx.update_many(
        RELATIONS,
        doc! { "note_id": id },
        doc! { "$set": { "note_id": Bson::Null } },
    )?;

    for norm in &norms {
        graph::prune_entity(ctx, note.project.as_deref(), norm)?;
    }

    Ok(1)
}

/// Deletes every note visible in `view`, one scope at a time: the notes, the note of their
/// relations (the relations stay), then the entities of the scope that no relation uses.
pub(super) fn forget_all(ctx: &mut Ctx<'_>, view: &View) -> Result<Forgotten, Fail> {
    let mut forgotten = Forgotten::default();

    if let Some(repo) = &view.repo {
        forgotten.repo = forget_scope(ctx, Some(&repo.key))?;
    }

    if view.global {
        forgotten.global = forget_scope(ctx, None)?;
    }

    Ok(forgotten)
}

/// Deletes the notes of the scope `project` (see [`forget_all`]); how many went.
fn forget_scope(ctx: &mut Ctx<'_>, project: Option<&str>) -> Result<usize, Fail> {
    let scope = scope_value(project);
    let deleted = ctx.delete_many(NOTES, doc! { "project": scope.clone() })?;

    ctx.update_many(
        RELATIONS,
        doc! { "project": scope.clone(), "note_id": { "$ne": Bson::Null } },
        doc! { "$set": { "note_id": Bson::Null } },
    )?;
    graph::prune_scope(ctx, project)?;

    Ok(usize::try_from(deleted).unwrap_or(usize::MAX))
}

/// The note of the scope `target` whose title (a cleaned one) compares equal to `title`, read
/// without counting a hit.
pub(super) fn find_by_title(ctx: &mut Ctx<'_>, target: &Target, title: &str) -> Result<Option<Note>, Fail> {
    let project = project_of(target);
    let filter = doc! { "project": scope_value(project.as_deref()), "title_norm": clean::normalize(title) };

    let Some(document) = ctx.find_one(NOTES, filter, note_fields())? else {
        return Ok(None);
    };

    let Stored { mut note, norms } = stored(&document);
    note.entities = entities_of(ctx, note.project.as_deref(), &norms)?;
    Ok(Some(note))
}

/// The note `id` when it is visible in `view` (its entity names are not filled in).
pub(super) fn load(ctx: &mut Ctx<'_>, view: &View, id: i64) -> Result<Option<Stored>, Fail> {
    let mut filter = visible(view);
    filter.insert("_id", id);

    Ok(ctx.find_one(NOTES, filter, note_fields())?.as_ref().map(stored))
}

/// The names of the entities `norms` of the scope `project`, by normalized name.
pub(super) fn entities_of(ctx: &mut Ctx<'_>, project: Option<&str>, norms: &[String]) -> Result<Vec<String>, Fail> {
    if norms.is_empty() {
        return Ok(Vec::new());
    }

    let found = ctx.find(
        ENTITIES,
        doc! { "project": scope_value(project), "name_norm": { "$in": norms } },
        doc! { "name_norm": 1 },
        0,
        doc! { "_id": 0, "name": cut("$name", ENTITY_MAX) },
    )?;

    Ok(found.iter().filter_map(|entity| text(entity, "name")).collect())
}

/// Parses a stored kind.
pub(super) fn kind_of(document: &Document) -> NoteKind {
    document
        .get_str("kind")
        .ok()
        .and_then(NoteKind::parse)
        .unwrap_or(NoteKind::Note)
}

/// The writer of a stored note.
pub(super) fn provenance_of(document: &Document) -> Provenance {
    Provenance {
        root: text(document, "writer_root").map(PathBuf::from),
        source: text(document, "source"),
        via_override: document.get_bool("via_override").unwrap_or(false),
    }
}

pub(super) fn stored(document: &Document) -> Stored {
    let project = text(document, "project");

    let note = Note {
        id: int(document, "_id"),
        scope: scope_of(project.as_deref()),
        project,
        kind: kind_of(document),
        title: text(document, "title").unwrap_or_default(),
        body: text(document, "body").unwrap_or_default(),
        tags: strings(document, "tags"),
        entities: Vec::new(),
        provenance: provenance_of(document),
        created_at: int(document, "created_at"),
        updated_at: int(document, "updated_at"),
        revision: int(document, "revision"),
        hits: int(document, "hits"),
    };

    Stored {
        note,
        norms: strings(document, "entities"),
    }
}

/// Fails when another note in the same scope already has `title_norm`.
fn ensure_title_free(ctx: &mut Ctx<'_>, note: &Note, title_norm: &str) -> Result<(), Fail> {
    let other = ctx.find_one(
        NOTES,
        doc! {
            "project": scope_value(note.project.as_deref()),
            "title_norm": title_norm,
            "_id": { "$ne": note.id },
        },
        id_only(),
    )?;

    match other {
        Some(other) => Err(Fail::Store(StoreError::Invalid(format!(
            "note {} already has this title: update that note or delete one of them",
            int(&other, "_id")
        )))),
        None => Ok(()),
    }
}

fn not_found(id: i64) -> StoreError {
    StoreError::NotFound(format!("note {id} not found in this repository or the global scope"))
}
