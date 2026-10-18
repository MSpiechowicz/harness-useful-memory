//! The team hook contract: Harness Desktop hook files (version 1) a team shares through the team
//! database.
//!
//! [`HookStore`] keeps them in the team database only (see [`crate::backend::Selection::hooks_place`]):
//! one row per hook, unique by its `id` (the 32 hex id of the desktop file it was shared from),
//! with the repository key it applies to (`repo`, a `git:` key, `None` for a global hook), the
//! file bytes as written (`body`, never masked), their [`hook_sha256`], a free-text `author` and
//! the times it was created and last written here. A scope (the global one, and each repository)
//! holds at most [`HOOKS_PER_SCOPE`] hooks.
//!
//! The body follows the desktop's file format with two team rules, checked by [`format`]: no
//! `seed` (Harness family hooks are never shared), and a project hook's `project` holds the
//! repository key instead of a local path. So only the repository identity leaves the machine.
//!
//! This server never runs a hook: Harness Desktop reads them through `harness-memory hooks`
//! ([`crate::cli_hooks`]) and decides on its own whether one runs.
//!
//! Every store reads hooks back through [`read_back`], so a row changed behind this server's back
//! never breaks a list and never reaches Harness Desktop: a row [`checked_hook`] would refuse (a
//! bad id, repository or author, a body that breaks the team body rules or names another id or
//! scope) is left out (noted on stderr). Lists page by the rows read, left out ones included
//! ([`HookPage::next_offset`]).

pub mod format;

use sha2::{Digest, Sha256};

use super::StoreError;
use crate::limits::{HOOK_AUTHOR_MAX, HOOKS_PER_SCOPE};
use crate::support::text::display_safe;

/// The longest repository key a hook row holds or is read back with, in bytes (Harness Desktop's
/// limit for a team hook's repository).
pub const HOOK_REPO_MAX: usize = 512;

/// A hook to store: the checked body and where it applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDraft {
    /// 32 lowercase hex characters, the body's `id`.
    pub id: String,
    /// The `git:` repository key of a project hook, `None` for a global one.
    pub repo: Option<String>,
    pub body: String,
    /// Who shared it (a free-text label, see [`check_author`]).
    pub author: String,
}

/// A stored team hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamHook {
    pub id: String,
    pub repo: Option<String>,
    /// [`hook_sha256`] of the body read back (never a stored hash).
    pub sha256: String,
    pub author: String,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds.
    pub updated_at: i64,
    pub body: String,
}

/// One page of [`HookStore::list_hooks`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookPage {
    /// The rows of the listed scopes in the database (a row [`read_back`] leaves out counts too).
    pub total: usize,
    pub hooks: Vec<TeamHook>,
    /// The offset of the next page: after the last row read for this one (left out rows
    /// included), `None` when no rows follow.
    pub next_offset: Option<usize>,
}

impl HookPage {
    /// The page of `hooks` read back from `scanned` rows at `offset`, of `total`.
    pub(crate) fn of(total: usize, offset: usize, scanned: usize, hooks: Vec<TeamHook>) -> Self {
        let next = offset.saturating_add(scanned);

        Self {
            total,
            hooks,
            next_offset: (scanned > 0 && next < total).then_some(next),
        }
    }
}

/// What [`HookStore::put_hook`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookPut {
    /// Inserted or updated, as read back.
    Saved(TeamHook),
    /// An insert of an id the database already holds; nothing changed.
    Exists,
    /// An update of an id the database does not hold; nothing changed.
    Missing,
    /// An update of a hook whose bytes are no longer the expected ones; nothing changed.
    Changed,
    /// The hook's scope already holds [`HOOKS_PER_SCOPE`] hooks; nothing changed.
    Full,
}

/// What [`HookStore::delete_hook`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookDelete {
    Deleted,
    /// No hook has the id.
    Missing,
    /// The hook's bytes are no longer the expected ones; nothing was deleted.
    Changed,
}

/// A team hook backend. Like [`super::MemoryStore`], one store serves one process.
pub trait HookStore: Send {
    /// One page (`offset`, at most `limit` rows) of the global hooks and those of the repository
    /// key `repo`: global first, then by `created_at`, then by id.
    fn list_hooks(&mut self, repo: Option<&str>, offset: usize, limit: usize) -> Result<HookPage, StoreError>;

    /// The hook `id`, whatever its scope.
    fn get_hook(&mut self, id: &str) -> Result<Option<TeamHook>, StoreError>;

    /// Inserts `draft` when `expect` is `None` ([`HookPut::Exists`] when the id is taken), else
    /// replaces the hook with its id while its [`hook_sha256`] is `expect` (its body, repository,
    /// author and `updated_at` change). A hook that enters a scope holding [`HOOKS_PER_SCOPE`]
    /// hooks is [`HookPut::Full`]. The draft is checked with [`checked_hook`].
    fn put_hook(&mut self, draft: &HookDraft, expect: Option<&str>) -> Result<HookPut, StoreError>;

    /// Deletes the hook `id`, only while its [`hook_sha256`] is `expect` when that is given.
    fn delete_hook(&mut self, id: &str, expect: Option<&str>) -> Result<HookDelete, StoreError>;
}

/// The lowercase hex SHA-256 of a hook body's bytes: the hash Harness Desktop approves a hook
/// file by.
pub fn hook_sha256(body: &str) -> String {
    Sha256::digest(body.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// True for a hook id: 32 lowercase hex characters.
pub fn is_hook_id(text: &str) -> bool {
    text.len() == 32
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// True for an expected hash: 64 lowercase hex characters.
pub fn is_sha256(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// `author` trimmed; fails when it is empty, longer than [`HOOK_AUTHOR_MAX`] characters, or holds
/// a control or invisible character.
pub fn check_author(author: &str) -> Result<String, StoreError> {
    let author = author.trim();

    if author.is_empty() {
        return Err(StoreError::Invalid("the author is empty".to_owned()));
    }

    if author.chars().count() > HOOK_AUTHOR_MAX {
        return Err(StoreError::Invalid(format!(
            "the author is longer than {HOOK_AUTHOR_MAX} characters"
        )));
    }

    if !display_safe(author) {
        return Err(StoreError::Invalid(
            "the author holds a control or invisible character".to_owned(),
        ));
    }

    Ok(author.to_owned())
}

/// `draft` ready to store, its author trimmed. Fails on an id that is not a hook id, a repository
/// that is not a `git:` key, a bad author, and a body [`format::check_team_body`] refuses or
/// whose id or scope differ from the draft's.
pub fn checked_hook(draft: &HookDraft) -> Result<HookDraft, StoreError> {
    if !is_hook_id(&draft.id) {
        return Err(StoreError::Invalid(
            "a hook id is 32 lowercase hex characters".to_owned(),
        ));
    }

    if let Some(repo) = &draft.repo
        && !(repo.starts_with("git:") && repo.len() <= HOOK_REPO_MAX && display_safe(repo))
    {
        return Err(StoreError::Invalid(
            "a team hook's repository is a git: repository key".to_owned(),
        ));
    }

    let author = check_author(&draft.author)?;
    let body = format::check_team_body(&draft.body).map_err(|problem| StoreError::Invalid(problem.message()))?;

    if body.id != draft.id {
        return Err(StoreError::Invalid("the body's id is not the hook's id".to_owned()));
    }

    let repo = body
        .repo_for(draft.repo.as_deref())
        .map_err(|problem| StoreError::Invalid(problem.message()))?;

    if repo != draft.repo {
        return Err(StoreError::Invalid(
            "a global hook is stored without a repository".to_owned(),
        ));
    }

    Ok(HookDraft {
        author,
        ..draft.clone()
    })
}

/// A hook row as a store read it, before [`read_back`] checks it. Text fields are read cut a
/// little over their limit (where the database can cut), so a value over the limit is still
/// recognized as one.
#[derive(Debug, Clone)]
pub(crate) struct StoredHook {
    pub id: String,
    pub repo: Option<String>,
    pub body: String,
    pub author: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// `stored` as a team hook, with the [`hook_sha256`] of the body read and the author trimmed.
/// `None`, noted on stderr, for a row this server never writes: one [`checked_hook`] refuses.
pub(crate) fn read_back(stored: StoredHook) -> Option<TeamHook> {
    let row = HookDraft {
        id: stored.id,
        repo: stored.repo,
        body: stored.body,
        author: stored.author,
    };

    let checked = match checked_hook(&row) {
        Ok(checked) => checked,
        Err(error) => {
            let shown: String = row.id.chars().filter(char::is_ascii_alphanumeric).take(32).collect();
            let reason: String = error
                .to_string()
                .chars()
                .filter(|c| !c.is_control())
                .take(200)
                .collect();
            eprintln!("harness-memory: left out team hook {shown}: {reason}");
            return None;
        }
    };

    Some(TeamHook {
        sha256: hook_sha256(&checked.body),
        id: checked.id,
        repo: checked.repo,
        author: checked.author,
        created_at: stored.created_at,
        updated_at: stored.updated_at,
        body: checked.body,
    })
}

/// True when a scope holding `count` hooks has no room for one more.
pub(crate) fn is_full(count: i64) -> bool {
    usize::try_from(count).is_ok_and(|count| count >= HOOKS_PER_SCOPE)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::limits::HOOK_BODY_MAX;

    #[test]
    fn the_hash_is_the_sha256_of_the_bytes() {
        // printf 'abc' | sha256sum
        assert_eq!(
            hook_sha256("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn authors_are_trimmed_labels() {
        assert_eq!(check_author("  Ada Lovelace ").expect("valid"), "Ada Lovelace");
        assert_eq!(
            check_author(&"a".repeat(HOOK_AUTHOR_MAX)).expect("at the limit").len(),
            80
        );

        for bad in ["", "  ", "a\u{202E}b", "a\nb", &"a".repeat(HOOK_AUTHOR_MAX + 1)] {
            assert!(check_author(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ids_and_hashes_are_lowercase_hex() {
        assert!(is_hook_id("1a2b3c4d5e6f70818293a4b5c6d7000b"));
        assert!(!is_hook_id("1A2B3C4D5E6F70818293A4B5C6D7000B"));
        assert!(!is_hook_id("1a2b"));
        assert!(is_sha256(&"a".repeat(64)));
        assert!(!is_sha256(&"g".repeat(64)));
        assert!(!is_sha256(&"A".repeat(64)));
    }

    const ID: &str = "1a2b3c4d5e6f70818293a4b5c6d7000b";
    const WIDGET: &str = "git:github.com/acme/widget";

    /// A valid team hook file: global without `project`, else a project hook of it.
    fn body(project: Option<&str>) -> String {
        let (scope, project) = match project {
            Some(key) => ("project", format!("\"{key}\"")),
            None => ("global", "null".to_owned()),
        };

        format!(
            "{{\"version\": 1, \"id\": \"{ID}\", \"name\": \"Rules\", \"scope\": \"{scope}\", \"project\": \
             {project}, \"event\": \"sessionStart\", \"action\": {{\"type\": \"addContext\", \"text\": \"Hi\"}}, \
             \"enabled\": true, \"createdAt\": \"2026-10-17T10:00:00Z\", \"createdBy\": \"desktop\", \"updatedAt\": \
             \"2026-10-17T10:00:00Z\", \"updatedBy\": \"desktop\"}}\n"
        )
    }

    fn stored() -> StoredHook {
        StoredHook {
            id: ID.to_owned(),
            repo: Some(WIDGET.to_owned()),
            body: body(Some(WIDGET)),
            author: " ada ".to_owned(),
            created_at: 1,
            updated_at: 2,
        }
    }

    #[test]
    fn rows_read_back_get_their_hash_or_are_left_out() {
        let kept = read_back(stored()).expect("kept");
        assert_eq!(kept.sha256, hook_sha256(&body(Some(WIDGET))));
        assert_eq!(kept.author, "ada");

        let changed = |change: fn(&mut StoredHook)| {
            let mut row = stored();
            change(&mut row);
            read_back(row)
        };
        assert_eq!(changed(|row| row.id = "x".to_owned()), None);
        assert_eq!(
            changed(|row| row.id = "0".repeat(32)),
            None,
            "another id than the body's"
        );
        assert_eq!(changed(|row| row.body = "a".repeat(HOOK_BODY_MAX + 1)), None);
        assert_eq!(changed(|row| row.body = "{}".to_owned()), None, "not a hook file");
        assert_eq!(
            changed(|row| row.body = body(None)),
            None,
            "a global body in a repository row"
        );
        assert_eq!(changed(|row| row.repo = None), None, "a project body in a global row");
        assert_eq!(
            changed(|row| row.repo = Some("git:github.com/acme/gadget".to_owned())),
            None
        );
        assert_eq!(changed(|row| row.author = "a".repeat(HOOK_AUTHOR_MAX + 1)), None);
        assert_eq!(changed(|row| row.author = "ada\u{202E}".to_owned()), None);

        let local = |row: &mut StoredHook| {
            let key = format!("path:/home/ada/{}", "w".repeat(10));
            row.repo = Some(key.clone());
            row.body = body(Some(&key));
        };
        assert_eq!(changed(local), None, "a path: key");

        let long = |row: &mut StoredHook| {
            let key = format!("git:github.com/acme/{}", "w".repeat(HOOK_REPO_MAX));
            row.repo = Some(key.clone());
            row.body = body(Some(&key));
        };
        assert_eq!(changed(long), None, "a repository key over 512 bytes");
    }

    #[test]
    fn a_page_says_where_the_next_one_starts() {
        assert_eq!(HookPage::of(5, 0, 2, Vec::new()).next_offset, Some(2));
        assert_eq!(HookPage::of(5, 3, 2, Vec::new()).next_offset, None);
        assert_eq!(HookPage::of(5, 9, 0, Vec::new()).next_offset, None);
        assert_eq!(HookPage::of(0, 0, 0, Vec::new()).next_offset, None);
    }
}
