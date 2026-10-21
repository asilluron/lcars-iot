//! Bootstrap settings read once from the environment.

use std::collections::HashMap;
use std::path::PathBuf;

use base64::Engine;
use lcars_sync::SyncConfig;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq)]
pub struct BootstrapConfig {
    pub screen_id: String,
    pub nats_urls: Vec<String>,
    pub nats_creds_b64: Option<String>,
    pub nats_ca_b64: Option<String>,
    pub nats_ca_file: Option<String>,
    pub ui_scale: f32,
    pub brightness_pct: u32,
    pub idle_dim_s: u64,
    pub idle_off_s: u64,
    pub night_start_min: u16,
    pub night_end_min: u16,
    pub audio_enabled: bool,
    pub audio_volume: u8,
    pub log_level: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("invalid {name}: {value}")]
    Invalid { name: &'static str, value: String },
    #[error("NATS_URL uses tls but NATS_CA_B64 or NATS_CA_FILE is missing")]
    MissingCa,
}

impl BootstrapConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        let vars: HashMap<String, String> = std::env::vars().collect();
        Self::from_map(&vars)
    }

    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self, ConfigError> {
        let nats_urls: Vec<String> = vars
            .get("NATS_URL")
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|url| !url.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let require_tls = nats_urls.iter().any(|url| url.starts_with("tls://"));
        let nats_ca_b64 = nonempty(vars, "NATS_CA_B64");
        let nats_ca_file = nonempty(vars, "NATS_CA_FILE");
        if require_tls && nats_ca_b64.is_none() && nats_ca_file.is_none() {
            return Err(ConfigError::MissingCa);
        }
        let (night_start_min, night_end_min) = parse_window(
            vars.get("NIGHT_WINDOW")
                .map(String::as_str)
                .unwrap_or("22:00-06:00"),
        )?;
        Ok(Self {
            screen_id: vars
                .get("SCREEN_ID")
                .cloned()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| "dev".into()),
            nats_urls,
            nats_creds_b64: nonempty(vars, "NATS_CREDS_B64"),
            nats_ca_b64,
            nats_ca_file,
            ui_scale: parse_or(vars, "UI_SCALE", 1.0)?,
            brightness_pct: parse_or(vars, "BRIGHTNESS_PCT", 80)?,
            idle_dim_s: parse_or(vars, "IDLE_DIM_S", 300)?,
            idle_off_s: parse_or(vars, "IDLE_OFF_S", 360)?,
            night_start_min,
            night_end_min,
            audio_enabled: parse_or(vars, "AUDIO_ENABLED", 1_u8)? != 0,
            audio_volume: parse_or(vars, "AUDIO_VOLUME", 60)?,
            log_level: vars
                .get("LOG_LEVEL")
                .cloned()
                .unwrap_or_else(|| "info".into()),
        })
    }

    #[must_use]
    #[allow(dead_code)]
    pub fn to_sync(&self, creds: Option<String>, ca_path: Option<PathBuf>) -> Option<SyncConfig> {
        if self.nats_urls.is_empty() {
            return None;
        }
        Some(SyncConfig {
            screen_id: self.screen_id.clone(),
            nats_urls: self.nats_urls.clone(),
            creds,
            ca_path,
            require_tls: self.nats_urls.iter().any(|url| url.starts_with("tls://")),
            version: env!("CARGO_PKG_VERSION").to_string(),
        })
    }
}

fn nonempty(vars: &HashMap<String, String>, name: &str) -> Option<String> {
    vars.get(name).cloned().filter(|value| !value.is_empty())
}

pub fn decode_b64(value: &str) -> Result<Vec<u8>, ConfigError> {
    base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .map_err(|_| ConfigError::Invalid {
            name: "base64",
            value: "credential".into(),
        })
}

fn parse_or<T: std::str::FromStr>(
    vars: &HashMap<String, String>,
    name: &'static str,
    default: T,
) -> Result<T, ConfigError> {
    let Some(value) = vars.get(name) else {
        return Ok(default);
    };
    value.parse().map_err(|_| ConfigError::Invalid {
        name,
        value: value.clone(),
    })
}

fn parse_window(value: &str) -> Result<(u16, u16), ConfigError> {
    let Some((start, end)) = value.split_once('-') else {
        return Err(ConfigError::Invalid {
            name: "NIGHT_WINDOW",
            value: value.to_string(),
        });
    };
    Ok((parse_hhmm(start)?, parse_hhmm(end)?))
}

fn parse_hhmm(value: &str) -> Result<u16, ConfigError> {
    let Some((hour, minute)) = value.split_once(':') else {
        return Err(ConfigError::Invalid {
            name: "NIGHT_WINDOW",
            value: value.to_string(),
        });
    };
    let hour: u16 = hour.parse().map_err(|_| ConfigError::Invalid {
        name: "NIGHT_WINDOW",
        value: value.to_string(),
    })?;
    let minute: u16 = minute.parse().map_err(|_| ConfigError::Invalid {
        name: "NIGHT_WINDOW",
        value: value.to_string(),
    })?;
    if hour > 23 || minute > 59 {
        return Err(ConfigError::Invalid {
            name: "NIGHT_WINDOW",
            value: value.to_string(),
        });
    }
    Ok(hour * 60 + minute)
}

#[cfg(test)]
mod tests {
    use super::{BootstrapConfig, ConfigError, parse_window};
    use std::collections::HashMap;

    #[test]
    fn defaults_and_tls_requirement() {
        let config = BootstrapConfig::from_map(&HashMap::new()).unwrap();
        assert_eq!(config.screen_id, "dev");
        assert!(config.nats_urls.is_empty());
        assert_eq!(config.brightness_pct, 80);
        assert_eq!(config.idle_dim_s, 300);
        assert_eq!(config.night_start_min, 22 * 60);
        assert!(config.to_sync(None, None).is_none());

        let mut vars = HashMap::new();
        vars.insert("NATS_URL".into(), "tls://192.0.2.10:4222".into());
        vars.insert("SCREEN_ID".into(), "kitchen".into());
        assert_eq!(
            BootstrapConfig::from_map(&vars).unwrap_err(),
            ConfigError::MissingCa
        );
        vars.insert("NATS_CA_FILE".into(), "ca.pem".into());
        let config = BootstrapConfig::from_map(&vars).unwrap();
        assert_eq!(config.nats_ca_file.as_deref(), Some("ca.pem"));
        vars.insert("NATS_CA_B64".into(), "Y2E=".into());
        let config = BootstrapConfig::from_map(&vars).unwrap();
        assert_eq!(config.screen_id, "kitchen");
        assert!(config.to_sync(None, None).unwrap().require_tls);
    }

    #[test]
    fn night_window_wraps() {
        assert_eq!(parse_window("22:00-06:00").unwrap(), (22 * 60, 6 * 60));
    }
}
