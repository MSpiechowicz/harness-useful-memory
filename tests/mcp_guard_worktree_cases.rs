//! Which folders count as a linked worktree of the repository saving is on for, tried through the
//! MCP tools. The desktop tests the same list (`saving_root_cases.rs` in
//! `business/src/ecosystem/memory_guards/tests` of Harness Desktop), so both give the same answer
//! for each case: a change to `CASES`, to a case's layout or to its answer updates the other
//! repository too.
//!
//! The answer of a case ([`Maps`]) is where the folder maps to: the main repository, the git folder
//! of a repository made with a separate git dir, a bare repository's folder, or the folder itself.
//! Those three folders are listed (and two more, `listed` and `plain`, which some cases name), so a
//! save is allowed unless the folder maps to itself. Real
//! worktrees, the separate and the bare repository and the submodule are made by git.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::guards::{CHAT, Guarded, structured, tool};
use common::opt_in::{global, global_note, kind, note, refusal, script};
use pretty_assertions::assert_eq;
use serde_json::json;

/// Where a case's folder maps to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Maps {
    /// The main repository (`main`).
    Main,
    /// The git folder `separate.git` of `git init --separate-git-dir`.
    Separate,
    /// The folder `bare.git` of a bare repository.
    Bare,
    /// The folder itself: it is not believed to be a worktree.
    Itself,
}

/// A case name, and where the folder maps to.
///
/// Keep in step with `CASES` of Harness Desktop's `saving_root_cases.rs`.
const CASES: &[(&str, Maps)] = &[
    ("main", Maps::Main),
    ("worktree-absolute", Maps::Main),
    ("worktree-relative", Maps::Main),
    ("forged-admin-inside", Maps::Itself),
    ("forged-admin-elsewhere", Maps::Itself),
    ("two-lines", Maps::Itself),
    ("crlf", Maps::Main),
    ("oversize", Maps::Itself),
    ("at-the-cap", Maps::Main),
    ("missing-commondir", Maps::Itself),
    ("bad-back-link", Maps::Itself),
    ("submodule", Maps::Itself),
    ("separate-worktree", Maps::Separate),
    ("separate-main", Maps::Itself),
    ("forged-git-folder", Maps::Itself),
    ("bare-worktree", Maps::Bare),
    ("listed-folder-names-off-checkout", Maps::Itself),
    ("forged-admin-in-listed-work-tree", Maps::Itself),
    ("not-a-git-folder", Maps::Itself),
];

/// The largest `.git` file read, in bytes (the same bound on both sides).
const CAP: usize = 16 * 1024;

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(["-c", "protocol.file.allow=always"])
        .args(["-c", "user.name=Tests", "-c", "user.email=tests@example.com"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs")
        .success()
}

fn git_ok(dir: &Path, args: &[&str]) {
    assert!(git(dir, args), "git {args:?}");
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).expect("write");
}

/// The repository saving is on for, with one commit so it can have worktrees.
fn main_repository(guarded: &Guarded) -> PathBuf {
    git_ok(&guarded.repo, &["commit", "-q", "--allow-empty", "-m", "start"]);
    guarded.repo.clone()
}

/// The linked worktree `name` of `main` next to it, and its admin folder.
fn linked(guarded: &Guarded, main: &Path, name: &str) -> (PathBuf, PathBuf) {
    git_ok(main, &["worktree", "add", "-q", "-b", name, &format!("../{name}")]);

    (
        common::mcp::canonical(&guarded.dir.path().join(name)),
        main.join(".git").join("worktrees").join(name),
    )
}

/// The folder of case `name`.
fn folder(name: &str, guarded: &Guarded, main: &Path) -> PathBuf {
    let base = guarded.dir.path();
    let shown = main.join(".git");

    match name {
        "main" => main.to_path_buf(),

        "worktree-absolute" => linked(guarded, main, name).0,

        // `git worktree add --relative-paths` (git 2.48 and later) writes these paths. An older
        // git gets the same layout by hand.
        "worktree-relative" => {
            let target = format!("../{name}");

            if git(
                main,
                &["worktree", "add", "-q", "--relative-paths", "-b", name, &target],
            ) {
                return common::mcp::canonical(&base.join(name));
            }

            let (folder, admin) = linked(guarded, main, name);
            write(
                &folder.join(".git"),
                &format!("gitdir: ../widget/.git/worktrees/{name}\n"),
            );
            write(&admin.join("gitdir"), &format!("../../../../{name}/.git\n"));
            write(&admin.join("commondir"), "../..\n");
            folder
        }

        // The folder holds everything: its own admin folder that points at the main repository.
        "forged-admin-inside" => {
            let folder = base.join(name);
            std::fs::create_dir_all(folder.join("admin")).expect("mkdir");
            write(&folder.join(".git"), "gitdir: admin\n");
            write(&folder.join("admin").join("gitdir"), "../.git\n");
            write(
                &folder.join("admin").join("commondir"),
                &format!("{}\n", shown.display()),
            );
            common::mcp::canonical(&folder)
        }

        // An admin folder outside the main repository's `worktrees` folder, with a good back-link.
        "forged-admin-elsewhere" => {
            let folder = base.join(name);
            let admin = base.join("elsewhere-admin");
            std::fs::create_dir_all(&folder).expect("mkdir");
            std::fs::create_dir_all(&admin).expect("mkdir");
            let folder = common::mcp::canonical(&folder);
            let admin = common::mcp::canonical(&admin);
            write(&folder.join(".git"), &format!("gitdir: {}\n", admin.display()));
            write(&admin.join("gitdir"), &format!("{}\n", folder.join(".git").display()));
            write(&admin.join("commondir"), &format!("{}\n", shown.display()));
            folder
        }

        "two-lines" => {
            let (folder, _) = linked(guarded, main, name);
            let first = std::fs::read_to_string(folder.join(".git")).expect("read");
            write(&folder.join(".git"), &format!("{first}gitdir: elsewhere\n"));
            folder
        }

        // Windows line ends in the `.git` file and in both files of the admin folder.
        "crlf" => {
            let (folder, admin) = linked(guarded, main, name);
            write(&folder.join(".git"), &format!("gitdir: {}\r\n", admin.display()));
            write(&admin.join("gitdir"), &format!("{}\r\n", folder.join(".git").display()));
            write(&admin.join("commondir"), &format!("{}\r\n", shown.display()));
            folder
        }

        // One byte over the cap, and exactly at it, padded with line ends that are trimmed.
        "oversize" | "at-the-cap" => {
            let (folder, admin) = linked(guarded, main, name);
            let line = format!("gitdir: {}", admin.display());
            let size = if name == "oversize" { CAP + 1 } else { CAP };
            write(
                &folder.join(".git"),
                &format!("{line}{}", "\n".repeat(size - line.len())),
            );
            folder
        }

        "missing-commondir" => {
            let (folder, admin) = linked(guarded, main, name);
            std::fs::remove_file(admin.join("commondir")).expect("remove");
            folder
        }

        "bad-back-link" => {
            let (folder, admin) = linked(guarded, main, name);
            write(&admin.join("gitdir"), "/nowhere/.git\n");
            folder
        }

        // A submodule: its `.git` file names `.git/modules/<name>`, which has no `commondir`.
        "submodule" => {
            let source = base.join("source");
            std::fs::create_dir(&source).expect("mkdir");
            git_ok(&source, &["init", "-q"]);
            git_ok(&source, &["commit", "-q", "--allow-empty", "-m", "start"]);

            git_ok(main, &["submodule", "add", "-q", source.to_str().expect("text"), name]);
            common::mcp::canonical(&main.join(name))
        }

        // A worktree of a repository whose git folder is kept elsewhere: `separate/.git` names
        // `separate.git`, which does not name `separate`. The main checkout is its own root.
        "separate-worktree" | "separate-main" => {
            let checkout = separate(base);

            if name == "separate-main" {
                return checkout;
            }

            git_ok(&checkout, &["worktree", "add", "-q", "-b", name, &format!("../{name}")]);
            common::mcp::canonical(&base.join(name))
        }

        // A folder whose `.git` file names the separate git folder, with no back-link.
        "forged-git-folder" => {
            separate(base);
            let folder = base.join(name);
            std::fs::create_dir_all(&folder).expect("mkdir");
            write(
                &folder.join(".git"),
                &format!("gitdir: {}\n", base.join("separate.git").display()),
            );
            common::mcp::canonical(&folder)
        }

        // A worktree of a bare repository: its common folder is the bare folder.
        "bare-worktree" => {
            let bare = base.join("bare.git");
            let origin = main.to_str().expect("text");
            git_ok(base, &["clone", "-q", "--bare", origin, bare.to_str().expect("text")]);
            git_ok(&bare, &["worktree", "add", "-q", "-b", name, &format!("../{name}")]);
            common::mcp::canonical(&base.join(name))
        }

        // The checkout `off` is not listed. The listed folder `listed` has a `.git` file that
        // names it (and so does not make `off` a git folder).
        "listed-folder-names-off-checkout" => {
            let off = base.join("off");
            std::fs::create_dir_all(&off).expect("mkdir");
            git_ok(&off, &["init", "-q"]);
            let off = common::mcp::canonical(&off);

            std::fs::create_dir_all(base.join("listed")).expect("mkdir");
            write(
                &base.join("listed").join(".git"),
                &format!("gitdir: {}\n", off.display()),
            );
            off
        }

        // `main` is listed and holds a `worktrees/y` folder with both link files as ordinary
        // content. The folder's `.git` names it, so the common folder is the listed work tree.
        "forged-admin-in-listed-work-tree" => {
            let folder = base.join(name);
            std::fs::create_dir_all(&folder).expect("mkdir");
            let folder = common::mcp::canonical(&folder);
            let admin = main.join("worktrees").join("y");
            std::fs::create_dir_all(&admin).expect("mkdir");
            write(&admin.join("commondir"), "../..\n");
            write(&admin.join("gitdir"), &format!("{}\n", folder.join(".git").display()));
            write(&folder.join(".git"), &format!("gitdir: {}\n", admin.display()));
            folder
        }

        // A `.git` file with a good back-link whose common folder is the plain folder `plain`,
        // which is listed: it is shaped like no git folder.
        "not-a-git-folder" => {
            let folder = base.join(name);
            let admin = base.join("plain").join("worktrees").join("z");
            std::fs::create_dir_all(&folder).expect("mkdir");
            std::fs::create_dir_all(&admin).expect("mkdir");
            let folder = common::mcp::canonical(&folder);
            write(&admin.join("commondir"), "../..\n");
            write(&admin.join("gitdir"), &format!("{}\n", folder.join(".git").display()));
            write(
                &folder.join(".git"),
                &format!("gitdir: {}\n", common::mcp::canonical(&admin).display()),
            );
            folder
        }

        other => panic!("unknown case {other}"),
    }
}

/// The main checkout `separate` of a repository whose git folder is `separate.git`, made once.
fn separate(base: &Path) -> PathBuf {
    let checkout = base.join("separate");

    if !checkout.exists() {
        let git_dir = base.join("separate.git");
        let arguments = ["init", "-q", "--separate-git-dir", git_dir.to_str().expect("text")];
        git_ok(base, &[&arguments[..], &[checkout.to_str().expect("text")]].concat());
        git_ok(&checkout, &["commit", "-q", "--allow-empty", "-m", "start"]);
    }

    common::mcp::canonical(&checkout)
}

#[test]
fn every_case_maps_to_the_main_repository_or_to_itself() {
    let guarded = Guarded::new();
    let main = main_repository(&guarded);
    let base = common::mcp::canonical(guarded.dir.path());
    let listed = kind(
        false,
        false,
        &[
            &main,
            &base.join("separate.git"),
            &base.join("bare.git"),
            &base.join("listed"),
            &base.join("plain"),
        ],
    );

    // Every folder exists before the entries are pinned, as they do when the user turned them on.
    let folders: Vec<PathBuf> = CASES.iter().map(|(name, _)| folder(name, &guarded, &main)).collect();
    global(&guarded, &listed, &listed);

    for ((name, maps), folder) in CASES.iter().zip(&folders) {
        let mut server = guarded.server(None, folder);

        let reason = refusal(&tool(&mut server, "note_save", note(name)));
        assert_eq!(reason.is_none(), *maps != Maps::Itself, "{name}: {reason:?}");
    }
}

#[test]
fn a_forged_folder_saves_nothing_for_the_enabled_repository() {
    let guarded = Guarded::new();
    let main = main_repository(&guarded);
    global(&guarded, &kind(false, false, &[&main]), &kind(false, false, &[&main]));

    // The control: the enabled repository saves a script, and the user's rules trust it.
    let mut control = guarded.server(None, &main);
    assert_eq!(refusal(&tool(&mut control, "script_save", script("control"))), None);
    let got = tool(&mut control, "script_get", json!({ "name": "control" }));
    assert_eq!(structured(&got)["script"]["approval"], "local");

    for name in ["forged-admin-inside", "forged-admin-elsewhere", "forged-git-folder"] {
        let forged = folder(name, &guarded, &main);
        let mut server = guarded.server(None, &forged);

        for (what, reply) in [
            ("repo note", tool(&mut server, "note_save", note(name))),
            ("global note", tool(&mut server, "note_save", global_note(name))),
            ("script", tool(&mut server, "script_save", script(name))),
        ] {
            assert_eq!(refusal(&reply).as_deref(), Some("repo-off"), "{name}: {what}");
        }

        let list = tool(&mut server, "script_list", json!({}));
        assert!(!common::mcp::text(&list).contains(name), "{name}: nothing was saved");

        // A chat whose guard file names the forged folder as its root.
        let chat = json!({ "version": 1, "paused": false, "repoRoot": forged.to_string_lossy(),
                           "notes": { "on": true, "ask": false }, "scripts": { "on": true, "ask": false } });
        guarded.chat(CHAT, &chat.to_string());
        let mut chat_server = guarded.server(Some(CHAT), &forged);
        let reply = tool(&mut chat_server, "note_save", note(&format!("{name} chat")));
        assert_eq!(refusal(&reply).as_deref(), Some("repo-off"), "{name}: chat");
    }
}

/// Listing the main checkout of a repository with a separate git folder turns on its worktrees,
/// and nothing that merely names the git folder.
#[test]
fn listing_a_checkout_with_a_separate_git_folder_turns_on_its_worktrees_only() {
    let guarded = Guarded::new();
    let main = main_repository(&guarded);
    let checkout = folder("separate-main", &guarded, &main);
    let worktree = folder("separate-worktree", &guarded, &main);
    let forged = folder("forged-git-folder", &guarded, &main);
    let other = folder("worktree-absolute", &guarded, &main);

    let listed = kind(false, false, &[&checkout]);
    global(&guarded, &listed, &listed);

    // The checkout is on by its path, its worktree through its git folder.
    for (what, path, saves) in [
        ("checkout", &checkout, true),
        ("worktree", &worktree, true),
        ("forged", &forged, false),
        ("unrelated", &other, false),
    ] {
        let mut server = guarded.server(None, path);
        let reason = refusal(&tool(&mut server, "note_save", note(what)));
        assert_eq!(reason.is_none(), saves, "{what}: {reason:?}");

        let reason = refusal(&tool(&mut server, "script_save", script(what)));
        assert_eq!(reason.is_none(), saves, "{what} script: {reason:?}");
    }

    // A chat whose root is the forged folder, with `project_path` naming the checkout, is refused.
    let chat = json!({ "version": 1, "paused": false, "repoRoot": forged.to_string_lossy(),
                       "notes": { "on": true, "ask": false }, "scripts": { "on": true, "ask": false } });
    guarded.chat(CHAT, &chat.to_string());
    let mut server = guarded.server(Some(CHAT), &checkout);
    let reason = refusal(&tool(&mut server, "note_save", note("chat")));
    assert_eq!(reason.as_deref(), Some("repo-off"));
}

/// The git folder listed (what the desktop stores when a worktree is added) turns on every
/// worktree of the repository but not the checkout.
#[test]
fn a_listed_git_folder_turns_on_the_worktrees_but_not_the_checkout() {
    let guarded = Guarded::new();
    let main = main_repository(&guarded);
    let base = common::mcp::canonical(guarded.dir.path());
    let checkout = folder("separate-main", &guarded, &main);
    let worktree = folder("separate-worktree", &guarded, &main);

    let listed = kind(false, false, &[&base.join("separate.git")]);
    global(&guarded, &listed, &listed);

    for (what, path, saves) in [("worktree", &worktree, true), ("checkout", &checkout, false)] {
        let mut server = guarded.server(None, path);
        let reason = refusal(&tool(&mut server, "note_save", note(what)));
        assert_eq!(reason.is_none(), saves, "{what}: {reason:?}");
    }
}

/// A folder listed for notes and scripts, with nothing else listed, cannot vouch for another
/// repository: the cases that try it save nothing, for a note and for a script.
#[test]
fn a_listed_folder_cannot_vouch_for_another_repository() {
    let guarded = Guarded::new();
    let main = main_repository(&guarded);

    // S1: the listed `listed` has a `.git` file that names `widget`, an off checkout.
    let listed_folder = guarded.dir.path().join("listed");
    std::fs::create_dir_all(&listed_folder).expect("mkdir");
    write(&listed_folder.join(".git"), &format!("gitdir: {}\n", main.display()));
    let listed_folder = common::mcp::canonical(&listed_folder);
    let only = kind(false, false, &[&listed_folder]);
    global(&guarded, &only, &only);

    let mut server = guarded.server(None, &main);
    assert_eq!(
        refusal(&tool(&mut server, "note_save", note("Named checkout"))).as_deref(),
        Some("repo-off")
    );
    assert_eq!(
        refusal(&tool(&mut server, "script_save", script("named-checkout"))).as_deref(),
        Some("repo-off")
    );
    let list = tool(&mut server, "script_list", json!({}));
    assert!(!common::mcp::text(&list).contains("named-checkout"));

    // S2: `widget` is listed and holds `worktrees/y` with both link files as ordinary content;
    // another folder's `.git` names it.
    let forged = folder("forged-admin-in-listed-work-tree", &guarded, &main);
    let only = kind(false, false, &[&main]);
    global(&guarded, &only, &only);

    let mut server = guarded.server(None, &forged);
    assert_eq!(
        refusal(&tool(&mut server, "note_save", note("Forged admin"))).as_deref(),
        Some("repo-off")
    );
    assert_eq!(
        refusal(&tool(&mut server, "script_save", script("forged-admin"))).as_deref(),
        Some("repo-off")
    );
    let list = tool(&mut server, "script_list", json!({}));
    assert!(!common::mcp::text(&list).contains("forged-admin"));

    // The listed work tree itself still saves, and so does its real worktree.
    let mut control = guarded.server(None, &main);
    assert_eq!(refusal(&tool(&mut control, "script_save", script("control"))), None);
    let got = tool(&mut control, "script_get", json!({ "name": "control" }));
    assert_eq!(structured(&got)["script"]["approval"], "local");
    let worktree = folder("worktree-absolute", &guarded, &main);
    let mut server = guarded.server(None, &worktree);
    assert_eq!(refusal(&tool(&mut server, "note_save", note("Real worktree"))), None);
}

/// A named pipe at `path`: opening it for reading waits for a writer.
#[cfg(unix)]
fn fifo(path: &Path) {
    let made = Command::new("mkfifo").arg(path).status().expect("mkfifo runs");
    assert!(made.success(), "mkfifo {}", path.display());
}

/// A named pipe in place of a file the worktree check reads returns at once and leaves the root as
/// it is.
#[cfg(unix)]
#[test]
fn a_named_pipe_in_place_of_a_link_file_cannot_hang_the_server() {
    use std::sync::Arc;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    for file in ["gitdir", "commondir"] {
        let guarded = Arc::new(Guarded::new());
        let main = main_repository(&guarded);
        global(&guarded, &kind(false, false, &[&main]), &common::opt_in::off());

        // A folder whose admin folder is made by hand (git itself reads a real one, and waits on
        // the pipe), with the other file as it should be.
        let folder = common::mcp::canonical(&guarded.work());
        let admin = folder.join("admin");
        std::fs::create_dir(&admin).expect("mkdir");
        write(&folder.join(".git"), "gitdir: admin\n");
        write(&admin.join("gitdir"), "../.git\n");
        write(&admin.join("commondir"), &format!("{}\n", main.join(".git").display()));
        std::fs::remove_file(admin.join(file)).expect("remove");
        fifo(&admin.join(file));

        let (sender, receiver) = channel();
        let worker = Arc::clone(&guarded);

        // The call runs on a thread so a hang fails the test instead of stopping it.
        std::thread::spawn(move || {
            let mut server = worker.server(None, &folder);
            let reason = refusal(&tool(&mut server, "note_save", note("Fifo")));
            let _ = sender.send(reason);
        });

        let reason = receiver
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("a named pipe as {file} hangs the server"));
        assert_eq!(reason.as_deref(), Some("repo-off"), "{file}");
    }
}
