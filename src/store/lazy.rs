//! A store opened on first use, so a server whose agent never touches scripts neither connects
//! to the team database for them nor creates the local file (nor, with every note scope in the
//! team database, opens the local notes store it never reads).

use super::Reach;
use super::{Edge, Forget, Forgotten, Hit, Link, MemoryStore, Neighborhood, Note, NoteDraft, NoteListQuery, NotePage};
use super::{NotePatch, NoteText, Saved, SearchQuery, VectorTarget};
use super::{PreviousVersion, TrustRecord, TrustStore, TrustedVersion, View};
use super::{
    SavedScript, Script, ScriptDraft, ScriptOrigin, ScriptRef, ScriptStore, ScriptSummary, StoreError, Target,
};

/// Opens the store behind it.
type Opener<S> = Box<dyn FnMut() -> Result<Box<S>, String> + Send>;

/// A store opened by the first call. A failed open is reported as [`StoreError::Backend`] with
/// the opener's message and tried again by the next call.
pub struct Lazy<S: ?Sized> {
    open: Opener<S>,
    store: Option<Box<S>>,
    reach: Option<Reach>,
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
            reach: None,
        }
    }

    /// The store recording its opens in `reach`: a failed open starts the cooldown of the reach,
    /// during which the open is not tried again and the failure is repeated at once.
    pub fn with_reach(mut self, reach: Reach) -> Self {
        self.reach = Some(reach);
        self
    }

    /// The open store, opening it first when needed.
    fn store(&mut self) -> Result<&mut S, StoreError> {
        let store = match self.store.take() {
            Some(store) => store,
            None => self.open_store()?,
        };

        Ok(&mut **self.store.insert(store))
    }

    /// Opens the store, unless the reach says the last open failed a moment ago.
    fn open_store(&mut self) -> Result<Box<S>, StoreError> {
        if let Some(message) = self.reach.as_ref().and_then(Reach::cooling) {
            return Err(StoreError::Backend(message));
        }

        match (self.open)() {
            Ok(store) => {
                if let Some(reach) = &self.reach {
                    reach.opened();
                }

                Ok(store)
            }
            Err(message) => {
                if let Some(reach) = &self.reach {
                    reach.failed(&message);
                }

                Err(StoreError::Backend(message))
            }
        }
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

    fn hidden_scripts(&mut self, view: &View) -> Result<Vec<ScriptSummary>, StoreError> {
        self.store()?.hidden_scripts(view)
    }

    fn origin(&self, id: i64) -> ScriptOrigin {
        self.store
            .as_ref()
            .map_or(ScriptOrigin::Single, |store| store.origin(id))
    }

    fn ready(&mut self) -> bool {
        self.store().is_ok()
    }
}

impl MemoryStore for Lazy<dyn MemoryStore> {
    fn remember(&mut self, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError> {
        self.store()?.remember(target, draft)
    }

    fn update(&mut self, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError> {
        self.store()?.update(view, id, patch)
    }

    fn get(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        self.store()?.get(view, ids)
    }

    fn peek(&mut self, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
        self.store()?.peek(view, ids)
    }

    fn list_notes(&mut self, query: &NoteListQuery) -> Result<NotePage, StoreError> {
        self.store()?.list_notes(query)
    }

    fn search(&mut self, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
        self.store()?.search(query)
    }

    fn vector_targets(&mut self, view: &View) -> Vec<VectorTarget> {
        self.store().map(|store| store.vector_targets(view)).unwrap_or_default()
    }

    fn hits_for(&mut self, query: &SearchQuery, ids: &[i64]) -> Result<Vec<Hit>, StoreError> {
        self.store()?.hits_for(query, ids)
    }

    fn similar(&mut self, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
        self.store()?.similar(view, id, limit)
    }

    fn similar_text(&mut self, view: &View, text: (&str, &str), limit: usize) -> Result<Vec<Hit>, StoreError> {
        self.store()?.similar_text(view, text, limit)
    }

    fn notes_since(&mut self, cursor: i64, limit: usize) -> Result<Vec<NoteText>, StoreError> {
        self.store()?.notes_since(cursor, limit)
    }

    fn note_ids(&mut self) -> Result<Vec<i64>, StoreError> {
        self.store()?.note_ids()
    }

    fn link(&mut self, target: &Target, link: &Link) -> Result<Edge, StoreError> {
        self.store()?.link(target, link)
    }

    fn neighbors(&mut self, view: &View, entity: &str, depth: u8, limit: usize) -> Result<Neighborhood, StoreError> {
        self.store()?.neighbors(view, entity, depth, limit)
    }

    fn forget(&mut self, view: &View, target: &Target, what: &Forget) -> Result<usize, StoreError> {
        self.store()?.forget(view, target, what)
    }

    fn forget_notes(&mut self, view: &View, exclude: &[i64]) -> Result<Forgotten, StoreError> {
        self.store()?.forget_notes(view, exclude)
    }

    fn find_by_title(&mut self, target: &Target, title: &str) -> Result<Option<Note>, StoreError> {
        self.store()?.find_by_title(target, title)
    }

    fn english_notes(&mut self) -> Result<bool, StoreError> {
        self.store()?.english_notes()
    }

    fn set_english_notes(&mut self, on: bool) -> Result<(), StoreError> {
        self.store()?.set_english_notes(on)
    }

    fn ready(&mut self) -> bool {
        self.store().is_ok()
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

    fn hold_back(&mut self, place: &str, script_id: Option<i64>, name: &str, sha256: &str) -> Result<(), StoreError> {
        self.store()?.hold_back(place, script_id, name, sha256)
    }

    fn is_held_back(&mut self, place: &str, script_id: i64, name: &str, sha256: &str) -> Result<bool, StoreError> {
        self.store()?.is_held_back(place, script_id, name, sha256)
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

        fn hold_back(&mut self, _place: &str, _id: Option<i64>, _name: &str, _sha256: &str) -> Result<(), StoreError> {
            Ok(())
        }

        fn is_held_back(&mut self, _place: &str, _id: i64, _name: &str, _sha256: &str) -> Result<bool, StoreError> {
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
