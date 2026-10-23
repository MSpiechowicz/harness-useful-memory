//! The model files on disk: where they live, whether a folder holds a model, and the download.
//!
//! A model lives in `<data>/harness-memory/models/<id>/`. Each file is downloaded to a temporary
//! file in that folder, checked against the manifest's size and SHA-256, and renamed into place;
//! a partial or wrong file is deleted. Only after every file is in place does `installed.json`
//! appear, which is what makes the folder a model.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use ureq::Agent;
use ureq::tls::{Certificate, RootCerts, TlsConfig, TlsProvider};

use super::manifest::{Manifest, ModelFile, Spec};
use super::{Event, Events};
use crate::support::private::{private_folder, read_private, write_atomic};

/// The marker file that says a folder holds a complete model.
const INSTALLED: &str = "installed.json";

/// The limit on how long the whole download may take.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// The limit on connecting.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// How many bytes pass between two `download` events.
const PROGRESS_STEP: u64 = 2 * 1024 * 1024;

/// The folder of the models inside the data folder.
pub fn models_folder(root: &Path) -> PathBuf {
    root.join("models")
}

/// The folder of model `id`.
pub fn model_dir(root: &Path, id: &str) -> PathBuf {
    models_folder(root).join(id)
}

/// The recorded spec of the model in `dir`; `None` when it holds no complete model.
pub fn installed(dir: &Path) -> Option<Spec> {
    let bytes = read_private(&dir.join(INSTALLED), 16 * 1024).ok().flatten()?;
    serde_json::from_slice(&bytes).ok()
}

/// Whether `dir` holds the complete model `manifest` names: every file at its size, and the marker.
pub fn is_present(dir: &Path, manifest: &Manifest) -> bool {
    let marked = installed(dir).is_some_and(|spec| spec.id == manifest.id);

    marked
        && manifest.files.iter().all(|file| {
            std::fs::metadata(dir.join(file.name))
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() == file.size)
        })
}

/// Deletes the folder of model `id`.
pub fn remove(root: &Path, id: &str) -> std::io::Result<()> {
    match std::fs::remove_dir_all(model_dir(root, id)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// The folders under the models folder, by model id.
pub fn folders(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(models_folder(root)) else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect()
}

/// Why a model could not be fetched. The messages name neither the URL's query nor any secret.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("{0}")]
    Refused(String),
    #[error("downloading {name} failed: {reason}")]
    Download { name: String, reason: String },
    #[error("{name} does not match the pinned checksum ({what}); it was not kept")]
    Mismatch { name: String, what: &'static str },
    #[error("writing the model failed: {0}")]
    Write(String),
}

/// Makes sure the model `manifest` names is in its folder, downloading what is missing (all of
/// it: a folder without the marker is not trusted). `base` replaces the base of every URL (see
/// [`check_base`]). Emits `download` while it runs and `verifying` before each check.
pub fn ensure(
    root: &Path,
    manifest: &Manifest,
    base: Option<&str>,
    events: &dyn Events,
    stopped: &dyn Fn() -> bool,
) -> Result<PathBuf, ModelError> {
    let dir = model_dir(root, manifest.id);

    if is_present(&dir, manifest) {
        return Ok(dir);
    }

    if let Some(base) = base {
        check_base(base)?;
    }

    private_folder(&dir, "model folder").map_err(ModelError::Write)?;
    let agent = agent(base);
    let total = manifest.total_size();
    let mut received = 0;

    events.emit(Event::Download { received, total });

    for file in manifest.files {
        // A file is renamed into place only after its checks, so one that is there is good.
        let there = std::fs::metadata(dir.join(file.name)).is_ok_and(|metadata| metadata.len() == file.size);

        if there {
            received += file.size;
            continue;
        }

        fetch(&agent, &dir, file, base, &mut received, total, events, stopped)?;
    }

    let marker = serde_json::to_vec(&manifest.spec()).map_err(|error| ModelError::Write(error.to_string()))?;
    write_atomic(&dir, &dir.join(INSTALLED), &marker, "installed", "model marker").map_err(ModelError::Write)?;

    Ok(dir)
}

/// Accepts `https://` and `http://127.0.0.1` or `http://localhost` (for tests and mirrors on this
/// computer); nothing else may serve the model.
pub fn check_base(base: &str) -> Result<(), ModelError> {
    let lower = base.trim().to_ascii_lowercase();

    if lower.starts_with("https://") {
        return Ok(());
    }

    let local = ["http://127.0.0.1", "http://localhost"].iter().any(|prefix| {
        lower
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '/']))
    });

    if local {
        return Ok(());
    }

    Err(ModelError::Refused(
        "the model base URL must be https:// (or http:// to localhost)".to_owned(),
    ))
}

/// The HTTP client: TLS with the ring provider and the platform's roots, https only unless the
/// base is a local mirror (which then gets no redirects either).
fn agent(base: Option<&str>) -> Agent {
    let local = base.is_some_and(|base| base.trim().to_ascii_lowercase().starts_with("http://"));

    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .root_certs(platform_roots())
        .build();

    Agent::config_builder()
        .tls_config(tls)
        .https_only(!local)
        .max_redirects(if local { 0 } else { 5 })
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .proxy(None)
        .user_agent(concat!("harness-memory/", env!("HARNESS_MEMORY_VERSION")))
        .build()
        .new_agent()
}

fn platform_roots() -> RootCerts {
    let loaded = rustls_native_certs::load_native_certs();
    let certs: Vec<Certificate<'static>> = loaded
        .certs
        .iter()
        .map(|cert| Certificate::from_der(cert.as_ref()).to_owned())
        .collect();

    RootCerts::new_with_certs(&certs)
}

/// Downloads `file` to a temporary file in `dir`, checks it and renames it into place. A failure
/// deletes the temporary file.
#[allow(clippy::too_many_arguments)]
fn fetch(
    agent: &Agent,
    dir: &Path,
    file: &ModelFile,
    base: Option<&str>,
    received: &mut u64,
    total: u64,
    events: &dyn Events,
    stopped: &dyn Fn() -> bool,
) -> Result<(), ModelError> {
    let name = file.name.to_owned();
    let failed = |reason: String| ModelError::Download {
        name: name.clone(),
        reason,
    };

    let response = agent
        .get(file.url_under(base))
        .call()
        .map_err(|error| failed(describe(&error)))?;

    let mut temporary = tempfile::Builder::new()
        .prefix(&format!("{}.", file.name))
        .suffix(".part")
        .tempfile_in(dir)
        .map_err(|error| ModelError::Write(error.to_string()))?;

    let mut body = response.into_body().into_reader().take(file.size + 1);
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut reported = *received;
    let mut chunk = vec![0_u8; 64 * 1024];

    loop {
        if stopped() {
            return Err(failed("stopped".to_owned()));
        }

        let read = body
            .read(&mut chunk)
            .map_err(|error| failed(error.kind().to_string()))?;

        if read == 0 {
            break;
        }

        hasher.update(&chunk[..read]);
        temporary
            .write_all(&chunk[..read])
            .map_err(|error| ModelError::Write(error.to_string()))?;
        size += read as u64;
        *received += read as u64;

        if *received - reported >= PROGRESS_STEP {
            reported = *received;
            events.emit(Event::Download {
                received: *received,
                total,
            });
        }

        if size > file.size {
            break;
        }
    }

    events.emit(Event::Download {
        received: *received,
        total,
    });
    events.emit(Event::Verifying);

    if size != file.size {
        return Err(ModelError::Mismatch { name, what: "size" });
    }

    let digest: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();

    if digest != file.sha256 {
        return Err(ModelError::Mismatch { name, what: "SHA-256" });
    }

    temporary
        .flush()
        .map_err(|error| ModelError::Write(error.to_string()))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| ModelError::Write(error.to_string()))?;
    temporary
        .persist(dir.join(file.name))
        .map_err(|error| ModelError::Write(error.error.to_string()))?;

    Ok(())
}

/// A transport failure described without the URL.
fn describe(error: &ureq::Error) -> String {
    match error {
        ureq::Error::StatusCode(code) => format!("the server answered {code}"),
        ureq::Error::Timeout(_) => "the server did not answer in time".to_owned(),
        ureq::Error::HostNotFound => "the host was not found".to_owned(),
        ureq::Error::ConnectionFailed => "cannot connect".to_owned(),
        ureq::Error::BodyExceedsLimit(_) => "the reply is too large".to_owned(),
        ureq::Error::Rustls(_) | ureq::Error::Tls(_) => "the TLS connection failed".to_owned(),
        _ => "the request failed".to_owned(),
    }
}
