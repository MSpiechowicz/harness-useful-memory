//! The model in memory: tokenizer and graph, one note per inference call.
//!
//! The int8 model quantises its activations per call, so a vector depends on what else is in the
//! batch. One text per call keeps a stored vector and a later query vector of the same text
//! comparable. The tokenizer alone takes about 290 MB of memory, so after [`IDLE`] without an
//! embed [`Resident`] drops both and loads them again on the next call.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokenizers::{Tokenizer, TruncationParams};
use tract_onnx::prelude::*;

use super::manifest::{ONNX_FILE, Spec, TOKENIZER_FILE};
use super::{Event, Events};

/// How long a loaded model may sit unused.
pub const IDLE: Duration = Duration::from_secs(10 * 60);

/// The longest text handed to the tokenizer, in characters: far more than the 128 tokens the model
/// reads, so the tail is never tokenized for nothing.
pub const TEXT_MAX: usize = 2000;

/// Something that turns a text into a unit vector.
pub trait Embed: Send {
    /// The normalised vector of `text`.
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, String>;
}

/// Loads an engine from a model folder.
pub trait Loader: Send + Sync {
    fn load(&self, dir: &Path, spec: &Spec) -> Result<Box<dyn Embed>, String>;
}

/// The tract engine over the files of a model folder.
pub struct TractLoader;

impl Loader for TractLoader {
    fn load(&self, dir: &Path, spec: &Spec) -> Result<Box<dyn Embed>, String> {
        Ok(Box::new(Tract::load(dir, spec)?))
    }
}

/// The tokenizer and the optimised graph of the multilingual MiniLM.
struct Tract {
    tokenizer: Tokenizer,
    model: Arc<TypedRunnableModel>,
    dims: usize,
}

impl Tract {
    fn load(dir: &Path, spec: &Spec) -> Result<Self, String> {
        let mut tokenizer = Tokenizer::from_file(dir.join(TOKENIZER_FILE))
            .map_err(|error| format!("cannot read the tokenizer: {error}"))?;

        tokenizer.with_padding(None);
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: spec.max_tokens,
                ..Default::default()
            }))
            .map_err(|error| format!("cannot set up the tokenizer: {error}"))?;

        let failed = |error: TractError| format!("cannot load the model: {error}");
        let mut model = tract_onnx::onnx().model_for_path(dir.join(ONNX_FILE)).map_err(failed)?;

        let batch = model.symbols.sym("batch_size");
        let length = model.symbols.sym("sequence_length");

        for input in 0..3 {
            let fact = InferenceFact::dt_shape(
                i64::datum_type(),
                tvec!(TDim::from(batch.clone()), TDim::from(length.clone())),
            );
            model.set_input_fact(input, fact).map_err(failed)?;
        }

        let model = model
            .into_optimized()
            .and_then(|model| model.into_runnable())
            .map_err(failed)?;

        Ok(Self {
            tokenizer,
            model,
            dims: spec.dims,
        })
    }
}

impl Embed for Tract {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
        let text: String = text.chars().take(TEXT_MAX).collect();
        let encoded = self
            .tokenizer
            .encode(text, true)
            .map_err(|error| format!("cannot tokenize: {error}"))?;

        let length = encoded.get_ids().len();
        let tensor = |values: Vec<i64>| -> Result<TValue, String> {
            tract_ndarray::Array2::from_shape_vec((1, length), values)
                .map(|array| array.into_tensor().into())
                .map_err(|error| error.to_string())
        };

        let ids = tensor(encoded.get_ids().iter().map(|&id| i64::from(id)).collect())?;
        let mask = tensor(encoded.get_attention_mask().iter().map(|&bit| i64::from(bit)).collect())?;
        let types = tensor(vec![0; length])?;

        let outputs = self
            .model
            .run(tvec!(ids, mask, types))
            .map_err(|error| format!("the model failed: {error}"))?;

        let hidden = outputs[0]
            .to_plain_array_view::<f32>()
            .map_err(|error| error.to_string())?;

        if hidden.shape().len() != 3 || hidden.shape()[2] != self.dims {
            return Err("the model output has an unexpected shape".to_owned());
        }

        Ok(pooled(
            hidden.iter().copied().collect::<Vec<f32>>().as_slice(),
            self.dims,
            encoded.get_attention_mask(),
        ))
    }
}

/// The mean of the token vectors the mask keeps, scaled to length 1.
fn pooled(hidden: &[f32], dims: usize, mask: &[u32]) -> Vec<f32> {
    let mut vector = vec![0_f32; dims];
    let mut kept = 0_f32;

    for (token, bit) in mask.iter().enumerate() {
        if *bit == 0 {
            continue;
        }

        kept += 1.0;

        for (sum, value) in vector.iter_mut().zip(&hidden[token * dims..(token + 1) * dims]) {
            *sum += value;
        }
    }

    let kept = kept.max(1.0);
    vector.iter_mut().for_each(|value| *value /= kept);

    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt().max(1e-12);
    vector.iter_mut().for_each(|value| *value /= norm);

    vector
}

/// An engine that is loaded when needed and dropped after [`Resident::unload_if_idle`].
pub struct Resident {
    loader: Arc<dyn Loader>,
    dir: std::path::PathBuf,
    spec: Spec,
    events: Arc<dyn Events>,
    engine: Option<Box<dyn Embed>>,
    used: Instant,
}

impl Resident {
    pub fn new(loader: Arc<dyn Loader>, dir: std::path::PathBuf, spec: Spec, events: Arc<dyn Events>) -> Self {
        Self {
            loader,
            dir,
            spec,
            events,
            engine: None,
            used: Instant::now(),
        }
    }

    /// The model's spec.
    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    /// Loads the engine when it is not (emitting `loading`).
    pub fn ensure(&mut self) -> Result<&mut dyn Embed, String> {
        if self.engine.is_none() {
            self.events.emit(Event::Loading);
            self.engine = Some(self.loader.load(&self.dir, &self.spec)?);
        }

        self.used = Instant::now();
        let engine = self.engine.as_mut().ok_or("the model is not loaded")?;
        Ok(&mut **engine)
    }

    /// The vector of `text`, loading the engine first when it was dropped.
    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
        let vector = self.ensure()?.embed(text)?;
        self.used = Instant::now();
        Ok(vector)
    }

    /// Drops the engine when it was last used `idle` ago or earlier; whether it did.
    pub fn unload_if_idle(&mut self, idle: Duration) -> bool {
        if self.engine.is_none() || self.used.elapsed() < idle {
            return false;
        }

        self.engine = None;
        true
    }

    pub fn is_loaded(&self) -> bool {
        self.engine.is_some()
    }
}
