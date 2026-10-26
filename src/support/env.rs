//! Injected process environment.
//!
//! Path logic takes an [`Env`] instead of reading `std::env` directly, so tests run against
//! fixture homes and never touch the real `~` or data folder.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::Error;

/// Environment variables captured by [`Env::from_process`]: the only ones the server's own logic
/// reads. Libraries and children read their own standard ones: the TLS setup of the team database
/// drivers may read `SSL_CERT_FILE` and `SSL_CERT_DIR` for the trusted CA certificates (not the
/// MongoDB driver, which trusts the Mozilla roots compiled into it or the `tlsCAFile`), the
/// libSQL client ignores proxy variables (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`), and
/// `git` gets the process environment without `GIT_*` and the team database settings (see
/// [`super::git`]).
pub const CAPTURED_VARS: &[&str] = &[
    // Where `git` is looked up.
    "PATH",
    // The per-machine data folder (see `crate::location`).
    "XDG_DATA_HOME",
    "LOCALAPPDATA",
    // The global git configuration git reads (passed on to it).
    "XDG_CONFIG_HOME",
    // The database folder override (see `crate::location::DIR_VAR`).
    "HARNESS_MEMORY_DIR",
    // The team database and its config file (see `crate::backend`). The URL and the token are
    // secrets: `Env`'s `Debug` hides them.
    "HARNESS_MEMORY_DATABASE_URL",
    "HARNESS_MEMORY_DATABASE_TOKEN",
    "HARNESS_MEMORY_CONFIG",
    // Which server a `mongodb://` URL names: MongoDB or Amazon DocumentDB (see
    // `crate::backend::KIND_VAR`).
    "HARNESS_MEMORY_DATABASE_KIND",
    // Which scopes stay on this computer with a team database (see `crate::backend::Routing`).
    "HARNESS_MEMORY_REPOSITORY_NOTES",
    "HARNESS_MEMORY_GLOBAL_NOTES",
    // Whether scripts stay on this computer with a team database (see `crate::backend::Routing`).
    "HARNESS_MEMORY_SCRIPTS",
    // Whether team scripts run without approval here (see `crate::backend::TRUST_TEAM_SCRIPTS_VAR`).
    "HARNESS_MEMORY_TRUST_TEAM_SCRIPTS",
    // Whether team hooks are shared, and whether Harness Desktop runs them without approval (see
    // `crate::backend::HOOKS_VAR`).
    "HARNESS_MEMORY_HOOKS",
    "HARNESS_MEMORY_TRUST_TEAM_HOOKS",
    // The same for team routines (see `crate::backend::ROUTINES_VAR`).
    "HARNESS_MEMORY_ROUTINES",
    "HARNESS_MEMORY_TRUST_TEAM_ROUTINES",
    // Whether `remote` scopes fall back to this computer while the team database is unreachable
    // (see `crate::backend::OFFLINE_FALLBACK_VAR`).
    "HARNESS_MEMORY_OFFLINE_FALLBACK",
    // The user name, the default author of a team hook or routine (`hooks put`, `routines put`,
    // `routines delete --mine`).
    "USER",
    "USERNAME",
    // How long `script_run` waits for Harness Desktop's answer, in milliseconds (for tests).
    "HARNESS_MEMORY_APPROVAL_WAIT_MS",
    // The chat Harness Desktop started this server's harness for, whose save guards apply (see
    // `crate::backend::SESSION_VAR`).
    "HARNESS_MEMORY_SESSION",
    // The Windows config folder (roaming), which holds the config file.
    "APPDATA",
];

/// Captured variables whose values [`Env`]'s `Debug` shows as `••••`.
const SECRET_VARS: &[&str] = &["HARNESS_MEMORY_DATABASE_URL", "HARNESS_MEMORY_DATABASE_TOKEN"];

/// Operating-system family, used for platform-specific paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsKind {
    /// Linux and other non-Apple Unix systems.
    Linux,
    /// macOS.
    MacOs,
    /// Windows.
    Windows,
}

impl OsKind {
    /// The OS this binary was built for.
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

/// Snapshot of the environment the server depends on.
#[derive(Clone, PartialEq, Eq)]
pub struct Env {
    /// The user's home directory.
    pub home: PathBuf,
    /// Captured variables (only [`CAPTURED_VARS`] in production).
    pub vars: BTreeMap<String, String>,
    /// OS family.
    pub os: OsKind,
}

impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let vars: BTreeMap<&str, &str> = self
            .vars
            .iter()
            .map(|(name, value)| {
                let shown = if SECRET_VARS.contains(&name.as_str()) {
                    "••••"
                } else {
                    value.as_str()
                };
                (name.as_str(), shown)
            })
            .collect();

        f.debug_struct("Env")
            .field("home", &self.home)
            .field("vars", &vars)
            .field("os", &self.os)
            .finish()
    }
}

impl Env {
    /// Captures the real environment once at startup.
    pub fn from_process() -> Result<Self, Error> {
        let home = std::env::home_dir()
            .filter(|home| !home.as_os_str().is_empty())
            .ok_or(Error::HomeUnknown)?;

        let vars = CAPTURED_VARS
            .iter()
            .filter_map(|name| std::env::var(name).ok().map(|value| ((*name).to_owned(), value)))
            .collect();

        Ok(Self {
            home,
            vars,
            os: OsKind::current(),
        })
    }

    /// An environment rooted at `home` with no variables and the current OS (for tests).
    pub fn for_tests(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            vars: BTreeMap::new(),
            os: OsKind::current(),
        }
    }

    /// Returns a copy with `name` set to `value`.
    pub fn with_var(mut self, name: &str, value: impl Into<String>) -> Self {
        self.vars.insert(name.to_owned(), value.into());
        self
    }

    /// Returns a copy that reports `os` (for platform-specific path tests).
    pub fn with_os(mut self, os: OsKind) -> Self {
        self.os = os;
        self
    }

    /// A captured variable, or `None` when unset or empty.
    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars
            .get(name)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }

    /// Expands a leading `~` (alone, or followed by `/` or `\`) to the home directory.
    pub fn expand_home(&self, raw: &str) -> PathBuf {
        if raw == "~" {
            return self.home.clone();
        }

        match raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
            Some(rest) => self.home.join(rest),
            None => PathBuf::from(raw),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_variables_count_as_unset() {
        let env = Env::for_tests("/home/u").with_var("XDG_DATA_HOME", "");

        assert_eq!(env.var("XDG_DATA_HOME"), None);
        assert_eq!(env.with_var("PATH", "/bin").var("PATH"), Some("/bin"));
    }

    #[test]
    fn debug_output_hides_the_database_secrets() {
        let env = Env::for_tests("/home/u")
            .with_var("HARNESS_MEMORY_DATABASE_URL", "postgres://u:hunter2@db/m")
            .with_var("HARNESS_MEMORY_DATABASE_TOKEN", "tok-secret")
            .with_var("PATH", "/bin");
        let shown = format!("{env:?}");

        assert!(!shown.contains("hunter2") && !shown.contains("tok-secret"), "{shown}");
        assert!(shown.contains("/bin"), "{shown}");
    }

    #[test]
    fn a_leading_tilde_expands_to_home() {
        let env = Env::for_tests("/home/u");

        assert_eq!(env.expand_home("~"), PathBuf::from("/home/u"));
        assert_eq!(env.expand_home("~/work"), PathBuf::from("/home/u").join("work"));
        assert_eq!(env.expand_home("~user/work"), PathBuf::from("~user/work"));
        assert_eq!(env.expand_home("/srv/~"), PathBuf::from("/srv/~"));
    }
}
