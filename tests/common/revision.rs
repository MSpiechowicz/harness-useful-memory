//! The stale edit check every backend passes: a patch that expects a revision changes the note only
//! if it is still at that revision.

use harness_memory::store::{MemoryStore, NoteKind, NotePatch, StoreError};
use pretty_assertions::assert_eq;

use super::{both, draft, repo};

const REPO: &str = "github.com/acme/widget";

pub fn a_patch_expecting_a_revision_changes_only_a_note_still_at_it(store: &mut dyn MemoryStore) {
    let saved = store
        .remember(&repo(REPO), &draft("Editor note", "first text", NoteKind::Note))
        .expect("saved");
    assert_eq!(saved.revision, 1);
    let view = both(REPO);

    // The note is at the revision the editor read: the change is made.
    let matching = NotePatch {
        body: Some("second text".to_owned()),
        expected_revision: Some(1),
        ..NotePatch::default()
    };
    let changed = store.update(&view, saved.id, &matching).expect("matching revision");
    assert_eq!(changed.revision, 2);
    assert!(!changed.queued);

    // An editor that still holds revision 1 does not overwrite what was saved since.
    let stale = NotePatch {
        body: Some("overwritten text".to_owned()),
        append: None,
        expected_revision: Some(1),
        ..NotePatch::default()
    };
    let error = store.update(&view, saved.id, &stale).expect_err("stale revision");
    assert!(matches!(error, StoreError::Stale(2)), "{error:?}");

    let note = store.peek(&view, &[saved.id]).expect("peek").remove(0);
    assert_eq!(note.body, "second text", "nothing was changed");
    assert_eq!(note.revision, 2);

    // A revision from the future is as stale, and a patch that expects none is made whatever the revision.
    let ahead = NotePatch {
        expected_revision: Some(9),
        ..stale.clone()
    };
    assert!(matches!(
        store.update(&view, saved.id, &ahead),
        Err(StoreError::Stale(2))
    ));

    let without = NotePatch {
        expected_revision: None,
        ..stale
    };
    assert_eq!(
        store
            .update(&view, saved.id, &without)
            .expect("no expectation")
            .revision,
        3
    );
}
