//! Silero voice activity, one 32 ms window at a time.

use std::path::Path;

use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector};

const WINDOW: usize = 512;

pub struct Vad {
    detector: VoiceActivityDetector,
    model: std::path::PathBuf,
    threshold: f32,
}

impl Vad {
    pub fn open(model: &Path, threshold: f32) -> Result<Self, String> {
        let detector = make(model, threshold)?;
        Ok(Self {
            detector,
            model: model.to_path_buf(),
            threshold,
        })
    }

    pub fn set_threshold(&mut self, threshold: f32) -> Result<(), String> {
        if (self.threshold - threshold).abs() < f32::EPSILON {
            return Ok(());
        }
        self.detector = make(&self.model, threshold)?;
        self.threshold = threshold;
        Ok(())
    }

    /// `true` while Silero currently hears speech.
    pub fn push(&mut self, samples: &[i16]) -> bool {
        let floats: Vec<f32> = samples
            .iter()
            .map(|sample| *sample as f32 / 32768.0)
            .collect();
        self.detector.accept_waveform(&floats);
        let speech = self.detector.detected();
        while !self.detector.is_empty() {
            self.detector.pop();
        }
        speech
    }
}

fn make(model: &Path, threshold: f32) -> Result<VoiceActivityDetector, String> {
    let config = VadModelConfig {
        silero_vad: SileroVadModelConfig {
            model: Some(model.display().to_string()),
            threshold,
            min_silence_duration: 0.1,
            min_speech_duration: 0.032,
            window_size: WINDOW as i32,
            max_speech_duration: 30.0,
        },
        sample_rate: 16_000,
        num_threads: 1,
        provider: Some("cpu".into()),
        debug: false,
        ..Default::default()
    };
    VoiceActivityDetector::create(&config, 2.0).ok_or_else(|| "silero vad failed to load".into())
}

#[must_use]
pub fn window_samples() -> usize {
    WINDOW
}
