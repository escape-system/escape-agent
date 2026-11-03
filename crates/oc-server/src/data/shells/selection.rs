//! Shell program discovery: configured shell, platform default and `PATH` lookup.

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Shell selection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShellKind {
    Cmd,
    PowerShell,
    Posix,
}

impl ShellKind {
    /// Argument vector for `command`.
    ///
    /// A POSIX shell that accepts `-o pipefail` gets it, so a pipeline reports
    /// its last non-zero segment instead of the last segment's status: without
    /// it `cargo clippy … | tail` looks like a success when clippy failed.
    ///
    /// POSIX does not require the option, so the shell is probed once (see
    /// [`oc_tools::util::shell_supports_pipefail`]). A shell that rejects it
    /// keeps the historical last-segment semantics instead of failing to start.
    pub(super) fn args(self, shell: &Path, command: &str) -> Vec<String> {
        match self {
            Self::Cmd => vec!["/c".to_owned(), command.to_owned()],
            Self::PowerShell => vec![
                "-NoLogo".to_owned(),
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                command.to_owned(),
            ],
            Self::Posix if oc_tools::util::shell_supports_pipefail(shell) => vec![
                "-o".to_owned(),
                "pipefail".to_owned(),
                "-c".to_owned(),
                command.to_owned(),
            ],
            Self::Posix => vec!["-c".to_owned(), command.to_owned()],
        }
    }
}

fn kind_of(program: &Path) -> ShellKind {
    let name = program
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name == "cmd" {
        ShellKind::Cmd
    } else if name == "pwsh" || name == "powershell" {
        ShellKind::PowerShell
    } else {
        ShellKind::Posix
    }
}

/// Resolves the configured shell, falling back to the platform default when
/// it is not installed (the config default is `pwsh` even on hosts without it).
pub(super) fn resolve_shell(requested: Option<&str>) -> (String, ShellKind) {
    if let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty())
        && let Some(path) = find_program(requested)
    {
        let kind = kind_of(&path);
        return (path.to_string_lossy().into_owned(), kind);
    }
    let path = platform_shell();
    let kind = kind_of(&path);
    (path.to_string_lossy().into_owned(), kind)
}

#[cfg(windows)]
fn platform_shell() -> PathBuf {
    for candidate in ["pwsh", "powershell"] {
        if let Some(path) = find_program(candidate) {
            return path;
        }
    }
    if let Some(comspec) = std::env::var_os("COMSPEC") {
        return PathBuf::from(comspec);
    }
    PathBuf::from("cmd.exe")
}

#[cfg(unix)]
fn platform_shell() -> PathBuf {
    if let Some(shell) = std::env::var_os("SHELL") {
        let path = PathBuf::from(shell);
        if path.is_file() {
            return path;
        }
    }
    for candidate in ["/bin/bash", "/usr/bin/bash", "/bin/sh", "/usr/bin/sh"] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from("sh")
}

fn find_program(program: &str) -> Option<PathBuf> {
    let direct = Path::new(program);
    if direct.is_absolute() || direct.components().count() > 1 {
        return direct.is_file().then(|| direct.to_path_buf());
    }
    let paths = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&paths) {
        let candidate = directory.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            if direct.extension().is_none() {
                for extension in ["exe", "cmd", "bat", "com"] {
                    let candidate = directory.join(format!("{program}.{extension}"));
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    None
}
