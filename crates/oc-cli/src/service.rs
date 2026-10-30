//! Managed-service discovery file: write, read, heartbeat and cleanup.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// `<state>/opencode/service[-<channel>].json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceRegistration {
    pub id: String,
    pub version: String,
    pub url: String,
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// State root: `$OPENCODE_STATE_DIR`, `$XDG_STATE_HOME/opencode`, or
/// `$HOME/.local/state/opencode`.
#[must_use]
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("OPENCODE_STATE_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(dir).join("opencode");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/opencode");
    }
    std::env::temp_dir().join("opencode-state")
}

/// `service.json` for the `latest` channel, otherwise `service-<channel>.json`.
#[must_use]
pub fn registration_path(channel: &str) -> PathBuf {
    let name = if channel == "latest" {
        "service.json".to_owned()
    } else {
        format!("service-{channel}.json")
    };
    state_dir().join(name)
}

/// Atomically write the registration with 0600 permissions where supported.
pub fn write(path: &Path, registration: &ServiceRegistration) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("tmp.{}", std::process::id()));
    let bytes = serde_json::to_vec(registration).map_err(io::Error::other)?;

    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
    }

    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&temp, path)
}

/// Read the registration; `None` if the file does not exist.
pub fn read(path: &Path) -> io::Result<Option<ServiceRegistration>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Remove the file only if it still belongs to this process.
pub fn remove_if_owned(path: &Path, id: &str) {
    if read(path)
        .ok()
        .flatten()
        .is_some_and(|registration| registration.id == id)
    {
        let _ = fs::remove_file(path);
    }
}

/// Cancel `shutdown` if the registration disappears or is replaced.
pub async fn watch(path: PathBuf, id: String, shutdown: CancellationToken) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = interval.tick() => {
                let owned = read(&path)
                    .ok()
                    .flatten()
                    .is_some_and(|registration| registration.id == id);
                if !owned {
                    tracing::warn!(path = %path.display(), "service registration lost; shutting down");
                    shutdown.cancel();
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registration(id: &str) -> ServiceRegistration {
        ServiceRegistration {
            id: id.to_owned(),
            version: "0.1.0".to_owned(),
            url: "http://127.0.0.1:4096".to_owned(),
            pid: 42,
            password: Some("secret".to_owned()),
        }
    }

    #[test]
    fn write_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.json");
        write(&path, &registration("svc_1")).unwrap();
        assert_eq!(read(&path).unwrap(), Some(registration("svc_1")));
    }

    #[test]
    fn remove_only_when_owned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.json");
        write(&path, &registration("svc_1")).unwrap();
        remove_if_owned(&path, "other");
        assert!(path.exists());
        remove_if_owned(&path, "svc_1");
        assert!(!path.exists());
    }

    #[test]
    fn invalid_json_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.json");
        std::fs::write(&path, b"{").unwrap();
        assert!(read(&path).is_err());
    }

    #[test]
    fn channel_naming() {
        assert!(registration_path("latest").ends_with("service.json"));
        assert!(registration_path("dev").ends_with("service-dev.json"));
    }
}
