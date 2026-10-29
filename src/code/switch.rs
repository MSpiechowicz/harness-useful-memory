//! The code index switch: `code.toml` in the data folder holds `enabled = true|false`.
//!
//! A missing file, or a file without the key, means ON. A file that cannot be read safely (another
//! user's, writable by others, not valid TOML, a wrong type) means OFF: indexing is never started on
//! the strength of a file nobody can vouch for. The environment variable `HARNESS_MEMORY_CODE_INDEX`
//! (`0` or `1`) overrides the file, for tests and benchmarks. Off hides the tools and keeps the index.

use std::path::Path;

use crate::support::Env;
use crate::support::private::{private_folder, read_private, write_atomic};

/// The settings file in the data folder.
pub const FILE: &str = "code.toml";

/// The environment variable that overrides the file: `0` is off, `1` is on.
pub const ENV_VAR: &str = "HARNESS_MEMORY_CODE_INDEX";

/// The most bytes read from the settings file.
const FILE_MAX: usize = 4096;

/// Where the answer of [`state`] comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `code.toml` says so.
    File,
    /// Nothing is set: the default, on.
    Default,
    /// `HARNESS_MEMORY_CODE_INDEX` says so.
    Env,
}

impl Source {
    /// The word `code status --json` prints.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Default => "default",
            Self::Env => "env",
        }
    }
}

/// Whether the code index is on, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    pub enabled: bool,
    pub source: Source,
}

/// The switch of the data folder `root`, read now.
pub fn state(env: &Env, root: &Path) -> State {
    match env.var(ENV_VAR).map(str::trim) {
        Some("0") => State {
            enabled: false,
            source: Source::Env,
        },
        Some("1") => State {
            enabled: true,
            source: Source::Env,
        },
        _ => from_file(root),
    }
}

/// True when the code index is on.
pub fn enabled(env: &Env, root: &Path) -> bool {
    state(env, root).enabled
}

fn from_file(root: &Path) -> State {
    let default = State {
        enabled: true,
        source: Source::Default,
    };
    let off = State {
        enabled: false,
        source: Source::File,
    };

    let bytes = match read_private(&root.join(FILE), FILE_MAX) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return default,
        Err(_) => return off,
    };

    let Some(table) = String::from_utf8(bytes)
        .ok()
        .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
    else {
        return off;
    };

    match table.get("enabled") {
        None => default,
        Some(toml::Value::Boolean(enabled)) => State {
            enabled: *enabled,
            source: Source::File,
        },
        Some(_) => off,
    }
}

/// Writes the switch to `code.toml`, creating the data folder (0700) when missing.
pub fn set_enabled(root: &Path, on: bool) -> Result<(), String> {
    private_folder(root, "data folder")?;

    let text = format!("enabled = {on}\n");

    write_atomic(root, &root.join(FILE), text.as_bytes(), "code", "code index settings")
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn env(value: Option<&str>) -> Env {
        let env = Env::for_tests("/home/nobody");

        match value {
            Some(value) => env.with_var(ENV_VAR, value),
            None => env,
        }
    }

    #[test]
    fn a_missing_file_means_on_by_default() {
        let dir = tempfile::tempdir().expect("dir");

        assert_eq!(
            state(&env(None), dir.path()),
            State {
                enabled: true,
                source: Source::Default
            }
        );
    }

    #[test]
    fn the_file_is_written_and_read_back() {
        let dir = tempfile::tempdir().expect("dir");

        set_enabled(dir.path(), false).expect("off");
        assert_eq!(
            state(&env(None), dir.path()),
            State {
                enabled: false,
                source: Source::File
            }
        );

        set_enabled(dir.path(), true).expect("on");
        assert_eq!(
            state(&env(None), dir.path()),
            State {
                enabled: true,
                source: Source::File
            }
        );
    }

    #[test]
    fn the_environment_wins_over_the_file() {
        let dir = tempfile::tempdir().expect("dir");
        set_enabled(dir.path(), true).expect("on");

        assert_eq!(
            state(&env(Some("0")), dir.path()),
            State {
                enabled: false,
                source: Source::Env
            }
        );

        set_enabled(dir.path(), false).expect("off");
        assert!(enabled(&env(Some("1")), dir.path()));
        assert!(!enabled(&env(Some("maybe")), dir.path()));
    }

    #[test]
    fn a_file_without_the_key_is_the_default_and_a_broken_one_is_off() {
        let dir = tempfile::tempdir().expect("dir");
        let write = |text: &str| {
            write_atomic(dir.path(), &dir.path().join(FILE), text.as_bytes(), "code", "test").expect("write")
        };

        write("other = 1\n");
        assert_eq!(state(&env(None), dir.path()).source, Source::Default);
        assert!(enabled(&env(None), dir.path()));

        write("enabled = \"yes\"\n");
        assert!(!enabled(&env(None), dir.path()));

        write("not toml [");
        assert!(!enabled(&env(None), dir.path()));
    }
}
