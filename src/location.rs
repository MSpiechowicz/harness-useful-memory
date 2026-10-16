//! Where the database lives, and the one-time move from Harness Desktop's folder.
//!
//! The database is `<data dir>/harness-memory/memory.sqlite3`, where the per-machine data folder
//! is `$XDG_DATA_HOME` (or `~/.local/share`) on Linux, `~/Library/Application Support` on macOS
//! and `%LOCALAPPDATA%` on Windows (not the roaming `%APPDATA%`: a SQLite database does not belong
//! in a roaming profile). [`DIR_VAR`] names another folder instead.
//!
//! Earlier versions shipped inside Harness Desktop and kept the database in that app's folder
//! ([`legacy_database_path`]). [`migrate_legacy`] copies it once to the new place and never
//! changes the original.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::support::{Env, OsKind};

/// Environment variable naming the folder that holds the database (empty = unset). Overrides the
/// data folder and skips the legacy migration. A relative value is ignored (see [`override_dir`]).
pub const DIR_VAR: &str = "HARNESS_MEMORY_DIR";

/// The database file name.
pub const FILE_NAME: &str = "memory.sqlite3";

/// The folder the server creates inside the data folder.
const APP_FOLDER: &str = "harness-memory";

/// Harness Desktop's bundle identifier, which named its data folder.
const LEGACY_APP_FOLDER: &str = "com.mspiechowicz.harness-useful-desktop";

/// The per-machine data folder (see the module docs); `None` on Windows when `LOCALAPPDATA` is
/// unset. Relative values of `XDG_DATA_HOME` (ignored, as the XDG spec says) and `LOCALAPPDATA`
/// do not count: the server runs inside the project, so they would put the database there.
pub fn data_dir(env: &Env) -> Option<PathBuf> {
    let rooted = |name| env.var(name).map(PathBuf::from).filter(|dir| dir.has_root());

    match env.os {
        OsKind::Linux => Some(rooted("XDG_DATA_HOME").unwrap_or_else(|| env.home.join(".local/share"))),
        OsKind::MacOs => Some(env.home.join("Library/Application Support")),
        OsKind::Windows => rooted("LOCALAPPDATA"),
    }
}

/// The folder [`DIR_VAR`] names, with a leading `~` expanded. `None` when unset or relative: the
/// server runs with the project as its working directory, so a relative folder would put the
/// database inside the repository.
pub fn override_dir(env: &Env) -> Option<PathBuf> {
    env.var(DIR_VAR)
        .map(|dir| env.expand_home(dir))
        .filter(|dir| dir.has_root())
}

/// Whether [`DIR_VAR`] is set to a relative folder, which [`override_dir`] ignores.
pub fn override_is_relative(env: &Env) -> bool {
    env.var(DIR_VAR).is_some() && override_dir(env).is_none()
}

/// The database file: `<`[`override_dir`]`>/memory.sqlite3` when set, else
/// `<data dir>/harness-memory/memory.sqlite3`. `None` when the data folder is unknown.
pub fn database_path(env: &Env) -> Option<PathBuf> {
    if let Some(dir) = override_dir(env) {
        return Some(dir.join(FILE_NAME));
    }

    data_dir(env).map(|dir| dir.join(APP_FOLDER).join(FILE_NAME))
}

/// Harness Desktop's database: `<data dir>/com.mspiechowicz.harness-useful-desktop/memory/
/// memory.sqlite3`. `None` when [`override_dir`] is set or the data folder is unknown.
pub fn legacy_database_path(env: &Env) -> Option<PathBuf> {
    if override_dir(env).is_some() {
        return None;
    }

    data_dir(env).map(|dir| dir.join(LEGACY_APP_FOLDER).join("memory").join(FILE_NAME))
}

/// What [`migrate_legacy`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Migration {
    /// The database exists already, or there is no legacy one.
    NotNeeded,
    /// The legacy database was copied to the new place.
    Copied,
    /// Copying failed (the reason); the legacy database is untouched.
    Failed(String),
}

/// Copies the legacy database to `path` when `path` does not exist and the legacy one does.
///
/// The copy is `VACUUM INTO` a temporary file from a read-only connection, so it is consistent
/// even while another process writes the legacy file in WAL mode. The temporary file then takes
/// the final name only if that is still free: a server starting at the same time keeps its own.
pub fn migrate_legacy(env: &Env, path: &Path) -> Migration {
    let Some(legacy) = legacy_database_path(env) else {
        return Migration::NotNeeded;
    };

    if path.exists() || !legacy.is_file() {
        return Migration::NotNeeded;
    }

    let temporary = path.with_file_name(format!("{FILE_NAME}.migrating-{}", std::process::id()));
    let result = copy_database(&legacy, &temporary).and_then(|()| claim(&temporary, path));
    std::fs::remove_file(&temporary).ok();

    match result {
        Ok(()) => Migration::Copied,
        Err(reason) => Migration::Failed(reason),
    }
}

/// Writes a compacted copy of `source` to `target` (replacing a stale `target`). `target` is
/// created new, empty and private first (0600 on Unix), which `VACUUM INTO` accepts; creating it
/// new refuses a link planted at that name.
fn copy_database(source: &Path, target: &Path) -> Result<(), String> {
    std::fs::remove_file(target).ok();
    crate::store::prepare_new_file(target).map_err(|error| format!("cannot create the copy: {}", error.kind()))?;

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(source, flags).map_err(|error| error.to_string())?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| error.to_string())?;
    conn.execute("VACUUM INTO ?1", [target.to_string_lossy()])
        .map_err(|error| error.to_string())?;

    Ok(())
}

/// Gives `temporary` the name `path` unless `path` exists by now.
fn claim(temporary: &Path, path: &Path) -> Result<(), String> {
    match std::fs::hard_link(temporary, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        // Some filesystems have no hard links: fall back to a rename, racy only in that case.
        Err(_) if !path.exists() => {
            std::fs::rename(temporary, path).map_err(|error| format!("cannot move the copy: {}", error.kind()))
        }
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_prefers_xdg_data_home() {
        let env = Env::for_tests("/home/u").with_os(OsKind::Linux);
        assert_eq!(
            database_path(&env),
            Some(PathBuf::from("/home/u/.local/share/harness-memory").join(FILE_NAME))
        );

        let env = env.with_var("XDG_DATA_HOME", "/data");
        assert_eq!(
            database_path(&env),
            Some(PathBuf::from("/data/harness-memory").join(FILE_NAME))
        );
        assert_eq!(
            legacy_database_path(&env),
            Some(PathBuf::from("/data/com.mspiechowicz.harness-useful-desktop/memory").join(FILE_NAME))
        );
    }

    #[test]
    fn macos_uses_application_support() {
        let env = Env::for_tests("/Users/u").with_os(OsKind::MacOs);

        assert_eq!(
            data_dir(&env),
            Some(PathBuf::from("/Users/u/Library/Application Support"))
        );
    }

    #[test]
    fn windows_uses_local_app_data() {
        let env = Env::for_tests("C:/Users/u").with_os(OsKind::Windows);
        assert_eq!(database_path(&env), None);

        // A folder that is absolute on the host running the test (the check uses the host's rules).
        let local = std::env::temp_dir().join("AppData").join("Local");
        let env = env.with_var("LOCALAPPDATA", local.to_string_lossy());
        assert_eq!(data_dir(&env), Some(local));
    }

    #[test]
    fn relative_data_folders_are_ignored() {
        let env = Env::for_tests("/home/u")
            .with_os(OsKind::Linux)
            .with_var("XDG_DATA_HOME", "data");
        assert_eq!(data_dir(&env), Some(PathBuf::from("/home/u/.local/share")));

        let env = Env::for_tests("C:/Users/u")
            .with_os(OsKind::Windows)
            .with_var("LOCALAPPDATA", "AppData");
        assert_eq!(data_dir(&env), None);
    }

    #[test]
    fn a_relative_folder_override_is_ignored() {
        let env = Env::for_tests("/home/u")
            .with_os(OsKind::Linux)
            .with_var(DIR_VAR, "data");

        assert!(override_is_relative(&env));
        assert_eq!(
            database_path(&env),
            Some(PathBuf::from("/home/u/.local/share/harness-memory").join(FILE_NAME))
        );
        assert!(legacy_database_path(&env).is_some());
    }

    #[test]
    fn the_folder_override_wins_and_skips_the_legacy_database() {
        let env = Env::for_tests("/home/u")
            .with_os(OsKind::Linux)
            .with_var(DIR_VAR, "~/memory");

        assert_eq!(
            database_path(&env),
            Some(PathBuf::from("/home/u").join("memory").join(FILE_NAME))
        );
        assert_eq!(legacy_database_path(&env), None);

        let env = env.with_var(DIR_VAR, "");
        assert_eq!(
            database_path(&env),
            Some(PathBuf::from("/home/u/.local/share/harness-memory").join(FILE_NAME))
        );
    }
}
