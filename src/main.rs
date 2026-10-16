//! The `harness-memory` executable: `harness-memory [mcp]` serves MCP over stdio,
//! `harness-memory push [--dry-run] [--repo <key|.>] [--no-global]` copies the local memory into
//! the team database, `harness-memory check` opens the configured database and says which it is,
//! `harness-memory --version` prints the version.

use std::io::{BufReader, Write};
use std::process::ExitCode;

use harness_memory::backend::{self, Backend, CONFIG_VAR, Place, Routing, Selection, URL_VAR};
use harness_memory::location::{DIR_VAR, Migration, legacy_database_path, migrate_legacy, override_is_relative};
use harness_memory::project::{ProjectResolver, normalize_remote};
use harness_memory::push::{Action, GLOBAL, Options, Summary, push};
use harness_memory::store::{MemoryStore, Scope, SplitStore, SqliteStore};
use harness_memory::support::Env;
use harness_memory::{Server, database_path, serve};

const USAGE: &str = "usage: harness-memory [mcp | push [--dry-run] [--repo <key|.>] [--no-global] | check | --version]";

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let args: Vec<Option<&str>> = args.iter().map(|arg| arg.to_str()).collect();

    match args.as_slice() {
        [] | [Some("mcp")] => run(),
        [Some("push"), rest @ ..] => match PushArgs::parse(rest) {
            Some(push_args) => run_push(&push_args),
            None => {
                eprintln!("{USAGE}");
                ExitCode::from(2)
            }
        },
        [Some("check")] => run_check(),
        [Some("--version")] => {
            println!("harness-memory {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn run() -> ExitCode {
    let Ok(env) = Env::from_process() else {
        eprintln!("harness-memory: the home folder is unknown");
        return ExitCode::FAILURE;
    };

    let Some(selection) = select(&env) else {
        return ExitCode::FAILURE;
    };

    let Some(store) = open_store(&env, &selection) else {
        return ExitCode::FAILURE;
    };

    let projects = ProjectResolver::new(env, std::env::current_dir().ok());
    let mut server = Server::new(store, projects);

    let stdin = BufReader::new(std::io::stdin().lock());
    let mut stdout = std::io::stdout().lock();
    let served = serve(stdin, &mut stdout, &mut server);
    stdout.flush().ok();

    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("harness-memory: stdio failed ({})", error.kind());
            ExitCode::FAILURE
        }
    }
}

/// The backend and routing from the environment and the config file, after printing its
/// warnings; `None` (with the reason printed) when the settings are wrong.
fn select(env: &Env) -> Option<Selection> {
    match backend::select(env) {
        Ok(selection) => {
            for warning in &selection.warnings {
                eprintln!("harness-memory: {warning}");
            }

            Some(selection)
        }
        Err(error) => {
            eprintln!("harness-memory: {error}");
            None
        }
    }
}

/// The store the server uses: the local one, the team database, or both split by scope (see
/// [`Routing`]); `None` (with the reason printed) when one cannot be opened.
fn open_store(env: &Env, selection: &Selection) -> Option<Box<dyn MemoryStore>> {
    let Selection { backend, routing, .. } = selection;

    if *backend == Backend::Local || routing.is_all_local() {
        return open_local(env);
    }

    let team = open_team(backend)?;

    if !routing.is_split() {
        return Some(team);
    }

    let local = open_local(env)?;
    let local_scope = if routing.repository == Place::Local {
        Scope::Repo
    } else {
        Scope::Global
    };

    Some(Box::new(SplitStore::new(team, local, local_scope)))
}

/// The team database; `None` (with the reason printed) when it cannot be opened. That stops the
/// server: an empty local database instead would look like the team's memory had gone.
fn open_team(backend: &Backend) -> Option<Box<dyn MemoryStore>> {
    match backend::open(backend)? {
        Ok(opened) => {
            if opened.newer {
                eprintln!("harness-memory: the team database is newer than this server; tools will fail");
            }

            Some(opened.store)
        }
        Err(message) => {
            eprintln!("harness-memory: {message}");
            None
        }
    }
}

/// `harness-memory check`: opens the configured database like the server does and prints one
/// line saying which it is (exit 0), or the reason it cannot be opened on stderr (exit 1). The
/// local database is only looked at, never created or migrated.
fn run_check() -> ExitCode {
    let Ok(env) = Env::from_process() else {
        eprintln!("harness-memory: the home folder is unknown");
        return ExitCode::FAILURE;
    };

    let Some(Selection { backend, routing, .. }) = select(&env) else {
        return ExitCode::FAILURE;
    };

    if backend == Backend::Local || routing.is_all_local() {
        let Some(local) = local_place(&env) else {
            return ExitCode::FAILURE;
        };

        println!("ok: {local}");
        return ExitCode::SUCCESS;
    }

    let opened = match backend::open(&backend) {
        Some(Ok(opened)) => opened,
        Some(Err(message)) => {
            eprintln!("harness-memory: {message}");
            return ExitCode::FAILURE;
        }
        None => return ExitCode::FAILURE,
    };

    let schema = backend.schema_version();
    let mut line = if opened.newer {
        format!("ok: {} (schema newer than {schema})", backend.describe())
    } else {
        format!("ok: {} (schema {schema})", backend.describe())
    };

    if routing.is_split() {
        let Some(local) = local_place(&env) else {
            return ExitCode::FAILURE;
        };

        line.push_str(&format!("; {}; {local}", routing_line(routing)));
    }

    if opened.newer {
        line.push_str("; the team database is newer than this harness-memory; update it");
    }

    println!("{line}");
    ExitCode::SUCCESS
}

/// `local database at <path>`, noting when the file does not exist yet (it is not created);
/// `None` (with the reason printed) when the data folder is unknown.
fn local_place(env: &Env) -> Option<String> {
    let Some(path) = database_path(env) else {
        eprintln!("harness-memory: the data folder is unknown");
        return None;
    };

    if override_is_relative(env) {
        eprintln!(
            "harness-memory: {DIR_VAR} must be an absolute folder; using {}",
            path.display()
        );
    }

    if path.exists() {
        Some(format!("local database at {}", path.display()))
    } else {
        Some(format!("local database at {} (not created yet)", path.display()))
    }
}

/// `repository notes: team database, global notes: this computer`.
fn routing_line(routing: Routing) -> String {
    format!(
        "repository notes: {}, global notes: {}",
        routing.repository.describe(),
        routing.global.describe()
    )
}

/// The local SQLite store, after the one-time move from Harness Desktop's folder; `None` (with
/// the reason printed) when it cannot be opened.
fn open_local(env: &Env) -> Option<Box<dyn MemoryStore>> {
    let Some(path) = database_path(env) else {
        eprintln!("harness-memory: the data folder is unknown");
        return None;
    };

    if override_is_relative(env) {
        eprintln!(
            "harness-memory: {DIR_VAR} must be an absolute folder; using {}",
            path.display()
        );
    }

    // A failed move is not fatal: the server starts on an empty database and the legacy one stays.
    if let Migration::Failed(reason) = migrate_legacy(env, &path) {
        eprintln!(
            "harness-memory: could not copy the Harness Desktop memory database ({reason}); starting empty. \
             The old notes are still in {}; to retry, stop every harness-memory server and delete {}",
            legacy_database_path(env)
                .map(|legacy| legacy.display().to_string())
                .unwrap_or_default(),
            path.display()
        );
    }

    let store = match SqliteStore::open(&path) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("harness-memory: {error}");
            return None;
        }
    };

    if store.is_newer() {
        eprintln!("harness-memory: memory database is newer than this server; tools will fail");
    }

    Some(Box::new(store))
}

/// The options of `harness-memory push`.
#[derive(Debug, Default)]
struct PushArgs {
    dry_run: bool,
    /// `--repo`: a repository key, a remote URL, or `.` for the current checkout's repository.
    repo: Option<String>,
    no_global: bool,
}

impl PushArgs {
    /// The options after `push`; `None` for an unknown, repeated or incomplete one.
    fn parse(args: &[Option<&str>]) -> Option<Self> {
        let mut parsed = Self::default();
        let mut args = args.iter();

        while let Some(arg) = args.next() {
            match (*arg)? {
                "--dry-run" if !parsed.dry_run => parsed.dry_run = true,
                "--no-global" if !parsed.no_global => parsed.no_global = true,
                "--repo" if parsed.repo.is_none() => {
                    let value = args.next().copied().flatten()?;
                    parsed.repo = Some(value.to_owned());
                }
                other => {
                    let value = other.strip_prefix("--repo=")?;

                    if parsed.repo.is_some() {
                        return None;
                    }

                    parsed.repo = Some(value.to_owned());
                }
            }
        }

        if parsed.repo.as_deref().is_some_and(|repo| repo.trim().is_empty()) {
            return None;
        }

        Some(parsed)
    }
}

/// The repository key `--repo` names: `.` is the repository of the working directory (found the
/// way the MCP server finds it), a `git:…` or `path:…` key is taken as written, and anything else
/// is read as a remote (`github.com/acme/widget`, `https://github.com/acme/widget.git`). The
/// reason is printed when there is none.
fn repo_key(env: &Env, raw: &str) -> Option<String> {
    let raw = raw.trim();

    if raw == "." {
        let mut projects = ProjectResolver::new(env.clone(), std::env::current_dir().ok());

        return match projects.resolve(None) {
            Ok(Some(project)) => Some(project.key),
            Ok(None) | Err(_) => {
                eprintln!(
                    "harness-memory: this folder is not inside a repository; give its key to --repo instead (as \
                     push --dry-run lists them)"
                );
                None
            }
        };
    }

    if raw.starts_with("git:") || raw.starts_with("path:") {
        return Some(raw.to_owned());
    }

    match normalize_remote(raw) {
        Some(remote) => Some(format!("git:{remote}")),
        None => {
            eprintln!(
                "harness-memory: --repo takes a repository key (git:host/owner/repo or path:/folder), a remote \
                 URL or ."
            );
            None
        }
    }
}

/// `harness-memory push`: the local database into the configured team database.
fn run_push(args: &PushArgs) -> ExitCode {
    let Ok(env) = Env::from_process() else {
        eprintln!("harness-memory: the home folder is unknown");
        return ExitCode::FAILURE;
    };

    let Some(Selection { backend, routing, .. }) = select(&env) else {
        return ExitCode::FAILURE;
    };

    if backend == Backend::Local {
        let config = backend::config_path(&env)
            .ok()
            .flatten()
            .map(|(path, _)| path.display().to_string())
            .unwrap_or_else(|| format!("the file {CONFIG_VAR} names"));
        eprintln!(
            "harness-memory: push copies the local memory into a team database, and none is set: \
             set {URL_VAR} or database_url in {config}"
        );
        return ExitCode::from(2);
    }

    if let Some(reason) = nothing_to_push(routing, args) {
        eprintln!("harness-memory: nothing to push: {reason}");
        return ExitCode::from(2);
    }

    let Some(local) = database_path(&env) else {
        eprintln!("harness-memory: the data folder is unknown");
        return ExitCode::FAILURE;
    };

    let repo = match &args.repo {
        Some(raw) => match repo_key(&env, raw) {
            Some(key) => Some(key),
            None => return ExitCode::from(2),
        },
        None => None,
    };

    let mut opened = match backend::open(&backend) {
        Some(Ok(opened)) => opened,
        Some(Err(message)) => {
            eprintln!("harness-memory: {message}");
            return ExitCode::FAILURE;
        }
        None => return ExitCode::FAILURE,
    };

    if opened.newer {
        eprintln!(
            "harness-memory: the team database ({}) was set up by a newer harness-memory; update this one \
             (run the install command again), then push. Nothing was pushed",
            backend.describe()
        );
        return ExitCode::FAILURE;
    }

    let options = Options {
        dry_run: args.dry_run,
        repo,
        skip_global: args.no_global || routing.global == Place::Local,
        skip_repositories: routing.repository == Place::Local,
    };

    let secrets = backend.secrets();
    let summary = match push(&local, opened.store.as_mut(), &options) {
        Ok(summary) => summary,
        Err(error) => {
            eprintln!("harness-memory: push failed: {}", secrets.scrub(&error.to_string()));
            return ExitCode::FAILURE;
        }
    };

    report_push(&options, &summary, &backend.describe(), &local);

    if routing.is_split() {
        let (kept, key) = if routing.repository == Place::Local {
            ("repository notes", "repository_notes")
        } else {
            ("global notes", "global_notes")
        };
        println!("{kept} stay on this computer ({key} = \"local\") and were left out");
    }

    for failure in &summary.failed {
        eprintln!("harness-memory: not pushed: {}", secrets.scrub(failure));
    }

    if summary.failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Why `push` with `args` has nothing to send under `routing`, if so.
fn nothing_to_push(routing: Routing, args: &PushArgs) -> Option<&'static str> {
    if routing.is_all_local() {
        return Some("repository_notes and global_notes are both \"local\", so nothing goes to the team database");
    }

    if routing.repository == Place::Local && args.repo.is_some() {
        return Some(
            "--repo names a repository, but repository notes stay on this computer (repository_notes = \"local\")",
        );
    }

    if routing.repository == Place::Local && args.no_global {
        return Some(
            "repository notes stay on this computer (repository_notes = \"local\") and --no-global leaves the global ones out",
        );
    }

    None
}

/// Prints what a push did: the totals, the notes whose team text was overwritten, and for a dry
/// run every note that would be shared, by repository.
fn report_push(options: &Options, summary: &Summary, place: &str, local: &std::path::Path) {
    let notes = format!(
        "{} added, {} updated, {} overwritten, {} skipped",
        summary.added, summary.updated, summary.overwritten, summary.skipped
    );
    let relations = format!("{} added, {} unchanged", summary.links_added, summary.links_unchanged);

    if options.dry_run {
        println!("dry run, nothing written to {place}: notes {notes}; relations {relations}");
    } else {
        println!(
            "pushed {} to {place}: notes {notes}; relations {relations}",
            local.display()
        );
    }

    if let Some(repo) = &options.repo
        && !summary.projects.contains(repo)
    {
        println!("the local memory has no notes or relations for {repo}");
    }

    let private = summary
        .projects
        .iter()
        .filter(|project| project.starts_with("path:"))
        .count();

    if private > 0 {
        println!(
            "note: this includes {private} repositories without a remote (path: keys), which may be private; \
             --repo <key> pushes only one repository"
        );
    }

    if options.dry_run {
        list_changes(summary);
    } else {
        for change in summary
            .changes
            .iter()
            .filter(|change| change.action == Action::Overwritten)
        {
            println!("\"{}\" ({}): overwrote the team's version", change.title, change.scope);
        }
    }

    if summary.entities_cut > 0 {
        println!(
            "{} added notes had more linked entities than one note may have; only the first were linked",
            summary.entities_cut
        );
    }
}

/// The notes a dry run would add or change, under their repository key (global last).
fn list_changes(summary: &Summary) {
    let mut scopes: Vec<&str> = Vec::new();

    for change in &summary.changes {
        if !scopes.contains(&change.scope.as_str()) {
            scopes.push(&change.scope);
        }
    }

    scopes.sort_by_key(|scope| (*scope == GLOBAL, *scope));

    for scope in scopes {
        println!("{scope}:");

        for change in summary.changes.iter().filter(|change| change.scope == scope) {
            let action = match change.action {
                Action::Added => "add",
                Action::Updated => "update",
                Action::Overwritten => "overwrite the team's version of",
            };

            println!("  {action} \"{}\"", change.title);
        }
    }
}
