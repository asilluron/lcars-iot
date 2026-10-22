//! Two-second clips for later training. Confirmed and rejected detections both land here.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::wav;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WakeMeta {
    pub confirmed: bool,
    pub trigger_score: f32,
    pub verifier_score: Option<f32>,
    pub gate_open: bool,
    pub unix_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

pub fn write_wake(dir: &Path, samples: &[i16], meta: &WakeMeta) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let stem = format!(
        "{}_{}",
        meta.unix_ms,
        if meta.confirmed {
            "confirmed"
        } else {
            "rejected"
        }
    );
    let wav_path = dir.join(format!("{stem}.wav"));
    let json_path = dir.join(format!("{stem}.json"));
    wav::write_wav(&wav_path, samples)?;
    fs::write(&json_path, serde_json::to_vec_pretty(meta)?)?;
    Ok(wav_path)
}

pub fn write_tag(json_path: &Path, positive: bool) -> io::Result<()> {
    let bytes = fs::read(json_path)?;
    let mut meta: WakeMeta = serde_json::from_slice(&bytes)?;
    meta.tag = Some(if positive { "positive" } else { "negative" }.into());
    fs::write(json_path, serde_json::to_vec_pretty(&meta)?)?;
    Ok(())
}

/// Delete the oldest wav/json pairs until the directory is within `max_bytes`.
pub fn enforce_budget(dir: &Path, max_bytes: u64) -> io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let mut pairs: Vec<(String, u64)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.ends_with(".wav") {
            continue;
        }
        let wav_len = entry.metadata()?.len();
        let json_len = fs::metadata(dir.join(name.replace(".wav", ".json")))
            .map(|meta| meta.len())
            .unwrap_or(0);
        pairs.push((
            name.trim_end_matches(".wav").to_string(),
            wav_len + json_len,
        ));
    }
    pairs.sort_by(|left, right| left.0.cmp(&right.0));
    let mut total: u64 = pairs.iter().map(|(_, bytes)| *bytes).sum();
    for (stem, bytes) in pairs {
        if total <= max_bytes {
            break;
        }
        let _ = fs::remove_file(dir.join(format!("{stem}.wav")));
        let _ = fs::remove_file(dir.join(format!("{stem}.json")));
        total = total.saturating_sub(bytes);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{WakeMeta, enforce_budget, write_tag, write_wake};

    #[test]
    fn log_names_and_budget_drop_the_oldest_pair() {
        let dir = std::env::temp_dir().join(format!("voice-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let older = WakeMeta {
            confirmed: false,
            trigger_score: 0.9,
            verifier_score: None,
            gate_open: false,
            unix_ms: 10,
            tag: None,
        };
        let newer = WakeMeta {
            confirmed: true,
            trigger_score: 0.95,
            verifier_score: Some(0.8),
            gate_open: true,
            unix_ms: 20,
            tag: None,
        };
        write_wake(&dir, &[1, 2, 3], &older).unwrap();
        write_wake(&dir, &[4, 5], &newer).unwrap();
        let json = dir.join("10_rejected.json");
        write_tag(&json, true).unwrap();
        let tagged: WakeMeta = serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
        assert_eq!(tagged.tag.as_deref(), Some("positive"));
        let size = std::fs::metadata(dir.join("10_rejected.wav"))
            .unwrap()
            .len()
            + std::fs::metadata(&json).unwrap().len();
        enforce_budget(&dir, size).unwrap();
        assert!(!dir.join("10_rejected.wav").exists());
        assert!(dir.join("20_confirmed.wav").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
