//! Team hooks in a MongoDB team database: one document per hook in `hooks`, `_id` the hook id,
//! `repo` the repository key (`null` for global hooks, always stored).
//!
//! A put reads the stored document, compares its hash and counts the scope in one write (a
//! transaction on a replica set or a sharded cluster). An update or a delete only applies while
//! the document still holds the body it read, and an insert of an id taken meanwhile hits the
//! unique `_id`: both are retried from a fresh read.
//!
//! A snapshot transaction does not see a hook another transaction adds to the scope meanwhile
//! (two puts can each count 63), so a put that adds a hook to a scope first writes that scope's
//! guard document in `meta` (`hook_scope:<repo key>`, `hook_scope:global`): two such puts write
//! the same document, one of them fails with a write conflict and is retried from a fresh count.
//! On a standalone server (no transactions) two teammates sharing into one scope at once can still
//! pass its limit together.
//!
//! Reads cut every field one character over the limit this server writes and pass the hook
//! through [`read_back`], which computes the hash from the body actually read: the stored
//! `sha256` is never read.

use mongodb::bson::{Bson, Document, doc};

use super::{Ctx, Fail, HOOKS, META, MongoStore, cut, int, now, number, scope_value, text};
use crate::limits::{HOOK_AUTHOR_MAX, HOOK_BODY_MAX};
use crate::store::hooks::{HOOK_REPO_MAX, StoredHook, is_full, read_back};
use crate::store::{HookDelete, HookDraft, HookPage, HookPut, HookStore, StoreError, TeamHook};
use crate::store::{checked_hook, hook_sha256};

impl HookStore for MongoStore {
    fn list_hooks(&mut self, repo: Option<&str>, offset: usize, limit: usize) -> Result<HookPage, StoreError> {
        list(&mut self.reader()?, repo, offset, limit).map_err(Fail::into_store)
    }

    fn get_hook(&mut self, id: &str) -> Result<Option<TeamHook>, StoreError> {
        load(&mut self.reader()?, id).map_err(Fail::into_store)
    }

    fn put_hook(&mut self, draft: &HookDraft, expect: Option<&str>) -> Result<HookPut, StoreError> {
        let draft = checked_hook(draft)?;
        self.write(|ctx| put(ctx, &draft, expect))
    }

    fn delete_hook(&mut self, id: &str, expect: Option<&str>) -> Result<HookDelete, StoreError> {
        self.write(|ctx| {
            let Some((_, body)) = stored_body(ctx, id)? else {
                return Ok(HookDelete::Missing);
            };

            if expect.is_some_and(|expect| hook_sha256(&body) != expect) {
                return Ok(HookDelete::Changed);
            }

            let filter = match expect {
                Some(_) => doc! { "_id": id, "body": body },
                None => doc! { "_id": id },
            };

            if ctx.delete_one(HOOKS, filter)? == 0 {
                return Err(Fail::Retry);
            }

            Ok(HookDelete::Deleted)
        })
    }
}

fn list(ctx: &mut Ctx<'_>, repo: Option<&str>, offset: usize, limit: usize) -> Result<HookPage, Fail> {
    let filter = listed(repo);
    let total = ctx.count(HOOKS, filter.clone())?;

    let found = ctx
        .collection(HOOKS)
        .find(filter)
        .sort(doc! { "repo": 1, "created_at": 1, "_id": 1 })
        .skip(u64::try_from(offset).unwrap_or(u64::MAX))
        .limit(i64::try_from(limit).unwrap_or(i64::MAX))
        .projection(hook_fields())
        .run()?
        .collect::<Result<Vec<_>, _>>()?;

    let hooks = found.iter().filter_map(hook_of).collect();

    Ok(HookPage::of(
        usize::try_from(total).unwrap_or(usize::MAX),
        offset,
        found.len(),
        hooks,
    ))
}

fn put(ctx: &mut Ctx<'_>, draft: &HookDraft, expect: Option<&str>) -> Result<HookPut, Fail> {
    let stored = stored_body(ctx, &draft.id)?;

    match (expect, &stored) {
        (None, Some(_)) => return Ok(HookPut::Exists),
        (Some(_), None) => return Ok(HookPut::Missing),
        (Some(expect), Some((_, body))) if hook_sha256(body) != expect => return Ok(HookPut::Changed),
        _ => {}
    }

    let moves = stored.as_ref().is_none_or(|(repo, _)| *repo != draft.repo);
    let repo = scope_value(draft.repo.as_deref());

    if moves {
        let guard = format!("hook_scope:{}", draft.repo.as_deref().unwrap_or("global"));
        ctx.update_one_upsert(META, doc! { "_id": guard }, doc! { "$inc": { "puts": 1_i64 } })?;

        let count = ctx.count(HOOKS, doc! { "repo": repo.clone(), "_id": { "$ne": &draft.id } })?;

        if is_full(i64::try_from(count).unwrap_or(i64::MAX)) {
            return Ok(HookPut::Full);
        }
    }

    let now = now();
    let fields = doc! {
        "repo": repo,
        "body": &draft.body,
        "sha256": hook_sha256(&draft.body),
        "author": &draft.author,
        "updated_at": now,
    };

    match stored {
        Some((_, body)) => {
            let matched = ctx.update_one(HOOKS, doc! { "_id": &draft.id, "body": body }, doc! { "$set": fields })?;

            if matched == 0 {
                return Err(Fail::Retry);
            }
        }
        None => {
            let mut document = doc! { "_id": &draft.id, "created_at": now };
            document.extend(fields);
            ctx.insert_one(HOOKS, document)?;
        }
    }

    match load(ctx, &draft.id)? {
        Some(saved) => Ok(HookPut::Saved(saved)),
        None => Err(Fail::Store(StoreError::Backend(
            "the saved hook could not be read back".to_owned(),
        ))),
    }
}

/// The global hooks and those of the repository `repo`.
fn listed(repo: Option<&str>) -> Document {
    let mut scopes = vec![Bson::Null];
    scopes.extend(repo.map(|repo| Bson::String(repo.to_owned())));

    doc! { "repo": { "$in": scopes } }
}

/// The repository and body of document `id` (the body cut one character over its limit),
/// whatever they hold.
fn stored_body(ctx: &mut Ctx<'_>, id: &str) -> Result<Option<(Option<String>, String)>, Fail> {
    let projection = doc! {
        "repo": cut("$repo", HOOK_REPO_MAX + 1),
        "body": cut("$body", HOOK_BODY_MAX + 1),
    };
    let found = ctx.find_one(HOOKS, doc! { "_id": id }, projection)?;

    Ok(found.map(|document| (text(&document, "repo"), text(&document, "body").unwrap_or_default())))
}

/// Hook `id`; a document [`read_back`] leaves out counts as missing.
fn load(ctx: &mut Ctx<'_>, id: &str) -> Result<Option<TeamHook>, Fail> {
    let found = ctx.find_one(HOOKS, doc! { "_id": id }, hook_fields())?;

    Ok(found.as_ref().and_then(hook_of))
}

/// The fields a [`TeamHook`] needs, the text ones cut one character over their limit (see
/// [`read_back`]).
fn hook_fields() -> Document {
    doc! {
        "_id": cut("$_id", 33),
        "repo": cut("$repo", HOOK_REPO_MAX + 1),
        "author": cut("$author", HOOK_AUTHOR_MAX + 1),
        "body": cut("$body", HOOK_BODY_MAX + 1),
        "created_at": number("$created_at"),
        "updated_at": number("$updated_at"),
    }
}

/// A hook document as a team hook; `None` for a field [`read_back`] refuses.
fn hook_of(document: &Document) -> Option<TeamHook> {
    read_back(StoredHook {
        id: text(document, "_id").unwrap_or_default(),
        repo: text(document, "repo"),
        author: text(document, "author").unwrap_or_default(),
        created_at: int(document, "created_at"),
        updated_at: int(document, "updated_at"),
        body: text(document, "body").unwrap_or_default(),
    })
}
