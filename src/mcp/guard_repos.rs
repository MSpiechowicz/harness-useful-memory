//! What `guards/global.json` says about the repositories of a guarded call (see
//! [`super::guards`]), by the file's version ([`RepoRule`]).
//!
//! The call's repositories are the git root of the resolved `project_path` (the server's working
//! folder when the call names none), and the chat's project root from its guard file.
//!
//! Version 1, a deny list (`offRepos`): any one repository is off when its canonical root is an
//! entry (compared as canonical paths), or when its project key (`git:<remote>`, see
//! [`crate::project`]) is that of an entry: another clone or a git worktree of a repository turned
//! off is off too. A chat root at the home folder, which has no project key, matches by its path
//! only. Each key is resolved at most once per call ([`Repos`]).
//!
//! Version 2, an allow list (`everyProject`, `onRepos`, `repoKeys`): saving is off unless
//! `everyProject` is on, or every repository of the call is in `onRepos`. A call with no repository
//! at all is off. Only canonical paths match, never a project key, so another clone is on only
//! when it is listed itself. Each entry also has the project key the repository had when the user
//! turned it on (`repoKeys`, `null` for a folder with no project such as the home folder), and a
//! repository is on only while its key is still that one: a remote that changed afterwards (an
//! edited `.git/config`, a retargeted `origin`) sends the notes of another project's key to this
//! folder, so the user turns it on again to accept it. A key made from the path matches another
//! made from the path, as a linked worktree of a repository with no remote has its own. An entry
//! with no pin (a file from before pins) names nothing. `everyProject` needs no pin. An entry is
//! read as it is written: one that is not its own canonical form (a link in
//! it, or a folder that is gone) names nothing, so a folder that became a link does not turn on
//! the repository it now leads to. A linked git worktree counts as the checkout it was made from,
//! or as the git folder when that belongs to no checkout ([`super::worktree`]): every call root is
//! mapped, and an entry allows the roots its own path, its checkout or its git folder lead to. A
//! call whose chat has no root also counts the server's working folder, so naming an enabled
//! project from a folder that is off still saves nothing (version 1 does not: it looks only at
//! the repositories above).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::Server;
use super::worktree::{Allowed, map};
use crate::store::ProjectKey;

/// What `guards/global.json` says about repositories for one kind.
pub(super) enum RepoRule {
    /// Version 1: saving is off for the repositories listed (`offRepos`).
    Off(Vec<String>),
    /// Version 2: saving is on for every repository (`everyProject`) or for those listed
    /// (`onRepos`).
    On {
        every_project: bool,
        repos: Vec<String>,
        /// The project key each entry had when it was turned on, by entry; `None` for none.
        pins: HashMap<String, Option<String>>,
    },
}

/// One repository of a call: its canonical root and its project key (`None` when it has none:
/// the home folder; an empty key stands for a folder that could not be resolved and matches no
/// pin).
#[derive(Clone)]
struct CallRepo {
    root: PathBuf,
    key: Option<String>,
}

impl CallRepo {
    fn of(project: &ProjectKey) -> Self {
        Self {
            root: canonical(&project.root),
            key: Some(project.key.clone()),
        }
    }
}

/// The repositories of one guarded call, resolved on first use and kept for the call.
pub(super) struct Repos<'a> {
    arguments: &'a Value,
    chat_root: Option<&'a str>,
    /// The call's repositories, each as a pair of its canonical root and its project key.
    resolved: Option<Vec<CallRepo>>,
    /// The server's working folder, resolved on first use (`Some(None)`: it has no project).
    working: Option<Option<CallRepo>>,
    /// The project key of each `offRepos` entry resolved so far (`None` for none).
    off_keys: HashMap<String, Option<String>>,
}

impl<'a> Repos<'a> {
    /// The repositories of the call with `arguments`, whose chat names the project root
    /// `chat_root` in its guard file.
    pub(super) fn new(arguments: &'a Value, chat_root: Option<&'a str>) -> Self {
        Self {
            arguments,
            chat_root,
            resolved: None,
            working: None,
            off_keys: HashMap::new(),
        }
    }

    /// True when `rule` does not let the call save: one of its repositories is turned off (version
    /// 1), or it is not on for every repository of the call (version 2). See the module docs.
    pub(super) fn refused_by(&mut self, server: &mut Server, rule: &RepoRule) -> bool {
        match rule {
            RepoRule::Off(off) => self.any_off(server, off),
            RepoRule::On {
                every_project,
                repos,
                pins,
            } => !*every_project && !self.all_on(server, repos, pins),
        }
    }

    /// True when one of the call's repositories is among `off` (see the module docs).
    fn any_off(&mut self, server: &mut Server, off: &[String]) -> bool {
        if off.is_empty() {
            return false;
        }

        let repos = self.resolved(server).clone();
        let roots: Vec<&PathBuf> = repos.iter().map(|repo| &repo.root).collect();
        let keys: Vec<&String> = repos.iter().filter_map(|repo| repo.key.as_ref()).collect();

        for entry in off {
            if roots.contains(&&canonical(Path::new(entry))) {
                return true;
            }

            let key = self
                .off_keys
                .entry(entry.clone())
                .or_insert_with(|| key_of(server, entry))
                .clone();

            if key.is_some_and(|key| keys.contains(&&key)) {
                return true;
            }
        }

        false
    }

    /// True when the call has a repository and each of them is among `on`, as its main root, with
    /// the project key its entry was pinned with in `pins`.
    ///
    /// A call with no chat root also counts the server's working folder, so an agent that works
    /// in a folder saving is off for cannot save by naming an enabled project: both must be on.
    fn all_on(&mut self, server: &mut Server, on: &[String], pins: &HashMap<String, Option<String>>) -> bool {
        let mut repos = self.resolved(server).clone();

        if self.chat_root.is_none()
            && let Some(working) = self.working_repo(server)
        {
            repos.push(working);
        }

        if repos.is_empty() {
            return false;
        }

        // An entry allows its own path, the checkout of a listed worktree, the worktrees of a listed
        // git folder and those of the git folder a listed checkout names (see `worktree`). A
        // relative entry, one with a link in it and one whose folder is gone name nothing.
        let allowed = Allowed::of(on.iter().map(Path::new));

        repos.iter().all(|repo| {
            allowed
                .allowing(&map(&repo.root))
                .into_iter()
                .filter_map(|entry| entry.to_str())
                .any(|entry| pin_matches(pins.get(entry), repo.key.as_deref()))
        })
    }

    /// The server's working folder with its project key; `None` when it has no project (the home
    /// folder, a folder outside a repository that is no project, a folder that is gone).
    fn working_repo(&mut self, server: &mut Server) -> Option<CallRepo> {
        self.working
            .get_or_insert_with(|| {
                let project = server.projects.resolve(None).ok().flatten()?;

                Some(CallRepo::of(&project))
            })
            .clone()
    }

    fn resolved(&mut self, server: &mut Server) -> &Vec<CallRepo> {
        let (arguments, chat_root) = (self.arguments, self.chat_root);

        self.resolved.get_or_insert_with(|| {
            let mut repos: Vec<CallRepo> = chat_repos(server, arguments).iter().map(CallRepo::of).collect();

            // The chat's root counts as given, so a repository at the home folder, which has no
            // project, is found by its path.
            if let Some(root) = chat_root {
                let key = match server.projects.resolve(Some(root)) {
                    Ok(project) => project.map(|project| project.key),
                    Err(_) => Some(String::new()),
                };

                repos.push(CallRepo {
                    root: canonical(Path::new(root)),
                    key,
                });
            }

            repos
        })
    }
}

/// True when the pin of the entry that allows a repository (`None`: it has none) is the repository's
/// `key`. A pin of `null` matches a repository with no key. A key made from the path matches
/// another made from the path.
fn pin_matches(pin: Option<&Option<String>>, key: Option<&str>) -> bool {
    match (pin, key) {
        (Some(None), None) => true,
        (Some(Some(pinned)), Some(key)) => {
            !key.is_empty() && !pinned.is_empty() && (pinned == key || (is_path_key(pinned) && is_path_key(key)))
        }
        _ => false,
    }
}

fn is_path_key(key: &str) -> bool {
    key.starts_with("path:")
}

/// The projects of a call with `arguments`: that of its `project_path` when given and usable, else
/// that of the server's working folder. A server shared by chats of several projects runs in the
/// folder of the first, which says nothing about a call that names its own project.
fn chat_repos(server: &mut Server, arguments: &Value) -> Vec<ProjectKey> {
    let project_path = arguments.get("project_path").and_then(Value::as_str);
    let mut projects: Vec<ProjectKey> = Vec::new();

    if project_path.is_some_and(|raw| !raw.trim().is_empty())
        && let Ok(Some(project)) = server.projects.resolve(project_path)
    {
        projects.push(project);
    }

    if projects.is_empty()
        && let Ok(Some(project)) = server.projects.resolve(None)
    {
        projects.push(project);
    }

    projects
}

/// The project key of the folder `entry` names; `None` when it is no usable folder or no
/// project.
fn key_of(server: &mut Server, entry: &str) -> Option<String> {
    if !Path::new(entry).is_absolute() {
        return None;
    }

    server
        .projects
        .resolve(Some(entry))
        .ok()
        .flatten()
        .map(|project| project.key)
}

/// `path` canonical, else as given.
fn canonical(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
