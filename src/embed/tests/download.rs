//! The model download: checked against the pinned size and SHA-256, kept only when right, and
//! fetched only from the manifest's place.

use std::collections::HashMap;

use tempfile::TempDir;

use super::support::{Collect, Fixture, ONNX_BYTES, TOKENIZER_BYTES, fake_manifest};
use crate::embed::manifest::{ONNX_FILE, TOKENIZER_FILE};
use crate::embed::model::{self, ModelError};
use crate::embed::{Event, MODEL};

fn folder_listing(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn a_model_is_downloaded_checked_and_kept() {
    let root = TempDir::new().expect("temp dir");
    let manifest = fake_manifest("fake@1");
    let fixture = Fixture::serving(&manifest);
    let events = Collect::default();

    let dir = model::ensure(root.path(), &manifest, Some(&fixture.url()), &events, &|| false).expect("download");

    assert_eq!(std::fs::read(dir.join(ONNX_FILE)).expect("onnx"), ONNX_BYTES);
    assert_eq!(
        std::fs::read(dir.join(TOKENIZER_FILE)).expect("tokenizer"),
        TOKENIZER_BYTES
    );
    assert!(model::is_present(&dir, &manifest));
    assert_eq!(model::installed(&dir).map(|spec| spec.id), Some("fake@1".to_owned()));

    let names = events.names();
    assert_eq!(names.first().map(String::as_str), Some("download"));
    assert_eq!(names.iter().filter(|name| *name == "verifying").count(), 2);
    assert!(events.all().contains(&Event::Download {
        received: (ONNX_BYTES.len() + TOKENIZER_BYTES.len()) as u64,
        total: (ONNX_BYTES.len() + TOKENIZER_BYTES.len()) as u64,
    }));

    // Present already: no request at all.
    let before = fixture.requested().len();
    model::ensure(root.path(), &manifest, Some(&fixture.url()), &events, &|| false).expect("present");
    assert_eq!(fixture.requested().len(), before);
}

#[test]
fn a_file_with_the_wrong_checksum_is_an_error_and_no_file_stays() {
    let root = TempDir::new().expect("temp dir");
    let manifest = fake_manifest("fake@1");

    // The right size, other bytes.
    let mut tampered = ONNX_BYTES.to_vec();
    tampered[0] ^= 0xff;
    let fixture = Fixture::with_files(HashMap::from([
        (format!("/{ONNX_FILE}"), tampered),
        (format!("/{TOKENIZER_FILE}"), TOKENIZER_BYTES.to_vec()),
    ]));

    let error = model::ensure(
        root.path(),
        &manifest,
        Some(&fixture.url()),
        &Collect::default(),
        &|| false,
    )
    .expect_err("a wrong checksum");

    assert!(matches!(error, ModelError::Mismatch { what: "SHA-256", .. }), "{error}");

    let dir = model::model_dir(root.path(), "fake@1");
    assert_eq!(
        folder_listing(&dir),
        Vec::<String>::new(),
        "no file, no partial file, no marker"
    );
    assert!(!model::is_present(&dir, &manifest));
}

#[test]
fn a_file_of_the_wrong_size_is_an_error_and_no_file_stays() {
    let root = TempDir::new().expect("temp dir");
    let manifest = fake_manifest("fake@1");

    for body in [&ONNX_BYTES[..5], &[ONNX_BYTES, b" and more"].concat()[..]] {
        let fixture = Fixture::with_files(HashMap::from([
            (format!("/{ONNX_FILE}"), body.to_vec()),
            (format!("/{TOKENIZER_FILE}"), TOKENIZER_BYTES.to_vec()),
        ]));

        let error = model::ensure(
            root.path(),
            &manifest,
            Some(&fixture.url()),
            &Collect::default(),
            &|| false,
        )
        .expect_err("a wrong size");

        assert!(matches!(error, ModelError::Mismatch { what: "size", .. }), "{error}");
        assert_eq!(
            folder_listing(&model::model_dir(root.path(), "fake@1")),
            Vec::<String>::new()
        );
    }
}

#[test]
fn a_verified_file_is_not_downloaded_again_after_a_later_failure() {
    let root = TempDir::new().expect("temp dir");
    let manifest = fake_manifest("fake@1");
    let fixture = Fixture::serving(&manifest);
    locked_remove(&fixture, TOKENIZER_FILE);

    model::ensure(
        root.path(),
        &manifest,
        Some(&fixture.url()),
        &Collect::default(),
        &|| false,
    )
    .expect_err("404");

    let dir = model::model_dir(root.path(), "fake@1");
    assert_eq!(
        folder_listing(&dir),
        vec![ONNX_FILE.to_owned()],
        "the checked file stays, nothing else"
    );

    crate::embed::locked(&fixture.files).insert(format!("/{TOKENIZER_FILE}"), TOKENIZER_BYTES.to_vec());
    let before = fixture.requested().len();
    model::ensure(
        root.path(),
        &manifest,
        Some(&fixture.url()),
        &Collect::default(),
        &|| false,
    )
    .expect("second try");

    assert_eq!(fixture.requested()[before..], [format!("/{TOKENIZER_FILE}")]);
    assert!(model::is_present(&dir, &manifest));
}

fn locked_remove(fixture: &Fixture, name: &str) {
    crate::embed::locked(&fixture.files).remove(&format!("/{name}"));
}

#[test]
fn only_the_base_url_serves_the_files() {
    let root = TempDir::new().expect("temp dir");
    let manifest = fake_manifest("fake@1");
    let fixture = Fixture::serving(&manifest);

    model::ensure(
        root.path(),
        &manifest,
        Some(&format!("{}/", fixture.url())),
        &Collect::default(),
        &|| false,
    )
    .expect("download");

    assert_eq!(
        fixture.requested(),
        vec![format!("/{ONNX_FILE}"), format!("/{TOKENIZER_FILE}")]
    );
}

#[test]
fn a_base_url_must_be_https_or_this_computer() {
    for fine in [
        "https://mirror.example.com/models",
        "http://127.0.0.1:8000",
        "http://localhost/x",
        "HTTP://LOCALHOST:1",
    ] {
        assert!(model::check_base(fine).is_ok(), "{fine}");
    }

    for refused in [
        "http://example.com/models",
        "ftp://127.0.0.1/",
        "http://127.0.0.1.evil.example/",
        "http://localhost@evil.example/",
        "file:///tmp/models",
        "mirror.example.com",
    ] {
        assert!(model::check_base(refused).is_err(), "{refused}");
    }
}

#[test]
fn the_compiled_in_files_are_the_pinned_release_assets() {
    let urls: Vec<&str> = MODEL.files.iter().map(|file| file.url).collect();

    assert_eq!(
        urls,
        vec![
            "https://github.com/MSpiechowicz/harness-useful-memory/releases/download/model-minilm-l12-int8-1/model_quantized.onnx",
            "https://github.com/MSpiechowicz/harness-useful-memory/releases/download/model-minilm-l12-int8-1/tokenizer.json",
        ]
    );
    assert_eq!(MODEL.id, "paraphrase-multilingual-minilm-l12-v2-int8@1");
}

#[test]
fn partial_files_of_an_interrupted_download_are_removed_when_the_next_one_starts() {
    let root = TempDir::new().expect("temp dir");
    let manifest = fake_manifest("fake@1");
    let fixture = Fixture::serving(&manifest);

    let dir = model::model_dir(root.path(), manifest.id);
    std::fs::create_dir_all(&dir).expect("model folder");
    std::fs::write(dir.join(format!("{ONNX_FILE}.aB3dE9.part")), b"half a model").expect("stale part");
    std::fs::write(dir.join("notes.txt"), b"not ours to touch").expect("other file");

    model::ensure(
        root.path(),
        &manifest,
        Some(&fixture.url()),
        &Collect::default(),
        &|| false,
    )
    .expect("download");

    assert!(
        folder_listing(&dir).iter().all(|name| !name.ends_with(".part")),
        "{:?}",
        folder_listing(&dir)
    );
    assert!(dir.join("notes.txt").exists());
}
