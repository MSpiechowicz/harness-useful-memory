//! A tracked folder shaped like a bare repository (`HEAD`, `objects/`, `refs/`, `config`) is found
//! by git as a repository whose `core.worktree` and `remote.origin.url` the folder's author picks.
//! The project of such a folder must not be another repository: its root and its key come from one
//! repository whose work tree has a `.git`, and an entry the user turned on never leads a write
//! into a repository that is off. The layouts of genuine repositories keep the keys they had, since
//! notes are stored by key.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::guards::{CHAT, Guarded, tool};
use common::mcp::{canonical, env};
use common::opt_in::{global, kind, note, refusal, script};
use harness_memory::Server;
use harness_memory::project::ProjectResolver;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tempfile::TempDir;

/// The remote of the repository that is off.
const OFF_URL: &str = "https://github.com/acme/off.git";

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "protocol.file.allow=always"])
        .args(["-c", "user.name=Tests", "-c", "user.email=tests@example.com"])
        .args(["-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

/// A repository `path` with one commit and, when given, an `origin`.
fn repository(path: &Path, remote: Option<&str>) -> PathBuf {
    std::fs::create_dir_all(path).expect("mkdir");
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["commit", "-q", "--allow-empty", "-m", "first"]);

    if let Some(remote) = remote {
        git(path, &["remote", "add", "origin", remote]);
    }

    canonical(path)
}

/// Makes `dir` look like a bare repository whose configuration says the work tree is `worktree`
/// (relative to `dir`) and whose `origin` is `remote`.
fn plant(dir: &Path, worktree: &str, remote: Option<&str>) {
    for folder in ["objects/keep", "refs/keep"] {
        std::fs::create_dir_all(dir.join(folder)).expect("mkdir");
        std::fs::write(dir.join(folder).join("x"), "").expect("keep");
    }

    std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");

    let mut config = format!("[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tworktree = {worktree}\n");

    if let Some(remote) = remote {
        config.push_str(&format!("[remote \"origin\"]\n\turl = {remote}\n"));
    }

    std::fs::write(dir.join("config"), config).expect("config");
}

/// A user whose repository `off` (remote [`OFF_URL`]) holds a note and a script from the time it
/// was on, with saving then turned on only for `listed` entries.
struct World {
    guarded: Guarded,
    off: PathBuf,
    before: Vec<String>,
}

impl World {
    fn new() -> Self {
        let guarded = Guarded::new();
        let off = repository(&guarded.dir.path().join("off"), Some(OFF_URL));

        // The user had `off` on and saved in it.
        let on = kind(false, false, &[&off]);
        global(&guarded, &on, &on);
        let mut seed = guarded.server(None, &off);
        let at = json!(off.to_string_lossy());

        let mut saved = note("Seeded");
        saved["project_path"] = at.clone();
        assert_eq!(refusal(&tool(&mut seed, "note_save", saved)), None);

        let mut saved = script("named");
        saved["project_path"] = at;
        assert_eq!(refusal(&tool(&mut seed, "script_save", saved)), None);

        let mut world = Self {
            guarded,
            off,
            before: Vec::new(),
        };
        world.before = world.held_by_off();
        world
    }

    /// Turns saving on for `entries` and for the chat's own repository, for notes and scripts.
    fn list(&self, entries: &[&Path]) {
        let mut listed = vec![self.guarded.repo.as_path()];
        listed.extend_from_slice(entries);

        let on = kind(false, false, &listed);
        global(&self.guarded, &on, &on);
    }

    /// A server for the chat of the user's own repository, which lets it save.
    fn server(&self) -> Server {
        let root = json!({ "version": 1, "paused": false, "repoRoot": self.guarded.repo.to_string_lossy(),
                           "notes": { "on": true, "ask": false }, "scripts": { "on": true, "ask": false } });
        self.guarded.chat(CHAT, &root.to_string());
        self.guarded.server(Some(CHAT), &self.guarded.repo)
    }

    /// What `off` holds, read as the user would: its notes, the first note and its scripts.
    fn held_by_off(&self) -> Vec<String> {
        let mut reader = self.server();
        let at = json!(self.off.to_string_lossy());
        let reads = [
            (
                "note_search",
                json!({ "query": "cache", "scope": "repo", "project_path": at }),
            ),
            ("note_get", json!({ "ids": [1], "scope": "repo", "project_path": at })),
            ("script_list", json!({ "scope": "repo", "project_path": at })),
        ];

        reads
            .into_iter()
            .map(|(name, arguments)| common::mcp::text(&tool(&mut reader, name, arguments)).to_owned())
            .collect()
    }

    /// The five write tools naming `project`, the way an agent would call them.
    fn writes(project: &Path) -> Vec<(&'static str, Value)> {
        let at = json!(project.to_string_lossy());

        let mut saved_note = note("Through the entry");
        saved_note["project_path"] = at.clone();
        let mut saved_script = script("named");
        saved_script["project_path"] = at.clone();

        vec![
            ("note_save", saved_note),
            (
                "note_update",
                json!({ "id": 1, "body": "OVERWRITTEN through the entry", "project_path": at }),
            ),
            ("note_delete", json!({ "id": 1, "project_path": at })),
            ("script_save", saved_script),
            ("script_delete", json!({ "name": "named", "project_path": at })),
        ]
    }

    /// The refusal of each write tool naming `project` (`None` for a call that was let through).
    fn refusals(&self, project: &Path) -> Vec<Option<String>> {
        let mut server = self.server();

        Self::writes(project)
            .into_iter()
            .map(|(name, arguments)| refusal(&tool(&mut server, name, arguments)))
            .collect()
    }

    /// Calls each write tool naming `project` and drops the answers (a tool may find nothing to
    /// change there).
    fn try_writes(&self, project: &Path) {
        let mut server = self.server();

        for (name, arguments) in Self::writes(project) {
            tool(&mut server, name, arguments);
        }
    }

    /// Nothing of `off` changed since the user left it.
    fn assert_off_untouched(&self, label: &str) {
        assert_eq!(
            self.held_by_off(),
            self.before,
            "{label}: the repository that is off changed"
        );
    }
}

fn resolve(home: &Path, folder: &Path) -> Option<harness_memory::store::ProjectKey> {
    ProjectResolver::new(env(home), None)
        .resolve(Some(folder.to_str().expect("utf-8")))
        .expect("resolves")
}

#[test]
fn a_listed_folder_shaped_like_a_bare_repository_is_not_another_repository() {
    for (label, worktree) in [("dot", "."), ("same-folder", "../sub")] {
        let world = World::new();
        let entry = canonical(&{
            let dir = world.guarded.dir.path().join("U").join("sub");
            std::fs::create_dir_all(&dir).expect("mkdir");
            dir
        });
        plant(&entry, worktree, Some(OFF_URL));
        world.list(&[&entry]);

        let project = resolve(&world.guarded.dir.path().join("home"), &entry).expect("a project");
        assert_eq!(project.key, format!("path:{}", entry.display()), "{label}");
        assert_eq!(project.root, entry, "{label}");
        assert_eq!(project.remote, None, "{label}");

        world.try_writes(&entry);
        world.assert_off_untouched(label);
    }
}

#[test]
fn a_shaped_folder_below_a_listed_entry_cannot_borrow_the_key_of_the_parent_that_is_off() {
    let world = World::new();

    // `off` is a repository, so a folder `off/L` is part of it. The user listed `off/L` back when
    // `off` was no repository, and the work tree `..` of `off/L/fake` is that listed folder.
    let entry = world.off.join("L");
    std::fs::create_dir_all(&entry).expect("mkdir");
    let fake = entry.join("fake");
    std::fs::create_dir_all(&fake).expect("mkdir");
    plant(&fake, "..", None);
    world.list(&[&canonical(&entry)]);

    let home = world.guarded.dir.path().join("home");
    let project = resolve(&home, &fake).expect("a project");
    assert_eq!(project.key, format!("path:{}", canonical(&fake).display()));
    assert_eq!(project.remote, None);

    let refused = Some("repo-off".to_owned());
    assert_eq!(world.refusals(&fake), vec![refused; 5]);
    world.assert_off_untouched("fake");
}

#[test]
fn a_tracked_folder_that_replaced_a_submodule_cannot_reach_another_repository_in_a_fresh_clone() {
    let world = World::new();
    let base = canonical(world.guarded.dir.path());

    // Upstream holds the submodule `sub`, cloned to `U`, and the user turned on `U/sub`.
    let source = repository(&base.join("source"), None);
    let up = repository(&base.join("up"), None);
    git(&up, &["submodule", "add", "-q", source.to_str().expect("text"), "sub"]);
    git(&up, &["commit", "-q", "-m", "add the submodule"]);
    git(
        &base,
        &["clone", "-q", "--recurse-submodules", up.to_str().expect("text"), "U"],
    );
    let entry = canonical(&base.join("U").join("sub"));
    world.list(&[&entry]);

    // Upstream swaps the submodule for a tracked folder shaped like a repository, and the user
    // clones afresh to the same place (a default pull or a submodule deinit leads to the same).
    git(&up, &["rm", "-q", "-f", "sub"]);
    let _ = std::fs::remove_dir_all(up.join("sub"));
    std::fs::create_dir_all(up.join("sub")).expect("mkdir");
    plant(&up.join("sub"), ".", Some(OFF_URL));
    std::fs::write(up.join("sub").join("README"), "vendored").expect("readme");
    git(&up, &["add", "-A", "-f"]);
    git(&up, &["commit", "-q", "-m", "vendor the library"]);

    std::fs::remove_dir_all(base.join("U")).expect("remove the clone");
    git(&base, &["clone", "-q", up.to_str().expect("text"), "U"]);
    assert!(entry.join("config").is_file(), "the clone holds the shaped folder");

    let project = resolve(&base.join("home"), &entry).expect("a project");
    assert!(project.key.starts_with("path:"), "{}", project.key);

    world.try_writes(&entry);
    world.assert_off_untouched("fresh clone");
}

/// The key and the root of `folder` for a user whose home is `home`.
fn identity(home: &Path, folder: &Path) -> (String, PathBuf) {
    let project = resolve(home, folder).expect("a project");
    (project.key, project.root)
}

#[test]
fn the_layouts_of_real_repositories_keep_their_keys() {
    let dir = TempDir::new().expect("temp");
    let base = canonical(dir.path());
    let home = base.join("home");
    std::fs::create_dir_all(&home).expect("home");
    let key = "git:github.com/acme/widget";
    let url = "https://github.com/acme/widget.git";

    // A plain checkout, and a folder inside it.
    let plain = repository(&base.join("plain"), Some(url));
    std::fs::create_dir_all(plain.join("src/deep")).expect("mkdir");
    assert_eq!(
        identity(&home, &plain.join("src/deep")),
        (key.to_owned(), plain.clone())
    );

    // A linked worktree: its `.git` is a file.
    git(&plain, &["worktree", "add", "-q", "../linked"]);
    let linked = canonical(&base.join("linked"));
    assert!(linked.join(".git").is_file());
    assert_eq!(identity(&home, &linked), (key.to_owned(), linked.clone()));

    // A real submodule: a `.git` file into the superproject's `modules`. It is keyed by its path
    // whatever its remote says (see `project_submodule.rs`).
    let source = repository(&base.join("source"), None);
    let up = repository(&base.join("up"), None);
    git(&up, &["submodule", "add", "-q", source.to_str().expect("text"), "sub"]);
    git(&up, &["commit", "-q", "-m", "add the submodule"]);
    git(
        &base,
        &["clone", "-q", "--recurse-submodules", up.to_str().expect("text"), "U"],
    );
    let submodule = canonical(&base.join("U").join("sub"));
    git(&submodule, &["remote", "set-url", "origin", url]);
    assert!(submodule.join(".git").is_file());
    assert_eq!(
        identity(&home, &submodule),
        (format!("path:{}", submodule.display()), submodule.clone())
    );

    // A checkout whose git folder lives elsewhere (`--separate-git-dir`).
    let separate = base.join("separate");
    std::fs::create_dir_all(&separate).expect("mkdir");
    git(
        &separate,
        &["init", "-q", "-b", "main", "--separate-git-dir", "../separate.git"],
    );
    git(&separate, &["remote", "add", "origin", url]);
    let separate = canonical(&separate);
    assert!(separate.join(".git").is_file());
    assert_eq!(identity(&home, &separate), (key.to_owned(), separate.clone()));

    // A linked worktree of a bare repository.
    let bare = base.join("hub.git");
    git(
        &base,
        &["clone", "-q", "--bare", plain.to_str().expect("text"), "hub.git"],
    );
    git(&bare, &["remote", "set-url", "origin", url]);
    git(&bare, &["worktree", "add", "-q", "../hubwork", "main"]);
    let hubwork = canonical(&base.join("hubwork"));
    assert_eq!(identity(&home, &hubwork), (key.to_owned(), hubwork.clone()));
}

#[test]
fn a_genuine_bare_repository_is_a_project_by_its_path_never_by_its_remote() {
    let dir = TempDir::new().expect("temp");
    let base = canonical(dir.path());
    let home = base.join("home");
    std::fs::create_dir_all(&home).expect("home");

    let origin = repository(&base.join("origin"), None);
    git(
        &base,
        &["clone", "-q", "--bare", origin.to_str().expect("text"), "hub.git"],
    );
    let bare = canonical(&base.join("hub.git"));
    git(
        &bare,
        &["remote", "set-url", "origin", "https://github.com/acme/widget.git"],
    );

    for folder in [bare.clone(), bare.join("refs")] {
        let (key, root) = identity(&home, &folder);
        assert_eq!(key, format!("path:{}", folder.display()), "{}", folder.display());
        assert_eq!(root, folder);
    }
}

#[test]
fn a_shaped_folder_inside_a_listed_repository_does_not_take_the_remote_it_names() {
    let world = World::new();
    let fake = world.guarded.repo.join("vendor").join("fake");
    std::fs::create_dir_all(&fake).expect("mkdir");
    plant(&fake, "..", Some(OFF_URL));
    world.list(&[]);

    let project = resolve(&world.guarded.dir.path().join("home"), &fake).expect("a project");
    assert_eq!(project.key, format!("path:{}", canonical(&fake).display()));
    assert_eq!(project.remote, None);

    world.try_writes(&fake);
    world.assert_off_untouched("fake in a repository");
}

#[test]
fn an_included_configuration_file_cannot_change_the_key() {
    let dir = TempDir::new().expect("temp");
    let base = canonical(dir.path());
    let home = base.join("home");
    std::fs::create_dir_all(&home).expect("home");

    // A tracked file (it arrives with a pull) that names another remote, included from `.git/config`.
    let planted = base.join("tracked.cfg");
    std::fs::write(&planted, format!("[remote \"origin\"]\n\turl = {OFF_URL}\n")).expect("file");
    let include = |repo: &Path| {
        git(
            repo,
            &["config", "--local", "include.path", planted.to_str().expect("text")],
        )
    };

    // The remote of the repository wins, and a repository with none stays keyed by its path.
    let plain = repository(&base.join("plain"), Some("https://example.invalid/acme/widget.git"));
    include(&plain);
    assert_eq!(
        identity(&home, &plain),
        ("git:example.invalid/acme/widget".to_owned(), plain.clone())
    );

    let bare_remote = repository(&base.join("none"), None);
    include(&bare_remote);
    assert_eq!(
        identity(&home, &bare_remote),
        (format!("path:{}", bare_remote.display()), bare_remote.clone())
    );

    // A linked worktree reads the configuration shared with its repository.
    git(&plain, &["worktree", "add", "-q", "../linked"]);
    let linked = canonical(&base.join("linked"));
    assert_eq!(
        identity(&home, &linked),
        ("git:example.invalid/acme/widget".to_owned(), linked)
    );
}
