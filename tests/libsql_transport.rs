//! The libSQL client's transport: proxy variables are ignored, what the server says reaches an
//! error only as a bare error code, a rejected `PRAGMA foreign_keys` stops the write, and a commit
//! whose reply is lost is reported as possibly applied and not repeated.
//!
//! The proxy and fake-server cases need no server; the lost-commit case runs when
//! `HARNESS_MEMORY_TEST_LIBSQL_URL` names a plain `http://` server (see `libsql_support`).

mod common;
mod libsql_support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use common::{draft, repo};
use harness_memory::store::sqlite::schema::SCHEMA_VERSION;
use harness_memory::store::{LibsqlStore, MemoryStore, NoteKind, StoreError};
use libsql_support::Database;
use serde_json::{Value, json};

const REPO: &str = "github.com/acme/widget";
const TOKEN: &str = "eyJsecret-token-value";

/// Set for the child process of [`proxy_variables_are_ignored`]: the URL it connects to.
const CHILD_URL_VAR: &str = "HARNESS_MEMORY_TEST_PROXY_CHILD_URL";

const PROXY_VARS: [&str; 6] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
];

#[test]
fn proxy_variables_are_ignored() {
    let proxy = TcpListener::bind("127.0.0.1:0").expect("proxy listener");
    let target = TcpListener::bind("127.0.0.1:0").expect("target listener");
    proxy.set_nonblocking(true).expect("nonblocking");
    target.set_nonblocking(true).expect("nonblocking");

    let proxy_url = format!("http://{}", proxy.local_addr().expect("address"));
    let target_url = format!("http://localhost:{}", target.local_addr().expect("address").port());

    // The proxy variables are set for a child process only: this process's environment is shared
    // by every test in it.
    let mut command = Command::new(std::env::current_exe().expect("test binary"));
    command
        .args(["--exact", "proxy_child", "--test-threads=1"])
        .env(CHILD_URL_VAR, &target_url)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    for var in PROXY_VARS {
        command.env(var, &proxy_url);
    }

    let mut child = command.spawn().expect("child test");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut direct = 0;

    loop {
        if proxy.accept().is_ok() {
            child.kill().ok();
            panic!("the libSQL client connected through the proxy");
        }

        // Dropping the connection unblocks the client at once.
        if target.accept().is_ok() {
            direct += 1;
        }

        if let Some(status) = child.try_wait().expect("child status") {
            assert!(status.success(), "child test failed: {status}");
            break;
        }

        if Instant::now() > deadline {
            child.kill().ok();
            panic!("the child test did not finish");
        }

        thread::sleep(Duration::from_millis(10));
    }

    assert!(proxy.accept().is_err(), "the libSQL client connected through the proxy");
    assert!(direct > 0, "the libSQL client never reached the server directly");
}

/// Connects once when run by [`proxy_variables_are_ignored`]; does nothing otherwise.
#[test]
fn proxy_child() {
    if let Ok(url) = std::env::var(CHILD_URL_VAR) {
        LibsqlStore::connect(&url, Some(TOKEN)).expect_err("the target drops the connection");
    }
}

#[test]
fn server_error_text_reaches_errors_only_as_a_bare_code() {
    for (code, shown) in [
        (json!("SQLITE_ERROR"), Some("SQLITE_ERROR")),
        (json!("near \"hunter2\": syntax error"), None),
        (json!("sqlite_error"), None),
        (json!("SQLITE_ERROR_WITH_A_CODE_LONGER_THAN_FORTY"), None),
        (Value::Null, None),
    ] {
        let url = fake_server(move |_| {
            let error = json!({ "message": "no such table: hunter2", "code": code });
            reply(None, None, vec![json!({ "type": "error", "error": error })])
        });

        let error = LibsqlStore::connect(&url, Some(TOKEN)).expect_err("the server fails");
        let message = error.to_string();

        assert!(matches!(error, StoreError::Backend(_)), "{error:?}");
        assert!(
            !message.contains("hunter2") && !message.contains("sqlite_error"),
            "{message}"
        );
        assert!(!message.contains("LONGER"), "{message}");

        match shown {
            Some(code) => assert!(message.contains(code), "{message}"),
            None => assert!(message.contains("statement failed"), "{message}"),
        }
    }
}

#[test]
fn a_rejected_foreign_keys_pragma_stops_the_write_and_is_named() {
    let url = fake_server(|body| {
        if body.contains("foreign_keys") {
            let refused = json!({ "type": "error", "error": { "message": "not allowed", "code": "SQLITE_AUTH" } });
            return reply(Some("baton"), None, vec![refused, executed(Vec::new())]);
        }

        if body.contains("user_version") {
            return reply(None, None, vec![executed(vec![vec![integer(SCHEMA_VERSION)]])]);
        }

        // The close that ends the refused stream.
        reply(None, None, Vec::new())
    });

    let mut store = LibsqlStore::connect(&url, Some(TOKEN)).expect("connect");
    let error = store
        .remember(&repo(REPO), &draft("Title", "Body", NoteKind::Note))
        .expect_err("refused");
    let message = error.to_string();

    assert!(matches!(error, StoreError::Backend(_)), "{error:?}");
    assert!(message.contains("PRAGMA foreign_keys"), "{message}");
    assert!(message.contains("SQLITE_AUTH"), "{message}");
}

#[test]
fn a_stream_sent_to_another_address_is_refused() {
    let elsewhere = format!("http://127.0.0.1:{}", unused_port());
    let url = fake_server(move |body| {
        if body.contains("BEGIN IMMEDIATE") {
            return reply(
                Some("baton"),
                Some(&elsewhere),
                vec![executed(Vec::new()), executed(Vec::new())],
            );
        }

        reply(None, None, vec![executed(vec![vec![integer(SCHEMA_VERSION)]])])
    });

    let mut store = LibsqlStore::connect(&url, Some(TOKEN)).expect("connect");
    let error = store
        .remember(&repo(REPO), &draft("Title", "Body", NoteKind::Note))
        .expect_err("refused");
    let message = error.to_string();

    assert!(message.contains("another address"), "{message}");
    assert!(!message.contains("127.0.0.1"), "{message}");
}

#[test]
fn a_commit_whose_reply_is_lost_may_have_applied_and_is_not_repeated() {
    let Some(db) = Database::libsql() else {
        return;
    };
    let Database::Libsql(server) = &db else {
        unreachable!("a libSQL database");
    };
    let Some(upstream) = server.url.strip_prefix("http://") else {
        eprintln!("skipped: the lost-commit case needs a plain http:// server");
        return;
    };

    let (url, armed) = lose_one_commit_reply(upstream.trim_end_matches('/').to_owned());
    let mut store = LibsqlStore::connect(&url, server.token.as_deref()).expect("connect");

    armed.store(true, Ordering::SeqCst);
    let error = store
        .remember(&repo(REPO), &draft("Lost reply", "Body", NoteKind::Note))
        .expect_err("the reply is lost");
    let message = error.to_string();

    assert!(matches!(error, StoreError::Backend(_)), "{error:?}");
    assert!(message.contains("may have been applied"), "{message}");
    assert_eq!(db.integer("SELECT count(*) FROM notes"), 1);
    assert_eq!(db.integer("SELECT revision FROM notes"), 1);

    store
        .remember(&repo(REPO), &draft("Next", "Body", NoteKind::Note))
        .expect("the next write goes through");
    assert_eq!(db.integer("SELECT count(*) FROM notes"), 2);
}

/// A relay to `upstream` (`host:port`) that, once armed, forwards the next request with a
/// `COMMIT` but drops the connection instead of returning the server's reply.
fn lose_one_commit_reply(upstream: String) -> (String, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("relay listener");
    let url = format!("http://127.0.0.1:{}", listener.local_addr().expect("address").port());
    let armed = Arc::new(AtomicBool::new(false));
    let shared = Arc::clone(&armed);

    thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(client) = client else {
                break;
            };
            let server = TcpStream::connect(&upstream).expect("libSQL server");
            relay(client, server, Arc::clone(&shared));
        }
    });

    (url, armed)
}

fn relay(client: TcpStream, server: TcpStream, armed: Arc<AtomicBool>) {
    let lost = Arc::new(AtomicBool::new(false));

    let mut requests = client.try_clone().expect("clone");
    let mut to_server = server.try_clone().expect("clone");
    let losing = Arc::clone(&lost);
    thread::spawn(move || {
        let mut buffer = vec![0; 64 * 1024];

        while let Ok(read @ 1..) = requests.read(&mut buffer) {
            let chunk = &buffer[..read];

            // Set before the request goes on, so its reply is the one dropped.
            if chunk.windows(6).any(|window| window == b"COMMIT") && armed.swap(false, Ordering::SeqCst) {
                losing.store(true, Ordering::SeqCst);
            }

            if to_server.write_all(chunk).is_err() {
                break;
            }
        }

        to_server.shutdown(Shutdown::Write).ok();
    });

    let mut replies = server;
    let mut to_client = client;
    thread::spawn(move || {
        let mut buffer = vec![0; 64 * 1024];

        while let Ok(read @ 1..) = replies.read(&mut buffer) {
            // The server has answered the commit: it ran. The client never hears so.
            if lost.load(Ordering::SeqCst) {
                to_client.shutdown(Shutdown::Both).ok();
                break;
            }

            if to_client.write_all(&buffer[..read]).is_err() {
                break;
            }
        }
    });
}

/// A local HTTP server answering each pipeline request body with `answer(body)`; its URL.
fn fake_server(answer: impl Fn(&str) -> Value + Send + Sync + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake listener");
    let url = format!("http://127.0.0.1:{}", listener.local_addr().expect("address").port());
    let answer = Arc::new(answer);

    thread::spawn(move || {
        for connection in listener.incoming() {
            let Ok(connection) = connection else {
                break;
            };
            let answer = Arc::clone(&answer);
            thread::spawn(move || serve(connection, answer.as_ref()));
        }
    });

    url
}

/// Answers requests on one keep-alive connection until the client closes it.
fn serve(connection: TcpStream, answer: &dyn Fn(&str) -> Value) {
    let mut writer = connection.try_clone().expect("clone");
    let mut reader = BufReader::new(connection);

    loop {
        let mut length = 0;

        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }

            let line = line.trim_end();
            if line.is_empty() {
                break;
            }

            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().expect("content length");
            }
        }

        let mut body = vec![0; length];
        reader.read_exact(&mut body).expect("request body");

        let reply = answer(&String::from_utf8(body).expect("UTF-8 body")).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{reply}",
            reply.len()
        );

        if writer.write_all(response.as_bytes()).is_err() {
            return;
        }
    }
}

/// A pipeline reply; one-shot streams get a trailing `close` result.
fn reply(baton: Option<&str>, base_url: Option<&str>, mut results: Vec<Value>) -> Value {
    if baton.is_none() {
        results.push(json!({ "type": "ok", "response": { "type": "close" } }));
    }

    json!({ "baton": baton, "base_url": base_url, "results": results })
}

fn executed(rows: Vec<Vec<Value>>) -> Value {
    json!({
        "type": "ok",
        "response": { "type": "execute", "result": { "cols": [], "rows": rows, "affected_row_count": 0, "last_insert_rowid": null } },
    })
}

fn integer(value: i64) -> Value {
    json!({ "type": "integer", "value": value.to_string() })
}

fn unused_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("listener")
        .local_addr()
        .expect("address")
        .port()
}
