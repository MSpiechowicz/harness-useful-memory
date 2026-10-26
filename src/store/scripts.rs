//! The script contract: deterministic scripts agents save and run, and the machine's trust in
//! their versions.
//!
//! [`ScriptStore`] keeps scripts next to the notes (the local SQLite file or the team database,
//! see [`crate::backend::Routing::scripts`]). [`TrustStore`] records which versions this machine
//! may run: a script (where it is kept and its id there) with the [`script_sha256`] of its
//! language and body, trusted as the script it was then (its name, scope and repository, see
//! [`TrustRecord::is_for`]). It always lives in the local SQLite file, so a script written
//! elsewhere runs only after it was reviewed here, and trusting one script never trusts another
//! with the same body. A trusted script renamed or moved to another scope needs approval again;
//! its description and timeout may change. It also remembers the versions reported here as
//! waiting for approval ([`TrustStore::note_unapproved`]), so saving their code again never
//! trusts it. Bodies are stored as written (never masked): a body that [`has_credentials`], holds
//! a [`hidden_character`] or (Python) declares a [`foreign_encoding`] is refused instead.
//!
//! Every store reads scripts back through [`read_back`], so a row changed behind this server's
//! back (a team database is shared) never breaks a list: its timeout is clamped, and a row with a
//! language this server cannot run or a field over its limit is left out.

use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use super::{Provenance, Scope, StoreError, Target, View};
use crate::backend::Place;
use crate::limits::{SCRIPT_BODY_MAX, SCRIPT_DESCRIPTION_MAX, SCRIPT_NAME_MAX, SCRIPT_TIMEOUT_MAX};
use crate::support::redact::{has_credentials, redact_text};
use crate::support::text::is_format_character;

/// What [`script_sha256`] hashes before the language and the body; a new encoding gets a new tag.
const SHA_TAG: &str = "harness-memory-script-v1";

/// The interpreter a script runs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Sh,
    Bash,
    Python,
    Node,
    Bun,
}

impl Language {
    /// Every language, in the order the tool schemas list them.
    pub const ALL: [Language; 5] = [Self::Sh, Self::Bash, Self::Python, Self::Node, Self::Bun];

    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sh => "sh",
            Self::Bash => "bash",
            Self::Python => "python",
            Self::Node => "node",
            Self::Bun => "bun",
        }
    }

    /// Parses a stored or agent-supplied name (any case, surrounding blanks ignored).
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim().to_ascii_lowercase();

        Self::ALL.into_iter().find(|language| language.as_str() == raw)
    }
}

/// A script to save, or to replace when a script with the same name exists in the scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptDraft {
    pub name: String,
    pub description: String,
    pub language: Language,
    pub body: String,
    /// Seconds a run may take (1 to [`SCRIPT_TIMEOUT_MAX`]).
    pub timeout_secs: u64,
    pub provenance: Provenance,
}

/// A stored script without its body, as lists show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptSummary {
    pub id: i64,
    pub scope: Scope,
    /// The repository key, for repository scripts.
    pub project: Option<String>,
    pub name: String,
    pub description: String,
    pub language: Language,
    /// [`script_sha256`] of the language and body read back (never a stored hash).
    pub sha256: String,
    pub timeout_secs: u64,
    /// The latest writer.
    pub provenance: Provenance,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds.
    pub updated_at: i64,
    pub run_count: i64,
    /// Unix seconds.
    pub last_run_at: Option<i64>,
    /// `None` when the last run was killed or ended by a signal.
    pub last_exit_code: Option<i32>,
}

/// A stored script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    pub summary: ScriptSummary,
    pub body: String,
}

/// The result of [`ScriptStore::save_script`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedScript {
    pub script: Script,
    /// True when the script was created, false when one with the same name was replaced.
    pub created: bool,
}

/// How a call names a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptRef {
    Id(i64),
    /// Looked up in the repository first, then in the global scope.
    Name(String),
}

/// Where a script a store lists is kept, and its id there (see [`ScriptStore::origin`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptOrigin {
    /// The store keeps every script in one place, under the id it shows.
    Single,
    /// In the team database as script `raw`; `read_only` when scripts are `remote+local`.
    Team { raw: i64, read_only: bool },
    /// In the local SQLite file of this computer as script `raw`.
    Local { raw: i64 },
}

impl ScriptOrigin {
    /// The place the script is kept and its id there, `single` being where a [`Self::Single`]
    /// store keeps its scripts (shown id `id`).
    pub fn spot(self, id: i64, single: Place) -> (Place, i64) {
        match self {
            Self::Single => (single, id),
            Self::Team { raw, .. } => (Place::Remote, raw),
            Self::Local { raw } => (Place::Local, raw),
        }
    }

    /// True for a team script that cannot be changed from here.
    pub fn is_read_only(self) -> bool {
        matches!(self, Self::Team { read_only: true, .. })
    }
}

/// A script backend. Like [`super::MemoryStore`], one store serves one process.
pub trait ScriptStore: Send {
    /// Saves `draft` in the scope `target`, replacing the script with the same name there (its
    /// run statistics are kept, the body, description, language, timeout, writer and
    /// `updated_at` change). The draft is checked with [`checked_draft`].
    fn save_script(&mut self, target: &Target, draft: &ScriptDraft) -> Result<SavedScript, StoreError>;

    /// The scripts visible in `view`, repository scripts first, then by name. A `query` keeps
    /// the scripts whose name or description contains it (ignoring case).
    fn list_scripts(&mut self, view: &View, query: Option<&str>) -> Result<Vec<ScriptSummary>, StoreError>;

    /// The script `script` names, if it is visible in `view`.
    fn get_script(&mut self, view: &View, script: &ScriptRef) -> Result<Option<Script>, StoreError>;

    /// Deletes the script `script` names, if it is visible in `view`; true when one was deleted.
    fn delete_script(&mut self, view: &View, script: &ScriptRef) -> Result<bool, StoreError>;

    /// Deletes the scripts among `ids` that are visible in `view`, in one write where the
    /// backend allows it; how many were deleted. The default deletes them one by one.
    fn delete_scripts(&mut self, view: &View, ids: &[i64]) -> Result<usize, StoreError> {
        let mut deleted = 0;

        for id in ids {
            if self.delete_script(view, &ScriptRef::Id(*id))? {
                deleted += 1;
            }
        }

        Ok(deleted)
    }

    /// Counts a run of script `id` that ended at `at` (Unix seconds) with `exit_code`.
    fn record_run(&mut self, id: i64, exit_code: Option<i32>, at: i64) -> Result<(), StoreError>;

    /// The team scripts visible in `view` that this computer hides from every read, under the ids
    /// they are shown as: for checks that must see them too, never to show them. An error when the
    /// hidden list cannot be read; none for a store without a hidden list (the default).
    fn hidden_scripts(&mut self, _view: &View) -> Result<Vec<ScriptSummary>, StoreError> {
        Ok(Vec::new())
    }

    /// Where the script this store shows as `id` is kept: [`ScriptOrigin::Single`] for a store of
    /// one place (the default), the team database or this computer for a
    /// [`super::SplitScripts`].
    fn origin(&self, _id: i64) -> ScriptOrigin {
        ScriptOrigin::Single
    }

    /// Whether the store can be used now: `false` for a [`super::Lazy`] store whose database
    /// cannot be opened (a failed open is not tried again for a while). The default is `true`.
    fn ready(&mut self) -> bool {
        true
    }

    /// Whether scripts meant for the team database are served from this computer right now (the
    /// offline fallback of a [`super::SplitScripts`]). The default is `false`.
    fn offline(&self) -> bool {
        false
    }
}

/// How this machine came to trust a script version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustOrigin {
    /// Saved through `script_save` on this machine.
    Saved,
    /// Approved by a person here (Harness Desktop or `harness-memory trust`).
    Approved,
}

impl TrustOrigin {
    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Saved => "saved",
            Self::Approved => "approved",
        }
    }

    /// Parses a stored name; anything else counts as approved.
    pub fn parse(raw: &str) -> Self {
        if raw == "saved" { Self::Saved } else { Self::Approved }
    }
}

/// A script version to trust: the script it belongs to (where it is kept, its id there, and the
/// name, scope and repository it is trusted as) and the language and body that make up its
/// [`script_sha256`].
#[derive(Debug, Clone, Copy)]
pub struct TrustedVersion<'a> {
    /// Where the script is kept: `local`, or `team:<hash>` naming the team database (see
    /// [`crate::backend::Selection::scripts_trust_key`]).
    pub place: &'a str,
    /// The script's id where it is kept (never an id a [`super::SplitScripts`] shows).
    pub script_id: i64,
    pub name: &'a str,
    pub scope: Scope,
    /// The repository key, for a repository script.
    pub project: Option<&'a str>,
    pub language: Language,
    pub body: &'a str,
    pub origin: TrustOrigin,
}

impl<'a> TrustedVersion<'a> {
    /// The version of `script` (as read from the store) kept in `place` as script `raw_id`,
    /// trusted as `origin`.
    pub fn of(place: &'a str, raw_id: i64, script: &'a Script, origin: TrustOrigin) -> Self {
        Self {
            place,
            script_id: raw_id,
            name: &script.summary.name,
            scope: script.summary.scope,
            project: script.summary.project.as_deref(),
            language: script.summary.language,
            body: &script.body,
            origin,
        }
    }
}

/// How a version of a script came to be trusted, and the script it was trusted as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustRecord {
    pub origin: TrustOrigin,
    pub name: String,
    /// The stored scope name (`repo` or `global`).
    pub scope: String,
    /// The repository key, for a repository script.
    pub project: Option<String>,
}

impl TrustRecord {
    /// True when the version was trusted as `summary` is now: same name, scope and repository. A
    /// script a teammate renamed or moved needs approval again.
    pub fn is_for(&self, summary: &ScriptSummary) -> bool {
        self.name == summary.name
            && self.scope == summary.scope.as_str()
            && self.project.as_deref() == summary.project.as_deref()
    }
}

/// A version trusted before, as [`TrustStore::previous`] reads it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousVersion {
    pub body: String,
    /// The stored language name.
    pub language: String,
    /// The name the script had then.
    pub name: String,
    /// The stored scope name it had then (`repo` or `global`).
    pub scope: String,
    /// RFC 3339 UTC.
    pub trusted_at: String,
    pub origin: TrustOrigin,
}

/// How many trusted versions are kept per script ([`TrustStore::trust`] drops older ones).
pub const TRUSTED_VERSIONS_KEPT: i64 = 5;

/// How many versions reported as waiting for approval are remembered
/// ([`TrustStore::note_unapproved`] forgets the oldest).
pub const UNAPPROVED_KEPT: i64 = 10_000;

/// The script versions this machine may run: script `script_id` kept in `place` (see
/// [`TrustedVersion::place`]) with the [`script_sha256`] `sha256`. Always the local SQLite file.
pub trait TrustStore: Send {
    /// How this version of the script came to be trusted here and as which script; `None` when
    /// it is not (check [`TrustRecord::is_for`] before running it).
    fn trust_of(&mut self, place: &str, script_id: i64, sha256: &str) -> Result<Option<TrustRecord>, StoreError>;

    /// True when this version of the script was saved or approved on this machine (as any name).
    fn is_trusted(&mut self, place: &str, script_id: i64, sha256: &str) -> Result<bool, StoreError> {
        Ok(self.trust_of(place, script_id, sha256)?.is_some())
    }

    /// Trusts `version` for its script; trusting it again makes it the script's latest trusted
    /// version (its time, name, scope, repository and origin are updated). Only the last
    /// [`TRUSTED_VERSIONS_KEPT`] versions of a script stay trusted.
    fn trust(&mut self, version: &TrustedVersion) -> Result<(), StoreError>;

    /// The version of script `script_id` in `place` trusted last, if any.
    fn previous(&mut self, place: &str, script_id: i64) -> Result<Option<PreviousVersion>, StoreError>;

    /// Remembers that the version `sha256` (a [`script_sha256`]) was reported here as waiting for
    /// approval. Only the last [`UNAPPROVED_KEPT`] are kept.
    fn note_unapproved(&mut self, sha256: &str) -> Result<(), StoreError>;

    /// True when one of `sha256s` was reported here as waiting for approval.
    fn was_unapproved(&mut self, sha256s: &[String]) -> Result<bool, StoreError>;

    /// Holds version `sha256` of the script `name` in `place` (as script `script_id` there, when
    /// known) back from team trust: it needs the user's approval before it runs even while every
    /// team script is trusted. Never forgotten until the user trusts the version ([`Self::trust`]
    /// with [`TrustOrigin::Approved`] releases it).
    fn hold_back(&mut self, place: &str, script_id: Option<i64>, name: &str, sha256: &str) -> Result<(), StoreError>;

    /// True when version `sha256` of script `script_id` (named `name`) in `place` is held back.
    fn is_held_back(&mut self, place: &str, script_id: i64, name: &str, sha256: &str) -> Result<bool, StoreError>;
}

/// The hash that names a script version for trust: the lowercase hex SHA-256 of
/// `harness-memory-script-v1 NUL <language> NUL <body>`, so the same body run by another
/// interpreter is another version.
pub fn script_sha256(language: Language, body: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(SHA_TAG.as_bytes());
    hash.update([0]);
    hash.update(language.as_str().as_bytes());
    hash.update([0]);
    hash.update(body.as_bytes());

    hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The first character of `body` that hides what it does, with its line (from 1): a control
/// character other than a newline, a tab or a carriage return before a newline, or an invisible
/// format character (bidirectional overrides and isolates, zero-width spaces, the byte-order mark
/// and the rest of [`is_format_character`]). Emoji keep what they need: the emoji joiner (U+200D)
/// next to an emoji, and the text and emoji presentation selectors (U+FE0E, U+FE0F) right after
/// one (U+FE0F also after a keycap digit, `#` or `*`). Any other variation selector is hidden,
/// since it can make two names that read the same differ (`ok` and `ok` + U+FE00 in JavaScript).
pub fn hidden_character(body: &str) -> Option<(char, usize)> {
    marked(body)
        .find(|mark| mark.hidden)
        .map(|mark| (mark.character, mark.line))
}

/// `body` as an agent is shown it: without its [`hidden_character`]s, and a carriage return
/// before a newline left out.
pub fn visible_body(body: &str) -> String {
    marked(body)
        .filter(|mark| !mark.hidden && !(mark.character == '\r' && mark.next == Some('\n')))
        .map(|mark| mark.character)
        .collect()
}

/// A character of a body, with its neighbours, its line and whether it is hidden.
struct Mark {
    character: char,
    next: Option<char>,
    line: usize,
    hidden: bool,
}

/// Every character of `body` as a [`Mark`].
fn marked(body: &str) -> impl Iterator<Item = Mark> + '_ {
    let mut previous = None;
    let mut line = 1;
    let mut characters = body.chars().peekable();

    std::iter::from_fn(move || {
        let character = characters.next()?;
        let next = characters.peek().copied();
        let mark = Mark {
            character,
            next,
            line,
            hidden: is_hidden(previous, character, next),
        };

        if character == '\n' {
            line += 1;
        }

        previous = Some(character);
        Some(mark)
    })
}

fn is_hidden(previous: Option<char>, character: char, next: Option<char>) -> bool {
    let after_emoji = previous.is_some_and(is_emoji_symbol);

    match character {
        '\n' | '\t' => false,
        '\r' => next != Some('\n'),
        '\u{200D}' => !(after_emoji || next.is_some_and(is_emoji_symbol)),
        '\u{FE0E}' => !after_emoji,
        '\u{FE0F}' => !(after_emoji || previous.is_some_and(|base| base.is_ascii_digit() || matches!(base, '#' | '*'))),
        other => other.is_control() || is_format_character(other),
    }
}

/// True for a symbol emoji are made of: the copyright and registered signs, the symbol blocks
/// between U+203C and U+3299 that emoji use (arrows, technical symbols, enclosed alphanumerics,
/// shapes, dingbats, supplemental arrows, miscellaneous symbols and arrows, and the few CJK
/// symbols), and the emoji blocks U+1F000–U+1FAFF.
fn is_emoji_symbol(character: char) -> bool {
    matches!(
        character,
        '\u{A9}'
            | '\u{AE}'
            | '\u{203C}'
            | '\u{2049}'
            | '\u{2122}'
            | '\u{2139}'
            | '\u{2190}'..='\u{21FF}'
            | '\u{2300}'..='\u{23FF}'
            | '\u{2460}'..='\u{27BF}'
            | '\u{2900}'..='\u{297F}'
            | '\u{2B00}'..='\u{2BFF}'
            | '\u{3030}'
            | '\u{303D}'
            | '\u{3297}'
            | '\u{3299}'
            | '\u{1F000}'..='\u{1FAFF}'
    )
}

/// The refusal for a body holding `character` on `line`.
pub fn hidden_character_error((character, line): (char, usize)) -> String {
    format!(
        "the script body holds an invisible or control character (U+{:04X} on line {line}): remove it, or write \
         it as an escape sequence",
        u32::from(character)
    )
}

/// A PEP 263 encoding declaration: `coding:` or `coding=` and the encoding's name, as Python looks
/// for it in a comment on the first two lines (here anywhere on them, so nothing Python reads is
/// missed).
static CODING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"coding[:=]\s*([-\w.]+)").expect("valid"));

/// The names of UTF-8 a declaration may give (compared ignoring case).
const UTF8_NAMES: [&str; 3] = ["utf-8", "utf8", "utf_8"];

/// The encoding names the first two lines of `body` declare (see [`CODING`]), in order.
fn declared_encodings(body: &str) -> impl Iterator<Item = &str> {
    body.split('\n').take(2).flat_map(|line| {
        CODING
            .captures_iter(line)
            .filter_map(|found| found.get(1))
            .map(|name| name.as_str())
    })
}

/// The first encoding other than UTF-8 a Python `body` declares on its first two lines (PEP 263,
/// `# -*- coding: utf-7 -*-`), which would make Python run other code than the text reviewed;
/// `None` for another language or a body that declares only UTF-8 (`utf-8`, `utf8`, `utf_8`, any
/// case) or nothing.
pub fn foreign_encoding(language: Language, body: &str) -> Option<String> {
    if language != Language::Python {
        return None;
    }

    declared_encodings(body)
        .find(|name| !UTF8_NAMES.contains(&name.to_ascii_lowercase().as_str()))
        .map(|name| name.chars().take(32).collect())
}

/// True when a Python `body` declares an encoding at all (UTF-8 then, once [`foreign_encoding`]
/// found none): a UTF-8 byte-order mark before it would make Python refuse `utf8`.
pub fn declares_encoding(body: &str) -> bool {
    declared_encodings(body).next().is_some()
}

/// The refusal for a body that declares the encoding `name`.
pub fn foreign_encoding_error(name: &str) -> String {
    format!(
        "the script body declares the encoding {name}: Python would run other code than the text shown, so only \
         utf-8 may be declared"
    )
}

/// A script row as a store read it, before [`read_back`] checks it. Text fields are read cut a
/// little over their limit (where the database can cut), so a value over the limit is still
/// recognized as one.
#[derive(Debug, Clone)]
pub(crate) struct Stored {
    pub id: i64,
    pub scope: Scope,
    pub project: Option<String>,
    pub name: String,
    pub description: String,
    pub language: String,
    pub body: String,
    pub timeout_secs: i64,
    pub provenance: Provenance,
    pub created_at: i64,
    pub updated_at: i64,
    pub run_count: i64,
    pub last_run_at: Option<i64>,
    pub last_exit_code: Option<i64>,
}

/// `stored` as a script, with its timeout clamped to 1 to [`SCRIPT_TIMEOUT_MAX`] seconds and its
/// [`script_sha256`] computed from the language and body read. `None`, noted on stderr, for a row
/// this server never writes: a language it cannot run, an invalid name, or a description or
/// body over its limit.
pub(crate) fn read_back(stored: Stored) -> Option<Script> {
    let id = stored.id;

    match check_stored(stored) {
        Ok(script) => Some(script),
        Err(reason) => {
            skipped(id, &reason);
            None
        }
    }
}

/// Notes on stderr that script `id` was left out, and why.
pub(crate) fn skipped(id: i64, reason: &str) {
    eprintln!("harness-memory: left out script {id}: {reason}");
}

fn check_stored(stored: Stored) -> Result<Script, String> {
    let Some(language) = Language::parse(&stored.language) else {
        return Err("its language is not one this server runs".to_owned());
    };

    if !is_valid_name(&stored.name) {
        return Err("its name is not a valid script name".to_owned());
    }

    if stored.description.chars().count() > SCRIPT_DESCRIPTION_MAX {
        return Err(format!(
            "its description is longer than {SCRIPT_DESCRIPTION_MAX} characters"
        ));
    }

    if stored.body.len() > SCRIPT_BODY_MAX {
        return Err(format!("its body is larger than {SCRIPT_BODY_MAX} bytes"));
    }

    let longest = i64::try_from(SCRIPT_TIMEOUT_MAX).unwrap_or(i64::MAX);
    let timeout_secs = u64::try_from(stored.timeout_secs.clamp(1, longest)).unwrap_or(1);

    let summary = ScriptSummary {
        id: stored.id,
        scope: stored.scope,
        project: stored.project,
        name: stored.name,
        description: stored.description,
        language,
        sha256: script_sha256(language, &stored.body),
        timeout_secs,
        provenance: stored.provenance,
        created_at: stored.created_at,
        updated_at: stored.updated_at,
        run_count: stored.run_count,
        last_run_at: stored.last_run_at,
        last_exit_code: stored.last_exit_code.and_then(|code| i32::try_from(code).ok()),
    };

    Ok(Script {
        summary,
        body: stored.body,
    })
}

/// True for a script name: `[a-z0-9]` then up to 63 of `[a-z0-9_-]`.
pub fn is_valid_name(name: &str) -> bool {
    let mut characters = name.chars();
    let first = characters
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest = characters.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');

    first && rest && name.len() <= SCRIPT_NAME_MAX
}

/// `draft` ready to store: the name and description trimmed, the description masked with
/// [`redact_text`], the body unchanged. Fails on a bad name, a description, body or timeout over
/// its limit, an empty body, or a body with a [`hidden_character`], a [`foreign_encoding`] or that
/// [`has_credentials`].
pub fn checked_draft(draft: &ScriptDraft) -> Result<ScriptDraft, StoreError> {
    let name = draft.name.trim();
    let description = draft.description.trim();

    if !is_valid_name(name) {
        return Err(StoreError::Invalid(format!(
            "script names are 1 to {SCRIPT_NAME_MAX} of a-z, 0-9, _ and -, starting with a letter or digit"
        )));
    }

    if description.chars().count() > SCRIPT_DESCRIPTION_MAX {
        return Err(StoreError::Invalid(format!(
            "the description is longer than {SCRIPT_DESCRIPTION_MAX} characters"
        )));
    }

    if draft.body.trim().is_empty() {
        return Err(StoreError::Invalid("the script body is empty".to_owned()));
    }

    if draft.body.len() > SCRIPT_BODY_MAX {
        return Err(StoreError::Invalid(format!(
            "the script body is larger than {SCRIPT_BODY_MAX} bytes"
        )));
    }

    if !(1..=SCRIPT_TIMEOUT_MAX).contains(&draft.timeout_secs) {
        return Err(StoreError::Invalid(format!(
            "timeout_secs must be 1 to {SCRIPT_TIMEOUT_MAX}"
        )));
    }

    if let Some(found) = hidden_character(&draft.body) {
        return Err(StoreError::Invalid(hidden_character_error(found)));
    }

    if let Some(name) = foreign_encoding(draft.language, &draft.body) {
        return Err(StoreError::Invalid(foreign_encoding_error(&name)));
    }

    if has_credentials(&draft.body) {
        return Err(StoreError::Invalid(
            "the script body holds a credential: read secrets from environment variables instead (for example \
             $GH_TOKEN)"
                .to_owned(),
        ));
    }

    Ok(ScriptDraft {
        name: name.to_owned(),
        description: redact_text(description),
        ..draft.clone()
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn draft(name: &str, body: &str) -> ScriptDraft {
        ScriptDraft {
            name: name.to_owned(),
            description: " Lists open pull requests ".to_owned(),
            language: Language::Bash,
            body: body.to_owned(),
            timeout_secs: 60,
            provenance: Provenance::default(),
        }
    }

    #[test]
    fn languages_parse_and_print_their_stored_names() {
        for language in Language::ALL {
            assert_eq!(Language::parse(language.as_str()), Some(language));
        }

        assert_eq!(Language::parse(" Python "), Some(Language::Python));
        assert_eq!(Language::parse("ruby"), None);
    }

    #[test]
    fn the_trust_hash_covers_the_language_and_the_body() {
        // printf 'harness-memory-script-v1\0bash\0abc' | sha256sum
        assert_eq!(
            script_sha256(Language::Bash, "abc"),
            "b9b56e4b9f812e7f72cdd0dca3526f2e0d7e71ed4825bd5464eefb817dc89c1e"
        );
        assert_ne!(script_sha256(Language::Sh, "abc"), script_sha256(Language::Bash, "abc"));
        assert_ne!(
            script_sha256(Language::Bash, "abd"),
            script_sha256(Language::Bash, "abc")
        );
    }

    #[test]
    fn hidden_characters_are_found_with_their_line() {
        assert_eq!(hidden_character("echo a\r\n\tprintf b\n"), None);
        assert_eq!(hidden_character("echo a\necho \u{202E}b"), Some(('\u{202E}', 2)));

        for character in [
            '\u{0}', '\u{1B}', '\u{7F}', '\u{85}', '\u{AD}', '\u{61C}', '\u{200B}', '\u{200C}', '\u{200D}', '\u{200E}',
            '\u{200F}', '\u{202A}', '\u{202E}', '\u{2060}', '\u{2064}', '\u{2066}', '\u{2069}', '\u{FE00}', '\u{FE0D}',
            '\u{FE0E}', '\u{FE0F}', '\u{FEFF}',
        ] {
            let body = format!("echo ok{character}");
            assert_eq!(
                hidden_character(&body),
                Some((character, 1)),
                "U+{:04X}",
                u32::from(character)
            );
        }
    }

    #[test]
    fn a_carriage_return_counts_only_before_a_newline() {
        // The auditor's Python and JavaScript bodies: an editor shows one line, the interpreter runs two.
        assert_eq!(
            hidden_character("x = 1 # note\rprint(\"CR-HIDDEN\")\n"),
            Some(('\r', 1))
        );
        assert_eq!(
            hidden_character("let x = 1; // note\rconsole.log(\"CR-HIDDEN\")\n"),
            Some(('\r', 1))
        );
        assert_eq!(hidden_character("echo a\r"), Some(('\r', 1)));
        assert_eq!(hidden_character("echo a\r\necho b\r\n"), None);
        assert_eq!(visible_body("echo a\r\necho b\r\n"), "echo a\necho b\n");
    }

    #[test]
    fn variation_selectors_outside_emoji_are_hidden() {
        // The auditor's JavaScript body: `ok` and `ok` + U+FE00 are two identifiers that read the same.
        let body = "const ok = \"safe\";\nconst ok\u{FE00} = \"EVIL\";\nconsole.log(ok\u{FE00});\n";
        assert_eq!(hidden_character(body), Some(('\u{FE00}', 2)));

        for body in [
            "ok\u{FE0F} = 1",
            "ok\u{FE0E} = 1",
            "\u{2764}\u{FE01}",
            "\u{1F600}\u{FE0D}",
            "a\u{200D}b",
            "x = '\u{200D}'",
        ] {
            assert!(hidden_character(body).is_some(), "{body:?}");
        }
    }

    #[test]
    fn emoji_with_joiners_and_variation_selectors_are_not_hidden() {
        for body in [
            "echo '\u{2714}\u{FE0F}'",
            "echo '\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}'",
            "echo '\u{1F469}\u{200D}\u{1F4BB} done'",
            "echo '\u{2764}\u{FE0F} \u{2615}\u{FE0E}'",
            "echo '\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}'",
            "echo '\u{2764}\u{FE0F}\u{200D}\u{1F525}'",
            "echo '1\u{FE0F}\u{20E3} #\u{FE0F}\u{20E3}'",
            "echo '\u{A9}\u{FE0F} \u{3299}\u{FE0F}'",
        ] {
            assert_eq!(hidden_character(body), None, "{body:?}");
            assert_eq!(visible_body(body), body, "{body:?}");
        }
    }

    #[test]
    fn python_bodies_may_declare_only_utf8() {
        for body in [
            "# -*- coding: utf-7 -*-\nprint(1)",
            "#!/usr/bin/env python3\n# vim: set fileencoding=latin-1 :\n",
            "# coding=UTF-16\n",
            "# coding: utf-8\n# coding: cp1252\n",
            "x = 1  # coding:\tshift_jis\n",
        ] {
            assert!(foreign_encoding(Language::Python, body).is_some(), "{body:?}");
            assert!(declares_encoding(body), "{body:?}");
        }

        for body in [
            "# -*- coding: utf-8 -*-\n",
            "# coding: UTF8\n",
            "# coding=utf_8\n",
            "print('no declaration')\n",
            "print(1)\nprint(2)\n# coding: utf-7\n",
            "open(path, encoding='latin-1')\n",
        ] {
            assert_eq!(foreign_encoding(Language::Python, body), None, "{body:?}");
        }

        assert_eq!(foreign_encoding(Language::Bash, "# coding: utf-7\n"), None);
        assert_eq!(
            foreign_encoding(Language::Python, "# coding: utf-7\n").as_deref(),
            Some("utf-7")
        );
        assert!(!declares_encoding("print(1)\n"));

        let refused = checked_draft(&ScriptDraft {
            language: Language::Python,
            ..draft("x", "# coding: utf-7\nprint(1)\n")
        });
        assert!(matches!(refused, Err(StoreError::Invalid(_))), "{refused:?}");
    }

    fn stored(language: &str, timeout_secs: i64) -> Stored {
        Stored {
            id: 7,
            scope: Scope::Global,
            project: None,
            name: "x".to_owned(),
            description: String::new(),
            language: language.to_owned(),
            body: "true".to_owned(),
            timeout_secs,
            provenance: Provenance::default(),
            created_at: 1,
            updated_at: 1,
            run_count: 0,
            last_run_at: None,
            last_exit_code: Some(i64::MAX),
        }
    }

    #[test]
    fn rows_read_back_are_clamped_or_left_out() {
        let clamped = |timeout| read_back(stored("sh", timeout)).expect("kept").summary.timeout_secs;
        assert_eq!(clamped(-5), 1);
        assert_eq!(clamped(0), 1);
        assert_eq!(clamped(i64::MAX), SCRIPT_TIMEOUT_MAX);
        assert_eq!(clamped(30), 30);

        let kept = read_back(stored("sh", 30)).expect("kept");
        assert_eq!(kept.summary.sha256, script_sha256(Language::Sh, "true"));
        assert_eq!(kept.summary.last_exit_code, None);

        let over = |change: fn(&mut Stored)| {
            let mut row = stored("sh", 30);
            change(&mut row);
            read_back(row)
        };
        assert_eq!(read_back(stored("ruby", 30)), None);
        assert_eq!(over(|row| row.name = "Bad Name".to_owned()), None);
        assert_eq!(
            over(|row| row.description = "d".repeat(SCRIPT_DESCRIPTION_MAX + 1)),
            None
        );
        assert_eq!(over(|row| row.body = "a".repeat(SCRIPT_BODY_MAX + 1)), None);
    }

    #[test]
    fn names_follow_the_pattern() {
        for name in ["open-prs", "a", "9_x", &"a".repeat(64)] {
            assert!(is_valid_name(name), "{name}");
        }

        for name in ["", "-x", "_x", "Open", "a b", "a.b", "ą", &"a".repeat(65)] {
            assert!(!is_valid_name(name), "{name}");
        }
    }

    #[test]
    fn a_checked_draft_is_trimmed_and_keeps_its_body() {
        let body = "gh pr list\n";
        let checked = checked_draft(&draft(" open-prs ", body)).expect("valid");

        assert_eq!(checked.name, "open-prs");
        assert_eq!(checked.description, "Lists open pull requests");
        assert_eq!(checked.body, body);
    }

    #[test]
    fn bad_drafts_are_refused() {
        let token = "ghp_".to_owned() + &"a".repeat(36);
        let cases = [
            draft("Bad Name", "true"),
            draft("x", " \n"),
            draft("x", &"a".repeat(SCRIPT_BODY_MAX + 1)),
            draft("x", &format!("curl -H 'Authorization: token {token}' $URL")),
            draft("x", "echo \u{202E}fdp.exe"),
            draft("x", "echo \u{1B}[2J"),
            ScriptDraft {
                timeout_secs: 0,
                ..draft("x", "true")
            },
            ScriptDraft {
                timeout_secs: SCRIPT_TIMEOUT_MAX + 1,
                ..draft("x", "true")
            },
            ScriptDraft {
                description: "d".repeat(SCRIPT_DESCRIPTION_MAX + 1),
                ..draft("x", "true")
            },
        ];

        for case in cases {
            assert!(matches!(checked_draft(&case), Err(StoreError::Invalid(_))), "{case:?}");
        }
    }
}
