//! Who added and who last changed a note or a script, on every backend: a write sets the author of
//! the change, a create sets who added it too, and an update (or a save under the same title or
//! name) never changes who added it.

use harness_memory::store::{
    Author, Authors, MemoryStore, NoteKind, NotePatch, Provenance, ScriptDraft, ScriptRef, ScriptStore, Target,
};
use pretty_assertions::assert_eq;

use super::scripts::script;
use super::{both, draft, repo};

const REPO: &str = "github.com/acme/widget";

fn ada() -> Author {
    Author {
        name: "Ada Lovelace".to_owned(),
        via: Some("claude-code".to_owned()),
    }
}

fn grace() -> Author {
    Author {
        name: "Grace Hopper".to_owned(),
        via: None,
    }
}

fn by(author: Option<Author>) -> Provenance {
    Provenance {
        source: Some("tests".to_owned()),
        author,
        ..Provenance::default()
    }
}

fn authors(added_by: Option<Author>, updated_by: Option<Author>) -> Authors {
    Authors { added_by, updated_by }
}

/// A create sets both authors, an update or a save under the same title only the one who changed
/// it last (also to none), and the lists, the reads and the lookup by title show them.
pub fn notes(store: &mut dyn MemoryStore) {
    let mut first = draft("Build cache", "Caches are stored in target", NoteKind::Location);
    first.provenance = by(Some(ada()));
    let saved = store.remember(&repo(REPO), &first).expect("create");
    let view = both(REPO);

    let read = |store: &mut dyn MemoryStore| store.peek(&view, &[saved.id]).expect("peek").remove(0);
    assert_eq!(read(store).authors, authors(Some(ada()), Some(ada())));

    let patch = NotePatch {
        body: Some("Caches are stored in target/debug".to_owned()),
        provenance: by(Some(grace())),
        ..NotePatch::default()
    };
    store.update(&view, saved.id, &patch).expect("update");
    assert_eq!(read(store).authors, authors(Some(ada()), Some(grace())));

    // Saving the title again updates the note in place.
    let mut again = draft("Build cache", "Caches live in target", NoteKind::Location);
    again.provenance = by(Some(ada()));
    let upsert = store.remember(&repo(REPO), &again).expect("save again");
    assert_eq!((upsert.id, upsert.created), (saved.id, false));
    assert_eq!(read(store).authors, authors(Some(ada()), Some(ada())));

    let found = store
        .find_by_title(&repo(REPO), "build cache")
        .expect("find")
        .expect("note");
    assert_eq!(found.authors, authors(Some(ada()), Some(ada())));

    let page = store.list_notes(&super::listing::listing(both(REPO))).expect("list");
    assert_eq!(page.rows[0].authors, authors(Some(ada()), Some(ada())));

    // A writer with no name changes who changed it last to none, and keeps who added it.
    let anonymous = NotePatch {
        tags: Some(vec!["cache".to_owned()]),
        provenance: by(None),
        ..NotePatch::default()
    };
    store.update(&view, saved.id, &anonymous).expect("update");
    assert_eq!(read(store).authors, authors(Some(ada()), None));

    // A note nobody put a name on has no authors, in a repository and in the global scope.
    let plain = store
        .remember(&Target::Global, &draft("Editor tabs", "Four wide", NoteKind::Note))
        .expect("global");
    let global = store.peek(&view, &[plain.id]).expect("peek").remove(0);
    assert_eq!(global.authors, Authors::default());
}

/// The same for scripts: saving the name again keeps who added the script.
pub fn scripts(store: &mut dyn ScriptStore) {
    let mut first = script("open-prs", "Lists open pull requests", "gh pr list\n");
    first.provenance.author = Some(ada());
    let created = store.save_script(&repo(REPO), &first).expect("create");
    assert_eq!(created.script.summary.authors, authors(Some(ada()), Some(ada())));

    let mut second: ScriptDraft = script("open-prs", "Open PRs", "gh pr list --json number\n");
    second.provenance.author = Some(grace());
    let replaced = store.save_script(&repo(REPO), &second).expect("replace");
    assert!(!replaced.created);
    assert_eq!(replaced.script.summary.authors, authors(Some(ada()), Some(grace())));

    let listed = store.list_scripts(&both(REPO), None).expect("list");
    assert_eq!(listed[0].authors, authors(Some(ada()), Some(grace())));

    let got = store
        .get_script(&both(REPO), &ScriptRef::Name("open-prs".to_owned()))
        .expect("get")
        .expect("script");
    assert_eq!(got.summary.authors, authors(Some(ada()), Some(grace())));

    let mut anonymous = script("open-prs", "Open PRs", "gh pr list --json title\n");
    anonymous.provenance.author = None;
    let last = store.save_script(&repo(REPO), &anonymous).expect("replace");
    assert_eq!(last.script.summary.authors, authors(Some(ada()), None));

    let nameless = store
        .save_script(&repo(REPO), &script("lint", "Lints", "just lint\n"))
        .expect("create");
    assert_eq!(nameless.script.summary.authors, Authors::default());
}
