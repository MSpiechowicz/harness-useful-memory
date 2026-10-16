//! Connecting to a libSQL server: which URLs are accepted, and that failures name neither the URL
//! nor the token. None of these needs a running server.

use harness_memory::store::{LibsqlStore, StoreError};

const TOKEN: &str = "eyJsecret-token-value";

fn refused(url: &str) -> String {
    match LibsqlStore::connect(url, Some(TOKEN)) {
        Err(StoreError::Invalid(message)) => message,
        other => panic!("{url}: expected an invalid URL, got {other:?}"),
    }
}

#[test]
fn plain_http_is_refused_beyond_this_machine() {
    let message = refused("http://db.example.com:8080");
    assert!(message.contains("libsql://"), "{message}");
    assert!(!message.contains("example.com"), "{message}");

    refused("http://10.0.0.5:8080");
    refused("http://127.0.0.2:8080");
    refused("http://localhost.example.com");
}

#[test]
fn credentials_in_the_url_are_refused_and_never_repeated() {
    for url in [
        "https://user:hunter2@db.example.com",
        "libsql://db.example.com?authToken=hunter2",
        "https://db.example.com/#hunter2",
    ] {
        let message = refused(url);
        assert!(!message.contains("hunter2"), "{message}");
        assert!(message.contains("token separately"), "{message}");
    }
}

#[test]
fn other_schemes_and_malformed_hosts_are_refused() {
    for url in [
        "db.example.com",
        "ftp://db.example.com",
        "file:///tmp/memory.db",
        "postgres://db.example.com",
        "https://",
        "https://db.example.com:port",
        "https://[::1",
        "https://db example.com",
    ] {
        refused(url);
    }
}

#[test]
fn an_unreachable_server_fails_without_naming_the_url_or_the_token() {
    // Port 9 (discard) on loopback: nothing listens there on a test machine.
    for url in ["http://127.0.0.1:9", "http://[::1]:9/", "http://LOCALHOST:9"] {
        let error = LibsqlStore::connect(url, Some(TOKEN)).expect_err("nothing listens");
        let message = error.to_string();

        assert!(matches!(error, StoreError::Backend(_)), "{error:?}");
        assert!(!message.contains(TOKEN) && !message.contains(":9"), "{message}");
    }
}

#[test]
fn a_token_that_cannot_be_a_header_is_refused() {
    let error = LibsqlStore::connect("http://127.0.0.1:9", Some("line\nbreak")).expect_err("refused");
    assert!(matches!(error, StoreError::Invalid(_)), "{error:?}");
    assert!(!error.to_string().contains("break"), "{error}");
}

#[test]
fn a_wrong_token_is_refused_without_repeating_it() {
    let (Ok(url), Ok(_token)) = (
        std::env::var("HARNESS_MEMORY_TEST_LIBSQL_URL"),
        std::env::var("HARNESS_MEMORY_TEST_LIBSQL_TOKEN"),
    ) else {
        eprintln!("skipped: set HARNESS_MEMORY_TEST_LIBSQL_URL and _TOKEN for a server that checks tokens");
        return;
    };

    let error = LibsqlStore::connect(&url, Some(TOKEN)).expect_err("wrong token");
    let message = error.to_string();
    assert!(message.contains("refused the token"), "{message}");
    assert!(!message.contains(TOKEN) && !message.contains(&url), "{message}");
}
