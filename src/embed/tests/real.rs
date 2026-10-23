//! The real model. Ignored by default: set `HARNESS_MEMORY_TEST_MODEL_DIR` to a folder that holds
//! `model_quantized.onnx` (or `onnx/model_quantized.onnx`) and `tokenizer.json`, then run
//! `cargo test -- --ignored real_model`.

use std::path::PathBuf;

use serde::Deserialize;
use tempfile::TempDir;

use crate::embed::manifest::{MODEL, ONNX_FILE, TOKENIZER_FILE};
use crate::embed::{Loader, TractLoader};

/// A model folder with the two files linked or copied in, from the environment; `None` (with a
/// note) when the variable is not set.
fn model_folder() -> Option<TempDir> {
    let Ok(source) = std::env::var("HARNESS_MEMORY_TEST_MODEL_DIR") else {
        eprintln!("HARNESS_MEMORY_TEST_MODEL_DIR is not set; skipping");
        return None;
    };

    let source = PathBuf::from(source);
    let onnx = [source.join(ONNX_FILE), source.join("onnx").join(ONNX_FILE)]
        .into_iter()
        .find(|path| path.is_file())
        .expect("model_quantized.onnx under HARNESS_MEMORY_TEST_MODEL_DIR");

    let folder = TempDir::new().expect("temp dir");

    for (from, name) in [(onnx, ONNX_FILE), (source.join(TOKENIZER_FILE), TOKENIZER_FILE)] {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&from, folder.path().join(name)).expect("link the model file");
        #[cfg(not(unix))]
        std::fs::copy(&from, folder.path().join(name)).expect("copy the model file");
    }

    Some(folder)
}

#[derive(Deserialize)]
struct Reference {
    sentences: Vec<String>,
    vectors_single: Vec<Vec<f64>>,
}

fn cosine(a: &[f32], b: &[f64]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| f64::from(*x) * y).sum();
    let norm = |values: &mut dyn Iterator<Item = f64>| values.map(|value| value * value).sum::<f64>().sqrt();

    dot / (norm(&mut a.iter().map(|x| f64::from(*x))) * norm(&mut b.iter().copied()))
}

#[test]
#[ignore = "needs the model files: set HARNESS_MEMORY_TEST_MODEL_DIR"]
fn real_model_vectors_match_the_python_reference() {
    let Some(folder) = model_folder() else {
        return;
    };

    let reference: Reference = serde_json::from_str(include_str!("reference.json")).expect("reference vectors");
    let mut engine = TractLoader.load(folder.path(), &MODEL.spec()).expect("load the model");

    let mut lowest = 1.0_f64;

    for (text, expected) in reference.sentences.iter().zip(&reference.vectors_single) {
        let vector = engine.embed(text).expect("embed");

        assert_eq!(vector.len(), 384);
        let length = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!((length - 1.0).abs() < 1e-3, "normalised: {length}");

        let similarity = cosine(&vector, expected);
        assert!(similarity >= 0.99, "{similarity} for {text:?}");
        lowest = lowest.min(similarity);
    }

    eprintln!("lowest cosine to the Python reference: {lowest:.4}");
}

#[test]
#[ignore = "needs the model files: set HARNESS_MEMORY_TEST_MODEL_DIR"]
fn real_model_ranks_the_english_note_for_a_polish_query_first() {
    let Some(folder) = model_folder() else {
        return;
    };

    let mut engine = TractLoader.load(folder.path(), &MODEL.spec()).expect("load the model");
    let notes = [
        "Run cargo clean before committing because target/debug grew to 276 GB.",
        "The login token expires after the laptop sleeps, so refresh it before the first request.",
        "The PostgreSQL connection pool with four threads fills up under load and queries time out.",
        "Keyboard shortcut Ctrl+[ toggles the left panel and Ctrl+] the right panel.",
    ];

    let vectors: Vec<Vec<f32>> = notes.iter().map(|note| engine.embed(note).expect("embed")).collect();
    let query = engine
        .embed("token logowania wygasa po uśpieniu laptopa")
        .expect("embed the query");

    let mut ranked: Vec<(usize, f32)> = vectors
        .iter()
        .enumerate()
        .map(|(index, vector)| (index, query.iter().zip(vector).map(|(a, b)| a * b).sum()))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));

    eprintln!("polish query ranking: {ranked:?}");
    assert_eq!(ranked[0].0, 1, "{ranked:?}");
    assert!(ranked[0].1 > ranked[1].1 + 0.1, "a clear margin: {ranked:?}");
}
