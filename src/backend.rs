//! Which store the server uses: the local SQLite file (the default) or a team database.
//!
//! The team database URL comes from [`URL_VAR`], else from `database_url` in the config file
//! (`<config dir>/harness-memory/config.toml`, or the file [`CONFIG_VAR`] names). Its scheme picks
//! the backend:
//!
//! | Scheme | Backend |
//! | --- | --- |
//! | `postgres://`, `postgresql://` | PostgreSQL |
//! | `libsql://`, `https://`, `http://` (`http` for this machine only) | libSQL (remote SQLite) |
//! | `mongodb://`, `mongodb+srv://` | MongoDB |
//!
//! The scheme is matched and passed on in lowercase (`POSTGRES://…` works); the rest of the URL is
//! kept as written.
//!
//! [`TOKEN_VAR`] or `database_token` is the libSQL auth token. With a team database the whole
//! store is remote: repository and global notes are shared by everyone using it, nothing is kept
//! locally, and the move from Harness Desktop's folder does not run.
//!
//! The URL and the token are secrets: errors show a [`redact_url`] form and pass through
//! [`Secrets::scrub`]. On Unix a config file other users can change, or that another user owns, is
//! refused, and one other users can read is reported (see [`permission_warning`]).

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::store::{LibsqlStore, MemoryStore, MongoStore, PostgresStore, StoreError};
use crate::support::redact::redact_text;
use crate::support::{Env, OsKind};

/// Environment variable holding the team database URL (empty = unset). Wins over the config file.
pub const URL_VAR: &str = "HARNESS_MEMORY_DATABASE_URL";

/// Environment variable holding the libSQL auth token (empty = unset). Wins over the config file.
pub const TOKEN_VAR: &str = "HARNESS_MEMORY_DATABASE_TOKEN";

/// Environment variable naming the config file instead of the default one (absolute, a leading
/// `~` is fine).
pub const CONFIG_VAR: &str = "HARNESS_MEMORY_CONFIG";

/// The config file name inside `<config dir>/harness-memory/`.
pub const CONFIG_FILE: &str = "config.toml";

/// What the mask of a secret looks like.
const MASK: &str = "••••";

/// The config file. Unknown keys are refused, so a misspelt `database_url` fails loudly instead of
/// quietly using the local database.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    database_url: Option<String>,
    database_token: Option<String>,
}

/// The selected store.
#[derive(Clone, PartialEq, Eq)]
pub enum Backend {
    /// The local SQLite file (see [`crate::location`]).
    Local,
    /// A PostgreSQL team database.
    Postgres { url: String },
    /// A libSQL (remote SQLite, `sqld`) team database.
    Libsql { url: String, token: Option<String> },
    /// A MongoDB team database.
    Mongo { url: String },
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

impl Backend {
    /// The backend's name and its [`redact_url`]: `PostgreSQL at postgres://alice@db:5432/memory`.
    pub fn describe(&self) -> String {
        match self {
            Self::Local => "the local database".to_owned(),
            Self::Postgres { url } => format!("PostgreSQL at {}", redact_url(url)),
            Self::Libsql { url, .. } => format!("libSQL at {}", redact_url(url)),
            Self::Mongo { url } => format!("MongoDB at {}", redact_url(url)),
        }
    }

    /// The secrets of this backend, for scrubbing messages.
    pub fn secrets(&self) -> Secrets {
        match self {
            Self::Local => Secrets::default(),
            Self::Postgres { url } | Self::Mongo { url } => Secrets::of(url, None),
            Self::Libsql { url, token } => Secrets::of(url, token.as_deref()),
        }
    }
}

/// The backend [`select`] picked, with warnings to print first.
#[derive(Debug)]
pub struct Selection {
    pub backend: Backend,
    pub warnings: Vec<String>,
}

/// Why no backend could be selected. Messages never hold the URL or the token.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("{CONFIG_VAR} must be an absolute path to the config file")]
    ConfigRelative,
    #[error("the config file {0} does not exist ({CONFIG_VAR} names it)")]
    ConfigMissing(PathBuf),
    #[error("cannot read the config file {path}: {reason}")]
    ConfigUnreadable { path: PathBuf, reason: String },
    #[error(
        "the config file {0} can be changed by other users, who could point the server at their own database; \
         run chmod 600 on it"
    )]
    ConfigWritable(PathBuf),
    #[error(
        "the config file {0} belongs to another user, who could point the server at their own database; use a \
         file you own"
    )]
    ConfigNotOwned(PathBuf),
    #[error("the config file {path} is not valid: {reason}")]
    ConfigInvalid { path: PathBuf, reason: String },
    #[error(
        "the database URL must start with postgres://, postgresql://, libsql://, https://, http:// (this machine \
         only), mongodb:// or mongodb+srv://{0}"
    )]
    Scheme(String),
    #[error("an http:// database URL is only allowed for this machine (localhost); use https:// or libsql://")]
    PlainHttp,
    #[error(
        "a libSQL URL takes no user, password or query: give the token in {TOKEN_VAR} or database_token \
         instead"
    )]
    LibsqlUrlSecrets,
}

/// The config folder: `$XDG_CONFIG_HOME` (or `~/.config`) on Linux, `~/Library/Application
/// Support` on macOS, `%APPDATA%` on Windows. Relative values do not count (the server runs inside
/// the project, so they would read a file from the repository).
pub fn config_dir(env: &Env) -> Option<PathBuf> {
    let rooted = |name| env.var(name).map(PathBuf::from).filter(|dir| dir.has_root());

    match env.os {
        OsKind::Linux => Some(rooted("XDG_CONFIG_HOME").unwrap_or_else(|| env.home.join(".config"))),
        OsKind::MacOs => Some(env.home.join("Library").join("Application Support")),
        OsKind::Windows => rooted("APPDATA"),
    }
}

/// The config file and whether [`CONFIG_VAR`] named it (then it has to exist).
pub fn config_path(env: &Env) -> Result<Option<(PathBuf, bool)>, BackendError> {
    if let Some(raw) = env.var(CONFIG_VAR) {
        let path = env.expand_home(raw);

        if !path.has_root() {
            return Err(BackendError::ConfigRelative);
        }

        return Ok(Some((path, true)));
    }

    Ok(config_dir(env).map(|dir| (dir.join("harness-memory").join(CONFIG_FILE), false)))
}

/// Picks the backend from the environment and the config file (see the module docs).
pub fn select(env: &Env) -> Result<Selection, BackendError> {
    let mut warnings: Vec<String> = Vec::new();
    let mut config = ConfigFile::default();

    if let Some((path, named)) = config_path(env)? {
        match read_config(env, &path) {
            Ok(text) => {
                config = parse_config(&path, &text)?;
                warnings.extend(permission_warning(&path));
            }
            Err(ConfigRead::Missing) if !named => {}
            Err(ConfigRead::Missing) => return Err(BackendError::ConfigMissing(path)),
            Err(ConfigRead::Refused(error)) => return Err(error),
        }
    }

    let url = env.var(URL_VAR).map(str::to_owned).or(config.database_url);
    let token = env.var(TOKEN_VAR).map(str::to_owned).or(config.database_token);

    Ok(Selection {
        backend: backend_for(url, token)?,
        warnings,
    })
}

/// Why [`read_config`] gave no text.
enum ConfigRead {
    Missing,
    Refused(BackendError),
}

/// The text of the config file at `path`, after checking (on Unix) that only its owner, the user
/// running the server, may change it. The checks look at the opened file, so a file swapped in
/// between the check and the read is not used.
fn read_config(env: &Env, path: &Path) -> Result<String, ConfigRead> {
    use std::io::Read;

    let unreadable = |error: std::io::Error| {
        ConfigRead::Refused(BackendError::ConfigUnreadable {
            path: path.to_owned(),
            reason: error.kind().to_string(),
        })
    };

    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(ConfigRead::Missing),
        Err(error) => return Err(unreadable(error)),
    };

    let metadata = file.metadata().map_err(unreadable)?;
    check_config_owner(env, path, &metadata).map_err(ConfigRead::Refused)?;

    let mut text = String::new();
    file.read_to_string(&mut text).map_err(unreadable)?;
    Ok(text)
}

/// Refuses a config file the group or others may write, or that another user owns (Unix only).
#[cfg(unix)]
fn check_config_owner(env: &Env, path: &Path, metadata: &std::fs::Metadata) -> Result<(), BackendError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if writable_by_others(metadata.permissions().mode()) {
        return Err(BackendError::ConfigWritable(path.to_owned()));
    }

    match current_uid(env) {
        Some(uid) if uid != metadata.uid() => Err(BackendError::ConfigNotOwned(path.to_owned())),
        _ => Ok(()),
    }
}

#[cfg(not(unix))]
fn check_config_owner(_env: &Env, _path: &Path, _metadata: &std::fs::Metadata) -> Result<(), BackendError> {
    Ok(())
}

/// The user the server runs as (its effective user id).
#[cfg(unix)]
fn current_uid(_env: &Env) -> Option<u32> {
    Some(nix::unistd::geteuid().as_raw())
}

/// The backend for a URL (none or blank: [`Backend::Local`]) and a token.
pub fn backend_for(url: Option<String>, token: Option<String>) -> Result<Backend, BackendError> {
    let Some(url) = url.map(|url| url.trim().to_owned()).filter(|url| !url.is_empty()) else {
        return Ok(Backend::Local);
    };

    let token = token
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty());

    // The stores only take lowercase schemes, so `POSTGRES://` is passed on as `postgres://`.
    let (scheme, url) = match url.split_once("://") {
        Some((scheme, rest)) => {
            let scheme = scheme.to_ascii_lowercase();
            let url = format!("{scheme}://{rest}");
            (scheme, url)
        }
        None => (String::new(), url),
    };

    match scheme.as_str() {
        "postgres" | "postgresql" => Ok(Backend::Postgres { url }),
        "libsql" | "https" | "http" if carries_secrets(&url) => Err(BackendError::LibsqlUrlSecrets),
        "libsql" | "https" => Ok(Backend::Libsql { url, token }),
        "http" if is_local_http(&url) => Ok(Backend::Libsql { url, token }),
        "http" => Err(BackendError::PlainHttp),
        "mongodb" | "mongodb+srv" => Ok(Backend::Mongo { url }),
        "" => Err(BackendError::Scheme(String::new())),
        other if other.len() <= 16 && other.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)) => {
            Err(BackendError::Scheme(format!(" (got {other}://)")))
        }
        _ => Err(BackendError::Scheme(String::new())),
    }
}

/// A store opened for a team backend, and whether its database was written by a newer server.
pub struct Opened {
    pub store: Box<dyn MemoryStore>,
    pub newer: bool,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("newer", &self.newer)
            .finish_non_exhaustive()
    }
}

/// Connects to a team backend. The error names the backend ([`Backend::describe`]) and is
/// scrubbed of its secrets. `None` for [`Backend::Local`], which `main` opens itself.
pub fn open(backend: &Backend) -> Option<Result<Opened, String>> {
    let opened = match backend {
        Backend::Local => return None,
        Backend::Postgres { url } => PostgresStore::connect(url).map(|store| Opened {
            newer: store.is_newer(),
            store: Box::new(store),
        }),
        Backend::Libsql { url, token } => open_libsql(url, token.as_deref()),
        Backend::Mongo { url } => open_mongo(url),
    };

    Some(opened.map_err(|error| {
        let reason = backend.secrets().scrub(&error.to_string());
        format!("cannot open the team database ({}): {reason}", backend.describe())
    }))
}

fn open_libsql(url: &str, token: Option<&str>) -> Result<Opened, StoreError> {
    LibsqlStore::connect(url, token).map(|store| Opened {
        newer: store.is_newer(),
        store: Box::new(store),
    })
}

fn open_mongo(url: &str) -> Result<Opened, StoreError> {
    MongoStore::connect(url).map(|store| Opened {
        newer: store.is_newer(),
        store: Box::new(store),
    })
}

/// `url` without its password and query: `scheme://user@host:port/path`, with `?…` when the URL
/// had a query (which can hold a token). Only the part after the last `@` is shown as the host,
/// so a password with an unescaped `@`, `/` or `?` in it is never shown.
pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.trim().split_once("://") else {
        return MASK.to_owned();
    };

    let (user, after) = match rest.rfind('@') {
        Some(at) => {
            let userinfo = &rest[..at];
            let user = userinfo.split(':').next().unwrap_or_default();
            (Some(user), &rest[at + 1..])
        }
        None => (None, rest),
    };

    let end = after.find(['?', '#']).unwrap_or(after.len());
    let location = &after[..end];
    let query = if end < after.len() { "?…" } else { "" };

    match user.filter(|user| !user.is_empty()) {
        Some(user) => format!("{scheme}://{user}@{location}{query}"),
        None => format!("{scheme}://{location}{query}"),
    }
}

/// The secret parts of a URL and a token, masked out of messages by [`Secrets::scrub`].
#[derive(Default, Clone, PartialEq, Eq)]
pub struct Secrets(Vec<String>);

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secrets({})", self.0.len())
    }
}

impl Secrets {
    /// The whole URL, its password (as written and decoded), the values of query parameters that
    /// look like credentials, and the token.
    pub fn of(url: &str, token: Option<&str>) -> Self {
        let mut secrets: Vec<String> = vec![url.trim().to_owned()];

        if let Some((_, rest)) = url.split_once("://")
            && let Some(at) = rest.rfind('@')
            && let Some((_, password)) = rest[..at].split_once(':')
        {
            secrets.push(password.to_owned());
            secrets.push(percent_decode_lossy(password));
        }

        if let Some((_, query)) = url.split_once('?') {
            for (key, value) in query.split('&').filter_map(|pair| pair.split_once('=')) {
                let key = key.to_ascii_lowercase();
                let sensitive = ["password", "token", "secret", "key", "auth"]
                    .iter()
                    .any(|word| key.contains(word));

                if sensitive {
                    secrets.push(value.to_owned());
                    secrets.push(percent_decode_lossy(value));
                }
            }
        }

        secrets.extend(token.map(str::to_owned));
        secrets.retain(|secret| !secret.is_empty());
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();
        Self(secrets)
    }

    /// `message` with every secret replaced by `••••`, then through the general secret filter.
    pub fn scrub(&self, message: &str) -> String {
        let mut out = message.to_owned();

        for secret in &self.0 {
            out = out.replace(secret.as_str(), MASK);
        }

        redact_text(&out)
    }
}

/// The warning for a config file other users can read (Unix; `None` elsewhere or when private).
pub fn permission_warning(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = std::fs::metadata(path).ok()?.permissions().mode();
        readable_by_others(mode).then(|| {
            format!(
                "{} can be read by other users and may hold the database password; run chmod 600 on it",
                path.display()
            )
        })
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// True when the group or everyone may read a file with Unix `mode`.
fn readable_by_others(mode: u32) -> bool {
    mode & 0o044 != 0
}

/// True when the group or everyone may write a file with Unix `mode`.
#[cfg(unix)]
fn writable_by_others(mode: u32) -> bool {
    mode & 0o022 != 0
}

/// Parses the config file. A TOML error is reported by its message and line only: the parser's
/// own report quotes the line, which can hold the URL.
fn parse_config(path: &Path, text: &str) -> Result<ConfigFile, BackendError> {
    toml::from_str(text).map_err(|error: toml::de::Error| {
        let line = error
            .span()
            .map(|span| text[..span.start.min(text.len())].matches('\n').count() + 1);
        let message = error.message().lines().next().unwrap_or("invalid TOML").to_owned();
        let reason = match line {
            Some(line) => format!("{message} (line {line})"),
            None => message,
        };

        BackendError::ConfigInvalid {
            path: path.to_owned(),
            reason: Secrets::of(text, None).scrub(&reason),
        }
    })
}

/// True when a URL has a user or password, a query or a fragment (where a token could hide).
fn carries_secrets(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();

    authority.contains('@') || rest.contains(['?', '#'])
}

/// True when an `http://` URL points at this machine (`localhost` or a loopback address).
fn is_local_http(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();

    let host = match authority.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };

    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Decodes `%XX` escapes, keeping anything malformed as written.
fn percent_decode_lossy(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        let escaped = (bytes[index] == b'%')
            .then(|| raw.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());

        match escaped {
            Some(byte) => {
                out.push(byte);
                index += 3;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(url: &str) -> Result<Backend, BackendError> {
        backend_for(Some(url.to_owned()), None)
    }

    #[test]
    fn the_scheme_picks_the_backend() {
        assert_eq!(backend_for(None, None).ok(), Some(Backend::Local));
        assert_eq!(pick("   ").ok(), Some(Backend::Local));
        assert!(matches!(pick("postgres://db/m"), Ok(Backend::Postgres { .. })));
        assert!(matches!(pick("PostgreSQL://db/m"), Ok(Backend::Postgres { .. })));
        assert!(matches!(pick("libsql://team.turso.io"), Ok(Backend::Libsql { .. })));
        assert!(matches!(pick("https://sqld.example.com"), Ok(Backend::Libsql { .. })));
        assert!(matches!(pick("mongodb://db:27017/m"), Ok(Backend::Mongo { .. })));
        assert!(matches!(
            pick("mongodb+srv://cluster.example.net/m"),
            Ok(Backend::Mongo { .. })
        ));
        assert!(matches!(pick("mysql://db/m"), Err(BackendError::Scheme(_))));
        assert!(matches!(
            pick("host=db user=u password=p"),
            Err(BackendError::Scheme(_))
        ));
    }

    #[test]
    fn an_uppercase_scheme_is_passed_on_in_lowercase() {
        assert_eq!(
            pick("POSTGRES://Alice:PassWord@DB.example.com/Memory?SSLMode=require").ok(),
            Some(Backend::Postgres {
                url: "postgres://Alice:PassWord@DB.example.com/Memory?SSLMode=require".to_owned()
            })
        );
        assert_eq!(
            pick("MongoDB+SRV://Cluster.example.net/M").ok(),
            Some(Backend::Mongo {
                url: "mongodb+srv://Cluster.example.net/M".to_owned()
            })
        );
        assert_eq!(
            pick(" LIBSQL://Team.turso.io ").ok(),
            Some(Backend::Libsql {
                url: "libsql://Team.turso.io".to_owned(),
                token: None
            })
        );
        assert!(
            matches!(pick("HTTP://localhost:8080"), Ok(Backend::Libsql { url, .. }) if url == "http://localhost:8080")
        );
    }

    #[test]
    fn plain_http_is_for_this_machine_only() {
        for local in ["http://localhost:8080", "http://127.0.0.1:8080/", "http://[::1]:8080"] {
            assert!(matches!(pick(local), Ok(Backend::Libsql { .. })), "{local}");
        }

        for remote in [
            "http://sqld.example.com",
            "http://127.0.0.1.example.com",
            "http://10.0.0.5:8080",
        ] {
            assert!(matches!(pick(remote), Err(BackendError::PlainHttp)), "{remote}");
        }
    }

    #[test]
    fn libsql_urls_carry_no_credentials() {
        for url in [
            "libsql://team.turso.io?authToken=abc",
            "https://u:p@sqld.example.com",
            "http://localhost@evil.example.com",
            "libsql://team.turso.io#token",
        ] {
            assert!(matches!(pick(url), Err(BackendError::LibsqlUrlSecrets)), "{url}");
        }

        assert!(!BackendError::LibsqlUrlSecrets.to_string().contains("abc"));
    }

    #[test]
    fn the_token_goes_with_libsql() {
        let backend = backend_for(Some("libsql://db".to_owned()), Some(" tok ".to_owned())).expect("libsql");
        assert_eq!(
            backend,
            Backend::Libsql {
                url: "libsql://db".to_owned(),
                token: Some("tok".to_owned())
            }
        );
    }

    #[test]
    fn redacted_urls_keep_the_place_and_drop_the_secrets() {
        assert_eq!(
            redact_url("postgres://alice:hunter2@db.example.com:5432/memory?sslmode=require"),
            "postgres://alice@db.example.com:5432/memory?…"
        );
        assert_eq!(
            redact_url("mongodb://u:pw@db.example.com/m?authSource=admin"),
            "mongodb://u@db.example.com/m?…"
        );
        assert_eq!(redact_url("postgres://u:p@ss/w?rd@db/m"), "postgres://u@db/m");
        assert_eq!(redact_url("not a url"), MASK);

        let shown = format!("{:?}", pick("mongodb://u:hunter2@db/m").expect("mongo"));
        assert!(!shown.contains("hunter2"), "{shown}");
    }

    #[test]
    fn scrubbing_masks_the_password_token_and_url() {
        let secrets = Secrets::of(
            "postgres://u:p%40ss@db/m?password=other&sslmode=require",
            Some("tok-123"),
        );
        let message = "failed for postgres://u:p%40ss@db/m?password=other&sslmode=require: p@ss, other, tok-123";
        let scrubbed = secrets.scrub(message);

        for secret in ["p%40ss", "p@ss", "other", "tok-123"] {
            assert!(!scrubbed.contains(secret), "{scrubbed}");
        }
    }

    #[test]
    fn the_environment_wins_over_the_config_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "database_url = \"postgres://file/m\"\ndatabase_token = \"file-token\"\n",
        )
        .expect("write");

        let env = Env::for_tests("/home/u").with_var(CONFIG_VAR, path.to_string_lossy());
        assert_eq!(
            select(&env).expect("file").backend,
            Backend::Postgres {
                url: "postgres://file/m".to_owned()
            }
        );

        let env = env
            .with_var(URL_VAR, "libsql://env.example.com")
            .with_var(TOKEN_VAR, "");
        assert_eq!(
            select(&env).expect("env").backend,
            Backend::Libsql {
                url: "libsql://env.example.com".to_owned(),
                token: Some("file-token".to_owned())
            }
        );
    }

    #[test]
    fn config_file_problems_are_errors_without_secrets() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        let env = Env::for_tests("/home/u").with_var(CONFIG_VAR, path.to_string_lossy());

        assert!(matches!(select(&env), Err(BackendError::ConfigMissing(_))));

        std::fs::write(&path, "database_ulr = \"postgres://u:hunter2@db/m\"\n").expect("write");
        let typo = select(&env).expect_err("unknown key").to_string();
        assert!(typo.contains("database_ulr") && typo.contains("line 1"), "{typo}");
        assert!(!typo.contains("hunter2"), "{typo}");

        std::fs::write(&path, "database_url = \"postgres://u:hunter2@db/m\n").expect("write");
        let broken = select(&env).expect_err("unterminated string").to_string();
        assert!(!broken.contains("hunter2"), "{broken}");

        let relative = Env::for_tests("/home/u").with_var(CONFIG_VAR, "config.toml");
        assert!(matches!(select(&relative), Err(BackendError::ConfigRelative)));
    }

    #[test]
    fn the_default_config_file_is_optional() {
        let dir = tempfile::tempdir().expect("temp dir");
        let env = Env::for_tests(dir.path())
            .with_os(OsKind::Linux)
            .with_var("XDG_CONFIG_HOME", dir.path().join("config").to_string_lossy());

        let selection = select(&env).expect("no file");
        assert_eq!(selection.backend, Backend::Local);
        assert!(selection.warnings.is_empty());
    }

    #[test]
    fn config_folders_per_platform() {
        let linux = Env::for_tests("/home/u").with_os(OsKind::Linux);
        assert_eq!(
            config_path(&linux).expect("path"),
            Some((PathBuf::from("/home/u/.config/harness-memory/config.toml"), false))
        );

        let xdg = linux.clone().with_var("XDG_CONFIG_HOME", "relative");
        assert_eq!(config_dir(&xdg), Some(PathBuf::from("/home/u/.config")));

        let mac = Env::for_tests("/Users/u").with_os(OsKind::MacOs);
        assert_eq!(
            config_dir(&mac),
            Some(PathBuf::from("/Users/u").join("Library").join("Application Support"))
        );

        let windows = Env::for_tests("C:/Users/u").with_os(OsKind::Windows);
        assert_eq!(config_dir(&windows), None);

        let roaming = std::env::temp_dir().join("AppData").join("Roaming");
        let windows = windows.with_var("APPDATA", roaming.to_string_lossy());
        assert_eq!(config_dir(&windows), Some(roaming));
    }

    #[test]
    fn group_or_world_readable_modes_are_reported() {
        assert!(!readable_by_others(0o100600));
        assert!(!readable_by_others(0o100700));
        assert!(readable_by_others(0o100640));
        assert!(readable_by_others(0o100604));
    }

    #[cfg(unix)]
    #[test]
    fn a_readable_config_file_gets_a_warning() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "database_url = \"postgres://db/m\"\n").expect("write");
        let env = Env::for_tests("/home/u").with_var(CONFIG_VAR, path.to_string_lossy());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let warnings = select(&env).expect("select").warnings;
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("chmod 600"), "{warnings:?}");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(select(&env).expect("select").warnings.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_config_file_others_can_change_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let named = dir.path().join("named.toml");
        let default = dir.path().join("config").join("harness-memory").join(CONFIG_FILE);
        std::fs::create_dir_all(default.parent().expect("parent")).expect("mkdir");

        let by_variable = Env::for_tests(dir.path()).with_var(CONFIG_VAR, named.to_string_lossy());
        let by_default = Env::for_tests(dir.path())
            .with_os(OsKind::Linux)
            .with_var("XDG_CONFIG_HOME", dir.path().join("config").to_string_lossy());

        for (path, env) in [(&named, &by_variable), (&default, &by_default)] {
            std::fs::write(path, "database_url = \"postgres://u:hunter2@db/m\"\n").expect("write");

            for mode in [0o620, 0o602, 0o666] {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
                let refused = select(env).expect_err("writable by others");
                assert!(
                    matches!(refused, BackendError::ConfigWritable(_)),
                    "{mode:o}: {refused:?}"
                );

                let message = refused.to_string();
                assert!(
                    message.contains("chmod 600") && !message.contains("hunter2"),
                    "{message}"
                );
            }

            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
            assert!(matches!(
                select(env).expect("private and owned").backend,
                Backend::Postgres { .. }
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_server_user_is_known() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let mine = std::fs::metadata(dir.path()).expect("metadata").uid();

        assert_eq!(current_uid(&Env::for_tests(dir.path())), Some(mine));
    }
}
