//! The `harness-memory` executable: `harness-memory [mcp]` serves MCP over stdio,
//! `harness-memory --version` prints the version.

use std::io::{BufReader, Write};
use std::process::ExitCode;

use harness_memory::location::{DIR_VAR, Migration, legacy_database_path, migrate_legacy, override_is_relative};
use harness_memory::project::ProjectResolver;
use harness_memory::store::SqliteStore;
use harness_memory::support::Env;
use harness_memory::{Server, database_path, serve};

const USAGE: &str = "usage: harness-memory [mcp | --version]";

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let args: Vec<Option<&str>> = args.iter().map(|arg| arg.to_str()).collect();

    match args.as_slice() {
        [] | [Some("mcp")] => run(),
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

    let Some(path) = database_path(&env) else {
        eprintln!("harness-memory: the data folder is unknown");
        return ExitCode::FAILURE;
    };

    if override_is_relative(&env) {
        eprintln!(
            "harness-memory: {DIR_VAR} must be an absolute folder; using {}",
            path.display()
        );
    }

    // A failed move is not fatal: the server starts on an empty database and the legacy one stays.
    if let Migration::Failed(reason) = migrate_legacy(&env, &path) {
        eprintln!(
            "harness-memory: could not copy the Harness Desktop memory database ({reason}); starting empty. \
             The old notes are still in {}; to retry, stop every harness-memory server and delete {}",
            legacy_database_path(&env)
                .map(|legacy| legacy.display().to_string())
                .unwrap_or_default(),
            path.display()
        );
    }

    let store = match SqliteStore::open(&path) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("harness-memory: {error}");
            return ExitCode::FAILURE;
        }
    };

    if store.is_newer() {
        eprintln!("harness-memory: memory database is newer than this server; tools will fail");
    }

    let projects = ProjectResolver::new(env, std::env::current_dir().ok());
    let mut server = Server::new(Box::new(store), projects);

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
