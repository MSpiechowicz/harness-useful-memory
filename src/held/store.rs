//! The held files on disk: `held/<chatKey>/<heldId>.json` next to the database, one folder per
//! chat (see the module docs of [`crate::held`]).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::{HELD_CHAT_BYTES, HELD_FILE_MAX, HELD_FOLDER, HELD_PER_CHAT, HeldItem, VERSION, now};
use crate::support::Env;
use crate::support::private::{
    check_folders, data_folder, is_lowercase_uuid, private_folder, random_uuid, read_private, write_atomic,
};

/// How long a held item is kept.
const KEEP_FOR: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The prefix of the temporary files a held item is written through, claimed or moved aside
/// under.
const TEMP_PREFIX: &str = ".held-";

/// Why an item was not held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldError {
    /// The chat has [`HELD_PER_CHAT`] items or [`HELD_CHAT_BYTES`] bytes waiting, or the item is
    /// larger than [`HELD_FILE_MAX`].
    Limit,
    /// The folder or the file could not be used (the reason).
    Failed(String),
}

/// The held items next to the database.
#[derive(Debug, Clone)]
pub struct HeldStore {
    data: PathBuf,
    folder: PathBuf,
}

/// A waiting item and the size of its file.
struct Waiting {
    item: HeldItem,
    bytes: u64,
}

impl HeldStore {
    /// The held items in the data folder `env` names; `None` when it is unknown.
    pub fn open(env: &Env) -> Option<Self> {
        data_folder(env).map(Self::in_folder)
    }

    /// The held items in the data folder `data`.
    pub fn in_folder(data: PathBuf) -> Self {
        let folder = data.join(HELD_FOLDER);
        Self { data, folder }
    }

    /// Holds `item` (see the module docs): the `heldId` it was held under.
    pub fn hold(&self, mut item: HeldItem) -> Result<String, HoldError> {
        if !is_lowercase_uuid(&item.chat_key) {
            return Err(HoldError::Failed("the chat key is not a lowercase UUID".to_owned()));
        }

        let chat = self.folder.join(&item.chat_key);

        private_folder(&self.folder, "held folder")
            .and_then(|()| private_folder(&chat, "chat's held folder"))
            .and_then(|()| self.check(Some(&chat)))
            .map_err(HoldError::Failed)?;

        self.sweep();

        let waiting = self.waiting(&chat).map_err(HoldError::Failed)?;
        let earlier = waiting.iter().find(|earlier| earlier.item.is_replaced_by(&item));
        let others: Vec<&Waiting> = waiting
            .iter()
            .filter(|other| earlier.is_none_or(|earlier| other.item.held_id != earlier.item.held_id))
            .collect();
        let other_bytes = others.iter().map(|other| other.bytes).sum::<u64>();

        // Moved aside before it is replaced: an item a save claimed meanwhile is gone, and this
        // one is held anew.
        let aside = match earlier {
            Some(earlier) => move_aside(&chat, &earlier.item.held_id).map_err(HoldError::Failed)?,
            None => None,
        };

        match (earlier, &aside) {
            (Some(earlier), Some(_)) => {
                item.held_id = earlier.item.held_id.clone();
                item.created_at = earlier.item.created_at.clone();
            }
            _ => {
                if others.len() >= HELD_PER_CHAT {
                    return Err(HoldError::Limit);
                }

                item.held_id =
                    random_uuid().ok_or_else(|| HoldError::Failed("no random id could be made".to_owned()))?;
                item.created_at = crate::support::time::rfc3339(now());
            }
        }

        let written = write(&chat, &mut item, other_bytes);
        let path = item_path(&chat, &item.held_id);

        match (&written, aside) {
            (Ok(()), Some(aside)) => {
                let _ = std::fs::remove_file(aside);
            }
            (Err(_), Some(aside)) => {
                let _ = std::fs::rename(aside, &path);
            }
            (_, None) => {}
        }

        written.map(|()| item.held_id)
    }

    /// Claims item `held_id` for saving or skipping (see the module docs); `None` when it is not
    /// waiting (a save or skip took it, or it was never held). Nothing lists, saves, skips or
    /// replaces a claimed item until it is [`Claim::restore`]d.
    pub fn claim(&self, held_id: &str) -> Result<Option<Claim>, String> {
        for chat in self.locate(held_id)? {
            let Some(path) = move_aside(&chat, held_id)? else {
                continue;
            };

            let home = item_path(&chat, held_id);

            match parse(&chat, &path, held_id) {
                Some(item) => return Ok(Some(Claim { item, path, home })),
                None => put_back(&path, &home)?,
            }
        }

        Ok(None)
    }

    /// The items waiting, of chat `chat` only (reading only its folder) when given, newest first.
    pub fn list(&self, chat: Option<&str>) -> Result<Vec<HeldItem>, String> {
        if !self.exists()? {
            return Ok(Vec::new());
        }

        let chats = match chat {
            Some(chat) if is_lowercase_uuid(chat) => vec![self.folder.join(chat)],
            Some(_) => Vec::new(),
            None => self.chat_folders()?,
        };

        let mut items = Vec::new();

        for chat in chats {
            if !is_folder(&chat) {
                continue;
            }

            self.check(Some(&chat))?;
            let waiting = self.waiting(&chat)?;
            items.extend(waiting.into_iter().map(|waiting| waiting.item));
        }

        items.sort_by(|a, b| (&b.created_at, &b.held_id).cmp(&(&a.created_at, &a.held_id)));
        Ok(items)
    }

    /// The item `held_id`, when it is waiting.
    pub fn get(&self, held_id: &str) -> Result<Option<HeldItem>, String> {
        let found = self
            .locate(held_id)?
            .into_iter()
            .find_map(|chat| parse(&chat, &item_path(&chat, held_id), held_id));

        Ok(found)
    }

    /// Removes the items and half-written files older than 30 days, in every chat's folder (and
    /// those left at the top of the held folder by an older layout).
    pub fn sweep(&self) {
        sweep_folder(&self.folder);

        for chat in self.chat_folders().unwrap_or_default() {
            sweep_folder(&chat);
        }
    }

    /// The parsed items in the folder `chat`, with the sizes of their files.
    fn waiting(&self, chat: &Path) -> Result<Vec<Waiting>, String> {
        let entries = match std::fs::read_dir(chat) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("the held folder cannot be read: {error}")),
        };

        let mut waiting = Vec::new();

        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };

            let path = item_path(chat, id);
            let Some(item) = is_lowercase_uuid(id).then(|| parse(chat, &path, id)).flatten() else {
                continue;
            };

            let bytes = std::fs::symlink_metadata(&path).map_or(0, |metadata| metadata.len());
            waiting.push(Waiting { item, bytes });
        }

        Ok(waiting)
    }

    /// The chats' folders that hold a file named for item `held_id` (only the one of its chat
    /// holds the item itself, see [`parse`]).
    fn locate(&self, held_id: &str) -> Result<Vec<PathBuf>, String> {
        if !is_lowercase_uuid(held_id) || !self.exists()? {
            return Ok(Vec::new());
        }

        let mut found = Vec::new();

        for chat in self.chat_folders()? {
            if std::fs::symlink_metadata(item_path(&chat, held_id)).is_ok() {
                self.check(Some(&chat))?;
                found.push(chat);
            }
        }

        Ok(found)
    }

    /// The chats' folders: the folders in the held folder named by a lowercase UUID.
    fn chat_folders(&self) -> Result<Vec<PathBuf>, String> {
        let entries = match std::fs::read_dir(&self.folder) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("the held folder cannot be read: {error}")),
        };

        let folders = entries
            .flatten()
            .filter(|entry| entry.file_name().to_str().is_some_and(is_lowercase_uuid))
            .map(|entry| entry.path())
            .filter(|path| is_folder(path))
            .collect();

        Ok(folders)
    }

    /// True when the held folder exists; fails when it or the data folder is not private.
    fn exists(&self) -> Result<bool, String> {
        match std::fs::symlink_metadata(&self.folder) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            _ => self.check(None).map(|()| true),
        }
    }

    /// Fails unless the data folder, the held folder and the folder `chat` (when given) are
    /// private folders of this user.
    fn check(&self, chat: Option<&Path>) -> Result<(), String> {
        let mut folders = vec![
            (self.data.as_path(), "data folder"),
            (self.folder.as_path(), "held folder"),
        ];
        folders.extend(chat.map(|chat| (chat, "chat's held folder")));
        check_folders(&folders)
    }

    /// The data folder.
    pub fn data(&self) -> &Path {
        &self.data
    }
}

/// A held item claimed for saving or skipping ([`HeldStore::claim`]): its file under a claim name.
#[derive(Debug)]
pub struct Claim {
    pub item: HeldItem,
    path: PathBuf,
    /// Where the file was waiting.
    home: PathBuf,
}

impl Claim {
    /// Removes the claimed item: it was saved or skipped.
    pub fn finish(self) -> Result<(), String> {
        std::fs::remove_file(&self.path).map_err(|error| format!("removing the held item failed: {error}"))
    }

    /// Replaces the claimed item's `current` with `current` (its `revision` changes with it),
    /// rewriting the file under its claim name, so [`Self::restore`] puts it back as it is now.
    pub fn refresh(&mut self, current: Value) -> Result<(), String> {
        let mut item = self.item.clone();
        item.current = current;
        item.revision = item.digest();

        let text = serde_json::to_vec(&item).map_err(|error| error.to_string())?;

        if text.len() > HELD_FILE_MAX {
            return Err(format!("the held item would be larger than {HELD_FILE_MAX} bytes"));
        }

        let folder = self
            .path
            .parent()
            .ok_or_else(|| "the held item has no folder".to_owned())?;
        write_atomic(folder, &self.path, &text, TEMP_PREFIX, "held item")?;

        self.item = item;
        Ok(())
    }

    /// Puts the item back where it waited: it was not saved.
    pub fn restore(self) -> Result<(), String> {
        put_back(&self.path, &self.home)
    }
}

/// Writes `item` into the folder `chat` under its `heldId`, with its revision, unless it is
/// larger than [`HELD_FILE_MAX`] or the chat's other items (`other_bytes`) and it are larger than
/// [`HELD_CHAT_BYTES`].
fn write(chat: &Path, item: &mut HeldItem, other_bytes: u64) -> Result<(), HoldError> {
    item.revision = item.digest();
    let text = serde_json::to_vec(&*item).map_err(|error| HoldError::Failed(error.to_string()))?;
    let total = other_bytes.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));

    if text.len() > HELD_FILE_MAX || total > HELD_CHAT_BYTES {
        return Err(HoldError::Limit);
    }

    write_atomic(chat, &item_path(chat, &item.held_id), &text, TEMP_PREFIX, "held item").map_err(HoldError::Failed)
}

/// Moves the file of item `held_id` in the folder `chat` to a new temporary name there: that
/// path, or `None` when the file is gone.
fn move_aside(chat: &Path, held_id: &str) -> Result<Option<PathBuf>, String> {
    let id = random_uuid().ok_or_else(|| "no random id could be made".to_owned())?;
    let aside = chat.join(format!("{TEMP_PREFIX}{id}.tmp"));

    match std::fs::rename(item_path(chat, held_id), &aside) {
        Ok(()) => Ok(Some(aside)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("moving the held item failed: {error}")),
    }
}

/// Moves the file at `aside` back to `home`, unless another file took that name meanwhile.
fn put_back(aside: &Path, home: &Path) -> Result<(), String> {
    if std::fs::symlink_metadata(home).is_ok() {
        return Err("the held item could not be put back: a newer one has its id".to_owned());
    }

    std::fs::rename(aside, home).map_err(|error| format!("putting the held item back failed: {error}"))
}

/// The item `id` read from `path` in the folder `chat`, when the file is readable, of this
/// version, names that id and the folder's chat, and has the revision of its content.
fn parse(chat: &Path, path: &Path, id: &str) -> Option<HeldItem> {
    let bytes = read_private(path, HELD_FILE_MAX).ok()??;
    let item: HeldItem = serde_json::from_slice(&bytes).ok()?;
    let in_its_folder = chat.file_name().and_then(|name| name.to_str()) == Some(item.chat_key.as_str());

    (item.version == VERSION && item.held_id == id && in_its_folder && item.revision == item.digest()).then_some(item)
}

fn item_path(chat: &Path, id: &str) -> PathBuf {
    chat.join(format!("{id}.json"))
}

/// True when `path` is a folder (not a symbolic link to one).
fn is_folder(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// Removes the items and half-written files in `folder` older than 30 days.
fn sweep_folder(folder: &Path) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let ours = name.ends_with(".json") || (name.starts_with(TEMP_PREFIX) && name.ends_with(".tmp"));

        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age > KEEP_FOR);

        if ours && stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
pub(super) fn path_of(store: &HeldStore, chat: &str, id: &str) -> PathBuf {
    item_path(&store.folder.join(chat), id)
}
