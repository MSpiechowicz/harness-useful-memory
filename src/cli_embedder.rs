//! The `harness-memory embedder` command: `embedder [run]` serves the embedder (JSON lines on
//! standard output, see [`crate::embed::Event`]), `embedder enable|disable` switch it on or off for
//! this computer, `embedder status --json` says where it stands, and `embedder remove-model`
//! deletes the model files and vectors while it is not running.

use std::process::ExitCode;
use std::sync::Arc;

use serde_json::json;

use crate::backend::{self, Backend, Selection};
use crate::embed::server::{Endpoint, Lock, LockError, Server};
use crate::embed::{
    Cache, EmbedderClient, Event, Events, JsonLines, MODEL, Options, Service, Source, TractLoader, cache, enabled,
    manifest, model, set_enabled, store_key_local, store_key_team,
};
use crate::store::{Lazy, MemoryStore, SqliteStore};
use crate::support::Env;
use crate::support::private::data_folder;

/// Runs the command; `args` are the words after `embedder`.
pub fn embedder(args: &[Option<&str>]) -> ExitCode {
    match args {
        [] | [Some("run")] => run(),
        [Some("enable")] => switch(true),
        [Some("disable")] => switch(false),
        [Some("status"), Some("--json")] => status(),
        [Some("remove-model")] => remove_model(),
        _ => {
            eprintln!("usage: harness-memory embedder [run | enable | disable | status --json | remove-model]");
            ExitCode::from(2)
        }
    }
}

/// The data folder, or the reason printed.
fn root() -> Option<(Env, std::path::PathBuf)> {
    let Ok(env) = Env::from_process() else {
        eprintln!("harness-memory: the home folder is unknown");
        return None;
    };

    match data_folder(&env) {
        Some(root) => Some((env, root)),
        None => {
            eprintln!("harness-memory: the data folder is unknown");
            None
        }
    }
}

fn switch(on: bool) -> ExitCode {
    let Some((_, root)) = root() else {
        return ExitCode::FAILURE;
    };

    match set_enabled(&root, on) {
        Ok(()) => {
            println!("{}", json!({ "enabled": on }));
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("harness-memory: {message}");
            ExitCode::FAILURE
        }
    }
}

fn status() -> ExitCode {
    let Some((_, root)) = root() else {
        return ExitCode::FAILURE;
    };

    let running = Lock::is_held(&root);
    let (active, vectors) = cache::peek(&root.join(crate::embed::CACHE_FILE)).unwrap_or((None, 0));

    let pending = if running {
        EmbedderClient::new(&root, &std::env::temp_dir())
            .ping()
            .ok()
            .and_then(|pong| pong.pending)
    } else {
        active
            .as_deref()
            .filter(|active| *active != MODEL.id)
            .map(|_| MODEL.id.to_owned())
    };

    println!(
        "{}",
        json!({
            "enabled": enabled(&root),
            "running": running,
            "active_model": active,
            "pending_model": pending,
            "model_present": model::is_present(&model::model_dir(&root, MODEL.id), &MODEL),
            "vectors": vectors,
        })
    );

    ExitCode::SUCCESS
}

fn remove_model() -> ExitCode {
    let Some((_, root)) = root() else {
        return ExitCode::FAILURE;
    };

    if Lock::is_held(&root) {
        eprintln!("harness-memory: the embedder is running; stop it first");
        return ExitCode::FAILURE;
    }

    if let Err(error) = std::fs::remove_dir_all(model::models_folder(&root))
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("harness-memory: cannot delete the model files ({})", error.kind());
        return ExitCode::FAILURE;
    }

    let file = root.join(crate::embed::CACHE_FILE);

    if file.exists() {
        let cleared = Cache::open(&file).and_then(|mut cache| cache.clear());

        if let Err(message) = cleared {
            eprintln!("harness-memory: {message}");
            return ExitCode::FAILURE;
        }
    }

    println!("{}", json!({ "removed": true }));
    ExitCode::SUCCESS
}

fn run() -> ExitCode {
    let Some((env, root)) = root() else {
        return ExitCode::FAILURE;
    };

    let events: Arc<dyn Events> = Arc::new(JsonLines);

    let lock = match Lock::take(&root) {
        Ok(lock) => lock,
        Err(LockError::Busy) => {
            events.emit(Event::Busy);
            return ExitCode::SUCCESS;
        }
        Err(LockError::Failed(message)) => {
            events.emit(Event::Error { message });
            return ExitCode::FAILURE;
        }
    };

    let mut options = Options::new(root.clone(), MODEL);
    options.base_url = std::env::var(manifest::BASE_URL_VAR)
        .ok()
        .filter(|base| !base.trim().is_empty());

    // For tests and the smoke script only: a short idle time shows the unload.
    if let Some(seconds) = std::env::var("HARNESS_MEMORY_EMBEDDER_IDLE_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
    {
        options.idle = std::time::Duration::from_secs(seconds);
    }

    if let Some(base) = &options.base_url
        && let Err(error) = model::check_base(base)
    {
        events.emit(Event::Error {
            message: error.to_string(),
        });
        return ExitCode::from(2);
    }

    let sources = match backend::select(&env) {
        Ok(selection) => {
            for warning in &selection.warnings {
                eprintln!("harness-memory: {warning}");
            }

            sources_for(&env, &selection)
        }
        Err(error) => {
            events.emit(Event::Error {
                message: error.to_string(),
            });
            Vec::new()
        }
    };

    let service = match Service::new(options, Arc::new(TractLoader), Arc::clone(&events), sources) {
        Ok(service) => Arc::new(service),
        Err(message) => {
            events.emit(Event::Error { message });
            return ExitCode::FAILURE;
        }
    };

    let server = match Server::start(Endpoint::of(&root, &std::env::temp_dir()), Arc::clone(&service)) {
        Ok(server) => server,
        Err(error) => {
            events.emit(Event::Error {
                message: format!("cannot listen on the socket ({})", error.kind()),
            });
            return ExitCode::FAILURE;
        }
    };

    // The harness that started the embedder closes its end when it exits.
    let stopper = Arc::clone(&service);
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
        stopper.stop().stop();
    });

    // SIGINT, SIGTERM and SIGHUP (the console events on Windows) end the run like a closed stdin.
    let signalled = Arc::clone(&service);

    if let Err(error) = ctrlc::set_handler(move || signalled.stop().stop()) {
        eprintln!("harness-memory: cannot watch for termination signals ({error})");
    }

    let working = Arc::clone(&service);
    std::thread::spawn(move || working.lifecycle());

    service.stop().wait_forever();
    server.shutdown();
    drop(lock);
    ExitCode::SUCCESS
}

/// The physical stores this computer is configured with: the local SQLite file when any note
/// scope is kept on it, and the team database when any is read from it.
pub fn sources_for(env: &Env, selection: &Selection) -> Vec<Source> {
    let Selection { backend, routing, .. } = selection;
    let mut sources = Vec::new();

    let local = *backend == Backend::Local || routing.repository.reads_local() || routing.global.reads_local();

    if local && let Some(path) = crate::database_path(env) {
        let opened = path.clone();
        let store: Lazy<dyn MemoryStore> = Lazy::new(move || {
            SqliteStore::open(&opened)
                .map(|store| Box::new(store) as Box<dyn MemoryStore>)
                .map_err(|error| error.to_string())
        });

        sources.push(Source::new(store_key_local(&path), Box::new(store)).when_file_exists(path));
    }

    let team = *backend != Backend::Local && (routing.repository.reads_team() || routing.global.reads_team());

    if team && let Some(key) = store_key_team(backend) {
        let target = backend.clone();
        let store: Lazy<dyn MemoryStore> = Lazy::new(move || match backend::open(&target) {
            Some(Ok(opened)) => Ok(opened.store),
            Some(Err(message)) => Err(message),
            None => Err("no team database is set".to_owned()),
        });

        sources.push(Source::new(key, Box::new(store)));
    }

    sources
}
