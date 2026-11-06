//! Where a call root stands for the save guards' allow list ([`super::guard_repos`]): [`map`] says
//! whether it is its own root, a linked worktree of a checkout, or a linked worktree of a git
//! folder that belongs to no checkout.
//!
//! A linked worktree has a `.git` file `gitdir: <admin>`, where `<admin>` is
//! `<common>/worktrees/<name>` and holds `commondir` (the common git folder) and `gitdir` (the
//! worktree's `.git` file, the back-link). A `.git` file is believed only when the back-link names
//! this very `.git` file and the admin folder really is a child of `<common>/worktrees`, so the
//! files of a folder alone (a forged admin folder inside it, or elsewhere with a good back-link)
//! cannot claim another repository. Every read is small and bounded, and any doubt (a missing or
//! large file, a submodule, which has no `commondir`) leaves the root as it is.
//!
//! - A common folder named `.git` belongs to the checkout that holds it: the worktree is a
//!   [`Mapping::Checkout`] of that folder's parent and matches the allow list by path.
//! - Any other common folder (a repository made with `--separate-git-dir`, or a bare one) leads to
//!   no checkout. It counts only when it is shaped like a git folder ([`is_git_folder`]: `HEAD` a
//!   regular file, `objects` and `refs` folders, no `.git` inside), and the worktree is then a
//!   [`Mapping::GitFolder`]. A work tree that only holds a `worktrees/<name>` folder with the two
//!   files is no git folder: a forged one in a listed folder leaves the root as itself.
//! - Everything else is [`Mapping::Itself`].
//!
//! An entry names something only when it is its own canonical form ([`names_itself`]): one with a
//! link in it names nothing, so a listed folder that a `git pull` replaced by a tracked link, or
//! any folder under a link, does not turn on the repository the link leads to. This is checked
//! before any of the entry's folders is read.
//!
//! A [`Mapping::GitFolder`] is allowed only by an entry that is that git folder, or by the entry
//! of the main checkout that keeps its git folder there ([`separate_git_dir`]: a `.git` file
//! `gitdir: X`, `X` shaped like a git folder with no `commondir`). Such an entry never allows a
//! root that maps to itself or to a checkout, so a listed folder cannot vouch for another
//! repository by naming it. An entry that is itself a verified worktree allows its
//! [`Mapping::Checkout`] and its own path ([`Allowed`]).
//!
//! What this does not stop: someone who can already write inside a repository's own git folder
//! (its `worktrees` folder, or a `.git` file in a listed checkout that names a git folder of
//! another repository) can still make a root that repository's worktree, and that listed
//! checkout still turns on the worktrees of the git folder it names. A plain folder that was
//! delivered without git (an archive, say) and that the user turned on, shaped like a git folder,
//! can vouch for a folder that has a planted `.git` file: the shape of a folder cannot prove it is
//! a git folder. That takes a way to put files there, an archive or a shell. With a separate git
//! folder, listing the git folder for one kind and the checkout for another can show a worktree as
//! on in the desktop's UI where this server saves.
//!
//! The desktop applies the same rule (`saving_root` in `business/src/ecosystem/memory_guards`) and
//! both repositories test the same cases (`tests/mcp_guard_worktree_cases.rs` here,
//! `business/src/ecosystem/memory_guards/tests/saving_root_cases.rs` there): a change to this rule
//! updates the other repository and its cases.
//!
//! The back-link is compared by path, so a new folder at the path of a worktree removed without
//! `git worktree remove` or a prune counts as that worktree until the entry goes. Git agrees it is one.

use std::io::Read;
use std::path::{Path, PathBuf};

/// The largest file read here, in bytes. The desktop reads the same bound.
const FILE_MAX: u64 = 16 * 1024;

/// The prefix of the line a worktree's `.git` file holds.
const GITDIR_PREFIX: &str = "gitdir:";

/// What a call root stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mapping {
    /// The root itself: not a verified worktree, or its common folder cannot be believed.
    Itself(PathBuf),
    /// A verified worktree of the checkout at this folder (its common folder is named `.git`).
    Checkout(PathBuf),
    /// A verified worktree of this git folder, which is not named `.git` and is shaped like one.
    GitFolder(PathBuf),
}

impl Mapping {
    /// The folder the root stands for.
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Itself(path) | Self::Checkout(path) | Self::GitFolder(path) => path,
        }
    }
}

/// What the call root `root` stands for (see the module docs).
pub(crate) fn map(root: &Path) -> Mapping {
    let itself = || Mapping::Itself(root.to_path_buf());

    let Some(common) = linked_common(root) else {
        return itself();
    };

    if common.file_name().is_some_and(|name| name == ".git") {
        return common
            .parent()
            .map_or_else(itself, |main| Mapping::Checkout(main.to_path_buf()));
    }

    if is_git_folder(&common) {
        return Mapping::GitFolder(common);
    }

    itself()
}

/// The roots the list entries allow (see the module docs), each with the entries that allow it.
#[derive(Default)]
pub(super) struct Allowed<'a> {
    /// Roots that are themselves a path in the list, and the checkouts of listed worktrees.
    paths: Vec<(PathBuf, &'a Path)>,
    /// The git folders whose worktrees are on.
    git_folders: Vec<(PathBuf, &'a Path)>,
}

impl<'a> Allowed<'a> {
    /// What `entries` allow. An entry that does not [name itself](names_itself) names nothing,
    /// before any of its folders is read.
    pub(super) fn of(entries: impl IntoIterator<Item = &'a Path>) -> Self {
        let mut allowed = Self::default();

        for entry in entries.into_iter().filter(|entry| names_itself(entry)) {
            if let Mapping::Checkout(main) = map(entry) {
                allowed.paths.push((main, entry));
            }

            if is_git_folder(entry) {
                allowed.git_folders.push((entry.to_path_buf(), entry));
            }

            if let Some(git_dir) = separate_git_dir(entry) {
                allowed.git_folders.push((git_dir, entry));
            }

            allowed.paths.push((entry.to_path_buf(), entry));
        }

        allowed
    }

    /// The entries that allow a call root that maps to `mapping`; none when it is not on.
    pub(super) fn allowing(&self, mapping: &Mapping) -> Vec<&'a Path> {
        let (list, wanted) = match mapping {
            Mapping::Itself(path) | Mapping::Checkout(path) => (&self.paths, path),
            Mapping::GitFolder(git_dir) => (&self.git_folders, git_dir),
        };

        list.iter()
            .filter(|(path, _)| path == wanted)
            .map(|(_, entry)| *entry)
            .collect()
    }
}

/// True when `entry` is an absolute path that is its own canonical form: no link in it, now. The
/// desktop writes entries canonical, so this holds for every entry it wrote for a folder that is
/// still there. A link in an entry (a folder that became a link after `git pull` brought one, or
/// a folder moved and replaced by a link) would lead to a repository the user never turned on, so
/// such an entry, and one whose folder is gone, names nothing. The canonical form is compared
/// exactly, as text: it is the form the desktop stored (`dunce::canonicalize`), so a spelling that
/// differs only by case or by a Windows drive letter is a different path and names nothing.
fn names_itself(entry: &Path) -> bool {
    entry.is_absolute() && canonical(entry).is_some_and(|real| real.as_os_str() == entry.as_os_str())
}

/// The git folder of the main checkout at `entry` when it keeps its git folder elsewhere
/// (`git init --separate-git-dir`): its `.git` file is one line `gitdir: X` and `X` is shaped like
/// a git folder ([`is_git_folder`]) with no `commondir`, so it is no worktree's admin folder. Only
/// a list entry, which the desktop wrote, is asked: a worktree of such a repository maps to `X`
/// ([`map`]), and the checkout cannot be told from `X` alone, as `X` names no work tree.
pub(super) fn separate_git_dir(entry: &Path) -> Option<PathBuf> {
    let git_dir = git_file_dir(entry)?;

    if !is_git_folder(&git_dir) || std::fs::symlink_metadata(git_dir.join("commondir")).is_ok() {
        return None;
    }

    Some(git_dir)
}

/// True when `path` is shaped like a git folder: `HEAD` a regular file, `objects` and `refs`
/// folders, and no `.git` inside (so a work tree is none, whatever else it holds).
pub(crate) fn is_git_folder(path: &Path) -> bool {
    let kind = |name: &str| std::fs::symlink_metadata(path.join(name)).map(|metadata| metadata.file_type());

    kind("HEAD").is_ok_and(|kind| kind.is_file())
        && kind("objects").is_ok_and(|kind| kind.is_dir())
        && kind("refs").is_ok_and(|kind| kind.is_dir())
        && kind(".git").is_err()
}

/// The canonical folder `root/.git` names, when that is a file of one line `gitdir: X`.
fn git_file_dir(root: &Path) -> Option<PathBuf> {
    let marker = root.join(".git");

    if !std::fs::symlink_metadata(&marker).ok()?.file_type().is_file() {
        return None;
    }

    let line = one_line(&marker)?;

    canonical(&root.join(line.strip_prefix(GITDIR_PREFIX)?.trim_start()))
}

/// The common git folder (canonical) of the verified linked worktree at `root`.
fn linked_common(root: &Path) -> Option<PathBuf> {
    let marker = root.join(".git");
    let admin = git_file_dir(root)?;

    let common = canonical(&admin.join(one_line(&admin.join("commondir"))?))?;
    let back = canonical(&admin.join(one_line(&admin.join("gitdir"))?))?;

    // The worktree's admin folder must point back at this `.git` file.
    if back != canonical(&marker)? {
        return None;
    }

    // The admin folder must be one of the common folder's own worktree folders.
    if admin.parent()? != common.join("worktrees") {
        return None;
    }

    Some(common)
}

/// The only line of the regular file `path`, without its line ending (only trailing CR and LF are
/// removed); `None` when it is missing, not a regular file, larger than [`FILE_MAX`], not text,
/// empty or holds more than one line.
fn one_line(path: &Path) -> Option<String> {
    let text = read_small(path)?;
    let line = text.trim_end_matches(['\r', '\n']);

    if line.is_empty() || line.contains(['\r', '\n']) {
        return None;
    }

    Some(line.to_owned())
}

/// The text of the regular file `path`, at most [`FILE_MAX`] bytes; `None` when it is missing,
/// larger or not text. On Unix the file is opened without waiting, so a FIFO put in its place
/// cannot hang the server.
fn read_small(path: &Path) -> Option<String> {
    let file = open_without_waiting(path).ok()?;

    if !file.metadata().ok()?.file_type().is_file() {
        return None;
    }

    let mut bytes = Vec::new();
    file.take(FILE_MAX + 1).read_to_end(&mut bytes).ok()?;

    if bytes.len() as u64 > FILE_MAX {
        return None;
    }

    String::from_utf8(bytes).ok()
}

#[cfg(unix)]
fn open_without_waiting(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_without_waiting(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

fn canonical(path: &Path) -> Option<PathBuf> {
    dunce::canonicalize(path).ok()
}
