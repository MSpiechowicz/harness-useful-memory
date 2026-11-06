//! Who wrote a note or a script: the person's git `user.name` (never the email) and the client the
//! write went through.
//!
//! A record keeps two of them, [`Authors::added_by`] (set once, when it is created) and
//! [`Authors::updated_by`] (set by every write). Both are `None` when no git name was found or
//! the database has no author columns. The name is read with `git config --get user.name` through
//! the hardened runner ([`crate::support::git`]) in the folder of the writer, once per folder and
//! process, and cleaned like the author of a team document ([`check_author`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};

use crate::limits::TEAM_DOC_AUTHOR_MAX;
use crate::store::Provenance;
use crate::store::clean;
use crate::store::team_docs::check_author;
use crate::support::Env;
use crate::support::git::Git;

/// The client name Harness Desktop writes the user's own saves with: such a write has no `via`.
pub const DESKTOP_CLIENT: &str = "harness-desktop";

/// The person behind a write and the client it went through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    /// The git `user.name`.
    pub name: String,
    /// The MCP client of an agent's write; `None` for the user's own (the CLI without `--client`,
    /// Harness Desktop).
    pub via: Option<String>,
}

/// Who added a record and who changed it last.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Authors {
    pub added_by: Option<Author>,
    pub updated_by: Option<Author>,
}

/// The four author columns of a record, as the databases store them: `added_by_name`,
/// `added_by_via`, `updated_by_name`, `updated_by_via`.
pub(crate) type Columns = [Option<String>; 4];

impl Author {
    /// The author named `name` writing through `client`; `None` when `name` is not a usable name.
    /// The desktop app is no client: its saves are the user's own.
    pub fn new(name: &str, client: Option<&str>) -> Option<Self> {
        let Ok(name) = check_author(name) else {
            // Never the name: it is what was refused.
            #[cfg(debug_assertions)]
            eprintln!("harness-memory: debug: a git user.name that is not a usable author name was left out");
            return None;
        };

        let via = clean::source(client).filter(|client| client != DESKTOP_CLIENT);

        Some(Self { name, via })
    }

    /// The author the stored `name` and `via` say; `None` without a usable name. A team database
    /// is shared, so what it holds is cleaned again.
    pub fn stored(name: Option<String>, via: Option<String>) -> Option<Self> {
        let name = name?;

        // A name longer than the limit is refused whole: trimming first could make a database's
        // cut of a longer value (a name, a space, more text) read as a valid one.
        if name.chars().count() > TEAM_DOC_AUTHOR_MAX {
            return None;
        }

        let name = check_author(&name).ok()?;
        let via = clean::source(via.as_deref());

        Some(Self { name, via })
    }

    /// The git name of the person working in `dir` (`root` of the writer, else the current
    /// folder), writing through `client`.
    pub fn resolve(env: &Env, dir: Option<&Path>, client: Option<&str>) -> Option<Self> {
        let name = git_name(env, dir)?;

        Self::new(&name, client)
    }
}

impl Authors {
    /// The authors the four stored values say (see [`Columns`]).
    pub(crate) fn stored(columns: Columns) -> Self {
        let [added_name, added_via, updated_name, updated_via] = columns;

        Self {
            added_by: Author::stored(added_name, added_via),
            updated_by: Author::stored(updated_name, updated_via),
        }
    }
}

/// The name and the client of `author` as stored; both `None` for no author or an unusable name.
pub(crate) fn name_and_via(author: Option<&Author>) -> (Option<String>, Option<String>) {
    let Some(author) = author.and_then(|author| Author::stored(Some(author.name.clone()), author.via.clone())) else {
        return (None, None);
    };

    (Some(author.name), author.via)
}

/// The four author columns of a record `provenance` creates: the original author of a copy
/// ([`Provenance::original_added_by`]) or else the writer is who added it, the writer who changed
/// it last.
pub(crate) fn created_columns(provenance: &Provenance) -> Columns {
    let added = match &provenance.original_added_by {
        Some(original) => original.as_ref(),
        None => provenance.author.as_ref(),
    };

    let (added_name, added_via) = name_and_via(added);
    let (updated_name, updated_via) = name_and_via(provenance.author.as_ref());

    [added_name, added_via, updated_name, updated_via]
}

/// The most folders whose answer is kept.
const CACHE_MAX: usize = 64;

/// What asking git for the name came to.
enum Asked {
    /// Git ran: the name it printed (as printed, not yet checked), or none when it printed none.
    Answer(Option<String>),
    /// Git did not run to the end (missing, failed, timed out): the answer is unknown.
    Unknown,
}

/// `git config --get user.name` in `dir` (else the home folder, so the global configuration
/// still applies), asked once per folder and process when git answered. A failed ask is not kept,
/// so the next write asks again.
fn git_name(env: &Env, dir: Option<&Path>) -> Option<String> {
    static CACHE: LazyLock<Names> = LazyLock::new(Names::default);

    cached_name(&CACHE, env, dir, ask_git)
}

/// The answers per home folder and asked (canonical) folder.
#[derive(Default)]
struct Names(Mutex<HashMap<(PathBuf, PathBuf), Option<String>>>);

/// [`git_name`] with the cache and the way of asking chosen.
fn cached_name(names: &Names, env: &Env, dir: Option<&Path>, ask: impl FnOnce(&Env, &Path) -> Asked) -> Option<String> {
    let dir = dir.map_or_else(|| env.home.clone(), Path::to_path_buf);

    // A folder that cannot be resolved cannot be told from another, so its answer is not kept.
    // dunce: a plain path on Windows too (std's \\?\C:\ form is one git cannot use with -C).
    let Ok(dir) = dunce::canonicalize(&dir) else {
        return match ask(env, &dir) {
            Asked::Answer(name) => name,
            Asked::Unknown => None,
        };
    };

    let key = (env.home.clone(), dir);

    if let Some(known) = names.0.lock().ok()?.get(&key) {
        return known.clone();
    }

    let name = match ask(env, &key.1) {
        Asked::Answer(name) => name,
        Asked::Unknown => return None,
    };

    if let Ok(mut cache) = names.0.lock() {
        if cache.len() >= CACHE_MAX {
            let evicted = cache.keys().next().cloned();

            if let Some(evicted) = evicted {
                cache.remove(&evicted);
            }
        }

        cache.insert(key, name.clone());
    }

    name
}

fn ask_git(env: &Env, dir: &Path) -> Asked {
    let Ok(git) = Git::new(env, dir) else {
        return Asked::Unknown;
    };

    let Ok(output) = git.run(&["config", "--get", "user.name"]) else {
        return Asked::Unknown;
    };

    // `git config --get` exits 1 for a key nobody set; any other failure is not an answer.
    match output.status {
        Some(0) => {}
        Some(1) => return Asked::Answer(None),
        _ => return Asked::Unknown,
    }

    let name = output
        .stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|name| !name.is_empty());

    Asked::Answer(name.map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn env_in(home: &Path) -> Env {
        Env::for_tests(home)
    }

    #[test]
    fn a_failed_ask_is_asked_again_and_an_answer_is_kept() {
        let dir = tempfile::tempdir().expect("temp");
        let env = env_in(dir.path());
        let names = Names::default();
        let asked = Cell::new(0);

        let failing = |_: &Env, _: &Path| {
            asked.set(asked.get() + 1);
            Asked::Unknown
        };
        assert_eq!(cached_name(&names, &env, Some(dir.path()), failing), None);
        assert_eq!(cached_name(&names, &env, Some(dir.path()), failing), None);
        assert_eq!(asked.get(), 2, "a failure is not kept");

        let answering = |_: &Env, _: &Path| {
            asked.set(asked.get() + 1);
            Asked::Answer(Some("Ada".to_owned()))
        };
        assert_eq!(
            cached_name(&names, &env, Some(dir.path()), answering),
            Some("Ada".to_owned())
        );
        assert_eq!(
            cached_name(&names, &env, Some(dir.path()), failing),
            Some("Ada".to_owned())
        );
        assert_eq!(asked.get(), 3, "an answer is kept");
    }

    #[test]
    fn no_name_is_an_answer_too_and_a_folder_is_one_key_however_it_is_spelled() {
        let dir = tempfile::tempdir().expect("temp");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        let env = env_in(dir.path());
        let names = Names::default();
        let asked = Cell::new(0);

        let none = |_: &Env, _: &Path| {
            asked.set(asked.get() + 1);
            Asked::Answer(None)
        };
        let plain = dir.path().join("sub");
        let roundabout = dir.path().join("sub").join("..").join("sub");

        assert_eq!(cached_name(&names, &env, Some(&plain), none), None);
        assert_eq!(cached_name(&names, &env, Some(&roundabout), none), None);
        assert_eq!(asked.get(), 1);
    }

    #[test]
    fn the_cache_stays_bounded() {
        let dir = tempfile::tempdir().expect("temp");
        let env = env_in(dir.path());
        let names = Names::default();
        let asked = Cell::new(0);

        let counting = |_: &Env, _: &Path| {
            asked.set(asked.get() + 1);
            Asked::Answer(Some("Ada".to_owned()))
        };

        for index in 0..(CACHE_MAX * 2) {
            let folder = dir.path().join(format!("repo-{index}"));
            std::fs::create_dir(&folder).expect("mkdir");
            assert_eq!(
                cached_name(&names, &env, Some(&folder), counting),
                Some("Ada".to_owned())
            );
        }

        let kept = (0..(CACHE_MAX * 2))
            .filter(|index| {
                let before = asked.get();
                let folder = dir.path().join(format!("repo-{index}"));
                cached_name(&names, &env, Some(&folder), counting);
                asked.get() == before
            })
            .count();

        assert!(kept <= CACHE_MAX, "{kept} folders are kept");
    }
}
