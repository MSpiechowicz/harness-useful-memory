//! The read tools say when the user does not plainly let agents save what they read: the notes
//! reads (`note_search`, `note_get`, `entity_neighbors`) for notes, the scripts reads
//! (`script_list`, `script_get`) for scripts. One line ends the reply, the read still returns
//! what it found, and scripts replies carry `structuredContent.saving`. Every switch is checked
//! for both kinds, and the other kind's reads stay without a notice.

mod common;

use common::guards::{CHAT, Guarded, structured, tool};
use common::mcp::{is_error, text};
use harness_memory::Server;
use harness_memory::limits::LIST_REPLY_MAX;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Notes,
    Scripts,
}

const KINDS: [Kind; 2] = [Kind::Notes, Kind::Scripts];

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Self::Notes => "notes",
            Self::Scripts => "scripts",
        }
    }

    fn other(self) -> Self {
        match self {
            Self::Notes => Self::Scripts,
            Self::Scripts => Self::Notes,
        }
    }

    /// The read tools of the kind, with the arguments that find what [`seeded`] saved.
    fn reads(self) -> Vec<(&'static str, Value)> {
        match self {
            Self::Notes => vec![
                ("note_search", json!({ "query": "cache" })),
                ("note_get", json!({ "ids": [1] })),
                ("entity_neighbors", json!({ "entity": "cache" })),
            ],
            Self::Scripts => vec![("script_list", json!({})), ("script_get", json!({ "name": "hello" }))],
        }
    }

    /// The chat file rules (`on`, `ask`) of notes and scripts with this kind set to `rule`.
    fn rules(self, rule: (bool, bool)) -> ((bool, bool), (bool, bool)) {
        match self {
            Self::Notes => (rule, (true, false)),
            Self::Scripts => ((true, false), rule),
        }
    }

    /// `guards/global.json` with this kind paused or turned off for `off`.
    fn global(self, paused: bool, off: &[String]) -> String {
        let guarded = json!({ "paused": paused, "offRepos": off });
        let free = json!({ "paused": false, "offRepos": [] });
        let (notes, scripts) = match self {
            Self::Notes => (guarded, free),
            Self::Scripts => (free, guarded),
        };
        json!({ "version": 1, "notes": notes, "scripts": scripts }).to_string()
    }
}

/// A global note (#1) and a global script (`hello`) saved while every guard is off, so every
/// read finds them from any folder.
fn seeded() -> Guarded {
    let guarded = Guarded::new();
    let mut server = guarded.chat_server();

    let note = json!({ "title": "Cache location", "body": "The cache lives in src/cache.rs",
                       "kind": "location", "scope": "global", "entities": ["cache"] });
    let script = json!({ "name": "hello", "description": "Says hi", "language": "sh",
                         "body": "echo hi", "scope": "global" });

    for (name, arguments) in [("note_save", note), ("script_save", script)] {
        let reply = tool(&mut server, name, arguments);
        assert!(!is_error(&reply), "{reply}");
        assert!(structured(&reply).get("saved").is_none(), "{reply}");
    }

    guarded
}

/// Calls the read `name` and checks it still returns what it read.
fn read(server: &mut Server, name: &str, arguments: Value) -> Value {
    let reply = tool(server, name, arguments);
    assert!(!is_error(&reply), "{name}: {reply}");

    match name {
        "note_search" => assert!(text(&reply).contains("Cache location"), "{name}: {reply}"),
        "note_get" => assert!(
            text(&reply).contains("The cache lives in src/cache.rs"),
            "{name}: {reply}"
        ),
        "script_list" => assert_eq!(structured(&reply)["scripts"][0]["name"], "hello", "{name}"),
        "script_get" => assert_eq!(structured(&reply)["script"]["body"], "echo hi", "{name}"),
        _ => {}
    }

    reply
}

/// Checks every read of `kind` ends with `line` and, for scripts, carries `saving`.
fn assert_notice(server: &mut Server, kind: Kind, line: &str, saving: &Value) {
    for (name, arguments) in kind.reads() {
        let reply = read(server, name, arguments);
        assert_eq!(text(&reply).lines().last(), Some(line), "{name}");
        assert_eq!(text(&reply).matches(line).count(), 1, "{name}");

        if kind == Kind::Scripts {
            assert_eq!(&structured(&reply)["saving"], saving, "{name}");
        }
    }
}

/// Checks no read of `kind` has a notice.
fn assert_no_notice(server: &mut Server, kind: Kind) {
    for (name, arguments) in kind.reads() {
        let reply = read(server, name, arguments);
        let text = text(&reply);
        assert!(!text.contains("Reading works."), "{name}: {text}");
        assert!(!text.contains("wait for the user's review"), "{name}: {text}");
        assert!(structured(&reply).get("saving").is_none(), "{name}: {reply}");
    }
}

/// The `saving` of a read whose kind cannot be saved for `reason`.
fn refused(reason: &str) -> Value {
    json!({ "allowed": false, "reason": reason })
}

/// The reason a write was refused for.
fn refusal(reply: &Value) -> String {
    assert!(!is_error(reply), "{reply}");
    assert_eq!(structured(reply)["saved"], false, "{reply}");
    structured(reply)["reason"].as_str().expect("reason").to_owned()
}

#[test]
fn reads_say_when_saving_their_kind_is_off_for_the_chat() {
    for kind in KINDS {
        let guarded = seeded();
        let (notes, scripts) = kind.rules((false, false));
        guarded.rules(false, notes, scripts);
        let mut server = guarded.chat_server();
        let words = kind.word();

        let line = format!(
            "Saving {words} is off for this chat (the user's setting). Reading works. Do not try to save {words}."
        );
        assert_notice(&mut server, kind, &line, &refused("chat-off"));
        assert_no_notice(&mut server, kind.other());
    }
}

#[test]
fn reads_of_both_kinds_say_when_saving_is_paused_for_the_chat() {
    let guarded = seeded();
    guarded.rules(true, (true, false), (true, false));
    let mut server = guarded.chat_server();

    let line = "Saving is paused for this chat (the user's setting). Reading works. Do not try to save.";

    for kind in KINDS {
        assert_notice(&mut server, kind, line, &refused("chat-paused"));
    }
}

#[test]
fn reads_say_when_saving_their_kind_is_paused_everywhere() {
    for kind in KINDS {
        let guarded = seeded();
        guarded.global(&kind.global(true, &[]));
        let words = kind.word();
        let line = format!(
            "Saving {words} is paused everywhere (the user's setting). Reading works. Do not try to save {words}."
        );

        // In a chat, and without one.
        for mut server in [guarded.chat_server(), guarded.server(None, &guarded.work())] {
            assert_notice(&mut server, kind, &line, &refused("global-paused"));
            assert_no_notice(&mut server, kind.other());
        }
    }
}

#[test]
fn reads_say_when_saving_their_kind_is_off_for_the_repository_and_writes_are_refused() {
    for kind in KINDS {
        let guarded = seeded();
        let root = guarded.repo.to_string_lossy().into_owned();
        guarded.global(&kind.global(false, std::slice::from_ref(&root)));
        let words = kind.word();
        let line = format!(
            "Saving {words} is off for this project (the user's setting). Reading works. Do not try to save {words}."
        );

        // The server started in the repository, and the repository named as the chat's root from
        // outside it.
        let in_repo = guarded.chat_server();

        let text = json!({ "version": 1, "paused": false, "repoRoot": root,
                           "notes": { "on": true, "ask": false }, "scripts": { "on": true, "ask": false } });
        guarded.chat(CHAT, &text.to_string());
        let outside = guarded.server(Some(CHAT), &guarded.work());

        for mut server in [in_repo, outside] {
            assert_notice(&mut server, kind, &line, &refused("repo-off"));
            assert_no_notice(&mut server, kind.other());

            if kind == Kind::Scripts {
                let script = json!({ "name": "other", "description": "", "language": "sh", "body": "true" });
                assert_eq!(refusal(&tool(&mut server, "script_save", script)), "repo-off");
                let delete = tool(&mut server, "script_delete", json!({ "name": "hello" }));
                assert_eq!(refusal(&delete), "repo-off");
            }
        }
    }
}

#[test]
fn reads_say_when_saves_of_their_kind_wait_for_review_and_saves_are_held() {
    for kind in KINDS {
        let guarded = seeded();
        let (notes, scripts) = kind.rules((true, true));
        guarded.rules(false, notes, scripts);
        guarded.desktop_alive();
        let mut server = guarded.chat_server();

        let line = match kind {
            Kind::Notes => "Notes you save in this chat wait for the user's review before they are kept.",
            Kind::Scripts => "Scripts you save in this chat wait for the user's review before they are kept.",
        };
        assert_notice(&mut server, kind, line, &json!({ "ask": true }));
        assert_no_notice(&mut server, kind.other());

        if kind == Kind::Scripts {
            let script = json!({ "name": "reviewed", "description": "", "language": "sh", "body": "true" });
            let held = tool(&mut server, "script_save", script);
            assert!(!is_error(&held), "{held}");
            assert_eq!(structured(&held)["held"], true, "{held}");
            assert_eq!(guarded.held_files().len(), 1);
        }
    }
}

#[test]
fn reads_say_when_the_save_settings_cannot_be_read() {
    let guarded = seeded();
    // A chat without its guard file.
    let mut server = guarded.server(Some("0b4e28ba-2fa1-4d3b-a3f5-ef19b5a7633b"), &guarded.repo);

    for kind in KINDS {
        let words = kind.word();
        let line = format!("Saving {words} is blocked: the save settings could not be read. Reading works.");
        assert_notice(&mut server, kind, &line, &refused("guard-unreadable"));
    }
}

#[test]
fn reads_have_no_notice_while_saving_is_allowed() {
    let guarded = seeded();

    for mut server in [guarded.chat_server(), guarded.server(None, &guarded.repo)] {
        for kind in KINDS {
            assert_no_notice(&mut server, kind);
        }
    }

    // An empty result gets no notice either.
    let mut server = guarded.chat_server();
    let reply = tool(&mut server, "note_search", json!({ "query": "nothing-like-this" }));
    assert_eq!(text(&reply), "no matching notes");
}

#[test]
fn a_full_search_reply_with_a_notice_stays_within_its_cap() {
    let guarded = Guarded::new();
    let mut server = guarded.chat_server();
    let search = json!({ "query": "cache", "limit": 20 });

    for index in 1..=20 {
        let note = json!({ "title": format!("Cache note {index} x"), "body": "The cache lives in src/cache.rs",
                           "kind": "location", "scope": "global" });
        let reply = tool(&mut server, "note_save", note);
        assert!(!is_error(&reply), "{reply}");
    }

    // Longer titles bring the 20 notes to just under the cap, with no room left for the notice.
    let unpadded = text(&tool(&mut server, "note_search", search.clone())).len();
    let padding = (LIST_REPLY_MAX - 60).checked_sub(unpadded).expect("the notes fit");

    for index in 1..=20 {
        let extra = padding / 20 + if index == 1 { padding % 20 } else { 0 };
        let title = format!("Cache note {index} x{}", "x".repeat(extra));
        let reply = tool(
            &mut server,
            "note_update",
            json!({ "id": index, "title": title, "scope": "global" }),
        );
        assert!(!is_error(&reply), "{reply}");
    }

    let padded = tool(&mut server, "note_search", search.clone());
    assert_eq!(text(&padded).len(), LIST_REPLY_MAX - 60, "{padded}");

    let (notes, scripts) = Kind::Notes.rules((false, false));
    guarded.rules(false, notes, scripts);
    let mut server = guarded.chat_server();

    let reply = tool(&mut server, "note_search", search);
    let text = text(&reply);
    let line = "Saving notes is off for this chat (the user's setting). Reading works. Do not try to save notes.";

    assert!(text.len() <= LIST_REPLY_MAX, "{} bytes: {text}", text.len());
    assert_eq!(text.lines().last(), Some(line));
    assert!(text.contains("… (1 more)"), "{text}");
}
