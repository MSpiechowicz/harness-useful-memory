//! Scripts in a MongoDB team database: one document per script in `scripts`.
//!
//! `_id` comes from the `script_ids` counter in `meta`, `project` is the repository key (`null`
//! for global scripts, always stored so the unique `(project, name)` index covers global names).
//! A save of an existing name replaces the script in place and keeps its run statistics; a
//! concurrent first save of the same name hits the unique index and is retried as a replace.
//!
//! Reads cut every field one character over the limit this server writes and pass the script
//! through [`read_back`], which leaves out values over the limit and computes the trust hash from
//! the language and body actually read: the stored `sha256` is never read.

use mongodb::bson::{Bson, Document, doc};
use mongodb::options::ReturnDocument;

use super::notes::provenance_of;
use super::{Ctx, Fail, META, MongoStore, PATH_MAX, SCRIPTS, SOURCE_MAX, cut, flag, int, now, number};
use super::{project_of, scope_of, scope_value, text, visible};
use crate::limits::{SCRIPT_BODY_MAX, SCRIPT_DESCRIPTION_MAX, SCRIPT_NAME_MAX};
use crate::store::scripts::{Stored, read_back};
use crate::store::{Language, Provenance, SavedScript, Script, ScriptDraft, ScriptRef, ScriptStore, ScriptSummary};
use crate::store::{StoreError, Target, View, checked_draft, clean, script_sha256};

/// The `_id` of the script id counter in `meta`.
const SCRIPT_IDS: &str = "script_ids";

/// The longest language name, with room to spare.
const LANGUAGE_MAX: usize = 16;

impl ScriptStore for MongoStore {
    fn save_script(&mut self, target: &Target, draft: &ScriptDraft) -> Result<SavedScript, StoreError> {
        let draft = checked_draft(draft)?;
        self.write(|ctx| save(ctx, target, &draft))
    }

    fn list_scripts(&mut self, view: &View, query: Option<&str>) -> Result<Vec<ScriptSummary>, StoreError> {
        let mut filter = readable(view);

        if let Some(query) = query.map(str::trim).filter(|query| !query.is_empty()) {
            let pattern = literal_pattern(query);

            filter.insert(
                "$or",
                vec![
                    doc! { "name": { "$regex": &pattern, "$options": "i" } },
                    doc! { "description": { "$regex": &pattern, "$options": "i" } },
                ],
            );
        }

        let found = self
            .reader()?
            .find(SCRIPTS, filter, repo_first(), 0, script_fields())
            .map_err(Fail::into_store)?;

        Ok(found
            .iter()
            .filter_map(script_of)
            .map(|script| script.summary)
            .collect())
    }

    /// A document [`read_back`] leaves out counts as missing (a name then finds the global script).
    fn get_script(&mut self, view: &View, script: &ScriptRef) -> Result<Option<Script>, StoreError> {
        let found = find(&mut self.reader()?, view, script, 2, script_fields()).map_err(Fail::into_store)?;

        Ok(found.iter().find_map(script_of))
    }

    fn delete_script(&mut self, view: &View, script: &ScriptRef) -> Result<bool, StoreError> {
        self.write(|ctx| {
            let found = find(ctx, view, script, 1, doc! { "_id": number("$_id") })?;
            let Some(found) = found.first() else {
                return Ok(false);
            };

            Ok(ctx.delete_one(SCRIPTS, doc! { "_id": int(found, "_id") })? > 0)
        })
    }

    fn delete_scripts(&mut self, view: &View, ids: &[i64]) -> Result<usize, StoreError> {
        let mut filter = readable(view);
        filter.insert("_id", doc! { "$in": ids });

        self.write(|ctx| {
            let deleted = ctx.delete_many(SCRIPTS, filter.clone())?;
            Ok(usize::try_from(deleted).unwrap_or(usize::MAX))
        })
    }

    /// A script deleted during its run is not counted (no error, like the other stores).
    fn record_run(&mut self, id: i64, exit_code: Option<i32>, at: i64) -> Result<(), StoreError> {
        let exit_code = exit_code.map_or(Bson::Null, Bson::Int32);

        self.write(|ctx| {
            ctx.update_one(
                SCRIPTS,
                doc! { "_id": id },
                doc! {
                    "$inc": { "run_count": 1_i64 },
                    "$set": { "last_run_at": at, "last_exit_code": exit_code.clone() },
                },
            )?;

            Ok(())
        })
    }
}

/// Replaces the script with the draft's name in the scope `target`, or creates it.
fn save(ctx: &mut Ctx<'_>, target: &Target, draft: &ScriptDraft) -> Result<SavedScript, Fail> {
    let project = project_of(target);
    let scope = scope_value(project.as_deref());
    let now = now();

    let mut fields = doc! {
        "description": &draft.description,
        "language": draft.language.as_str(),
        "body": &draft.body,
        "sha256": script_sha256(draft.language, &draft.body),
        "timeout_secs": i64::try_from(draft.timeout_secs).unwrap_or(i64::MAX),
        "updated_at": now,
    };
    fields.extend(writer_fields(&draft.provenance));

    let replaced = ctx.find_one_and_update(
        SCRIPTS,
        doc! { "project": scope.clone(), "name": &draft.name },
        doc! { "$set": fields.clone() },
        false,
        true,
        script_fields(),
    )?;

    if let Some(document) = replaced {
        return saved(&document, false);
    }

    let mut document = doc! {
        "_id": next_script_id(ctx)?,
        "project": scope,
        "name": &draft.name,
        "created_at": now,
        "run_count": 0_i64,
        "last_run_at": Bson::Null,
        "last_exit_code": Bson::Null,
    };
    document.extend(fields);
    ctx.insert_one(SCRIPTS, document.clone())?;

    saved(&document, true)
}

fn saved(document: &Document, created: bool) -> Result<SavedScript, Fail> {
    let Some(script) = script_of(document) else {
        return Err(Fail::Store(StoreError::Backend(
            "the saved script could not be read back".to_owned(),
        )));
    };

    Ok(SavedScript { script, created })
}

/// A new script id. Taken outside the transaction, like note ids: an id of an attempt that is
/// retried is skipped, never reused.
fn next_script_id(ctx: &mut Ctx<'_>) -> Result<i64, Fail> {
    let counter = ctx
        .collection(META)
        .find_one_and_update(doc! { "_id": SCRIPT_IDS }, doc! { "$inc": { "value": 1_i64 } })
        .upsert(true)
        .return_document(ReturnDocument::After)
        .projection(doc! { "value": number("$value") })
        .run()?;

    Ok(counter.as_ref().map_or(1, |counter| int(counter, "value")))
}

/// Up to `limit` documents `script` names in `view`: by id, or by name in the repository first,
/// then in the global scope.
fn find(
    ctx: &mut Ctx<'_>,
    view: &View,
    script: &ScriptRef,
    limit: i64,
    projection: Document,
) -> Result<Vec<Document>, Fail> {
    let mut filter = readable(view);

    match script {
        ScriptRef::Id(id) => filter.insert("_id", *id),
        ScriptRef::Name(name) => filter.insert("name", name.trim()),
    };

    ctx.find(SCRIPTS, filter, repo_first(), limit, projection)
}

/// The scripts `view` can read, leaving out documents with a language this server cannot run.
fn readable(view: &View) -> Document {
    let languages: Vec<&str> = Language::ALL.iter().map(|language| language.as_str()).collect();

    let mut filter = visible(view);
    filter.insert("language", doc! { "$in": languages });
    filter
}

/// Repository scripts (a string `project`) before global ones (`null`), then by name.
fn repo_first() -> Document {
    doc! { "project": -1, "name": 1, "_id": 1 }
}

/// A regular expression matching `text` literally: every ASCII character other than a letter or
/// a digit is escaped (a backslash before one is always literal), a NUL byte spelled `\x00`.
pub(super) fn literal_pattern(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len() * 2);

    for character in text.chars() {
        if character == '\0' {
            pattern.push_str("\\x00");
            continue;
        }

        if character.is_ascii() && !character.is_ascii_alphanumeric() {
            pattern.push('\\');
        }

        pattern.push(character);
    }

    pattern
}

/// The writer fields of a script document; an unknown writer is stored as such.
fn writer_fields(provenance: &Provenance) -> Document {
    doc! {
        "source": clean::source(provenance.source.as_deref()),
        "writer_root": provenance.root.as_ref().map(|root| root.to_string_lossy().into_owned()),
        "via_override": provenance.via_override,
    }
}

/// The fields a [`Script`] needs, the text ones cut one character over their limit (see
/// [`read_back`]).
fn script_fields() -> Document {
    doc! {
        "_id": number("$_id"),
        "project": cut("$project", PATH_MAX),
        "name": cut("$name", SCRIPT_NAME_MAX + 1),
        "description": cut("$description", SCRIPT_DESCRIPTION_MAX + 1),
        "language": cut("$language", LANGUAGE_MAX),
        "body": cut("$body", SCRIPT_BODY_MAX + 1),
        "timeout_secs": number("$timeout_secs"),
        "source": cut("$source", SOURCE_MAX),
        "writer_root": cut("$writer_root", PATH_MAX),
        "via_override": flag("$via_override"),
        "created_at": number("$created_at"),
        "updated_at": number("$updated_at"),
        "run_count": number("$run_count"),
        "last_run_at": number_or_null("$last_run_at"),
        "last_exit_code": number_or_null("$last_exit_code"),
    }
}

/// The number at `path`; `null` when it is not a number.
fn number_or_null(path: &str) -> Bson {
    Bson::Document(doc! { "$cond": [{ "$isNumber": path }, path, Bson::Null] })
}

/// An integer field, `None` when it is missing or not a number.
fn optional_int(document: &Document, key: &str) -> Option<i64> {
    match document.get(key) {
        Some(Bson::Int64(_) | Bson::Int32(_) | Bson::Double(_)) => Some(int(document, key)),
        _ => None,
    }
}

/// A script document as a script; `None` for a language this server cannot run or a field
/// [`read_back`] refuses.
fn script_of(document: &Document) -> Option<Script> {
    let project = text(document, "project");

    read_back(Stored {
        id: int(document, "_id"),
        scope: scope_of(project.as_deref()),
        project,
        name: text(document, "name").unwrap_or_default(),
        description: text(document, "description").unwrap_or_default(),
        language: text(document, "language").unwrap_or_default(),
        body: text(document, "body").unwrap_or_default(),
        timeout_secs: int(document, "timeout_secs"),
        provenance: provenance_of(document),
        created_at: int(document, "created_at"),
        updated_at: int(document, "updated_at"),
        run_count: int(document, "run_count"),
        last_run_at: optional_int(document, "last_run_at"),
        last_exit_code: optional_int(document, "last_exit_code"),
    })
}
