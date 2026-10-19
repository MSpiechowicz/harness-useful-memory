//! Team documents: Harness Desktop files (version 1) a team shares through the team database, one
//! kind per table (a collection in MongoDB), named by the closed [`DocKind`] only (never by
//! input): team hooks ([`crate::store::hooks`]) and team routines ([`crate::store::routines`]).
//!
//! [`TeamDocStore`] keeps them in the team database only (see
//! [`crate::backend::Selection::docs_place`]): one row per document, unique by its `id` (the 32 hex
//! id of the desktop file it was shared from), with the repository key it applies to (`repo`, a
//! `git:` key, `None` for a global one), the file bytes as written (`body`, never masked), their
//! [`doc_sha256`], a free-text `author` and the times it was created and last written here. A
//! scope (the global one, and each repository) holds at most [`DocKind::per_scope`] documents of
//! a kind.
//!
//! Each kind checks its body with its own format ([`DocKind::check_team_body`]), with the same two
//! team rules: nothing the Harness family adds itself (`seed`), and a project document's `project`
//! holds the repository key instead of a local path ([`TeamBody::repo_in`]). So only the
//! repository identity leaves the machine.
//!
//! This server never runs a document: Harness Desktop reads them through `harness-memory hooks`
//! and `harness-memory routines` ([`crate::cli_team_docs`]) and decides on its own whether one
//! runs.
//!
//! Every store reads documents back through [`read_back`], so a row changed behind this server's
//! back never breaks a list and never reaches Harness Desktop: a row [`checked_doc`] would refuse
//! (a bad id, repository or author, a body that breaks the kind's rules or names another id or
//! scope) is left out (noted on stderr). Lists page by the rows read, left out ones included
//! ([`DocPage::next_offset`]).

pub(crate) mod fields;

use sha2::{Digest, Sha256};

use super::StoreError;
use crate::limits::{HOOKS_PER_SCOPE, ROUTINES_PER_SCOPE, TEAM_DOC_AUTHOR_MAX};
use crate::support::text::display_safe;

/// The longest repository key a document row holds or is read back with, in bytes (Harness
/// Desktop's limit for a team document's repository).
pub const DOC_REPO_MAX: usize = 512;

/// What a team document is. The table (or collection) of each kind comes from here only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocKind {
    /// Harness Desktop hooks (`docs/hooks-format.md` in the desktop repository).
    Hooks,
    /// Harness Desktop routines (`docs/routines-format.md` in the desktop repository).
    Routines,
}

impl DocKind {
    /// Every kind.
    pub const ALL: [DocKind; 2] = [Self::Hooks, Self::Routines];

    /// The table (a collection in MongoDB) holding this kind, also the setting, command and JSON
    /// key naming it: `hooks` or `routines`.
    pub fn table(self) -> &'static str {
        match self {
            Self::Hooks => "hooks",
            Self::Routines => "routines",
        }
    }

    /// One document of this kind, in messages: `hook` or `routine`.
    pub fn noun(self) -> &'static str {
        match self {
            Self::Hooks => "hook",
            Self::Routines => "routine",
        }
    }

    /// The most documents of this kind one scope holds.
    pub fn per_scope(self) -> usize {
        match self {
            Self::Hooks => HOOKS_PER_SCOPE,
            Self::Routines => ROUTINES_PER_SCOPE,
        }
    }

    /// The document in `body` when it is a valid file of this kind that the team rules let
    /// through (its `project` aside, see [`TeamBody::repo_in`]).
    pub fn check_team_body(self, body: &str) -> Result<TeamBody, BodyProblem> {
        match self {
            Self::Hooks => super::hooks::format::check_team_body(body),
            Self::Routines => super::routines::format::check_team_body(body),
        }
    }
}

/// Why a body cannot be stored. Each variant holds the message for a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyProblem {
    /// It breaks the kind's file format (or holds a credential).
    Invalid(String),
    /// It is a valid file the team rules keep out: one the Harness family adds itself, or a
    /// project document that does not name the repository it is shared for by its `git:` key.
    Refused(String),
}

impl BodyProblem {
    /// `Invalid` with the reason a `kind` file breaks its format.
    pub(crate) fn invalid(kind: DocKind, reason: &str) -> Self {
        Self::Invalid(format!("the {} file is invalid: {reason}", kind.noun()))
    }

    /// The reason, for a person.
    pub fn message(&self) -> String {
        match self {
            Self::Invalid(message) | Self::Refused(message) => message.clone(),
        }
    }
}

/// What the team rules need of a body a kind's check accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamBody {
    pub id: String,
    /// The `project` of a project document (`None`: a global one).
    pub project: Option<String>,
}

impl TeamBody {
    /// The repository a `kind` document is stored for when it is shared from the repository
    /// `identity` (its key, `None` without one): `None` for a global document (whatever
    /// `identity` is), the key for a project document whose `project` is that `git:` key.
    /// Refused otherwise.
    pub fn repo_in(&self, kind: DocKind, identity: Option<&str>) -> Result<Option<String>, BodyProblem> {
        let Some(project) = &self.project else {
            return Ok(None);
        };

        let (noun, plural) = (kind.noun(), kind.table());
        let refused = |reason: String| Err(BodyProblem::Refused(reason));

        let Some(identity) = identity else {
            return refused(format!(
                "a project {noun} is shared for a repository: give its folder with --project"
            ));
        };

        if identity.starts_with("path:") || project.starts_with("path:") {
            return refused(format!(
                "the repository has no origin remote, so its {plural} are not shared (only a git: repository key \
                 leaves this computer)"
            ));
        }

        if !project.starts_with("git:") {
            return refused(format!(
                "a team {noun}'s project is the repository key (git:host/owner/repo), not a local path"
            ));
        }

        if project != identity {
            return refused(format!("the {noun}'s project is not the repository of --project"));
        }

        Ok(Some(project.clone()))
    }
}

/// A document to store: the checked body and where it applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocDraft {
    /// 32 lowercase hex characters, the body's `id`.
    pub id: String,
    /// The `git:` repository key of a project document, `None` for a global one.
    pub repo: Option<String>,
    pub body: String,
    /// Who shared it (a free-text label, see [`check_author`]).
    pub author: String,
}

/// A stored team document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamDoc {
    pub id: String,
    pub repo: Option<String>,
    /// [`doc_sha256`] of the body read back (never a stored hash).
    pub sha256: String,
    pub author: String,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds.
    pub updated_at: i64,
    pub body: String,
}

/// One page of [`TeamDocStore::list_docs`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocPage {
    /// The rows of the listed scopes in the database (a row [`read_back`] leaves out counts too).
    pub total: usize,
    pub docs: Vec<TeamDoc>,
    /// The offset of the next page: after the last row read for this one (left out rows
    /// included), `None` when no rows follow.
    pub next_offset: Option<usize>,
}

impl DocPage {
    /// The page of `docs` read back from `scanned` rows at `offset`, of `total`.
    pub(crate) fn of(total: usize, offset: usize, scanned: usize, docs: Vec<TeamDoc>) -> Self {
        let next = offset.saturating_add(scanned);

        Self {
            total,
            docs,
            next_offset: (scanned > 0 && next < total).then_some(next),
        }
    }
}

/// What [`TeamDocStore::put_doc`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocPut {
    /// Inserted or updated, as read back.
    Saved(TeamDoc),
    /// An insert of an id the database already holds; nothing changed.
    Exists,
    /// An update of an id the database does not hold; nothing changed.
    Missing,
    /// An update of a document whose bytes are no longer the expected ones; nothing changed.
    Changed,
    /// The document's scope already holds [`DocKind::per_scope`] documents; nothing changed.
    Full,
}

/// What [`TeamDocStore::delete_doc`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocDelete {
    Deleted,
    /// No document of the kind has the id.
    Missing,
    /// The document's bytes are no longer the expected ones; nothing was deleted.
    Changed,
}

/// A team document backend. Like [`super::MemoryStore`], one store serves one process. Every
/// method works on the table of `kind` only.
pub trait TeamDocStore: Send {
    /// One page (`offset`, at most `limit` rows) of the global documents and those of the
    /// repository key `repo`: global first, then by `created_at`, then by id.
    fn list_docs(
        &mut self,
        kind: DocKind,
        repo: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<DocPage, StoreError>;

    /// The document `id`, whatever its scope.
    fn get_doc(&mut self, kind: DocKind, id: &str) -> Result<Option<TeamDoc>, StoreError>;

    /// Inserts `draft` when `expect` is `None` ([`DocPut::Exists`] when the id is taken), else
    /// replaces the document with its id while its [`doc_sha256`] is `expect` (its body,
    /// repository, author and `updated_at` change). A document that enters a scope holding
    /// [`DocKind::per_scope`] documents is [`DocPut::Full`]. The draft is checked with
    /// [`checked_doc`].
    fn put_doc(&mut self, kind: DocKind, draft: &DocDraft, expect: Option<&str>) -> Result<DocPut, StoreError>;

    /// Deletes the document `id`, only while its [`doc_sha256`] is `expect` when that is given.
    fn delete_doc(&mut self, kind: DocKind, id: &str, expect: Option<&str>) -> Result<DocDelete, StoreError>;

    /// Deletes every document whose stored author is `author` (compared as stored, after
    /// [`check_author`]), whatever its scope, in one write. Returns the ids deleted (each cut a
    /// little over a document id, so a row written by hand cannot make a reply of any size), in
    /// id order.
    fn delete_docs_by(&mut self, kind: DocKind, author: &str) -> Result<Vec<String>, StoreError>;
}

/// The lowercase hex SHA-256 of a body's bytes: the hash Harness Desktop approves a file by.
pub fn doc_sha256(body: &str) -> String {
    Sha256::digest(body.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// True for a document id: 32 lowercase hex characters.
pub fn is_doc_id(text: &str) -> bool {
    text.len() == 32 && is_lower_hex(text)
}

/// True for an expected hash: 64 lowercase hex characters.
pub fn is_sha256(text: &str) -> bool {
    text.len() == 64 && is_lower_hex(text)
}

fn is_lower_hex(text: &str) -> bool {
    text.bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// `author` trimmed; fails when it is empty, longer than [`TEAM_DOC_AUTHOR_MAX`] characters, or
/// holds a control or invisible character.
pub fn check_author(author: &str) -> Result<String, StoreError> {
    let author = author.trim();

    if author.is_empty() {
        return Err(StoreError::Invalid("the author is empty".to_owned()));
    }

    if author.chars().count() > TEAM_DOC_AUTHOR_MAX {
        return Err(StoreError::Invalid(format!(
            "the author is longer than {TEAM_DOC_AUTHOR_MAX} characters"
        )));
    }

    if !display_safe(author) {
        return Err(StoreError::Invalid(
            "the author holds a control or invisible character".to_owned(),
        ));
    }

    Ok(author.to_owned())
}

/// `draft` ready to store as a `kind` document, its author trimmed. Fails on an id that is not a
/// document id, a repository that is not a `git:` key, a bad author, and a body
/// [`DocKind::check_team_body`] refuses or whose id or scope differ from the draft's.
pub fn checked_doc(kind: DocKind, draft: &DocDraft) -> Result<DocDraft, StoreError> {
    let noun = kind.noun();

    if !is_doc_id(&draft.id) {
        return Err(StoreError::Invalid(format!(
            "a {noun} id is 32 lowercase hex characters"
        )));
    }

    if let Some(repo) = &draft.repo
        && !is_repo_key(kind, repo)
    {
        return Err(StoreError::Invalid(format!(
            "a team {noun}'s repository is a git: repository key"
        )));
    }

    let author = check_author(&draft.author)?;
    let body = kind
        .check_team_body(&draft.body)
        .map_err(|problem| StoreError::Invalid(problem.message()))?;

    if body.id != draft.id {
        return Err(StoreError::Invalid(format!("the body's id is not the {noun}'s id")));
    }

    let repo = body
        .repo_in(kind, draft.repo.as_deref())
        .map_err(|problem| StoreError::Invalid(problem.message()))?;

    if repo != draft.repo {
        return Err(StoreError::Invalid(format!(
            "a global {noun} is stored without a repository"
        )));
    }

    Ok(DocDraft {
        author,
        ..draft.clone()
    })
}

/// True for a repository key a `kind` row may hold: a `git:` key of at most [`DOC_REPO_MAX`]
/// bytes without control or invisible characters (and, for routines, without whitespace, as the
/// routine format says).
fn is_repo_key(kind: DocKind, repo: &str) -> bool {
    let spaced = kind == DocKind::Routines && repo.chars().any(char::is_whitespace);

    repo.starts_with("git:") && repo.len() <= DOC_REPO_MAX && display_safe(repo) && !spaced
}

/// A document row as a store read it, before [`read_back`] checks it. Text fields are read cut a
/// little over their limit (where the database can cut), so a value over the limit is still
/// recognized as one.
#[derive(Debug, Clone)]
pub(crate) struct StoredDoc {
    pub id: String,
    pub repo: Option<String>,
    pub body: String,
    pub author: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// `stored` as a team document of `kind`, with the [`doc_sha256`] of the body read and the
/// author trimmed. `None`, noted on stderr, for a row this server never writes: one
/// [`checked_doc`] refuses.
pub(crate) fn read_back(kind: DocKind, stored: StoredDoc) -> Option<TeamDoc> {
    let row = DocDraft {
        id: stored.id,
        repo: stored.repo,
        body: stored.body,
        author: stored.author,
    };

    let checked = match checked_doc(kind, &row) {
        Ok(checked) => checked,
        Err(error) => {
            let shown: String = row.id.chars().filter(char::is_ascii_alphanumeric).take(32).collect();
            let reason: String = error
                .to_string()
                .chars()
                .filter(|c| !c.is_control())
                .take(200)
                .collect();
            eprintln!("harness-memory: left out team {} {shown}: {reason}", kind.noun());
            return None;
        }
    };

    Some(TeamDoc {
        sha256: doc_sha256(&checked.body),
        id: checked.id,
        repo: checked.repo,
        author: checked.author,
        created_at: stored.created_at,
        updated_at: stored.updated_at,
        body: checked.body,
    })
}

/// Notes on stderr a `kind` row a store could not read (a column of an unexpected type).
pub(crate) fn unreadable(kind: DocKind, error: &StoreError) {
    eprintln!("harness-memory: left out a team {}: {error}", kind.noun());
}

/// True when a scope holding `count` documents of `kind` has no room for one more.
pub(crate) fn is_full(kind: DocKind, count: i64) -> bool {
    usize::try_from(count).is_ok_and(|count| count >= kind.per_scope())
}

/// The longest id a store reads back for [`TeamDocStore::delete_docs_by`]: one character over a
/// document id, so a row written by hand cannot make a reply of any size.
pub(crate) const LISTED_ID_MAX: usize = 33;

/// The error of a write whose document could not be read back.
pub(crate) fn not_read_back(kind: DocKind) -> StoreError {
    StoreError::Backend(format!("the saved {} could not be read back", kind.noun()))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::limits::TEAM_DOC_BODY_MAX;

    #[test]
    fn the_hash_is_the_sha256_of_the_bytes() {
        // printf 'abc' | sha256sum
        assert_eq!(
            doc_sha256("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn authors_are_trimmed_labels() {
        assert_eq!(check_author("  Ada Lovelace ").expect("valid"), "Ada Lovelace");
        assert_eq!(
            check_author(&"a".repeat(TEAM_DOC_AUTHOR_MAX))
                .expect("at the limit")
                .len(),
            80
        );

        for bad in ["", "  ", "a\u{202E}b", "a\nb", &"a".repeat(TEAM_DOC_AUTHOR_MAX + 1)] {
            assert!(check_author(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ids_and_hashes_are_lowercase_hex() {
        assert!(is_doc_id("1a2b3c4d5e6f70818293a4b5c6d7000b"));
        assert!(!is_doc_id("1A2B3C4D5E6F70818293A4B5C6D7000B"));
        assert!(!is_doc_id("1a2b"));
        assert!(is_sha256(&"a".repeat(64)));
        assert!(!is_sha256(&"g".repeat(64)));
        assert!(!is_sha256(&"A".repeat(64)));
    }

    #[test]
    fn every_kind_has_its_own_table() {
        let tables: Vec<&str> = DocKind::ALL.iter().map(|kind| kind.table()).collect();
        assert_eq!(tables, ["hooks", "routines"]);
        assert_eq!(DocKind::Routines.per_scope(), 64);
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

    fn stored() -> StoredDoc {
        StoredDoc {
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
        let kept = read_back(DocKind::Hooks, stored()).expect("kept");
        assert_eq!(kept.sha256, doc_sha256(&body(Some(WIDGET))));
        assert_eq!(kept.author, "ada");

        assert_eq!(
            read_back(DocKind::Routines, stored()),
            None,
            "a hook body is no routine"
        );

        let changed = |change: fn(&mut StoredDoc)| {
            let mut row = stored();
            change(&mut row);
            read_back(DocKind::Hooks, row)
        };
        assert_eq!(changed(|row| row.id = "x".to_owned()), None);
        assert_eq!(
            changed(|row| row.id = "0".repeat(32)),
            None,
            "another id than the body's"
        );
        assert_eq!(changed(|row| row.body = "a".repeat(TEAM_DOC_BODY_MAX + 1)), None);
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
        assert_eq!(changed(|row| row.author = "a".repeat(TEAM_DOC_AUTHOR_MAX + 1)), None);
        assert_eq!(changed(|row| row.author = "ada\u{202E}".to_owned()), None);

        let local = |row: &mut StoredDoc| {
            let key = format!("path:/home/ada/{}", "w".repeat(10));
            row.repo = Some(key.clone());
            row.body = body(Some(&key));
        };
        assert_eq!(changed(local), None, "a path: key");

        let long = |row: &mut StoredDoc| {
            let key = format!("git:github.com/acme/{}", "w".repeat(DOC_REPO_MAX));
            row.repo = Some(key.clone());
            row.body = body(Some(&key));
        };
        assert_eq!(changed(long), None, "a repository key over 512 bytes");
    }

    #[test]
    fn a_page_says_where_the_next_one_starts() {
        assert_eq!(DocPage::of(5, 0, 2, Vec::new()).next_offset, Some(2));
        assert_eq!(DocPage::of(5, 3, 2, Vec::new()).next_offset, None);
        assert_eq!(DocPage::of(5, 9, 0, Vec::new()).next_offset, None);
        assert_eq!(DocPage::of(0, 0, 0, Vec::new()).next_offset, None);
    }

    #[test]
    fn refusals_name_the_kind() {
        let project = TeamBody {
            id: ID.to_owned(),
            project: Some(WIDGET.to_owned()),
        };

        let message = |kind| match project.repo_in(kind, None) {
            Err(problem) => problem.message(),
            Ok(repo) => panic!("shared as {repo:?}"),
        };
        assert_eq!(
            message(DocKind::Hooks),
            "a project hook is shared for a repository: give its folder with --project"
        );
        assert_eq!(
            message(DocKind::Routines),
            "a project routine is shared for a repository: give its folder with --project"
        );
    }
}
