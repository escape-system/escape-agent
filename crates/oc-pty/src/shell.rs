//! Platform shell discovery and default arguments.

use std::path::{Path, PathBuf};

/// Shells that upstream starts with `-l` when no explicit args are supplied.
const LOGIN_SHELLS: &[&str] = &["bash", "sh", "zsh", "dash", "ksh", "fish"];

pub(crate) fn resolve(explicit: Option<&str>) -> String {
    if let Some(shell) = explicit.map(str::trim).filter(|shell| !shell.is_empty()) {
        return shell.to_string();
    }
    platform_default()
}

/// Arguments used when [`crate::CreatePty::args`] is empty.
pub(crate) fn default_args(shell: &str) -> Vec<String> {
    let name = Path::new(shell)
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if LOGIN_SHELLS.contains(&name.as_str()) {
        vec!["-l".to_string()]
    } else {
        Vec::new()
    }
}

#[cfg(windows)]
fn platform_default() -> String {
    for candidate in ["pwsh", "powershell"] {
        if let Some(path) = which(candidate) {
            return path.to_string_lossy().into_owned();
        }
    }
    std::env::var("COMSPEC")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "cmd.exe".to_string())
}

#[cfg(unix)]
fn platform_default() -> String {
    if let Some(shell) = std::env::var("SHELL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && is_executable(Path::new(value)))
    {
        return shell;
    }
    if let Some(path) = which("bash") {
        return path.to_string_lossy().into_owned();
    }
    "sh".to_string()
}

#[cfg(windows)]
fn which(program: &str) -> Option<PathBuf> {
    let direct = Path::new(program);
    if direct.is_absolute() || direct.components().count() > 1 {
        return direct.is_file().then(|| direct.to_path_buf());
    }
    let paths = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&paths) {
        for extension in ["exe", "cmd", "bat", "com", ""] {
            let candidate = if extension.is_empty() {
                directory.join(program)
            } else {
                directory.join(format!("{program}.{extension}"))
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn which(program: &str) -> Option<PathBuf> {
    let direct = Path::new(program);
    if direct.is_absolute() || direct.components().count() > 1 {
        return is_executable(direct).then(|| direct.to_path_buf());
    }
    let paths = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&paths) {
        let candidate = directory.join(program);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{default_args, resolve};

    #[test]
    fn login_shells_get_login_flag() {
        assert_eq!(default_args("/bin/bash"), vec!["-l"]);
        assert_eq!(default_args("/bin/sh"), vec!["-l"]);
        assert!(
            default_args("C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe")
                .is_empty()
        );
        assert!(default_args("cmd.exe").is_empty());
    }

    #[test]
    fn explicit_shell_wins() {
        assert_eq!(resolve(Some("/bin/sh")), "/bin/sh");
        assert!(!resolve(None).is_empty());
    }
}
