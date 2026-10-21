use std::time::Duration;

use serde_json::json;

use super::store::path_of;
use super::*;
use crate::support::private::random_uuid;

fn item(chat: &str, tool: HeldTool, arguments: Value) -> HeldItem {
    HeldItem::new(chat, "tests", tool, arguments, Value::Null)
}

#[test]
fn stale_items_and_temporary_files_are_swept() {
    let dir = tempfile::tempdir().expect("temp");
    let store = HeldStore::in_folder(dir.path().to_path_buf());
    let chat = random_uuid().expect("uuid");
    let id = store
        .hold(item(&chat, HeldTool::NoteSave, json!({ "title": "a" })))
        .expect("held");

    let old = SystemTime::now() - Duration::from_secs(31 * 24 * 60 * 60);
    let temporary = dir.path().join(HELD_FOLDER).join(&chat).join(".held-x.tmp");
    std::fs::write(&temporary, "x").expect("write");

    for path in [path_of(&store, &chat, &id), temporary.clone()] {
        let file = std::fs::File::options().write(true).open(&path).expect("open");
        file.set_modified(old).expect("mtime");
    }

    store.sweep();
    assert_eq!(store.list(None), Ok(Vec::new()));
    assert!(!temporary.exists());
}

#[test]
fn only_holdable_tools_are_held() {
    assert_eq!(HeldTool::of("note_save"), Some(HeldTool::NoteSave));
    assert_eq!(HeldTool::of("script_save").map(HeldTool::kind), Some(HeldKind::Script));

    for name in [
        "note_delete",
        "entity_link",
        "entity_delete",
        "script_delete",
        "script_run",
    ] {
        assert_eq!(HeldTool::of(name), None, "{name}");
    }
}

#[test]
fn each_chat_has_its_own_folder_and_old_flat_files_are_ignored() {
    let dir = tempfile::tempdir().expect("temp");
    let store = HeldStore::in_folder(dir.path().to_path_buf());
    let (chat, other) = (random_uuid().expect("uuid"), random_uuid().expect("uuid"));
    let mine = store
        .hold(item(&chat, HeldTool::NoteSave, json!({ "title": "mine" })))
        .expect("held");
    let theirs = store
        .hold(item(&other, HeldTool::NoteSave, json!({ "title": "theirs" })))
        .expect("held");

    let path = path_of(&store, &chat, &mine);
    assert!(path.is_file(), "{}", path.display());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path.parent().expect("folder"))
            .expect("folder")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    // A file of the old flat layout, even a valid one, is not listed.
    let flat = dir.path().join(HELD_FOLDER).join(format!("{mine}.json"));
    std::fs::copy(&path, &flat).expect("copy");

    let ids = |chat: Option<&str>| -> Vec<String> {
        let mut ids: Vec<String> = store
            .list(chat)
            .expect("list")
            .into_iter()
            .map(|item| item.held_id)
            .collect();
        ids.sort();
        ids
    };
    let mut both = vec![mine.clone(), theirs.clone()];
    both.sort();
    assert_eq!(ids(None), both);
    assert_eq!(ids(Some(&chat)), std::slice::from_ref(&mine));
    assert_eq!(ids(Some(&other)), std::slice::from_ref(&theirs));

    // An item copied into another chat's folder is not that chat's.
    std::fs::copy(&path, path_of(&store, &other, &mine)).expect("copy");
    assert_eq!(ids(Some(&other)), std::slice::from_ref(&theirs));

    let claim = store.claim(&mine).expect("claim").expect("waiting");
    assert_eq!(claim.item.chat_key, chat);
    claim.finish().expect("done");
    assert_eq!(ids(Some(&chat)), Vec::<String>::new());
}

#[test]
fn a_chat_holds_at_most_four_mebibytes() {
    let dir = tempfile::tempdir().expect("temp");
    let store = HeldStore::in_folder(dir.path().to_path_buf());
    let (chat, other) = (random_uuid().expect("uuid"), random_uuid().expect("uuid"));
    let body = "x".repeat(400 * 1024);
    let big = |title: &str| item(&chat, HeldTool::NoteSave, json!({ "title": title, "body": body }));

    let mut held = 0;
    let refused = loop {
        match store.hold(big(&format!("note {held}"))) {
            Ok(_) => held += 1,
            Err(error) => break error,
        }
    };

    assert_eq!(refused, HoldError::Limit);
    assert_eq!(held, 10, "ten items of 400 KiB fit in 4 MiB");
    assert_eq!(store.list(Some(&chat)).expect("list").len(), held);

    // Replacing an item frees its own bytes first, and another chat has its own budget.
    store.hold(big("note 3")).expect("replaced");
    store
        .hold(item(
            &other,
            HeldTool::NoteSave,
            json!({ "title": "other", "body": body }),
        ))
        .expect("another chat");
}
