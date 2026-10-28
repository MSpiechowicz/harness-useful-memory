//! Turning a `postgres://` URL into a client configuration and a TLS policy.
//!
//! TLS is required for every host that is not this machine: `sslmode` may be left out (it then
//! means `require`), `allow` and `prefer` are raised to `require`, and `disable` is refused. A
//! local server (`localhost`, a loopback address or a Unix socket folder) follows `sslmode`, with
//! `prefer` as the default. TLS always verifies the certificate chain and the host name (rustls),
//! so `require` behaves like libpq's `verify-full`; `verify-ca` and `verify-full` are accepted as
//! the same. The trusted roots are the platform's, or the PEM file `sslrootcert` names instead.
//!
//! `sslmode` and `sslrootcert` are taken out of the URL here because the client library knows
//! only `disable`, `prefer` and `require` and has no `sslrootcert`; every other parameter is the
//! library's (`options`, `connect_timeout`, `application_name`, `channel_binding`, ...).
//!
//! A connection to a network that goes silent (a dropped route, a firewall that forgets the
//! connection) ends in seconds instead of hanging for the system's two hours: `tcp_user_timeout`
//! gives up on data the other side does not acknowledge, and TCP keepalives probe an idle one.
//! Neither cuts a healthy connection, also not one waiting for a long statement (the server
//! acknowledges the probes). A URL that sets any of these parameters keeps its own value.

use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use postgres::config::{Host, SslMode};
use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use tokio_postgres_rustls::MakeRustlsConnect;

/// How long connecting may take when the URL sets no `connect_timeout`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long sent data may stay unacknowledged before the connection is closed (Linux only): a
/// healthy server acknowledges within a round trip, whatever its statement takes.
const TCP_USER_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a connection is idle before the first keepalive probe.
const KEEPALIVES_IDLE: Duration = Duration::from_secs(15);

/// The pause between keepalive probes.
const KEEPALIVES_INTERVAL: Duration = Duration::from_secs(5);

/// How many keepalive probes go unanswered before the connection is closed.
const KEEPALIVES_RETRIES: u32 = 3;

/// The `application_name` the server shows for these sessions, unless the URL names one.
const APPLICATION_NAME: &str = "harness-memory";

/// Why a URL was refused. Messages name parameters, never their values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    #[error("the database URL is not a valid postgres:// URL ({0})")]
    Invalid(String),
    #[error(
        "this sslmode is not supported; use disable (local servers only), prefer, require, verify-ca or verify-full"
    )]
    UnknownSslMode,
    #[error(
        "sslmode=disable is only allowed for a server on this machine; the team database needs TLS \
         (leave sslmode out or use require)"
    )]
    PlaintextRemote,
}

/// What [`parse`] makes of a URL.
pub struct Settings {
    pub config: postgres::Config,
    /// Every host is this machine.
    pub local: bool,
    /// The PEM file of `sslrootcert`, trusted instead of the platform's roots.
    pub root_cert: Option<PathBuf>,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("ssl_mode", &self.config.get_ssl_mode())
            .field("local", &self.local)
            .field("root_cert", &self.root_cert)
            .finish_non_exhaustive()
    }
}

/// Parses `url` and applies the TLS policy of the module docs.
pub fn parse(url: &str) -> Result<Settings, UrlError> {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };

    let mut kept: Vec<&str> = Vec::new();
    let mut ssl_mode: Option<String> = None;
    let mut root_cert: Option<PathBuf> = None;

    for pair in query.unwrap_or_default().split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));

        match key {
            "sslmode" => ssl_mode = Some(percent_decode(value)?),
            "sslrootcert" => root_cert = Some(PathBuf::from(percent_decode(value)?)),
            _ => kept.push(pair),
        }
    }

    let rebuilt = match kept.is_empty() {
        true => base.to_owned(),
        false => format!("{base}?{}", kept.join("&")),
    };

    let mut config = postgres::Config::from_str(&rebuilt).map_err(|error| UrlError::Invalid(error.to_string()))?;
    let local = is_local(config.get_hosts(), config.get_hostaddrs());
    config.ssl_mode(ssl_policy(ssl_mode.as_deref(), local)?);

    if config.get_connect_timeout().is_none() {
        config.connect_timeout(CONNECT_TIMEOUT);
    }

    if config.get_application_name().is_none() {
        config.application_name(APPLICATION_NAME);
    }

    let named = |name: &str| kept.iter().any(|pair| pair.split('=').next() == Some(name));

    if !named("tcp_user_timeout") {
        config.tcp_user_timeout(TCP_USER_TIMEOUT);
    }

    if !named("keepalives_idle") {
        config.keepalives_idle(KEEPALIVES_IDLE);
    }

    if !named("keepalives_interval") {
        config.keepalives_interval(KEEPALIVES_INTERVAL);
    }

    if !named("keepalives_retries") {
        config.keepalives_retries(KEEPALIVES_RETRIES);
    }

    Ok(Settings {
        config,
        local,
        root_cert,
    })
}

/// The TLS mode for `sslmode` (as written in the URL) on a local or remote host.
fn ssl_policy(requested: Option<&str>, local: bool) -> Result<SslMode, UrlError> {
    let mode = match requested {
        None if local => SslMode::Prefer,
        None => SslMode::Require,
        Some("disable") if local => SslMode::Disable,
        Some("disable") => return Err(UrlError::PlaintextRemote),
        Some("allow" | "prefer") if local => SslMode::Prefer,
        Some("allow" | "prefer" | "require" | "verify-ca" | "verify-full") => SslMode::Require,
        Some(_) => return Err(UrlError::UnknownSslMode),
    };

    Ok(mode)
}

/// True when every host (and every `hostaddr`) is this machine: `localhost`, a loopback address
/// or a Unix socket folder. A URL without a host connects to the local socket.
fn is_local(hosts: &[Host], hostaddrs: &[IpAddr]) -> bool {
    let local_host = |host: &Host| match host {
        Host::Tcp(name) => {
            let name = name.trim_start_matches('[').trim_end_matches(']');
            name.eq_ignore_ascii_case("localhost") || name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
        }
        #[cfg(unix)]
        Host::Unix(_) => true,
    };

    hosts.iter().all(local_host) && hostaddrs.iter().all(IpAddr::is_loopback)
}

/// The rustls connector: trusting the PEM file `root_cert` when given, else the platform's roots.
/// `required` makes a missing trust store an error (with `prefer` a plaintext server still works).
pub fn tls(root_cert: Option<&PathBuf>, required: bool) -> Result<MakeRustlsConnect, String> {
    let mut roots = RootCertStore::empty();

    match root_cert {
        Some(path) => {
            let certs = CertificateDer::pem_file_iter(path)
                .map_err(|error| format!("cannot read sslrootcert {}: {error}", path.display()))?;

            for cert in certs {
                let cert = cert.map_err(|error| format!("cannot read sslrootcert {}: {error}", path.display()))?;
                roots.add(cert).map_err(|error| {
                    format!("sslrootcert {} holds an unusable certificate: {error}", path.display())
                })?;
            }
        }
        None => {
            let native = rustls_native_certs::load_native_certs();
            roots.add_parsable_certificates(native.certs);
        }
    }

    if required && roots.is_empty() {
        return Err(
            "no trusted root certificates were found; name the server's CA file with sslrootcert in the URL".to_owned(),
        );
    }

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("TLS setup failed: {error}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();

    Ok(MakeRustlsConnect::new(config))
}

/// Decodes `%XX` escapes (and `+` is kept as is, like libpq).
fn percent_decode(raw: &str) -> Result<String, UrlError> {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] != b'%' {
            out.push(bytes[index]);
            index += 1;
            continue;
        }

        let hex = raw
            .get(index + 1..index + 3)
            .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            .ok_or_else(|| UrlError::Invalid("a bad % escape".to_owned()))?;
        out.push(hex);
        index += 3;
    }

    String::from_utf8(out).map_err(|_| UrlError::Invalid("a parameter is not UTF-8".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(url: &str) -> Result<SslMode, UrlError> {
        parse(url).map(|settings| settings.config.get_ssl_mode())
    }

    #[test]
    fn remote_hosts_require_tls() {
        assert_eq!(mode("postgres://u:p@db.example.com/memory"), Ok(SslMode::Require));
        assert_eq!(mode("postgres://db.example.com/m?sslmode=prefer"), Ok(SslMode::Require));
        assert_eq!(mode("postgres://db.example.com/m?sslmode=allow"), Ok(SslMode::Require));
        assert_eq!(
            mode("postgres://db.example.com/m?sslmode=verify-full"),
            Ok(SslMode::Require)
        );
        assert_eq!(
            mode("postgresql://u:p@10.0.0.5:5432/m?sslmode=disable"),
            Err(UrlError::PlaintextRemote)
        );
    }

    #[test]
    fn local_hosts_follow_sslmode() {
        assert_eq!(mode("postgres://u:p@localhost/m"), Ok(SslMode::Prefer));
        assert_eq!(
            mode("postgres://127.0.0.1:55432/m?sslmode=disable"),
            Ok(SslMode::Disable)
        );
        assert_eq!(mode("postgres://[::1]/m?sslmode=require"), Ok(SslMode::Require));
        // A socket folder is local only where there are Unix sockets. Windows reads it as a host name.
        #[cfg(unix)]
        assert_eq!(mode("postgres:///m?host=/run/postgresql"), Ok(SslMode::Prefer));
    }

    #[test]
    fn a_local_host_name_with_a_remote_address_is_remote() {
        assert_eq!(
            mode("postgres://localhost/m?hostaddr=10.0.0.5&sslmode=disable"),
            Err(UrlError::PlaintextRemote)
        );
        assert_eq!(
            mode("postgres://localhost,db.example.com/m?sslmode=disable"),
            Err(UrlError::PlaintextRemote)
        );
    }

    #[test]
    fn libpq_only_parameters_are_taken_out_and_others_kept() {
        let settings = parse(
            "postgres://u@db.example.com/m?sslrootcert=%2Fetc%2Fca.pem&options=-csearch_path%3Dteam&sslmode=require",
        )
        .expect("parses");

        assert_eq!(settings.root_cert, Some(PathBuf::from("/etc/ca.pem")));
        assert_eq!(settings.config.get_options(), Some("-csearch_path=team"));
        assert_eq!(settings.config.get_connect_timeout(), Some(&CONNECT_TIMEOUT));
        assert_eq!(settings.config.get_application_name(), Some(APPLICATION_NAME));
        assert!(!settings.local);
    }

    #[test]
    fn a_silent_network_ends_the_connection_unless_the_url_says_otherwise() {
        let config = parse("postgres://u@db.example.com/m").expect("parses").config;

        assert_eq!(config.get_tcp_user_timeout(), Some(&TCP_USER_TIMEOUT));
        assert!(config.get_keepalives());
        assert_eq!(config.get_keepalives_idle(), KEEPALIVES_IDLE);
        assert_eq!(config.get_keepalives_interval(), Some(KEEPALIVES_INTERVAL));
        assert_eq!(config.get_keepalives_retries(), Some(KEEPALIVES_RETRIES));

        let own = parse("postgres://u@db.example.com/m?tcp_user_timeout=60&keepalives_idle=90&keepalives_retries=9")
            .expect("parses")
            .config;

        assert_eq!(own.get_tcp_user_timeout(), Some(&Duration::from_secs(60)));
        assert_eq!(own.get_keepalives_idle(), Duration::from_secs(90));
        assert_eq!(own.get_keepalives_interval(), Some(KEEPALIVES_INTERVAL));
        assert_eq!(own.get_keepalives_retries(), Some(9));
    }

    #[test]
    fn bad_values_are_named_without_echoing_them() {
        let unknown = mode("postgres://db.example.com/m?sslmode=hunter2").expect_err("unknown mode");
        assert_eq!(unknown, UrlError::UnknownSslMode);

        let invalid = mode("postgres://u:hunter2@db.example.com/m?bogus=1").expect_err("unknown option");
        assert!(!invalid.to_string().contains("hunter2"), "{invalid}");
    }
}
