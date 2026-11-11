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

fn strip_data_heredocs(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut pending_delim: Option<String> = None;

    for line in cmd.lines() {
        if let Some(delim) = &pending_delim {
            if line.trim() == delim.as_str() {
                pending_delim = None;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
        if let Some(delim) = detect_heredoc_delimiter(line) {
            pending_delim = Some(delim);
        }
    }
    out
}

fn detect_heredoc_delimiter(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'<' && bytes[i + 1] == b'<' {
            if i + 2 < bytes.len() && bytes[i + 2] == b'<' {
                i += 3;
                continue;
            }
            let prefix = line[..i].to_lowercase();
            // Do not strip heredocs fed directly into a shell interpreter.
            let feeds_shell =
                prefix
                    .split([';', '&', '|', '('])
                    .next_back()
                    .is_some_and(|clause| {
                        let c = clause.trim();
                        c == "sh"
                            || c == "bash"
                            || c == "zsh"
                            || c.starts_with("sh ")
                            || c.starts_with("bash ")
                            || c.starts_with("zsh ")
                    });
            if feeds_shell {
                return None;
            }
            let mut rest = &line[i + 2..];
            if let Some(stripped) = rest.strip_prefix('-') {
                rest = stripped;
            }
            rest = rest.trim_start_matches([' ', '\t']);
            let token_end = rest
                .find([' ', '\t', ';', '&', '|', ')'])
                .unwrap_or(rest.len());
            let raw_token = rest[..token_end]
                .trim_matches('\'')
                .trim_matches('"')
                .trim_start_matches('\\');
            if !raw_token.is_empty() {
                return Some(raw_token.to_string());
            }
        }
        i += 1;
    }
    None
}

fn is_root_or_home_deletion(lower: &str) -> bool {
    let stripped = strip_data_heredocs(lower);
    if !stripped.contains("rm ") {
        return false;
    }

    for (raw_clause, _) in split_shell_clauses(&stripped) {
        let mut has_rm = false;
        let mut has_recursive = false;
        let mut targets_root_or_home = false;

        for token in raw_clause.split_whitespace() {
            if token == "rm" {
                has_rm = true;
                continue;
            }
            if has_rm
                && token.starts_with('-')
                && (token == "--recursive"
                    || (!token.starts_with("--") && (token.contains('r') || token.contains('R'))))
            {
                has_recursive = true;
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

        if has_rm && has_recursive && targets_root_or_home {
            return true;
        }
    }

    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClauseSep {
    Seq,
    AndOr,
    Background,
    End,
}

fn split_shell_clauses(cmd: &str) -> Vec<(&str, ClauseSep)> {
    let bytes = cmd.as_bytes();
    let len = bytes.len();
    let mut clauses = Vec::new();
    let mut start = 0;
    let mut i = 0;

    while i < len {
        match bytes[i] {
            b';' | b'\n' => {
                clauses.push((&cmd[start..i], ClauseSep::Seq));
                i += 1;
                start = i;
            }
            b'&' => {
                if i + 1 < len && bytes[i + 1] == b'&' {
                    clauses.push((&cmd[start..i], ClauseSep::AndOr));
                    i += 2;
                    start = i;
                } else if (i > 0 && (bytes[i - 1] == b'>' || bytes[i - 1] == b'<'))
                    || (i + 1 < len && bytes[i + 1] == b'>')
                {
                    i += 1;
                } else {
                    clauses.push((&cmd[start..i], ClauseSep::Background));
                    i += 1;
                    start = i;
                }
            }
            b'|' if i + 1 < len && bytes[i + 1] == b'|' => {
                clauses.push((&cmd[start..i], ClauseSep::AndOr));
                i += 2;
                start = i;
            }
            _ => {
                i += 1;
            }
        }
    }

    if start < len {
        clauses.push((&cmd[start..len], ClauseSep::End));
    }

    clauses
}

fn normalize_clause(raw: &str) -> &str {
    let mut s = raw
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim();
    loop {
        if let Some(rest) = s.strip_prefix("env ") {
            s = rest.trim_start();
            continue;
        }
        if let Some((first, rest)) = s.split_once(char::is_whitespace)
            && first.contains('=')
            && !first.starts_with('=')
        {
            s = rest.trim_start();
            continue;
        }
        break;
    }
    s
}

fn check_foreground_long_lived(cmd: &str) -> Option<&'static str> {
    let stripped = strip_data_heredocs(cmd);
    let lower = stripped.to_lowercase();
    let trimmed = lower.trim();

    // Shell-level unmanaged backgrounding
    if trimmed.ends_with('&') && !trimmed.ends_with("&&") {
        return Some(
            "Foreground command uses trailing '&' backgrounding. Unmanaged background processes \
             leave untracked zombies and can corrupt workspace state. Run bounded commands in \
             foreground or manage services through the designated supervisor.",
        );
    }

    let clauses = split_shell_clauses(trimmed);
    for (idx, (raw_clause, sep)) in clauses.iter().enumerate() {
        let clause = normalize_clause(raw_clause);
        if clause.is_empty() {
            continue;
        }

        if clause == "disown"
            || clause.starts_with("nohup ")
            || clause.starts_with("disown ")
            || clause.starts_with("setsid ")
        {
            return Some(
                "Foreground command uses shell background wrappers (nohup/disown/setsid). \
                 Run bounded commands in foreground or manage services through the designated supervisor.",
            );
        }

        let matches_server = SERVER_PATTERNS
            .iter()
            .any(|pattern| clause == *pattern || clause.starts_with(&format!("{pattern} ")));
        if matches_server {
            let has_following_command = clauses[idx + 1..]
                .iter()
                .any(|(rest, _)| !normalize_clause(rest).is_empty());
            let is_bounded_inline_bg = *sep == ClauseSep::Background && has_following_command;
            if !is_bounded_inline_bg {
                return Some(
                    "This command appears to start a long-lived foreground server or watcher process. \
                     Running it in the foreground will hang the turn until the 120s timeout. \
                     Run bounded verification commands (such as tests or builds) instead.",
                );
            }
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
        // Regression for ses_e3922412df48OEb4ewmuhNpQRy: `rm -f out/...` followed
        // by a README heredoc containing `| URL_PATH | / |` and `--enable-unsafe-swiftshader`
        // must not be mistaken for `rm -rf /`.
        assert!(
            check_shell_command(
                "cd /root/workspace/.verify && rm -f out/cube2.canvas.png out/cube3.canvas.png && cat > README.md <<'EOF'\n\
                 | URL_PATH | / | path to open |\n\
                 --no-sandbox --enable-unsafe-swiftshader\n\
                 python3 -m http.server\n\
                 EOF\n\
                 ls -l /root/workspace/.verify"
            )
            .is_ok()
        );
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
        assert!(
            check_shell_command(
                "cd /root/horse && setsid nohup python3 -m http.server 8123 >/dev/null 2>&1 & sleep 1"
            )
            .is_err()
        );
        assert!(check_shell_command("cd /root/horse && python3 -m http.server 8123").is_err());

        // Bounded verification command with inline ephemeral server is allowed
        assert!(
            check_shell_command(
                "cd /root/horse && python3 -m http.server 8123 > /dev/null 2>&1 & SRV=$!; sleep 1; node verify_test.js; kill $SRV 2>/dev/null || true"
            )
            .is_ok()
        );
        assert!(
            check_shell_command(
                "python3 -m http.server 8123 --directory /root/horse >/dev/null 2>&1 & PID=$!; sleep 1; node /tmp/test_horse.js; kill $PID"
            )
            .is_ok()
        );

        // But docker compose up -d is allowed
        assert!(check_shell_command("docker compose up -d").is_ok());
    }
}
