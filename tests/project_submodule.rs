//! A submodule is keyed by its path and its remote is never read: an upstream change of
//! `.gitmodules`, brought in by `git submodule sync` or a fresh `clone --recurse-submodules`,
//! points the module's `origin` at another repository (the user's `off`), and the key of the
//! submodule must not become that repository's. Every other layout keeps the key it had.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::guards::{CHAT, Guarded, tool};
use common::mcp::{canonical, env};
use common::opt_in::{global, kind, note, refusal, script};
use harness_memory::project::ProjectResolver;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

const OFF_URL: &str = "https://example.invalid/acme/off.git";
const LIB_URL: &str = "https://example.invalid/acme/lib.git";
const INNER_URL: &str = "https://example.invalid/acme/inner.git";
const UP_URL: &str = "https://example.invalid/acme/up.git";

/// Runs git in `dir` with the fake URLs mapped to the folders in `srv` (no network).
fn git(dir: &Path, srv: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    command.args(["-c", "protocol.file.allow=always"]);
    command.args(["-c", "user.name=Tests", "-c", "user.email=tests@example.com"]);
    command.args(["-c", "init.defaultBranch=main"]);

    for (name, url) in [("off", OFF_URL), ("lib", LIB_URL), ("inner", INNER_URL), ("up", UP_URL)] {
        command.args(["-c", &format!("url.{}.insteadOf={url}", srv.join(name).display())]);
    }

    let status = command
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

fn origin(dir: &Path) -> String {
    let output = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(dir)
        .output()
        .expect("git runs");
    String::from_utf8(output.stdout).expect("utf-8").trim().to_owned()
}

/// A user with `off` (a note and a script saved in it), an upstream `up` that holds the submodule
/// `sub` (of `lib`, which holds the submodule `inner`), and clones of them in `work`.
struct Lab {
    guarded: Guarded,
    srv: PathBuf,
    work: PathBuf,
    before: Vec<String>,
}

impl Lab {
    fn new() -> Self {
        let guarded = Guarded::new();
        let base = canonical(guarded.dir.path());
        let (srv, work) = (base.join("srv"), base.join("work"));
        std::fs::create_dir_all(&work).expect("mkdir");

        for name in ["off", "lib", "inner", "up"] {
            let dir = srv.join(name);
            std::fs::create_dir_all(&dir).expect("mkdir");
            git(&dir, &srv, &["init", "-q"]);
            git(&dir, &srv, &["commit", "-q", "--allow-empty", "-m", "first"]);
        }

        git(&srv.join("lib"), &srv, &["submodule", "add", "-q", INNER_URL, "inner"]);
        git(&srv.join("lib"), &srv, &["commit", "-q", "-m", "add inner"]);

        // `off` also holds the commit of `lib` the superproject pins, so a fresh clone can fetch it
        // from the retargeted URL.
        git(&srv.join("off"), &srv, &["fetch", "-q", LIB_URL, "main:library"]);
        git(&srv.join("up"), &srv, &["submodule", "add", "-q", LIB_URL, "sub"]);
        git(&srv.join("up"), &srv, &["commit", "-q", "-m", "add sub"]);

        git(&work, &srv, &["clone", "-q", OFF_URL, "O"]);
        git(&work, &srv, &["clone", "-q", "--recurse-submodules", UP_URL, "U"]);

        let mut lab = Self {
            guarded,
            srv,
            work,
            before: Vec::new(),
        };

        // The user had `off` on and saved in it, then turned on only `U` and `U/sub`.
        let off = lab.off();
        let on = kind(false, false, &[&off]);
        global(&lab.guarded, &on, &on);
        let mut seed = lab.guarded.server(None, &off);
        let at = json!(off.to_string_lossy());
        let mut saved = note("Seeded");
        saved["project_path"] = at.clone();
        assert_eq!(refusal(&tool(&mut seed, "note_save", saved)), None);
        let mut saved = script("named");
        saved["project_path"] = at;
        assert_eq!(refusal(&tool(&mut seed, "script_save", saved)), None);

        let (up, sub) = (lab.up_clone(), lab.sub());
        lab.list(&[&up, &sub]);
        lab.before = lab.held_by_off();
        lab
    }

    fn off(&self) -> PathBuf {
        canonical(&self.work.join("O"))
    }

    fn up_clone(&self) -> PathBuf {
        canonical(&self.work.join("U"))
    }

    fn sub(&self) -> PathBuf {
        self.up_clone().join("sub")
    }

    fn git(&self, dir: &Path, args: &[&str]) {
        git(dir, &self.srv, args);
    }

    fn list(&self, entries: &[&Path]) {
        let mut listed = vec![self.guarded.repo.as_path()];
        listed.extend_from_slice(entries);

        let on = kind(false, false, &listed);
        global(&self.guarded, &on, &on);
    }

    /// Upstream names `off` as the URL of `sub`.
    fn retarget_upstream(&self) {
        let up = self.srv.join("up");
        self.git(&up, &["config", "-f", ".gitmodules", "submodule.sub.url", OFF_URL]);
        self.git(&up, &["add", "-A"]);
        self.git(&up, &["commit", "-q", "-m", "move the library"]);
    }

    /// What `off` holds as the user would read it.
    fn held_by_off(&self) -> Vec<String> {
        let mut reader = self.guarded.server(None, &self.guarded.repo);
        let at = json!(self.off().to_string_lossy());
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

    /// The write tools naming `project`, in the chat of the user's repository.
    fn attack(&self, project: &Path) {
        let root = json!({ "version": 1, "paused": false, "repoRoot": self.guarded.repo.to_string_lossy(),
                           "notes": { "on": true, "ask": false }, "scripts": { "on": true, "ask": false } });
        self.guarded.chat(CHAT, &root.to_string());
        let mut server = self.guarded.server(Some(CHAT), &self.guarded.repo);
        let at = json!(project.to_string_lossy());

        let mut saved_note = note("Through the submodule");
        saved_note["project_path"] = at.clone();
        let mut saved_script = script("named");
        saved_script["project_path"] = at.clone();

        let calls: Vec<(&str, Value)> = vec![
            ("note_save", saved_note),
            (
                "note_update",
                json!({ "id": 1, "body": "OVERWRITTEN through the submodule", "project_path": at }),
            ),
            ("note_delete", json!({ "id": 1, "project_path": at })),
            ("script_save", saved_script),
            ("script_delete", json!({ "name": "named", "project_path": at })),
        ];

        for (name, arguments) in calls {
            tool(&mut server, name, arguments);
        }
    }

    fn identity(&self, folder: &Path) -> Option<(String, PathBuf)> {
        let home = self.guarded.dir.path().join("home");
        ProjectResolver::new(env(&home), None)
            .resolve(Some(folder.to_str().expect("utf-8")))
            .expect("resolves")
            .map(|project| (project.key, project.root))
    }

    fn key(&self, folder: &Path) -> String {
        self.identity(folder).expect("a project").0
    }
}

#[test]
fn a_submodule_synced_to_another_repository_keeps_its_own_key_and_leaves_that_repository_alone() {
    let lab = Lab::new();
    let (up, sub) = (lab.up_clone(), lab.sub());
    let path_key = format!("path:{}", sub.display());
    assert_eq!(lab.key(&sub), path_key);

    lab.retarget_upstream();
    lab.git(&up, &["pull", "-q"]);
    lab.git(&up, &["submodule", "sync", "-q"]);
    assert_eq!(origin(&sub), OFF_URL, "the module's origin now names off");

    assert_eq!(lab.key(&sub), path_key);
    lab.attack(&sub);
    assert_eq!(lab.held_by_off(), lab.before);
}

#[test]
fn a_submodule_cloned_afresh_with_the_retargeted_url_keeps_its_own_key_and_leaves_that_repository_alone() {
    let lab = Lab::new();
    let (up, sub) = (lab.up_clone(), lab.sub());
    lab.retarget_upstream();

    std::fs::remove_dir_all(&up).expect("remove the clone");
    lab.git(&lab.work, &["clone", "-q", "--recurse-submodules", UP_URL, "U"]);
    assert_eq!(origin(&sub), OFF_URL, "the fresh module's origin names off");

    assert_eq!(lab.key(&sub), format!("path:{}", sub.display()));
    lab.attack(&sub);
    assert_eq!(lab.held_by_off(), lab.before);
}

#[test]
fn a_linked_worktree_of_a_submodule_and_a_submodule_of_a_submodule_are_keyed_by_their_paths() {
    let lab = Lab::new();
    let sub = lab.sub();
    lab.git(&sub, &["remote", "set-url", "origin", OFF_URL]);

    // A linked worktree of the submodule: git finds no superproject from there.
    lab.git(&sub, &["worktree", "add", "-q", "-b", "other", "../../linked"]);
    let linked = canonical(&lab.work.join("linked"));
    assert_eq!(
        lab.identity(&linked),
        Some((format!("path:{}", linked.display()), linked.clone()))
    );

    // A folder inside the worktree is the worktree's project too.
    std::fs::create_dir_all(linked.join("deep")).expect("mkdir");
    assert_eq!(lab.key(&linked.join("deep")), format!("path:{}", linked.display()));

    // A submodule of the submodule.
    lab.git(&sub, &["submodule", "update", "-q", "--init"]);
    let inner = canonical(&sub.join("inner"));
    lab.git(&inner, &["remote", "set-url", "origin", OFF_URL]);
    assert_eq!(
        lab.identity(&inner),
        Some((format!("path:{}", inner.display()), inner.clone()))
    );
}

#[test]
fn a_clone_that_is_not_a_submodule_keeps_its_remote_key_wherever_it_lies() {
    let lab = Lab::new();
    let (up, sub) = (lab.up_clone(), lab.sub());
    let key = "git:example.invalid/acme/off";

    // Cloned inside the superproject and inside the submodule, tracked by neither.
    for parent in [&up, &sub] {
        lab.git(parent, &["clone", "-q", OFF_URL, "nested"]);
        let nested = canonical(&parent.join("nested"));
        assert_eq!(
            lab.identity(&nested),
            Some((key.to_owned(), nested.clone())),
            "{}",
            nested.display()
        );
    }

    // A linked worktree of a plain clone, and the clone itself.
    let off = lab.off();
    lab.git(&off, &["worktree", "add", "-q", "-b", "wt", "../off-wt"]);
    let linked = canonical(&lab.work.join("off-wt"));
    assert_eq!(lab.identity(&off), Some((key.to_owned(), off)));
    assert_eq!(lab.identity(&linked), Some((key.to_owned(), linked)));
}

/// A folder whose git data is in a superproject's `.git/modules`, as a submodule leaves it after
/// upstream drops the gitlink: git reports no superproject, and its `origin` names another repository.
#[test]
fn a_leftover_submodule_is_keyed_by_its_path_when_git_finds_no_superproject() {
    let base = tempfile::tempdir().expect("tempdir");
    let home = base.path().join("home");
    std::fs::create_dir_all(&home).expect("home");

    let srv = canonical(base.path());
    let (up, source) = (srv.join("up"), srv.join("source"));

    for repo in [&up, &source] {
        std::fs::create_dir_all(repo).expect("mkdir");
        git(repo, &srv, &["init", "-q"]);
        git(repo, &srv, &["commit", "-q", "--allow-empty", "-m", "start"]);
    }

    git(
        &up,
        &srv,
        &["submodule", "add", "-q", source.to_str().expect("text"), "sub"],
    );
    git(&up, &srv, &["commit", "-q", "-m", "add the submodule"]);
    git(
        &up,
        &srv,
        &["config", "-f", ".git/modules/sub/config", "remote.origin.url", OFF_URL],
    );
    git(&up, &srv, &["rm", "-q", "--cached", "sub"]);
    git(&up, &srv, &["commit", "-q", "-m", "drop the gitlink"]);

    let leftover = canonical(&up.join("sub"));
    assert!(leftover.join(".git").is_file());

    let project = ProjectResolver::new(env(&home), None)
        .resolve(Some(leftover.to_str().expect("utf-8")))
        .expect("resolved")
        .expect("a project");

    assert_eq!(project.key, format!("path:{}", leftover.display()));
    assert_eq!(project.remote, None);
}
