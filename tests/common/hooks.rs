//! Team hook store cases every backend must pass. Each case takes an empty store (no hooks) and
//! panics on a mismatch; [`CASES`] lists them all, for a backend that prefers one loop over one
//! test per case.

use std::time::Duration;

use harness_memory::limits::HOOKS_PER_SCOPE;
use harness_memory::store::{HookDelete, HookDraft, HookPut, HookStore, StoreError, TeamHook, hook_sha256};
use pretty_assertions::assert_eq;

pub const WIDGET: &str = "git:github.com/acme/widget";
pub const GADGET: &str = "git:github.com/acme/gadget";

/// A case: an empty store in, panics out.
pub type Case = fn(&mut dyn HookStore);

/// Every case, by name.
pub const CASES: [(&str, Case); 7] = [
    ("puts_insert_and_read_back", puts_insert_and_read_back),
    (
        "an_insert_of_a_taken_id_changes_nothing",
        an_insert_of_a_taken_id_changes_nothing,
    ),
    ("updates_need_the_current_sha", updates_need_the_current_sha),
    (
        "lists_global_first_then_by_creation",
        lists_global_first_then_by_creation,
    ),
    ("deletes_honour_the_expected_sha", deletes_honour_the_expected_sha),
    ("a_scope_holds_at_most_64_hooks", a_scope_holds_at_most_64_hooks),
    ("bad_drafts_store_nothing", bad_drafts_store_nothing),
];

/// Runs every case, each on a store `fresh` returns empty.
pub fn conformance(fresh: &mut dyn FnMut() -> Box<dyn HookStore>) {
    for (name, case) in CASES {
        eprintln!("hook case {name}");
        case(fresh().as_mut());
    }
}

/// The hook id `n` (32 hex characters).
pub fn id(n: u32) -> String {
    format!("{n:032x}")
}

/// A valid team hook file named `name`: global without `repo`, else a project hook of `repo`.
pub fn body(id: &str, repo: Option<&str>, name: &str) -> String {
    let (scope, project) = match repo {
        Some(repo) => ("project", serde_json::Value::from(repo)),
        None => ("global", serde_json::Value::Null),
    };

    let hook = serde_json::json!({
        "version": 1,
        "id": id,
        "name": name,
        "description": null,
        "scope": scope,
        "project": project,
        "event": "sessionStart",
        "matcher": null,
        "action": { "type": "addContext", "text": format!("Context from {name}.") },
        "enabled": true,
        "createdAt": "2026-10-17T10:00:00Z",
        "createdBy": "desktop",
        "updatedAt": "2026-10-17T10:00:00Z",
        "updatedBy": "desktop",
    });

    let mut text = serde_json::to_string_pretty(&hook).expect("JSON");
    text.push('\n');
    text
}

/// The draft of [`body`], shared by `ada`.
pub fn draft(id: &str, repo: Option<&str>, name: &str) -> HookDraft {
    HookDraft {
        id: id.to_owned(),
        repo: repo.map(str::to_owned),
        body: body(id, repo, name),
        author: "ada".to_owned(),
    }
}

fn saved(put: Result<HookPut, StoreError>) -> TeamHook {
    match put.expect("put") {
        HookPut::Saved(hook) => hook,
        other => panic!("not saved: {other:?}"),
    }
}

fn insert(store: &mut dyn HookStore, id: &str, repo: Option<&str>, name: &str) -> TeamHook {
    saved(store.put_hook(&draft(id, repo, name), None))
}

fn listed(store: &mut dyn HookStore, repo: Option<&str>) -> Vec<String> {
    let page = store.list_hooks(repo, 0, 200).expect("list");
    assert_eq!(page.total, page.hooks.len(), "one page holds them all");

    page.hooks.into_iter().map(|hook| hook.id).collect()
}

/// An insert stores the body byte for byte with its hash, the trimmed author and one time for
/// both stamps; `get` reads the same.
pub fn puts_insert_and_read_back(store: &mut dyn HookStore) {
    let hook = HookDraft {
        author: "  Ada Lovelace ".to_owned(),
        ..draft(&id(1), Some(WIDGET), "Repo rules")
    };
    let inserted = saved(store.put_hook(&hook, None));

    assert_eq!(inserted.id, id(1));
    assert_eq!(inserted.repo.as_deref(), Some(WIDGET));
    assert_eq!(inserted.body, hook.body);
    assert_eq!(inserted.sha256, hook_sha256(&hook.body));
    assert_eq!(inserted.author, "Ada Lovelace");
    assert_eq!(inserted.created_at, inserted.updated_at);
    assert!(inserted.created_at > 1_700_000_000, "{}", inserted.created_at);

    assert_eq!(store.get_hook(&id(1)).expect("get"), Some(inserted));
    assert_eq!(store.get_hook(&id(2)).expect("get"), None);
}

pub fn an_insert_of_a_taken_id_changes_nothing(store: &mut dyn HookStore) {
    let first = insert(store, &id(1), None, "First");

    let again = store.put_hook(&draft(&id(1), None, "Second"), None).expect("put");
    assert_eq!(again, HookPut::Exists);
    assert_eq!(store.get_hook(&id(1)).expect("get"), Some(first));
}

/// An update applies only while the stored hash is the expected one; it keeps `created_at` and
/// may move the hook to another scope.
pub fn updates_need_the_current_sha(store: &mut dyn HookStore) {
    let first = insert(store, &id(1), None, "First");

    let missing = store.put_hook(&draft(&id(2), None, "Other"), Some(&first.sha256));
    assert_eq!(missing.expect("put"), HookPut::Missing);

    let stale = store.put_hook(&draft(&id(1), None, "Second"), Some(&"0".repeat(64)));
    assert_eq!(stale.expect("put"), HookPut::Changed);
    assert_eq!(store.get_hook(&id(1)).expect("get"), Some(first.clone()));

    let moved = HookDraft {
        author: "grace".to_owned(),
        ..draft(&id(1), Some(WIDGET), "Second")
    };
    let updated = saved(store.put_hook(&moved, Some(&first.sha256)));

    assert_eq!(updated.body, moved.body);
    assert_eq!(updated.sha256, hook_sha256(&moved.body));
    assert_eq!(updated.repo.as_deref(), Some(WIDGET));
    assert_eq!(updated.author, "grace");
    assert_eq!(updated.created_at, first.created_at);
    assert!(updated.updated_at >= first.updated_at);

    let replayed = store.put_hook(&moved, Some(&first.sha256));
    assert_eq!(replayed.expect("put"), HookPut::Changed, "the old hash is spent");

    assert_eq!(listed(store, None), Vec::<String>::new());
    assert_eq!(listed(store, Some(WIDGET)), [id(1)]);
}

/// A list holds the global hooks and the asked repository's, global first, then by creation time,
/// then by id; it pages with a total.
pub fn lists_global_first_then_by_creation(store: &mut dyn HookStore) {
    insert(store, &id(9), None, "Older global");
    // Creation times have a resolution of one second.
    std::thread::sleep(Duration::from_millis(1100));
    insert(store, &id(5), Some(WIDGET), "Widget");
    insert(store, &id(3), None, "Newer global");
    insert(store, &id(4), None, "Same second global");
    insert(store, &id(2), Some(GADGET), "Gadget");

    assert_eq!(listed(store, None), [id(9), id(3), id(4)]);
    assert_eq!(listed(store, Some(WIDGET)), [id(9), id(3), id(4), id(5)]);
    assert_eq!(listed(store, Some(GADGET)), [id(9), id(3), id(4), id(2)]);
    assert_eq!(
        listed(store, Some("git:github.com/acme/unknown")),
        [id(9), id(3), id(4)]
    );

    let page = store.list_hooks(Some(WIDGET), 1, 2).expect("page");
    assert_eq!((page.total, page.next_offset), (4, Some(3)));
    let ids: Vec<String> = page.hooks.into_iter().map(|hook| hook.id).collect();
    assert_eq!(ids, [id(3), id(4)]);

    let last = store.list_hooks(Some(WIDGET), 3, 2).expect("last page");
    assert_eq!((last.hooks.len(), last.next_offset), (1, None));

    let past = store.list_hooks(Some(WIDGET), 10, 2).expect("past the end");
    assert_eq!((past.total, past.hooks.len(), past.next_offset), (4, 0, None));
}

pub fn deletes_honour_the_expected_sha(store: &mut dyn HookStore) {
    let kept = insert(store, &id(1), None, "Kept");
    insert(store, &id(2), Some(WIDGET), "Gone");

    assert_eq!(
        store.delete_hook(&id(1), Some(&"0".repeat(64))).expect("delete"),
        HookDelete::Changed
    );
    assert_eq!(store.get_hook(&id(1)).expect("get"), Some(kept.clone()));

    assert_eq!(
        store.delete_hook(&id(1), Some(&kept.sha256)).expect("delete"),
        HookDelete::Deleted
    );
    assert_eq!(store.delete_hook(&id(1), None).expect("again"), HookDelete::Missing);
    assert_eq!(
        store.delete_hook(&id(1), Some(&kept.sha256)).expect("again"),
        HookDelete::Missing
    );
    assert_eq!(store.delete_hook(&id(2), None).expect("delete"), HookDelete::Deleted);
    assert_eq!(listed(store, Some(WIDGET)), Vec::<String>::new());
}

/// A full scope takes no new hook (nor one moved into it), other scopes still do, and a hook in
/// the full scope can still be updated.
pub fn a_scope_holds_at_most_64_hooks(store: &mut dyn HookStore) {
    let limit = u32::try_from(HOOKS_PER_SCOPE).expect("small");

    for n in 1..=limit {
        insert(store, &id(n), None, &format!("Global {n}"));
    }

    let over = store.put_hook(&draft(&id(1000), None, "One too many"), None);
    assert_eq!(over.expect("put"), HookPut::Full);

    let widget = insert(store, &id(2000), Some(WIDGET), "Widget");
    let moved = store.put_hook(&draft(&id(2000), None, "Widget"), Some(&widget.sha256));
    assert_eq!(moved.expect("put"), HookPut::Full);

    let first = store.get_hook(&id(1)).expect("get").expect("stored");
    saved(store.put_hook(&draft(&id(1), None, "Renamed"), Some(&first.sha256)));

    let page = store.list_hooks(None, 0, 200).expect("list");
    assert_eq!(page.total, HOOKS_PER_SCOPE);
}

/// A draft [`harness_memory::store::checked_hook`] refuses is refused by the store too, and
/// nothing is stored.
pub fn bad_drafts_store_nothing(store: &mut dyn HookStore) {
    let seeded = body(&id(1), None, "Seeded").replace(
        "\"updatedBy\": \"desktop\"",
        "\"updatedBy\": \"desktop\",\n  \"seed\": \"memory-save\"",
    );
    assert!(seeded.contains("memory-save"));

    let cases = [
        HookDraft {
            id: id(2),
            ..draft(&id(1), None, "Other id")
        },
        HookDraft {
            id: "X".repeat(32),
            ..draft(&id(1), None, "Bad id")
        },
        draft(&id(1), Some("path:/home/ada/notes"), "Local repository"),
        HookDraft {
            repo: Some(WIDGET.to_owned()),
            ..draft(&id(1), None, "Global body, project row")
        },
        HookDraft {
            repo: None,
            ..draft(&id(1), Some(WIDGET), "Project body, global row")
        },
        HookDraft {
            body: seeded,
            ..draft(&id(1), None, "Seeded")
        },
        HookDraft {
            body: "{\"version\": 1}".to_owned(),
            ..draft(&id(1), None, "Not a hook")
        },
        HookDraft {
            author: " ".to_owned(),
            ..draft(&id(1), None, "No author")
        },
    ];

    for case in cases {
        let put = store.put_hook(&case, None);
        assert!(matches!(put, Err(StoreError::Invalid(_))), "{case:?}: {put:?}");
    }

    assert_eq!(listed(store, Some(WIDGET)), Vec::<String>::new());
}
