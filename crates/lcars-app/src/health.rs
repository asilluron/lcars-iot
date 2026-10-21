//! Heartbeat file and tmpfs credentials. Nothing secret is written outside the run dir.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{BootstrapConfig, ConfigError, decode_b64};

pub struct RuntimeFiles {
    pub run_dir: PathBuf,
    pub creds_path: Option<PathBuf>,
    pub ca_path: Option<PathBuf>,
}

pub fn materialize(config: &BootstrapConfig) -> Result<RuntimeFiles, ConfigError> {
    let run_dir = run_dir();
    std::fs::create_dir_all(&run_dir).map_err(|_| ConfigError::Invalid {
        name: "LCARS_RUN_DIR",
        value: run_dir.display().to_string(),
    })?;
    let creds_path = match &config.nats_creds_b64 {
        Some(value) => {
            let bytes = decode_b64(value)?;
            let path = run_dir.join("user.creds");
            write_secret(&path, &bytes)?;
            Some(path)
        }
        None => None,
    };
    let ca_path = match &config.nats_ca_b64 {
        Some(value) => {
            let bytes = decode_b64(value)?;
            let path = run_dir.join("ca.pem");
            write_secret(&path, &bytes)?;
            Some(path)
        }
        None => None,
    };
    Ok(RuntimeFiles {
        run_dir,
        creds_path,
        ca_path,
    })
}

pub fn touch_heartbeat(dir: &Path) {
    let path = dir.join("heartbeat");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".into());
    if let Err(error) = std::fs::write(path, stamp) {
        tracing::debug!(%error, "heartbeat file");
    }
}

fn run_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LCARS_RUN_DIR") {
        return PathBuf::from(dir);
    }
    let preferred = PathBuf::from("/run/lcars");
    if preferred.parent().is_some_and(|parent| parent.exists()) || cfg!(target_os = "linux") {
        return preferred;
    }
    std::env::temp_dir().join("lcars")
}

fn write_secret(path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| ConfigError::Invalid {
            name: "secret",
            value: path.display().to_string(),
        })?;
    file.write_all(bytes).map_err(|_| ConfigError::Invalid {
        name: "secret",
        value: path.display().to_string(),
    })
}
