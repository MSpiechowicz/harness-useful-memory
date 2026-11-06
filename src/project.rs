//! Which repository a call is about.
//!
//! The directory is the tool's `project_path` (absolute, existing) or the server's working
//! directory. Its git work tree, when there is one, holds the directory and has a `.git` of its
//! own, is the project root; otherwise (a `core.worktree` or gitfile can name a work tree
//! elsewhere, and a tracked folder shaped like a bare repository names one on purpose) the
//! directory itself is, keyed by its path, and no remote is read. A genuine bare repository has no
//! work tree, so it is such a directory too. The key of a work tree is `git:<host/owner/repo>` from
//! the normalized `origin` remote of the repository its `.git` leads to, so every clone of a
//! repository shares its memory; without a usable remote it is `path:<canonical root>`. A work
//! tree that is a submodule (git reports a superproject for it, or for the main work tree of the
//! module a linked worktree belongs to) is keyed by its path and its remote is never read: the
//! remote of a submodule is the superproject's `.gitmodules` to say, and an upstream change of
//! that file, brought in by `git submodule sync` or a fresh `clone --recurse-submodules`, would
//! otherwise give it the key of another repository. A root that is the home folder or a
//! filesystem root is no project: agents started there must name the repository.
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
        let Some(start) = self.folder(project_path)? else {
            return Ok(None);
        };

        if let Some(known) = self.cache.get(&start) {
            return Ok(known.clone());
        }

        let identity = self.identify(&start);
        self.cache.insert(start, identity.clone());
        Ok(identity)
    }

    /// The canonical folder [`Self::resolve`] starts from: that of `project_path`, else the working
    /// directory; `Ok(None)` when there is no usable working directory. Fails only for an unusable
    /// `project_path`.
    pub fn folder(&self, project_path: Option<&str>) -> Result<Option<PathBuf>, String> {
        let Some(raw) = project_path.map(str::trim).filter(|raw| !raw.is_empty()) else {
            let Some(cwd) = &self.cwd else {
                return Ok(None);
            };

            return Ok(validate_project_dir(&self.env, cwd).ok());
        };

        if !Path::new(raw).is_absolute() {
            return Err("project_path must be an absolute path".to_owned());
        }

        validate_project_dir(&self.env, Path::new(raw))
            .map(Some)
            .map_err(|_| "project_path must be an existing directory that is not a filesystem root".to_owned())
    }

    fn identify(&self, dir: &Path) -> Option<ProjectKey> {
        let tree = self.work_tree(dir);
        let root = tree
            .as_ref()
            .map_or_else(|| dir.to_path_buf(), |tree| tree.root.clone());

        if self.is_home(&root) || root.parent().is_none() {
            return None;
        }

        // Only a root that passed `work_tree` has a remote to read, from the repository its own
        // `.git` leads to: the same one that gave the root. A submodule has none to read.
        let remote = match tree {
            Some(tree) if !self.is_submodule(&tree) => self
                .git(&root, &["config", "--local", "--get", "remote.origin.url"])
                .and_then(|url| normalize_remote(&url)),
            _ => None,
        };

        let key = match &remote {
            Some(remote) => format!("git:{remote}"),
            None => format!("path:{}", root.display()),
        };

        Some(ProjectKey { key, root, remote })
    }

    /// The canonical git work tree holding `dir` (itself canonical). `None` outside git, and for
    /// every work tree that is not plainly the checkout of the repository git found:
    ///
    /// - one that does not hold `dir`: a `core.worktree` or a gitfile in a folder can point
    ///   anywhere, and must not give the folder another checkout's root and key;
    /// - one without a `.git` (a folder or a gitfile): a tracked folder shaped like a bare
    ///   repository (`HEAD`, `objects/`, `refs/`, `config`) is a repository to git, with a work
    ///   tree and a remote its author chose, but a checkout always has its `.git`;
    /// - one that git does not find again from its own folder, in the repository `dir` belongs
    ///   to: `core.worktree = ..` in such a folder names a parent checkout that is another
    ///   repository, and its remote is not this folder's.
    ///
    /// A genuine bare repository has no work tree either, so it is a project by its path.
    fn work_tree(&self, dir: &Path) -> Option<WorkTree> {
        let lines = self.git_lines(
            dir,
            &["rev-parse", "--show-toplevel", "--absolute-git-dir", "--git-common-dir"],
        )?;
        let (reported, git_dir, common) = (lines.first()?, lines.get(1)?, lines.get(2)?);

        let top = validate_project_dir(&self.env, Path::new(reported)).ok()?;

        if !dir.starts_with(&top) || !top.join(".git").exists() {
            return None;
        }

        let git_dir = canonical_or_same(Path::new(git_dir));

        if top != dir {
            let own = self.git(&top, &["rev-parse", "--absolute-git-dir"])?;

            if canonical_or_same(Path::new(&own)) != git_dir {
                return None;
            }
        }

        // `--git-common-dir` is relative to the folder git ran in.
        let common_dir = canonical_or_same(&dir.join(common));

        Some(WorkTree {
            root: top,
            git_dir,
            common_dir,
        })
    }

    /// True when `tree` is a submodule: git finds a superproject above its root (`git rev-parse
    /// --show-superproject-working-tree` prints it), or it is a linked worktree of a submodule
    /// (git finds none from there, so the main work tree of the module is asked: the module's
    /// `core.worktree`, accepted when its `.git` leads back to the module). A nested clone that
    /// the repository above does not track as a submodule has no superproject.
    fn is_submodule(&self, tree: &WorkTree) -> bool {
        // Git can miss the superproject (a tracked folder shaped like a repository above it, or a
        // gitlink upstream dropped), but a module's git data always lives in `.git/modules`.
        if in_modules_folder(&tree.common_dir) || self.has_superproject(&tree.root) {
            return true;
        }

        // A linked worktree of a plain checkout has the checkout's `.git` as its common folder: it
        // is no submodule's, whose common folder is a folder of the superproject's `modules`.
        if tree.common_dir == tree.git_dir || tree.common_dir.file_name().is_some_and(|name| name == ".git") {
            return false;
        }

        let Some(configured) = self.git(&tree.root, &["config", "--local", "--get", "core.worktree"]) else {
            return false;
        };

        let main = canonical_or_same(&tree.common_dir.join(configured));
        let leads_back = self
            .git(&main, &["rev-parse", "--absolute-git-dir"])
            .is_some_and(|own| canonical_or_same(Path::new(&own)) == tree.common_dir);

        leads_back && self.has_superproject(&main)
    }

    /// True when git prints a superproject work tree for the work tree at `root`.
    fn has_superproject(&self, root: &Path) -> bool {
        self.git(root, &["rev-parse", "--show-superproject-working-tree"])
            .is_some()
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
        canonical_home(&self.env)
    }

    fn is_home(&self, dir: &Path) -> bool {
        dir == self.home()
    }

    /// The first stdout line of a successful `git <args>` in `dir`; `None` when git is missing,
    /// fails or prints nothing.
    fn git(&self, dir: &Path, args: &[&str]) -> Option<String> {
        self.git_lines(dir, args)?.into_iter().next()
    }

    /// The non-empty stdout lines of a successful `git <args>` in `dir`; `None` when git is
    /// missing, fails or prints nothing.
    fn git_lines(&self, dir: &Path, args: &[&str]) -> Option<Vec<String>> {
        let output = Git::new(&self.env, dir).ok()?.run(args).ok()?;

        if !output.success() {
            return None;
        }

        let lines: Vec<String> = output
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();

        (!lines.is_empty()).then_some(lines)
    }
}

/// A git work tree, as git reports it from a folder inside.
struct WorkTree {
    /// The canonical work tree root.
    root: PathBuf,
    /// The canonical git folder of the work tree (a linked worktree's own admin folder).
    git_dir: PathBuf,
    /// The canonical common git folder: the same as `git_dir` unless this is a linked worktree.
    common_dir: PathBuf,
}

/// `path` canonical, else as given.
fn canonical_or_same(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The home folder of `env`, canonical when it exists.
pub fn canonical_home(env: &Env) -> PathBuf {
    validate_project_dir(env, &env.home).unwrap_or_else(|_| env.home.clone())
}

/// `host/owner/repo` from a git remote URL: scheme, credentials, port, query and a trailing
/// `.git` or `/` are dropped, the host is lowercased, and the scp form `user@host:owner/repo`
/// gives the same result as `https://host/owner/repo.git`. `None` for local paths, `file://`
/// URLs, anything unparsable, URLs with whitespace, control or invisible format characters
/// (the key is shown to agents, so such a remote falls back to the path key), and URLs with a
/// `?`, `#` or `\` in the authority (git and this parser could read another host there).
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

    // Git ends the authority at `?`, `#` or `\` (or reads them differently from this parser), so
    // the host it connects to may not be the one this would take from the text after an `@`.
    if authority.contains(['?', '#', '\\']) {
        return None;
    }

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

/// True when `git_dir` sits in the `modules` folder of a `.git` folder: the git data of a submodule.
fn in_modules_folder(git_dir: &Path) -> bool {
    let parts: Vec<_> = git_dir.components().map(|part| part.as_os_str()).collect();

    parts.windows(2).any(|pair| pair[0] == ".git" && pair[1] == "modules")
}
