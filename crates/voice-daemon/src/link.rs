//! Hub address and credentials. Same variables the panel already reads.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;

#[derive(Clone, Debug)]
pub struct Hub {
    pub urls: Vec<String>,
    pub ca_path: Option<PathBuf>,
    pub creds_path: Option<PathBuf>,
    pub require_tls: bool,
    pub screen_id: String,
}

impl Hub {
    pub fn from_env() -> Result<Self, String> {
        let urls = std::env::var("NATS_URL").unwrap_or_default();
        let urls: Vec<String> = urls
            .split(',')
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string)
            .collect();
        let require_tls = urls.iter().any(|url| url.starts_with("tls://"));
        let dir = run_dir();
        let ca_path = materialize("NATS_CA_FILE", "NATS_CA_B64", &dir, "ca.pem")?;
        let creds_path = materialize("NATS_CREDS_FILE", "NATS_CREDS_B64", &dir, "user.creds")?;
        if require_tls && ca_path.is_none() {
            return Err("NATS_URL uses tls but NATS_CA_B64 or NATS_CA_FILE is missing".into());
        }
        let screen_id = std::env::var("SCREEN_ID")
            .ok()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| "dev".into());
        Ok(Self {
            urls,
            ca_path,
            creds_path,
            require_tls,
            screen_id,
        })
    }

    pub async fn connect(&self) -> Result<async_nats::Client, String> {
        if self.urls.is_empty() {
            return Err("NATS_URL is empty".into());
        }
        let mut options = async_nats::ConnectOptions::new()
            .name("voice-daemon")
            .connection_timeout(Duration::from_secs(5))
            .max_reconnects(None);
        if self.require_tls {
            options = options.require_tls(true);
        }
        if let Some(path) = &self.ca_path {
            options = options.add_root_certificates(path.clone());
        }
        if let Some(path) = &self.creds_path {
            options = options
                .credentials_file(path)
                .await
                .map_err(|error| error.to_string())?;
        }
        let addrs = self
            .urls
            .iter()
            .map(|url| url.parse::<async_nats::ServerAddr>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        options
            .connect(addrs)
            .await
            .map_err(|error| error.to_string())
    }
}

fn materialize(
    file_var: &str,
    b64_var: &str,
    dir: &Path,
    name: &str,
) -> Result<Option<PathBuf>, String> {
    if let Some(path) = std::env::var_os(file_var) {
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() {
            return Ok(None);
        }
        return Ok(Some(path));
    }
    let Some(value) = std::env::var(b64_var)
        .ok()
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .map_err(|_| format!("{b64_var} is not valid base64"))?;
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let path = dir.join(name);
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| error.to_string())?;
    file.write_all(&bytes).map_err(|error| error.to_string())?;
    Ok(Some(path))
}

fn run_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("VOICE_RUN_DIR") {
        return PathBuf::from(dir);
    }
    let preferred = PathBuf::from("/run/voice");
    if cfg!(target_os = "linux") {
        preferred
    } else {
        std::env::temp_dir().join("voice")
    }
}
