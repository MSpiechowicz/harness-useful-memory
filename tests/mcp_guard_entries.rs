//! The entries of `onRepos` are read as they are written, as Harness Desktop reads them: an entry
//! names something only when it is its own canonical form. One with a link in it, or whose folder
//! is gone, names nothing: it turns on neither the folder it leads to nor anything else (see
//! `mcp_guard_links.rs` for one a `git pull` brought).

mod common;

use common::guards::{Guarded, tool};
use common::mcp::{canonical, git};
use common::opt_in::{global, kind, note, off, refusal, with_pins};
use pretty_assertions::assert_eq;

#[cfg(unix)]
#[test]
fn an_entry_that_is_a_link_turns_on_nothing() {
    let guarded = Guarded::new();
    let entry = guarded.dir.path().join("entry-link");
    std::os::unix::fs::symlink(&guarded.repo, &entry).expect("link");

    global(&guarded, &kind(false, false, &[&entry]), &off());

    // Neither the folder the link leads to nor the link itself.
    for folder in [&guarded.repo, &entry] {
        let mut server = guarded.server(None, folder);
        assert_eq!(
            refusal(&tool(&mut server, "note_save", note("Through an entry link"))).as_deref(),
            Some("repo-off"),
            "{}",
            folder.display()
        );
    }

    // The same folder listed as itself saves.
    global(&guarded, &kind(false, false, &[&guarded.repo]), &off());
    let mut listed = guarded.server(None, &guarded.repo);
    assert_eq!(refusal(&tool(&mut listed, "note_save", note("Listed as itself"))), None);
}

#[test]
fn an_entry_whose_folder_is_gone_turns_on_nothing_until_it_is_itself_again() {
    let guarded = Guarded::new();
    let ghost = canonical(guarded.dir.path()).join("ghost");

    global(&guarded, &kind(false, false, &[&ghost]), &off());

    // The folder comes back as itself, but the entry has no pin (there was no folder to ask): it
    // names nothing until the user turns it on again.
    std::fs::create_dir_all(&ghost).expect("mkdir");
    git(&ghost, &["init", "-q"]);
    let mut server = guarded.server(None, &ghost);
    assert_eq!(
        refusal(&tool(&mut server, "note_save", note("Back with no pin"))).as_deref(),
        Some("repo-off")
    );

    // Turned on again, with its key: the entry names the folder.
    let pinned = with_pins(
        &kind(false, false, &[&ghost]),
        &[(&ghost, Some(&format!("path:{}", ghost.display())))],
    );
    global(&guarded, &pinned, &off());
    let mut server = guarded.server(None, &ghost);
    assert_eq!(refusal(&tool(&mut server, "note_save", note("Back as itself"))), None);
}
