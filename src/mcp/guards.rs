//! The save guards: what the user lets agents save to Harness Memory, per chat, per repository
//! and everywhere, and "Ask before saving". Reading and recall are never guarded, nor is
//! `script_run`, but while any guard applies to a call ([`applies`]) `script_save` trusts nothing
//! it saves, so `script_run` asks the user first.
//!
//! The guarded tools are the note writes (`note_save`, `note_update`, `note_delete`,
//! `entity_link`, `entity_delete`) and the script writes (`script_save`, `script_delete`). Before
//! any of their bodies runs ([`gate`]), in this order:
//!
//! 1. `guards/global.json` next to the database: the kind paused everywhere, then the kind turned
//!    off for the chat's repository (`offRepos`, canonical git roots). The chat's repository is
//!    the git root of the resolved `project_path` (that of the server's working folder when the
//!    call names none), and the `repoRoot` of the chat's file (below): any one turned off, by its root or by its project
//!    key ([`super::guard_repos`]), refuses, so global-scope writes from that repository, and
//!    writes from another clone or worktree of it, are refused too.
//! 2. Without [`SESSION_VAR`] nothing more applies. A value that is not a lowercase UUID refuses.
//! 3. `guards/chats/<chatKey>.json`: a missing file refuses (Harness Desktop writes it before it
//!    starts the chat). Its optional `repoRoot` (the chat's project root, absolute) counts as the
//!    chat's repository in step 1.
//! 4. The chat paused, or the kind off in the chat, refuses.
//! 5. "Ask" on for the kind holds `note_save`, `note_update` and `script_save` for the user's
//!    review ([`super::hold`]) when Harness Desktop runs (`desktop.alive` fresh), else refuses.
//!    Deletes and links pass.
//!
//! The read tools tell the agent what the same rules say about saving what they read
//! ([`super::saving`]).
//!
//! A guard file that cannot be read (a symbolic link, another user's, another version, not the
//! shape below) refuses: the guards fail closed. A missing global file applies no rule. Every reply is a
//! normal tool result (not an error, so the agent does not retry), with `structuredContent`
//! `{"held": true, "heldId"}` or `{"saved": false, "reason"}`.
//!
//! The guards stop agents that use these tools. They are no sandbox: an agent with a shell can
//! change files under the user's home or run the user-only commands.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::Server;
use super::calls::Reply;
use super::guard_repos::Repos;
use crate::backend::SESSION_VAR;
use crate::held::HeldTool;
use crate::support::private::{check_folders, data_folder, desktop_alive, is_lowercase_uuid, read_private};

/// The folder next to the database that holds the guard files Harness Desktop writes.
pub const GUARDS_FOLDER: &str = "guards";

/// The folder in [`GUARDS_FOLDER`] that holds one file per chat.
const CHATS_FOLDER: &str = "chats";

/// The largest guard file read, in bytes.
const GUARD_FILE_MAX: usize = 512 * 1024;

/// The reply to a held call.
pub const HELD_TEXT: &str = "Waiting for the user's review in Harness Desktop. Nothing is saved yet. Do not save it \
                             again or retry, the user decides. Continue with your task.";

/// What a guarded tool writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Notes,
    Scripts,
}

impl Kind {
    /// The kind tool `name` writes; `None` for a tool that is not guarded.
    fn of(name: &str) -> Option<Self> {
        match name {
            "note_save" | "note_update" | "note_delete" | "entity_link" | "entity_delete" => Some(Self::Notes),
            "script_save" | "script_delete" => Some(Self::Scripts),
            _ => None,
        }
    }

    pub(super) fn words(self) -> &'static str {
        match self {
            Self::Notes => "notes",
            Self::Scripts => "scripts",
        }
    }
}

/// Why a guarded call was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reason {
    ChatOff,
    ChatPaused,
    RepoOff,
    GlobalPaused,
    NoDesktop,
    HeldLimit,
    GuardUnreadable,
}

impl Reason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::ChatOff => "chat-off",
            Self::ChatPaused => "chat-paused",
            Self::RepoOff => "repo-off",
            Self::GlobalPaused => "global-paused",
            Self::NoDesktop => "no-desktop",
            Self::HeldLimit => "held-limit",
            Self::GuardUnreadable => "guard-unreadable",
        }
    }
}

/// `guards/global.json`.
#[derive(Deserialize)]
struct GlobalFile {
    notes: GlobalKind,
    scripts: GlobalKind,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GlobalKind {
    paused: bool,
    off_repos: Vec<String>,
}

/// `guards/chats/<chatKey>.json`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatFile {
    paused: bool,
    notes: ChatKind,
    scripts: ChatKind,
    /// The chat's project root as Harness Desktop resolves it (absolute), when it has one.
    #[serde(default)]
    repo_root: Option<String>,
}

#[derive(Deserialize)]
struct ChatKind {
    on: bool,
    ask: bool,
}

/// The guard files that apply to a call.
struct Rules {
    data: Option<PathBuf>,
    /// `Err` when `guards/global.json` cannot be read.
    global: Result<Option<GlobalFile>, ()>,
    chat: Chat,
}

/// The chat of a call and its rules.
enum Chat {
    /// No [`SESSION_VAR`]: no chat rules.
    None,
    /// A session that is not a lowercase UUID, no data folder, or a chat file that is missing or
    /// cannot be read.
    Unreadable,
    Found {
        key: String,
        file: ChatFile,
    },
}

impl Chat {
    /// The chat's project root from its file, when it names one.
    fn repo_root(&self) -> Option<&str> {
        match self {
            Self::Found { file, .. } => file.repo_root.as_deref(),
            _ => None,
        }
    }
}

/// What the guards decide for one call.
enum Decision {
    Allow,
    Refuse(Reason),
    /// Hold `tool` for chat `chat`, with the held items in the data folder `data`.
    Hold {
        tool: HeldTool,
        chat: String,
        data: PathBuf,
        repo_root: Option<String>,
    },
}

/// The reply that refuses or holds the call of tool `name` with `arguments`; `None` when the
/// guards let it run (see the module docs).
pub(super) fn gate(server: &mut Server, name: &str, arguments: &Value) -> Option<Reply> {
    let kind = Kind::of(name)?;

    match decide(server, kind, name, arguments) {
        Decision::Allow => None,
        Decision::Refuse(reason) => Some(refused(reason, kind, name)),
        Decision::Hold {
            tool,
            chat,
            data,
            repo_root,
        } => Some(super::hold::hold(server, tool, arguments, &chat, data, repo_root)),
    }
}

/// True when a guard applies to the call with `arguments`: a guard file cannot be read, notes or
/// scripts are paused everywhere or turned off for the call's repository, or the call's chat is
/// paused, has notes or scripts off, or asks before saving them. `script_save` then trusts
/// nothing it saves.
pub(super) fn applies(server: &mut Server, arguments: &Value) -> bool {
    let rules = rules(server);

    let global = match &rules.global {
        Ok(global) => global,
        Err(()) => return true,
    };

    if let Some(global) = global {
        let mut repos = Repos::new(arguments, rules.chat.repo_root());

        for guard in [&global.notes, &global.scripts] {
            if guard.paused || repos.any_off(server, &guard.off_repos) {
                return true;
            }
        }
    }

    match &rules.chat {
        Chat::None => false,
        Chat::Unreadable => true,
        Chat::Found { file, .. } => {
            let strict = |guard: &ChatKind| !guard.on || guard.ask;
            file.paused || strict(&file.notes) || strict(&file.scripts)
        }
    }
}

fn decide(server: &mut Server, kind: Kind, name: &str, arguments: &Value) -> Decision {
    let (chat, data, repo_root) = match verdict(server, kind, arguments) {
        Verdict::Allow => return Decision::Allow,
        Verdict::Refuse(reason) => return Decision::Refuse(reason),
        Verdict::Ask { chat, data, repo_root } => (chat, data, repo_root),
    };

    // Deletes and links pass while the chat asks before saving.
    let Some(tool) = HeldTool::of(name) else {
        return Decision::Allow;
    };

    let Some(data) = data.filter(|data| desktop_alive(data)) else {
        return Decision::Refuse(Reason::NoDesktop);
    };

    Decision::Hold {
        tool,
        chat,
        data,
        repo_root,
    }
}

/// What the guards say about saving `kind` in a call with `arguments`, whatever the tool.
pub(super) enum Verdict {
    Allow,
    Refuse(Reason),
    /// "Ask before saving" is on for the kind in chat `chat`, whose data folder is `data`.
    Ask {
        chat: String,
        data: Option<PathBuf>,
        repo_root: Option<String>,
    },
}

/// The guards' verdict on saving `kind` in a call with `arguments`, read from the guard files
/// now (steps 1 to 5 of the module docs, without the tool).
pub(super) fn verdict(server: &mut Server, kind: Kind, arguments: &Value) -> Verdict {
    let rules = rules(server);

    match &rules.global {
        Err(()) => return Verdict::Refuse(Reason::GuardUnreadable),
        Ok(Some(global)) => {
            let guard = match kind {
                Kind::Notes => &global.notes,
                Kind::Scripts => &global.scripts,
            };

            if guard.paused {
                return Verdict::Refuse(Reason::GlobalPaused);
            }

            if Repos::new(arguments, rules.chat.repo_root()).any_off(server, &guard.off_repos) {
                return Verdict::Refuse(Reason::RepoOff);
            }
        }
        Ok(None) => {}
    }

    let (key, chat) = match rules.chat {
        Chat::None => return Verdict::Allow,
        Chat::Unreadable => return Verdict::Refuse(Reason::GuardUnreadable),
        Chat::Found { key, file } => (key, file),
    };

    if chat.paused {
        return Verdict::Refuse(Reason::ChatPaused);
    }

    let guard = match kind {
        Kind::Notes => &chat.notes,
        Kind::Scripts => &chat.scripts,
    };

    if !guard.on {
        return Verdict::Refuse(Reason::ChatOff);
    }

    if !guard.ask {
        return Verdict::Allow;
    }

    Verdict::Ask {
        chat: key,
        data: rules.data,
        repo_root: chat.repo_root,
    }
}

/// The guard files of the server's data folder and chat (see the module docs).
fn rules(server: &Server) -> Rules {
    let env = server.projects.env();
    let data = data_folder(env);

    let global = match &data {
        Some(data) => read_global(data),
        None => Ok(None),
    };

    let chat = match (env.var(SESSION_VAR), &data) {
        (None, _) => Chat::None,
        (Some(session), Some(data)) if is_lowercase_uuid(session) => match read_chat(data, session) {
            Ok(Some(file)) => Chat::Found {
                key: session.to_owned(),
                file,
            },
            Ok(None) | Err(()) => Chat::Unreadable,
        },
        (Some(_), _) => Chat::Unreadable,
    };

    Rules { data, global, chat }
}

/// `guards/global.json` in `data`: `None` when there is none, `Err` when it cannot be read.
fn read_global(data: &Path) -> Result<Option<GlobalFile>, ()> {
    let guards = data.join(GUARDS_FOLDER);
    read_guard(
        &guards.join("global.json"),
        &[(data, "data folder"), (&guards, "guards folder")],
    )
}

/// The file of chat `key` in `data`: `None` when there is none, `Err` when it cannot be read or
/// names a project root that is not an absolute path.
fn read_chat(data: &Path, key: &str) -> Result<Option<ChatFile>, ()> {
    let guards = data.join(GUARDS_FOLDER);
    let chats = guards.join(CHATS_FOLDER);
    let file: Option<ChatFile> = read_guard(
        &chats.join(format!("{key}.json")),
        &[
            (data, "data folder"),
            (&guards, "guards folder"),
            (&chats, "chats folder"),
        ],
    )?;

    let relative = file
        .as_ref()
        .and_then(|file| file.repo_root.as_deref())
        .is_some_and(|root| !Path::new(root).is_absolute());

    if relative {
        return Err(());
    }

    Ok(file)
}

/// The guard file `path` of version 1 in the private `folders`: `None` when there is none.
fn read_guard<T: DeserializeOwned>(path: &Path, folders: &[(&Path, &str)]) -> Result<Option<T>, ()> {
    let Some(bytes) = read_private(path, GUARD_FILE_MAX).map_err(|_| ())? else {
        return Ok(None);
    };

    check_folders(folders).map_err(|_| ())?;

    let value: Value = serde_json::from_slice(&bytes).map_err(|_| ())?;

    if value.get("version") != Some(&json!(1)) {
        return Err(());
    }

    serde_json::from_value(value).map(Some).map_err(|_| ())
}

/// The reply to a held call: `heldId` names the held item.
pub(super) fn held(held_id: &str) -> Reply {
    Reply {
        text: HELD_TEXT.to_owned(),
        is_error: false,
        structured: Some(json!({ "held": true, "heldId": held_id })),
    }
}

/// The reply refusing a call of tool `name`, writing `kind`, for `reason`.
pub(super) fn refused(reason: Reason, kind: Kind, name: &str) -> Reply {
    let words = kind.words();
    let why = match reason {
        Reason::ChatOff => format!("The user turned off saving {words} for this chat."),
        Reason::ChatPaused => "The user paused saving to Harness Memory for this chat.".to_owned(),
        Reason::RepoOff => format!("The user turned off saving {words} for this project."),
        Reason::GlobalPaused => format!("The user paused saving {words} everywhere."),
        Reason::NoDesktop => {
            format!("The user reviews {words} before they are saved, and Harness Desktop is not running.")
        }
        Reason::HeldLimit => "Too many saves are waiting for the user's review in this chat.".to_owned(),
        Reason::GuardUnreadable => {
            format!("The Harness Memory save settings could not be read, so saving {words} is blocked.")
        }
    };

    let deletes = matches!(name, "note_delete" | "entity_delete" | "script_delete");
    let rest = if deletes {
        "Nothing was deleted. Do not retry. Continue with your task."
    } else {
        "Nothing was saved. Do not retry. Continue without saving."
    };

    Reply {
        text: format!("{why} {rest}"),
        is_error: false,
        structured: Some(json!({ "saved": false, "reason": reason.as_str() })),
    }
}

/// The kind tool `name` writes, for the held items.
pub(super) fn kind_of(tool: HeldTool) -> Kind {
    match tool {
        HeldTool::NoteSave | HeldTool::NoteUpdate => Kind::Notes,
        HeldTool::ScriptSave => Kind::Scripts,
    }
}
