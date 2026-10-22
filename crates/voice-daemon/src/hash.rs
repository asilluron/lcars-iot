//! Content hashes logged at startup and sent with each session.

use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

#[must_use]
pub fn file_hash(path: &Path) -> String {
    let Ok(bytes) = fs::read(path) else {
        return "absent".into();
    };
    format!("{:x}", Sha256::digest(bytes))
}
