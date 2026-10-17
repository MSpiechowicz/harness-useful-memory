//! Script store cases every backend must pass. Each case takes an empty store (no scripts, no
//! repositories) and panics on a mismatch:
//!
//! ```ignore
//! #[test]
//! fn saving_upserts_by_name() {
//!     let mut store = fresh_store();
//!     common::scripts::saving_upserts_by_name(&mut store);
//! }
//! ```
//!
//! [`CASES`] lists them all, for a backend that prefers one loop over one test per case.

use std::path::PathBuf;

use harness_memory::store::{Language, Provenance, Scope, ScriptDraft, ScriptRef, ScriptStore, StoreError, Target};
use harness_memory::store::{View, checked_draft, script_sha256};
use pretty_assertions::assert_eq;

use super::{both, global_only, repo, repo_only};

pub const REPO: &str = "github.com/acme/widget";
pub const OTHER: &str = "github.com/acme/gadget";

/// A case: an empty store in, panics out.
pub type Case = fn(&mut dyn ScriptStore);

/// Every case, by name.
pub const CASES: [(&str, Case); 9] = [
    ("saving_upserts_by_name", saving_upserts_by_name),
    ("bad_drafts_store_nothing", bad_drafts_store_nothing),
    ("lists_repo_first_then_by_name", lists_repo_first_then_by_name),
    (
        "queries_match_name_or_description_ignoring_case",
        queries_match_name_or_description_ignoring_case,
    ),
    ("views_decide_what_is_visible", views_decide_what_is_visible),
    (
        "names_resolve_repo_first_and_delete",
        names_resolve_repo_first_and_delete,
    ),
    ("runs_are_counted", runs_are_counted),
    (
        "a_run_of_a_missing_script_is_no_error",
        a_run_of_a_missing_script_is_no_error,
    ),
    (
        "deleting_many_takes_only_visible_ids",
        deleting_many_takes_only_visible_ids,
    ),
];

/// Runs every case, each on a store `fresh` returns empty.
pub fn conformance(fresh: &mut dyn FnMut() -> Box<dyn ScriptStore>) {
    for (name, case) in CASES {
        eprintln!("script case {name}");
        case(fresh().as_mut());
    }
}

/// A bash draft written by `tests` from `/work/<REPO>`.
pub fn script(name: &str, description: &str, body: &str) -> ScriptDraft {
    ScriptDraft {
        name: name.to_owned(),
        description: description.to_owned(),
        language: Language::Bash,
        body: body.to_owned(),
        timeout_secs: 60,
        provenance: Provenance {
            root: Some(PathBuf::from("/work").join(REPO)),
            source: Some("tests".to_owned()),
            via_override: false,
        },
    }
}

fn save(store: &mut dyn ScriptStore, target: &Target, name: &str, description: &str) -> i64 {
    let draft = script(name, description, &format!("echo {name}\n"));
    store.save_script(target, &draft).expect("save").script.summary.id
}

fn names(store: &mut dyn ScriptStore, view: &View, query: Option<&str>) -> Vec<String> {
    store
        .list_scripts(view, query)
        .expect("list")
        .into_iter()
        .map(|summary| summary.name)
        .collect()
}

/// Saving a name again in the same scope replaces the script and keeps its run statistics; the
/// same name in another scope is another script.
pub fn saving_upserts_by_name(store: &mut dyn ScriptStore) {
    let first = store
        .save_script(
            &repo(REPO),
            &script(" open-prs ", " Lists open pull requests ", "gh pr list\n"),
        )
        .expect("create");
    let summary = &first.script.summary;

    assert!(first.created);
    assert_eq!(first.script.body, "gh pr list\n");
    assert_eq!(summary.name, "open-prs");
    assert_eq!(summary.description, "Lists open pull requests");
    assert_eq!(summary.scope, Scope::Repo);
    assert_eq!(summary.project.as_deref(), Some("git:github.com/acme/widget"));
    assert_eq!(summary.language, Language::Bash);
    assert_eq!(summary.sha256, script_sha256(Language::Bash, "gh pr list\n"));
    assert_eq!(summary.timeout_secs, 60);
    assert_eq!(summary.provenance, script("x", "", "x").provenance);
    assert_eq!(summary.updated_at, summary.created_at);
    assert_eq!(
        (summary.run_count, summary.last_run_at, summary.last_exit_code),
        (0, None, None)
    );

    store.record_run(summary.id, Some(3), 1_700_000_000).expect("run");

    let replaced = ScriptDraft {
        language: Language::Python,
        timeout_secs: 120,
        provenance: Provenance {
            root: None,
            source: Some("codex".to_owned()),
            via_override: true,
        },
        ..script("open-prs", "Open PRs as JSON", "print('prs')\n")
    };
    let second = store.save_script(&repo(REPO), &replaced).expect("replace");
    let updated = &second.script.summary;

    assert!(!second.created);
    assert_eq!(updated.id, summary.id);
    assert_eq!(second.script.body, "print('prs')\n");
    assert_eq!(updated.sha256, script_sha256(Language::Python, "print('prs')\n"));
    assert_ne!(updated.sha256, summary.sha256);
    assert_eq!(updated.description, "Open PRs as JSON");
    assert_eq!(updated.language, Language::Python);
    assert_eq!(updated.timeout_secs, 120);
    assert_eq!(updated.provenance, replaced.provenance);
    assert_eq!(updated.created_at, summary.created_at);
    assert!(updated.updated_at >= summary.updated_at);
    assert_eq!(
        (updated.run_count, updated.last_run_at, updated.last_exit_code),
        (1, Some(1_700_000_000), Some(3))
    );

    let global = store
        .save_script(&Target::Global, &script("open-prs", "", "gh pr list\n"))
        .expect("global");
    assert!(global.created);
    assert_ne!(global.script.summary.id, summary.id);
    assert_eq!(global.script.summary.scope, Scope::Global);
    assert_eq!(global.script.summary.project, None);

    assert_eq!(store.list_scripts(&both(REPO), None).expect("list").len(), 2);
}

/// A draft [`checked_draft`] refuses is refused by the store too, and nothing is stored.
pub fn bad_drafts_store_nothing(store: &mut dyn ScriptStore) {
    let token = "ghp_".to_owned() + &"a".repeat(36);
    let cases = [
        script("Bad Name", "", "true"),
        script("empty", "", "  \n"),
        script(
            "secret",
            "",
            &format!("curl -H 'Authorization: token {token}' \"$URL\""),
        ),
        ScriptDraft {
            timeout_secs: 0,
            ..script("slow", "", "true")
        },
    ];

    for draft in cases {
        assert!(checked_draft(&draft).is_err(), "{draft:?}");

        let saved = store.save_script(&repo(REPO), &draft);
        assert!(matches!(saved, Err(StoreError::Invalid(_))), "{saved:?}");
    }

    assert_eq!(names(store, &both(REPO), None), Vec::<String>::new());
}

pub fn lists_repo_first_then_by_name(store: &mut dyn ScriptStore) {
    save(store, &Target::Global, "beta", "");
    save(store, &repo(REPO), "zeta", "");
    save(store, &Target::Global, "aardvark", "");
    save(store, &repo(REPO), "alpha", "");

    assert_eq!(names(store, &both(REPO), None), ["alpha", "zeta", "aardvark", "beta"]);

    let listed = store.list_scripts(&both(REPO), None).expect("list");
    assert_eq!(listed[0].scope, Scope::Repo);
    assert_eq!(listed[3].scope, Scope::Global);
}

pub fn queries_match_name_or_description_ignoring_case(store: &mut dyn ScriptStore) {
    save(store, &repo(REPO), "open-prs", "Lists open Pull Requests");
    save(store, &repo(REPO), "release", "Tags and publishes");
    save(store, &Target::Global, "weather", "Fetches the forecast");

    assert_eq!(names(store, &both(REPO), Some("PULL")), ["open-prs"]);
    assert_eq!(names(store, &both(REPO), Some("Open-")), ["open-prs"]);
    assert_eq!(names(store, &both(REPO), Some("e")), ["open-prs", "release", "weather"]);
    assert_eq!(names(store, &both(REPO), Some("forecast")), ["weather"]);
    assert_eq!(names(store, &both(REPO), Some("nothing like it")), Vec::<String>::new());
    assert_eq!(
        names(store, &both(REPO), Some("  ")).len(),
        3,
        "a blank query keeps all"
    );
}

/// A repository view sees its own scripts (and the global ones when it includes them), never
/// another repository's, also by id.
pub fn views_decide_what_is_visible(store: &mut dyn ScriptStore) {
    let mine = save(store, &repo(REPO), "mine", "");
    save(store, &repo(OTHER), "theirs", "");
    save(store, &Target::Global, "shared", "");

    assert_eq!(names(store, &both(REPO), None), ["mine", "shared"]);
    assert_eq!(names(store, &repo_only(REPO), None), ["mine"]);
    assert_eq!(names(store, &global_only(), None), ["shared"]);
    assert_eq!(names(store, &both(OTHER), None), ["theirs", "shared"]);
    assert_eq!(names(store, &both("github.com/acme/unknown"), None), ["shared"]);

    let hidden = store.get_script(&both(OTHER), &ScriptRef::Id(mine)).expect("get");
    assert_eq!(hidden, None);
    assert!(!store.delete_script(&both(OTHER), &ScriptRef::Id(mine)).expect("delete"));
    assert!(
        !store
            .delete_script(&global_only(), &ScriptRef::Name("mine".to_owned()))
            .expect("delete")
    );

    let found = store
        .get_script(&both(REPO), &ScriptRef::Id(mine))
        .expect("get")
        .expect("visible");
    assert_eq!(found.summary.name, "mine");
    assert_eq!(found.body, "echo mine\n");
}

/// A name finds the repository script before the global one with the same name; deleting it
/// uncovers the global one.
pub fn names_resolve_repo_first_and_delete(store: &mut dyn ScriptStore) {
    let local = save(store, &repo(REPO), "deploy", "repo");
    let global = save(store, &Target::Global, "deploy", "global");
    let name = ScriptRef::Name("deploy".to_owned());

    let found = store.get_script(&both(REPO), &name).expect("get").expect("found");
    assert_eq!(found.summary.id, local);
    let found = store.get_script(&global_only(), &name).expect("get").expect("found");
    assert_eq!(found.summary.id, global);
    let missing = store
        .get_script(&both(REPO), &ScriptRef::Name("absent".to_owned()))
        .expect("get");
    assert_eq!(missing, None);

    assert!(store.delete_script(&both(REPO), &name).expect("delete"));
    let found = store.get_script(&both(REPO), &name).expect("get").expect("found");
    assert_eq!(found.summary.id, global);
    assert!(!store.delete_script(&both(REPO), &ScriptRef::Id(local)).expect("delete"));

    assert!(
        store
            .delete_script(&both(REPO), &ScriptRef::Id(global))
            .expect("delete")
    );
    assert_eq!(names(store, &both(REPO), None), Vec::<String>::new());
}

pub fn runs_are_counted(store: &mut dyn ScriptStore) {
    let id = save(store, &repo(REPO), "count-me", "");

    store.record_run(id, Some(0), 1_700_000_000).expect("run");
    store.record_run(id, None, 1_700_000_100).expect("killed run");

    let summary = store
        .get_script(&both(REPO), &ScriptRef::Id(id))
        .expect("get")
        .expect("found")
        .summary;
    assert_eq!(
        (summary.run_count, summary.last_run_at, summary.last_exit_code),
        (2, Some(1_700_000_100), None)
    );

    store.record_run(id, Some(-1), 1_700_000_200).expect("run");
    let summary = &store.list_scripts(&both(REPO), None).expect("list")[0];
    assert_eq!((summary.run_count, summary.last_exit_code), (3, Some(-1)));
}

/// Counting a run of a script that is gone (deleted while it ran) is no error and changes nothing.
pub fn a_run_of_a_missing_script_is_no_error(store: &mut dyn ScriptStore) {
    let id = save(store, &Target::Global, "short-lived", "");
    assert!(store.delete_script(&global_only(), &ScriptRef::Id(id)).expect("delete"));

    store
        .record_run(id, Some(0), 1_700_000_000)
        .expect("nothing left to count");
    store.record_run(id + 1000, None, 1_700_000_000).expect("never existed");
    assert_eq!(names(store, &both(REPO), None), Vec::<String>::new());
}

/// More global scripts than one bulk delete statement binds on the SQLite-dialect stores.
const MANY: usize = 510;

/// `delete_scripts` deletes the visible scripts among the ids, all of them however many, and
/// leaves another repository's script and unknown ids alone.
pub fn deleting_many_takes_only_visible_ids(store: &mut dyn ScriptStore) {
    let mut ids = vec![
        save(store, &repo(REPO), "mine", ""),
        save(store, &repo(OTHER), "theirs", ""),
    ];

    for index in 0..MANY {
        ids.push(save(store, &Target::Global, &format!("shared-{index}"), ""));
    }

    let kept = save(store, &Target::Global, "kept", "");
    ids.push(kept + 1_000_000);

    assert_eq!(store.delete_scripts(&both(REPO), &ids).expect("delete"), MANY + 1);
    assert_eq!(names(store, &both(REPO), None), ["kept"]);
    assert_eq!(names(store, &both(OTHER), None), ["theirs", "kept"]);
    assert_eq!(store.delete_scripts(&both(REPO), &ids).expect("again"), 0);
    assert_eq!(store.delete_scripts(&both(REPO), &[]).expect("none"), 0);
}
