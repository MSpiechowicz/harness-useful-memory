//! Team hooks in a libSQL team database: the statements of the SQLite-dialect stores
//! ([`crate::store::sqlite::hooks`]) over this store's transactions.

use super::LibsqlStore;
use crate::store::sqlite::hooks;
use crate::store::{HookDelete, HookDraft, HookPage, HookPut, HookStore, StoreError, TeamHook};

impl HookStore for LibsqlStore {
    fn list_hooks(&mut self, repo: Option<&str>, offset: usize, limit: usize) -> Result<HookPage, StoreError> {
        hooks::list(self, repo, offset, limit)
    }

    fn get_hook(&mut self, id: &str) -> Result<Option<TeamHook>, StoreError> {
        hooks::get(self, id)
    }

    fn put_hook(&mut self, draft: &HookDraft, expect: Option<&str>) -> Result<HookPut, StoreError> {
        hooks::put(self, draft, expect)
    }

    fn delete_hook(&mut self, id: &str, expect: Option<&str>) -> Result<HookDelete, StoreError> {
        hooks::delete(self, id, expect)
    }
}
