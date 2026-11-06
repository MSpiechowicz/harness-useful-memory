//! A listed folder that a `git pull` replaced by a tracked link turns on nothing, tried through the
//! MCP tools. The user turned on a submodule `U/sub`; upstream then commits a link `sub -> ../O`
//! at that path, and a default `git pull` exits 0 and leaves `U/sub` a link. The entry would
//! canonicalise to `O`, a repository the user never turned on, so an entry names something only
//! when it is its own canonical form. The desktop tests the same layouts
//! (`saving_root_links.rs` in `business/src/ecosystem/memory_guards/tests` of Harness Desktop): a
//! change to `LAYOUTS` or to a layout updates the other repository too.

#![cfg(unix)]

mod common;

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::guards::{Guarded, tool};
use common::mcp::canonical;
use common::opt_in::{global, kind, note, refusal, script};

/// Where upstream's link leads.
#[derive(Clone, Copy)]
enum Link {
    /// `sub -> ../O`, a relative target.
    Relative,
    /// `sub -> <O>`, an absolute target.
    Absolute,
    /// The entry is `U/libs/sub` and upstream turns the middle folder `libs` into `libs -> ../Op`,
    /// which holds the repository `sub`.
    MiddleFolder,
}

/// The names of the layouts, in step with the desktop's `LAYOUTS`.
const LAYOUTS: &[(&str, Link)] = &[
    ("relative", Link::Relative),
    ("absolute", Link::Absolute),
    ("middle-folder", Link::MiddleFolder),
];

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "protocol.file.allow=always"])
        .args(["-c", "user.name=Tests", "-c", "user.email=tests@example.com"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

fn repository(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path).expect("mkdir");
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["commit", "-q", "--allow-empty", "-m", "first"]);

    canonical(path)
}

/// A clone `clone` of `up` with the entry `entry` turned on for notes and scripts, and the
/// repository `off` the link will lead to, which is not.
struct Pulled {
    guarded: Guarded,
    up: PathBuf,
    clone: PathBuf,
    entry: PathBuf,
    off: PathBuf,
    link: Link,
}

impl Pulled {
    /// Upstream `up` holds the submodule `sub` (`libs/sub` for the middle folder), cloned to `U`.
    fn new(link: Link) -> Self {
        let guarded = Guarded::new();
        let base = canonical(guarded.dir.path());

        let source = repository(&base.join("source"));
        let up = repository(&base.join("up"));
        let at = match link {
            Link::MiddleFolder => "libs/sub",
            _ => "sub",
        };

        git(&up, &["submodule", "add", "-q", source.to_str().expect("text"), at]);
        git(&up, &["commit", "-q", "-m", "add the submodule"]);
        git(
            &base,
            &["clone", "-q", "--recurse-submodules", up.to_str().expect("text"), "U"],
        );

        // The repository the link will lead to: `O`, or `Op/sub` below a plain folder.
        let off = match link {
            Link::MiddleFolder => repository(&base.join("Op").join("sub")),
            _ => repository(&base.join("O")),
        };

        let clone = canonical(&base.join("U"));
        let entry = clone.join(at);
        let on = kind(false, false, &[&entry]);
        global(&guarded, &on, &on);

        Self {
            guarded,
            up,
            clone,
            entry,
            off,
            link,
        }
    }

    /// Upstream replaces the submodule (or the middle folder) by a tracked link, and `U` pulls it
    /// with a default `git pull`, which exits 0 and leaves the entry's path behind a link.
    fn pull_the_link(&self) {
        let target = match self.link {
            Link::Relative => PathBuf::from("../O"),
            Link::Absolute => self.off.clone(),
            Link::MiddleFolder => PathBuf::from("../Op"),
        };
        let (removed, replaced) = match self.link {
            Link::MiddleFolder => ("libs/sub", "libs"),
            _ => ("sub", "sub"),
        };

        git(&self.up, &["rm", "-qf", removed]);
        let _ = std::fs::remove_dir_all(self.up.join(replaced));
        symlink(&target, self.up.join(replaced)).expect("link");
        git(&self.up, &["add", replaced]);
        git(&self.up, &["commit", "-q", "-m", "a link where the submodule was"]);

        git(&self.clone, &["pull", "-q"]);

        let at = self.clone.join(replaced);
        assert!(
            std::fs::symlink_metadata(&at).expect("pulled").is_symlink(),
            "the pull left a link"
        );
    }

    /// The reason a save of a note and of a script from `folder` is refused; `None` for a save.
    fn refusals(&self, folder: &Path, title: &str) -> (Option<String>, Option<String>) {
        let mut server = self.guarded.server(None, folder);
        let path = serde_json::json!(folder.to_string_lossy());

        let mut saved_note = note(title);
        saved_note["project_path"] = path.clone();
        let mut saved_script = script("probe");
        saved_script["project_path"] = path;

        (
            refusal(&tool(&mut server, "note_save", saved_note)),
            refusal(&tool(&mut server, "script_save", saved_script)),
        )
    }
}

#[test]
fn a_listed_folder_that_a_pull_replaced_by_a_link_turns_on_nothing() {
    for (name, link) in LAYOUTS {
        let pulled = Pulled::new(*link);

        // Before the pull: the entry saves, the repository the link will lead to does not.
        assert_eq!(pulled.refusals(&pulled.entry, "Before"), (None, None), "{name}: entry");
        let off = Some("repo-off".to_owned());
        assert_eq!(
            pulled.refusals(&pulled.off, "Before"),
            (off.clone(), off.clone()),
            "{name}: off"
        );

        pulled.pull_the_link();

        // After it: the repository the link leads to is still off, and so is the link itself.
        assert_eq!(
            pulled.refusals(&pulled.off, "After"),
            (off.clone(), off.clone()),
            "{name}: off"
        );
        assert_eq!(
            pulled.refusals(&pulled.entry, "After"),
            (off.clone(), off),
            "{name}: link"
        );
    }
}
