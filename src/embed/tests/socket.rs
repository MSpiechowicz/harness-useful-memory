//! The socket, the single-instance lock and the client, over a real local socket.

use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use interprocess::local_socket::ListenerOptions;
use interprocess::local_socket::traits::ListenerExt as _;
use tempfile::TempDir;

use super::support::{Notes, World, fake_manifest, ids, install, target};
use crate::embed::client::{ClientError, EmbedderClient};
use crate::embed::protocol::NoteInput;
#[cfg(unix)]
use crate::embed::server;
use crate::embed::server::{Endpoint, Lock, LockError, Server};

const MODEL: &str = "fake@1";

/// A world with a model, a note, and a server on its socket.
struct Running {
    world: World,
    server: Option<Server>,
    client: EmbedderClient,
    key: String,
    note: i64,
}

fn running() -> Running {
    let dir = TempDir::new().expect("temp dir");
    let stores = dir.path().join("stores");
    std::fs::create_dir_all(&stores).expect("stores");
    let mut notes = Notes::new(&stores, "main");
    let note = notes.add("Socket", "answered over a local socket");

    let manifest = fake_manifest(MODEL);
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).expect("data");
    install(&root, &manifest);

    let world = World::open(dir, manifest, None, vec![notes.source()]);
    world.service.prepare().expect("prepare");
    world.service.backfill_pass(true);

    let endpoint = Endpoint::of(&world.root, &std::env::temp_dir());
    let server = Server::start(endpoint.clone(), world.service.clone()).expect("start the server");

    Running {
        world,
        server: Some(server),
        client: EmbedderClient::from_endpoint(endpoint),
        key: notes.key,
        note,
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.shutdown();
        }
    }
}

#[test]
fn ping_and_search_are_answered_over_the_socket() {
    let running = running();

    let pong = running.client.ping().expect("ping");
    assert_eq!(pong.model.as_deref(), Some(MODEL));
    assert!(pong.ready);
    assert_eq!(pong.pending, None);

    let (model, hits) = running
        .client
        .search_with_model("answered over a local socket", &[target(&running.key, &[None])], 5, 0.0)
        .expect("search");

    assert_eq!(model, MODEL);
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), vec![running.note]);
}

#[test]
fn upsert_and_forget_over_the_socket_reach_the_cache() {
    let running = running();
    let everything = [target(&running.key, &[None])];

    running
        .client
        .upsert(
            &running.key,
            &[NoteInput {
                id: 77,
                project: None,
                title: "Pushed".to_owned(),
                body: "kubernetes rollout restarts".to_owned(),
                tags: Vec::new(),
            }],
        )
        .expect("upsert");

    let found = running.world.ask("kubernetes rollout restarts", &everything);
    assert_eq!(ids(&found).first(), Some(&77));

    running.client.forget(&running.key, &[77]).expect("forget");
    let found = running.world.ask("kubernetes rollout restarts", &everything);
    assert!(!ids(&found).contains(&77));
}

#[test]
fn many_clients_at_once_are_all_answered() {
    let running = running();
    let key = running.key.clone();

    let threads: Vec<_> = (0..16)
        .map(|_| {
            let client = running.client.clone();
            let key = key.clone();

            std::thread::spawn(move || client.search("answered over a local socket", &[target(&key, &[None])], 5, 0.0))
        })
        .collect();

    for thread in threads {
        let hits = thread.join().expect("thread").expect("search");
        assert_eq!(hits.len(), 1);
    }
}

#[test]
fn a_request_over_the_cap_or_in_another_version_is_refused() {
    let running = running();

    let exchange = |line: &[u8]| -> String {
        let mut stream = running.client_stream();
        stream.write_all(line).expect("write");
        stream.write_all(b"\n").expect("newline");
        let mut answer = String::new();
        BufReader::new(stream).read_line(&mut answer).expect("read");
        answer
    };

    let big = format!(
        "{{\"v\":1,\"op\":\"forget\",\"store\":\"{}\",\"ids\":[]}}",
        "x".repeat(70 * 1024)
    );
    assert!(exchange(big.as_bytes()).contains("too large"));
    assert!(exchange(b"{\"v\":2,\"op\":\"ping\"}").contains("unsupported protocol version"));
    assert!(exchange(b"not json").contains("\"ok\":false"));
    assert!(exchange(b"{\"v\":1,\"op\":\"ping\"}").contains("\"ok\":true"));
}

impl Running {
    fn client_stream(&self) -> interprocess::local_socket::Stream {
        self.server
            .as_ref()
            .expect("server")
            .endpoint()
            .connect(Duration::from_secs(5))
            .expect("connect")
    }
}

#[test]
fn a_client_without_an_embedder_fails_at_once() {
    let dir = TempDir::new().expect("temp dir");
    let client = EmbedderClient::new(dir.path(), dir.path());

    let started = std::time::Instant::now();
    let error = client.search("anything", &[], 5, 0.0).expect_err("nobody listens");

    assert!(matches!(error, ClientError::Unreachable(_)), "{error}");
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(client.upsert("local:x", &[]).is_ok(), "nothing to send is fine");
    assert!(client.forget("local:x", &[1]).is_err());
}

#[test]
fn a_model_that_is_not_ready_answers_an_error_and_ping_says_so() {
    let dir = TempDir::new().expect("temp dir");
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).expect("data");
    let world = World::open(dir, fake_manifest(MODEL), None, Vec::new());

    let endpoint = Endpoint::of(&root, &std::env::temp_dir());
    let server = Server::start(endpoint.clone(), world.service.clone()).expect("start");
    let client = EmbedderClient::from_endpoint(endpoint);

    let pong = client.ping().expect("ping");
    assert!(!pong.ready);
    assert_eq!(pong.model, None);
    assert!(matches!(client.search("x", &[], 5, 0.0), Err(ClientError::Refused(_))));

    server.shutdown();
}

#[test]
fn a_second_embedder_in_the_same_folder_finds_the_lock_taken() {
    let dir = TempDir::new().expect("temp dir");

    assert!(!Lock::is_held(dir.path()));
    let first = Lock::take(dir.path()).expect("first lock");
    assert!(Lock::is_held(dir.path()));
    assert!(matches!(Lock::take(dir.path()), Err(LockError::Busy)));

    drop(first);
    assert!(!Lock::is_held(dir.path()));
    assert!(Lock::take(dir.path()).is_ok(), "released with the first");
}

#[cfg(unix)]
#[test]
fn a_socket_left_by_a_killed_embedder_is_replaced_and_a_file_is_not() {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    let dir = TempDir::new().expect("temp dir");
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).expect("data");
    let world = World::open(dir, fake_manifest(MODEL), None, Vec::new());
    let endpoint = Endpoint::of(&root, &std::env::temp_dir());

    // A dead process's socket: bound, then left behind.
    drop(std::os::unix::net::UnixListener::bind(endpoint.path()).expect("bind the stale socket"));
    assert!(
        std::fs::symlink_metadata(endpoint.path())
            .expect("stale")
            .file_type()
            .is_socket()
    );

    let server = Server::start(endpoint.clone(), world.service.clone()).expect("replaces the stale socket");
    let mode = std::fs::metadata(endpoint.path()).expect("socket").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(EmbedderClient::from_endpoint(endpoint.clone()).ping().is_ok());
    server.shutdown();
    assert!(!endpoint.path().exists(), "the socket goes with the server");

    // Anything else at that path is not ours to delete.
    std::fs::write(endpoint.path(), b"precious").expect("a regular file");
    assert!(Server::start(endpoint.clone(), world.service.clone()).is_err());
    assert_eq!(std::fs::read(endpoint.path()).expect("still there"), b"precious");
}

#[cfg(unix)]
#[test]
fn a_long_data_folder_gets_its_socket_in_a_folder_of_this_user() {
    let temp = TempDir::new().expect("temp dir");
    let runtime = TempDir::new().expect("runtime dir");
    let short = std::path::Path::new("/data/harness-memory");
    let long = std::path::PathBuf::from(format!("/home/someone/{}/harness-memory", "deep/".repeat(30)));

    let near = Endpoint::placed(short, temp.path(), Some(runtime.path()));
    assert_eq!(near.path(), short.join("embedder.sock"));

    let run = Endpoint::placed(&long, temp.path(), Some(runtime.path()));
    assert_eq!(run.path().parent(), Some(runtime.path()));
    assert!(run.path().as_os_str().len() <= 100);

    let far = Endpoint::placed(&long, temp.path(), None);
    let uid = nix::unistd::getuid().as_raw();
    assert_eq!(
        far.path().parent(),
        Some(temp.path().join(format!("harness-memory-{uid}")).as_path())
    );
    assert_ne!(
        far.path(),
        Endpoint::placed(&long.join("other"), temp.path(), None).path(),
        "one per data folder"
    );

    // A runtime folder that does not fit the socket path is passed over.
    let deep = temp.path().join("d".repeat(90));
    std::fs::create_dir(&deep).expect("deep runtime dir");
    assert_eq!(
        Endpoint::placed(&long, temp.path(), Some(&deep)).path().parent(),
        far.path().parent()
    );
}

#[cfg(unix)]
#[test]
fn a_link_in_place_of_the_socket_folder_is_refused_and_its_target_left_alone() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("temp dir");
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).expect("data");
    let world = World::open(dir, fake_manifest(MODEL), None, Vec::new());

    let temp = TempDir::new().expect("temp");
    let long = std::path::PathBuf::from(format!("/home/someone/{}/harness-memory", "deep/".repeat(30)));
    let endpoint = Endpoint::placed(&long, temp.path(), None);
    let folder = endpoint.path().parent().expect("folder").to_path_buf();

    let target = temp.path().join("target");
    std::fs::create_dir(&target).expect("target");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).expect("mode");
    std::os::unix::fs::symlink(&target, &folder).expect("link");

    assert!(Server::start(endpoint, world.service.clone()).is_err());
    assert_eq!(
        std::fs::metadata(&target).expect("target").permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(std::fs::read_dir(&target).expect("listing").count(), 0);
}

#[cfg(unix)]
#[test]
fn a_runtime_folder_is_used_only_when_it_is_private_and_this_users() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("temp dir");
    let folder = dir.path().join("run");
    std::fs::create_dir(&folder).expect("folder");

    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).expect("private");
    assert_eq!(server::usable_runtime(folder.clone()), Some(folder.clone()));

    for mode in [0o750, 0o705, 0o755] {
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(mode)).expect("mode");
        assert_eq!(server::usable_runtime(folder.clone()), None, "{mode:o}");
    }

    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).expect("private");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&folder, &link).expect("link");

    assert_eq!(server::usable_runtime(link), None, "not through a link");
    assert_eq!(server::usable_runtime("run".into()), None, "not a relative path");
    assert_eq!(server::usable_runtime(dir.path().join("missing")), None);
}

#[cfg(unix)]
#[test]
fn the_folder_of_a_long_data_folders_socket_is_made_private_and_a_strangers_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("temp dir");
    let temp = TempDir::new().expect("temp folder");
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).expect("data");
    let long = root.join("deep/".repeat(30));
    let world = World::open(dir, fake_manifest(MODEL), None, Vec::new());

    let endpoint = Endpoint::placed(&long, temp.path(), None);
    let folder = endpoint.path().parent().expect("folder").to_path_buf();

    // Taken by a folder other users may write to (a stranger's would fail the same check).
    std::fs::create_dir(&folder).expect("squatted folder");
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o777)).expect("open it up");

    let refused = Server::start(endpoint.clone(), world.service.clone())
        .err()
        .expect("refused");
    assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(refused.to_string().contains("write"), "{refused}");
    assert!(!endpoint.path().exists());
    assert_eq!(
        endpoint
            .connect(Duration::from_millis(200))
            .err()
            .map(|error| error.kind()),
        Some(std::io::ErrorKind::PermissionDenied)
    );

    // A link in its place is refused too.
    std::fs::remove_dir(&folder).expect("remove");
    std::os::unix::fs::symlink(temp.path(), &folder).expect("link");
    assert!(Server::start(endpoint.clone(), world.service.clone()).is_err());
    std::fs::remove_file(&folder).expect("remove the link");

    // Free, it is made `0700` and serves.
    let server = Server::start(endpoint.clone(), world.service.clone()).expect("start");
    let mode = std::fs::metadata(&folder).expect("folder").permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    assert!(EmbedderClient::from_endpoint(endpoint.clone()).ping().is_ok());
    server.shutdown();
}

#[test]
fn a_silent_connection_does_not_stop_others_from_being_served() {
    let running = running();
    let _silent = running.client_stream();

    assert!(running.client.ping().is_ok());
}

/// The calls to an embedder that accepts and never answers (a stuck one) give up at their own
/// timeouts on every platform: the streams' timeouts are not relied on (named pipes have none).
#[test]
fn a_stuck_embedder_costs_each_call_no_more_than_its_timeout() {
    const CALLS: usize = 4;

    let dir = TempDir::new().expect("temp dir");
    let endpoint = Endpoint::of(dir.path(), &std::env::temp_dir());
    endpoint.publish().expect("publish");
    let listener = ListenerOptions::new()
        .name(endpoint.name().expect("name"))
        .create_sync()
        .expect("listen");

    // Accepts the calls and keeps their connections open without a word.
    let stuck = std::thread::spawn(move || listener.incoming().take(CALLS).flatten().collect::<Vec<_>>());

    let client = EmbedderClient::from_endpoint(endpoint);
    let timed = |call: &dyn Fn() -> Result<(), ClientError>| {
        let started = std::time::Instant::now();
        let result = call();

        assert!(matches!(result, Err(ClientError::Unreachable(_))), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    };

    let note = NoteInput {
        id: 1,
        project: None,
        title: "Stuck".to_owned(),
        body: "nobody answers".to_owned(),
        tags: Vec::new(),
    };

    timed(&|| client.ping().map(drop));
    timed(&|| client.warm());
    timed(&|| client.upsert("local:x", std::slice::from_ref(&note)));
    timed(&|| client.forget("local:x", &[1]));

    drop(stuck.join().expect("the stuck embedder"));
}

#[test]
fn warming_loads_a_model_the_idle_unload_dropped() {
    let running = running();
    let service = &running.world.service;
    let loads = || running.world.fake.loads.load(std::sync::atomic::Ordering::SeqCst);

    assert_eq!(loads(), 1);
    assert!(
        crate::embed::locked(&service.active)
            .as_mut()
            .expect("active")
            .resident
            .unload_if_idle(Duration::ZERO)
    );

    running.client.warm().expect("warm");

    let started = std::time::Instant::now();
    while !crate::embed::locked(&service.active)
        .as_ref()
        .is_some_and(|active| active.resident.is_loaded())
    {
        assert!(started.elapsed() < Duration::from_secs(5), "the model was never loaded");
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(loads(), 2);

    // Warm again with the model loaded: nothing is loaded twice.
    running.client.warm().expect("warm again");
    assert_eq!(loads(), 2);
}

#[test]
fn a_status_probe_holding_the_lock_for_an_instant_does_not_make_an_embedder_exit_as_busy() {
    let dir = TempDir::new().expect("temp dir");
    drop(Lock::take(dir.path()).expect("creates the lock file"));

    // What `Lock::is_held` does, held for longer than a real probe.
    let file = std::fs::File::open(dir.path().join(crate::embed::LOCK_FILE)).expect("lock file");
    file.lock_shared().expect("shared lock");

    let releasing = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        drop(file);
    });

    assert!(Lock::take(dir.path()).is_ok(), "waited for the probe to let go");
    releasing.join().expect("probe");
}
