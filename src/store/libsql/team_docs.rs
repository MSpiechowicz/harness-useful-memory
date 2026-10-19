//! Team documents in a libSQL team database: the statements of the SQLite-dialect stores
//! ([`crate::store::sqlite::team_docs`]) over this store's transactions.

use super::LibsqlStore;
use crate::store::sqlite::team_docs;
use crate::store::{DocDelete, DocDraft, DocKind, DocPage, DocPut, StoreError, TeamDoc, TeamDocStore};

impl TeamDocStore for LibsqlStore {
    fn list_docs(
        &mut self,
        kind: DocKind,
        repo: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<DocPage, StoreError> {
        team_docs::list(self, kind, repo, offset, limit)
    }

    fn get_doc(&mut self, kind: DocKind, id: &str) -> Result<Option<TeamDoc>, StoreError> {
        team_docs::get(self, kind, id)
    }

    fn put_doc(&mut self, kind: DocKind, draft: &DocDraft, expect: Option<&str>) -> Result<DocPut, StoreError> {
        team_docs::put(self, kind, draft, expect)
    }

    fn delete_doc(&mut self, kind: DocKind, id: &str, expect: Option<&str>) -> Result<DocDelete, StoreError> {
        team_docs::delete(self, kind, id, expect)
    }

    fn delete_docs_by(&mut self, kind: DocKind, author: &str) -> Result<Vec<String>, StoreError> {
        team_docs::delete_by(self, kind, author)
    }
}
