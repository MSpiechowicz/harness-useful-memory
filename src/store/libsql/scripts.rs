//! Scripts in a libSQL team database: the statements of the local file
//! ([`crate::store::sqlite::scripts`]) over this store's transactions. Script trust stays in the
//! local file, so this store has no [`crate::store::TrustStore`].

use super::LibsqlStore;
use crate::store::sqlite::scripts;
use crate::store::{SavedScript, Script, ScriptDraft, ScriptRef, ScriptStore, ScriptSummary, StoreError, Target, View};

impl ScriptStore for LibsqlStore {
    fn save_script(&mut self, target: &Target, draft: &ScriptDraft) -> Result<SavedScript, StoreError> {
        scripts::save(self, target, draft)
    }

    fn list_scripts(&mut self, view: &View, query: Option<&str>) -> Result<Vec<ScriptSummary>, StoreError> {
        scripts::list(self, view, query)
    }

    fn get_script(&mut self, view: &View, script: &ScriptRef) -> Result<Option<Script>, StoreError> {
        scripts::get(self, view, script)
    }

    fn delete_script(&mut self, view: &View, script: &ScriptRef) -> Result<bool, StoreError> {
        scripts::delete(self, view, script)
    }

    fn delete_scripts(&mut self, view: &View, ids: &[i64]) -> Result<usize, StoreError> {
        scripts::delete_many(self, view, ids)
    }

    fn record_run(&mut self, id: i64, exit_code: Option<i32>, at: i64) -> Result<(), StoreError> {
        scripts::record_run(self, id, exit_code, at)
    }
}
