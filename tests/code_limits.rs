//! The code index in a server that may use 1.5 GB of address space, over repositories built to make it
//! use more: a call that would exhaust the memory ends the process, and the test fails. Also the server
//! that must end on SIGTERM while a call is busy with something that does not look at the signal.

// `ulimit -v` sets an address-space limit only on Linux (macOS refuses it)
#![cfg(target_os = "linux")]

mod common;

use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

use common::stdio::{BINARY, Client};
use serde_json::json;
use tempfile::TempDir;

/// The address space the server may use, in KB.
const LIMIT_KB: u64 = 1_500_000;

/// A server under `ulimit -v`, with its data folder and home in `base`.
fn limited_server(base: &Path) -> Client {
    for folder in ["data", "home"] {
        std::fs::create_dir_all(base.join(folder)).expect("mkdir");
    }

    let mut command = Command::new("sh");

    command
        .arg("-c")
        .arg(format!("ulimit -v {LIMIT_KB} && exec \"$0\" mcp"))
        .arg(BINARY)
        .current_dir(base)
        .env("HOME", base.join("home"))
        .env("XDG_DATA_HOME", base.join("home/data"))
        .env("XDG_CONFIG_HOME", base.join("home/config"))
        .env("HARNESS_MEMORY_DIR", base.join("data"))
        .env("HARNESS_MEMORY_CODE_INDEX", "1");

    Client::start(command)
}

fn repo(base: &TempDir) -> std::path::PathBuf {
    let repo = base.path().join("repo");

    // the code index only reads git repositories
    std::fs::create_dir_all(repo.join(".git")).expect("repo");
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").expect("head");

    repo.canonicalize().expect("canonical")
}

fn symbol(client: &mut Client, repo: &Path, name: &str) -> String {
    client.call(
        "code_symbol",
        json!({ "project_path": repo.to_string_lossy(), "symbol": name }),
    )
}

#[test]
fn ignore_files_that_are_links_to_a_device_do_not_exhaust_the_memory() {
    let base = TempDir::new().expect("temp");
    let repo = repo(&base);

    std::fs::write(repo.join("a.rs"), "pub fn wanted_name() {}\n").expect("write");
    symlink("/dev/zero", repo.join(".gitignore")).expect("link");
    symlink("/dev/zero", repo.join(".ignore")).expect("link");
    std::fs::create_dir_all(repo.join(".git/info")).expect("mkdir");
    symlink("/dev/zero", repo.join(".git/info/exclude")).expect("link");

    let mut client = limited_server(base.path());

    assert!(symbol(&mut client, &repo, "wanted_name").contains("a.rs:1"));
}

#[test]
fn a_git_file_pointing_elsewhere_and_a_huge_ignore_file_are_not_read() {
    let base = TempDir::new().expect("temp");
    let repo = repo(&base);
    let outside = base.path().join("outside");

    std::fs::create_dir_all(outside.join("info")).expect("mkdir");
    std::fs::write(outside.join("info/exclude"), "a.rs\n").expect("write");
    std::fs::write(outside.join("commondir"), "../common\n").expect("write");
    std::fs::write(outside.join("HEAD"), "ref: refs/heads/main\n").expect("write");
    std::fs::remove_dir_all(repo.join(".git")).expect("no folder");
    std::fs::write(repo.join(".git"), format!("gitdir: {}\n", outside.display())).expect("gitfile");
    std::fs::write(repo.join(".gitignore"), vec![b'#'; 50_000_000]).expect("big ignore file");
    std::fs::write(repo.join("a.rs"), "pub fn wanted_name() {}\n").expect("write");

    let mut client = limited_server(base.path());

    assert!(symbol(&mut client, &repo, "wanted_name").contains("a.rs:1"));
}

#[test]
fn files_of_self_calling_functions_of_one_name_do_not_exhaust_the_memory() {
    let base = TempDir::new().expect("temp");
    let repo = repo(&base);

    for (file, line, lines) in [
        ("rust.rs", "fn x(){x()}", 12_000),
        ("big.rs", "fn x(){x()}", 80_000),
        ("methods.rs", "impl A { fn x(&self){self.x()} }", 20_000),
        ("script.ts", "function x(){x()}", 20_000),
        ("script.py", "def x(): x()", 20_000),
        ("main.go", "func x(){x()}", 20_000),
    ] {
        std::fs::write(repo.join(file), format!("{line}\n").repeat(lines)).expect("write");
    }

    let mut client = limited_server(base.path());
    let found = symbol(&mut client, &repo, "x");

    assert!(found.contains("x"), "{found}");

    // the second call finds the graph cached and the index consistent
    assert!(symbol(&mut client, &repo, "x").contains("x"));
}

#[test]
fn hundreds_of_folders_with_a_megabyte_ignore_file_each_do_not_exhaust_the_memory() {
    let base = TempDir::new().expect("temp");
    let repo = repo(&base);

    for n in 0..400 {
        let folder = repo.join(format!("d{n}"));

        std::fs::create_dir_all(&folder).expect("mkdir");
        std::fs::write(folder.join("f.rs"), format!("pub fn wanted_{n}() {{}}\n")).expect("write");

        // a megabyte of NUL bytes: one line of a million characters, which used to be compiled for every
        // folder and kept until the walk ended
        std::fs::File::create(folder.join(".gitignore"))
            .and_then(|file| file.set_len(1_000_000))
            .expect("sparse file");
    }

    let mut client = limited_server(base.path());
    let started = std::time::Instant::now();

    assert!(symbol(&mut client, &repo, "wanted_399").contains("d399/f.rs:1"));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(120),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn nested_ignore_files_of_a_megabyte_each_with_thousands_of_patterns_stay_within_the_memory() {
    let base = TempDir::new().expect("temp");
    let repo = repo(&base);
    let mut folder = repo.clone();

    for level in 0..6 {
        std::fs::create_dir_all(&folder).expect("mkdir");

        // 20 000 star patterns of about fifty bytes: as many bytes as a megabyte allows
        let rules: String = (0..20_000)
            .map(|k| format!("**/L{level}k{k}y0*L{level}k{k}y1*L{level}k{k}y2*\n"))
            .collect();

        std::fs::write(folder.join(".gitignore"), rules).expect("rules");
        folder = folder.join(format!("n{level}"));
    }

    std::fs::create_dir_all(&folder).expect("mkdir");
    std::fs::write(folder.join("deep.rs"), "pub fn wanted_name() {}\n").expect("write");

    let mut client = limited_server(base.path());

    assert!(symbol(&mut client, &repo, "wanted_name").contains("deep.rs:1"));
}

/// A file of about a megabyte in which one half-megabyte name is used by tens of thousands of items: copied
/// into each of them, it needs tens of gigabytes. The server must index the repository (the marker file
/// stays findable) within its memory, whatever the file becomes.
fn long_name_is_not_copied_into_every_item(file: &str, source: String) {
    let base = TempDir::new().expect("temp");
    let repo = repo(&base);

    std::fs::write(repo.join(file), source).expect("write");
    std::fs::write(repo.join("marker.rs"), "pub fn wanted_name() {}\n").expect("write");

    let mut client = limited_server(base.path());
    let started = std::time::Instant::now();

    assert!(symbol(&mut client, &repo, "wanted_name").contains("marker.rs:1"));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(120),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_rust_use_list_under_a_half_megabyte_path_does_not_exhaust_the_memory() {
    long_name_is_not_copied_into_every_item(
        "lib.rs",
        format!(
            "use {}::{{{}}};\nfn f(){{}}\n",
            "a".repeat(500_000),
            "x,".repeat(240_000)
        ),
    );
}

#[test]
fn a_rust_impl_of_a_half_megabyte_type_does_not_exhaust_the_memory() {
    long_name_is_not_copied_into_every_item(
        "lib.rs",
        format!("impl {} {{{}}}\n", "A".repeat(500_000), "fn a(){}".repeat(57_000)),
    );
}

#[test]
fn a_ts_import_from_a_half_megabyte_path_does_not_exhaust_the_memory() {
    long_name_is_not_copied_into_every_item(
        "a.ts",
        format!(
            "import {{{}}} from \"{}\";\n",
            "x,".repeat(240_000),
            "a".repeat(500_000)
        ),
    );
}

#[test]
fn a_python_class_of_a_half_megabyte_name_does_not_exhaust_the_memory() {
    long_name_is_not_copied_into_every_item(
        "a.py",
        format!(
            "class {}:\n{}",
            "A".repeat(500_000),
            " def a(s): s.b=1\n".repeat(50_000)
        ),
    );
}

#[test]
fn types_nested_or_repeated_a_hundred_thousand_times_are_read_quickly() {
    let shapes = [
        (
            "a.ts",
            format!("let x: {}number{};\n", "Promise<".repeat(110_000), ">".repeat(110_000)),
        ),
        (
            "a.py",
            format!("x: {}int{} = 1\n", "Optional[".repeat(90_000), "]".repeat(90_000)),
        ),
        ("a.ts", format!("let x: Foo{};\n", "[]".repeat(480_000))),
    ];

    for (file, source) in shapes {
        let base = TempDir::new().expect("temp");
        let repo = repo(&base);

        std::fs::write(repo.join(file), source).expect("write");
        std::fs::write(repo.join("marker.rs"), "pub fn wanted_name() {}\n").expect("write");

        let mut client = limited_server(base.path());
        let started = std::time::Instant::now();

        assert!(
            symbol(&mut client, &repo, "wanted_name").contains("marker.rs:1"),
            "{file}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "{file}: {:?}",
            started.elapsed()
        );
    }
}

#[test]
fn sigterm_ends_a_server_that_is_busy_in_a_call_that_does_not_look_at_it() {
    let base = TempDir::new().expect("temp");
    let bin = base.path().join("bin");
    let work = base.path().join("work");

    for folder in [&bin, &work, &base.path().join("data"), &base.path().join("home")] {
        std::fs::create_dir_all(folder).expect("mkdir");
    }

    // a git that takes its time: the server waits for it (up to fifteen seconds) without looking at its input
    let git = bin.join("git");

    std::fs::write(&git, "#!/bin/sh\nsleep 8\n").expect("fake git");
    std::fs::set_permissions(&git, std::os::unix::fs::PermissionsExt::from_mode(0o755)).expect("chmod");

    let mut command = Command::new(BINARY);

    command
        .arg("mcp")
        .current_dir(&work)
        .env("HOME", base.path().join("home"))
        .env("XDG_CONFIG_HOME", base.path().join("home/config"))
        .env("HARNESS_MEMORY_DIR", base.path().join("data"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()));

    let mut client = Client::start(command);

    client.send(&json!({ "jsonrpc": "2.0", "id": 41, "method": "tools/call",
        "params": { "name": "note_search", "arguments": { "query": "anything", "project_path": work.to_string_lossy() } } }));
    std::thread::sleep(std::time::Duration::from_millis(1_000));

    let sent = Command::new("kill")
        .args(["-TERM", &client.pid().to_string()])
        .status()
        .expect("kill");
    let signalled = std::time::Instant::now();

    assert!(sent.success());
    assert!(
        client.exit_within(std::time::Duration::from_secs(4)).is_some(),
        "the server was still busy {:?} after SIGTERM",
        signalled.elapsed()
    );
    assert!(
        signalled.elapsed() < std::time::Duration::from_millis(3_500),
        "{:?}",
        signalled.elapsed()
    );
}

/// The peak resident memory of the process `pid`, in KiB.
#[cfg(target_os = "linux")]
fn peak_kib(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .expect("status")
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
        .expect("VmHWM")
}

#[cfg(target_os = "linux")]
#[test]
fn ignore_files_full_of_wildcards_cost_the_server_little_memory_on_every_call() {
    let base = TempDir::new().expect("temp");
    let repo = repo(&base);
    let mut folder = repo.clone();
    let mut state = 1_u64;

    // four nested folders, each with a megabyte of lines of four thousand characters of `*x` pairs: compiled
    // by the server on every call, they used to need about 650 MB at their peak
    for level in 0..4 {
        std::fs::create_dir_all(&folder).expect("mkdir");

        let rules: String = (0..250)
            .map(|_| {
                let mut line = String::new();

                while line.len() < 3_998 {
                    state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    line.push('*');
                    line.push(char::from(b'a' + u8::try_from((state >> 33) % 10).expect("digit")));
                }

                line + "\n"
            })
            .collect();

        std::fs::write(folder.join(".gitignore"), rules).expect("rules");
        folder = folder.join(format!("n{level}"));
    }

    std::fs::create_dir_all(&folder).expect("mkdir");
    std::fs::write(folder.join("deep.rs"), "pub fn wanted_name() {}\n").expect("write");

    let mut client = limited_server(base.path());
    let started = std::time::Instant::now();

    assert!(symbol(&mut client, &repo, "wanted_name").contains("deep.rs:1"));
    assert!(symbol(&mut client, &repo, "wanted_name").contains("deep.rs:1"));

    let peak = peak_kib(client.pid());

    assert!(peak < 200_000, "the server peaked at {peak} KiB");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
}

/// The text of the reply to a `code_symbol` call, whether it is an error or not.
fn reply_text(client: &mut Client, repo: &Path, name: &str) -> String {
    let reply = client.request(
        "tools/call",
        json!({ "name": "code_symbol", "arguments": { "project_path": repo.to_string_lossy(), "symbol": name } }),
    );

    reply["result"]["content"][0]["text"].as_str().expect("text").to_owned()
}

/// The peak resident memory of the process `pid`, in kB (Linux).
#[cfg(target_os = "linux")]
fn peak_kb(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .expect("status")
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse().ok())
        .expect("VmHWM")
}

#[test]
fn a_folder_of_more_files_than_the_cap_at_a_long_path_is_refused_without_holding_them() {
    let base = TempDir::new().expect("temp");
    let repo = repo(&base);
    let mut folder = repo.clone();

    // a path of about nine hundred bytes (the longest the walk keeps is 1,024), which every entry of the folder
    // would carry
    for level in 0..9 {
        folder.push(format!("{level:0>100}"));
    }

    std::fs::create_dir_all(&folder).expect("mkdir");

    for n in 0..50_100 {
        std::fs::File::create(folder.join(format!("f{n}.txt"))).expect("file");
    }

    std::fs::write(repo.join("marker.rs"), "pub fn wanted_name() {}\n").expect("write");

    let mut client = limited_server(base.path());
    let started = std::time::Instant::now();
    let answer = reply_text(&mut client, &repo, "wanted_name");

    assert_eq!(answer, "repository too large to index");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "{:?}",
        started.elapsed()
    );

    // the entries were counted as they were read, not listed and sorted first
    #[cfg(target_os = "linux")]
    assert!(peak_kb(client.pid()) < 150_000, "peak {} kB", peak_kb(client.pid()));
}

#[test]
fn a_chain_of_a_thousand_nested_folders_with_an_ignore_file_each_is_cheap_on_every_call() {
    let base = TempDir::new().expect("temp");
    let repo = repo(&base);
    let mut folder = repo.clone();

    for _ in 0..1_500 {
        folder.push("d");
        std::fs::create_dir_all(&folder).expect("mkdir");
        std::fs::write(folder.join(".gitignore"), "*.log\n!keep.log\n/generated/\n").expect("rules");
        std::fs::write(folder.join("f.rs"), "pub fn nested_fn() {}\n").expect("write");
    }

    std::fs::write(repo.join("marker.rs"), "pub fn wanted_name() {}\n").expect("write");

    let mut client = limited_server(base.path());

    for call in 0..2 {
        let started = std::time::Instant::now();
        let answer = reply_text(&mut client, &repo, "wanted_name");

        assert!(answer.contains("marker.rs:1"), "{answer}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(8),
            "call {call}: {:?}",
            started.elapsed()
        );
    }
}

/// A folder `levels` deep, each level a name of 250 characters, below `repo`: a path of `levels * 251` bytes.
#[cfg(target_os = "linux")]
fn long_folder(repo: &Path, levels: usize) -> std::path::PathBuf {
    let mut folder = repo.to_path_buf();

    for level in 0..levels {
        folder.push(format!("{level:0>250}"));
    }

    std::fs::create_dir_all(&folder).expect("mkdir");

    folder
}

/// `count` small source files in `folder`, and a marker next to `.git`.
#[cfg(target_os = "linux")]
fn fill(folder: &Path, repo: &Path, count: usize) {
    for n in 0..count {
        std::fs::write(folder.join(format!("f{n}.rs")), format!("pub fn f{n}() {{}}\n")).expect("file");
    }

    std::fs::write(repo.join("marker.rs"), "pub fn wanted_name() {}\n").expect("write");
}

#[cfg(target_os = "linux")]
#[test]
fn twenty_thousand_files_at_paths_of_nearly_four_kilobytes_stay_cheap() {
    let base = TempDir::new().expect("temp");
    let repo = repo(&base);

    // paths of about 3.6 KB, which used to cost the server about 0.8 GB
    fill(&long_folder(&repo, 14), &repo, 19_900);

    let mut client = limited_server(base.path());
    let answer = reply_text(&mut client, &repo, "wanted_name");

    assert!(answer.contains("marker.rs:1"), "{answer}");
    assert!(peak_kb(client.pid()) < 150_000, "peak {} kB", peak_kb(client.pid()));
}

#[cfg(target_os = "linux")]
#[test]
fn twenty_thousand_files_at_the_longest_kept_paths_are_refused() {
    let base = TempDir::new().expect("temp");
    let repo = repo(&base);

    // paths of about 1,000 bytes, the longest the walk keeps
    fill(&long_folder(&repo, 4), &repo, 19_900);

    let mut client = limited_server(base.path());
    let answer = reply_text(&mut client, &repo, "wanted_name");

    // 20 MB of paths are past the 16 MiB the walk keeps: refused, where holding them cost about 140 MB
    assert_eq!(answer, "repository too large to index");
    assert!(peak_kb(client.pid()) < 100_000, "peak {} kB", peak_kb(client.pid()));
}

/// An ignore file of about 250 KiB: lines of 1,000 characters with 64 wildcards each, which compile to far
/// more than their text.
#[cfg(target_os = "linux")]
fn hostile_ignore(seed: u64) -> String {
    let mut state = seed.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(3);

    (0..250)
        .map(|_| {
            let mut line = String::new();

            while line.len() < 1_000 {
                state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);

                // a wildcard every 16 characters, 62 of them
                if line.len().is_multiple_of(16) && line.len() < 992 {
                    line.push('*');
                }

                line.push(char::from(b'a' + u8::try_from((state >> 33) % 20).expect("letter")));
            }

            line + "\n"
        })
        .collect()
}

#[cfg(target_os = "linux")]
#[test]
fn eighteen_hostile_ignore_files_in_different_repositories_do_not_pile_up_in_the_server() {
    let base = TempDir::new().expect("temp");
    let mut client = limited_server(base.path());

    for n in 0..18 {
        let repo = base.path().join(format!("repo{n}"));

        std::fs::create_dir_all(repo.join(".git")).expect("repo");
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").expect("head");
        std::fs::write(repo.join(".gitignore"), hostile_ignore(n)).expect("rules");
        std::fs::write(repo.join("a.rs"), "pub fn wanted_name() {}\n").expect("write");

        let repo = repo.canonicalize().expect("canonical");

        assert!(symbol(&mut client, &repo, "wanted_name").contains("a.rs:1"));
    }

    let peak = peak_kib(client.pid());

    // 480 MB when the compiled rules of every file were kept for the next call
    assert!(peak < 250_000, "the server peaked at {peak} KiB");
}
