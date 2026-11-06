//! The `project-key` command of the `harness-memory` executable, where Harness Desktop asks which
//! project a folder is, the way the MCP server decides it ([`crate::project`]). It has no MCP
//! tool: only a person (or Harness Desktop for them) can run it, like `held`.
//!
//! `harness-memory project-key --json` reads a JSON array of absolute folder paths on standard
//! input (at most [`PATHS_MAX`] of them, the input at most [`INPUT_MAX`] bytes) and prints `[{"path", "root", "key"}]`, one object per path in order:
//! `root` is the project root (the folder itself when it is no project), `key` the project key
//! (`git:<host/owner/repo>` or `path:<root>`), `null` for a folder that has no project, such as
//! the home folder. A path that cannot be used (not absolute, longer than [`PATH_MAX`] bytes, not an
//! existing folder, a filesystem root) gives `{"path", "error"}` instead, and the other paths still
//! answer. Input past a bound or not an array of strings exits 1 with `{"error": "invalid", "message"}` on standard output
//! and the message on standard error. Usage errors exit 2.

use std::io::{IsTerminal, Read};
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::cli::{self, Args};
use crate::project::ProjectResolver;
use crate::support::Env;

/// The usage of this command, for the executable's usage line.
pub const USAGE: &str = "project-key --json < paths.json";

/// The most paths one call answers: two lists of 256 repositories, as Harness Desktop keeps.
pub const PATHS_MAX: usize = 512;

/// The longest path resolved, in bytes.
pub const PATH_MAX: usize = 4096;

/// The largest input read, in bytes.
pub const INPUT_MAX: usize = 4 * 1024 * 1024;

/// `harness-memory project-key <args>`.
pub fn project_key(args: &[Option<&str>]) -> ExitCode {
    let Some(parsed) = Args::parse(args, &["--json"], &[]) else {
        return cli::usage(USAGE);
    };

    if !parsed.flag("--json") || !parsed.positional.is_empty() {
        return cli::usage(USAGE);
    }

    let Ok(env) = Env::from_process() else {
        eprintln!("harness-memory: the home folder is unknown");
        return ExitCode::FAILURE;
    };

    let paths = match read_paths() {
        Ok(paths) => paths,
        Err(message) => {
            println!("{}", json!({ "error": "invalid", "message": message }));
            eprintln!("harness-memory: {message}");
            return ExitCode::FAILURE;
        }
    };

    let mut resolver = ProjectResolver::new(env, None);
    let answers: Vec<Value> = paths.iter().map(|path| answer(&mut resolver, path)).collect();
    println!("{}", Value::Array(answers));
    ExitCode::SUCCESS
}

/// The answer for one `path`.
fn answer(resolver: &mut ProjectResolver, path: &str) -> Value {
    if path.len() > PATH_MAX {
        return json!({ "path": path, "error": format!("a path is longer than {PATH_MAX} bytes") });
    }

    match resolver.resolve(Some(path)) {
        Ok(Some(project)) => json!({ "path": path, "root": project.root, "key": project.key }),
        Ok(None) => match resolver.folder(Some(path)) {
            Ok(Some(folder)) => json!({ "path": path, "root": folder, "key": null }),
            _ => json!({ "path": path, "error": "this folder cannot be used" }),
        },
        Err(message) => json!({ "path": path, "error": message }),
    }
}

/// The paths on standard input; the reason when there is none to read or they pass a bound.
fn read_paths() -> Result<Vec<String>, String> {
    let stdin = std::io::stdin();

    if stdin.is_terminal() {
        return Err("give the folders as a JSON array of paths on standard input".to_owned());
    }

    let mut bytes = Vec::new();
    let limit = u64::try_from(INPUT_MAX + 1).unwrap_or(u64::MAX);
    stdin
        .lock()
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read standard input ({})", error.kind()))?;

    parse_paths(&bytes)
}

/// The paths in the JSON array `bytes`; the reason when it is not an array of strings or passes a
/// bound ([`INPUT_MAX`], [`PATHS_MAX`]).
fn parse_paths(bytes: &[u8]) -> Result<Vec<String>, String> {
    if bytes.len() > INPUT_MAX {
        return Err(format!("the input is larger than {INPUT_MAX} bytes"));
    }

    let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(bytes) else {
        return Err("the input is not a JSON array of folder paths".to_owned());
    };

    if items.len() > PATHS_MAX {
        return Err(format!("more than {PATHS_MAX} folders"));
    }

    items
        .into_iter()
        .map(|item| match item {
            Value::String(path) => Ok(path),
            _ => Err("the input is not a JSON array of folder paths".to_owned()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_array_of_strings_within_the_bounds_is_read() {
        assert_eq!(parse_paths(b"[\"/a\", \"/b\"]").expect("paths"), ["/a", "/b"]);
        assert_eq!(parse_paths(b"[]").expect("none"), Vec::<String>::new());

        for refused in [&b"{}"[..], b"[1]", b"[\"/a\", null]", b"not json", b""] {
            assert!(parse_paths(refused).is_err(), "{refused:?}");
        }

        let many = serde_json::to_vec(&vec!["/a"; PATHS_MAX + 1]).expect("json");
        assert!(parse_paths(&many).is_err());
    }
}
