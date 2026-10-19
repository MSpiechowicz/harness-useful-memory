//! Team routine store cases every backend must pass, the routine kind of the team document store
//! (the hook cases are in `common::hooks`). Each case takes an empty store (no hooks, no
//! routines) and panics on a mismatch; [`CASES`] lists them all, for a backend that prefers one
//! loop over one test per case.

use std::time::Duration;

use harness_memory::limits::ROUTINES_PER_SCOPE;
use harness_memory::store::{
    HookDraft, HookPut, HookStore, RoutineDelete, RoutineDraft, RoutinePut, RoutineStore, StoreError, TeamDocStore,
    TeamRoutine, doc_sha256,
};
use pretty_assertions::assert_eq;

pub const WIDGET: &str = "git:github.com/acme/widget";
pub const GADGET: &str = "git:github.com/acme/gadget";

/// A case: an empty store in, panics out.
pub type Case = fn(&mut dyn TeamDocStore);

/// Every case, by name.
pub const CASES: [(&str, Case); 9] = [
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
    ("a_scope_holds_at_most_64_routines", a_scope_holds_at_most_64_routines),
    ("bad_drafts_store_nothing", bad_drafts_store_nothing),
    (
        "deleting_mine_keeps_other_authors_rows",
        deleting_mine_keeps_other_authors_rows,
    ),
    ("hooks_and_routines_are_kept_apart", hooks_and_routines_are_kept_apart),
];

/// Runs every case, each on a store `fresh` returns empty.
pub fn conformance(fresh: &mut dyn FnMut() -> Box<dyn TeamDocStore>) {
    for (name, case) in CASES {
        eprintln!("routine case {name}");
        case(fresh().as_mut());
    }
}

/// The routine id `n` (32 hex characters).
pub fn id(n: u32) -> String {
    format!("{n:032x}")
}

/// A valid team routine file named `name`: global without `repo`, else a project routine of
/// `repo`, an agent asked every weekday morning.
pub fn body(id: &str, repo: Option<&str>, name: &str) -> String {
    let (scope, project) = match repo {
        Some(repo) => ("project", serde_json::Value::from(repo)),
        None => ("global", serde_json::Value::Null),
    };

    let routine = serde_json::json!({
        "version": 1,
        "id": id,
        "name": name,
        "description": null,
        "scope": scope,
        "project": project,
        "when": { "type": "weekdays", "at": "09:15" },
        "action": {
            "type": "agent",
            "harness": "omp",
            "model": null,
            "prompt": format!("Summarize what {name} should know today."),
            "approvalMode": "write",
        },
        "enabled": true,
        "createdAt": "2026-10-18T10:00:00Z",
        "createdBy": "desktop",
        "updatedAt": "2026-10-18T10:00:00Z",
        "updatedBy": "desktop",
    });

    let mut text = serde_json::to_string_pretty(&routine).expect("JSON");
    text.push('\n');
    text
}

/// The draft of [`body`], shared by `ada`.
pub fn draft(id: &str, repo: Option<&str>, name: &str) -> RoutineDraft {
    RoutineDraft {
        id: id.to_owned(),
        repo: repo.map(str::to_owned),
        body: body(id, repo, name),
        author: "ada".to_owned(),
    }
}

/// A valid team hook file, global (the shape of `common::hooks::body`).
fn hook_body(id: &str) -> String {
    serde_json::json!({
        "version": 1,
        "id": id,
        "name": "Hook",
        "scope": "global",
        "project": null,
        "event": "sessionStart",
        "action": { "type": "addContext", "text": "Context." },
        "enabled": true,
        "createdAt": "2026-10-17T10:00:00Z",
        "createdBy": "desktop",
        "updatedAt": "2026-10-17T10:00:00Z",
        "updatedBy": "desktop",
    })
    .to_string()
}

fn saved(put: Result<RoutinePut, StoreError>) -> TeamRoutine {
    match put.expect("put") {
        RoutinePut::Saved(routine) => routine,
        other => panic!("not saved: {other:?}"),
    }
}

fn insert(store: &mut dyn TeamDocStore, id: &str, repo: Option<&str>, name: &str) -> TeamRoutine {
    saved(store.put_routine(&draft(id, repo, name), None))
}

fn insert_by(store: &mut dyn TeamDocStore, id: &str, repo: Option<&str>, author: &str) -> TeamRoutine {
    let routine = RoutineDraft {
        author: author.to_owned(),
        ..draft(id, repo, "Shared")
    };

    saved(store.put_routine(&routine, None))
}

fn listed(store: &mut dyn TeamDocStore, repo: Option<&str>) -> Vec<String> {
    let page = store.list_routines(repo, 0, 200).expect("list");
    assert_eq!(page.total, page.routines.len(), "one page holds them all");

    page.routines.into_iter().map(|routine| routine.id).collect()
}

/// An insert stores the body byte for byte with its hash, the trimmed author and one time for
/// both stamps; `get` reads the same.
pub fn puts_insert_and_read_back(store: &mut dyn TeamDocStore) {
    let routine = RoutineDraft {
        author: "  Ada Lovelace ".to_owned(),
        ..draft(&id(1), Some(WIDGET), "Repo triage")
    };
    let inserted = saved(store.put_routine(&routine, None));

    assert_eq!(inserted.id, id(1));
    assert_eq!(inserted.repo.as_deref(), Some(WIDGET));
    assert_eq!(inserted.body, routine.body);
    assert_eq!(inserted.sha256, doc_sha256(&routine.body));
    assert_eq!(inserted.author, "Ada Lovelace");
    assert_eq!(inserted.created_at, inserted.updated_at);
    assert!(inserted.created_at > 1_700_000_000, "{}", inserted.created_at);

    assert_eq!(store.get_routine(&id(1)).expect("get"), Some(inserted));
    assert_eq!(store.get_routine(&id(2)).expect("get"), None);
}

pub fn an_insert_of_a_taken_id_changes_nothing(store: &mut dyn TeamDocStore) {
    let first = insert(store, &id(1), None, "First");

    let again = store.put_routine(&draft(&id(1), None, "Second"), None).expect("put");
    assert_eq!(again, RoutinePut::Exists);
    assert_eq!(store.get_routine(&id(1)).expect("get"), Some(first));
}

/// An update applies only while the stored hash is the expected one; it keeps `created_at` and
/// may move the routine to another scope.
pub fn updates_need_the_current_sha(store: &mut dyn TeamDocStore) {
    let first = insert(store, &id(1), None, "First");

    let missing = store.put_routine(&draft(&id(2), None, "Other"), Some(&first.sha256));
    assert_eq!(missing.expect("put"), RoutinePut::Missing);

    let stale = store.put_routine(&draft(&id(1), None, "Second"), Some(&"0".repeat(64)));
    assert_eq!(stale.expect("put"), RoutinePut::Changed);
    assert_eq!(store.get_routine(&id(1)).expect("get"), Some(first.clone()));

    let moved = RoutineDraft {
        author: "grace".to_owned(),
        ..draft(&id(1), Some(WIDGET), "Second")
    };
    let updated = saved(store.put_routine(&moved, Some(&first.sha256)));

    assert_eq!(updated.body, moved.body);
    assert_eq!(updated.sha256, doc_sha256(&moved.body));
    assert_eq!(updated.repo.as_deref(), Some(WIDGET));
    assert_eq!(updated.author, "grace");
    assert_eq!(updated.created_at, first.created_at);
    assert!(updated.updated_at >= first.updated_at);

    let replayed = store.put_routine(&moved, Some(&first.sha256));
    assert_eq!(replayed.expect("put"), RoutinePut::Changed, "the old hash is spent");

    assert_eq!(listed(store, None), Vec::<String>::new());
    assert_eq!(listed(store, Some(WIDGET)), [id(1)]);
}

/// A list holds the global routines and the asked repository's, global first, then by creation
/// time, then by id; it pages with a total.
pub fn lists_global_first_then_by_creation(store: &mut dyn TeamDocStore) {
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

    let page = store.list_routines(Some(WIDGET), 1, 2).expect("page");
    assert_eq!((page.total, page.next_offset), (4, Some(3)));
    let ids: Vec<String> = page.routines.into_iter().map(|routine| routine.id).collect();
    assert_eq!(ids, [id(3), id(4)]);

    let past = store.list_routines(Some(WIDGET), 10, 2).expect("past the end");
    assert_eq!((past.total, past.routines.len(), past.next_offset), (4, 0, None));
}

pub fn deletes_honour_the_expected_sha(store: &mut dyn TeamDocStore) {
    let kept = insert(store, &id(1), None, "Kept");
    insert(store, &id(2), Some(WIDGET), "Gone");

    assert_eq!(
        store.delete_routine(&id(1), Some(&"0".repeat(64))).expect("delete"),
        RoutineDelete::Changed
    );
    assert_eq!(store.get_routine(&id(1)).expect("get"), Some(kept.clone()));

    assert_eq!(
        store.delete_routine(&id(1), Some(&kept.sha256)).expect("delete"),
        RoutineDelete::Deleted
    );
    assert_eq!(
        store.delete_routine(&id(1), None).expect("again"),
        RoutineDelete::Missing
    );
    assert_eq!(
        store.delete_routine(&id(2), None).expect("delete"),
        RoutineDelete::Deleted
    );
    assert_eq!(listed(store, Some(WIDGET)), Vec::<String>::new());
}

/// A full scope takes no new routine (nor one moved into it), other scopes still do, and a
/// routine in the full scope can still be updated.
pub fn a_scope_holds_at_most_64_routines(store: &mut dyn TeamDocStore) {
    let limit = u32::try_from(ROUTINES_PER_SCOPE).expect("small");

    for n in 1..=limit {
        insert(store, &id(n), None, &format!("Global {n}"));
    }

    let over = store.put_routine(&draft(&id(1000), None, "One too many"), None);
    assert_eq!(over.expect("put"), RoutinePut::Full);

    let widget = insert(store, &id(2000), Some(WIDGET), "Widget");
    let moved = store.put_routine(&draft(&id(2000), None, "Widget"), Some(&widget.sha256));
    assert_eq!(moved.expect("put"), RoutinePut::Full);

    let first = store.get_routine(&id(1)).expect("get").expect("stored");
    saved(store.put_routine(&draft(&id(1), None, "Renamed"), Some(&first.sha256)));

    let page = store.list_routines(None, 0, 200).expect("list");
    assert_eq!(page.total, ROUTINES_PER_SCOPE);
}

/// A draft `checked_routine` refuses is refused by the store too, and nothing is stored.
pub fn bad_drafts_store_nothing(store: &mut dyn TeamDocStore) {
    let seeded = body(&id(1), None, "Seeded").replace(
        "\"updatedBy\": \"desktop\"",
        "\"updatedBy\": \"desktop\",\n  \"seed\": \"memory-save\"",
    );
    assert!(seeded.contains("memory-save"));

    let cases = [
        RoutineDraft {
            id: id(2),
            ..draft(&id(1), None, "Other id")
        },
        RoutineDraft {
            id: "X".repeat(32),
            ..draft(&id(1), None, "Bad id")
        },
        draft(&id(1), Some("path:/home/ada/notes"), "Local repository"),
        draft(&id(1), Some("git:github.com/acme/wid get"), "Spaced repository"),
        RoutineDraft {
            repo: Some(WIDGET.to_owned()),
            ..draft(&id(1), None, "Global body, project row")
        },
        RoutineDraft {
            repo: None,
            ..draft(&id(1), Some(WIDGET), "Project body, global row")
        },
        RoutineDraft {
            body: seeded,
            ..draft(&id(1), None, "Seeded")
        },
        RoutineDraft {
            body: hook_body(&id(1)),
            ..draft(&id(1), None, "A hook")
        },
        RoutineDraft {
            author: " ".to_owned(),
            ..draft(&id(1), None, "No author")
        },
    ];

    for case in cases {
        let put = store.put_routine(&case, None);
        assert!(matches!(put, Err(StoreError::Invalid(_))), "{case:?}: {put:?}");
    }

    assert_eq!(listed(store, Some(WIDGET)), Vec::<String>::new());
}

/// `delete --mine` deletes every routine of the author in every scope, and only those: other
/// authors' routines, and hooks of the same author, stay.
pub fn deleting_mine_keeps_other_authors_rows(store: &mut dyn TeamDocStore) {
    insert_by(store, &id(3), None, "ada");
    insert_by(store, &id(1), Some(WIDGET), "ada");
    insert_by(store, &id(2), None, "grace");
    insert_by(store, &id(4), Some(GADGET), "Ada");
    insert_by(store, &id(5), Some(WIDGET), "ada lovelace");

    let hook = HookDraft {
        id: id(6),
        repo: None,
        body: hook_body(&id(6)),
        author: "ada".to_owned(),
    };
    assert!(matches!(store.put_hook(&hook, None), Ok(HookPut::Saved(_))));

    let deleted = store.delete_routines_by("  ada ").expect("delete mine");
    assert_eq!(
        deleted,
        [id(1), id(3)],
        "in id order, the author trimmed and compared exactly"
    );

    assert_eq!(listed(store, None), [id(2)]);
    assert_eq!(listed(store, Some(WIDGET)), [id(2), id(5)]);
    assert_eq!(listed(store, Some(GADGET)), [id(2), id(4)]);
    assert!(store.get_hook(&id(6)).expect("get").is_some(), "hooks stay");

    assert_eq!(store.delete_routines_by("ada").expect("again"), Vec::<String>::new());
    assert!(matches!(store.delete_routines_by(" "), Err(StoreError::Invalid(_))));
    assert!(matches!(
        store.delete_routines_by("ada\u{202E}"),
        Err(StoreError::Invalid(_))
    ));
}

/// A hook and a routine with the same id live side by side; neither kind lists, reads or deletes
/// the other.
pub fn hooks_and_routines_are_kept_apart(store: &mut dyn TeamDocStore) {
    let routine = insert(store, &id(1), None, "Routine");
    let hook = HookDraft {
        id: id(1),
        repo: None,
        body: hook_body(&id(1)),
        author: "ada".to_owned(),
    };
    assert!(matches!(store.put_hook(&hook, None), Ok(HookPut::Saved(_))));

    assert_eq!(store.get_routine(&id(1)).expect("get"), Some(routine.clone()));
    assert_eq!(
        store.get_hook(&id(1)).expect("get").map(|hook| hook.body),
        Some(hook_body(&id(1)))
    );
    assert_eq!(store.list_hooks(None, 0, 50).expect("hooks").total, 1);

    let as_hook = store.put_hook(&draft(&id(2), None, "Routine body"), None);
    assert!(matches!(as_hook, Err(StoreError::Invalid(_))), "{as_hook:?}");

    assert_eq!(
        store.delete_routine(&id(1), Some(&routine.sha256)).expect("delete"),
        RoutineDelete::Deleted
    );
    assert!(store.get_hook(&id(1)).expect("get").is_some(), "the hook stays");
    assert_eq!(listed(store, None), Vec::<String>::new());
}
