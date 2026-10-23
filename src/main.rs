//! The `harness-memory` executable: `harness-memory [mcp]` serves MCP over stdio,
//! `harness-memory push [--dry-run] [--json] [--overwrite none|all|ID[,ID…]|-] [--delete-copied]
//! [--scope repo|global] [--repo <key|.>] [--no-global]` copies the local memory into the team
//! database, `harness-memory check` opens the configured database and says which it is and where
//! scripts are, `harness-memory kinds` lists the team database kinds this build supports,
//! `harness-memory features` lists what this build can do for Harness Desktop,
//! `harness-memory scripts list|get|hide|unhide|delete|discard-local|push` and `harness-memory
//! trust` list, hide, delete, copy and approve scripts ([`harness_memory::cli_trust`]),
//! `harness-memory notes list|get|hide|unhide|delete|discard-local` browse, hide and delete notes
//! ([`harness_memory::cli_notes`]), `harness-memory notes save|update`, `scripts save` and
//! `held list|save|skip` save the user's notes and scripts and the held items past the save guards
//! ([`harness_memory::cli_held`]), `harness-memory hooks config|list|get|put|delete` and
//! `harness-memory routines config|list|get|put|delete` share Harness Desktop hooks and routines
//! with the team ([`harness_memory::cli_team_docs`]), `harness-memory settings get|set` reads and
//! changes the English notes setting ([`harness_memory::cli_settings`]), `harness-memory --version`
//! prints the version.

use std::collections::BTreeSet;
use std::io::{BufReader, Write};
use std::process::ExitCode;

use harness_memory::backend::{self, Backend, CONFIG_VAR, NotesPlace, Routing, Secrets, Selection, URL_VAR};
use harness_memory::cli_push::{PushArgs, overwrite, repo_key};
use harness_memory::location::{DIR_VAR, override_is_relative};
use harness_memory::mcp::stop_on_sigterm;
use harness_memory::open::{open_hidden, open_server};
use harness_memory::push::{Action, Change, GLOBAL, Options, Summary, push};
use harness_memory::store::Scope;
use harness_memory::support::Env;
use harness_memory::support::runner::Stop;
use harness_memory::support::time::rfc3339;
use harness_memory::{cli_embedder, cli_held, cli_notes, cli_settings, cli_team_docs, cli_trust};
use harness_memory::{database_path, serve};
use serde_json::{Value, json};

const USAGE: &str = "usage: harness-memory [mcp | push [--dry-run] [--json] [--overwrite none|all|ID[,ID...]|-] \
                     [--delete-copied] [--scope repo|global] [--repo <key|.>] [--no-global] | check | kinds | features \
                     | scripts list --json [--project PATH | --repo KEY | --all-repos] [--group needsReview|repo|global] [--scope repo|global] \
                     [--place local|team] [--review needed|trusted] [--query TEXT] [--sort name|language|scope|place|runs|lastRun|updated] \
                     [--order asc|desc] [--offset N] [--limit N] [--hidden] | scripts get <id> --json [--project PATH | --repo KEY | --all-repos] \
                     [--hidden] | scripts hide <id> [--project PATH | --repo KEY | --all-repos] [--json] | scripts unhide <id> [--project PATH | --repo KEY | --all-repos] \
                     [--json] | scripts delete <id> [--project PATH | --repo KEY | --all-repos] | scripts delete --all --yes [--project PATH | --repo KEY] \
                     [--json] | scripts discard-local (--all | ID[,ID...] | -) [--scope repo|global] --yes [--json] | \
                     scripts push [--dry-run] [--json] [--overwrite none|all|ID[,ID...]|-] [--delete-copied] [--scope \
                     repo|global] [--repo <key|.>] [--no-global] | trust <id>[=<sha256>]... [--repo KEY | --all-repos] | --all [--project PATH | --repo KEY] \
                     | notes list --json [--project PATH | --repo KEY | --all-repos] [--kind KIND] [--scope repo|global] \
                     [--place local|team] [--lang en|other|unknown] [--query TEXT] [--sort updated|title|kind|scope|place] [--order asc|desc] [--offset N] [--limit N] [--hidden] | notes \
                     get <id> --json [--project PATH | --repo KEY | --all-repos] [--hidden] | notes hide <id> [--project PATH | --repo KEY | --all-repos] [--json] | notes \
                     unhide <id> [--project PATH | --repo KEY | --all-repos] [--json] | notes delete <id> [--project PATH | --repo KEY | --all-repos] | notes delete --all \
                     --yes [--project PATH | --repo KEY] [--json] | notes repos --json | scripts repos --json | notes discard-local (--all | ID[,ID...] | -) [--scope \
                     repo|global] --yes [--json] | hooks config --json | hooks list --json [--project PATH] [--offset \
                     N] [--limit N] | hooks get <id> --json | hooks put --json [--project PATH] [--expect-sha SHA256] \
                     [--author NAME] < hook.json | hooks delete <id> [--expect-sha SHA256] --json | routines config \
                     --json | routines list --json [--project PATH] [--offset N] [--limit N] | routines get <id> \
                     --json | routines put --json [--project PATH] [--expect-sha SHA256] [--author NAME] < \
                     routine.json | routines delete <id> [--expect-sha SHA256] --json | routines delete --mine \
                     [--author NAME] --json | notes save --json [--project PATH] [--client NAME] < note.json | notes update <id> --json \
                     [--project PATH] [--client NAME] < patch.json | scripts save --json [--project PATH] [--client NAME] \
                     < script.json | held list --json \
                     [--chat KEY] | held save <heldId> --json [< draft.json] | held skip <heldId> --json | settings get --json | \
                     settings set english_notes=true|false --json | --version]";

/// What this build can do that older ones could not, as `harness-memory features` lists it: the
/// `remote+local` note routing, hiding team notes on this computer, `push --json`, and the same for
/// scripts (`scripts = "remote+local"`, `scripts hide`, `scripts push --json`), and reading every
/// repository at once (`--all-repos`, `--repo KEY`, `notes repos`, `scripts repos`), and the save
/// guards with their held items and user-only saves (`notes save`, `notes update`, `scripts save`,
/// `held list|save|skip`), the `settings` command, the notes language (`notes list --lang`, the
/// `lang` of a note, the English notes setting), and the shared embedder (`embedder`).
const FEATURES: [&str; 11] = [
    "notes-remote-local",
    "notes-hide",
    "push-json",
    "scripts-remote-local",
    "scripts-hide",
    "scripts-push-json",
    "all-repos",
    "save-guards",
    "settings",
    "notes-lang",
    "embedder",
];

/// mimalloc hands freed memory back to the operating system, which the embedder's idle unload of
/// the model relies on (glibc's allocator keeps most of it).
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
        [Some("features")] => {
            // One feature per line, for Harness Desktop to tell what it can offer.
            for feature in FEATURES {
                println!("{feature}");
            }

            ExitCode::SUCCESS
        }
        [Some("kinds")] => {
            // One kind per line, for Harness Desktop to tell which kinds it can offer.
            for kind in backend::KINDS {
                println!("{kind}");
            }

            ExitCode::SUCCESS
        }
        [Some("scripts"), rest @ ..] => cli_trust::scripts(rest),
        [Some("trust"), rest @ ..] => cli_trust::trust(rest),
        [Some("notes"), rest @ ..] => cli_notes::notes(rest),
        [Some("held"), rest @ ..] => cli_held::held(rest),
        [Some("settings"), rest @ ..] => cli_settings::settings(rest),
        [Some("embedder"), rest @ ..] => cli_embedder::embedder(rest),
        [Some("hooks"), rest @ ..] => cli_team_docs::hooks(rest),
        [Some("routines"), rest @ ..] => cli_team_docs::routines(rest),
        [Some("--version")] => {
            println!("harness-memory {}", env!("HARNESS_MEMORY_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn run() -> ExitCode {
    // Before anything starts a thread: SIGTERM then stops a running script before the server ends.
    let stop = Stop::default();
    stop_on_sigterm(&stop);

    let Ok(env) = Env::from_process() else {
        eprintln!("harness-memory: the home folder is unknown");
        return ExitCode::FAILURE;
    };

    let Some(selection) = select(&env) else {
        return ExitCode::FAILURE;
    };

    let Some(server) = open_server(&env, &selection, std::env::current_dir().ok()) else {
        return ExitCode::FAILURE;
    };
    let mut server = server.with_stop(stop);

    let stdin = BufReader::new(std::io::stdin());
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

/// `harness-memory check`: opens the configured database like the server does and prints a line
/// saying which it is, then `scripts: team database`, `scripts: this computer` or `scripts: team
/// database and this computer` (exit 0), or the
/// reason it cannot be opened on stderr (exit 1). The local database is only looked at, never
/// created or migrated. With every note scope kept on this computer, the team database is opened
/// for the scripts when they go there.
fn run_check() -> ExitCode {
    let Ok(env) = Env::from_process() else {
        eprintln!("harness-memory: the home folder is unknown");
        return ExitCode::FAILURE;
    };

    let Some(selection) = select(&env) else {
        return ExitCode::FAILURE;
    };

    let selection_scripts = selection.scripts_place();
    let team = cli_trust::check_line(&selection)
        .map(|line| format!("\n{line}"))
        .unwrap_or_default();
    let scripts = format!("scripts: {}", selection_scripts.describe());
    let Selection { backend, routing, .. } = selection;

    if backend == Backend::Local || routing.is_all_local() {
        let Some(local) = local_place(&env) else {
            return ExitCode::FAILURE;
        };

        if !selection_scripts.reads_team() {
            println!("ok: {local}");
            println!("{scripts}{team}");
            return ExitCode::SUCCESS;
        }

        let Some(newer) = check_team(&backend) else {
            return ExitCode::FAILURE;
        };

        println!("ok: {local}");
        println!("{scripts}{}{team}", newer_note(newer));
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

    if routing.needs_both_stores() {
        let Some(local) = local_place(&env) else {
            return ExitCode::FAILURE;
        };

        line.push_str(&format!("; {}; {local}", routing_line(routing)));
    }

    if opened.newer {
        line.push_str("; the team database is newer than this harness-memory; update it");
    }

    println!("{line}");
    println!("{scripts}{team}");
    ExitCode::SUCCESS
}

/// Opens the team database `backend` for the scripts: whether it was written by a newer server,
/// or `None` with the reason printed.
fn check_team(backend: &Backend) -> Option<bool> {
    match backend::open(backend)? {
        Ok(opened) => Some(opened.newer),
        Err(message) => {
            eprintln!("harness-memory: {message}");
            None
        }
    }
}

/// What the scripts line adds when the team database is newer than this server.
fn newer_note(newer: bool) -> &'static str {
    if newer {
        "; the team database is newer than this harness-memory; update it"
    } else {
        ""
    }
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

/// What `push` does with one note scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Taken {
    Pushed,
    /// Left out by `--no-global`, or by `--scope` naming the other scope.
    NotAsked,
    /// Kept on this computer (`local`), so never pushed.
    Local,
    /// `remote+local`, which only `--scope` naming it pushes.
    Unnamed,
}

/// How `push` takes one note scope routed to `place`: `asked` when the options leave it in,
/// `named` when `--scope` names it.
fn taken(place: NotesPlace, asked: bool, named: bool) -> Taken {
    if !asked {
        return Taken::NotAsked;
    }

    match place {
        NotesPlace::Remote => Taken::Pushed,
        NotesPlace::Local => Taken::Local,
        NotesPlace::RemoteLocal if named => Taken::Pushed,
        NotesPlace::RemoteLocal => Taken::Unnamed,
    }
}

/// A note scope as `push` names it in its messages.
struct ScopeWords {
    /// `repository notes`.
    words: &'static str,
    /// The setting: `repository_notes`.
    key: &'static str,
    /// The `--scope` value: `repo`.
    flag: &'static str,
}

const REPOSITORY_WORDS: ScopeWords = ScopeWords {
    words: "repository notes",
    key: "repository_notes",
    flag: "repo",
};

const GLOBAL_WORDS: ScopeWords = ScopeWords {
    words: "global notes",
    key: "global_notes",
    flag: "global",
};

/// How `push` takes the two note scopes under a routing and its options.
#[derive(Debug, Clone, Copy)]
struct PushScopes {
    repository: Taken,
    global: Taken,
}

impl PushScopes {
    fn of(routing: Routing, args: &PushArgs) -> Self {
        let repository = taken(
            routing.repository,
            args.scope != Some(Scope::Global),
            args.scope == Some(Scope::Repo),
        );
        let global = taken(
            routing.global,
            !args.no_global && args.scope != Some(Scope::Repo),
            args.scope == Some(Scope::Global),
        );

        Self { repository, global }
    }

    /// Both scopes with their words.
    fn each(self) -> [(Taken, ScopeWords); 2] {
        [(self.repository, REPOSITORY_WORDS), (self.global, GLOBAL_WORDS)]
    }
}

/// `harness-memory push`: the local database into the configured team database.
fn run_push(args: &PushArgs) -> ExitCode {
    if let Some(problem) = args.conflict("notes") {
        eprintln!("harness-memory: {problem}");
        return ExitCode::from(2);
    }

    let Ok(env) = Env::from_process() else {
        eprintln!("harness-memory: the home folder is unknown");
        return ExitCode::FAILURE;
    };

    let Some(selection) = select(&env) else {
        return ExitCode::FAILURE;
    };
    let (backend, routing) = (selection.backend.clone(), selection.routing);

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

    let scopes = PushScopes::of(routing, args);

    if let Some(reason) = nothing_to_push(routing, args, scopes) {
        return report_nothing(args, &reason);
    }

    let Some(local) = database_path(&env) else {
        eprintln!("harness-memory: the data folder is unknown");
        return ExitCode::FAILURE;
    };

    if args.json && !local.is_file() {
        return report_nothing(args, "no notes are saved on this computer");
    }

    let Some(overwrite) = overwrite(args) else {
        return ExitCode::from(2);
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
        skip_global: scopes.global != Taken::Pushed,
        skip_repositories: scopes.repository != Taken::Pushed,
        overwrite,
        delete_copied: args.delete_copied,
    };

    let secrets = backend.secrets();

    // A team note hidden here is never written over, so a list that cannot be read stops it all.
    let hidden = match open_hidden(&env, &selection).map(|mut hidden| hidden.try_ids()) {
        Some(Ok(ids)) => ids,
        Some(Err(error)) => {
            eprintln!(
                "harness-memory: nothing was pushed: {}",
                secrets.scrub(&error.to_string())
            );
            return ExitCode::FAILURE;
        }
        None => Vec::new(),
    };

    let summary = match push(&local, opened.store.as_mut(), &hidden, &options) {
        Ok(summary) => summary,
        Err(error) => {
            eprintln!("harness-memory: push failed: {}", secrets.scrub(&error.to_string()));
            return ExitCode::FAILURE;
        }
    };

    if args.json {
        println!("{}", push_json(args.dry_run, None, &summary, &secrets));
    } else {
        report_push(&options, &summary, &backend.describe(), &local);
        report_left_out(scopes);
        report_failures(&summary, &secrets);
    }

    if summary.failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Says there is nothing to push and why: in the JSON object (exit 0) with `--json`, else on
/// stderr (exit 2).
fn report_nothing(args: &PushArgs, reason: &str) -> ExitCode {
    if args.json {
        let empty = Summary::default();
        println!("{}", push_json(args.dry_run, Some(reason), &empty, &Secrets::default()));
        return ExitCode::SUCCESS;
    }

    eprintln!("harness-memory: nothing to push: {reason}");
    ExitCode::from(2)
}

/// Why `push` with `args` has nothing to send under `routing`, taking the scopes as `scopes`
/// says, if so.
fn nothing_to_push(routing: Routing, args: &PushArgs, scopes: PushScopes) -> Option<String> {
    if routing.is_all_local() {
        return Some(
            "repository_notes and global_notes are both \"local\", so nothing goes to the team database".to_owned(),
        );
    }

    let named = match args.scope {
        Some(Scope::Repo) => Some((scopes.repository, REPOSITORY_WORDS)),
        Some(Scope::Global) => Some((scopes.global, GLOBAL_WORDS)),
        None => None,
    };

    if let Some((Taken::Local, scope)) = named {
        return Some(format!(
            "--scope {} names the {}, but they stay on this computer ({} = \"local\")",
            scope.flag, scope.words, scope.key
        ));
    }

    if args.repo.is_some() {
        match scopes.repository {
            Taken::Local => {
                return Some(
                    "--repo names a repository, but repository notes stay on this computer (repository_notes = \
                     \"local\")"
                        .to_owned(),
                );
            }
            Taken::Unnamed => {
                return Some(
                    "--repo names a repository, but repository notes are \"remote+local\" (repository_notes), so \
                     only --scope repo pushes them"
                        .to_owned(),
                );
            }
            Taken::Pushed | Taken::NotAsked => {}
        }
    }

    if scopes.repository == Taken::Pushed || scopes.global == Taken::Pushed {
        return None;
    }

    let reasons: Vec<String> = scopes
        .each()
        .into_iter()
        .map(|(taken, scope)| left_out_reason(taken, &scope, args))
        .collect();

    Some(reasons.join(" and "))
}

/// Why a scope taken as `taken` is not pushed (it is not [`Taken::Pushed`]).
fn left_out_reason(taken: Taken, scope: &ScopeWords, args: &PushArgs) -> String {
    match taken {
        Taken::Local => format!("{} stay on this computer ({} = \"local\")", scope.words, scope.key),
        Taken::Unnamed => format!(
            "{} are \"remote+local\" ({}), so only --scope {} pushes them",
            scope.words, scope.key, scope.flag
        ),
        Taken::NotAsked if scope.flag == GLOBAL_WORDS.flag && args.no_global => {
            "--no-global leaves the global ones out".to_owned()
        }
        Taken::NotAsked => {
            let other = if scope.flag == GLOBAL_WORDS.flag {
                REPOSITORY_WORDS.flag
            } else {
                GLOBAL_WORDS.flag
            };
            format!("--scope {other} leaves the {} out", scope.words)
        }
        Taken::Pushed => format!("{} are pushed", scope.words),
    }
}

/// Prints which scopes the routing left out of a push that ran.
fn report_left_out(scopes: PushScopes) {
    for (taken, scope) in scopes.each() {
        match taken {
            Taken::Local => println!(
                "{} stay on this computer ({} = \"local\") and were left out",
                scope.words, scope.key
            ),
            Taken::Unnamed => println!(
                "{} are \"remote+local\" ({}) and were left out: push --scope {} copies the ones saved on this \
                 computer",
                scope.words, scope.key, scope.flag
            ),
            Taken::Pushed | Taken::NotAsked => {}
        }
    }
}

/// Prints what could not be pushed, or not deleted after it was copied.
fn report_failures(summary: &Summary, secrets: &Secrets) {
    for failure in &summary.failed {
        let message = secrets.scrub(&failure.message);

        if failure.deleting {
            eprintln!("harness-memory: {message}");
        } else {
            eprintln!("harness-memory: not pushed: {message}");
        }
    }
}

/// The `push --json` object: `nothing` names why there was nothing to push, and each change's
/// `changedSince` says `--delete-copied` left its note because it changed after the push read it.
fn push_json(dry_run: bool, nothing: Option<&str>, summary: &Summary, secrets: &Secrets) -> Value {
    let failed_notes: BTreeSet<i64> = summary
        .failed
        .iter()
        .filter(|failure| !failure.deleting)
        .filter_map(|failure| failure.local_id)
        .collect();

    let changes: Vec<Value> = summary
        .changes
        .iter()
        .map(|change| {
            let scope = if change.repo.is_some() { "repo" } else { "global" };

            json!({
                "localId": change.local_id,
                "scope": scope,
                "repo": change.repo.as_deref().map(|key| key.strip_prefix("git:").unwrap_or(key)),
                "title": change.title,
                "action": change.action.as_str(),
                "conflict": change.conflict,
                "teamId": change.team_id,
                "localUpdatedAt": rfc3339(change.local_updated_at),
                "teamUpdatedAt": change.team_updated_at.map(rfc3339),
                "deleted": change.deleted,
                "changedSince": change.changed_since,
                "failed": change.failed,
            })
        })
        .collect();

    let failed: Vec<Value> = summary
        .failed
        .iter()
        .map(|failure| json!({ "localId": failure.local_id, "message": secrets.scrub(&failure.message) }))
        .collect();

    json!({
        "dryRun": dry_run,
        "nothing": nothing,
        "notes": {
            "added": summary.added,
            "updated": summary.updated,
            "overwritten": summary.overwritten,
            "kept": summary.kept,
            "identical": summary.identical,
            "teamNewer": summary.team_newer,
            "failed": failed_notes.len(),
        },
        "relations": { "added": summary.links_added, "unchanged": summary.links_unchanged },
        "entitiesCut": summary.entities_cut,
        "deleted": summary.deleted,
        "changes": changes,
        "failed": failed,
    })
}

/// Prints what a push did: the totals, the notes whose team text was overwritten or kept, and
/// for a dry run every note that would be shared, by repository.
fn report_push(options: &Options, summary: &Summary, place: &str, local: &std::path::Path) {
    let kept = if summary.kept > 0 {
        format!(", {} kept", summary.kept)
    } else {
        String::new()
    };
    let notes = format!(
        "{} added, {} updated, {} overwritten{kept}, {} skipped",
        summary.added,
        summary.updated,
        summary.overwritten,
        summary.skipped()
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
        for change in summary.changes.iter().filter(|change| !change.failed) {
            match change.action {
                Action::Overwritten => {
                    println!("\"{}\" ({}): overwrote the team's version", change.title, change.scope)
                }
                Action::Kept => println!(
                    "\"{}\" ({}): kept the team's version, which differs from the newer one here",
                    change.title, change.scope
                ),
                _ => {}
            }
        }
    }

    if summary.entities_cut > 0 {
        println!(
            "{} added notes had more linked entities than one note may have; only the first were linked",
            summary.entities_cut
        );
    }

    if let Some(deleted) = summary.deleted {
        println!("deleted {deleted} copied notes from this computer");
    }
}

/// The notes a dry run would add, change or keep, under their repository key (global last).
fn list_changes(summary: &Summary) {
    let listed: Vec<(&Change, &str)> = summary
        .changes
        .iter()
        .filter_map(|change| {
            let action = match change.action {
                Action::Added => "add",
                Action::Updated => "update",
                Action::Overwritten => "overwrite the team's version of",
                Action::Kept => "keep the team's version of",
                Action::Identical | Action::TeamNewer => return None,
            };

            Some((change, action))
        })
        .collect();

    let mut scopes: Vec<&str> = Vec::new();

    for (change, _) in &listed {
        if !scopes.contains(&change.scope.as_str()) {
            scopes.push(&change.scope);
        }
    }

    scopes.sort_by_key(|scope| (*scope == GLOBAL, *scope));

    for scope in scopes {
        println!("{scope}:");

        for (change, action) in listed.iter().filter(|(change, _)| change.scope == scope) {
            println!("  {action} \"{}\"", change.title);
        }
    }
}
