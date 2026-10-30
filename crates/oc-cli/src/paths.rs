//! Default data paths and generated credentials.

use std::path::PathBuf;

use base64::Engine;
use rand::Rng;

pub(crate) fn default_db_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("OPENCODE_DATA_DIR") {
        return PathBuf::from(dir).join("opencode.db");
    }
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(dir).join("opencode").join("opencode.db");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/opencode/opencode.db");
    }
    std::env::temp_dir().join("opencode").join("opencode.db")
}

pub(crate) fn random_password() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
