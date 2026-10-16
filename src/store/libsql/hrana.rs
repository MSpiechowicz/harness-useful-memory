//! A small blocking client for the libSQL server's HTTP pipeline (`POST /v2/pipeline`, Hrana over
//! HTTP).
//!
//! Each request carries a list of stream requests. A [`Stream`] that keeps its baton stays on one
//! server connection across requests, which is what an interactive transaction needs; a one-shot
//! stream closes in the same request, so every statement runs on its own. Error messages carry the
//! server's error code (only when it looks like one, `[A-Z_]{1,40}`) or the HTTP status: never the
//! URL, the token, a statement, stored text or a message the server wrote.
//!
//! Proxy variables (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`) are ignored: requests always go
//! straight to the server, so the token never passes through a proxy, in clear for `http://`.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use ureq::Agent;
use ureq::Timeout;
use ureq::tls::{Certificate, RootCerts, TlsConfig, TlsProvider};

use super::endpoint;
use crate::store::StoreError;
use crate::store::sqlite::sql::{Executed, Row, Sql, Value};

/// How long one HTTP request may take, waiting for the write lock included (the server queues a
/// writer for about 5 s before it takes the lock from an idle transaction).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest reply read; every query is bounded far below it.
const REPLY_MAX: u64 = 16 * 1024 * 1024;

/// The HTTP side: where requests go and the credentials they carry.
pub(super) struct Client {
    agent: Agent,
    base: String,
    authorization: Option<String>,
}

impl Client {
    /// A client for the server at `base` ([`endpoint::parse`]d). TLS uses the ring provider and
    /// the platform's trusted roots; redirects are not followed and proxy variables are ignored,
    /// so the token goes to the configured host only.
    pub(super) fn new(base: String, token: Option<&str>) -> Result<Self, StoreError> {
        let authorization = match token.map(str::trim).filter(|token| !token.is_empty()) {
            Some(token) if token.bytes().all(|byte| byte.is_ascii_graphic()) => Some(format!("Bearer {token}")),
            Some(_) => {
                return Err(StoreError::Invalid(
                    "the libSQL token must be printable ASCII".to_owned(),
                ));
            }
            None => None,
        };

        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .root_certs(platform_roots())
            .build();

        let agent = Agent::config_builder()
            .tls_config(tls)
            .timeout_global(Some(REQUEST_TIMEOUT))
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .user_agent(concat!("harness-memory/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();

        Ok(Self {
            agent,
            base,
            authorization,
        })
    }

    /// A stream where every statement runs on its own.
    pub(super) fn single(&self) -> Stream<'_> {
        Stream {
            client: self,
            keep: false,
            baton: None,
            base: None,
            closed: false,
        }
    }

    /// A stream that stays on one server connection until [`Stream::finish`].
    pub(super) fn stream(&self) -> Stream<'_> {
        Stream {
            keep: true,
            ..self.single()
        }
    }

    /// The base URL a stream's later requests use when the server names one (`base_url`): only
    /// where [`endpoint::may_follow`] allows, so the token goes nowhere else.
    fn follow(&self, url: &str) -> Result<String, Failed> {
        let moved = || {
            Failed::answered(backend(
                "the libSQL server sent the stream to another address".to_owned(),
            ))
        };
        let base = endpoint::parse(url).map_err(|_| moved())?;

        if !endpoint::may_follow(&self.base, &base) {
            return Err(moved());
        }

        Ok(base)
    }

    fn post(&self, base: &str, baton: Option<&str>, requests: Vec<serde_json::Value>) -> Result<Reply, Failed> {
        let body = json!({ "baton": baton, "requests": requests }).to_string();
        let mut request = self
            .agent
            .post(format!("{base}/v2/pipeline"))
            .header("content-type", "application/json");

        if let Some(authorization) = &self.authorization {
            request = request.header("authorization", authorization);
        }

        let mut response = request.send(body).map_err(Failed::unanswered)?;
        let status = response.status().as_u16();

        let refused = match status {
            200 => None,
            401 | 403 => Some(format!("the libSQL server refused the token (HTTP {status})")),
            404 => Some("no libSQL pipeline at this URL (HTTP 404)".to_owned()),
            _ => Some(format!("the libSQL server answered HTTP {status}")),
        };

        if let Some(message) = refused {
            // A gateway can answer 5xx after the server ran the request.
            return Err(Failed {
                error: backend(message),
                maybe_ran: status >= 500,
            });
        }

        let bytes = response
            .body_mut()
            .with_config()
            .limit(REPLY_MAX)
            .read_to_vec()
            .map_err(Failed::unanswered)?;

        serde_json::from_slice(&bytes).map_err(|_| Failed {
            error: unexpected(),
            maybe_ran: true,
        })
    }
}

/// A request that got no usable reply.
struct Failed {
    error: StoreError,
    /// The server may have run the request: it may have arrived, but no reply was read.
    maybe_ran: bool,
}

impl Failed {
    /// A failure decided before the request could run.
    fn answered(error: StoreError) -> Self {
        Self {
            error,
            maybe_ran: false,
        }
    }

    /// A transport failure: one before the connection opened cannot have run the request.
    fn unanswered(error: ureq::Error) -> Self {
        let before_sending = matches!(
            error,
            ureq::Error::HostNotFound
                | ureq::Error::ConnectionFailed
                | ureq::Error::Timeout(Timeout::Resolve | Timeout::Connect)
        );

        Self {
            error: transport(error),
            maybe_ran: !before_sending,
        }
    }
}

/// One Hrana stream. See the module docs.
pub(super) struct Stream<'c> {
    client: &'c Client,
    /// Keep the stream open between requests.
    keep: bool,
    baton: Option<String>,
    /// The URL the server asked this stream's later requests to use.
    base: Option<String>,
    /// The server ended a kept stream; its transaction is gone.
    closed: bool,
}

impl Stream<'_> {
    /// Sends `requests` (plus a close for a one-shot stream); one outcome per request.
    pub(super) fn send(&mut self, requests: Vec<serde_json::Value>) -> Result<Vec<Outcome>, StoreError> {
        self.exchange(requests).map_err(|failed| failed.error)
    }

    /// [`Self::send`], telling whether a failed request may have run.
    fn exchange(&mut self, mut requests: Vec<serde_json::Value>) -> Result<Vec<Outcome>, Failed> {
        if self.closed {
            return Err(Failed::answered(ended()));
        }

        let count = requests.len();
        if !self.keep {
            requests.push(json!({ "type": "close" }));
        }

        let base = self.base.as_deref().unwrap_or(&self.client.base);
        let reply = self.client.post(base, self.baton.as_deref(), requests);
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                self.closed = self.keep;
                return Err(error);
            }
        };

        // Only a kept stream sends more requests.
        if let Some(url) = reply.base_url.filter(|_| self.keep) {
            self.base = Some(self.client.follow(&url)?);
        }

        self.closed = self.keep && reply.baton.is_none();
        self.baton = reply.baton.filter(|_| self.keep);

        let mut results = reply.results;
        if results.len() < count {
            return Err(Failed {
                error: unexpected(),
                maybe_ran: true,
            });
        }

        results.truncate(count);
        Ok(results)
    }

    /// Runs one statement and returns its result.
    fn statement(&mut self, sql: &str, args: &[Value], want_rows: bool) -> Result<StatementResult, StoreError> {
        let request = json!({
            "type": "execute",
            "stmt": { "sql": sql, "args": args.iter().map(value_json).collect::<Vec<_>>(), "want_rows": want_rows },
        });

        match self.send(vec![request])?.remove(0).into_result()? {
            Response::Execute { result } => Ok(result),
            Response::Other => Err(unexpected()),
        }
    }

    /// Commits a kept stream's transaction and closes the stream in the same request. When the
    /// request may have reached the server but no answer came back, the commit may have run: the
    /// error says so, and nothing sends the write again.
    pub(super) fn commit(self) -> Result<(), StoreError> {
        match self.finish("COMMIT") {
            Ok(()) => Ok(()),
            Err(Failed {
                error: StoreError::Backend(reason),
                maybe_ran: true,
            }) => Err(backend(format!(
                "the libSQL server did not confirm the commit ({reason}), so the write may have been \
                 applied: read it back before repeating it"
            ))),
            Err(failed) => Err(failed.error),
        }
    }

    /// Rolls back a kept stream's transaction and closes the stream in the same request.
    pub(super) fn rollback(self) -> Result<(), StoreError> {
        self.finish("ROLLBACK").map_err(|failed| failed.error)
    }

    /// Ends a kept stream with `sql` and closes it in the same request.
    fn finish(mut self, sql: &str) -> Result<(), Failed> {
        if self.baton.is_none() {
            return Err(Failed::answered(ended()));
        }

        self.keep = false;
        let outcome = self.exchange(vec![execute(sql)])?.remove(0);
        outcome.into_result().map(drop).map_err(Failed::answered)
    }

    /// Closes a kept stream (the server rolls back an open transaction); errors are ignored, as
    /// the server also drops a stream nobody uses.
    pub(super) fn close(mut self) {
        if self.baton.is_some() {
            self.keep = false;
            self.send(Vec::new()).ok();
        }
    }
}

impl Sql for Stream<'_> {
    fn query(&mut self, sql: &str, args: &[Value]) -> Result<Vec<Row>, StoreError> {
        self.statement(sql, args, true)?
            .rows
            .into_iter()
            .map(|cells| {
                cells
                    .into_iter()
                    .map(Cell::into_value)
                    .collect::<Result<_, _>>()
                    .map(Row::new)
            })
            .collect()
    }

    fn execute(&mut self, sql: &str, args: &[Value]) -> Result<Executed, StoreError> {
        let result = self.statement(sql, args, false)?;

        Ok(Executed {
            changes: usize::try_from(result.affected_row_count).unwrap_or(usize::MAX),
            last_insert_rowid: result.last_insert_rowid.and_then(|id| id.parse().ok()).unwrap_or(0),
        })
    }

    fn batch(&mut self, sql: &str) -> Result<(), StoreError> {
        let outcome = self.send(vec![json!({ "type": "sequence", "sql": sql })])?.remove(0);
        outcome.into_result().map(drop)
    }
}

/// An `execute` request for `sql` without arguments.
pub(super) fn execute(sql: &str) -> serde_json::Value {
    json!({ "type": "execute", "stmt": { "sql": sql, "want_rows": false } })
}

#[derive(Deserialize)]
struct Reply {
    baton: Option<String>,
    base_url: Option<String>,
    results: Vec<Outcome>,
}

/// The outcome of one stream request.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Outcome {
    Ok { response: Response },
    Error { error: Failure },
}

impl Outcome {
    /// The response, or the server's error as a [`StoreError`].
    pub(super) fn into_result(self) -> Result<Response, StoreError> {
        match self {
            Self::Ok { response } => Ok(response),
            Self::Error { error } => Err(error.into_store_error()),
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Response {
    Execute {
        result: StatementResult,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
pub(super) struct StatementResult {
    #[serde(default)]
    rows: Vec<Vec<Cell>>,
    #[serde(default)]
    affected_row_count: u64,
    last_insert_rowid: Option<String>,
}

/// An error the server reports for one request; its message may quote the statement, so only the
/// code is kept.
#[derive(Deserialize)]
pub(super) struct Failure {
    code: Option<String>,
}

impl Failure {
    /// True when another writer held the lock.
    pub(super) fn is_busy(&self) -> bool {
        self.code().is_some_and(|code| code.contains("BUSY"))
    }

    /// The server's error code when it looks like one (`[A-Z_]{1,40}`); anything else may be
    /// free text, which never reaches a message.
    pub(super) fn code(&self) -> Option<&str> {
        self.code.as_deref().filter(|code| {
            (1..=40).contains(&code.len()) && code.bytes().all(|byte| byte.is_ascii_uppercase() || byte == b'_')
        })
    }

    pub(super) fn into_store_error(self) -> StoreError {
        backend(self.code().unwrap_or("statement failed").to_owned())
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Cell {
    Null,
    Integer { value: String },
    Float { value: f64 },
    Text { value: String },
    Blob {},
}

impl Cell {
    fn into_value(self) -> Result<Value, StoreError> {
        match self {
            Self::Null => Ok(Value::Null),
            Self::Integer { value } => value.parse().map(Value::Integer).map_err(|_| unexpected()),
            Self::Float { value } => Ok(Value::Real(value)),
            Self::Text { value } => Ok(Value::Text(value)),
            Self::Blob {} => Err(backend("unexpected blob value".to_owned())),
        }
    }
}

fn value_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => json!({ "type": "null" }),
        Value::Integer(number) => json!({ "type": "integer", "value": number.to_string() }),
        Value::Real(number) => json!({ "type": "float", "value": number }),
        Value::Text(text) => json!({ "type": "text", "value": text }),
    }
}

/// The platform's trusted roots (an empty set when none load: every TLS connection then fails
/// with a certificate error instead of trusting anything).
fn platform_roots() -> RootCerts {
    let loaded = rustls_native_certs::load_native_certs();
    let certs: Vec<Certificate<'static>> = loaded
        .certs
        .iter()
        .map(|cert| Certificate::from_der(cert.as_ref()).to_owned())
        .collect();

    RootCerts::new_with_certs(&certs)
}

/// A transport failure, described without the URL.
fn transport(error: ureq::Error) -> StoreError {
    let reason = match error {
        ureq::Error::Timeout(_) => "the libSQL server did not answer in time".to_owned(),
        ureq::Error::HostNotFound => "the libSQL host was not found".to_owned(),
        ureq::Error::ConnectionFailed => "cannot connect to the libSQL server".to_owned(),
        ureq::Error::Io(error) => match error.get_ref().and_then(|inner| inner.downcast_ref::<rustls::Error>()) {
            Some(tls) => format!("TLS with the libSQL server failed ({tls})"),
            None => format!("cannot reach the libSQL server ({})", error.kind()),
        },
        ureq::Error::Rustls(error) => format!("TLS with the libSQL server failed ({error})"),
        ureq::Error::Tls(reason) => format!("TLS with the libSQL server failed ({reason})"),
        ureq::Error::BodyExceedsLimit(_) => "the libSQL server's reply is too large".to_owned(),
        _ => "the request to the libSQL server failed".to_owned(),
    };

    backend(reason)
}

fn backend(message: String) -> StoreError {
    StoreError::Backend(message)
}

fn unexpected() -> StoreError {
    backend("unexpected reply from the libSQL server".to_owned())
}

fn ended() -> StoreError {
    backend("the libSQL server ended the transaction".to_owned())
}
