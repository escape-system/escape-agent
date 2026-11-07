//! Default data paths and generated credentials.

use std::path::PathBuf;

use base64::Engine;
use rand::Rng;

pub(crate) fn default_db_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("ESCAPE_DATA_DIR") {
        return PathBuf::from(dir).join("escape.db");
    }
    if let Some(dir) = std::env::var_os("OPENCODE_DATA_DIR") {
        return PathBuf::from(dir).join("opencode-rust.db");
    }
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(dir).join("escape-agent").join("escape.db");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/escape-agent/escape.db");
    }
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile).join(".local/share/escape-agent/escape.db");
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local_app_data)
            .join("escape-agent")
            .join("escape.db");
    }
    std::env::temp_dir().join("escape-agent").join("escape.db")
}

/// Discover `web/dist` relative to the running binary when neither
/// `--web-dist`, `OC_WEB_DIST`, nor `./web/dist` in `current_dir()` is present.
pub(crate) fn discover_web_dist() -> Option<PathBuf> {
    if std::env::var_os("OC_WEB_DIST").is_some() {
        return None;
    }
    if let Ok(cwd) = std::env::current_dir() {
        let local = cwd.join("web").join("dist");
        if local.is_dir() {
            return Some(local);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let mut ancestor = exe.parent();
    for _ in 0..4 {
        let dir = ancestor?;
        let candidate = dir.join("web").join("dist");
        if candidate.join("index.html").is_file() {
            return Some(candidate);
        }
        ancestor = dir.parent();
    }
    None
}

pub(crate) fn random_password() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
