//! A store opened on first use, so a server whose agent never touches scripts neither connects
//! to the team database for them nor creates the local file.

use super::{PreviousVersion, TrustRecord, TrustStore, TrustedVersion, View};
use super::{SavedScript, Script, ScriptDraft, ScriptRef, ScriptStore, ScriptSummary, StoreError, Target};

/// Opens the store behind it.
type Opener<S> = Box<dyn FnMut() -> Result<Box<S>, String> + Send>;

/// A store opened by the first call. A failed open is reported as [`StoreError::Backend`] with
/// the opener's message and tried again by the next call.
pub struct Lazy<S: ?Sized> {
    open: Opener<S>,
    store: Option<Box<S>>,
}

impl<S: ?Sized> std::fmt::Debug for Lazy<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lazy")
            .field("opened", &self.store.is_some())
            .finish_non_exhaustive()
    }
}

impl<S: ?Sized> Lazy<S> {
    /// A store `open` opens when it is first used; its error message must be safe to show.
    pub fn new(open: impl FnMut() -> Result<Box<S>, String> + Send + 'static) -> Self {
        Self {
            open: Box::new(open),
            store: None,
        }
    }

    /// The open store, opening it first when needed.
    fn store(&mut self) -> Result<&mut S, StoreError> {
        let store = match self.store.take() {
            Some(store) => store,
            None => (self.open)().map_err(StoreError::Backend)?,
        };

        Ok(&mut **self.store.insert(store))
    }
}

impl ScriptStore for Lazy<dyn ScriptStore> {
    fn save_script(&mut self, target: &Target, draft: &ScriptDraft) -> Result<SavedScript, StoreError> {
        self.store()?.save_script(target, draft)
    }

    fn list_scripts(&mut self, view: &View, query: Option<&str>) -> Result<Vec<ScriptSummary>, StoreError> {
        self.store()?.list_scripts(view, query)
    }

    fn get_script(&mut self, view: &View, script: &ScriptRef) -> Result<Option<Script>, StoreError> {
        self.store()?.get_script(view, script)
    }

    fn delete_script(&mut self, view: &View, script: &ScriptRef) -> Result<bool, StoreError> {
        self.store()?.delete_script(view, script)
    }

    fn delete_scripts(&mut self, view: &View, ids: &[i64]) -> Result<usize, StoreError> {
        self.store()?.delete_scripts(view, ids)
    }

    fn record_run(&mut self, id: i64, exit_code: Option<i32>, at: i64) -> Result<(), StoreError> {
        self.store()?.record_run(id, exit_code, at)
    }
}

impl TrustStore for Lazy<dyn TrustStore> {
    fn trust_of(&mut self, place: &str, script_id: i64, sha256: &str) -> Result<Option<TrustRecord>, StoreError> {
        self.store()?.trust_of(place, script_id, sha256)
    }

    fn trust(&mut self, version: &TrustedVersion) -> Result<(), StoreError> {
        self.store()?.trust(version)
    }

    fn previous(&mut self, place: &str, script_id: i64) -> Result<Option<PreviousVersion>, StoreError> {
        self.store()?.previous(place, script_id)
    }

    fn note_unapproved(&mut self, sha256: &str) -> Result<(), StoreError> {
        self.store()?.note_unapproved(sha256)
    }

    fn was_unapproved(&mut self, sha256s: &[String]) -> Result<bool, StoreError> {
        self.store()?.was_unapproved(sha256s)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Trusts everything once opened.
    struct Open;

    impl TrustStore for Open {
        fn trust_of(&mut self, _place: &str, _id: i64, _sha256: &str) -> Result<Option<TrustRecord>, StoreError> {
            Ok(Some(TrustRecord {
                origin: crate::store::TrustOrigin::Approved,
                name: "x".to_owned(),
                scope: "global".to_owned(),
                project: None,
            }))
        }

        fn trust(&mut self, _version: &TrustedVersion) -> Result<(), StoreError> {
            Ok(())
        }

        fn previous(&mut self, _place: &str, _script_id: i64) -> Result<Option<PreviousVersion>, StoreError> {
            Ok(None)
        }

        fn note_unapproved(&mut self, _sha256: &str) -> Result<(), StoreError> {
            Ok(())
        }

        fn was_unapproved(&mut self, _sha256s: &[String]) -> Result<bool, StoreError> {
            Ok(false)
        }
    }

    #[test]
    fn the_store_opens_on_first_use_once_and_failures_are_retried() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&attempts);
        let mut lazy: Lazy<dyn TrustStore> = Lazy::new(move || {
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("cannot open the team database".to_owned())
            } else {
                Ok(Box::new(Open) as Box<dyn TrustStore>)
            }
        });
        assert_eq!(attempts.load(Ordering::SeqCst), 0, "nothing opens up front");

        let failed = lazy.is_trusted("team", 1, "x").expect_err("first open fails");
        assert_eq!(
            failed.to_string(),
            "memory database error: cannot open the team database"
        );

        assert!(lazy.is_trusted("team", 1, "x").expect("opened"));
        assert_eq!(lazy.previous("team", 1).expect("previous"), None);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }
}
