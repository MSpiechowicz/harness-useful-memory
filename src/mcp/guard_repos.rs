//! Whether a repository turned off in `guards/global.json` (`offRepos`) is the repository of a
//! guarded call (see [`super::guards`]).
//!
//! The call's repositories are the git root of the resolved `project_path`, that of the server's
//! working folder, and the chat's project root from its guard file. One is off when its canonical
//! root is an `offRepos` entry (compared as canonical paths), or when its project key
//! (`git:<remote>`, see [`crate::project`]) is that of an entry: another clone or a git worktree
//! of a repository turned off is off too. A chat root at the home folder, which has no project
//! key, matches by its path only. Each key is resolved at most once per call ([`Repos`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::Server;
use crate::store::ProjectKey;

/// The repositories of one guarded call, resolved on first use and kept for the call.
pub(super) struct Repos<'a> {
    arguments: &'a Value,
    chat_root: Option<&'a str>,
    /// The canonical roots and the project keys of the call's repositories.
    resolved: Option<(Vec<PathBuf>, Vec<String>)>,
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
            off_keys: HashMap::new(),
        }
    }

    /// True when one of the call's repositories is among `off` (see the module docs).
    pub(super) fn any_off(&mut self, server: &mut Server, off: &[String]) -> bool {
        if off.is_empty() {
            return false;
        }

        let (roots, keys) = self.resolved(server).clone();

        for entry in off {
            if roots.contains(&canonical(Path::new(entry))) {
                return true;
            }

            let key = self
                .off_keys
                .entry(entry.clone())
                .or_insert_with(|| key_of(server, entry))
                .clone();

            if key.is_some_and(|key| keys.contains(&key)) {
                return true;
            }
        }

        false
    }

    fn resolved(&mut self, server: &mut Server) -> &(Vec<PathBuf>, Vec<String>) {
        let (arguments, chat_root) = (self.arguments, self.chat_root);

        self.resolved.get_or_insert_with(|| {
            let mut projects = chat_repos(server, arguments);
            let mut roots: Vec<PathBuf> = projects.iter().map(|project| canonical(&project.root)).collect();

            // The chat's root counts as given, so a repository at the home folder, which has no
            // project, is found by its path.
            if let Some(root) = chat_root {
                roots.push(canonical(Path::new(root)));

                if let Ok(Some(project)) = server.projects.resolve(Some(root)) {
                    projects.push(project);
                }
            }

            let keys = projects.into_iter().map(|project| project.key).collect();
            (roots, keys)
        })
    }
}

/// The projects of a call with `arguments`: that of its `project_path` (when given and usable),
/// then that of the server's working folder, each once.
pub(super) fn chat_repos(server: &mut Server, arguments: &Value) -> Vec<ProjectKey> {
    let project_path = arguments.get("project_path").and_then(Value::as_str);
    let mut projects: Vec<ProjectKey> = Vec::new();

    if project_path.is_some_and(|raw| !raw.trim().is_empty())
        && let Ok(Some(project)) = server.projects.resolve(project_path)
    {
        projects.push(project);
    }

    if let Ok(Some(project)) = server.projects.resolve(None)
        && !projects.iter().any(|known| known.root == project.root)
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
