//! A repository is untrusted input: its ignore files, `.git` entry and links must not make the walk read or
//! hold anything outside the root, or more than a small cap.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use super::*;

fn never() -> bool {
    false
}

fn root() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

fn names(root: &Path) -> Vec<String> {
    walk(root, &never)
        .expect("walk")
        .entries
        .into_iter()
        .map(|entry| entry.rel)
        .collect()
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }

    fs::write(path, text).expect("write");
}

/// A file outside `root` whose patterns would hide `a.rs`, when anything read it.
fn outside_rules(base: &Path) -> std::path::PathBuf {
    let path = base.join("outside-rules");

    write(&path, "a.rs\n");

    path
}

#[test]
fn ignore_files_that_are_links_are_not_read() {
    let outside = root();
    let rules = outside_rules(outside.path());
    let dir = root();

    write(&dir.path().join("a.rs"), "fn a() {}");
    write(&dir.path().join("b/a.rs"), "fn a() {}");
    symlink(&rules, dir.path().join(".gitignore")).expect("link");
    symlink(&rules, dir.path().join(".ignore")).expect("link");
    symlink(&rules, dir.path().join("b/.gitignore")).expect("link");
    fs::create_dir_all(dir.path().join(".git/info")).expect("mkdir");
    symlink(&rules, dir.path().join(".git/info/exclude")).expect("link");

    let walked = walk(dir.path(), &never).expect("walk");
    let found: Vec<&str> = walked.entries.iter().map(|e| e.rel.as_str()).collect();

    assert_eq!(found, vec!["a.rs", "b/a.rs"]);
    assert_eq!(walked.skipped.unsafe_or_large, 4);
}

#[test]
fn an_ignore_file_that_is_a_link_to_a_device_ends_the_walk_not_the_process() {
    let dir = root();

    write(&dir.path().join("a.rs"), "fn a() {}");
    symlink("/dev/zero", dir.path().join(".gitignore")).expect("link");
    symlink("/dev/zero", dir.path().join(".ignore")).expect("link");
    fs::create_dir_all(dir.path().join(".git/info")).expect("mkdir");
    symlink("/dev/zero", dir.path().join(".git/info/exclude")).expect("link");

    assert_eq!(names(dir.path()), vec!["a.rs"]);
}

#[test]
fn an_ignore_file_over_the_cap_is_left_unread() {
    let dir = root();
    let mut big = String::from("a.rs\n");

    big.push_str(&"#".repeat(2_000_000));
    write(&dir.path().join(".gitignore"), &big);
    write(&dir.path().join("a.rs"), "fn a() {}");

    let walked = walk(dir.path(), &never).expect("walk");

    assert_eq!(walked.entries.len(), 1);
    assert_eq!(walked.skipped.unsafe_or_large, 1);
}

#[test]
fn an_ignore_file_with_many_patterns_costs_a_bounded_number() {
    let dir = root();
    let lines = "x*\n".repeat(300_000);

    write(&dir.path().join(".gitignore"), &lines);
    write(&dir.path().join("a.rs"), "fn a() {}");

    assert_eq!(names(dir.path()), vec!["a.rs"]);
}

#[test]
fn a_git_file_pointing_elsewhere_is_not_followed() {
    let outside = root();

    write(&outside.path().join("gitdir/info/exclude"), "a.rs\n");
    write(&outside.path().join("gitdir/commondir"), "../common\n");
    write(&outside.path().join("common/info/exclude"), "a.rs\n");

    let dir = root();

    write(
        &dir.path().join(".git"),
        &format!("gitdir: {}\n", outside.path().join("gitdir").display()),
    );
    write(&dir.path().join("a.rs"), "fn a() {}");

    assert_eq!(names(dir.path()), vec!["a.rs"]);
}

#[test]
fn the_git_folder_is_not_followed_when_it_is_a_link() {
    let outside = root();

    write(&outside.path().join("info/exclude"), "a.rs\n");

    let dir = root();

    symlink(outside.path(), dir.path().join(".git")).expect("link");
    write(&dir.path().join("a.rs"), "fn a() {}");

    assert_eq!(names(dir.path()), vec!["a.rs"]);
}

#[test]
fn a_git_info_folder_that_is_a_link_leads_nowhere() {
    let outside = root();

    write(&outside.path().join("exclude"), "a.rs\n");

    let dir = root();

    fs::create_dir_all(dir.path().join(".git")).expect("mkdir");
    symlink(outside.path(), dir.path().join(".git/info")).expect("link");
    write(&dir.path().join("a.rs"), "fn a() {}");

    assert_eq!(names(dir.path()), vec!["a.rs"]);
}

#[test]
fn the_exclude_file_of_a_real_git_folder_counts() {
    let dir = root();

    write(&dir.path().join(".git/info/exclude"), "a.rs\n");
    write(&dir.path().join("a.rs"), "fn a() {}");
    write(&dir.path().join("b.rs"), "fn b() {}");

    assert_eq!(names(dir.path()), vec!["b.rs"]);
}

#[test]
fn nested_rules_negation_and_folder_only_patterns_work_like_git() {
    let dir = root();

    write(&dir.path().join(".gitignore"), "*.gen.rs\nlogs/\n/top.rs\n");
    write(&dir.path().join("src/.gitignore"), "!keep.gen.rs\n/local.rs\n");
    write(&dir.path().join("top.rs"), "x");
    write(&dir.path().join("src/top.rs"), "x");
    write(&dir.path().join("src/local.rs"), "x");
    write(&dir.path().join("src/deep/local.rs"), "x");
    write(&dir.path().join("src/drop.gen.rs"), "x");
    write(&dir.path().join("src/keep.gen.rs"), "x");
    write(&dir.path().join("logs/a.rs"), "x");
    write(&dir.path().join("src/logs"), "a file called logs is not a folder");
    write(&dir.path().join("src/logs.rs"), "x");

    assert_eq!(
        names(dir.path()),
        vec!["src/deep/local.rs", "src/keep.gen.rs", "src/logs.rs", "src/top.rs"]
    );
}

#[test]
fn a_dot_ignore_file_beats_a_gitignore_of_the_same_folder() {
    let dir = root();

    write(&dir.path().join(".gitignore"), "a.rs\n");
    write(&dir.path().join(".ignore"), "!a.rs\nb.rs\n");
    write(&dir.path().join("a.rs"), "x");
    write(&dir.path().join("b.rs"), "x");
    write(&dir.path().join("c.rs"), "x");

    assert_eq!(names(dir.path()), vec!["a.rs", "c.rs"]);
}

#[test]
fn an_ignored_folder_cannot_be_brought_back_from_below() {
    let dir = root();

    write(&dir.path().join(".gitignore"), "out/\n");
    write(&dir.path().join("out/.gitignore"), "!a.rs\n");
    write(&dir.path().join("out/a.rs"), "x");

    assert!(names(dir.path()).is_empty());
}

#[test]
fn hidden_files_and_tool_folders_are_skipped_unless_a_rule_names_them() {
    let dir = root();

    write(&dir.path().join(".gitignore"), "!.next/\n.ignored/\n");
    write(&dir.path().join(".next/a.rs"), "x");
    write(&dir.path().join(".svelte-kit/a.rs"), "x");
    write(&dir.path().join(".ignored/a.rs"), "x");
    write(&dir.path().join(".hidden.rs"), "x");
    write(&dir.path().join("shown.rs"), "x");

    assert_eq!(names(dir.path()), vec![".next/a.rs", "shown.rs"]);
}

#[test]
fn hidden_folders_that_hold_source_are_indexed_except_tool_state() {
    let dir = root();

    for path in [
        ".vitepress/config.ts",
        ".storybook/main.ts",
        ".github/scripts/release.js",
        ".github/scripts/.hidden.js",
        ".vscode/tasks.ts",
        ".cache/a.ts",
        ".svelte-kit/a.ts",
        ".turbo/a.ts",
        "pkg/.next/a.ts",
        "src/main.ts",
    ] {
        write(&dir.path().join(path), "x");
    }

    assert_eq!(
        names(dir.path()),
        vec![
            ".github/scripts/release.js",
            ".storybook/main.ts",
            ".vitepress/config.ts",
            "src/main.ts"
        ]
    );
}

#[test]
fn a_file_with_two_names_is_left_out() {
    let outside = root();
    let target = outside.path().join("secret.rs");

    write(&target, "fn secret() {}");

    let dir = root();

    write(&dir.path().join("a.rs"), "fn a() {}");
    fs::hard_link(&target, dir.path().join("linked.rs")).expect("hard link");

    let walked = walk(dir.path(), &never).expect("walk");
    let found: Vec<&str> = walked.entries.iter().map(|e| e.rel.as_str()).collect();

    assert_eq!(found, vec!["a.rs"]);
    assert_eq!(walked.skipped.unsafe_or_large, 1);
}

#[test]
fn a_file_that_gained_a_second_name_after_the_walk_is_not_read() {
    let dir = root();
    let path = dir.path().join("a.rs");

    write(&path, "fn a() {}");

    let entry = walk(dir.path(), &never).expect("walk").entries.remove(0);

    fs::hard_link(&path, dir.path().join("b.rs")).expect("hard link");

    assert_eq!(read_text(dir.path(), &entry), Read::Skipped(Skip::Unsafe));
}

#[test]
fn a_folder_moved_out_and_replaced_by_a_link_is_not_read_through() {
    let outside = root();
    let dir = root();

    write(&dir.path().join("sub/a.rs"), "fn a() {}");

    let entry = walk(dir.path(), &never).expect("walk").entries.remove(0);
    let moved = outside.path().join("sub");

    fs::rename(dir.path().join("sub"), &moved).expect("move");
    symlink(&moved, dir.path().join("sub")).expect("link");

    // the very file the walk saw (same device and inode), but now outside the root
    assert_eq!(read_text(dir.path(), &entry), Read::Skipped(Skip::Unsafe));
}

#[test]
fn a_file_replaced_by_a_link_is_not_read() {
    let outside = root();
    let dir = root();
    let path = dir.path().join("a.rs");

    write(&path, "fn a() {}");
    write(&outside.path().join("b.rs"), "fn b() {}");

    let entry = walk(dir.path(), &never).expect("walk").entries.remove(0);

    fs::remove_file(&path).expect("remove");
    symlink(outside.path().join("b.rs"), &path).expect("link");

    assert_eq!(read_text(dir.path(), &entry), Read::Skipped(Skip::Unreadable));
}

/// An ignore file of exactly `size` bytes that hides `x.rs` (the rest is a comment).
fn rules_of(size: usize) -> String {
    let mut text = String::from("x.rs\n#");

    text.push_str(&"-".repeat(size - text.len() - 1));
    text.push('\n');

    text
}

#[test]
fn the_ignore_files_of_one_walk_are_read_up_to_a_total_and_the_rest_count_as_skipped() {
    let dir = root();

    for n in 0..10 {
        write(&dir.path().join(format!("d{n}/.gitignore")), &rules_of(40));
        write(&dir.path().join(format!("d{n}/x.rs")), "fn x() {}");
    }

    let limits = Limits {
        ignore_bytes: 100,
        ..Limits::default()
    };
    let walked = walk_with(dir.path(), &never, limits).expect("walk");
    let found: Vec<&str> = walked.entries.iter().map(|e| e.rel.as_str()).collect();

    // the first two files fit (80 bytes), the third would pass 100, and the others are not even opened
    assert_eq!(
        found,
        ["d2", "d3", "d4", "d5", "d6", "d7", "d8", "d9"].map(|d| format!("{d}/x.rs"))
    );
    assert_eq!(walked.skipped.unsafe_or_large, 8);
}

#[test]
fn a_single_huge_line_is_dropped_and_the_file_counts_as_partly_read() {
    let dir = root();

    // would hide q.rs if it were compiled
    write(
        &dir.path().join(".gitignore"),
        &format!("x.rs\n{{q.rs,{}}}\ny.rs\n", "z".repeat(5_000)),
    );

    for name in ["q.rs", "x.rs", "y.rs", "z.rs"] {
        write(&dir.path().join(name), "fn a() {}");
    }

    let walked = walk(dir.path(), &never).expect("walk");
    let found: Vec<&str> = walked.entries.iter().map(|e| e.rel.as_str()).collect();

    assert_eq!(found, ["q.rs", "z.rs"]);
    assert_eq!(walked.skipped.unsafe_or_large, 1);
}

#[test]
fn many_folders_with_a_sparse_ignore_file_each_are_read_for_a_bounded_total() {
    let dir = root();

    for n in 0..60 {
        let folder = dir.path().join(format!("d{n}"));

        write(&folder.join("f.rs"), "fn f() {}");

        // a megabyte of NUL bytes, which is one line of a million characters
        fs::File::create(folder.join(".gitignore"))
            .and_then(|file| file.set_len(1_000_000))
            .expect("sparse file");
    }

    let walked = walk(dir.path(), &never).expect("walk");

    assert_eq!(walked.entries.len(), 60);
    // every file was left out or its only line dropped: nothing was kept for the whole walk
    assert_eq!(walked.skipped.unsafe_or_large, 60);
}

#[test]
fn the_rules_of_folders_the_walk_has_left_are_not_kept() {
    let dir = root();

    for name in ["a", "b", "c"] {
        write(&dir.path().join(name).join(".gitignore"), "x.rs\n");
    }

    let ignores = Ignores::new(dir.path(), None, MAX_IGNORE_TOTAL);
    let kept = |ignores: &Ignores| {
        let mut keys: Vec<PathBuf> = ignores.chains.lock().expect("chains").keys().cloned().collect();

        keys.sort();

        keys
    };

    ignores.chain(&dir.path().join("a"));
    assert_eq!(kept(&ignores), [dir.path().to_path_buf(), dir.path().join("a")]);

    ignores.chain(&dir.path().join("b"));
    assert_eq!(kept(&ignores), [dir.path().to_path_buf(), dir.path().join("b")]);
}

#[test]
fn a_walk_past_its_deadline_ends_with_the_time_error() {
    let dir = root();

    for n in 0..20 {
        write(&dir.path().join(format!("d{n}/.gitignore")), &rules_of(100_000));
        write(&dir.path().join(format!("d{n}/f.rs")), "fn f() {}");
    }

    let deadline = Instant::now();

    std::thread::sleep(std::time::Duration::from_millis(5));

    let outcome = walk_until(dir.path(), &never, Some(deadline), Limits::default());

    assert_eq!(outcome.err().as_deref(), Some(TOO_SLOW));
}

#[test]
fn a_walk_with_time_left_is_not_cut() {
    let dir = root();

    write(&dir.path().join("d/f.rs"), "fn f() {}");

    let deadline = Instant::now() + std::time::Duration::from_secs(600);
    let walked = walk_until(dir.path(), &never, Some(deadline), Limits::default()).expect("walk");

    assert_eq!(walked.entries.len(), 1);
}

#[test]
fn every_dot_ignore_beats_every_gitignore_whatever_the_depth_as_in_ripgrep() {
    let dir = root();

    // the outer .ignore asks for b.rs back, the inner .gitignore (closer to the file) hides everything
    write(&dir.path().join(".ignore"), "!sub/b.rs\n");
    write(&dir.path().join("sub/.gitignore"), "*.rs\n");
    write(&dir.path().join("sub/a.rs"), "x");
    write(&dir.path().join("sub/b.rs"), "x");

    assert_eq!(names(dir.path()), vec!["sub/b.rs"]);
}

#[test]
fn a_gitignore_beats_the_exclude_file_and_a_closer_dot_ignore_beats_a_farther_one() {
    let dir = root();

    write(&dir.path().join(".git/info/exclude"), "a.rs\nb.rs\n");
    write(&dir.path().join(".gitignore"), "!a.rs\n");
    write(&dir.path().join(".ignore"), "c.rs\n!d.rs\n");
    write(&dir.path().join("sub/.ignore"), "!c.rs\nd.rs\n");

    for name in ["a.rs", "b.rs", "c.rs", "d.rs"] {
        write(&dir.path().join(name), "x");
        write(&dir.path().join("sub").join(name), "x");
    }

    assert_eq!(names(dir.path()), vec!["a.rs", "d.rs", "sub/a.rs", "sub/c.rs"]);
}

#[test]
fn a_folder_past_the_entry_cap_makes_the_repository_too_large_before_it_is_held() {
    let dir = root();

    for n in 0..11 {
        write(&dir.path().join(format!("wide/f{n}.txt")), "x");
    }

    write(&dir.path().join("a.rs"), "fn a() {}");

    let limits = |per_dir| Limits {
        per_dir,
        ..Limits::default()
    };

    assert_eq!(walk_with(dir.path(), &never, limits(10)).unwrap_err(), TOO_LARGE);
    // eleven entries in the folder are within a cap of eleven
    assert_eq!(
        walk_with(dir.path(), &never, limits(11)).expect("walk").entries.len(),
        1
    );
}

#[test]
fn entries_with_a_path_over_the_cap_and_folders_below_the_depth_cap_are_left_out_and_counted() {
    let dir = root();
    let long = "n".repeat(100);

    write(&dir.path().join("ok.rs"), "fn a() {}");
    write(&dir.path().join(format!("{long}/{long}/a.rs")), "fn a() {}");
    write(&dir.path().join("d1/d2/d3/deep.rs"), "fn a() {}");

    let limits = Limits {
        path_bytes: 150,
        depth: 2,
        ..Limits::default()
    };
    let walked = walk_with(dir.path(), &never, limits).expect("walk");

    // the long folder is entered, its long child folder is not, and d3 is below the depth cap
    assert_eq!(rels(&walked), ["ok.rs"]);
    assert_eq!(walked.skipped.unreadable, 2);
}

fn rels(walked: &Walked) -> Vec<&str> {
    walked.entries.iter().map(|entry| entry.rel.as_str()).collect()
}

#[test]
fn the_result_is_sorted_by_path_whatever_order_the_system_lists_a_folder_in() {
    let dir = root();

    for path in ["b.rs", "a0.rs", "a.rs", "a/b.rs", "a/a/z.rs", "c/a.rs"] {
        write(&dir.path().join(path), "fn a() {}");
    }

    assert_eq!(
        names(dir.path()),
        ["a/a/z.rs", "a/b.rs", "a.rs", "a0.rs", "b.rs", "c/a.rs"]
    );
}

#[test]
fn folders_below_the_ignore_chain_cap_do_not_add_ignore_files_and_count_as_skipped() {
    let dir = root();
    let mut folder = dir.path().to_path_buf();

    for n in 0..(MAX_IGNORE_CHAIN + 6) {
        folder.push(format!("d{n}"));
        write(&folder.join(".gitignore"), "# a comment\n");
    }

    // the last file, whose rules are beyond the cap, would hide hidden.rs
    write(&folder.join(".gitignore"), "hidden.rs\n");
    write(&folder.join("hidden.rs"), "fn a() {}");
    write(&folder.join("shown.rs"), "fn a() {}");

    let walked = walk(dir.path(), &never).expect("walk");

    assert_eq!(walked.entries.len(), 2, "the rules past the cap were not read");
    assert_eq!(walked.skipped.unsafe_or_large, 6);
}

#[test]
fn the_paths_of_the_files_kept_are_capped_in_total() {
    let dir = root();

    for n in 0..6 {
        write(&dir.path().join(format!("file_{n}.rs")), "fn a() {}");
    }

    // six paths of nine bytes
    let limits = |path_total| Limits {
        path_total,
        ..Limits::default()
    };

    assert_eq!(walk_with(dir.path(), &never, limits(53)).unwrap_err(), TOO_LARGE);
    assert_eq!(
        walk_with(dir.path(), &never, limits(54)).expect("walk").entries.len(),
        6
    );
}

// macOS refuses a path over 1,024 bytes, so the tree cannot be made there
#[cfg(not(target_os = "macos"))]
#[test]
fn a_path_over_a_kilobyte_below_the_root_is_left_out_and_counted_whatever_the_root_is() {
    let dir = root();
    let mut folder = dir.path().to_path_buf();

    for level in 0..4 {
        folder.push(format!("{level:0>250}"));
    }

    // 1,003 bytes below the root: kept, and one folder deeper (1,254) is not entered
    write(&folder.join("kept.rs"), "fn a() {}");
    write(&folder.join(format!("{:0>250}/deep.rs", 4)), "fn a() {}");
    write(&dir.path().join("ok.rs"), "fn a() {}");

    let walked = walk(dir.path(), &never).expect("walk");

    assert_eq!(walked.entries.len(), 2);
    assert!(walked.entries.iter().all(|entry| entry.rel.len() < 1_024));
    assert_eq!(walked.skipped.unreadable, 1);
}

// ---- the line of a file, read when an answer is written ----------------------------------------------

/// The stamp the index would hold for `rel` below `dir`.
fn stamp(dir: &Path, rel: &str) -> (i64, i64) {
    let entry = walk(dir, &never)
        .expect("walk")
        .entries
        .into_iter()
        .find(|entry| entry.rel == rel)
        .expect("indexed");

    (entry.mtime_ns, entry.size)
}

fn line(dir: &Path, rel: &str, number: i64) -> Option<String> {
    let (mtime, size) = stamp(dir, rel);

    read_line(dir, rel, mtime, size, number)
}

#[test]
fn one_line_is_read_trimmed_and_a_line_that_is_not_there_is_none() {
    let dir = root();

    write(&dir.path().join("a.rs"), "\u{feff}first\r\n   second line  \nthird");

    assert_eq!(line(dir.path(), "a.rs", 1).as_deref(), Some("first"));
    assert_eq!(line(dir.path(), "a.rs", 2).as_deref(), Some("second line"));
    assert_eq!(line(dir.path(), "a.rs", 3).as_deref(), Some("third"));
    assert_eq!(line(dir.path(), "a.rs", 4), None);
    assert_eq!(line(dir.path(), "a.rs", 0), None);
    assert_eq!(line(dir.path(), "a.rs", -2), None);
}

#[test]
fn a_line_is_not_read_from_a_file_that_is_not_the_one_the_index_holds() {
    let dir = root();
    let path = dir.path().join("a.rs");

    write(&path, "one\ntwo\n");

    let (mtime, size) = stamp(dir.path(), "a.rs");

    assert_eq!(read_line(dir.path(), "a.rs", mtime, size, 2).as_deref(), Some("two"));
    assert_eq!(read_line(dir.path(), "a.rs", mtime + 1, size, 2), None, "another time");
    assert_eq!(read_line(dir.path(), "a.rs", mtime, size + 1, 2), None, "another size");

    write(&path, "one\nTWO and more\n");

    assert_eq!(read_line(dir.path(), "a.rs", mtime, size, 2), None, "the file changed");
}

#[test]
fn a_line_longer_than_the_cap_and_a_file_over_a_megabyte_are_not_read() {
    let dir = root();

    write(
        &dir.path().join("a.rs"),
        &format!("short\n{}\nafter\n", "x".repeat(MAX_LINE_BYTES + 1)),
    );
    write(
        &dir.path().join("exact.rs"),
        &format!("{}\nafter\n", "y".repeat(MAX_LINE_BYTES)),
    );

    assert_eq!(line(dir.path(), "a.rs", 1).as_deref(), Some("short"));
    assert_eq!(line(dir.path(), "a.rs", 2), None);
    assert_eq!(line(dir.path(), "a.rs", 3).as_deref(), Some("after"));
    assert_eq!(
        line(dir.path(), "exact.rs", 1).map(|text| text.len()),
        Some(MAX_LINE_BYTES)
    );

    // the stamp of a file that grew past the cap after it was indexed does not match, and the cap is checked
    // all the same
    let big = dir.path().join("big.rs");

    write(&big, "x\n");

    let (mtime, _) = stamp(dir.path(), "big.rs");

    fs::write(&big, vec![b'a'; 1_000_001]).expect("grow");

    assert_eq!(read_line(dir.path(), "big.rs", mtime, 1_000_001, 1), None);
}

#[test]
fn a_line_is_not_read_through_a_link_a_second_name_or_a_folder_moved_out() {
    let outside = root();
    let dir = root();

    write(&outside.path().join("secret.rs"), "secret = 1\n");
    write(&dir.path().join("a.rs"), "one\n");
    write(&dir.path().join("sub/b.rs"), "two\n");
    write(&dir.path().join("c.rs"), "three\n");

    let (a, b, c) = (
        stamp(dir.path(), "a.rs"),
        stamp(dir.path(), "sub/b.rs"),
        stamp(dir.path(), "c.rs"),
    );

    // a file replaced by a link to a file outside
    fs::remove_file(dir.path().join("a.rs")).expect("remove");
    symlink(outside.path().join("secret.rs"), dir.path().join("a.rs")).expect("link");
    assert_eq!(read_line(dir.path(), "a.rs", a.0, a.1, 1), None);

    // a folder moved out and replaced by a link
    fs::rename(dir.path().join("sub"), outside.path().join("sub")).expect("move");
    symlink(outside.path().join("sub"), dir.path().join("sub")).expect("link");
    assert_eq!(read_line(dir.path(), "sub/b.rs", b.0, b.1, 1), None);

    // a second name for the file
    fs::hard_link(dir.path().join("c.rs"), dir.path().join("c2.rs")).expect("hard link");
    assert_eq!(read_line(dir.path(), "c.rs", c.0, c.1, 1), None);
}
