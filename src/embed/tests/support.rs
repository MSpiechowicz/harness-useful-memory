//! The fakes and fixtures the embedder tests share.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::embed::manifest::{Manifest, ModelFile, ONNX_FILE, Pooling, TOKENIZER_FILE};
use crate::embed::protocol::{Reply, Request, Target};
use crate::embed::{Embed, Event, Events, Loader, Options, Service, Source, locked, model};
use crate::store::{Forget, MemoryStore, NoteDraft, NoteKind, NotePatch, ProjectKey, Provenance, SqliteStore};
use crate::store::{Target as WriteTarget, View};

pub const ONNX_BYTES: &[u8] = b"fake onnx graph bytes";
pub const TOKENIZER_BYTES: &[u8] = b"{\"fake\":\"tokenizer\"}";

/// A manifest of two tiny files; every id has the same bytes.
pub fn fake_manifest(id: &str) -> Manifest {
    let file = |name: &'static str, bytes: &[u8]| ModelFile {
        name,
        url: Box::leak(format!("https://invalid.example/{name}").into_boxed_str()),
        sha256: Box::leak(hex(bytes).into_boxed_str()),
        size: bytes.len() as u64,
    };

    let files: &'static [ModelFile] = Box::leak(Box::new([
        file(ONNX_FILE, ONNX_BYTES),
        file(TOKENIZER_FILE, TOKENIZER_BYTES),
    ]));

    Manifest {
        id: Box::leak(id.to_owned().into_boxed_str()),
        files,
        dims: 384,
        max_tokens: 128,
        pooling: Pooling::Mean,
        normalize: true,
    }
}

pub fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Puts the files of `manifest` in place the way a download does.
pub fn install(root: &Path, manifest: &Manifest) {
    let base = Fixture::serving(manifest);
    let events = Collect::default();

    model::ensure(root, manifest, Some(&base.url()), &events, &|| false).expect("install the fake model");
}

/// Events kept in a list.
#[derive(Default)]
pub struct Collect(Mutex<Vec<Event>>);

impl Events for Collect {
    fn emit(&self, event: Event) {
        locked(&self.0).push(event);
    }
}

impl Collect {
    pub fn all(&self) -> Vec<Event> {
        locked(&self.0).clone()
    }

    /// The names of the events (`ready`, `backfill`, ...), in order.
    pub fn names(&self) -> Vec<String> {
        self.all()
            .iter()
            .map(|event| event.to_json()["event"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    pub fn errors(&self) -> Vec<String> {
        self.all()
            .into_iter()
            .filter_map(|event| match event {
                Event::Error { message } => Some(message),
                _ => None,
            })
            .collect()
    }
}

/// The fake engine: a hashed bag of words, different for every model id.
pub struct Fake {
    pub loads: Arc<AtomicUsize>,
    pub embeds: Arc<AtomicUsize>,
}

impl Fake {
    pub fn new() -> Self {
        Self {
            loads: Arc::new(AtomicUsize::new(0)),
            embeds: Arc::new(AtomicUsize::new(0)),
        }
    }
}

struct FakeEngine {
    model: String,
    embeds: Arc<AtomicUsize>,
}

impl Loader for Fake {
    fn load(&self, _dir: &Path, spec: &crate::embed::manifest::Spec) -> Result<Box<dyn Embed>, String> {
        self.loads.fetch_add(1, Ordering::SeqCst);

        Ok(Box::new(FakeEngine {
            model: spec.id.clone(),
            embeds: Arc::clone(&self.embeds),
        }))
    }
}

impl Embed for FakeEngine {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
        self.embeds.fetch_add(1, Ordering::SeqCst);
        Ok(fake_vector(&self.model, text))
    }
}

pub fn fake_vector(model: &str, text: &str) -> Vec<f32> {
    let mut vector = vec![0_f32; 384];

    for word in text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        let digest = Sha256::digest(format!("{model}|{word}").as_bytes());

        for pair in digest.as_chunks::<2>().0.iter().take(3) {
            vector[(usize::from(pair[0]) * 256 + usize::from(pair[1])) % 384] += 1.0;
        }
    }

    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();

    if norm == 0.0 {
        vector[0] = 1.0;
        return vector;
    }

    vector.iter_mut().for_each(|value| *value /= norm);
    vector
}

/// A local HTTP server that serves the files of a manifest and records what was asked for.
pub struct Fixture {
    port: u16,
    pub requests: Arc<Mutex<Vec<String>>>,
    pub fail: Arc<AtomicBool>,
    pub files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl Fixture {
    pub fn serving(manifest: &Manifest) -> Self {
        let bytes = |name: &str| if name == ONNX_FILE { ONNX_BYTES } else { TOKENIZER_BYTES }.to_vec();
        let files = manifest
            .files
            .iter()
            .map(|file| (format!("/{}", file.name), bytes(file.name)))
            .collect();

        Self::with_files(files)
    }

    pub fn with_files(files: HashMap<String, Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fixture");
        let port = listener.local_addr().expect("address").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(AtomicBool::new(false));
        let files = Arc::new(Mutex::new(files));

        let (seen, failing, served) = (Arc::clone(&requests), Arc::clone(&fail), Arc::clone(&files));

        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (seen, failing, served) = (Arc::clone(&seen), Arc::clone(&failing), Arc::clone(&served));
                std::thread::spawn(move || answer(stream, &seen, &failing, &served));
            }
        });

        Self {
            port,
            requests,
            fail,
            files,
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn requested(&self) -> Vec<String> {
        locked(&self.requests).clone()
    }
}

fn answer(stream: TcpStream, seen: &Mutex<Vec<String>>, failing: &AtomicBool, files: &Mutex<HashMap<String, Vec<u8>>>) {
    let mut reader = BufReader::new(stream);
    let mut request = String::new();

    if reader.read_line(&mut request).is_err() {
        return;
    }

    let mut header = String::new();

    while reader.read_line(&mut header).is_ok_and(|read| read > 2) {
        header.clear();
    }

    let path = request.split_whitespace().nth(1).unwrap_or_default().to_owned();
    locked(seen).push(path.clone());

    let body = locked(files).get(&path).cloned();
    let mut stream = reader.into_inner();

    let reply = match body {
        Some(body) if !failing.load(Ordering::SeqCst) => {
            let mut reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            reply.extend(body);
            reply
        }
        Some(_) => b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
    };

    let _ = stream.write_all(&reply);
    let _ = stream.flush();
}

/// A SQLite database the tests write notes to, and the key a source over it has.
pub struct Notes {
    pub path: PathBuf,
    pub key: String,
    store: SqliteStore,
}

pub const REPO: &str = "git:example.com/team/app";

impl Notes {
    pub fn new(dir: &Path, name: &str) -> Self {
        let path = dir.join(format!("{name}.sqlite3"));

        Self {
            key: format!("local:{name}"),
            store: SqliteStore::open(&path).expect("open the database"),
            path,
        }
    }

    /// A source over this database, with its own connection.
    pub fn source(&self) -> Source {
        let store = SqliteStore::open(&self.path).expect("open the database for the source");
        Source::new(self.key.clone(), Box::new(store))
    }

    pub fn add(&mut self, title: &str, body: &str) -> i64 {
        self.add_to(WriteTarget::Global, title, body)
    }

    pub fn add_to_repo(&mut self, title: &str, body: &str) -> i64 {
        let key = ProjectKey {
            key: REPO.to_owned(),
            root: PathBuf::from("/work/app"),
            remote: None,
        };

        self.add_to(WriteTarget::Repo(key), title, body)
    }

    fn add_to(&mut self, target: WriteTarget, title: &str, body: &str) -> i64 {
        let draft = NoteDraft {
            title: title.to_owned(),
            body: body.to_owned(),
            kind: NoteKind::Note,
            tags: Vec::new(),
            entities: Vec::new(),
            provenance: Provenance::default(),
        };

        self.store.remember(&target, &draft).expect("save the note").id
    }

    pub fn edit(&mut self, id: i64, body: &str) {
        let patch = NotePatch {
            body: Some(body.to_owned()),
            ..NotePatch::default()
        };

        self.store
            .update(&View::every_repo(), id, &patch)
            .expect("edit the note");
    }

    pub fn delete(&mut self, id: i64) {
        self.store
            .forget(&View::every_repo(), &WriteTarget::Global, &Forget::Note(id))
            .expect("delete the note");
    }
}

/// The target for a search of one store.
pub fn target(store: &str, projects: &[Option<&str>]) -> Target {
    Target {
        store: store.to_owned(),
        projects: projects.iter().map(|project| project.map(str::to_owned)).collect(),
    }
}

/// One embedder over a temporary data folder, with its collected events and the fake engine.
pub struct World {
    pub dir: TempDir,
    pub root: PathBuf,
    pub events: Arc<Collect>,
    pub fake: Arc<Fake>,
    pub service: Arc<Service>,
}

impl World {
    /// A service for `manifest` over `sources`, in the data folder `dir`, with the model of
    /// `installed` (if any) already in place. The model is loaded by `prepare`.
    pub fn open(dir: TempDir, manifest: Manifest, base: Option<String>, sources: Vec<Source>) -> Self {
        let root = dir.path().join("data");
        std::fs::create_dir_all(&root).expect("data folder");

        Self::reopen(dir, root, manifest, base, sources)
    }

    /// The same data folder again, as after a restart.
    pub fn reopen(dir: TempDir, root: PathBuf, manifest: Manifest, base: Option<String>, sources: Vec<Source>) -> Self {
        Self::reopen_with(dir, root, manifest, base, sources, |_| {})
    }

    /// [`World::reopen`] with `tweak` applied to the options (shorter timings, for instance).
    pub fn reopen_with(
        dir: TempDir,
        root: PathBuf,
        manifest: Manifest,
        base: Option<String>,
        sources: Vec<Source>,
        tweak: impl FnOnce(&mut Options),
    ) -> Self {
        let events = Arc::new(Collect::default());
        let fake = Arc::new(Fake::new());

        let mut options = Options::new(root.clone(), manifest);
        options.base_url = base;
        tweak(&mut options);

        let service = Service::new(
            options,
            Arc::clone(&fake) as Arc<dyn Loader>,
            Arc::clone(&events) as Arc<dyn Events>,
            sources,
        )
        .expect("open the service");

        Self {
            dir,
            root,
            events,
            fake,
            service: Arc::new(service),
        }
    }

    /// Search through the request handler, as the socket does.
    pub fn ask(&self, text: &str, targets: &[Target]) -> Reply {
        let request = Request::Search {
            text: text.to_owned(),
            targets: targets.to_vec(),
            limit: 20,
            floor: 0.0,
        };

        serde_json::from_value(self.service.handle(request)).expect("a reply")
    }
}

/// The ids of the hits, closest first.
pub fn ids(reply: &Reply) -> Vec<i64> {
    reply.hits.iter().map(|hit| hit.id).collect()
}
