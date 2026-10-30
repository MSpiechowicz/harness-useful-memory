//! The bounded walk of a repository and the safe read of a file.
//!
//! The caller passes the canonical root. The walk honours `.gitignore` and `.ignore` (also outside a git
//! repository) and the `.git/info/exclude` of a real `.git` folder, never follows links, stays on the
//! root's file system, skips the fixed exclusions and files over 1 MB, and gives up with "repository too
//! large to index" past the caps. Stored paths are relative and `/`-separated.
//!
//! The repository is untrusted input, so the `ignore` crate loads none of its ignore files: they are read
//! here (`read_ignore`) with the same care as a source file. A link, a device or a file over 1 MB is never
//! read, a `.git` file that points elsewhere is never followed, and nothing outside the root is opened.
//! The ignore files of one walk are read up to a total of [`MAX_IGNORE_TOTAL`] bytes (the rest count as
//! skipped), a line over [`MAX_IGNORE_LINE`] bytes is dropped, and only the rules of the folders on the path
//! to the current folder are kept in memory. The walk gives up with "indexing took too long" at its deadline.
//! [`read_text`] opens the file without following a link, checks the handle (a regular file with one link,
//! at most 1 MB, inside the root, and on Unix the very file the walk saw, so a swap for a link is noticed)
//! and reads at most one byte more than the cap.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Instant, UNIX_EPOCH};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use super::extract;
use super::pace::{Pace, TOO_SLOW};

/// The largest file that is read, in bytes.
pub const MAX_FILE_BYTES: u64 = 1_000_000;

/// The caps of a walk.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Entries (files and folders) the walk may visit.
    pub walked: usize,
    /// Files (sources and package files) the index may hold.
    pub indexed: usize,
    /// Bytes of ignore files the walk may read in total.
    pub ignore_bytes: usize,
    /// Entries one folder may hold. A folder with more makes the repository too large, before anything of it
    /// is buffered.
    pub per_dir: usize,
    /// The deepest folder that is read (the root is 0). Deeper ones are left out and counted.
    pub depth: usize,
    /// The longest path below the root, in bytes, that is visited. A longer entry is left out and counted.
    pub path_bytes: usize,
    /// The bytes of the paths of all the files kept. Past it the repository is too large: the index, the
    /// stamps compared on every call and the graph each hold the paths again.
    pub path_total: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            walked: 200_000,
            indexed: 20_000,
            ignore_bytes: MAX_IGNORE_TOTAL,
            per_dir: 50_000,
            depth: 128,
            path_bytes: 1_024,
            path_total: 16 * 1024 * 1024,
        }
    }
}

/// The error of a walk past its caps.
pub const TOO_LARGE: &str = "repository too large to index";

/// The error of a repository the worker process broke on (it was killed past its memory or time limit, or
/// crashed), which the server answers instead of indexing it: the same for whatever the cause.
pub const LIMITS: &str = "This repository could not be indexed within the code index limits.";

/// The error of a walk or a refresh that `stop` cancelled.
pub const CANCELLED: &str = "cancelled";

/// Folders that never hold code worth indexing, wherever they are.
const EXCLUDED_DIRS: &[&str] = &["node_modules", "target", ".venv", "venv", "__pycache__", "vendor"];

/// Hidden folders that hold tool state, caches or generated output: never indexed. Any other hidden folder
/// that no ignore file excludes is (`.github`, `.storybook`, `.vitepress`). Hidden files are never indexed.
const HIDDEN_TOOL_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".svelte-kit",
    ".next",
    ".nuxt",
    ".output",
    ".cache",
    ".turbo",
    ".venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".idea",
    ".vscode",
    ".gradle",
    ".terraform",
    ".yarn",
    ".pnpm-store",
    ".direnv",
    ".angular",
    ".astro",
    ".docusaurus",
    ".expo",
    ".nx",
    ".parcel-cache",
    ".vercel",
    ".netlify",
    ".serverless",
    ".wrangler",
    ".ruff_cache",
    ".eggs",
];

/// Folders that are build output at the top of the repository and next to a project manifest, but may be
/// plain source folders elsewhere (`src/node/build/`). `.gitignore` covers the rest.
const OUTPUT_DIRS: &[&str] = &["dist", "build"];

/// Files that make a folder a project root, whose `dist` and `build` folders are output.
const PROJECT_MANIFESTS: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "go.mod",
    "pyproject.toml",
    "setup.py",
    "pom.xml",
    "build.gradle",
    "deno.json",
];

/// What a file is to the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A source file of the language with this name (`files.lang`).
    Source(&'static str),
    /// A manifest the resolvers read: `Cargo.toml`, `go.mod`, `tsconfig*.json`, `jsconfig*.json`.
    Package,
}

/// A file the walk found.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Relative to the root, `/`-separated. The absolute path is the root's plus this: an entry holds the
    /// path once.
    pub rel: String,
    pub mtime_ns: i64,
    pub size: i64,
    /// Device and inode of what the walk saw (Unix).
    pub identity: Option<(u64, u64)>,
    pub class: Class,
}

/// What was left out, and why.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Skipped {
    /// Folders and files that could not be read (permissions, races) or whose names are not UTF-8.
    pub unreadable: usize,
    /// Files whose text is not UTF-8.
    pub not_utf8: usize,
    /// Files that were over the size cap, not regular, or no longer the file the walk saw when read.
    pub unsafe_or_large: usize,
    /// Files that went past the per-file caps on definitions and references: indexed, but cut.
    pub truncated: usize,
    /// Source files a tool wrote (a bundle, generated or minified code): left out, not an error.
    pub generated: usize,
}

impl Skipped {
    /// Everything counted.
    pub fn total(&self) -> usize {
        self.unreadable + self.not_utf8 + self.unsafe_or_large
    }
}

/// The result of a walk: the indexable files, sorted by path.
#[derive(Debug, Default)]
pub struct Walked {
    pub entries: Vec<Entry>,
    pub skipped: Skipped,
}

/// Walks `root` with the default caps and no deadline. `stop` is polled for every entry.
pub fn walk(root: &Path, stop: &dyn Fn() -> bool) -> Result<Walked, String> {
    walk_with(root, stop, Limits::default())
}

/// Walks `root` within `limits`, with no deadline.
pub fn walk_with(root: &Path, stop: &dyn Fn() -> bool, limits: Limits) -> Result<Walked, String> {
    walk_until(root, stop, None, limits)
}

/// Walks `root` with the default caps, within the cancellation and the time budget of `pace`.
pub(super) fn walk_paced(root: &Path, pace: &Pace) -> Result<Walked, String> {
    walk_until(root, pace.stop(), Some(pace.deadline()), Limits::default())
}

/// Walks `root` within `limits`. `stop` is polled for every entry, and `deadline` (when there is one) ends
/// the walk, also while it reads ignore files or skips entries, with [`TOO_SLOW`].
///
/// The folders are read here, one entry at a time, and not by a library that lists a whole folder (and sorts
/// it) before it yields the first entry: the caps count what is read, so a folder of millions of files is
/// refused before it is held. A folder keeps only the names of its subfolders while the walk is below them
/// (at most [`Limits::walked`] of them in all). The files found are sorted by path at the end, so that the
/// result does not depend on the order the system lists a folder in.
pub(super) fn walk_until(
    root: &Path,
    stop: &dyn Fn() -> bool,
    deadline: Option<Instant>,
    limits: Limits,
) -> Result<Walked, String> {
    let mut walk = Walk {
        root,
        stop,
        limits,
        ignores: Ignores::new(root, deadline, limits.ignore_bytes),
        device: fs::symlink_metadata(root).ok().and_then(|metadata| device(&metadata)),
        walked: Walked::default(),
        path_bytes: 0,
        // the root itself
        visited: 1,
    };

    walk.poll()?;
    walk.folder(root, 0)?;

    // the filter turns every entry away once the deadline passed, which ends the loop quietly
    if walk.ignores.expired() {
        return Err(TOO_SLOW.into());
    }

    let mut walked = walk.walked;

    walked.skipped.unsafe_or_large += walk.ignores.refused.load(Ordering::Relaxed);

    // the order of a depth first walk that visits the entries of a folder by name
    walked
        .entries
        .sort_by(|left, right| Path::new(&left.rel).cmp(Path::new(&right.rel)));

    Ok(walked)
}

/// The state of one walk.
struct Walk<'a> {
    root: &'a Path,
    stop: &'a dyn Fn() -> bool,
    limits: Limits,
    ignores: Ignores,
    /// The device of the root: a folder on another one is not entered (Unix).
    device: Option<u64>,
    walked: Walked,
    /// The bytes of the paths of the files kept so far (see [`Limits::path_total`]).
    path_bytes: usize,
    /// Entries the walk counts against [`Limits::walked`]: the ones it did not turn away.
    visited: usize,
}

impl Walk<'_> {
    fn poll(&self) -> Result<(), String> {
        if (self.stop)() {
            return Err(CANCELLED.into());
        }

        if self.ignores.expired() {
            return Err(TOO_SLOW.into());
        }

        Ok(())
    }

    /// Reads the folder `path` (`depth` below the root), takes its files, then enters its subfolders by name.
    fn folder(&mut self, path: &Path, depth: usize) -> Result<(), String> {
        let Ok(reader) = fs::read_dir(path) else {
            self.walked.skipped.unreadable += 1;

            return Ok(());
        };
        let mut subfolders: Vec<OsString> = Vec::new();
        let mut listed = 0_usize;

        for item in reader {
            self.poll()?;

            // counted before anything of the entry is held
            listed += 1;

            if listed > self.limits.per_dir {
                return Err(TOO_LARGE.into());
            }

            let Ok(item) = item else {
                self.walked.skipped.unreadable += 1;

                continue;
            };
            let name = item.file_name();

            // the path of the entry below the root
            let below = path
                .as_os_str()
                .len()
                .saturating_sub(self.root.as_os_str().len())
                .saturating_add(name.len());

            if below >= self.limits.path_bytes {
                self.walked.skipped.unreadable += 1;

                continue;
            }

            let full = path.join(&name);
            let Ok(kind) = item.file_type() else {
                self.walked.skipped.unreadable += 1;

                continue;
            };
            let candidate = Candidate {
                path: &full,
                parent: path,
                name: &name,
                is_dir: kind.is_dir(),
                depth: depth + 1,
            };

            if !self.ignores.allows(&candidate) {
                continue;
            }

            self.visited += 1;

            if self.visited > self.limits.walked {
                return Err(TOO_LARGE.into());
            }

            if kind.is_dir() {
                self.note_folder(&item, depth, &name, &mut subfolders);
            } else if kind.is_file() {
                self.file(&item, &full)?;
            }
        }

        subfolders.sort();

        for name in subfolders {
            self.folder(&path.join(name), depth + 1)?;
        }

        Ok(())
    }

    /// Keeps the folder `name` to enter, unless it is too deep or on another file system.
    fn note_folder(&mut self, item: &fs::DirEntry, depth: usize, name: &OsStr, subfolders: &mut Vec<OsString>) {
        if depth + 1 > self.limits.depth {
            self.walked.skipped.unreadable += 1;

            return;
        }

        let other_device = match (self.device, item.metadata().ok().and_then(|metadata| device(&metadata))) {
            (Some(root), Some(found)) => root != found,
            _ => false,
        };

        if !other_device {
            subfolders.push(name.to_owned());
        }
    }

    /// Takes the file `full` when it is a source or a package file the index may read.
    fn file(&mut self, item: &fs::DirEntry, full: &Path) -> Result<(), String> {
        let Some(rel) = full.strip_prefix(self.root).ok().and_then(relative_key) else {
            self.walked.skipped.unreadable += 1;

            return Ok(());
        };
        let Some(class) = classify(&rel) else {
            return Ok(());
        };
        let Ok(metadata) = item.metadata() else {
            self.walked.skipped.unreadable += 1;

            return Ok(());
        };

        if metadata.len() > MAX_FILE_BYTES {
            return Ok(());
        }

        // a second name for a file may lead out of the repository
        if maybe_linked(&metadata) {
            self.walked.skipped.unsafe_or_large += 1;

            return Ok(());
        }

        if self.walked.entries.len() >= self.limits.indexed {
            return Err(TOO_LARGE.into());
        }

        self.path_bytes = self.path_bytes.saturating_add(rel.len());

        if self.path_bytes > self.limits.path_total {
            return Err(TOO_LARGE.into());
        }

        self.walked.entries.push(Entry {
            rel,
            mtime_ns: mtime_ns(&metadata),
            size: i64::try_from(metadata.len()).unwrap_or(i64::MAX),
            identity: identity(&metadata),
            class,
        });

        Ok(())
    }
}

/// An entry the walk is deciding on.
struct Candidate<'a> {
    path: &'a Path,
    parent: &'a Path,
    name: &'a OsStr,
    is_dir: bool,
    /// Below the root: 1 for the entries of the root.
    depth: usize,
}

/// The largest ignore file that is read, in bytes. The server reads ignore files on every call, in its own
/// process, so these caps are small: real ones are a few hundred bytes.
const MAX_IGNORE_BYTES: u64 = 256 * 1024;

/// The most bytes of ignore files one walk reads, for a repository that makes a thousand folders carry a
/// large one each.
pub const MAX_IGNORE_TOTAL: usize = 256 * 1024;

/// The longest ignore line that is compiled, in bytes. A longer one is dropped (a pattern this long is
/// never written by hand, and compiling it costs several times its size).
const MAX_IGNORE_LINE: usize = 1_024;

/// The most wildcards (`*`, `?`, `[`, `{`) one ignore line may hold. A pattern with more is never written by
/// hand, and the matcher compiled from one grows much faster than the line.
const MAX_IGNORE_WILDCARDS: usize = 64;

/// The most ignore patterns one file contributes.
const MAX_IGNORE_LINES: usize = 20_000;

/// The most ignore patterns a walk compiles.
const MAX_IGNORE_PATTERNS: usize = 100_000;

/// The ignore files of a folder, in the order ripgrep ranks them: `.ignore`, `.gitignore`,
/// `.git/info/exclude`.
type Rules = [Option<Arc<Gitignore>>; 3];

/// An ignore file compiled by an earlier walk of this process, with what that walk counted for it.
struct Compiled {
    mtime_ns: i64,
    size: u64,
    /// The bytes the file counted against the total of a walk.
    text_len: usize,
    /// What the compiled rules weigh in memory, estimated by [`compiled_cost`].
    cost: usize,
    /// The patterns it contributed.
    added: usize,
    /// A line was left out (counts as a refused file).
    dropped: bool,
    rules: Option<Arc<Gitignore>>,
}

/// The ignore files compiled so far by this process, by path, so that a server does not compile the same
/// files again on every call. A file is used again only while its modification time and size are the same.
static COMPILED: OnceLock<Mutex<HashMap<PathBuf, Compiled>>> = OnceLock::new();

/// The most ignore files kept compiled, and the most bytes of memory the compiled rules may weigh (see
/// [`compiled_cost`]). Past either, all are dropped, and a file that is over the budget by itself is not kept.
const COMPILED_FILES: usize = 256;
const COMPILED_COST: usize = 8 * 1024 * 1024;

/// Bytes of memory a compiled ignore file costs per byte of its text, at the worst: lines of a thousand
/// characters with a wildcard every few of them, measured at 70 to 100 times the text.
const COMPILED_PER_BYTE: usize = 100;

/// Bytes a compiled pattern costs on top of its text (the matcher around it).
const COMPILED_PER_PATTERN: usize = 512;

/// What compiled rules of `text_len` bytes of text and `added` patterns weigh. The cache is budgeted by this,
/// not by the text: a file of 250 KiB of wildcard lines compiles to 25 MB.
fn compiled_cost(text_len: usize, added: usize) -> usize {
    text_len
        .saturating_mul(COMPILED_PER_BYTE)
        .saturating_add(added.saturating_mul(COMPILED_PER_PATTERN))
}

fn compiled_files() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Compiled>> {
    COMPILED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// The most folders on one path from the root that may contribute ignore files. Every entry is matched against
/// all of them, so a chain of a thousand nested folders with one each would cost a thousand matches per entry.
/// Real repositories have a handful. The ignore files of deeper folders are left unread and count as skipped.
const MAX_IGNORE_CHAIN: usize = 64;

/// What the ignore files of a directory and its parents say about a path.
struct Chain {
    parent: Option<Arc<Chain>>,
    own: Rules,
    /// Folders in this chain, itself included, that contribute ignore files.
    folders: usize,
}

/// What an ignore chain decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Ignore,
    Keep,
    Undecided,
}

impl Chain {
    /// The first match, as ripgrep ranks them: every `.ignore` on the way up (closest first) before any
    /// `.gitignore`, and those before `.git/info/exclude`.
    fn verdict(&self, path: &Path, is_dir: bool) -> Verdict {
        for kind in 0..3 {
            let mut node = Some(self);

            while let Some(chain) = node {
                if let Some(matcher) = chain.own.get(kind).and_then(Option::as_ref) {
                    match matcher.matched(path, is_dir) {
                        Match::Ignore(_) => return Verdict::Ignore,
                        Match::Whitelist(_) => return Verdict::Keep,
                        Match::None => {}
                    }
                }

                node = chain.parent.as_deref();
            }
        }

        Verdict::Undecided
    }
}

/// The ignore rules of a walk, read by this module and never by the `ignore` crate.
struct Ignores {
    root: PathBuf,
    /// The rules of the folders from the root to the folder the walk is in, with those of their parents.
    /// The walk goes depth first, so a folder it has left is dropped from here.
    chains: Mutex<HashMap<PathBuf, Arc<Chain>>>,
    /// Ignore files left unread (links, over a cap, not regular files).
    refused: AtomicUsize,
    /// Patterns compiled so far.
    patterns: AtomicUsize,
    /// Bytes of ignore files read so far.
    bytes: AtomicUsize,
    /// The most bytes of ignore files the walk reads.
    byte_cap: usize,
    deadline: Option<Instant>,
    /// Set once the deadline passed.
    late: AtomicBool,
}

impl Ignores {
    fn new(root: &Path, deadline: Option<Instant>, byte_cap: usize) -> Self {
        Self {
            root: root.to_path_buf(),
            chains: Mutex::new(HashMap::new()),
            refused: AtomicUsize::new(0),
            patterns: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
            byte_cap,
            deadline,
            late: AtomicBool::new(false),
        }
    }

    /// True once the deadline passed.
    fn expired(&self) -> bool {
        if self.late.load(Ordering::Relaxed) {
            return true;
        }

        if self.deadline.is_some_and(|deadline| Instant::now() > deadline) {
            self.late.store(true, Ordering::Relaxed);

            return true;
        }

        false
    }

    /// True when the walk enters or lists `entry`.
    fn allows(&self, entry: &Candidate) -> bool {
        // past the deadline nothing else is listed, so the walk ends
        if self.expired() {
            return false;
        }

        let name = entry.name.to_str();

        if entry.is_dir && (name == Some(".git") || excluded_dir(entry)) {
            return false;
        }

        match self.chain(entry.parent).verdict(entry.path, entry.is_dir) {
            Verdict::Ignore => false,
            Verdict::Keep => true,
            // a hidden file is skipped, and so is a hidden folder of a tool, unless an ignore file asked for
            // it back; any other hidden folder is walked
            Verdict::Undecided => {
                !name.is_some_and(|name| name.starts_with('.') && (!entry.is_dir || HIDDEN_TOOL_DIRS.contains(&name)))
            }
        }
    }

    /// The chain of the directory `dir`, built (reading its ignore files) on first use.
    fn chain(&self, dir: &Path) -> Arc<Chain> {
        if let Some(found) = self.chains.lock().unwrap_or_else(PoisonError::into_inner).get(dir) {
            return Arc::clone(found);
        }

        let parent = if dir == self.root {
            None
        } else {
            dir.parent()
                .filter(|up| up.starts_with(&self.root))
                .map(|up| self.chain(up))
        };
        let above = parent.as_ref().map_or(0, |parent| parent.folders);
        let own = self.read_dir_rules(dir, above < MAX_IGNORE_CHAIN);
        let built = match (own.iter().all(Option::is_none), parent) {
            (true, Some(parent)) => parent,
            (empty, parent) => Arc::new(Chain {
                parent,
                own,
                folders: above + usize::from(!empty),
            }),
        };
        let mut chains = self.chains.lock().unwrap_or_else(PoisonError::into_inner);

        // the folders on the way down stay, the ones the walk has left are of no use
        chains.retain(|kept, _| dir.starts_with(kept));
        chains.insert(dir.to_path_buf(), Arc::clone(&built));

        built
    }

    /// The rules of the ignore files in `dir`.
    fn read_dir_rules(&self, dir: &Path, room: bool) -> Rules {
        // only a real `.git` folder: a `.git` file (a worktree or a submodule) points elsewhere, and a link
        // could point anywhere
        let exclude = fs::symlink_metadata(dir.join(".git")).is_ok_and(|meta| meta.is_dir());

        [
            self.read_ignore(dir, &dir.join(".ignore"), room),
            self.read_ignore(dir, &dir.join(".gitignore"), room),
            exclude
                .then(|| self.read_ignore(dir, &dir.join(".git").join("info").join("exclude"), room))
                .flatten(),
        ]
    }

    /// The rules of one ignore file, when it is a small regular file inside the root and the chain of folders
    /// above it has `room` for one more.
    fn read_ignore(&self, dir: &Path, file: &Path, room: bool) -> Option<Arc<Gitignore>> {
        let Ok(seen) = fs::symlink_metadata(file) else {
            return None; // most directories have none
        };

        // the total of the walk is capped, so many folders with a large file each cost a bounded amount
        if !room || self.expired() || self.bytes.load(Ordering::Relaxed) >= self.byte_cap {
            self.refused.fetch_add(1, Ordering::Relaxed);

            return None;
        }

        let stamp = (mtime_ns(&seen), seen.len());
        let reusable = seen.is_file() && !maybe_linked(&seen);

        // the same file as an earlier walk compiled: its rules, counted as they were then
        if reusable
            && let Some(hit) = compiled_files()
                .get(file)
                .filter(|hit| (hit.mtime_ns, hit.size) == stamp)
                .map(|hit| (hit.text_len, hit.added, hit.dropped, hit.rules.clone()))
        {
            return self.account(hit.0, hit.1, hit.2).then_some(hit.3).flatten();
        }

        let Some(text) = read_ignore_text(&self.root, file) else {
            self.refused.fetch_add(1, Ordering::Relaxed);

            return None;
        };

        if self.bytes.load(Ordering::Relaxed).saturating_add(text.len()) > self.byte_cap {
            self.refused.fetch_add(1, Ordering::Relaxed);

            return None;
        }

        let mut builder = GitignoreBuilder::new(dir);
        let mut added = 0_usize;
        let mut dropped = false;

        for (index, line) in text.lines().take(MAX_IGNORE_LINES).enumerate() {
            let line = if index == 0 {
                line.trim_start_matches('\u{feff}')
            } else {
                line
            };

            if line.len() > MAX_IGNORE_LINE || wildcards(line) > MAX_IGNORE_WILDCARDS {
                dropped = true;

                continue;
            }

            if self.patterns.load(Ordering::Relaxed) + added >= MAX_IGNORE_PATTERNS {
                break;
            }

            if builder.add_line(None, line).is_ok() {
                added += 1;
            }
        }

        let rules = builder.build().ok().map(Arc::new);

        if reusable {
            remember(file, stamp, text.len(), added, dropped, &rules);
        }

        self.account(text.len(), added, dropped).then_some(rules).flatten()
    }

    /// Counts a compiled file against the walk's totals; false when it takes the walk past the byte total.
    fn account(&self, text_len: usize, added: usize, dropped: bool) -> bool {
        if self.bytes.fetch_add(text_len, Ordering::Relaxed) + text_len > self.byte_cap {
            self.refused.fetch_add(1, Ordering::Relaxed);

            return false;
        }

        self.patterns.fetch_add(added, Ordering::Relaxed);

        // a file with a line left out is partly read
        if dropped {
            self.refused.fetch_add(1, Ordering::Relaxed);
        }

        true
    }
}

/// How many wildcard characters `line` holds.
fn wildcards(line: &str) -> usize {
    line.bytes()
        .filter(|byte| matches!(byte, b'*' | b'?' | b'[' | b'{'))
        .count()
}

/// Keeps the rules compiled from `file`, whose stamp (modification time, size) was `stamp`.
fn remember(
    file: &Path,
    stamp: (i64, u64),
    text_len: usize,
    added: usize,
    dropped: bool,
    rules: &Option<Arc<Gitignore>>,
) {
    let cost = compiled_cost(text_len, added);

    // too heavy to keep: compiled again on the next walk, which costs time and not memory
    if cost > COMPILED_COST {
        return;
    }

    let mut kept = compiled_files();
    let held: usize = kept.values().map(|entry| entry.cost).sum();

    if kept.len() >= COMPILED_FILES || held.saturating_add(cost) > COMPILED_COST {
        kept.clear();
    }

    kept.insert(
        file.to_path_buf(),
        Compiled {
            mtime_ns: stamp.0,
            size: stamp.1,
            text_len,
            cost,
            added,
            dropped,
            rules: rules.clone(),
        },
    );
}

/// The text of an ignore file: a regular file of at most [`MAX_IGNORE_BYTES`] inside `root`, opened without
/// following a link. `None` for anything else.
fn read_ignore_text(root: &Path, file: &Path) -> Option<String> {
    let handle = open_checked(root, file)?;
    let mut bytes = Vec::new();

    handle.take(MAX_IGNORE_BYTES + 1).read_to_end(&mut bytes).ok()?;

    if bytes.len() as u64 > MAX_IGNORE_BYTES {
        return None;
    }

    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Opens `path` for reading when it is a regular file with one name, of at most [`MAX_FILE_BYTES`] or
/// [`MAX_IGNORE_BYTES`] (the larger), inside `root`; a link at the last component is refused.
fn open_checked(root: &Path, path: &Path) -> Option<File> {
    let handle = open_nofollow(path).ok()?;
    let metadata = handle.metadata().ok()?;

    if !metadata.is_file() || maybe_linked(&metadata) || metadata.len() > MAX_FILE_BYTES.max(MAX_IGNORE_BYTES) {
        return None;
    }

    is_inside(root, path, &handle, &metadata).then_some(handle)
}

#[cfg(unix)]
fn open_nofollow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    // O_NONBLOCK: a pipe swapped in after the check does not block the open
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(windows)]
fn open_nofollow(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    // FILE_FLAG_OPEN_REPARSE_POINT: a link is opened itself (and then refused as not a regular file)
    OpenOptions::new().read(true).custom_flags(0x0020_0000).open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_nofollow(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

/// True when the file behind the open handle lives under `root`, whatever the folders on the way were
/// swapped for after the walk.
fn is_inside(root: &Path, path: &Path, handle: &File, metadata: &Metadata) -> bool {
    let Some(real) = real_path(path, handle, metadata) else {
        return false;
    };

    real.starts_with(root) || fs::canonicalize(root).is_ok_and(|canonical| real.starts_with(canonical))
}

/// The real path of an open file: where the descriptor points on Linux, else the canonical path of `path`
/// when it names the very file that is open.
fn real_path(path: &Path, handle: &File, metadata: &Metadata) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        if let Ok(real) = fs::read_link(format!("/proc/self/fd/{}", handle.as_raw_fd())) {
            return Some(real);
        }
    }

    let _ = handle;
    let canonical = fs::canonicalize(path).ok()?;
    let seen = fs::metadata(&canonical).ok()?;

    same_file(&seen, metadata).then_some(canonical)
}

/// True when `seen` and `opened` describe the same file: the same device and inode where the platform says
/// so. Windows has none in std (see "Deviations" in `docs/plans/code-graph.md`), where size and modification
/// time are the best check left.
fn same_file(seen: &Metadata, opened: &Metadata) -> bool {
    match (identity(seen), identity(opened)) {
        (Some(left), Some(right)) => left == right,
        _ => seen.len() == opened.len() && seen.modified().ok() == opened.modified().ok(),
    }
}

/// True for a folder the walk does not enter.
fn excluded_dir(entry: &Candidate) -> bool {
    let Some(name) = entry.name.to_str() else {
        return false;
    };

    if EXCLUDED_DIRS.contains(&name) {
        return true;
    }

    OUTPUT_DIRS.contains(&name)
        && (entry.depth == 1
            || PROJECT_MANIFESTS
                .iter()
                .any(|manifest| entry.parent.join(manifest).is_file()))
}

/// The stored form of a path below the root: UTF-8, components joined by `/`.
fn relative_key(rel: &Path) -> Option<String> {
    let text = rel.to_str()?;

    Some(if cfg!(windows) {
        with_slashes(text)
    } else {
        text.to_string()
    })
}

/// `src\lib\a.rs` -> `src/lib/a.rs`.
pub(super) fn with_slashes(path: &str) -> String {
    path.replace('\\', "/")
}

/// What the file `rel` is to the index, if anything.
fn classify(rel: &str) -> Option<Class> {
    let name = rel.rsplit('/').next().unwrap_or(rel);

    if matches!(name, "Cargo.toml" | "go.mod") || is_ts_config(name) {
        return Some(Class::Package);
    }

    extract::spec_for(rel).map(|spec| Class::Source(spec.name))
}

/// `tsconfig.json`, `tsconfig.app.json`, `jsconfig.json`, ...
pub(super) fn is_ts_config(name: &str) -> bool {
    (name.starts_with("tsconfig") || name.starts_with("jsconfig")) && name.ends_with(".json")
}

/// Modification time in nanoseconds since the Unix epoch (negative before it).
fn mtime_ns(metadata: &Metadata) -> i64 {
    let Ok(modified) = metadata.modified() else {
        return 0;
    };

    match modified.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_nanos()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_nanos()).map_or(i64::MIN, |nanos| -nanos),
    }
}

#[cfg(unix)]
fn identity(metadata: &Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    Some((metadata.dev(), metadata.ino()))
}

/// Windows has a volume serial number and a file index, but std keeps them behind an unstable feature
/// (`windows_by_handle`), and this crate forbids `unsafe`, which a call to the system would need.
#[cfg(not(unix))]
fn identity(_metadata: &Metadata) -> Option<(u64, u64)> {
    None
}

/// The device a file lives on (Unix). Elsewhere a folder of another volume is a reparse point, which the walk
/// does not enter either.
fn device(metadata: &Metadata) -> Option<u64> {
    identity(metadata).map(|(device, _)| device)
}

/// True when the file may have a second name, which could lead out of the repository: it has several, or
/// the system does not say (a link count of 0, as some file systems report). Windows has no link count in
/// std (`number_of_links` is unstable, see [`identity`]), so a hard link there is not noticed.
#[cfg(unix)]
fn maybe_linked(metadata: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    metadata.nlink() != 1
}

#[cfg(not(unix))]
fn maybe_linked(_metadata: &Metadata) -> bool {
    false
}

/// Why a file was not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// It could not be opened or read.
    Unreadable,
    /// Its text is not UTF-8.
    NotUtf8,
    /// Over the size cap, not a regular file, or not the file the walk saw.
    Unsafe,
    /// A source file a tool wrote: a bundle of other sources, generated or minified code (see `generated`).
    Generated,
}

/// The outcome of [`read_text`].
#[derive(Debug, PartialEq, Eq)]
pub enum Read {
    Text(String),
    Skipped(Skip),
}

/// Reads the file of `entry`, which the walk of `root` found, as UTF-8 (a leading byte order mark is dropped).
pub fn read_text(root: &Path, entry: &Entry) -> Read {
    let abs = root.join(&entry.rel);
    let Ok(file) = open_nofollow(&abs) else {
        return Read::Skipped(Skip::Unreadable);
    };
    let Ok(metadata) = file.metadata() else {
        return Read::Skipped(Skip::Unreadable);
    };

    if !metadata.is_file() || maybe_linked(&metadata) || metadata.len() > MAX_FILE_BYTES {
        return Read::Skipped(Skip::Unsafe);
    }

    if entry.identity.is_some() && entry.identity != identity(&metadata) {
        return Read::Skipped(Skip::Unsafe);
    }

    if !is_inside(root, &abs, &file, &metadata) {
        return Read::Skipped(Skip::Unsafe);
    }

    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));

    if file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).is_err() {
        return Read::Skipped(Skip::Unreadable);
    }

    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Read::Skipped(Skip::Unsafe);
    }

    let text = match String::from_utf8(bytes) {
        Ok(text) => match text.strip_prefix('\u{feff}') {
            Some(stripped) => stripped.to_string(),
            None => text,
        },
        Err(_) => return Read::Skipped(Skip::NotUtf8),
    };

    // a bundle of the sources would show every symbol twice (manifests are read as they are)
    if matches!(entry.class, Class::Source(_)) && generated::generated(&text) {
        return Read::Skipped(Skip::Generated);
    }

    Read::Text(text)
}

/// The longest line of a file, in bytes, that [`read_line`] gives back.
pub const MAX_LINE_BYTES: usize = 2_048;

/// Line `line` (counting from 1) of the file `rel` below `root`, with its line break and the spaces around it
/// cut off, or `None`: when the file is not exactly what the index holds (`stamp_mtime` and `size` are its stamp
/// there), when the line is not there or is longer than [`MAX_LINE_BYTES`], or when the file is not safe to
/// read (opened without following a link, a regular file of one name and at most 1 MB, inside `root`). Only
/// the line asked for is kept, and a lossy decoding stands in for text that is not UTF-8.
pub fn read_line(root: &Path, rel: &str, stamp_mtime: i64, size: i64, line: i64) -> Option<String> {
    read_lines(root, rel, stamp_mtime, size, line, 0)?.pop()
}

/// Line `line` of the file `rel` with up to `before` lines above it (fewer at the top of the file), in order,
/// the wanted line last. The same checks and limits as [`read_line`] apply to every line returned.
pub fn read_lines(
    root: &Path,
    rel: &str,
    stamp_mtime: i64,
    size: i64,
    line: i64,
    before: usize,
) -> Option<Vec<String>> {
    use std::io::BufRead as _;

    let wanted = usize::try_from(line).ok().filter(|line| *line >= 1)?;
    let abs = root.join(rel);
    let file = open_nofollow(&abs).ok()?;
    let metadata = file.metadata().ok()?;

    if !metadata.is_file() || maybe_linked(&metadata) || metadata.len() > MAX_FILE_BYTES {
        return None;
    }

    if mtime_ns(&metadata) != stamp_mtime || i64::try_from(metadata.len()).ok()? != size {
        return None;
    }

    if !is_inside(root, &abs, &file, &metadata) {
        return None;
    }

    let mut reader = std::io::BufReader::new(file.take(MAX_FILE_BYTES + 1));
    let mut text = Vec::new();
    let first = wanted.saturating_sub(before).max(1);

    // the lines before the first one asked for are read past, one at a time and not kept
    for _ in 1..first {
        text.clear();

        if reader.read_until(b'\n', &mut text).ok()? == 0 || !text.ends_with(b"\n") {
            return None;
        }
    }

    let mut lines = Vec::with_capacity(wanted - first + 1);

    for number in first..=wanted {
        text.clear();

        // one byte more than the limit says the line is too long
        let mut limited = reader.by_ref().take(MAX_LINE_BYTES as u64 + 1);

        // the end of the file: there is no such line
        if limited.read_until(b'\n', &mut text).ok()? == 0 {
            return None;
        }

        if text.last() == Some(&b'\n') {
            text.pop();
        } else if number < wanted {
            return None;
        }

        if text.len() > MAX_LINE_BYTES {
            return None;
        }

        let decoded = String::from_utf8_lossy(&text);
        let decoded = decoded.strip_prefix('\u{feff}').unwrap_or(&decoded);

        lines.push(decoded.trim().to_owned());
    }

    Some(lines)
}

impl Skipped {
    /// Counts a file that [`read_text`] left out.
    pub fn count(&mut self, skip: Skip) {
        match skip {
            Skip::Unreadable => self.unreadable += 1,
            Skip::NotUtf8 => self.not_utf8 += 1,
            Skip::Unsafe => self.unsafe_or_large += 1,
            Skip::Generated => self.generated += 1,
        }
    }
}

mod generated;

#[cfg(all(test, unix))]
mod hostile;

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn never() -> bool {
        false
    }

    fn rels(walked: &Walked) -> Vec<&str> {
        walked.entries.iter().map(|e| e.rel.as_str()).collect()
    }

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn windows_separators_become_slashes() {
        assert_eq!(with_slashes(r"src\code\walk.rs"), "src/code/walk.rs");
        assert_eq!(with_slashes("already/fine.rs"), "already/fine.rs");
        assert_eq!(with_slashes(""), "");
    }

    #[test]
    fn an_empty_folder_walks_to_nothing() {
        let dir = root();
        let walked = walk(dir.path(), &never).expect("walk");

        assert!(walked.entries.is_empty());
        assert_eq!(walked.skipped, Skipped::default());
    }

    #[test]
    fn an_ignore_file_compiled_by_an_earlier_walk_is_used_again_until_it_changes() {
        let dir = root();

        fs::write(dir.path().join(".gitignore"), "ignored.rs\n").expect("write");
        fs::write(dir.path().join("ignored.rs"), "fn a() {}").expect("write");
        fs::write(dir.path().join("kept.rs"), "fn b() {}").expect("write");

        for _ in 0..2 {
            assert_eq!(rels(&walk(dir.path(), &never).expect("walk")), vec!["kept.rs"]);
        }

        // another size (the stamp the compiled rules are kept under) is another file
        fs::write(dir.path().join(".gitignore"), "kept.rs\n\n\n").expect("write");

        assert_eq!(rels(&walk(dir.path(), &never).expect("walk")), vec!["ignored.rs"]);
    }

    #[test]
    fn compiled_rules_that_weigh_more_than_the_cache_budget_are_not_kept_but_still_apply() {
        let dir = root();
        // lines of a thousand characters with 60 wildcards: about 250 KiB of text that compiles to megabytes
        let line = format!("{}\n", "*abcdefghijklmnop".repeat(60));
        let rules = format!("ignored.rs\n{}", line.repeat(240));

        fs::write(dir.path().join(".gitignore"), rules).expect("write");
        fs::write(dir.path().join("ignored.rs"), "fn a() {}").expect("write");
        fs::write(dir.path().join("kept.rs"), "fn b() {}").expect("write");

        assert_eq!(rels(&walk(dir.path(), &never).expect("walk")), vec!["kept.rs"]);
        assert!(!compiled_files().contains_key(&dir.path().join(".gitignore")));
    }

    #[test]
    fn a_line_left_out_of_an_ignore_file_counts_in_every_walk_whether_the_rules_were_compiled_or_reused() {
        let dir = root();
        let long = "*".repeat(MAX_IGNORE_WILDCARDS + 1);
        let wide = format!("{}\n", "a".repeat(MAX_IGNORE_LINE + 1));

        fs::write(dir.path().join(".gitignore"), format!("{long}\nignored.rs\n")).expect("write");
        fs::create_dir_all(dir.path().join("sub")).expect("mkdir");
        fs::write(dir.path().join("sub/.gitignore"), wide).expect("write");
        fs::write(dir.path().join("ignored.rs"), "fn a() {}").expect("write");
        fs::write(dir.path().join("kept.rs"), "fn b() {}").expect("write");

        for _ in 0..2 {
            let walked = walk(dir.path(), &never).expect("walk");

            // the pattern of too many wildcards and the line of too many bytes are dropped, the rest holds
            assert_eq!(rels(&walked), vec!["kept.rs"]);
            assert_eq!(walked.skipped.unsafe_or_large, 2, "one per file with a line left out");
        }
    }

    #[test]
    fn a_folder_that_is_not_a_git_repository_still_honours_gitignore() {
        let dir = root();
        fs::write(dir.path().join(".gitignore"), "ignored.rs\n").expect("write");
        fs::write(dir.path().join("ignored.rs"), "fn a() {}").expect("write");
        fs::write(dir.path().join("kept.rs"), "fn b() {}").expect("write");
        fs::create_dir_all(dir.path().join("node_modules/x")).expect("mkdir");
        fs::write(dir.path().join("node_modules/x/index.js"), "x").expect("write");
        fs::create_dir_all(dir.path().join("src/vendor")).expect("mkdir");
        fs::write(dir.path().join("src/vendor/lib.go"), "package v").expect("write");
        fs::write(dir.path().join("src/main.go"), "package main").expect("write");
        fs::write(dir.path().join("notes.md"), "# notes").expect("write");

        let walked = walk(dir.path(), &never).expect("walk");

        assert_eq!(rels(&walked), vec!["kept.rs", "src/main.go"]);
    }

    #[test]
    fn package_files_and_sources_are_classified() {
        let dir = root();

        for name in [
            "Cargo.toml",
            "go.mod",
            "tsconfig.json",
            "tsconfig.base.json",
            "jsconfig.json",
            "a.ts",
            "b.svelte",
        ] {
            fs::write(dir.path().join(name), "x").expect("write");
        }

        let walked = walk(dir.path(), &never).expect("walk");
        let classes: Vec<(&str, Class)> = walked.entries.iter().map(|e| (e.rel.as_str(), e.class)).collect();

        assert!(classes.contains(&("Cargo.toml", Class::Package)));
        assert!(classes.contains(&("tsconfig.base.json", Class::Package)));
        assert!(classes.contains(&("jsconfig.json", Class::Package)));
        assert!(classes.contains(&("a.ts", Class::Source("ts"))));
        assert!(classes.contains(&("b.svelte", Class::Source("ts"))));
    }

    #[test]
    fn a_file_over_one_megabyte_is_left_out() {
        let dir = root();
        fs::write(dir.path().join("small.rs"), "fn a() {}").expect("write");
        fs::write(dir.path().join("big.rs"), vec![b'a'; 1_000_001]).expect("write");

        let walked = walk(dir.path(), &never).expect("walk");

        assert_eq!(rels(&walked), vec!["small.rs"]);
    }

    #[test]
    fn the_read_refuses_what_grew_past_the_cap_after_the_walk() {
        let dir = root();
        let path = dir.path().join("grow.rs");
        fs::write(&path, "fn a() {}").expect("write");
        let entry = walk(dir.path(), &never).expect("walk").entries.remove(0);

        fs::write(&path, vec![b'a'; 1_000_001]).expect("write");

        assert_eq!(read_text(dir.path(), &entry), Read::Skipped(Skip::Unsafe));
    }

    #[test]
    fn text_that_is_not_utf8_is_skipped_and_a_byte_order_mark_is_dropped() {
        let dir = root();
        fs::write(dir.path().join("bad.rs"), [0xff, 0xfe, 0x00, b'a']).expect("write");
        fs::write(dir.path().join("bom.rs"), "\u{feff}fn a() {}").expect("write");

        let walked = walk(dir.path(), &never).expect("walk");
        let bad = walked.entries.iter().find(|e| e.rel == "bad.rs").expect("bad");
        let bom = walked.entries.iter().find(|e| e.rel == "bom.rs").expect("bom");

        assert_eq!(read_text(dir.path(), bad), Read::Skipped(Skip::NotUtf8));
        assert_eq!(read_text(dir.path(), bom), Read::Text("fn a() {}".into()));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed_loops_and_outside_targets_included() {
        use std::os::unix::fs::symlink;

        let outside = root();
        fs::write(outside.path().join("secret.rs"), "fn secret() {}").expect("write");
        fs::create_dir_all(outside.path().join("deep")).expect("mkdir");
        fs::write(outside.path().join("deep/more.rs"), "fn more() {}").expect("write");

        let dir = root();
        fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        fs::write(dir.path().join("src/real.rs"), "fn real() {}").expect("write");
        symlink(dir.path(), dir.path().join("src/loop")).expect("loop");
        symlink(outside.path().join("secret.rs"), dir.path().join("src/linked.rs")).expect("file link");
        symlink(outside.path(), dir.path().join("linked_dir")).expect("dir link");

        let walked = walk(dir.path(), &never).expect("walk");

        assert_eq!(rels(&walked), vec!["src/real.rs"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_swapped_for_another_after_the_walk_is_not_read() {
        let dir = root();
        let path = dir.path().join("a.rs");
        let other = dir.path().join("b.rs");
        fs::write(&path, "fn a() {}").expect("write");
        let entry = walk(dir.path(), &never).expect("walk").entries.remove(0);

        fs::write(&other, "fn b() {}").expect("write");
        fs::rename(&other, &path).expect("swap");

        assert_eq!(read_text(dir.path(), &entry), Read::Skipped(Skip::Unsafe));
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_skipped_not_fatal() {
        use std::os::unix::fs::PermissionsExt;

        let dir = root();
        let path = dir.path().join("locked.rs");
        fs::write(&path, "fn a() {}").expect("write");
        fs::write(dir.path().join("ok.rs"), "fn b() {}").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod");

        let walked = walk(dir.path(), &never).expect("walk");
        let locked = walked.entries.iter().find(|e| e.rel == "locked.rs").expect("locked");
        let outcome = read_text(dir.path(), locked);

        // a superuser can read anything
        if File::open(&path).is_err() {
            assert_eq!(outcome, Read::Skipped(Skip::Unreadable));
        }

        assert_eq!(rels(&walked), vec!["locked.rs", "ok.rs"]);
    }

    #[test]
    fn the_caps_fail_with_a_clear_error() {
        let dir = root();

        for n in 0..5 {
            fs::write(dir.path().join(format!("f{n}.rs")), "fn a() {}").expect("write");
        }

        let few_files = Limits {
            walked: 100,
            indexed: 4,
            ..Limits::default()
        };
        assert_eq!(walk_with(dir.path(), &never, few_files).unwrap_err(), TOO_LARGE);

        let few_entries = Limits {
            walked: 3,
            indexed: 100,
            ..Limits::default()
        };
        assert_eq!(walk_with(dir.path(), &never, few_entries).unwrap_err(), TOO_LARGE);

        let enough = Limits {
            walked: 100,
            indexed: 5,
            ..Limits::default()
        };
        assert_eq!(walk_with(dir.path(), &never, enough).expect("walk").entries.len(), 5);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_count_of_zero_or_two_is_not_safe_and_one_is() {
        let dir = root();
        let path = dir.path().join("a.rs");

        fs::write(&path, "fn a() {}").expect("write");

        let open = File::open(&path).expect("open");

        assert!(!maybe_linked(&open.metadata().expect("metadata")));

        fs::hard_link(&path, dir.path().join("b.rs")).expect("link");
        assert!(maybe_linked(&open.metadata().expect("metadata")));

        // a file that is gone from every folder but still open reports no names at all
        fs::remove_file(&path).expect("remove");
        fs::remove_file(dir.path().join("b.rs")).expect("remove");
        assert!(maybe_linked(&open.metadata().expect("metadata")));
    }

    #[test]
    fn a_stop_request_cancels_the_walk() {
        let dir = root();
        fs::write(dir.path().join("a.rs"), "fn a() {}").expect("write");

        assert_eq!(walk(dir.path(), &|| true).unwrap_err(), CANCELLED);
    }
}
