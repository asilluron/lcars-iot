//! Connection settings for one screen. Credentials are never included in `Debug`.

use std::path::PathBuf;

#[derive(Clone)]
pub struct SyncConfig {
    pub screen_id: String,
    pub nats_urls: Vec<String>,
    pub creds: Option<String>,
    pub ca_path: Option<PathBuf>,
    pub require_tls: bool,
    pub version: String,
}

impl std::fmt::Debug for SyncConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncConfig")
            .field("screen_id", &self.screen_id)
            .field("nats_urls", &self.nats_urls)
            .field("require_tls", &self.require_tls)
            .field("has_creds", &self.creds.is_some())
            .field("has_ca", &self.ca_path.is_some())
            .field("version", &self.version)
            .finish()
    }
}
