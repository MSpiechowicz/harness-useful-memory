//! Repository identity: git remotes normalize to one key without credentials, other folders are
//! keyed by path, and the home folder or a filesystem root is no project.

mod common;

use std::path::PathBuf;

use common::mcp::{canonical, env, git, repo_with_remote};
use harness_memory::project::{ProjectResolver, normalize_remote};
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[test]
fn https_with_credentials_and_scp_remotes_share_one_key() {
    let home = TempDir::new().expect("home");
    let work = TempDir::new().expect("work");
    let https = repo_with_remote(work.path(), "a", "https://bob:ghp_s3cret@GitHub.com/Acme/widget.git/");
    let scp = repo_with_remote(work.path(), "b", "git@github.com:Acme/widget.git");

    let mut resolver = ProjectResolver::new(env(home.path()), Some(https.join("src/deep")));
    let from_https = resolver.resolve(None).expect("resolves").expect("a project");
    let from_scp = resolver
        .resolve(Some(scp.to_str().expect("utf-8")))
        .expect("resolves")
        .expect("a project");

    assert_eq!(from_https.key, "git:github.com/Acme/widget");
    assert_eq!(from_scp.key, from_https.key);
    assert_eq!(from_https.root, canonical(&https), "the work tree, not the subfolder");
    assert_eq!(from_https.remote.as_deref(), Some("github.com/Acme/widget"));

    let shown = format!("{from_https:?}");
    assert!(!shown.contains("bob") && !shown.contains("s3cret"), "{shown}");
}

#[test]
fn a_folder_outside_git_is_keyed_by_its_path() {
    let home = TempDir::new().expect("home");
    let work = TempDir::new().expect("work");
    let plain = work.path().join("notes");
    std::fs::create_dir(&plain).expect("mkdir");

    let mut resolver = ProjectResolver::new(env(home.path()), Some(plain.clone()));
    let project = resolver.resolve(None).expect("resolves").expect("a project");

    assert_eq!(project.key, format!("path:{}", canonical(&plain).display()));
    assert_eq!(project.remote, None);
}

#[test]
fn a_repository_without_a_remote_is_keyed_by_its_work_tree() {
    let home = TempDir::new().expect("home");
    let work = TempDir::new().expect("work");
    let dir = work.path().join("local");
    std::fs::create_dir_all(dir.join("sub")).expect("mkdir");
    git(&dir, &["init", "-q"]);

    let mut resolver = ProjectResolver::new(env(home.path()), Some(dir.join("sub")));
    let project = resolver.resolve(None).expect("resolves").expect("a project");
    assert_eq!(project.key, format!("path:{}", canonical(&dir).display()));
}

#[test]
fn the_home_folder_and_a_filesystem_root_are_no_project() {
    let home = TempDir::new().expect("home");

    let mut at_home = ProjectResolver::new(env(home.path()), Some(home.path().to_path_buf()));
    assert_eq!(at_home.resolve(None).expect("resolves"), None);

    let mut at_root = ProjectResolver::new(env(home.path()), Some(PathBuf::from("/")));
    assert_eq!(at_root.resolve(None).expect("resolves"), None);

    let mut nowhere = ProjectResolver::new(env(home.path()), None);
    assert_eq!(nowhere.resolve(None).expect("resolves"), None);
}

#[test]
fn project_path_must_be_an_absolute_existing_folder() {
    let home = TempDir::new().expect("home");
    let mut resolver = ProjectResolver::new(env(home.path()), Some(home.path().to_path_buf()));

    assert!(resolver.resolve(Some("relative/dir")).is_err());
    assert!(resolver.resolve(Some("/definitely/not/here/xyz")).is_err());

    let file = home.path().join("file.txt");
    std::fs::write(&file, "x").expect("write");
    assert!(resolver.resolve(Some(file.to_str().expect("utf-8"))).is_err());
}

#[test]
fn remotes_normalize_to_host_owner_repo() {
    let cases = [
        ("https://github.com/o/r.git", Some("github.com/o/r")),
        ("https://user:token@GITHUB.com/o/r", Some("github.com/o/r")),
        ("ssh://git@github.com:22/o/r.git", Some("github.com/o/r")),
        (
            "git@gitlab.example.com:group/sub/r.git",
            Some("gitlab.example.com/group/sub/r"),
        ),
        ("https://host/o/r/?ref=x#frag", Some("host/o/r")),
        ("/srv/git/r.git", None),
        ("./r", None),
        ("C:\\repos\\r", None),
        ("file:///srv/git/r.git", None),
        ("https://host", None),
        ("", None),
    ];

    for (raw, expected) in cases {
        assert_eq!(normalize_remote(raw).as_deref(), expected, "{raw}");
    }
}
