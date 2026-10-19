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
//! store is remote by default: repository and global notes are shared by everyone using it,
//! nothing is kept locally, and the move from Harness Desktop's folder does not run.
//!
//! [`Routing`] can keep one scope on this computer instead: `repository_notes` and `global_notes`
//! in the config file (or [`REPOSITORY_NOTES_VAR`] and [`GLOBAL_NOTES_VAR`]) are `"remote"` (the
//! default) or `"local"`. With one of each the server reads and writes through a
//! [`crate::store::SplitStore`]; with both `"local"` the team database is not used for notes.
//! Scripts follow `scripts` (or [`SCRIPTS_VAR`]) the same way, `"remote"` by default.
//! `trust_team_scripts = true` (or [`TRUST_TEAM_SCRIPTS_VAR`] `1`) lets every script read from the
//! team database run without a person approving it on this computer first.
//!
//! Team hooks follow `hooks` (or [`HOOKS_VAR`]), `"local"` by default: only `"remote"` with a team
//! database shares them through it, otherwise `harness-memory hooks` lists none.
//! `trust_team_hooks` (or [`TRUST_TEAM_HOOKS_VAR`]) is only passed on to Harness Desktop, which
//! decides whether a team hook runs without approval (this server never runs a hook). Team
//! routines follow `routines` (or [`ROUTINES_VAR`]) and `trust_team_routines` (or
//! [`TRUST_TEAM_ROUTINES_VAR`]) the same way ([`Selection::docs_place`]).
//!
//! The URL and the token are secrets: errors show a [`redact_url`] form and pass through
//! [`Secrets::scrub`]. On Unix a config file other users can change, or that another user owns, is
//! refused, and one other users can read is reported (see [`permission_warning`]).

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::store::{
    DocKind, LibsqlStore, MemoryStore, MongoStore, PostgresStore, ScriptStore, StoreError, TeamDocStore,
};
use crate::support::redact::redact_text;
use crate::support::{Env, OsKind};

/// Environment variable holding the team database URL (empty = unset). Wins over the config file.
pub const URL_VAR: &str = "HARNESS_MEMORY_DATABASE_URL";

/// Environment variable holding the libSQL auth token (empty = unset). Wins over the config file.
pub const TOKEN_VAR: &str = "HARNESS_MEMORY_DATABASE_TOKEN";

/// Environment variable naming the config file instead of the default one (absolute, a leading
/// `~` is fine).
pub const CONFIG_VAR: &str = "HARNESS_MEMORY_CONFIG";

/// Environment variable keeping repository notes in the team database (`remote`, the default) or
/// on this computer (`local`). Wins over `repository_notes` in the config file; empty = unset.
pub const REPOSITORY_NOTES_VAR: &str = "HARNESS_MEMORY_REPOSITORY_NOTES";

/// Environment variable keeping global notes in the team database (`remote`, the default) or on
/// this computer (`local`). Wins over `global_notes` in the config file; empty = unset.
pub const GLOBAL_NOTES_VAR: &str = "HARNESS_MEMORY_GLOBAL_NOTES";

/// Environment variable keeping scripts in the team database (`remote`, the default) or on this
/// computer (`local`). Wins over `scripts` in the config file; empty = unset.
pub const SCRIPTS_VAR: &str = "HARNESS_MEMORY_SCRIPTS";

/// Environment variable trusting every script read from the team database (`1`) or not (`0`, the
/// default). Wins over `trust_team_scripts` in the config file; empty = unset.
pub const TRUST_TEAM_SCRIPTS_VAR: &str = "HARNESS_MEMORY_TRUST_TEAM_SCRIPTS";

/// Environment variable sharing team hooks through the team database (`remote`) or not (`local`,
/// the default). Wins over `hooks` in the config file; empty = unset.
pub const HOOKS_VAR: &str = "HARNESS_MEMORY_HOOKS";

/// Environment variable telling Harness Desktop to run team hooks without approval (`1`) or not
/// (`0`, the default). Wins over `trust_team_hooks` in the config file; empty = unset.
pub const TRUST_TEAM_HOOKS_VAR: &str = "HARNESS_MEMORY_TRUST_TEAM_HOOKS";

/// Environment variable sharing team routines through the team database (`remote`) or not
/// (`local`, the default). Wins over `routines` in the config file; empty = unset.
pub const ROUTINES_VAR: &str = "HARNESS_MEMORY_ROUTINES";

/// Environment variable telling Harness Desktop to run team routines a person turned on without
/// approving each version (`1`) or not (`0`, the default). Wins over `trust_team_routines` in the
/// config file; empty = unset.
pub const TRUST_TEAM_ROUTINES_VAR: &str = "HARNESS_MEMORY_TRUST_TEAM_ROUTINES";

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
    repository_notes: Option<String>,
    global_notes: Option<String>,
    scripts: Option<String>,
    trust_team_scripts: Option<bool>,
    hooks: Option<String>,
    trust_team_hooks: Option<bool>,
    routines: Option<String>,
    trust_team_routines: Option<bool>,
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

    /// The schema version this harness-memory brings the backend's database to.
    pub fn schema_version(&self) -> i64 {
        match self {
            Self::Local | Self::Libsql { .. } => crate::store::sqlite::schema::SCHEMA_VERSION,
            Self::Postgres { .. } => crate::store::postgres::schema::SCHEMA_VERSION,
            Self::Mongo { .. } => crate::store::mongo::schema::SCHEMA_VERSION,
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

/// Where one scope's notes are kept when a team database is set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Place {
    /// In the team database (`remote`).
    #[default]
    Remote,
    /// In the local SQLite file of this computer (`local`).
    Local,
}

impl Place {
    /// `remote` or `local`, as the settings write it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::Local => "local",
        }
    }

    /// Where it is, in words: `team database` or `this computer`.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Remote => "team database",
            Self::Local => "this computer",
        }
    }

    /// Parses a setting (`remote` or `local`, any case, surrounding blanks ignored).
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "remote" => Some(Self::Remote),
            "local" => Some(Self::Local),
            _ => None,
        }
    }
}

/// Where repository notes, global notes and scripts go when a team database is set; all
/// [`Place::Remote`] by default. Without a team database everything is local whatever it says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Routing {
    pub repository: Place,
    pub global: Place,
    pub scripts: Place,
}

impl Routing {
    /// True when one scope is in the team database and the other on this computer.
    pub fn is_split(self) -> bool {
        self.repository != self.global
    }

    /// True when both note scopes stay on this computer, so the team database is not used for
    /// notes (scripts follow [`Routing::scripts`]).
    pub fn is_all_local(self) -> bool {
        self.repository == Place::Local && self.global == Place::Local
    }
}

/// The backend [`select`] picked and the routing of the scopes, with warnings to print first.
#[derive(Debug)]
pub struct Selection {
    pub backend: Backend,
    pub routing: Routing,
    /// Whether every script read from the team database counts as approved here.
    pub trust_team_scripts: bool,
    /// Where team hooks go: [`Place::Local`] (the default) shares none.
    pub hooks: Place,
    /// Whether Harness Desktop runs team hooks without approval (`trust_team_hooks`).
    pub trust_team_hooks: bool,
    /// Where team routines go: [`Place::Local`] (the default) shares none.
    pub routines: Place,
    /// Whether Harness Desktop runs the team routines a person turned on without approval
    /// (`trust_team_routines`).
    pub trust_team_routines: bool,
    pub warnings: Vec<String>,
}

impl Selection {
    /// Where scripts are kept: [`Routing::scripts`] with a team database, else this computer.
    pub fn scripts_place(&self) -> Place {
        match self.backend {
            Backend::Local => Place::Local,
            _ => self.routing.scripts,
        }
    }

    /// The name this machine's script trust gives the place scripts are kept: `local`, or
    /// `team:` and [`Backend::team_key`], so two team databases never share an approval.
    pub fn scripts_key(&self) -> String {
        match self.scripts_place() {
            Place::Local => LOCAL_KEY.to_owned(),
            Place::Remote => self.backend.team_key(),
        }
    }

    /// True when the scripts come from the team database and `trust_team_scripts` is on.
    pub fn team_scripts_trusted(&self) -> bool {
        self.trust_team_scripts && self.scripts_place() == Place::Remote
    }

    /// True when team hooks are shared through the team database and `trust_team_hooks` is on.
    pub fn team_hooks_trusted(&self) -> bool {
        self.team_docs_trusted(DocKind::Hooks)
    }

    /// Where team hooks are shared: [`Selection::hooks`] with a team database, else this computer
    /// (where there are none).
    pub fn hooks_place(&self) -> Place {
        self.docs_place(DocKind::Hooks)
    }

    /// True when team routines are shared through the team database and `trust_team_routines` is
    /// on.
    pub fn team_routines_trusted(&self) -> bool {
        self.team_docs_trusted(DocKind::Routines)
    }

    /// Where team routines are shared: [`Selection::routines`] with a team database, else this
    /// computer (where there are none).
    pub fn routines_place(&self) -> Place {
        self.docs_place(DocKind::Routines)
    }

    /// Where the team documents of `kind` are shared: its setting with a team database, else this
    /// computer (where there are none).
    pub fn docs_place(&self, kind: DocKind) -> Place {
        let place = match kind {
            DocKind::Hooks => self.hooks,
            DocKind::Routines => self.routines,
        };

        match self.backend {
            Backend::Local => Place::Local,
            _ => place,
        }
    }

    /// True when the team documents of `kind` are shared and their trust setting is on.
    pub fn team_docs_trusted(&self, kind: DocKind) -> bool {
        let trusted = match kind {
            DocKind::Hooks => self.trust_team_hooks,
            DocKind::Routines => self.trust_team_routines,
        };

        trusted && self.docs_place(kind) == Place::Remote
    }
}

/// The trust name of the scripts on this computer (see [`Selection::scripts_key`]).
pub const LOCAL_KEY: &str = "local";

impl Backend {
    /// `team:` and the first 16 hex digits of the SHA-256 of [`Backend::team_identity`]; `local`
    /// for the local database.
    pub fn team_key(&self) -> String {
        use sha2::{Digest, Sha256};

        let Some(identity) = self.team_identity() else {
            return LOCAL_KEY.to_owned();
        };

        let digest = Sha256::digest(identity.as_bytes());
        let hex: String = digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect();
        format!("team:{hex}")
    }

    /// The team database without credentials: its kind, hosts and database name
    /// (`postgres://db.example.com/memory`). The hosts are in lowercase, without the scheme's
    /// default port (PostgreSQL 5432, MongoDB 27017, libSQL over HTTPS 443, over HTTP 80) and,
    /// for MongoDB, sorted. The user, password, query and token are left out, so changing them,
    /// or writing the same database another way, keeps the approvals.
    pub fn team_identity(&self) -> Option<String> {
        let (kind, url) = match self {
            Self::Local => return None,
            Self::Postgres { url } => ("postgres", url),
            Self::Libsql { url, .. } => ("libsql", url),
            Self::Mongo { url } if url.starts_with("mongodb+srv://") => ("mongodb+srv", url),
            Self::Mongo { url } => ("mongodb", url),
        };

        let (scheme, rest) = url.trim().split_once("://").unwrap_or(("", url.trim()));
        let rest = &rest[..rest.find(['?', '#']).unwrap_or(rest.len())];
        let location = rest.rfind('@').map_or(rest, |at| &rest[at + 1..]);
        let (hosts, database) = location.split_once('/').unwrap_or((location, ""));
        let database = database.trim_end_matches('/');

        let database = match (self, database) {
            (Self::Mongo { .. }, "") => crate::store::mongo::DEFAULT_DATABASE,
            _ => database,
        };

        let default_port = match scheme.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => Some("5432"),
            "mongodb" => Some("27017"),
            "libsql" | "https" => Some("443"),
            "http" => Some("80"),
            _ => None,
        };

        let mut hosts: Vec<String> = hosts
            .split(',')
            .filter(|host| !host.is_empty())
            .map(|host| without_port(&host.to_ascii_lowercase(), default_port))
            .collect();

        if matches!(self, Self::Mongo { .. }) {
            hosts.sort();
        }

        Some(format!("{kind}://{}/{database}", hosts.join(",")))
    }
}

/// `host` without `:<port>` when it is `default` (`[::1]:5432` keeps its brackets).
fn without_port(host: &str, default: Option<&str>) -> String {
    match (host.rsplit_once(':'), default) {
        (Some((name, port)), Some(default)) if port == default && !name.is_empty() => name.to_owned(),
        _ => host.to_owned(),
    }
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
    #[error("{0} must be \"remote\" or \"local\"")]
    Routing(String),
    #[error("{0} must be 1 or 0")]
    Flag(String),
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
    let backend = backend_for(url, token)?;

    let routing = Routing {
        repository: place(
            env,
            REPOSITORY_NOTES_VAR,
            config.repository_notes.as_deref(),
            "repository_notes",
            Place::Remote,
        )?,
        global: place(
            env,
            GLOBAL_NOTES_VAR,
            config.global_notes.as_deref(),
            "global_notes",
            Place::Remote,
        )?,
        scripts: place(env, SCRIPTS_VAR, config.scripts.as_deref(), "scripts", Place::Remote)?,
    };

    if backend != Backend::Local && routing.is_all_local() {
        let used = match routing.scripts {
            Place::Local => "using the local database only",
            Place::Remote => "notes use the local database, scripts the team database",
        };

        warnings.push(format!(
            "the team database ({}) is set, but repository_notes and global_notes are both \"local\": {used}",
            backend.describe()
        ));
    }

    let trust_team_scripts = flag(env, TRUST_TEAM_SCRIPTS_VAR, config.trust_team_scripts)?;
    let hooks = place(env, HOOKS_VAR, config.hooks.as_deref(), "hooks", Place::Local)?;
    let trust_team_hooks = flag(env, TRUST_TEAM_HOOKS_VAR, config.trust_team_hooks)?;
    let routines = place(env, ROUTINES_VAR, config.routines.as_deref(), "routines", Place::Local)?;
    let trust_team_routines = flag(env, TRUST_TEAM_ROUTINES_VAR, config.trust_team_routines)?;

    Ok(Selection {
        backend,
        routing,
        trust_team_scripts,
        hooks,
        trust_team_hooks,
        routines,
        trust_team_routines,
        warnings,
    })
}

/// The switch from the environment variable `var` (`1`, `true`, `0` or `false`, any case), else
/// from the config file (off when neither is set); an error naming `var` for another value.
fn flag(env: &Env, var: &str, file: Option<bool>) -> Result<bool, BackendError> {
    let Some(raw) = env.var(var) else {
        return Ok(file.unwrap_or(false));
    };

    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        _ => Err(BackendError::Flag(var.to_owned())),
    }
}

/// The [`Place`] from the environment variable `var`, else from the config file's `key` (`default`
/// when neither is set); an error naming where the unknown value came from.
fn place(env: &Env, var: &str, file: Option<&str>, key: &str, default: Place) -> Result<Place, BackendError> {
    let (raw, origin) = match env.var(var) {
        Some(raw) => (Some(raw), var.to_owned()),
        None => (file, format!("{key} in the config file")),
    };

    match raw {
        None => Ok(default),
        Some(raw) => Place::parse(raw).ok_or(BackendError::Routing(origin)),
    }
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

    Some(opened.map_err(|error| open_failed(backend, &error)))
}

/// Connects to a team backend for scripts, like [`open`].
pub fn open_scripts(backend: &Backend) -> Option<Result<Box<dyn ScriptStore>, String>> {
    let opened: Result<Box<dyn ScriptStore>, StoreError> = match backend {
        Backend::Local => return None,
        Backend::Postgres { url } => PostgresStore::connect(url).map(|store| Box::new(store) as Box<dyn ScriptStore>),
        Backend::Libsql { url, token } => {
            LibsqlStore::connect(url, token.as_deref()).map(|store| Box::new(store) as Box<dyn ScriptStore>)
        }
        Backend::Mongo { url } => MongoStore::connect(url).map(|store| Box::new(store) as Box<dyn ScriptStore>),
    };

    Some(opened.map_err(|error| open_failed(backend, &error)))
}

/// Connects to a team backend for team documents (hooks, routines), like [`open`].
pub fn open_team_docs(backend: &Backend) -> Option<Result<Box<dyn TeamDocStore>, String>> {
    let opened: Result<Box<dyn TeamDocStore>, StoreError> = match backend {
        Backend::Local => return None,
        Backend::Postgres { url } => PostgresStore::connect(url).map(|store| Box::new(store) as Box<dyn TeamDocStore>),
        Backend::Libsql { url, token } => {
            LibsqlStore::connect(url, token.as_deref()).map(|store| Box::new(store) as Box<dyn TeamDocStore>)
        }
        Backend::Mongo { url } => MongoStore::connect(url).map(|store| Box::new(store) as Box<dyn TeamDocStore>),
    };

    Some(opened.map_err(|error| open_failed(backend, &error)))
}

/// `cannot open the team database (<backend>): <reason>`, scrubbed of the backend's secrets.
fn open_failed(backend: &Backend, error: &StoreError) -> String {
    let reason = backend.secrets().scrub(&error.to_string());
    format!("cannot open the team database ({}): {reason}", backend.describe())
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
#[cfg(unix)]
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

    #[test]
    fn team_keys_name_the_database_without_its_credentials() {
        let identity = |url: &str| pick(url).expect("backend").team_identity();

        assert_eq!(
            identity("postgres://alice:s3cret@DB.example.com:5432/memory?sslmode=require").as_deref(),
            Some("postgres://db.example.com/memory")
        );
        assert_eq!(
            identity("postgresql://bob@db.example.com:5432/memory/").as_deref(),
            Some("postgres://db.example.com/memory")
        );
        assert_eq!(
            identity("postgres://db.example.com:6543/memory").as_deref(),
            Some("postgres://db.example.com:6543/memory")
        );
        assert_eq!(
            identity("libsql://team-acme.turso.io").as_deref(),
            Some("libsql://team-acme.turso.io/")
        );
        assert_eq!(
            identity("mongodb://u:p@m1:27017,m2:27017/?replicaSet=rs0").as_deref(),
            Some("mongodb://m1,m2/harness_memory")
        );
        assert_eq!(
            identity("mongodb+srv://u:p@cluster.example.net/notes").as_deref(),
            Some("mongodb+srv://cluster.example.net/notes")
        );
        assert_eq!(Backend::Local.team_identity(), None);

        let key = |url: &str| pick(url).expect("backend").team_key();
        let same = key("postgres://alice:one@db.example.com:5432/memory");
        assert_eq!(
            same,
            key("postgres://carol:two@db.example.com:5432/memory?sslmode=require")
        );
        assert_ne!(same, key("postgres://alice:one@db.example.com:5432/other"));
        assert_ne!(same, key("postgres://alice:one@db.example.com:6543/memory"));
        assert_ne!(same, key("postgres://alice:one@db2.example.com:5432/memory"));
        assert!(same.starts_with("team:") && same.len() == 21, "{same}");
        assert!(
            same[5..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert_eq!(Backend::Local.team_key(), "local");
    }

    #[test]
    fn team_keys_are_the_same_for_every_way_of_writing_one_database() {
        let key = |url: &str| pick(url).expect("backend").team_key();

        for equivalents in [
            &[
                "postgres://db.example.com/memory",
                "postgresql://alice@DB.example.com:5432/memory/",
                "postgres://db.example.com:5432/memory?options=-c%20user=x@evil.example",
                "postgres://bob:p@ss@db.example.com/memory#x@elsewhere",
            ][..],
            &[
                "mongodb://m1,m2/notes",
                "mongodb://u:p@m2:27017,M1:27017/notes?replicaSet=rs0",
                "mongodb://m2,m1:27017/notes?authSource=admin@other",
            ],
            &[
                "libsql://team.turso.io",
                "libsql://team.turso.io:443",
                "https://TEAM.turso.io:443/",
            ],
            &["http://127.0.0.1:80", "http://127.0.0.1"],
        ] {
            let first = key(equivalents[0]);
            for url in &equivalents[1..] {
                assert_eq!(key(url), first, "{url} and {}", equivalents[0]);
            }
        }

        assert_ne!(key("http://127.0.0.1:8080"), key("http://127.0.0.1"));
        assert_ne!(key("mongodb://m1,m2/notes"), key("mongodb://m1,m3/notes"));
        assert_ne!(key("mongodb://m1:27018/notes"), key("mongodb://m1/notes"));
    }

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
    fn both_scopes_go_to_the_team_database_by_default() {
        let env = Env::for_tests("/home/u").with_var(URL_VAR, "postgres://db/m");
        let selection = select(&env).expect("select");

        assert_eq!(selection.routing, Routing::default());
        assert_eq!(selection.routing.repository, Place::Remote);
        assert_eq!(selection.routing.global, Place::Remote);
        assert!(!selection.routing.is_split());
        assert!(selection.warnings.is_empty());
    }

    #[test]
    fn the_config_file_and_the_environment_route_the_scopes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "database_url = \"postgres://file/m\"\nrepository_notes = \"remote\"\nglobal_notes = \"Local\"\n",
        )
        .expect("write");

        let env = Env::for_tests("/home/u").with_var(CONFIG_VAR, path.to_string_lossy());
        let routing = select(&env).expect("file").routing;
        assert_eq!(
            routing,
            Routing {
                repository: Place::Remote,
                global: Place::Local,
                scripts: Place::Remote,
            }
        );
        assert!(routing.is_split());

        let env = env
            .with_var(REPOSITORY_NOTES_VAR, " local ")
            .with_var(GLOBAL_NOTES_VAR, "remote");
        assert_eq!(
            select(&env).expect("env").routing,
            Routing {
                repository: Place::Local,
                global: Place::Remote,
                scripts: Place::Remote,
            }
        );

        let env = env.with_var(GLOBAL_NOTES_VAR, "");
        assert_eq!(select(&env).expect("empty is unset").routing.global, Place::Local);
    }

    #[test]
    fn team_scripts_are_trusted_only_when_the_settings_say_so() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("config.toml");
        let env = Env::for_tests("/home/u").with_var(CONFIG_VAR, path.to_string_lossy());

        std::fs::write(&path, "database_url = \"postgres://file/m\"\n").expect("write");
        assert!(!select(&env).expect("default").trust_team_scripts);

        std::fs::write(
            &path,
            "database_url = \"postgres://file/m\"\ntrust_team_scripts = true\n",
        )
        .expect("write");
        assert!(select(&env).expect("file").trust_team_scripts);

        let env = env.with_var(TRUST_TEAM_SCRIPTS_VAR, "0");
        assert!(!select(&env).expect("env wins").trust_team_scripts);
        let env = env.with_var(TRUST_TEAM_SCRIPTS_VAR, " TRUE ");
        assert!(select(&env).expect("any case").trust_team_scripts);

        let error = select(&env.with_var(TRUST_TEAM_SCRIPTS_VAR, "yes"))
            .expect_err("bad value")
            .to_string();
        assert_eq!(error, "HARNESS_MEMORY_TRUST_TEAM_SCRIPTS must be 1 or 0");

        std::fs::write(&path, "trust_team_scripts = \"yes\"\n").expect("write");
        let env = Env::for_tests("/home/u").with_var(CONFIG_VAR, path.to_string_lossy());
        assert!(select(&env).is_err(), "a string is not a switch");
    }

    #[test]
    fn scripts_go_to_the_team_database_unless_the_settings_keep_them_local() {
        let team = Env::for_tests("/home/u").with_var(URL_VAR, "postgres://db/m");
        let selection = select(&team).expect("default");
        assert_eq!(selection.routing.scripts, Place::Remote);
        assert_eq!(selection.scripts_place(), Place::Remote);

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "database_url = \"postgres://file/m\"\nscripts = \" Local \"\n").expect("write");

        let env = Env::for_tests("/home/u").with_var(CONFIG_VAR, path.to_string_lossy());
        let selection = select(&env).expect("file");
        assert_eq!(selection.routing.scripts, Place::Local);
        assert_eq!(selection.routing.repository, Place::Remote);
        assert!(!selection.routing.is_all_local());

        let env = env.with_var(SCRIPTS_VAR, "remote");
        assert_eq!(select(&env).expect("env wins").routing.scripts, Place::Remote);

        let env = env.with_var(SCRIPTS_VAR, "");
        assert_eq!(select(&env).expect("empty is unset").routing.scripts, Place::Local);

        let error = select(&env.with_var(SCRIPTS_VAR, "team"))
            .expect_err("unknown value")
            .to_string();
        assert_eq!(error, "HARNESS_MEMORY_SCRIPTS must be \"remote\" or \"local\"");

        std::fs::write(&path, "scripts = \"both\"\n").expect("write");
        let env = Env::for_tests("/home/u").with_var(CONFIG_VAR, path.to_string_lossy());
        let error = select(&env).expect_err("unknown value").to_string();
        assert_eq!(error, "scripts in the config file must be \"remote\" or \"local\"");

        let local = Env::for_tests("/home/u").with_var(SCRIPTS_VAR, "remote");
        assert_eq!(select(&local).expect("no team database").scripts_place(), Place::Local);
    }

    #[test]
    fn hooks_are_shared_only_when_the_settings_say_so_with_a_team_database() {
        let team = Env::for_tests("/home/u").with_var(URL_VAR, "postgres://db/m");
        let selection = select(&team).expect("default");
        assert_eq!((selection.hooks, selection.hooks_place()), (Place::Local, Place::Local));
        assert!(!selection.trust_team_hooks);

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "database_url = \"postgres://file/m\"\nhooks = \"remote\"\ntrust_team_hooks = true\n",
        )
        .expect("write");

        let env = Env::for_tests("/home/u").with_var(CONFIG_VAR, path.to_string_lossy());
        let selection = select(&env).expect("file");
        assert_eq!(selection.hooks_place(), Place::Remote);
        assert!(selection.team_hooks_trusted());
        assert_eq!(
            selection.routing,
            Routing::default(),
            "hooks leave the notes and scripts alone"
        );

        let env = env.with_var(HOOKS_VAR, "local").with_var(TRUST_TEAM_HOOKS_VAR, "0");
        let selection = select(&env).expect("env wins");
        assert_eq!(selection.hooks_place(), Place::Local);
        assert!(!selection.trust_team_hooks);

        let error = select(&env.with_var(HOOKS_VAR, "team"))
            .expect_err("unknown value")
            .to_string();
        assert_eq!(error, "HARNESS_MEMORY_HOOKS must be \"remote\" or \"local\"");

        let no_team = Env::for_tests("/home/u")
            .with_var(HOOKS_VAR, "remote")
            .with_var(TRUST_TEAM_HOOKS_VAR, "1");
        let selection = select(&no_team).expect("no team database");
        assert!(selection.trust_team_hooks && !selection.team_hooks_trusted());
        assert_eq!(
            (selection.hooks, selection.hooks_place()),
            (Place::Remote, Place::Local)
        );
        assert!(selection.warnings.is_empty());
    }

    #[test]
    fn routines_are_shared_only_when_the_settings_say_so_with_a_team_database() {
        let team = Env::for_tests("/home/u").with_var(URL_VAR, "postgres://db/m");
        let selection = select(&team).expect("default");
        assert_eq!(
            (selection.routines, selection.routines_place()),
            (Place::Local, Place::Local)
        );
        assert!(!selection.trust_team_routines);

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "database_url = \"postgres://file/m\"\nroutines = \"remote\"\ntrust_team_routines = true\n",
        )
        .expect("write");

        let env = Env::for_tests("/home/u").with_var(CONFIG_VAR, path.to_string_lossy());
        let selection = select(&env).expect("file");
        assert_eq!(selection.routines_place(), Place::Remote);
        assert!(selection.team_routines_trusted());
        assert_eq!(
            (selection.hooks_place(), selection.team_hooks_trusted()),
            (Place::Local, false),
            "routines leave the hooks alone"
        );
        assert_eq!(selection.routing, Routing::default());

        let env = env
            .with_var(ROUTINES_VAR, "local")
            .with_var(TRUST_TEAM_ROUTINES_VAR, "0");
        let selection = select(&env).expect("env wins");
        assert_eq!(selection.routines_place(), Place::Local);
        assert!(!selection.trust_team_routines);

        let error = select(&env.clone().with_var(ROUTINES_VAR, "team"))
            .expect_err("unknown value")
            .to_string();
        assert_eq!(error, "HARNESS_MEMORY_ROUTINES must be \"remote\" or \"local\"");

        let error = select(&env.with_var(TRUST_TEAM_ROUTINES_VAR, "yes"))
            .expect_err("unknown flag")
            .to_string();
        assert!(error.contains(TRUST_TEAM_ROUTINES_VAR), "{error}");

        let no_team = Env::for_tests("/home/u")
            .with_var(ROUTINES_VAR, "remote")
            .with_var(TRUST_TEAM_ROUTINES_VAR, "1");
        let selection = select(&no_team).expect("no team database");
        assert!(selection.trust_team_routines && !selection.team_routines_trusted());
        assert_eq!(
            (selection.routines, selection.routines_place()),
            (Place::Remote, Place::Local)
        );
    }

    #[test]
    fn unknown_routing_values_are_errors() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "repository_notes = \"team\"\n").expect("write");

        let env = Env::for_tests("/home/u").with_var(CONFIG_VAR, path.to_string_lossy());
        let error = select(&env).expect_err("unknown value").to_string();
        assert_eq!(
            error,
            "repository_notes in the config file must be \"remote\" or \"local\""
        );

        let env = Env::for_tests("/home/u").with_var(GLOBAL_NOTES_VAR, "both");
        let error = select(&env).expect_err("unknown value").to_string();
        assert_eq!(error, "HARNESS_MEMORY_GLOBAL_NOTES must be \"remote\" or \"local\"");

        std::fs::write(&path, "global_notes = 1\n").expect("write");
        let env = Env::for_tests("/home/u").with_var(CONFIG_VAR, path.to_string_lossy());
        assert!(matches!(select(&env), Err(BackendError::ConfigInvalid { .. })));
    }

    #[test]
    fn both_scopes_local_leave_the_team_database_unused_with_a_warning() {
        let env = Env::for_tests("/home/u")
            .with_var(URL_VAR, "postgres://u:hunter2@db/m")
            .with_var(REPOSITORY_NOTES_VAR, "local")
            .with_var(GLOBAL_NOTES_VAR, "local")
            .with_var(SCRIPTS_VAR, "local");
        let selection = select(&env).expect("select");

        assert!(selection.routing.is_all_local());
        assert_eq!(selection.warnings.len(), 1);
        assert!(
            selection.warnings[0].contains("both \"local\": using the local database only"),
            "{:?}",
            selection.warnings
        );
        assert!(!selection.warnings[0].contains("hunter2"));

        let scripts_shared = env.with_var(SCRIPTS_VAR, "remote");
        let warnings = select(&scripts_shared).expect("select").warnings;
        assert_eq!(
            warnings,
            [
                "the team database (PostgreSQL at postgres://u@db/m) is set, but repository_notes and global_notes \
              are both \"local\": notes use the local database, scripts the team database"
            ]
        );

        let local = Env::for_tests("/home/u")
            .with_var(REPOSITORY_NOTES_VAR, "local")
            .with_var(GLOBAL_NOTES_VAR, "local");
        assert!(select(&local).expect("no team database").warnings.is_empty());
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

    #[cfg(unix)]
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
