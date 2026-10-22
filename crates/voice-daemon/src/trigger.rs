//! Streaming microWakeWord trigger. One 10 ms block in, a score out when the window fills.

use std::path::Path;

use micro_wakeword::{AUDIO_BLOCK_SAMPLES, Detector};

pub struct Trigger {
    detector: Detector,
    model: std::path::PathBuf,
    window: usize,
    threshold: f32,
}

impl Trigger {
    pub fn open(model: &Path, threshold: f32, window: usize) -> Result<Self, String> {
        let window = window.max(1);
        let detector = Detector::builder(model)
            .wake_word("computer")
            .probability_cutoff(threshold)
            .sliding_window_size(window)
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            detector,
            model: model.to_path_buf(),
            window,
            threshold,
        })
    }

    pub fn set_threshold(&mut self, threshold: f32) -> Result<(), String> {
        if (self.threshold - threshold).abs() < f32::EPSILON {
            return Ok(());
        }
        self.detector = Detector::builder(&self.model)
            .wake_word("computer")
            .probability_cutoff(threshold)
            .sliding_window_size(self.window)
            .build()
            .map_err(|error| error.to_string())?;
        self.threshold = threshold;
        Ok(())
    }

    /// `Some(score)` when the sliding window average is above the cutoff.
    pub fn push(&mut self, block: &[i16]) -> Result<Option<f32>, String> {
        if block.len() != AUDIO_BLOCK_SAMPLES {
            return Err(format!(
                "trigger wants {AUDIO_BLOCK_SAMPLES} samples, got {}",
                block.len()
            ));
        }
        Ok(self
            .detector
            .process_audio(block)
            .map_err(|error| error.to_string())?
            .map(|detection| detection.probability))
    }

    pub fn reset(&mut self) -> Result<(), String> {
        self.detector.reset().map_err(|error| error.to_string())
    }
}
