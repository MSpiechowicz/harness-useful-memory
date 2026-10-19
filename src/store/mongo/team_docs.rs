//! Team documents in a MongoDB team database: one document per team document in the collection
//! of its kind ([`DocKind::table`]), `_id` the document id, `repo` the repository key (`null` for
//! global ones, always stored).
//!
//! A put reads the stored document, compares its hash and counts the scope in one write (a
//! transaction on a replica set or a sharded cluster). An update or a delete only applies while
//! the document still holds the body it read, and an insert of an id taken meanwhile hits the
//! unique `_id`: both are retried from a fresh read.
//!
//! A snapshot transaction does not see a document another transaction adds to the scope meanwhile
//! (two puts can each count 63), so a put that adds a document to a scope first writes that
//! scope's guard document in `meta` (`hook_scope:<repo key>`, `hook_scope:global`, and
//! `routine_scope:…` for routines): two such puts write the same document, one of them fails with
//! a write conflict and is retried from a fresh count. On a standalone server (no transactions)
//! two teammates sharing into one scope at once can still pass its limit together.
//!
//! Reads cut every field one character over the limit this server writes and pass the document
//! through [`read_back`], which computes the hash from the body actually read: the stored
//! `sha256` is never read.

use mongodb::bson::{Bson, Document, doc};

use super::{Ctx, Fail, META, MongoStore, cut, int, now, number, scope_value, text};
use crate::limits::{TEAM_DOC_AUTHOR_MAX, TEAM_DOC_BODY_MAX};
use crate::store::StoreError;
use crate::store::team_docs::{
    DOC_REPO_MAX, DocDelete, DocDraft, DocKind, DocPage, DocPut, LISTED_ID_MAX, StoredDoc, TeamDoc, TeamDocStore,
    check_author, checked_doc, doc_sha256, is_full, not_read_back, read_back,
};

impl TeamDocStore for MongoStore {
    fn list_docs(
        &mut self,
        kind: DocKind,
        repo: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<DocPage, StoreError> {
        list(&mut self.reader()?, kind, repo, offset, limit).map_err(|fail| self.failed(fail))
    }

    fn get_doc(&mut self, kind: DocKind, id: &str) -> Result<Option<TeamDoc>, StoreError> {
        load(&mut self.reader()?, kind, id).map_err(|fail| self.failed(fail))
    }

    fn put_doc(&mut self, kind: DocKind, draft: &DocDraft, expect: Option<&str>) -> Result<DocPut, StoreError> {
        let draft = checked_doc(kind, draft)?;
        self.write(|ctx| put(ctx, kind, &draft, expect))
    }

    fn delete_doc(&mut self, kind: DocKind, id: &str, expect: Option<&str>) -> Result<DocDelete, StoreError> {
        self.write(|ctx| {
            let Some((_, body)) = stored_body(ctx, kind, id)? else {
                return Ok(DocDelete::Missing);
            };

            if expect.is_some_and(|expect| doc_sha256(&body) != expect) {
                return Ok(DocDelete::Changed);
            }

            let filter = match expect {
                Some(_) => doc! { "_id": id, "body": body },
                None => doc! { "_id": id },
            };

            if ctx.delete_one(kind.table(), filter)? == 0 {
                return Err(Fail::Retry);
            }

            Ok(DocDelete::Deleted)
        })
    }

    fn delete_docs_by(&mut self, kind: DocKind, author: &str) -> Result<Vec<String>, StoreError> {
        let author = check_author(author)?;

        self.write(|ctx| {
            let filter = doc! { "author": &author };
            let projection = doc! { "_id": cut("$_id", LISTED_ID_MAX) };
            let found = ctx.find(kind.table(), filter.clone(), doc! { "_id": 1 }, 0, projection)?;

            // Another teammate's write between the read and the delete is retried from a fresh
            // read, so the ids are the documents deleted.
            if ctx.delete_many(kind.table(), filter)? != u64::try_from(found.len()).unwrap_or(u64::MAX) {
                return Err(Fail::Retry);
            }

            Ok(found
                .iter()
                .map(|document| text(document, "_id").unwrap_or_default())
                .collect())
        })
    }
}

fn list(ctx: &mut Ctx<'_>, kind: DocKind, repo: Option<&str>, offset: usize, limit: usize) -> Result<DocPage, Fail> {
    let filter = listed(repo);
    let total = ctx.count(kind.table(), filter.clone())?;

    let found = ctx.find_page(
        kind.table(),
        filter,
        doc! { "repo": 1, "created_at": 1, "_id": 1 },
        u64::try_from(offset).unwrap_or(u64::MAX),
        i64::try_from(limit).unwrap_or(i64::MAX),
        doc_fields(),
    )?;

    let docs = found.iter().filter_map(|document| doc_of(kind, document)).collect();

    Ok(DocPage::of(
        usize::try_from(total).unwrap_or(usize::MAX),
        offset,
        found.len(),
        docs,
    ))
}

fn put(ctx: &mut Ctx<'_>, kind: DocKind, draft: &DocDraft, expect: Option<&str>) -> Result<DocPut, Fail> {
    let table = kind.table();
    let stored = stored_body(ctx, kind, &draft.id)?;

    match (expect, &stored) {
        (None, Some(_)) => return Ok(DocPut::Exists),
        (Some(_), None) => return Ok(DocPut::Missing),
        (Some(expect), Some((_, body))) if doc_sha256(body) != expect => return Ok(DocPut::Changed),
        _ => {}
    }

    let moves = stored.as_ref().is_none_or(|(repo, _)| *repo != draft.repo);
    let repo = scope_value(draft.repo.as_deref());

    if moves {
        let guard = format!("{}_scope:{}", kind.noun(), draft.repo.as_deref().unwrap_or("global"));
        ctx.update_one_upsert(META, doc! { "_id": guard }, doc! { "$inc": { "puts": 1_i64 } })?;

        let count = ctx.count(table, doc! { "repo": repo.clone(), "_id": { "$ne": &draft.id } })?;

        if is_full(kind, i64::try_from(count).unwrap_or(i64::MAX)) {
            return Ok(DocPut::Full);
        }
    }

    let now = now();
    let fields = doc! {
        "repo": repo,
        "body": &draft.body,
        "sha256": doc_sha256(&draft.body),
        "author": &draft.author,
        "updated_at": now,
    };

    match stored {
        Some((_, body)) => {
            let matched = ctx.update_one(table, doc! { "_id": &draft.id, "body": body }, doc! { "$set": fields })?;

            if matched == 0 {
                return Err(Fail::Retry);
            }
        }
        None => {
            let mut document = doc! { "_id": &draft.id, "created_at": now };
            document.extend(fields);
            ctx.insert_one(table, document)?;
        }
    }

    match load(ctx, kind, &draft.id)? {
        Some(saved) => Ok(DocPut::Saved(saved)),
        None => Err(Fail::Store(not_read_back(kind))),
    }
}

/// The global documents and those of the repository `repo`.
fn listed(repo: Option<&str>) -> Document {
    let mut scopes = vec![Bson::Null];
    scopes.extend(repo.map(|repo| Bson::String(repo.to_owned())));

    doc! { "repo": { "$in": scopes } }
}

/// The repository and body of document `id` (the body cut one character over its limit),
/// whatever they hold.
fn stored_body(ctx: &mut Ctx<'_>, kind: DocKind, id: &str) -> Result<Option<(Option<String>, String)>, Fail> {
    let projection = doc! {
        "repo": cut("$repo", DOC_REPO_MAX + 1),
        "body": cut("$body", TEAM_DOC_BODY_MAX + 1),
    };
    let found = ctx.find_one(kind.table(), doc! { "_id": id }, projection)?;

    Ok(found.map(|document| (text(&document, "repo"), text(&document, "body").unwrap_or_default())))
}

/// Document `id`; one [`read_back`] leaves out counts as missing.
fn load(ctx: &mut Ctx<'_>, kind: DocKind, id: &str) -> Result<Option<TeamDoc>, Fail> {
    let found = ctx.find_one(kind.table(), doc! { "_id": id }, doc_fields())?;

    Ok(found.as_ref().and_then(|document| doc_of(kind, document)))
}

/// The fields a [`TeamDoc`] needs, the text ones cut one character over their limit (see
/// [`read_back`]).
fn doc_fields() -> Document {
    doc! {
        "_id": cut("$_id", 33),
        "repo": cut("$repo", DOC_REPO_MAX + 1),
        "author": cut("$author", TEAM_DOC_AUTHOR_MAX + 1),
        "body": cut("$body", TEAM_DOC_BODY_MAX + 1),
        "created_at": number("$created_at"),
        "updated_at": number("$updated_at"),
    }
}

/// A stored document as a team document of `kind`; `None` for a field [`read_back`] refuses.
fn doc_of(kind: DocKind, document: &Document) -> Option<TeamDoc> {
    read_back(
        kind,
        StoredDoc {
            id: text(document, "_id").unwrap_or_default(),
            repo: text(document, "repo"),
            author: text(document, "author").unwrap_or_default(),
            created_at: int(document, "created_at"),
            updated_at: int(document, "updated_at"),
            body: text(document, "body").unwrap_or_default(),
        },
    )
}
