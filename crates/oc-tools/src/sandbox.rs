//! Kernel-level shell sandboxing (`bwrap` / Bubblewrap) and secret-env scrubbing.
//!
//! Uses Linux Bubblewrap (`bwrap`) when enabled or available so shell commands
//! see a read-only root filesystem (`--ro-bind / /`) with writes strictly
//! restricted to `workdir` and an isolated `/tmp` (`--tmpfs /tmp`), plus
//! optional network namespace isolation (`--unshare-net`) and automatic secret
//! environment variable scrubbing (`docs/30-tos-safe-harness.md`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{Map, Value};
use tokio::process::Command;

/// Environment variables scrubbed by default before executing untrusted shell
/// steps so API credentials cannot leak via `env` / `printenv`.
pub const DEFAULT_SECRET_SCRUB_ENV: &[&str] = &[
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_GENERATIVE_AI_API_KEY",
    "ANTHROPIC_API_KEY",
    "OPENROUTER_API_KEY",
    "OPENAI_API_KEY",
    "NOUS_API_KEY",
    "GOOGLE_VERTEX_ACCESS_TOKEN",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "OPENCODE_PASSWORD",
    "OPENCODE_SERVER_PASSWORD",
    "AWS_SECRET_ACCESS_KEY",
];

/// Sandboxing policy for child commands executed by [`crate::ShellTool`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellSandboxMode {
    /// No OS namespace sandbox wrapper (legacy default when unconfigured).
    #[default]
    Off,
    /// Require Linux Bubblewrap (`bwrap`); fails cleanly if unavailable.
    Bubblewrap,
    /// Require Bubblewrap (`bwrap`) with `--unshare-net` network isolation.
    BubblewrapNoNet,
    /// Use Bubblewrap when installed and functional, otherwise fall back cleanly.
    Auto,
}

impl ShellSandboxMode {
    /// Parse `"off" | "bwrap" | "bubblewrap" | "offline" | "auto"`.
    #[must_use]
    pub fn from_config_str(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "disabled" | "none" => Some(Self::Off),
            "bwrap" | "bubblewrap" | "rw" | "workspace" => Some(Self::Bubblewrap),
            "no-net" | "nonet" | "offline" | "isolated" => Some(Self::BubblewrapNoNet),
            "auto" | "true" | "enabled" => Some(Self::Auto),
            _ => None,
        }
    }
}

/// Probe once whether `/usr/bin/bwrap` / `bwrap` in `PATH` executes cleanly in
/// the current Linux kernel/container namespace setup.
#[must_use]
pub fn bwrap_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("bwrap")
            .args([
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--",
                "true",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// Apply optional Bubblewrap sandbox wrapping and secret env scrubbing to a [`Command`].
pub fn build_sandboxed_shell_command(
    shell: &Path,
    shell_args: &[&str],
    command_str: &str,
    workdir: &Path,
    mode: ShellSandboxMode,
    extra_writable: &[PathBuf],
    scrub_secrets: bool,
) -> Result<(Command, bool), String> {
    let use_bwrap = match mode {
        ShellSandboxMode::Off => false,
        ShellSandboxMode::Bubblewrap | ShellSandboxMode::BubblewrapNoNet => {
            if !bwrap_available() {
                return Err(
                    "Shell sandbox mode requires Linux `bwrap` (Bubblewrap), which is unavailable in this environment."
                        .to_owned(),
                );
            }
            true
        }
        ShellSandboxMode::Auto => workdir != Path::new("/") && bwrap_available(),
    };

    let mut process = if use_bwrap {
        let mut cmd = Command::new("bwrap");
        cmd.args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
        ]);
        cmd.arg("--bind").arg(workdir).arg(workdir);

        let mut seen = BTreeSet::new();
        seen.insert(workdir.to_path_buf());
        for path in extra_writable {
            if path.exists() && seen.insert(path.clone()) {
                cmd.arg("--bind").arg(path).arg(path);
            }
        }
        cmd.args(["--die-with-parent", "--new-session"]);
        if mode == ShellSandboxMode::BubblewrapNoNet {
            cmd.arg("--unshare-net");
        }
        cmd.arg("--chdir").arg(workdir);
        cmd.arg("--").arg(shell);
        for arg in shell_args {
            cmd.arg(arg);
        }
        cmd.arg(command_str);
        cmd
    } else {
        let mut cmd = Command::new(shell);
        for arg in shell_args {
            cmd.arg(arg);
        }
        cmd.arg(command_str);
        cmd.current_dir(workdir);
        cmd
    };

    if scrub_secrets {
        for var in DEFAULT_SECRET_SCRUB_ENV {
            process.env_remove(var);
        }
    }
    Ok((process, use_bwrap))
}

/// Deterministic `pre_tool_use` policy gate blocking reads or shell access to
/// sensitive local credential stores.
pub fn pre_tool_use_secret_guard(tool: &str, input: &Map<String, Value>) -> Result<(), String> {
    const PROTECTED_MARKERS: &[&str] = &[
        "antigravity-accounts.json",
        "application_default_credentials.json",
        "escape.db",
        "opencode-rust.db",
        ".ssh/id_rsa",
        ".ssh/id_ed25519",
        ".netrc",
    ];
    let inspect_str = |text: &str| -> Result<(), String> {
        let lower = text.to_ascii_lowercase();
        for marker in PROTECTED_MARKERS {
            if lower.contains(marker) {
                return Err(format!(
                    "Blocked by deterministic pre_tool_use policy: tool `{tool}` may not access protected credential artifact `{marker}`."
                ));
            }
        }
        Ok(())
    };
    match tool {
        "read" | "write" | "edit" | "symbol_edit" => {
            if let Some(path) = input.get("path").and_then(Value::as_str) {
                inspect_str(path)?;
            }
        }
        "patch" => {
            if let Some(patch) = input.get("patchText").and_then(Value::as_str) {
                inspect_str(patch)?;
            }
        }
        "shell" | "lane" => {
            if let Some(cmd) = input.get("command").and_then(Value::as_str) {
                inspect_str(cmd)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Deterministic `post_tool_use` secret redactor scrubbing known credential token
/// formats before tool outputs re-enter the LLM conversation.
#[must_use]
pub fn redact_sensitive_output(text: &str) -> Option<String> {
    const TOKEN_PREFIXES: &[&str] = &[
        "AIza", "sk-ant-", "sk-proj-", "ya29.", "1//0", "ghp_", "xoxb-",
    ];
    let has_pem = text.contains("PRIVATE KEY-----");
    let has_prefix = TOKEN_PREFIXES.iter().any(|prefix| text.contains(prefix));
    if !has_pem && !has_prefix {
        return None;
    }

    let mut out = text.to_owned();
    let mut changed = false;

    for marker in [
        "-----BEGIN PRIVATE KEY-----",
        "-----BEGIN RSA PRIVATE KEY-----",
        "-----BEGIN EC PRIVATE KEY-----",
    ] {
        while let Some(start) = out.find(marker) {
            let end = out[start..]
                .find("-----END")
                .and_then(|rel| {
                    out[start + rel..]
                        .find("-----\n")
                        .map(|eol| start + rel + eol + 5)
                        .or_else(|| {
                            out[start + rel..]
                                .find("KEY-----")
                                .map(|eol| start + rel + eol + 8)
                        })
                })
                .unwrap_or_else(|| (start + marker.len()).min(out.len()));
            out.replace_range(start..end, "[REDACTED_PRIVATE_KEY]");
            changed = true;
        }
    }

    for prefix in TOKEN_PREFIXES {
        let mut search_from = 0_usize;
        while let Some(rel) = out[search_from..].find(prefix) {
            let start = search_from + rel;
            // Require word boundary or delimiter ahead of prefix so ordinary words aren't matched.
            let prev_ok = start == 0 || !out.as_bytes()[start - 1].is_ascii_alphanumeric();
            let mut end = start + prefix.len();
            while end < out.len() {
                let b = out.as_bytes()[end];
                if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/') {
                    end += 1;
                } else {
                    break;
                }
            }
            if prev_ok && end - start >= 18 {
                out.replace_range(start..end, "[REDACTED_SECRET]");
                search_from = start + "[REDACTED_SECRET]".len();
                changed = true;
            } else {
                search_from = start + prefix.len();
            }
        }
    }

    changed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bwrap_sandbox_allows_workdir_writes_and_blocks_outside_writes_when_available() {
        if !bwrap_available() {
            return;
        }
        let sandbox_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let inside_file = sandbox_dir.path().join("inside.txt");
        let outside_file = outside_dir.path().join("outside.txt");

        let script = format!(
            "echo ok > '{}' && ! touch '{}'",
            inside_file.display(),
            outside_file.display()
        );
        let (mut cmd, wrapped) = build_sandboxed_shell_command(
            Path::new("/bin/sh"),
            &["-c"],
            &script,
            sandbox_dir.path(),
            ShellSandboxMode::BubblewrapNoNet,
            &[],
            true,
        )
        .expect("bwrap setup");
        assert!(wrapped);
        let status = cmd.status().await.expect("run sandboxed");
        assert!(
            status.success(),
            "sandboxed write inside workdir should succeed and outside write must be blocked"
        );
        assert!(inside_file.exists());
        assert!(!outside_file.exists());
    }

    #[tokio::test]
    async fn secret_env_vars_are_scrubbed_from_sandboxed_shell() {
        let dir = tempfile::tempdir().unwrap();
        let (mut cmd, _) = build_sandboxed_shell_command(
            Path::new("/bin/sh"),
            &["-c"],
            "test -z \"$GEMINI_API_KEY\" && test -z \"$ANTHROPIC_API_KEY\"",
            dir.path(),
            ShellSandboxMode::Off,
            &[],
            true,
        )
        .expect("setup");
        let status = cmd.status().await.unwrap();
        assert!(status.success());
    }

    #[test]
    fn redacts_api_keys_and_pem_blocks_without_touching_clean_text() {
        assert!(redact_sensitive_output("cargo test passed cleanly").is_none());
        let leaked = "GEMINI_API_KEY=AIzaSyDummyKeyValue1234567890ABCDEF and oauth=ya29.a0ARrdaM1234567890abcdef";
        let scrubbed = redact_sensitive_output(leaked).expect("redacted");
        assert!(!scrubbed.contains("AIzaSy"));
        assert!(!scrubbed.contains("ya29."));
        assert!(scrubbed.contains("[REDACTED_SECRET]"));
    }

    #[test]
    fn pre_tool_use_secret_guard_blocks_protected_artifacts() {
        let mut input = Map::new();
        input.insert(
            "path".to_owned(),
            Value::String("/home/user/.config/opencode/antigravity-accounts.json".to_owned()),
        );
        assert!(pre_tool_use_secret_guard("read", &input).is_err());
    }
}
