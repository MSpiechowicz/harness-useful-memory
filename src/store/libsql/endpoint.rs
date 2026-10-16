//! The server URL: `libsql://` and `https://` go over TLS; plain `http://` is accepted only for
//! the same machine (`localhost`, `127.0.0.1`, `[::1]`).
//!
//! Credentials never travel in the URL: one with a user, a password, a query or a fragment is
//! refused, so the URL is safe to show and the token goes in the `Authorization` header only.
//! Error messages never repeat the URL.
//!
//! A server may name another base URL for a stream's later requests ([`may_follow`]): the same
//! origin only, except that a Turso database (`*.turso.io` over `https://`) may move a stream to
//! another `*.turso.io` host on the same port.

use crate::store::StoreError;

/// Hosts that may be reached without TLS.
const LOOPBACK: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// The base URL requests go to (`scheme://host[:port][/path]`, no trailing slash) for `raw`.
pub(super) fn parse(raw: &str) -> Result<String, StoreError> {
    let raw = raw.trim();
    let Some((scheme, rest)) = raw.split_once("://") else {
        return Err(invalid("the libSQL URL must start with libsql://, https:// or http://"));
    };

    let secure = match scheme.to_ascii_lowercase().as_str() {
        "libsql" | "https" => true,
        "http" => false,
        _ => return Err(invalid("the libSQL URL must start with libsql://, https:// or http://")),
    };

    if rest.contains(['?', '#']) {
        return Err(invalid(
            "the libSQL URL must not carry a query or a fragment; give the token separately",
        ));
    }

    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));

    if authority.contains('@') {
        return Err(invalid(
            "the libSQL URL must not carry a user or a password; give the token separately",
        ));
    }

    let host = host_of(authority)?.to_ascii_lowercase();

    if !secure && !LOOPBACK.contains(&host.as_str()) {
        return Err(invalid(
            "plain http:// is allowed only for localhost, 127.0.0.1 and [::1]; use libsql:// or https://",
        ));
    }

    let scheme = if secure { "https" } else { "http" };
    Ok(format!("{scheme}://{authority}{}", path.trim_end_matches('/')))
}

/// The Turso domain whose hosts may hand streams to each other.
const TURSO: &str = "turso.io";

/// True when a stream on the [`parse`]d base URL `from` may send its later requests (and the
/// token) to the [`parse`]d base URL `to`: the same origin, or for a Turso database another
/// Turso host over `https://` on the same port.
pub(super) fn may_follow(from: &str, to: &str) -> bool {
    let (from_scheme, from_authority) = scheme_and_authority(from);
    let (to_scheme, to_authority) = scheme_and_authority(to);

    if from_scheme.eq_ignore_ascii_case(to_scheme) && from_authority.eq_ignore_ascii_case(to_authority) {
        return true;
    }

    let (Ok(from_host), Ok(to_host)) = (host_of(from_authority), host_of(to_authority)) else {
        return false;
    };

    let from_port = &from_authority[from_host.len()..];
    let to_port = &to_authority[to_host.len()..];

    from_scheme == "https" && to_scheme == "https" && from_port == to_port && is_turso(from_host) && is_turso(to_host)
}

/// True for a host strictly under [`TURSO`] (a database's host, never the bare domain).
fn is_turso(host: &str) -> bool {
    let host = host.to_ascii_lowercase();

    match host.strip_suffix(TURSO).and_then(|rest| rest.strip_suffix('.')) {
        Some(name) => !name.is_empty() && !name.starts_with('.') && !name.ends_with('.'),
        None => false,
    }
}

/// The scheme and `host[:port]` of a [`parse`]d base URL.
fn scheme_and_authority(base: &str) -> (&str, &str) {
    let (scheme, rest) = base.split_once("://").unwrap_or(("", base));
    let end = rest.find('/').unwrap_or(rest.len());
    (scheme, &rest[..end])
}

/// The host of `host[:port]` (an IPv6 address keeps its brackets).
fn host_of(authority: &str) -> Result<&str, StoreError> {
    let (host, port) = match authority.strip_prefix('[') {
        Some(rest) => {
            let Some(end) = rest.find(']') else {
                return Err(invalid("the libSQL URL has an unclosed [ in its host"));
            };
            authority.split_at(end + 2)
        }
        None => authority.split_at(authority.find(':').unwrap_or(authority.len())),
    };

    let port_ok = match port.strip_prefix(':') {
        Some(digits) => !digits.is_empty() && digits.len() <= 5 && digits.bytes().all(|byte| byte.is_ascii_digit()),
        None => port.is_empty(),
    };

    let host_ok = !host.is_empty()
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-_[]:".contains(&byte));

    if !host_ok || !port_ok {
        return Err(invalid("the libSQL URL needs a host name and at most a numeric port"));
    }

    Ok(host)
}

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{may_follow, parse};

    fn follows(from: &str, to: &str) -> bool {
        may_follow(&parse(from).expect("from"), &parse(to).expect("to"))
    }

    #[test]
    fn a_stream_stays_on_its_origin() {
        assert!(follows("http://127.0.0.1:8080", "http://127.0.0.1:8080/v2"));
        assert!(follows("https://db.example.com", "https://DB.example.com"));
        assert!(follows("libsql://db.example.com", "https://db.example.com"));

        assert!(!follows("http://127.0.0.1:8080", "http://127.0.0.1:8081"));
        assert!(!follows("http://localhost:8080", "http://127.0.0.1:8080"));
        assert!(!follows("https://db.example.com", "https://other.example.com"));
        assert!(!follows("https://a.example.turso.io.evil.com", "https://b.turso.io"));
    }

    #[test]
    fn a_turso_stream_may_move_to_another_turso_host_over_https() {
        assert!(follows(
            "libsql://db-org.turso.io",
            "https://db-org.aws-us-east-1.turso.io"
        ));
        assert!(follows(
            "https://db-org.aws-eu-west-1.turso.io",
            "https://db-org.turso.io/"
        ));

        assert!(!follows("libsql://db-org.turso.io", "https://turso.io"));
        assert!(!follows("libsql://db-org.turso.io", "https://db-org.turso.io.evil.com"));
        assert!(!follows("libsql://db-org.turso.io", "https://evilturso.io"));
        assert!(!follows("libsql://db-org.turso.io", "https://db-org.turso.io:8443"));
        assert!(!follows("libsql://turso.io", "https://db-org.turso.io"));
        assert!(!follows("https://db.example.com", "https://db-org.turso.io"));
    }
}
