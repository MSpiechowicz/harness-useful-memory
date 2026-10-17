//! Which repository a call is about.
//!
//! The directory is the tool's `project_path` (absolute, existing) or the server's working
//! directory. Its git work tree, when there is one and it holds the directory, is the project
//! root; otherwise (a `core.worktree` or gitfile can name a work tree elsewhere) the directory
//! itself is, keyed by its path. The key of a work tree is `git:<host/owner/repo>` from the
//! normalized `origin` remote, so every clone of a repository shares its memory; without a usable
//! remote it is `path:<canonical root>`. A root that is the home folder or a filesystem root is no
//! project: agents started there must name the repository.
//! Results are cached per directory for the life of the process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::store::ProjectKey;
use crate::support::Env;
use crate::support::git::Git;
use crate::support::paths::validate_project_dir;
use crate::support::text::display_safe;

/// Resolves and caches project identities. See the module docs.
pub struct ProjectResolver {
    env: Env,
    cwd: Option<PathBuf>,
    cache: HashMap<PathBuf, Option<ProjectKey>>,
}

impl std::fmt::Debug for ProjectResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectResolver")
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

impl ProjectResolver {
    /// A resolver for a server started in `cwd` (`None` when the working directory is unknown).
    pub fn new(env: Env, cwd: Option<PathBuf>) -> Self {
        Self {
            env,
            cwd,
            cache: HashMap::new(),
        }
    }

    /// The project of `project_path`, else of the working directory; `Ok(None)` when there is
    /// none. Fails only for an unusable `project_path`.
    pub fn resolve(&mut self, project_path: Option<&str>) -> Result<Option<ProjectKey>, String> {
        let start = match project_path.map(str::trim).filter(|raw| !raw.is_empty()) {
            Some(raw) => {
                if !Path::new(raw).is_absolute() {
                    return Err("project_path must be an absolute path".to_owned());
                }

                validate_project_dir(&self.env, Path::new(raw)).map_err(|_| {
                    "project_path must be an existing directory that is not a filesystem root".to_owned()
                })?
            }
            None => {
                let Some(cwd) = &self.cwd else {
                    return Ok(None);
                };

                match validate_project_dir(&self.env, cwd) {
                    Ok(dir) => dir,
                    Err(_) => return Ok(None),
                }
            }
        };

        if let Some(known) = self.cache.get(&start) {
            return Ok(known.clone());
        }

        let identity = self.identify(&start);
        self.cache.insert(start, identity.clone());
        Ok(identity)
    }

    fn identify(&self, dir: &Path) -> Option<ProjectKey> {
        let toplevel = self.work_tree(dir);
        let root = toplevel.clone().unwrap_or_else(|| dir.to_path_buf());

        if self.is_home(&root) || root.parent().is_none() {
            return None;
        }

        let remote = match toplevel {
            Some(_) => self
                .git(&root, &["config", "--get", "remote.origin.url"])
                .and_then(|url| normalize_remote(&url)),
            None => None,
        };

        let key = match &remote {
            Some(remote) => format!("git:{remote}"),
            None => format!("path:{}", root.display()),
        };

        Some(ProjectKey { key, root, remote })
    }

    /// The canonical git work tree holding `dir` (itself canonical). `None` outside git, and when
    /// git names a work tree that does not hold `dir`: a `core.worktree` or a gitfile in a folder
    /// can point anywhere, and must not give the folder another checkout's root and key.
    fn work_tree(&self, dir: &Path) -> Option<PathBuf> {
        let reported = self.git(dir, &["rev-parse", "--show-toplevel"])?;
        let top = validate_project_dir(&self.env, Path::new(&reported)).ok()?;

        dir.starts_with(&top).then_some(top)
    }

    /// The environment the server runs with.
    pub fn env(&self) -> &Env {
        &self.env
    }

    /// The server's working directory, when known.
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// The home folder, canonical when it exists.
    pub fn home(&self) -> PathBuf {
        validate_project_dir(&self.env, &self.env.home).unwrap_or_else(|_| self.env.home.clone())
    }

    fn is_home(&self, dir: &Path) -> bool {
        dir == self.home()
    }

    /// The first stdout line of a successful `git <args>` in `dir`; `None` when git is missing,
    /// fails or prints nothing.
    fn git(&self, dir: &Path, args: &[&str]) -> Option<String> {
        let output = Git::new(&self.env, dir).ok()?.run(args).ok()?;

        if !output.success() {
            return None;
        }

        let line = output.stdout.lines().next()?.trim().to_owned();
        (!line.is_empty()).then_some(line)
    }
}

/// `host/owner/repo` from a git remote URL: scheme, credentials, port, query and a trailing
/// `.git` or `/` are dropped, the host is lowercased, and the scp form `user@host:owner/repo`
/// gives the same result as `https://host/owner/repo.git`. `None` for local paths, `file://`
/// URLs, anything unparsable, and URLs with whitespace, control or invisible format characters
/// (the key is shown to agents, so such a remote falls back to the path key).
pub fn normalize_remote(raw: &str) -> Option<String> {
    let url = raw.trim();

    if url.is_empty() || url.chars().any(char::is_whitespace) || !display_safe(url) {
        return None;
    }

    let (authority, path) = match url.split_once("://") {
        Some((scheme, rest)) => {
            if scheme.eq_ignore_ascii_case("file") {
                return None;
            }

            rest.split_once('/')?
        }
        None => {
            let (authority, path) = url.split_once(':')?;

            // A local path (`/srv/repo`, `./repo`, `C:\repo`) is not a remote host.
            if authority.contains(['/', '\\']) || authority.len() == 1 {
                return None;
            }

            (authority, path)
        }
    };

    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = strip_port(host).to_ascii_lowercase();
    let host_ok = host
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || ".-_[]:".contains(character));

    if host.is_empty() || !host_ok {
        return None;
    }

    let path = path.split(['?', '#']).next().unwrap_or_default().trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_end_matches('/');

    if path.is_empty() {
        return None;
    }

    Some(format!("{host}/{path}"))
}

/// `host` without a numeric `:port`.
fn strip_port(host: &str) -> &str {
    match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) && !name.ends_with(':') => {
            name
        }
        _ => host,
    }
}
