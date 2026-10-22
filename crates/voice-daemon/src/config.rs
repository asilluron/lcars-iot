//! Runtime settings. Thresholds reload on `SIGHUP`; model paths do not.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq)]
pub struct VoiceConfig {
    pub trigger: TriggerSection,
    pub verifier: VerifierSection,
    pub gate: GateSection,
    pub vad: VadSection,
    pub session: SessionSection,
    pub capture: CaptureSection,
    pub log: LogSection,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TriggerSection {
    pub threshold: f32,
    pub sliding_window: usize,
    pub model: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VerifierSection {
    pub threshold: f32,
    pub melspectrogram: PathBuf,
    pub embedding: PathBuf,
    pub classifier: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GateSection {
    pub onset_ms: u64,
    pub silence_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VadSection {
    pub threshold: f32,
    pub model: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionSection {
    pub silence_end_ms: u64,
    pub hard_cap_ms: u64,
    pub cooldown_ms: u64,
    pub preroll_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CaptureSection {
    pub device: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogSection {
    pub dir: PathBuf,
    pub max_bytes: u64,
}

/// Values a `SIGHUP` is allowed to change.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LiveSettings {
    pub trigger_threshold: f32,
    pub verifier_threshold: f32,
    pub onset_ms: u64,
    pub silence_ms: u64,
    pub vad_threshold: f32,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("could not read {path}: {message}")]
    Read { path: String, message: String },
    #[error("could not parse {path}: {message}")]
    Parse { path: String, message: String },
}

#[derive(Deserialize)]
struct FileConfig {
    trigger: TriggerFile,
    verifier: VerifierFile,
    gate: GateFile,
    vad: VadFile,
    session: SessionFile,
    capture: CaptureFile,
    log: LogFile,
}

#[derive(Deserialize)]
struct TriggerFile {
    threshold: f32,
    sliding_window: usize,
    model: PathBuf,
}

#[derive(Deserialize)]
struct VerifierFile {
    threshold: f32,
    melspectrogram: PathBuf,
    embedding: PathBuf,
    classifier: PathBuf,
}

#[derive(Deserialize)]
struct GateFile {
    onset_ms: u64,
    silence_ms: u64,
}

#[derive(Deserialize)]
struct VadFile {
    threshold: f32,
    model: PathBuf,
}

#[derive(Deserialize)]
struct SessionFile {
    silence_end_ms: u64,
    hard_cap_ms: u64,
    cooldown_ms: u64,
    preroll_ms: u64,
}

#[derive(Deserialize)]
struct CaptureFile {
    device: String,
}

#[derive(Deserialize)]
struct LogFile {
    dir: PathBuf,
    max_bytes: u64,
}

impl VoiceConfig {
    #[must_use]
    pub fn live(&self) -> LiveSettings {
        LiveSettings {
            trigger_threshold: self.trigger.threshold,
            verifier_threshold: self.verifier.threshold,
            onset_ms: self.gate.onset_ms,
            silence_ms: self.gate.silence_ms,
            vad_threshold: self.vad.threshold,
        }
    }

    /// Copy the tunable numbers from `fresh`. Model paths stay put.
    pub fn apply_live(&mut self, fresh: &Self) {
        self.trigger.threshold = fresh.trigger.threshold;
        self.verifier.threshold = fresh.verifier.threshold;
        self.gate = fresh.gate.clone();
        self.vad.threshold = fresh.vad.threshold;
    }
}

pub fn load(path: &Path) -> Result<VoiceConfig, ConfigError> {
    let text = fs::read_to_string(path).map_err(|error| ConfigError::Read {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    let file: FileConfig = toml::from_str(&text).map_err(|error| ConfigError::Parse {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    Ok(VoiceConfig {
        trigger: TriggerSection {
            threshold: file.trigger.threshold,
            sliding_window: file.trigger.sliding_window.max(1),
            model: resolve(base, file.trigger.model),
        },
        verifier: VerifierSection {
            threshold: file.verifier.threshold,
            melspectrogram: resolve(base, file.verifier.melspectrogram),
            embedding: resolve(base, file.verifier.embedding),
            classifier: resolve(base, file.verifier.classifier),
        },
        gate: GateSection {
            onset_ms: file.gate.onset_ms,
            silence_ms: file.gate.silence_ms,
        },
        vad: VadSection {
            threshold: file.vad.threshold,
            model: resolve(base, file.vad.model),
        },
        session: SessionSection {
            silence_end_ms: file.session.silence_end_ms,
            hard_cap_ms: file.session.hard_cap_ms,
            cooldown_ms: file.session.cooldown_ms,
            preroll_ms: file.session.preroll_ms,
        },
        capture: CaptureSection {
            device: file.capture.device,
        },
        log: LogSection {
            dir: resolve(base, file.log.dir),
            max_bytes: file.log.max_bytes,
        },
    })
}

/// `--config`, then `VOICE_CONFIG`, then the image path, then the repo file.
#[must_use]
pub fn resolve_config_path(flag: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = flag {
        return Some(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("VOICE_CONFIG") {
        return Some(PathBuf::from(path));
    }
    for candidate in ["/opt/voice/voice.toml", "voice/voice.toml"] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn resolve(base: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::{VoiceConfig, load};

    #[test]
    fn sighup_keeps_model_paths_and_refreshes_thresholds() {
        let dir = std::env::temp_dir().join(format!("voice-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("voice.toml");
        std::fs::write(
            &path,
            r#"
[trigger]
threshold = 0.85
sliding_window = 3
model = "models/computer.tflite"

[verifier]
threshold = 0.5
melspectrogram = "models/melspectrogram.onnx"
embedding = "models/embedding_model.onnx"
classifier = "models/computer-verifier.onnx"

[gate]
onset_ms = 650
silence_ms = 400

[vad]
threshold = 0.5
model = "models/silero_vad.onnx"

[session]
silence_end_ms = 8000
hard_cap_ms = 30000
cooldown_ms = 1500
preroll_ms = 1500

[capture]
device = "auto"

[log]
dir = "/data/wake-log"
max_bytes = 524288000
"#,
        )
        .unwrap();
        let mut config = load(&path).unwrap();
        let model = config.trigger.model.clone();
        std::fs::write(
            &path,
            r#"
[trigger]
threshold = 0.72
sliding_window = 3
model = "models/other.tflite"

[verifier]
threshold = 0.4
melspectrogram = "models/melspectrogram.onnx"
embedding = "models/embedding_model.onnx"
classifier = "models/computer-verifier.onnx"

[gate]
onset_ms = 700
silence_ms = 500

[vad]
threshold = 0.6
model = "models/silero_vad.onnx"

[session]
silence_end_ms = 8000
hard_cap_ms = 30000
cooldown_ms = 1500
preroll_ms = 1500

[capture]
device = "auto"

[log]
dir = "/data/wake-log"
max_bytes = 524288000
"#,
        )
        .unwrap();
        let fresh = load(&path).unwrap();
        config.apply_live(&fresh);
        assert_eq!(config.trigger.model, model);
        assert!((config.trigger.threshold - 0.72).abs() < f32::EPSILON);
        assert!((config.verifier.threshold - 0.4).abs() < f32::EPSILON);
        assert_eq!(config.gate.onset_ms, 700);
        assert_eq!(config.gate.silence_ms, 500);
        assert!((config.vad.threshold - 0.6).abs() < f32::EPSILON);
        let _ = VoiceConfig::live(&config);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
