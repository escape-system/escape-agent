//! Shell Command Safety Guards (Hermes `terminal_tool_guards` + hardline floor port).
//!
//! Provides two essential protection layers before commands reach the OS shell:
//! 1. Hardline Blocklist: refuses irreversible destructive operations (root wipe, raw disk write, fork bombs).
//! 2. Long-Lived Foreground Interceptor: catches servers (e.g. `vite`, `npm run dev`) and shell-level
//!    backgrounding (`&`, `nohup`) that would hang the foreground turn until the 120s timeout.

const SERVER_PATTERNS: &[&str] = &[
    "npm run dev",
    "pnpm dev",
    "pnpm run dev",
    "yarn dev",
    "bun dev",
    "vite",
    "next dev",
    "python -m http.server",
    "python3 -m http.server",
    "cargo watch",
];

/// Checks whether a shell command is safe to execute in foreground.
///
/// Returns `Ok(())` if execution may proceed, or an error description if blocked.
pub fn check_shell_command(command: &str) -> Result<(), &'static str> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    // 1. Informational queries are always exempt
    if is_help_or_version(trimmed) {
        return Ok(());
    }

    // 2. Hardline blocklist: catastrophic system destruction
    if is_hardline_blocked(trimmed) {
        return Err(
            "Blocked by hardline safety guard: this command performs an irreversible, \
             destructive system operation (root/home wipe, raw disk write, or system shutdown).",
        );
    }

    // 3. Foreground long-lived / unmanaged background process check
    if let Some(reason) = check_foreground_long_lived(trimmed) {
        return Err(reason);
    }

    Ok(())
}

fn is_help_or_version(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    lower.contains(" --help")
        || lower.ends_with(" -h")
        || lower.contains(" --version")
        || lower.ends_with(" -v")
}

fn is_hardline_blocked(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();

    // Fork bombs
    if lower.contains(":(){ :|:& };:")
        || lower.contains(":(){:|:&};:")
        || lower.contains(":() { :|:& }; :")
    {
        return true;
    }

    // System shutdown / halt
    if (lower.starts_with("shutdown")
        || lower.contains("; shutdown")
        || lower.contains("&& shutdown"))
        && (lower.contains(" -h") || lower.contains(" -r") || lower.contains(" now"))
    {
        return true;
    }
    if lower == "reboot"
        || lower.starts_with("reboot ")
        || lower.contains("; reboot")
        || lower == "poweroff"
        || lower.starts_with("poweroff ")
        || lower == "init 0"
    {
        return true;
    }

    // Direct write to raw block devices
    if lower.contains("> /dev/sd")
        || lower.contains("> /dev/nvme")
        || lower.contains("> /dev/hd")
        || (lower.contains("dd ") && lower.contains("of=/dev/sd"))
        || (lower.contains("dd ") && lower.contains("of=/dev/nvme"))
    {
        return true;
    }

    // Root / home directory recursive deletion (rm -rf / or rm -rf ~)
    if is_root_or_home_deletion(&lower) {
        return true;
    }

    false
}

fn is_root_or_home_deletion(lower: &str) -> bool {
    if !lower.contains("rm ") {
        return false;
    }

    let tokens: Vec<&str> = lower.split_whitespace().collect();
    let mut has_rm = false;
    let mut has_recursive = false;
    let mut has_force = false;
    let mut targets_root_or_home = false;

    for token in tokens {
        if token == "rm" {
            has_rm = true;
            continue;
        }
        if has_rm && token.starts_with('-') {
            if token.contains('r') || token.contains('R') {
                has_recursive = true;
            }
            if token.contains('f') {
                has_force = true;
            }
        }
        if has_rm
            && (token == "/"
                || token == "/*"
                || token == "~"
                || token == "$home"
                || token == "${home}")
        {
            targets_root_or_home = true;
        }
    }

    let _ = has_force;
    has_rm && has_recursive && targets_root_or_home
}

fn check_foreground_long_lived(cmd: &str) -> Option<&'static str> {
    let lower = cmd.to_lowercase();
    let trimmed = lower.trim();

    // Shell-level unmanaged backgrounding
    if trimmed.ends_with('&') && !trimmed.ends_with("&&") {
        return Some(
            "Foreground command uses trailing '&' backgrounding. Unmanaged background processes \
             leave untracked zombies and can corrupt workspace state. Run bounded commands in \
             foreground or manage services through the designated supervisor.",
        );
    }
    if trimmed.starts_with("nohup ")
        || trimmed.starts_with("disown ")
        || trimmed.starts_with("setsid ")
    {
        return Some(
            "Foreground command uses shell background wrappers (nohup/disown/setsid). \
             Run bounded commands in foreground or manage services through the designated supervisor.",
        );
    }

    for pattern in SERVER_PATTERNS {
        if trimmed == *pattern
            || trimmed.starts_with(&format!("{pattern} "))
            || trimmed.contains(&format!("; {pattern}"))
            || trimmed.contains(&format!("&& {pattern}"))
        {
            return Some(
                "This command appears to start a long-lived foreground server or watcher process. \
                 Running it in the foreground will hang the turn until the 120s timeout. \
                 Run bounded verification commands (such as tests or builds) instead.",
            );
        }
    }

    // docker compose up without -d (detached)
    if trimmed.contains("docker compose up")
        && !trimmed.contains(" -d")
        && !trimmed.contains("--detach")
    {
        return Some(
            "This command runs 'docker compose up' in foreground mode, which blocks indefinitely. \
             Use detached mode ('docker compose up -d') or run bounded container commands.",
        );
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_safe_commands() {
        assert!(check_shell_command("cargo test --workspace").is_ok());
        assert!(check_shell_command("cargo check").is_ok());
        assert!(check_shell_command("git status").is_ok());
        assert!(check_shell_command("ls -la src/").is_ok());
        assert!(check_shell_command("npm test").is_ok());
        assert!(check_shell_command("pytest tests/").is_ok());
    }

    #[test]
    fn exempts_help_and_version_flags() {
        assert!(check_shell_command("vite --version").is_ok());
        assert!(check_shell_command("npm run dev --help").is_ok());
        assert!(check_shell_command("python3 -m http.server -h").is_ok());
    }

    #[test]
    fn blocks_hardline_catastrophic_commands() {
        assert!(check_shell_command("rm -rf /").is_err());
        assert!(check_shell_command("rm -rf /*").is_err());
        assert!(check_shell_command("rm -rf ~").is_err());
        assert!(check_shell_command("rm -rf $HOME").is_err());
        assert!(check_shell_command(":(){ :|:& };:").is_err());
        assert!(check_shell_command("echo bad > /dev/sda").is_err());
        assert!(check_shell_command("reboot").is_err());
        assert!(check_shell_command("shutdown -h now").is_err());
    }

    #[test]
    fn blocks_foreground_dev_servers_and_backgrounding() {
        assert!(check_shell_command("npm run dev").is_err());
        assert!(check_shell_command("pnpm dev").is_err());
        assert!(check_shell_command("vite").is_err());
        assert!(check_shell_command("next dev").is_err());
        assert!(check_shell_command("python3 -m http.server 8080").is_err());
        assert!(check_shell_command("cargo watch -x check").is_err());
        assert!(check_shell_command("docker compose up").is_err());
        // Trailing ampersand
        assert!(check_shell_command("sleep 100 &").is_err());
        assert!(check_shell_command("nohup sleep 100").is_err());

        // But docker compose up -d is allowed
        assert!(check_shell_command("docker compose up -d").is_ok());
    }
}
