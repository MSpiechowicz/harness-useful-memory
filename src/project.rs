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
        canonical_home(&self.env)
    }

    fn is_home(&self, dir: &Path) -> bool {
        dir == self.home()
    }

    /// True when `root` is too big to be one project's code: a folder that holds the home folder (`/home`,
    /// `/Users`, a drive), or a system folder (`/etc`, `/usr`, `/var`, `C:\Windows`, ...). The code index
    /// refuses such a root, whatever its size, because indexing it reads files that are nobody's code.
    pub fn is_broad(&self, root: &Path) -> bool {
        is_broad_root(&self.home(), root)
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

/// The home folder of `env`, canonical when it exists.
pub fn canonical_home(env: &Env) -> PathBuf {
    validate_project_dir(env, &env.home).unwrap_or_else(|_| env.home.clone())
}

/// True when `root` is too big to be one project's code (see [`ProjectResolver::is_broad`]).
fn is_broad_root(home: &Path, root: &Path) -> bool {
    home.starts_with(root) || is_system_dir(&root.to_string_lossy())
}

/// Why the code index does not read a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeRootRefusal {
    /// A folder that holds the home folder, or a system folder.
    Broad,
    /// A hidden folder directly under the home folder (`~/.config`, `~/.ssh`): where tools keep settings
    /// and credentials, never a project.
    HiddenInHome,
    /// Not a git work tree: the code index prints lines of the files it reads, and a folder that is no
    /// repository (a settings folder, a notes folder) is not what an agent is asked to understand.
    NotGit,
}

impl CodeRootRefusal {
    /// What the agent is told.
    pub fn message(self, root: &Path) -> String {
        match self {
            Self::Broad => format!(
                "project_path {} holds your home folder or is a system folder, which is too big to index. Pass the \
                 absolute path of the repository you are working in",
                root.display()
            ),
            Self::HiddenInHome => format!(
                "project_path {} is a hidden folder of your home folder, where tools keep settings and credentials, \
                 so the code index does not read it. Pass the absolute path of the repository you are working in",
                root.display()
            ),
            Self::NotGit => "The code index only works inside a git repository.".to_owned(),
        }
    }
}

/// Why the code index must not read `root` (canonical), if it must not. `home` is the canonical home folder.
pub fn code_root_refusal(home: &Path, root: &Path) -> Option<CodeRootRefusal> {
    if is_broad_root(home, root) {
        return Some(CodeRootRefusal::Broad);
    }

    // macOS and Windows file systems ignore case: `/users/me/.ssh` is `/Users/me/.ssh`
    let ignore_case = cfg!(any(target_os = "macos", windows));

    if first_below(home, root, ignore_case).is_some_and(|first| is_tool_folder(&first, cfg!(windows))) {
        return Some(CodeRootRefusal::HiddenInHome);
    }

    (!is_git_work_tree(root)).then_some(CodeRootRefusal::NotGit)
}

/// The most bytes read from a `.git` file.
const GITFILE_MAX: u64 = 4096;

/// The name of the first component of `root` below `home`, when `root` is inside it. Components are compared
/// without regard to case when `ignore_case`.
fn first_below(home: &Path, root: &Path, ignore_case: bool) -> Option<String> {
    let mut home = home.components();
    let mut root = root.components();

    loop {
        match (home.next(), root.next()) {
            (None, Some(first)) => return Some(first.as_os_str().to_string_lossy().into_owned()),
            (Some(left), Some(right)) => {
                let (left, right) = (left.as_os_str().to_string_lossy(), right.as_os_str().to_string_lossy());
                let same = if ignore_case {
                    left.to_lowercase() == right.to_lowercase()
                } else {
                    left == right
                };

                if !same {
                    return None;
                }
            }
            _ => return None,
        }
    }
}

/// True for a folder directly under the home folder where tools keep settings and credentials: a hidden one
/// (`.config`, `.ssh`), and on Windows `AppData`.
fn is_tool_folder(name: &str, windows: bool) -> bool {
    name.starts_with('.') || (windows && name.eq_ignore_ascii_case("AppData"))
}

/// True when `root` is a git work tree: it has a `.git` folder that holds a `HEAD`, or a `.git` file (a worktree,
/// a submodule or a repository with a separate git folder) whose first line is `gitdir: <path>` with a path to a
/// folder that holds a `HEAD`. A link named `.git`, an empty one and a file that says anything else are not.
/// Nothing is run: git would read the repository's own configuration.
pub fn is_git_work_tree(root: &Path) -> bool {
    let git = root.join(".git");
    let Ok(meta) = std::fs::symlink_metadata(&git) else {
        return false;
    };

    if meta.is_dir() {
        return has_head(&git);
    }

    if !meta.is_file() {
        return false;
    }

    let Some(first) = read_first_line(&git) else {
        return false;
    };
    let Some(target) = first.strip_prefix("gitdir:") else {
        return false;
    };
    let target = target.trim();

    !target.is_empty() && has_head(&root.join(target))
}

/// True when `folder` is a folder (not a link) that holds a `HEAD`.
fn has_head(folder: &Path) -> bool {
    std::fs::metadata(folder).is_ok_and(|meta| meta.is_dir()) && std::fs::symlink_metadata(folder.join("HEAD")).is_ok()
}

/// The first line of the file `path`, from at most [`GITFILE_MAX`] bytes.
fn read_first_line(path: &Path) -> Option<String> {
    use std::io::Read;

    let mut bytes = Vec::new();

    std::fs::File::open(path)
        .ok()?
        .take(GITFILE_MAX)
        .read_to_end(&mut bytes)
        .ok()?;

    String::from_utf8_lossy(&bytes).lines().next().map(str::to_owned)
}

/// Unix folders that hold the system or other people's files, never one project.
const SYSTEM_DIRS: &[&str] = &[
    "/",
    "/etc",
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/var",
    "/boot",
    "/dev",
    "/proc",
    "/sys",
    "/run",
    "/tmp",
    "/opt",
    "/srv",
    "/mnt",
    "/media",
    "/home",
    "/root",
    "/nix",
    "/snap",
    "/private",
    // macOS resolves /etc, /var and /tmp to these
    "/private/etc",
    "/private/var",
    "/private/tmp",
    "/Users",
    "/System",
    "/Library",
    "/Applications",
    "/Volumes",
];

/// Windows folders of the same kind, below a drive and in lowercase.
const WINDOWS_SYSTEM_DIRS: &[&str] = &[
    "windows",
    "windows\\system32",
    "program files",
    "program files (x86)",
    "programdata",
    "users",
];

/// True when `path` is exactly one of the system folders, in Unix or Windows spelling.
fn is_system_dir(path: &str) -> bool {
    // macOS and Windows file systems ignore case: `/USERS` is `/Users`
    system_dir(path, cfg!(any(target_os = "macos", windows)))
}

fn system_dir(path: &str, ignore_case: bool) -> bool {
    let trimmed = if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    };

    if SYSTEM_DIRS.contains(&trimmed) {
        return true;
    }

    if ignore_case && SYSTEM_DIRS.iter().any(|dir| dir.eq_ignore_ascii_case(trimmed)) {
        return true;
    }

    let lower = path.to_ascii_lowercase();
    let plain = lower.strip_prefix("\\\\?\\").unwrap_or(&lower);
    let bytes = plain.as_bytes();

    if bytes.get(1) != Some(&b':') || !bytes.first().is_some_and(u8::is_ascii_alphabetic) {
        return false;
    }

    let below = plain
        .get(2..)
        .unwrap_or("")
        .trim_matches(['\\', '/'])
        .replace('/', "\\");

    below.is_empty() || WINDOWS_SYSTEM_DIRS.contains(&below.as_str())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_folders_are_recognised_in_both_spellings() {
        for dir in [
            "/",
            "/etc",
            "/usr",
            "/bin",
            "/var/",
            "/home",
            "/Users",
            "/tmp",
            "/private/etc",
            "/private/var/",
            "/private/tmp",
        ] {
            assert!(is_system_dir(dir), "{dir}");
        }

        for dir in [
            "C:\\",
            "c:\\windows",
            "C:\\Windows\\",
            "D:\\Program Files",
            "\\\\?\\C:\\Users",
            "C:/Windows",
        ] {
            assert!(is_system_dir(dir), "{dir}");
        }
    }

    #[test]
    fn system_folder_names_are_compared_without_case_where_the_file_system_ignores_it() {
        for dir in ["/USERS", "/users/", "/Private/Var", "/ETC"] {
            assert!(system_dir(dir, true), "{dir}");
            assert!(!system_dir(dir, false), "{dir}");
        }

        assert!(!system_dir("/USERS/me/app", true));
    }

    #[test]
    fn a_hidden_folder_under_home_and_a_folder_without_git_are_refused_for_the_code_index() {
        let dir = tempfile::tempdir().expect("temp");
        let home = dir.path().canonicalize().expect("canonical");
        let make = |relative: &str, git: bool| {
            let path = home.join(relative);

            std::fs::create_dir_all(&path).expect("mkdir");

            if git {
                std::fs::create_dir_all(path.join(".git")).expect("git");
                std::fs::write(path.join(".git/HEAD"), "ref: refs/heads/main\n").expect("head");
            }

            path
        };

        assert_eq!(code_root_refusal(&home, &make("work/app", true)), None);
        assert_eq!(
            code_root_refusal(&home, &make("work/linked", false)),
            Some(CodeRootRefusal::NotGit)
        );
        assert_eq!(
            code_root_refusal(&home, &make(".config/tool", true)),
            Some(CodeRootRefusal::HiddenInHome)
        );
        assert_eq!(
            code_root_refusal(&home, &make(".ssh", false)),
            Some(CodeRootRefusal::HiddenInHome)
        );
        assert_eq!(
            code_root_refusal(&home, dir.path().parent().expect("parent")),
            Some(CodeRootRefusal::Broad)
        );

        // a gitfile (a worktree or a submodule) counts, a link named .git does not
        let worktree = make("work/tree", false);
        let gitdir = make("work/app/.git/worktrees/tree", false);

        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/feature\n").expect("head");
        std::fs::write(worktree.join(".git"), format!("gitdir: {}\n", gitdir.display())).expect("gitfile");
        assert_eq!(code_root_refusal(&home, &worktree), None);

        #[cfg(unix)]
        {
            let linked = make("work/sneaky", false);

            std::os::unix::fs::symlink("/etc", linked.join(".git")).expect("link");
            assert_eq!(code_root_refusal(&home, &linked), Some(CodeRootRefusal::NotGit));
        }
    }

    #[test]
    fn project_folders_are_not_system_folders() {
        for dir in [
            "/home/me/app",
            "/usr/local/src/app",
            "/opt/app",
            "/var/www/site",
            "/private/var/folders/ab/T/app",
            "/srv/app",
            "C:\\Users\\me\\app",
            "D:\\work",
            "\\\\?\\C:\\src\\app",
        ] {
            assert!(!is_system_dir(dir), "{dir}");
        }
    }

    #[test]
    fn only_a_git_folder_with_a_head_or_a_gitfile_leading_to_one_makes_a_work_tree() {
        let dir = tempfile::tempdir().expect("temp");
        let base = dir.path().canonicalize().expect("canonical");
        let tree = |name: &str| {
            let path = base.join(name);

            std::fs::create_dir_all(&path).expect("mkdir");

            path
        };
        let separate = base.join("separate-git-dir");

        std::fs::create_dir_all(&separate).expect("mkdir");
        std::fs::write(separate.join("HEAD"), "ref: refs/heads/main\n").expect("head");

        // an empty .git folder or file is nothing
        let empty_dir = tree("empty-dir");
        std::fs::create_dir_all(empty_dir.join(".git")).expect("git");
        assert!(!is_git_work_tree(&empty_dir));

        let empty_file = tree("empty-file");
        std::fs::write(empty_file.join(".git"), "").expect("gitfile");
        assert!(!is_git_work_tree(&empty_file));

        // a gitfile that says something else, or leads nowhere, or to a folder without a HEAD
        for (name, text) in [
            ("words", "just some words\n".to_owned()),
            ("missing", "gitdir: /no/such/folder\n".to_owned()),
            ("headless", format!("gitdir: {}\n", empty_dir.display())),
            ("blank", "gitdir:\n".to_owned()),
            ("second-line", format!("something\ngitdir: {}\n", separate.display())),
        ] {
            let path = tree(name);

            std::fs::write(path.join(".git"), text).expect("gitfile");
            assert!(!is_git_work_tree(&path), "{name}");
        }

        // a git folder with a HEAD, and the gitfiles of a separate git folder, absolute and relative
        let real = tree("real");
        std::fs::create_dir_all(real.join(".git")).expect("git");
        std::fs::write(real.join(".git/HEAD"), "ref: refs/heads/main\n").expect("head");
        assert!(is_git_work_tree(&real));

        let absolute = tree("absolute");
        std::fs::write(absolute.join(".git"), format!("gitdir: {}\n", separate.display())).expect("gitfile");
        assert!(is_git_work_tree(&absolute));

        let relative = tree("relative");
        std::fs::write(relative.join(".git"), "gitdir: ../separate-git-dir\n").expect("gitfile");
        assert!(is_git_work_tree(&relative));

        assert!(!is_git_work_tree(&tree("none")));
    }

    #[test]
    fn the_hidden_folders_of_the_home_folder_are_found_whatever_the_case_of_the_path_where_it_is_ignored() {
        let home = Path::new("/Users/me");

        assert_eq!(
            first_below(home, Path::new("/Users/me/.ssh/keys"), false).as_deref(),
            Some(".ssh")
        );
        assert_eq!(first_below(home, Path::new("/users/ME/.ssh"), false), None);
        assert_eq!(
            first_below(home, Path::new("/users/ME/.ssh"), true).as_deref(),
            Some(".ssh")
        );
        assert_eq!(
            first_below(home, Path::new("/Users/me"), true),
            None,
            "the home folder itself"
        );
        assert_eq!(
            first_below(home, Path::new("/Users/mean/.ssh"), true),
            None,
            "another user"
        );
        assert_eq!(first_below(home, Path::new("/elsewhere/.ssh"), true), None);

        assert!(is_tool_folder(".config", false));
        assert!(!is_tool_folder("AppData", false));
        assert!(is_tool_folder("appdata", true));
        assert!(is_tool_folder("AppData", true));
        assert!(!is_tool_folder("app", true));
    }
}
