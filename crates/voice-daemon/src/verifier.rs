//! One openWakeWord score over a clip.
//!
//! The shared mel and embedding networks plus the classifier all have to be on
//! disk. A missing classifier refuses the detection.

use std::collections::VecDeque;
use std::path::Path;

use tract_onnx::prelude::*;

const CHUNK: usize = 1280;
const LOOKBACK: usize = 480;
const MEL_INPUT: usize = LOOKBACK + CHUNK;
const MELS_PER_CHUNK: usize = 8;
const MEL_BINS: usize = 32;
const MEL_KEEP: usize = 10;
const EMBED_FRAMES: usize = 76;
const EMBED_DIM: usize = 96;
const FEATURES: usize = 16;

type Plan = SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

pub struct Verifier {
    engine: Option<Engine>,
}

struct Engine {
    mel: Plan,
    embedding: Plan,
    classifier: Plan,
}

struct Stream {
    lookback: Vec<f32>,
    mels: VecDeque<Vec<f32>>,
    embeddings: VecDeque<Vec<f32>>,
}

impl Verifier {
    /// Loads the three networks. A missing classifier leaves the verifier closed.
    pub fn open(mel: &Path, embedding: &Path, classifier: &Path) -> Self {
        if !classifier.is_file() {
            tracing::warn!(
                path = %classifier.display(),
                "verifier classifier is absent; triggers will be rejected"
            );
            return Self { engine: None };
        }
        match Engine::load(mel, embedding, classifier) {
            Ok(engine) => Self {
                engine: Some(engine),
            },
            Err(error) => {
                tracing::warn!(%error, "verifier failed to load; triggers will be rejected");
                Self { engine: None }
            }
        }
    }

    #[must_use]
    pub fn available(&self) -> bool {
        self.engine.is_some()
    }

    /// `None` when no classifier is loaded. Otherwise the max score on the clip.
    pub fn score(&self, samples: &[i16]) -> Option<f32> {
        let engine = self.engine.as_ref()?;
        let mut audio: Vec<f32> = samples.iter().map(|sample| *sample as f32).collect();
        if audio.is_empty() {
            return Some(0.0);
        }
        let remainder = audio.len() % CHUNK;
        if remainder != 0 {
            audio.extend(std::iter::repeat_n(0.0, CHUNK - remainder));
        }
        let mut stream = Stream::new();
        let mut best = 0.0_f32;
        for chunk in audio.chunks(CHUNK) {
            let features = match stream.push(engine, chunk) {
                Ok(features) => features,
                Err(error) => {
                    tracing::warn!(%error, "verifier embedding failed");
                    return Some(0.0);
                }
            };
            match engine.classify(&features) {
                Ok(score) => best = best.max(score),
                Err(error) => {
                    tracing::warn!(%error, "verifier classify failed");
                    return Some(0.0);
                }
            }
        }
        Some(best)
    }
}

impl Engine {
    fn load(mel: &Path, embedding: &Path, classifier: &Path) -> Result<Self, String> {
        Ok(Self {
            mel: plan(mel, &[1, MEL_INPUT])?,
            embedding: plan(embedding, &[1, EMBED_FRAMES, MEL_BINS, 1])?,
            classifier: plan(classifier, &[1, FEATURES, EMBED_DIM])?,
        })
    }

    fn classify(&self, features: &[f32]) -> Result<f32, String> {
        let tensor = Tensor::from_shape(&[1, FEATURES, EMBED_DIM], features).map_err(err)?;
        let outputs = self.classifier.run(tvec!(tensor.into())).map_err(err)?;
        let values = outputs[0]
            .to_array_view::<f32>()
            .map_err(err)?
            .iter()
            .copied()
            .collect::<Vec<_>>();
        Ok(values.first().copied().unwrap_or(0.0))
    }
}

impl Stream {
    fn new() -> Self {
        Self {
            lookback: vec![0.0; LOOKBACK],
            mels: VecDeque::new(),
            embeddings: VecDeque::new(),
        }
    }

    fn push(&mut self, engine: &Engine, chunk: &[f32]) -> Result<Vec<f32>, String> {
        let mut input = Vec::with_capacity(MEL_INPUT);
        input.extend_from_slice(&self.lookback);
        input.extend_from_slice(chunk);
        let tail = chunk.len() - LOOKBACK;
        self.lookback.copy_from_slice(&chunk[tail..]);

        let tensor = Tensor::from_shape(&[1, MEL_INPUT], &input).map_err(err)?;
        let outputs = engine.mel.run(tvec!(tensor.into())).map_err(err)?;
        let raw = outputs[0]
            .to_array_view::<f32>()
            .map_err(err)?
            .iter()
            .map(|value| (value / 10.0) + 2.0)
            .collect::<Vec<_>>();
        let mut mel = vec![0.0; MELS_PER_CHUNK * MEL_BINS];
        let copy = raw.len().min(mel.len());
        mel[..copy].copy_from_slice(&raw[..copy]);
        self.mels.push_back(mel);
        while self.mels.len() > MEL_KEEP {
            self.mels.pop_front();
        }

        let mut stacked = vec![0.0; MEL_KEEP * MELS_PER_CHUNK * MEL_BINS];
        let have = self.mels.len() * MELS_PER_CHUNK * MEL_BINS;
        let offset = stacked.len() - have;
        for (index, chunk) in self.mels.iter().enumerate() {
            let at = offset + index * chunk.len();
            stacked[at..at + chunk.len()].copy_from_slice(chunk);
        }
        let start = (MEL_KEEP * MELS_PER_CHUNK - EMBED_FRAMES) * MEL_BINS;
        let window = &stacked[start..];
        let tensor = Tensor::from_shape(&[1, EMBED_FRAMES, MEL_BINS, 1], window).map_err(err)?;
        let outputs = engine.embedding.run(tvec!(tensor.into())).map_err(err)?;
        let raw = outputs[0]
            .to_array_view::<f32>()
            .map_err(err)?
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let mut embedding = vec![0.0; EMBED_DIM];
        let copy = raw.len().min(EMBED_DIM);
        if raw.len() >= EMBED_DIM {
            embedding.copy_from_slice(&raw[raw.len() - EMBED_DIM..]);
        } else {
            embedding[..copy].copy_from_slice(&raw[..copy]);
        }
        self.embeddings.push_back(embedding);
        while self.embeddings.len() > FEATURES {
            self.embeddings.pop_front();
        }
        let mut features = vec![0.0; FEATURES * EMBED_DIM];
        let have = self.embeddings.len() * EMBED_DIM;
        let offset = features.len() - have;
        for (index, embedding) in self.embeddings.iter().enumerate() {
            let at = offset + index * EMBED_DIM;
            features[at..at + EMBED_DIM].copy_from_slice(embedding);
        }
        Ok(features)
    }
}

fn plan(path: &Path, shape: &[usize]) -> Result<Plan, String> {
    tract_onnx::onnx()
        .model_for_path(path)
        .map_err(err)?
        .with_input_fact(0, f32::fact(shape).into())
        .map_err(err)?
        .into_optimized()
        .map_err(err)?
        .into_runnable()
        .map_err(err)
}

fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::Verifier;

    #[test]
    fn a_missing_classifier_rejects_the_clip() {
        let verifier = Verifier::open(
            Path::new("models/melspectrogram.onnx"),
            Path::new("models/embedding_model.onnx"),
            Path::new("models/computer-verifier.onnx"),
        );
        assert!(!verifier.available());
        assert_eq!(verifier.score(&[0, 1, 2]), None);
    }
}
