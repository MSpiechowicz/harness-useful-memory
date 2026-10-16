//! The connection URL: the scheme, where the servers are, the TLS rules, and error messages that
//! never carry the credentials of the URL.

use std::net::{Ipv4Addr, Ipv6Addr};

use mongodb::error::{Error, ErrorKind, WriteFailure};
use mongodb::options::{ClientOptions, ServerAddress, Tls};

use crate::store::StoreError;
use crate::support::redact::redact_text;

/// What [`check`] learned from a URL.
pub(super) struct Checked {
    /// The driver talks to this machine only (see [`local_seeds`]), so TLS may be off.
    pub local: bool,
}

/// Checks the scheme and the TLS options of `url` before the driver reads it (so a refused URL
/// costs no DNS lookup). A server that is not on this machine must use TLS with certificate checks.
pub(super) fn check(url: &str) -> Result<Checked, StoreError> {
    let (srv, rest) = if let Some(rest) = url.strip_prefix("mongodb+srv://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("mongodb://") {
        (false, rest)
    } else {
        return Err(StoreError::Invalid(
            "a MongoDB URL starts with mongodb:// or mongodb+srv://".to_owned(),
        ));
    };

    let (address, query) = rest.split_once('?').unwrap_or((rest, ""));
    let authority = address.split('/').next().unwrap_or("");
    let hosts = authority.rsplit_once('@').map_or(authority, |(_, hosts)| hosts);

    let options: Vec<(String, String)> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.to_ascii_lowercase()))
        .collect();

    let single_server = options
        .iter()
        .any(|(key, value)| matches!(key.as_str(), "directconnection" | "loadbalanced") && value == "true");

    let local = !srv && local_seeds(hosts, single_server);

    if local {
        return Ok(Checked { local });
    }

    for (key, value) in &options {
        if matches!(key.as_str(), "tls" | "ssl") && value == "false" {
            return Err(tls_required());
        }

        let unchecked = matches!(
            key.as_str(),
            "tlsinsecure" | "tlsallowinvalidcertificates" | "tlsallowinvalidhostnames"
        );
        if unchecked && value == "true" {
            return Err(certificates_required());
        }
    }

    Ok(Checked { local })
}

/// True when the driver can only reach this machine through `hosts` (the comma-separated seeds).
///
/// Loopback seeds alone are not enough: without `directConnection=true` or `loadBalanced=true`
/// the driver also connects to the replica-set members the server lists, which may be remote.
/// Unix socket seeds are local by themselves.
fn local_seeds(hosts: &str, single_server: bool) -> bool {
    if hosts.is_empty() {
        return false;
    }

    let mut seeds = hosts.split(',');

    if seeds.clone().all(is_socket) {
        return true;
    }

    single_server && seeds.all(is_local_host)
}

/// Turns TLS on for a remote server when the URL left it unset, and refuses options the driver
/// parsed that switch it or its certificate checks off (the authoritative check after [`check`]).
pub(super) fn enforce_tls(options: &mut ClientOptions, checked: &Checked) -> Result<(), StoreError> {
    if checked.local && single_server(options) {
        return Ok(());
    }

    match &options.tls {
        None => {
            options.tls = Some(Tls::Enabled(Default::default()));
            Ok(())
        }
        Some(Tls::Disabled) => Err(tls_required()),
        Some(Tls::Enabled(tls)) => {
            // `tlsAllowInvalidHostnames` is refused by [`check`]; rustls has no such setting.
            if tls.allow_invalid_certificates == Some(true) {
                return Err(certificates_required());
            }

            Ok(())
        }
    }
}

/// True when the parsed options keep the driver on the seeds: a direct or load-balanced
/// connection, or Unix sockets only.
fn single_server(options: &ClientOptions) -> bool {
    if options.direct_connection == Some(true) || options.load_balanced == Some(true) {
        return true;
    }

    !options.hosts.is_empty() && options.hosts.iter().all(is_socket_address)
}

#[cfg(unix)]
fn is_socket_address(address: &ServerAddress) -> bool {
    matches!(address, ServerAddress::Unix { .. })
}

#[cfg(not(unix))]
fn is_socket_address(_address: &ServerAddress) -> bool {
    false
}

fn tls_required() -> StoreError {
    StoreError::Invalid(
        "TLS is required for a MongoDB server that is not on this machine: remove tls=false (or ssl=false) \
         from the URL"
            .to_owned(),
    )
}

fn certificates_required() -> StoreError {
    StoreError::Invalid(
        "certificate checks are required for a MongoDB server that is not on this machine: remove \
         tlsInsecure, tlsAllowInvalidCertificates and tlsAllowInvalidHostnames from the URL"
            .to_owned(),
    )
}

/// True for a Unix socket path (`%2F...sock`).
fn is_socket(host: &str) -> bool {
    let lower = host.trim().to_ascii_lowercase();
    lower.starts_with("%2f") || lower.ends_with(".sock")
}

/// True for `localhost`, a loopback address or a Unix socket path.
fn is_local_host(host: &str) -> bool {
    if is_socket(host) {
        return true;
    }

    let lower = host.trim().to_ascii_lowercase();

    let name = match lower.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or(""),
        None => lower.rsplit_once(':').map_or(lower.as_str(), |(name, _)| name),
    };

    if name == "localhost" {
        return true;
    }

    if let Ok(address) = name.parse::<Ipv4Addr>() {
        return address.is_loopback();
    }

    name.parse::<Ipv6Addr>().is_ok_and(|address| address.is_loopback())
}

/// A driver error while reading the URL or creating the client, as a message without the URL's
/// credentials. Only messages the driver builds from the URL and local settings are shown; DNS
/// answers (SRV and TXT records) and server replies are not.
pub(super) fn connect_error(error: &Error, url: &str) -> StoreError {
    let detail = match error.kind.as_ref() {
        ErrorKind::InvalidArgument { message, .. } => Some(format!("invalid MongoDB URL: {message}")),
        ErrorKind::InvalidTlsConfig { message, .. } => Some(format!("invalid TLS settings: {message}")),
        _ => None,
    };

    match detail {
        Some(detail) => StoreError::Backend(scrub(&detail, url).chars().take(200).collect()),
        None => backend(error),
    }
}

/// A driver error as a short message: its kind and server error code (with the code name when it
/// is a plain word), never server text or the URL.
pub(super) fn backend(error: &Error) -> StoreError {
    let message = match error.kind.as_ref() {
        ErrorKind::Authentication { .. } => {
            "authentication failed (check the user, the password and authSource)".to_owned()
        }
        ErrorKind::ServerSelection { .. } => "cannot reach a usable MongoDB server (timed out)".to_owned(),
        ErrorKind::Io(io) => format!("network error ({:?})", io.kind()),
        ErrorKind::DnsResolve { .. } => "cannot resolve the MongoDB host".to_owned(),
        ErrorKind::InvalidTlsConfig { .. } => "invalid TLS settings".to_owned(),
        ErrorKind::InvalidArgument { .. } => "invalid MongoDB settings".to_owned(),
        ErrorKind::Command(command) => coded("MongoDB command failed", Some(&command.code_name), command.code),
        ErrorKind::Write(WriteFailure::WriteError(write)) => {
            coded("write failed", write.code_name.as_deref(), write.code)
        }
        ErrorKind::Write(WriteFailure::WriteConcernError(concern)) => {
            coded("write concern failed", Some(&concern.code_name), concern.code)
        }
        ErrorKind::Transaction { .. } => "transaction failed".to_owned(),
        ErrorKind::IncompatibleServer { .. } => "the MongoDB server version is not supported".to_owned(),
        ErrorKind::SessionsNotSupported => "the MongoDB server does not support sessions".to_owned(),
        _ => "MongoDB request failed".to_owned(),
    };

    StoreError::Backend(message)
}

/// `what` with the server's error code, and its code name when that is 1 to 40 ASCII letters
/// (anything else the server sent is left out).
fn coded(what: &str, code_name: Option<&str>, code: i32) -> String {
    let plain = |name: &&str| (1..=40).contains(&name.len()) && name.bytes().all(|byte| byte.is_ascii_alphabetic());

    match code_name.filter(plain) {
        Some(name) => format!("{what}: {name} ({code})"),
        None => format!("{what} ({code})"),
    }
}

/// `message` with the user name and password of `url` (raw and percent-decoded) masked, then
/// passed through `redact_text`.
fn scrub(message: &str, url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    let mut text = message.to_owned();

    if let Some((user_info, _)) = authority.rsplit_once('@') {
        let mut secrets: Vec<String> = vec![user_info.to_owned()];

        for part in user_info.split(':') {
            secrets.push(part.to_owned());
            secrets.push(percent_decode(part));
        }

        secrets.retain(|secret| !secret.is_empty());
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));

        for secret in secrets {
            text = text.replace(&secret, "••••");
        }
    }

    redact_text(&text)
}

/// `%XX` escapes decoded (invalid escapes and non-UTF-8 results are kept as they are).
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        let escaped = (bytes[index] == b'%')
            .then(|| text.get(index + 1..index + 3))
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

    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_and_sockets_are_local() {
        for host in [
            "localhost",
            "localhost:27017",
            "127.0.0.1:1",
            "[::1]:27017",
            "%2Ftmp%2Fmongodb-27017.sock",
        ] {
            assert!(is_local_host(host), "{host}");
        }

        for host in [
            "db.example.com",
            "10.0.0.5:27017",
            "[2001:db8::1]:27017",
            "localhost.example.com",
        ] {
            assert!(!is_local_host(host), "{host}");
        }
    }

    #[test]
    fn remote_servers_need_tls_with_certificate_checks() {
        let refused = [
            "mongodb://db.example.com/?tls=false",
            "mongodb://db.example.com/?SSL=FALSE",
            "mongodb+srv://cluster.example.com/?tls=false",
            "mongodb://localhost,db.example.com/?tls=false",
            "mongodb://localhost,db.example.com/?directConnection=true&tls=false",
            "mongodb://db.example.com/?tlsInsecure=true",
            "mongodb://db.example.com/?tlsAllowInvalidCertificates=true",
        ];
        for url in refused {
            assert!(matches!(check(url), Err(StoreError::Invalid(_))), "{url}");
        }

        assert!(!check("mongodb://db.example.com/x?tls=true").expect("remote").local);
        assert!(matches!(check("postgres://localhost"), Err(StoreError::Invalid(_))));
    }

    #[test]
    fn loopback_seeds_are_local_only_when_the_driver_stays_on_them() {
        // Without a direct or load-balanced connection the driver also connects to the replica-set
        // members the server lists, which may be remote: TLS rules apply.
        let discovering = [
            "mongodb://localhost/?tls=false&replicaSet=rs0",
            "mongodb://localhost:27017/?tls=false",
            "mongodb://127.0.0.1,[::1]/?tls=false",
            "mongodb://127.0.0.1/?directConnection=false&tls=false",
            "mongodb://localhost/?tlsAllowInvalidCertificates=true",
        ];
        for url in discovering {
            assert!(matches!(check(url), Err(StoreError::Invalid(_))), "{url}");
        }

        let local = [
            "mongodb://127.0.0.1:27017/?directConnection=true&tls=false",
            "mongodb://localhost:57017/x?replicaSet=rs0&DIRECTCONNECTION=TRUE",
            "mongodb://localhost/?loadBalanced=true&tls=false",
            "mongodb://%2Ftmp%2Fmongodb-27017.sock/?tls=false",
        ];
        for url in local {
            assert!(check(url).expect("local").local, "{url}");
        }

        assert!(
            !check("mongodb://localhost/?replicaSet=rs0")
                .expect("TLS turned on")
                .local
        );
    }

    #[test]
    fn parsed_options_decide_when_the_url_looked_local() {
        let parse = |url: &str| {
            ClientOptions::parse(url)
                .run()
                .unwrap_or_else(|error| panic!("{url}: {error:?}"))
        };
        let local = Checked { local: true };

        let mut direct = parse("mongodb://127.0.0.1:27017/?directConnection=true&tls=false");
        assert!(enforce_tls(&mut direct, &local).is_ok());

        let mut discovering = parse("mongodb://127.0.0.1:27017/?tls=false");
        assert!(matches!(
            enforce_tls(&mut discovering, &local),
            Err(StoreError::Invalid(_))
        ));

        let mut unset = parse("mongodb://127.0.0.1:27017/");
        enforce_tls(&mut unset, &local).expect("TLS turned on");
        assert!(matches!(unset.tls, Some(Tls::Enabled(_))));
    }

    #[test]
    fn server_code_names_are_shown_only_when_they_are_plain_words() {
        assert_eq!(
            coded("write failed", Some("DuplicateKey"), 11000),
            "write failed: DuplicateKey (11000)"
        );
        assert_eq!(coded("write failed", None, 11000), "write failed (11000)");

        for name in [
            "",
            "Duplicate Key",
            "Bad: see https://evil.example",
            "Über",
            &"A".repeat(41),
        ] {
            assert_eq!(coded("write failed", Some(name), 2), "write failed (2)", "{name}");
        }
    }

    #[test]
    fn credentials_are_masked_in_messages() {
        let url = "mongodb://al%40ice:s3cr%2Ft@db.example.com/x";
        let scrubbed = scrub("bad option for al@ice with s3cr/t and s3cr%2Ft", url);
        assert!(!scrubbed.contains("s3cr"), "{scrubbed}");
        assert!(!scrubbed.contains("al@ice"), "{scrubbed}");
    }
}
