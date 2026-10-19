//! Amazon DocumentDB: the MongoDB store in its DocumentDB mode ([`super::Flavor::DocumentDb`]).
//!
//! DocumentDB speaks the MongoDB wire protocol, so the store and its collections are the same.
//! What differs:
//!
//! - No text index and no `$text` (DocumentDB has none before engine 5.0, and none on an array
//!   field or with the language `none` after it): search matches the indexed `terms` instead (see
//!   the module docs of [`super`]).
//! - Retryable writes are off unless the URL sets `retryWrites` (engines before 8.0.2 refuse
//!   them).
//! - Every read goes to the primary, whatever `readPreference` the URL sets (the AWS console
//!   string sets `secondaryPreferred`): transactions only run on the primary, and a replica can
//!   lag behind it, so a read there could miss a note saved a moment earlier.
//! - Reads that shape fields on the server run as aggregations (see [`super::Ctx`]): a `find`
//!   projection holding expressions is MongoDB 4.4 and newer.
//! - The server certificates chain to the Amazon RDS certificate authorities, which are not in
//!   the platform's trusted roots. The bundle is built into this binary ([`BUNDLE`]) and handed to
//!   the driver in a private temporary file for each connect ([`write_bundle`]). The AWS names of
//!   the bundle in a URL stand for it in either mode (see [`super::connection::tls_files`]).
//! - A server that cannot be reached gets a hint about the VPN, at connect and later (see
//!   [`backend`]): a cluster only accepts connections from inside its AWS network. A certificate
//!   that cannot be verified gets a hint of its own instead.
//! - The connect-time probe is `isMaster` (engine 4.0 may not know `hello`), and only a replica
//!   set (`setName`) gets transactions: an elastic cluster has none, and gets a warning.

use std::io::Write;

use mongodb::error::{Error, ErrorKind};
use mongodb::options::{AuthMechanism, ClientOptions, ReadPreference, SelectionCriteria};
use tempfile::NamedTempFile;

use crate::store::StoreError;

/// The Amazon RDS global certificate bundle, which signs every DocumentDB server certificate.
///
/// Source: <https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem>, downloaded on
/// 2026-10-19, SHA-256
/// `90a6f1a7978a1b1b096dca6c57511ca48d3b8cf052f8e704baf358f34ea2f21b`.
pub const BUNDLE: &str = include_str!("../../../assets/aws-rds-global-bundle.pem");

/// The names AWS gives the bundle in its connection strings (`tlsCAFile=global-bundle.pem`).
const AWS_BUNDLE_NAMES: [&str; 2] = ["global-bundle.pem", "rds-combined-ca-bundle.pem"];

/// The domains of DocumentDB cluster endpoints, instance-based and elastic, in the global and the
/// China partitions.
const DOCUMENTDB_DOMAINS: [&str; 4] = [
    ".docdb.amazonaws.com",
    ".docdb-elastic.amazonaws.com",
    ".docdb.amazonaws.com.cn",
    ".docdb-elastic.amazonaws.com.cn",
];

/// What a cluster that cannot be reached needs.
pub const VPN_HINT: &str =
    "Amazon DocumentDB only accepts connections from inside its AWS network: connect to your VPN and try again.";

/// What a cluster whose certificate the connection refused needs.
pub const CERTIFICATE_HINT: &str = "The certificate of the Amazon DocumentDB cluster could not be verified: check \
     the host name in the URL, and leave tlsCAFile out or point it at the Amazon RDS certificate bundle.";

/// The warning for a cluster without transactions (no replica set: an elastic cluster).
pub const NO_TRANSACTIONS_WARNING: &str = "this Amazon DocumentDB cluster has no transactions (an elastic \
     cluster does not), so a write that changes several documents is not atomic as a whole. Elastic clusters are \
     untested.";

/// True when a host of the `mongodb://` or `mongodb+srv://` URL is a DocumentDB cluster endpoint
/// (`<cluster>.cluster-<id>.<region>.docdb.amazonaws.com` and the like).
pub fn is_documentdb_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let address = rest.split(['?', '#']).next().unwrap_or_default();
    let authority = address.split('/').next().unwrap_or_default();
    let hosts = authority.rsplit_once('@').map_or(authority, |(_, hosts)| hosts);

    hosts.split(',').any(is_documentdb_host)
}

/// True for a DocumentDB endpoint name, with or without a port.
fn is_documentdb_host(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => name,
        _ => host.as_str(),
    };
    let name = name.trim_end_matches('.');

    DOCUMENTDB_DOMAINS.iter().any(|domain| name.ends_with(domain))
}

/// Applies DocumentDB's rules to the parsed URL: retryable writes off unless the URL set them,
/// reads from the primary only, and password logins against `admin` unless the URL sets
/// `authSource`.
pub(super) fn prepare(options: &mut ClientOptions, url: &str) {
    options.retry_writes.get_or_insert(false);
    options.selection_criteria = Some(SelectionCriteria::ReadPreference(ReadPreference::Primary));

    // Amazon DocumentDB keeps password users in `admin`. Without `authSource` the driver would
    // log in to the database named in the URL path, so a URL with a path would fail to log in.
    if let Some(credential) = options.credential.as_mut()
        && !names_auth_source(url)
        && !url.starts_with("mongodb+srv://")
        && matches!(
            credential.mechanism,
            None | Some(AuthMechanism::ScramSha1 | AuthMechanism::ScramSha256)
        )
    {
        credential.source = Some("admin".to_owned());
    }
}

/// True when the query of `url` sets `authSource` (any case). The driver fills the source in from
/// the URL path otherwise, so the parsed options cannot tell.
fn names_auth_source(url: &str) -> bool {
    url.split_once('?').is_some_and(|(_, query)| {
        query
            .split('&')
            .filter_map(|pair| pair.split_once('=').map(|(key, _)| key))
            .any(|key| key.eq_ignore_ascii_case("authsource"))
    })
}

/// True when the file name of `path` is one AWS gives the bundle (any case).
pub(super) fn is_aws_bundle_name(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| AWS_BUNDLE_NAMES.iter().any(|known| name.eq_ignore_ascii_case(known)))
}

/// [`BUNDLE`] in a new file for one connect: created under a random name in the temp folder
/// (never an existing file or a link, 0600 on Unix), and removed when the returned file is
/// dropped. The driver reads it once, while `Client::with_options` builds its TLS settings, so
/// the caller keeps it until that returns.
pub(super) fn write_bundle() -> Result<NamedTempFile, StoreError> {
    let failed = |error: std::io::Error| {
        StoreError::Backend(format!(
            "cannot write the Amazon certificate bundle to the temp folder: {}",
            error.kind()
        ))
    };

    let mut file = tempfile::Builder::new()
        .prefix("harness-memory-aws-rds-")
        .suffix(".pem")
        .tempfile()
        .map_err(failed)?;
    file.write_all(BUNDLE.as_bytes()).map_err(failed)?;
    file.flush().map_err(failed)?;

    Ok(file)
}

/// The path the driver is given for `bundle`. On Linux this is the open file itself
/// (`/proc/self/fd/N`), so renaming or replacing the name in a shared temp folder between writing
/// and reading cannot hand the driver another file. Elsewhere the temp folder is per user.
pub(super) fn bundle_path(bundle: &NamedTempFile) -> std::path::PathBuf {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        let own = std::path::PathBuf::from(format!("/proc/self/fd/{}", bundle.as_file().as_raw_fd()));
        if own.exists() {
            return own;
        }
    }

    bundle.path().to_owned()
}

/// A driver error on Amazon DocumentDB as a store error, at connect and later: a server that
/// cannot be reached gets [`VPN_HINT`], or [`CERTIFICATE_HINT`] when the TLS handshake refused
/// its certificate.
pub(super) fn backend(error: &Error) -> StoreError {
    let message = super::connection::message(error);

    let unreachable = matches!(
        error.kind.as_ref(),
        ErrorKind::ServerSelection { .. } | ErrorKind::Io(_)
    );

    let hint = if certificate_refused(error) {
        CERTIFICATE_HINT
    } else if unreachable {
        VPN_HINT
    } else {
        return StoreError::Backend(message);
    };

    StoreError::Backend(format!("{message}. {hint}"))
}

/// True when the TLS handshake refused the server's certificate: the error, or the last error of
/// a server the driver could not select, is rustls's `invalid peer certificate`. The text is only
/// looked at, never shown.
fn certificate_refused(error: &Error) -> bool {
    const REFUSED: &str = "invalid peer certificate";

    match error.kind.as_ref() {
        ErrorKind::ServerSelection { message, .. } => message.contains(REFUSED),
        ErrorKind::Io(io) => io.to_string().contains(REFUSED),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documentdb_endpoints_are_recognised_by_their_domain() {
        for url in [
            "mongodb://u:p@docs.cluster-abc123.eu-central-1.docdb.amazonaws.com:27017/?tls=true",
            "mongodb://DOCS.cluster-abc.eu-central-1.DocDB.AmazonAWS.com/memory",
            "mongodb://a.example.com,docs.cluster-ro-abc.us-east-1.docdb.amazonaws.com:27017/",
            "mongodb+srv://u:p@docs-123456789012.us-east-1.docdb-elastic.amazonaws.com/",
            "mongodb://docs.cluster-abc.cn-north-1.docdb.amazonaws.com.cn:27017/",
            "mongodb://docs.abc.cn-north-1.docdb-elastic.amazonaws.com.cn/",
            "mongodb://docs.cluster-abc.eu-central-1.docdb.amazonaws.com./",
        ] {
            assert!(is_documentdb_url(url), "{url}");
        }

        for url in [
            "mongodb://localhost:27017/?directConnection=true",
            "mongodb+srv://cluster0.abcde.mongodb.net/",
            "mongodb://docdb.amazonaws.com.example.net/",
            "mongodb://u:p@db.example.com/?x=.docdb.amazonaws.com",
            "mongodb://docs.docdb.amazonaws.com@db.example.com/",
            "mongodb://rds.eu-central-1.amazonaws.com/",
        ] {
            assert!(!is_documentdb_url(url), "{url}");
        }
    }

    #[test]
    fn the_built_in_bundle_holds_the_amazon_rds_authorities() {
        assert!(BUNDLE.contains("-----BEGIN CERTIFICATE-----"));
        assert!(BUNDLE.matches("-----BEGIN CERTIFICATE-----").count() > 10);
    }

    fn parse(url: &str) -> ClientOptions {
        ClientOptions::parse(url)
            .run()
            .unwrap_or_else(|error| panic!("{url}: {error:?}"))
    }

    #[test]
    fn the_aws_console_string_works_untouched() {
        let url = "mongodb://user:pw@docs.cluster-abc.eu-central-1.docdb.amazonaws.com:27017/?tls=true&\
                   tlsCAFile=global-bundle.pem&replicaSet=rs0&readPreference=secondaryPreferred&retryWrites=false";

        let mut options = parse(url);
        prepare(&mut options, url);
        let bundle = super::super::connection::tls_files(&mut options, true)
            .expect("the built-in bundle")
            .expect("a bundle file");

        let Some(mongodb::options::Tls::Enabled(tls)) = &options.tls else {
            panic!("TLS is on: {:?}", options.tls);
        };
        assert_eq!(tls.ca_file_path, Some(bundle_path(&bundle)));
        assert_eq!(std::fs::read_to_string(bundle.path()).expect("written"), BUNDLE);
        assert_eq!(
            std::fs::read_to_string(bundle_path(&bundle)).expect("readable by the path the driver gets"),
            BUNDLE
        );
        assert_eq!(options.retry_writes, Some(false));

        // The driver reads the bundle when it builds its TLS settings (no connection yet).
        mongodb::sync::Client::with_options(options).expect("the driver takes the bundle");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(bundle.path()).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }

        let path = bundle.path().to_owned();
        drop(bundle);
        assert!(!path.exists(), "the bundle file is removed once the client is built");
    }

    #[test]
    fn every_connect_gets_a_bundle_file_of_its_own() {
        let first = write_bundle().expect("first");
        let second = write_bundle().expect("second");

        assert_ne!(first.path(), second.path());
        assert_eq!(std::fs::read_to_string(second.path()).expect("written"), BUNDLE);
    }

    #[test]
    fn reads_go_to_the_primary_whatever_the_url_prefers() {
        let console = "mongodb://user:pw@docs.cluster-abc.eu-central-1.docdb.amazonaws.com:27017/?tls=true&\
                       tlsCAFile=global-bundle.pem&replicaSet=rs0&readPreference=secondaryPreferred&retryWrites=false";

        for url in [
            console,
            "mongodb://docs.cluster-abc.eu-central-1.docdb.amazonaws.com/?readPreference=secondary",
            "mongodb://docs.cluster-abc.eu-central-1.docdb.amazonaws.com/?readPreference=nearest",
            "mongodb://docs.cluster-abc.eu-central-1.docdb.amazonaws.com/",
        ] {
            let mut options = parse(url);
            prepare(&mut options, url);

            assert!(
                matches!(
                    options.selection_criteria,
                    Some(SelectionCriteria::ReadPreference(ReadPreference::Primary))
                ),
                "{url}: {:?}",
                options.selection_criteria
            );
        }
    }

    #[test]
    fn password_logins_go_to_admin_unless_the_url_says_otherwise() {
        let source = |url: &str| {
            let mut options = parse(url);
            prepare(&mut options, url);
            options.credential.and_then(|credential| credential.source)
        };
        let host = "docs.cluster-abc.eu-central-1.docdb.amazonaws.com:27017";

        assert_eq!(
            source(&format!("mongodb://u:p@{host}/harness_memory?tls=true")).as_deref(),
            Some("admin")
        );
        assert_eq!(
            source(&format!("mongodb://u:p@{host}/?tls=true")).as_deref(),
            Some("admin")
        );
        assert_eq!(
            source(&format!("mongodb://u:p@{host}/harness_memory?authSource=team")).as_deref(),
            Some("team")
        );
        assert_eq!(
            source(&format!("mongodb://u:p@{host}/m?authMechanism=SCRAM-SHA-1")).as_deref(),
            Some("admin")
        );
        // No user, nothing to log in.
        assert_eq!(source(&format!("mongodb://{host}/m")), None);
    }

    #[test]
    fn retryable_writes_are_off_unless_the_url_sets_them() {
        let unset_url = "mongodb://docs.cluster-abc.eu-central-1.docdb.amazonaws.com/";
        let mut unset = parse(unset_url);
        prepare(&mut unset, unset_url);
        assert_eq!(unset.retry_writes, Some(false));

        let explicit_url = "mongodb://docs.cluster-abc.eu-central-1.docdb.amazonaws.com/?retryWrites=true";
        let mut explicit = parse(explicit_url);
        prepare(&mut explicit, explicit_url);
        assert_eq!(explicit.retry_writes, Some(true));
    }

    #[test]
    fn a_refused_certificate_gets_its_own_hint_and_a_lost_network_the_vpn_hint() {
        let refused = Error::from(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid peer certificate: UnknownIssuer",
        ));
        let message = backend(&refused).to_string();
        assert!(message.ends_with(CERTIFICATE_HINT), "{message}");
        assert!(!message.contains("VPN") && !message.contains(';'), "{message}");

        let reset = Error::from(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        let message = backend(&reset).to_string();
        assert!(message.ends_with(VPN_HINT), "{message}");
        assert!(!message.contains(';'), "{message}");

        for hint in [VPN_HINT, CERTIFICATE_HINT, NO_TRANSACTIONS_WARNING] {
            assert!(!hint.contains(';'), "{hint}");
        }
    }
}
